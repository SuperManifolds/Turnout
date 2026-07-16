//! Fetching remote raster tiles (WMS / XYZ / `ArcGIS`) and reading local `MBTiles`,
//! plus the decode/cache helpers that turn fetched bytes into pixmaps.

use std::sync::{Arc, Mutex};

use tiny_skia::Pixmap;
use turnout_core::geo::{latlon_to_mercator, tile_bounds};

use crate::server_core::UnpoisonExt;

use super::{DecodedTile, TileState, TILE_SIZE};

/// How a remote layer's tile is fetched over HTTP, resolved once per request.
pub(super) enum RemoteReq {
    Wms(String, String),
    ArcGis(String, String),
    Xyz(String),
}

pub(super) fn decode_remote_bytes(bytes: &[u8]) -> Option<DecodedTile> {
    let dyn_img = image::load_from_memory(bytes).ok()?;
    let resized = if dyn_img.width() != TILE_SIZE || dyn_img.height() != TILE_SIZE {
        dyn_img.resize_exact(TILE_SIZE, TILE_SIZE, image::imageops::FilterType::Triangle)
    } else {
        dyn_img
    };
    let rgba = resized.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    Some((rgba.into_raw(), w, h))
}

pub(super) fn decoded_to_pixmap(decoded: &DecodedTile) -> Option<Pixmap> {
    let (data, w, h) = decoded;
    Pixmap::from_vec(data.clone(), tiny_skia::IntSize::from_wh(*w, *h)?)
}

/// Returns a shared, cached read-only connection for `path`, opening it on first use.
pub(super) fn mbtiles_conn(state: &TileState, path: &str) -> Option<Arc<Mutex<rusqlite::Connection>>> {
    let mut conns = state.mbtiles_conns.lock().unpoison();
    if let Some(conn) = conns.get(path) {
        return Some(Arc::clone(conn));
    }
    let conn = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ).ok()?;
    let arc = Arc::new(Mutex::new(conn));
    // Bounded LRU: evicting only drops this map's ref; an in-flight request keeps
    // its own Arc, so the connection closes when the last user releases it.
    conns.put(path.to_string(), Arc::clone(&arc));
    Some(arc)
}

pub(super) fn read_mbtiles_tile(conn: &rusqlite::Connection, z: u8, x: u32, tms_y: u32, max_zoom: u8) -> Option<Pixmap> {
    if z <= max_zoom {
        return query_mbtiles_tile(conn, z, x, tms_y);
    }
    overzoom_mbtiles_tile(conn, z, x, tms_y, max_zoom)
}

fn query_mbtiles_tile(conn: &rusqlite::Connection, z: u8, x: u32, tms_y: u32) -> Option<Pixmap> {
    let data: Vec<u8> = conn.query_row(
        "SELECT tile_data FROM tiles WHERE zoom_level = ?1 AND tile_column = ?2 AND tile_row = ?3",
        rusqlite::params![z, x, tms_y],
        |row| row.get(0),
    ).ok()?;
    let decoded = decode_remote_bytes(&data)?;
    decoded_to_pixmap(&decoded)
}

fn overzoom_mbtiles_tile(conn: &rusqlite::Connection, z: u8, x: u32, tms_y: u32, max_zoom: u8) -> Option<Pixmap> {
    let dz = z - max_zoom;
    let y = turnout_core::geo::tms_y(z, tms_y);
    let parent_x = x >> dz;
    let parent_y = y >> dz;
    let parent_tms_y = (1u32 << max_zoom) - 1 - parent_y;
    let parent_pm = query_mbtiles_tile(conn, max_zoom, parent_x, parent_tms_y)?;

    let scale = 1u32 << dz;
    let sub_x = x - (parent_x * scale);
    let sub_y = y - (parent_y * scale);
    let region_size = TILE_SIZE / scale;
    let src_x = sub_x * region_size;
    let src_y = sub_y * region_size;

    let parent_img = image::RgbaImage::from_raw(TILE_SIZE, TILE_SIZE, parent_pm.data().to_vec())?;
    let cropped = image::imageops::crop_imm(&parent_img, src_x, src_y, region_size, region_size);
    let upscaled = image::imageops::resize(&*cropped, TILE_SIZE, TILE_SIZE, image::imageops::FilterType::Triangle);
    let (w, h) = (upscaled.width(), upscaled.height());
    Pixmap::from_vec(upscaled.into_raw(), tiny_skia::IntSize::from_wh(w, h)?)
}

