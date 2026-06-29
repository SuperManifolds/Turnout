use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, RwLock};

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use lru::LruCache;
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, PixmapPaint, Stroke, Transform};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tower_http::cors::CorsLayer;
use turnout_core::geo::{latlon_to_mercator, latlon_to_tile_pixel, tile_bounds};
use turnout_core::kml::{Geometry, KmzData, Style};

const TILE_SIZE: u32 = 256;
const CACHE_CAPACITY: usize = 512;
const PREFERRED_PORT: u16 = 17853;

const DEFAULT_LINE_COLOR: [u8; 4] = [255, 100, 0, 200];
const DEFAULT_LINE_WIDTH: f32 = 2.0;
const DEFAULT_FILL_COLOR: [u8; 4] = [255, 100, 0, 80];
const POINT_RADIUS: f32 = 5.0;
const POINT_COLOR: [u8; 4] = [255, 60, 0, 220];
const WMS_BBOX: (f64, f64, f64, f64) = (-180.0, -85.051_129, 180.0, 85.051_129);

pub(crate) struct DecodedImage {
    pixmap: Pixmap,
    north: f64,
    south: f64,
    east: f64,
    west: f64,
    rotation: f64,
}

pub enum LayerSource {
    Kmz { data: KmzData, images: Vec<DecodedImage>, path: Option<String> },
    Wms { base_url: String, layer_name: String },
    ArcGis { base_url: String, service_name: String },
}

pub struct Layer {
    pub id: u32,
    pub name: String,
    pub bbox: (f64, f64, f64, f64),
    pub kind: &'static str,
    pub visible: bool,
    pub opacity: f32,
    pub source: LayerSource,
}

pub struct TileState {
    pub(crate) layers: RwLock<Vec<Layer>>,
    cache: Mutex<LruCache<(u8, u32, u32), Vec<u8>>>,
    next_id: Mutex<u32>,
    http: reqwest::Client,
}

pub struct ServerHandle {
    pub port: u16,
    pub state: Arc<TileState>,
    pub shutdown_tx: watch::Sender<bool>,
}

impl ServerHandle {
    pub fn add_kmz_layer(&self, data: KmzData, path: Option<String>, kind: &'static str) -> bool {
        let Some(bbox) = data.bbox() else { return false };
        let name = data.name.clone().unwrap_or_else(|| "Overlay".to_string());
        let images = decode_images(&data);
        let id = self.next_id();

        let mut layers = self.state.layers.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        layers.push(Layer {
            id, name, bbox, kind, visible: true, opacity: 1.0,
            source: LayerSource::Kmz { data, images, path },
        });
        drop(layers);
        self.clear_cache();
        true
    }

    pub fn add_wms_layer(&self, base_url: String, layer_name: String, display_name: String) -> u32 {
        let id = self.next_id();

        let mut layers = self.state.layers.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        layers.push(Layer {
            id, name: display_name, bbox: WMS_BBOX, kind: "wms", visible: true, opacity: 1.0,
            source: LayerSource::Wms { base_url, layer_name },
        });
        drop(layers);
        self.clear_cache();
        id
    }

    pub fn add_arcgis_layer(&self, base_url: String, service_name: String, display_name: String) -> u32 {
        let id = self.next_id();

        let mut layers = self.state.layers.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        layers.push(Layer {
            id, name: display_name, bbox: WMS_BBOX, kind: "arcgis", visible: true, opacity: 1.0,
            source: LayerSource::ArcGis { base_url, service_name },
        });
        drop(layers);
        self.clear_cache();
        id
    }

    pub fn remove_layer(&self, id: u32) -> bool {
        let mut layers = self.state.layers.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = layers.len();
        layers.retain(|l| l.id != id);
        let removed = layers.len() < before;
        drop(layers);
        if removed {
            self.clear_cache();
        }
        removed
    }

    pub fn set_layer_visible(&self, id: u32, visible: bool) -> bool {
        let mut layers = self.state.layers.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(layer) = layers.iter_mut().find(|l| l.id == id) {
            layer.visible = visible;
            drop(layers);
            self.clear_cache();
            true
        } else {
            false
        }
    }

    pub fn set_layer_opacity(&self, id: u32, opacity: f32) -> bool {
        let mut layers = self.state.layers.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(layer) = layers.iter_mut().find(|l| l.id == id) {
            layer.opacity = opacity.clamp(0.0, 1.0);
            drop(layers);
            self.clear_cache();
            true
        } else {
            false
        }
    }

    pub fn layer_count(&self) -> usize {
        self.state.layers.read().unwrap_or_else(std::sync::PoisonError::into_inner).len()
    }

    fn next_id(&self) -> u32 {
        let mut next = self.state.next_id.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let id = *next;
        *next += 1;
        id
    }

    pub fn clear_cache(&self) {
        let mut cache = self.state.cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.clear();
    }
}

pub async fn start() -> Result<ServerHandle, Box<dyn std::error::Error + Send + Sync>> {
    let state = Arc::new(TileState {
        layers: RwLock::new(Vec::new()),
        cache: Mutex::new(LruCache::new(
            NonZeroUsize::new(CACHE_CAPACITY).expect("nonzero"),
        )),
        next_id: Mutex::new(0),
        http: reqwest::Client::new(),
    });

    let app = Router::new()
        .route("/{z}/{x}/{y}", get(serve_tile))
        .layer(CorsLayer::permissive())
        .with_state(Arc::clone(&state));

    let listener = match TcpListener::bind(format!("127.0.0.1:{PREFERRED_PORT}")).await {
        Ok(l) => l,
        Err(_) => TcpListener::bind("127.0.0.1:0").await?,
    };
    let port = listener.local_addr()?.port();

    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.changed().await;
            })
            .await
            .ok();
    });

    Ok(ServerHandle { port, state, shutdown_tx })
}

fn decode_images(data: &KmzData) -> Vec<DecodedImage> {
    data.ground_overlays
        .iter()
        .filter_map(|go| {
            let img_bytes = data.images.get(&go.href).or_else(|| {
                let basename = go.href.rsplit('/').next()?;
                data.images
                    .iter()
                    .find(|(k, _)| k.ends_with(basename))
                    .map(|(_, v)| v)
            })?;

            let dyn_img = image::load_from_memory(img_bytes).ok()?;
            let rgba = dyn_img.to_rgba8();
            let (w, h) = (rgba.width(), rgba.height());
            let pixmap = Pixmap::from_vec(rgba.into_raw(), tiny_skia::IntSize::from_wh(w, h)?)?;

            Some(DecodedImage {
                pixmap,
                north: go.north,
                south: go.south,
                east: go.east,
                west: go.west,
                rotation: go.rotation,
            })
        })
        .collect()
}

async fn fetch_wms_tile(
    client: &reqwest::Client,
    base_url: &str,
    layer_name: &str,
    z: u32, x: u32, y: u32,
) -> Option<Pixmap> {
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
        .map_err(|e| eprintln!("WMS fetch error: {e}"))
        .ok()?;

    if !resp.status().is_success() {
        eprintln!("WMS returned HTTP {}", resp.status());
        return None;
    }

    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if content_type.contains("xml") || content_type.contains("text") {
        let body = resp.text().await.ok().unwrap_or_default();
        eprintln!("WMS error for layer={layer_name}: {}", &body[..body.len().min(500)]);
        return None;
    }

    let bytes = resp.bytes().await.ok()?;
    let dyn_img = image::load_from_memory(&bytes)
        .map_err(|e| eprintln!("WMS image decode error: {e}"))
        .ok()?;
    let rgba = dyn_img.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    Pixmap::from_vec(rgba.into_raw(), tiny_skia::IntSize::from_wh(w, h)?)
}