pub(super) fn get_remote_cached(state: &TileState, layer_id: u32, z: u8, x: u32, y: u32) -> Option<DecodedTile> {
    let mut cache = state.remote_cache.lock().unpoison();
    cache.get(&(layer_id, z, x, y)).cloned()
}

pub(super) fn put_remote_cached(state: &TileState, layer_id: u32, z: u8, x: u32, y: u32, decoded: DecodedTile) {
    let mut cache = state.remote_cache.lock().unpoison();
    cache.put((layer_id, z, x, y), decoded);
}

pub(super) async fn fetch_wms_tile(
    client: &reqwest::Client,
    base_url: &str,
    layer_name: &str,
    z: u32, x: u32, y: u32,
) -> Option<Vec<u8>> {
    let (tile_w, tile_s, tile_e, tile_n) = tile_bounds(z, x, y);
    let (minx, miny) = latlon_to_mercator(tile_s, tile_w);
    let (maxx, maxy) = latlon_to_mercator(tile_n, tile_e);

    let bbox_str = format!("{minx},{miny},{maxx},{maxy}");

    let req = client
        .get(base_url)
        .query(&[
            ("service", "WMS"),
            ("version", "1.1.1"),
            ("request", "GetMap"),
            ("layers", layer_name),
            ("styles", ""),
            ("srs", "EPSG:3857"),
            ("bbox", &bbox_str),
            ("width", "256"),
            ("height", "256"),
            ("format", "image/png"),
            ("transparent", "true"),
        ]);

    let resp = req
        .send()
        .await
        .map_err(|e| tracing::warn!("WMS fetch error: {e}"))
        .ok()?;

    if !resp.status().is_success() {
        tracing::warn!("WMS returned HTTP {}", resp.status());
        return None;
    }

    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if content_type.contains("xml") || content_type.contains("text") {
        let body = resp.text().await.ok().unwrap_or_default();
        tracing::warn!("WMS error for layer={layer_name}: {}", &body[..body.len().min(500)]);
        return None;
    }

    Some(resp.bytes().await.ok()?.to_vec())
}

pub(crate) fn xyz_to_quadkey(z: u32, x: u32, y: u32) -> String {
    let mut quadkey = String::with_capacity(z as usize);
    for i in (1..=z).rev() {
        let mut digit = 0u8;
        let mask = 1u32 << (i - 1);
        if (x & mask) != 0 { digit += 1; }
        if (y & mask) != 0 { digit += 2; }
        quadkey.push((b'0' + digit) as char);
    }
    quadkey
}

pub(super) async fn fetch_xyz_tile(
    client: &reqwest::Client,
    url_template: &str,
    z: u32, x: u32, y: u32,
) -> Option<Vec<u8>> {
    let url = url_template
        .replace("{z}", &z.to_string())
        .replace("{x}", &x.to_string())
        .replace("{y}", &y.to_string())
        .replace("{q}", &xyz_to_quadkey(z, x, y));

    let resp = client
        .get(&url)
        .header("User-Agent", "Mozilla/5.0")
        .send()
        .await
        .map_err(|e| tracing::warn!("XYZ fetch error for {url}: {e}"))
        .ok()?;

    if !resp.status().is_success() {
        tracing::warn!("XYZ returned HTTP {} for {url}", resp.status());
        return None;
    }

    Some(resp.bytes().await.ok()?.to_vec())
}

pub(super) async fn fetch_arcgis_tile(
    client: &reqwest::Client,
    base_url: &str,
    service_name: &str,
    z: u32, x: u32, y: u32,
) -> Option<Vec<u8>> {
    let url = format!("{base_url}/{service_name}/MapServer/tile/{z}/{y}/{x}");

    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| tracing::warn!("ArcGIS fetch error: {e}"))
        .ok()?;

    if !resp.status().is_success() {
        return None;
    }

    Some(resp.bytes().await.ok()?.to_vec())
}