async fn fetch_arcgis_tile(
    client: &reqwest::Client,
    base_url: &str,
    service_name: &str,
    z: u32, x: u32, y: u32,
) -> Option<Pixmap> {
    let url = format!("{base_url}/{service_name}/MapServer/tile/{z}/{y}/{x}");

    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| eprintln!("ArcGIS fetch error: {e}"))
        .ok()?;

    if !resp.status().is_success() {
        return None;
    }

    let bytes = resp.bytes().await.ok()?;
    let dyn_img = image::load_from_memory(&bytes)
        .map_err(|e| eprintln!("ArcGIS image decode error: {e}"))
        .ok()?;
    let rgba = dyn_img.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    Pixmap::from_vec(rgba.into_raw(), tiny_skia::IntSize::from_wh(w, h)?)
}

enum RemoteReq {
    Wms(String, String),
    ArcGis(String, String),
}

async fn serve_tile(
    Path((z, x, y)): Path<(u8, u32, u32)>,
    State(state): State<Arc<TileState>>,
) -> impl IntoResponse {
    {
        let mut cache = state.cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(png) = cache.get(&(z, x, y)) {
            return (StatusCode::OK, [("content-type", "image/png")], png.clone());
        }
    }

    let remote_requests: Vec<(usize, RemoteReq)> = {
        let layers = state.layers.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        layers.iter().enumerate().filter_map(|(i, l)| {
            if !l.visible { return None; }
            match &l.source {
                LayerSource::Wms { base_url, layer_name } =>
                    Some((i, RemoteReq::Wms(base_url.clone(), layer_name.clone()))),
                LayerSource::ArcGis { base_url, service_name } =>
                    Some((i, RemoteReq::ArcGis(base_url.clone(), service_name.clone()))),
                LayerSource::Kmz { .. } => None,
            }
        }).collect()
    };

    let mut remote_tiles: Vec<(usize, Pixmap)> = Vec::new();
    for (idx, req) in &remote_requests {
        let result = match req {
            RemoteReq::Wms(url, layer) => fetch_wms_tile(&state.http, url, layer, z.into(), x, y).await,
            RemoteReq::ArcGis(url, svc) => fetch_arcgis_tile(&state.http, url, svc, z.into(), x, y).await,
        };
        if let Some(pixmap) = result {
            remote_tiles.push((*idx, pixmap));
        }
    }

    let png = render_tile(&state, &remote_tiles, z.into(), x, y);

    {
        let mut cache = state.cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.put((z, x, y), png.clone());
    }

    (StatusCode::OK, [("content-type", "image/png")], png)
}

fn render_tile(state: &TileState, remote_tiles: &[(usize, Pixmap)], z: u32, x: u32, y: u32) -> Vec<u8> {
    let mut pixmap = Pixmap::new(TILE_SIZE, TILE_SIZE).expect("256x256 pixmap");
    let (tile_w, tile_s, tile_e, tile_n) = tile_bounds(z, x, y);

    let layers = state.layers.read().unwrap_or_else(std::sync::PoisonError::into_inner);
    for (i, layer) in layers.iter().enumerate() {
        if !layer.visible {
            continue;
        }
        let opacity = layer.opacity;
        match &layer.source {
            LayerSource::Kmz { data, images, .. } => {
                render_ground_overlays(&mut pixmap, images, opacity, z, x, y, tile_w, tile_s, tile_e, tile_n);
                render_geometry(&mut pixmap, data, opacity, z, x, y, tile_w, tile_s, tile_e, tile_n);
            }
            LayerSource::Wms { .. } | LayerSource::ArcGis { .. } => {
                if let Some((_, remote_pixmap)) = remote_tiles.iter().find(|(idx, _)| *idx == i) {
                    let paint = PixmapPaint { opacity, ..PixmapPaint::default() };
                    pixmap.draw_pixmap(
                        0, 0,
                        remote_pixmap.as_ref(),
                        &paint,
                        Transform::identity(),
                        None,
                    );
                }
            }
        }
    }

    pixmap.encode_png().unwrap_or_default()
}

fn render_ground_overlays(
    pixmap: &mut Pixmap,
    images: &[DecodedImage],
    opacity: f32,
    z: u32, x: u32, y: u32,
    tile_w: f64, tile_s: f64, tile_e: f64, tile_n: f64,
) {
    let paint = PixmapPaint { opacity, ..PixmapPaint::default() };

    for ov in images {
        if ov.east < tile_w || ov.west > tile_e || ov.north < tile_s || ov.south > tile_n {
            continue;
        }

        let (px_left, py_top) = latlon_to_tile_pixel(ov.north, ov.west, z, x, y);
        let (px_right, py_bottom) = latlon_to_tile_pixel(ov.south, ov.east, z, x, y);

        let dest_w = px_right - px_left;
        let dest_h = py_bottom - py_top;
        if dest_w.abs() < 0.01 || dest_h.abs() < 0.01 {
            continue;
        }

        let sx = dest_w / ov.pixmap.width() as f32;
        let sy = dest_h / ov.pixmap.height() as f32;

        let transform = if ov.rotation.abs() > 0.001 {
            let cx = (px_left + px_right) / 2.0;
            let cy = (py_top + py_bottom) / 2.0;
            Transform::from_translate(cx, cy)
                .pre_concat(Transform::from_rotate(-ov.rotation as f32))
                .pre_concat(Transform::from_translate(-cx, -cy))
                .pre_concat(Transform::from_translate(px_left, py_top))
                .pre_concat(Transform::from_scale(sx, sy))
        } else {
            Transform::from_translate(px_left, py_top).pre_concat(Transform::from_scale(sx, sy))
        };

        pixmap.draw_pixmap(0, 0, ov.pixmap.as_ref(), &paint, transform, None);
    }
}

fn apply_opacity(rgba: [u8; 4], opacity: f32) -> [u8; 4] {
    [rgba[0], rgba[1], rgba[2], (f32::from(rgba[3]) * opacity) as u8]
}

fn render_geometry(
    pixmap: &mut Pixmap,
    data: &KmzData,
    opacity: f32,
    z: u32, x: u32, y: u32,
    tile_w: f64, tile_s: f64, tile_e: f64, tile_n: f64,
) {
    let margin = (tile_e - tile_w) * 0.1;
    let ew = tile_w - margin;
    let ee = tile_e + margin;
    let es = tile_s - margin;
    let en = tile_n + margin;

    for pm in &data.placemarks {
        let style = data.resolve_style(pm);
        render_single_geometry(pixmap, &pm.geometry, opacity, z, x, y, ew, es, ee, en, &style);
    }
}

fn render_single_geometry(
    pixmap: &mut Pixmap,
    geom: &Geometry,
    opacity: f32,
    z: u32, x: u32, y: u32,
    ew: f64, es: f64, ee: f64, en: f64,
    style: &Style,
) {
    match geom {
        Geometry::Point { lon, lat } => {
            if *lon >= ew && *lon <= ee && *lat >= es && *lat <= en {
                render_point(pixmap, *lat, *lon, opacity, z, x, y, style);
            }
        }
        Geometry::LineString { coords } => {
            if coords_intersect(coords, ew, es, ee, en) {
                render_linestring(pixmap, coords, opacity, z, x, y, style);
            }
        }
        Geometry::Polygon { outer, inner } => {
            if coords_intersect(outer, ew, es, ee, en) {
                render_polygon(pixmap, outer, inner, opacity, z, x, y, style);
            }
        }
        Geometry::Multi(geoms) => {
            for g in geoms {
                render_single_geometry(pixmap, g, opacity, z, x, y, ew, es, ee, en, style);
            }
        }
    }
}

fn coords_intersect(coords: &[(f64, f64)], w: f64, s: f64, e: f64, n: f64) -> bool {
    coords.iter().any(|(lon, lat)| *lon >= w && *lon <= e && *lat >= s && *lat <= n)
}

fn render_point(pixmap: &mut Pixmap, lat: f64, lon: f64, opacity: f32, z: u32, x: u32, y: u32, style: &Style) {
    let (px, py) = latlon_to_tile_pixel(lat, lon, z, x, y);

    let rgba = apply_opacity(style.line_color.unwrap_or(POINT_COLOR), opacity);
    let mut paint = Paint::default();
    paint.set_color(Color::from_rgba8(rgba[0], rgba[1], rgba[2], rgba[3]));
    paint.anti_alias = true;

    let mut pb = PathBuilder::new();
    pb.push_circle(px, py, POINT_RADIUS);
    if let Some(path) = pb.finish() {
        pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
    }
}

fn render_linestring(
    pixmap: &mut Pixmap,
    coords: &[(f64, f64)],
    opacity: f32,
    z: u32, x: u32, y: u32,
    style: &Style,
) {
    if coords.len() < 2 {
        return;
    }

    let mut pb = PathBuilder::new();
    let (px, py) = latlon_to_tile_pixel(coords[0].1, coords[0].0, z, x, y);
    pb.move_to(px, py);
    for &(lon, lat) in &coords[1..] {
        let (px, py) = latlon_to_tile_pixel(lat, lon, z, x, y);
        pb.line_to(px, py);
    }

    let Some(path) = pb.finish() else { return };

    let rgba = apply_opacity(style.line_color.unwrap_or(DEFAULT_LINE_COLOR), opacity);
    let mut paint = Paint::default();
    paint.set_color(Color::from_rgba8(rgba[0], rgba[1], rgba[2], rgba[3]));
    paint.anti_alias = true;

    let stroke = Stroke {
        width: style.line_width.unwrap_or(DEFAULT_LINE_WIDTH),
        ..Stroke::default()
    };

    pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
}

fn render_polygon(
    pixmap: &mut Pixmap,
    outer: &[(f64, f64)],
    inner: &[Vec<(f64, f64)>],
    opacity: f32,
    z: u32, x: u32, y: u32,
    style: &Style,
) {
    if outer.len() < 3 {
        return;
    }

    let mut pb = PathBuilder::new();
    let (px, py) = latlon_to_tile_pixel(outer[0].1, outer[0].0, z, x, y);
    pb.move_to(px, py);
    for &(lon, lat) in &outer[1..] {
        let (px, py) = latlon_to_tile_pixel(lat, lon, z, x, y);
        pb.line_to(px, py);
    }
    pb.close();

    for ring in inner {
        if ring.len() < 3 {
            continue;
        }
        let (px, py) = latlon_to_tile_pixel(ring[0].1, ring[0].0, z, x, y);
        pb.move_to(px, py);
        for &(lon, lat) in &ring[1..] {
            let (px, py) = latlon_to_tile_pixel(lat, lon, z, x, y);
            pb.line_to(px, py);
        }
        pb.close();
    }

    let Some(path) = pb.finish() else { return };

    let should_fill = style.poly_fill.unwrap_or(true);
    let should_outline = style.poly_outline.unwrap_or(true);

    if should_fill {
        let rgba = apply_opacity(style.fill_color.unwrap_or(DEFAULT_FILL_COLOR), opacity);
        let mut paint = Paint::default();
        paint.set_color(Color::from_rgba8(rgba[0], rgba[1], rgba[2], rgba[3]));
        paint.anti_alias = true;
        pixmap.fill_path(&path, &paint, FillRule::EvenOdd, Transform::identity(), None);
    }

    if should_outline {
        let rgba = apply_opacity(style.line_color.unwrap_or(DEFAULT_LINE_COLOR), opacity);
        let mut paint = Paint::default();
        paint.set_color(Color::from_rgba8(rgba[0], rgba[1], rgba[2], rgba[3]));
        paint.anti_alias = true;

        let stroke = Stroke {
            width: style.line_width.unwrap_or(DEFAULT_LINE_WIDTH),
            ..Stroke::default()
        };
        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }
}
