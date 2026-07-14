//! Re-tiles `CartoMetro`'s per-city map tiles into standard Web Mercator XYZ tiles
//! that NIMBY Rails understands.
//!
//! `CartoMetro` (cartometro.com) serves each city on its own `OpenLayers` tile grid:
//! Web Mercator (`EPSG:3857`), but with the grid origin and resolutions derived from
//! the city's extent and 300px tiles, so the z/x/y are city-relative rather than
//! world-relative. This module runs a local server that, for a standard world XYZ
//! tile the game requests, resamples the overlapping `CartoMetro` tiles into a 256px
//! PNG. Both grids are Mercator, so it is a pure in-projection affine resample.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::routing::get;
use image::{RgbaImage};
use lru::LruCache;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tower_http::cors::CorsLayer;

use crate::tile_server::UnpoisonExt;

/// Earth radius used by `EPSG:3857` (half-world extent is `R·π`).
const R: f64 = 6_378_137.0;
const HALF_WORLD: f64 = std::f64::consts::PI * R;
const OUT_TILE: u32 = 256;
const CM_BASE: &str = "https://cartometro.com/images-maps";
const CM_TILE: u32 = 300;
const PREFERRED_PORT: u16 = 17972;
const BIND_ATTEMPTS: u32 = 10;
const BIND_RETRY_DELAY: Duration = Duration::from_millis(50);
const OUT_CACHE_CAPACITY: usize = 512;
const SRC_CACHE_CAPACITY: usize = 512;
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// One city's `CartoMetro` tile grid, parsed from the bundled `cities.json`.
#[derive(Deserialize, Clone)]
struct City {
    prefix: String,
    version: String,
    /// `[lon_min, lat_min, lon_max, lat_max]` in WGS84.
    extent: [f64; 4],
    center: [f64; 2],
    /// Full-map pixel `[width, height]` per `CartoMetro` zoom level.
    zoom_sizes: Vec<[u32; 2]>,
    #[allow(dead_code)]
    tile_size: u32,
}

impl City {
    /// City extent in `EPSG:3857` as `(x_min, y_min, x_max, y_max)`.
    fn merc_extent(&self) -> (f64, f64, f64, f64) {
        let [lon_min, lat_min, lon_max, lat_max] = self.extent;
        (merc_x(lon_min), merc_y(lat_min), merc_x(lon_max), merc_y(lat_max))
    }

    /// `CartoMetro` zoom whose pixel resolution is the coarsest that is still at
    /// least as fine as `res_std` (metres/px) — avoids upscaling while limiting
    /// how many source tiles a request touches. Clamped to the available range.
    fn pick_zoom(&self, res_std: f64) -> usize {
        let (cx0, _, cx1, _) = self.merc_extent();
        let width = cx1 - cx0;
        let last = self.zoom_sizes.len() - 1;
        for (cz, size) in self.zoom_sizes.iter().enumerate() {
            let res_cm = width / f64::from(size[0]);
            if res_cm <= res_std {
                return cz;
            }
        }
        last
    }

    /// Standard Web Mercator zoom range this city's data spans, for the `TileJSON`.
    fn std_zoom_range(&self) -> (u32, u32) {
        let (cx0, _, cx1, _) = self.merc_extent();
        let width = cx1 - cx0;
        let zoom_for = |px: u32| {
            let res_cm = width / f64::from(px);
            (2.0 * HALF_WORLD / f64::from(OUT_TILE) / res_cm).log2().round().max(0.0) as u32
        };
        let min = zoom_for(self.zoom_sizes[0][0]);
        let max = zoom_for(self.zoom_sizes[self.zoom_sizes.len() - 1][0]);
        (min.min(max), min.max(max))
    }
}

fn merc_x(lon: f64) -> f64 {
    R * lon.to_radians()
}

fn merc_y(lat: f64) -> f64 {
    R * (std::f64::consts::FRAC_PI_4 + lat.to_radians() / 2.0).tan().ln()
}

/// `metro-tram-london` -> `Metro Tram London`.
fn friendly_name(slug: &str) -> String {
    slug.split('-')
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn src_tile_url(city: &City, cz: usize, x: u32, y: u32) -> String {
    let p = &city.prefix;
    let v = &city.version;
    format!("{CM_BASE}/z{cz}/s{CM_TILE}/x{x}/y{y}/{p}_{cz}_{CM_TILE}_{x}_{y}.png?x={v}")
}

type SrcKey = (String, usize, u32, u32);
type OutKey = (String, u32, u32, u32);

struct ServerData {
    cities: HashMap<String, City>,
    port: u16,
    client: reqwest::Client,
    out_cache: Mutex<LruCache<OutKey, Vec<u8>>>,
    src_cache: Mutex<LruCache<SrcKey, Option<Arc<RgbaImage>>>>,
}

impl ServerData {
    /// Fetches and decodes a `CartoMetro` source tile, memoising the result (including
    /// misses, as `None`) so a viewport's repeated lookups hit at most one request.
    async fn source_tile(&self, city: &City, cz: usize, x: u32, y: u32) -> Option<Arc<RgbaImage>> {
        let key = (city.prefix.clone(), cz, x, y);
        if let Some(hit) = self.src_cache.lock().unpoison().get(&key).cloned() {
            return hit;
        }
        let decoded = self.fetch_source(city, cz, x, y).await;
        self.src_cache.lock().unpoison().put(key, decoded.clone());
        decoded
    }

    async fn fetch_source(&self, city: &City, cz: usize, x: u32, y: u32) -> Option<Arc<RgbaImage>> {
        let url = src_tile_url(city, cz, x, y);
        let resp = self.client.get(&url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let bytes = resp.bytes().await.ok()?;
        let img = image::load_from_memory(&bytes).ok()?;
        Some(Arc::new(img.to_rgba8()))
    }

    /// Renders one standard XYZ tile by resampling the city's `CartoMetro` tiles.
    /// Returns `None` when the tile lies entirely outside the city extent.
    async fn render(&self, city: &City, z: u32, x: u32, y: u32) -> Option<Vec<u8>> {
        let (cx0, cy0, cx1, cy1) = city.merc_extent();
        let world = 2.0 * HALF_WORLD;
        let n = f64::from(1u32 << z.min(30));
        let span = world / n;
        let tx0 = -HALF_WORLD + f64::from(x) * span;
        let tx1 = tx0 + span;
        let ty1 = HALF_WORLD - f64::from(y) * span;
        let ty0 = ty1 - span;

        // Bail if the requested tile doesn't overlap the city at all.
        if tx1 <= cx0 || tx0 >= cx1 || ty1 <= cy0 || ty0 >= cy1 {
            return None;
        }

        let res_std = span / f64::from(OUT_TILE);
        let cz = city.pick_zoom(res_std);
        let [sw, sh] = city.zoom_sizes[cz];
        let (sw_f, sh_f) = (f64::from(sw), f64::from(sh));

        let mut out = RgbaImage::new(OUT_TILE, OUT_TILE);
        let mut tiles: HashMap<(u32, u32), Option<Arc<RgbaImage>>> = HashMap::new();
        let mut wrote = false;

        for oy in 0..OUT_TILE {
            let my = ty1 - (f64::from(oy) + 0.5) * res_std;
            if my < cy0 || my > cy1 {
                continue;
            }
            let v = (cy1 - my) / (cy1 - cy0) * sh_f;
            for ox in 0..OUT_TILE {
                let mx = tx0 + (f64::from(ox) + 0.5) * res_std;
                if mx < cx0 || mx > cx1 {
                    continue;
                }
                let u = (mx - cx0) / (cx1 - cx0) * sw_f;
                let col = (u / f64::from(CM_TILE)) as u32 * CM_TILE;
                let row = (v / f64::from(CM_TILE)) as u32 * CM_TILE;

                let cached = tiles.get(&(col, row)).cloned();
                let src = if let Some(t) = cached {
                    t
                } else {
                    let t = self.source_tile(city, cz, col, row).await;
                    tiles.insert((col, row), t.clone());
                    t
                };
                let Some(src) = src else { continue };
                let iu = ((u - f64::from(col)) as u32).min(src.width().saturating_sub(1));
                let iv = ((v - f64::from(row)) as u32).min(src.height().saturating_sub(1));
                out.put_pixel(ox, oy, *src.get_pixel(iu, iv));
                wrote = true;
            }
        }

        if !wrote {
            return None;
        }
        let mut buf = std::io::Cursor::new(Vec::new());
        out.write_to(&mut buf, image::ImageFormat::Png).ok()?;
        Some(buf.into_inner())
    }

    fn tilejson(&self, slug: &str, city: &City) -> String {
        let (min_zoom, max_zoom) = city.std_zoom_range();
        let [lon_min, lat_min, lon_max, lat_max] = city.extent;
        serde_json::json!({
            "tilejson": "3.0.0",
            "name": format!("CartoMetro {}", friendly_name(slug)),
            "attribution": "© cartometro.com",
            "tiles": [format!("http://127.0.0.1:{}/{slug}/{{z}}/{{x}}/{{y}}.png", self.port)],
            "minzoom": min_zoom,
            "maxzoom": max_zoom,
            "bounds": [lon_min, lat_min, lon_max, lat_max],
            "center": [city.center[0], city.center[1], max_zoom],
        })
        .to_string()
    }
}

fn load_cities() -> HashMap<String, City> {
    let raw = include_str!("../resources/cartometro/cities.json");
    serde_json::from_str(raw).expect("bundled cartometro cities.json is valid")
}

// --- Server ---

pub struct ServerHandle {
    port: u16,
    shutdown_tx: watch::Sender<bool>,
}

impl ServerHandle {
    fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }
}

#[derive(Default)]
pub struct CartoMetroState {
    handle: Mutex<Option<ServerHandle>>,
}

async fn bind_local() -> Result<TcpListener, String> {
    for _ in 0..BIND_ATTEMPTS {
        if let Ok(listener) = TcpListener::bind(format!("127.0.0.1:{PREFERRED_PORT}")).await {
            return Ok(listener);
        }
        tokio::time::sleep(BIND_RETRY_DELAY).await;
    }
    TcpListener::bind("127.0.0.1:0").await.map_err(|e| e.to_string())
}

async fn start() -> Result<ServerHandle, String> {
    let listener = bind_local().await?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();

    let client = reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .user_agent("Turnout/0.2.0 (+https://github.com/SuperManifolds/Turnout)")
        .build()
        .map_err(|e| e.to_string())?;

    let data = Arc::new(ServerData {
        cities: load_cities(),
        port,
        client,
        out_cache: Mutex::new(LruCache::new(
            NonZeroUsize::new(OUT_CACHE_CAPACITY).expect("nonzero"),
        )),
        src_cache: Mutex::new(LruCache::new(
            NonZeroUsize::new(SRC_CACHE_CAPACITY).expect("nonzero"),
        )),
    });

    let app = Router::new()
        .route("/{city}/tilejson.json", get(serve_tilejson))
        .route("/{city}/{z}/{x}/{y}", get(serve_tile))
        .layer(CorsLayer::permissive())
        .with_state(data);

    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.changed().await;
            })
            .await
            .ok();
    });

    Ok(ServerHandle { port, shutdown_tx })
}

async fn serve_tilejson(
    Path(city): Path<String>,
    State(data): State<Arc<ServerData>>,
) -> impl IntoResponse {
    let json_header = [(axum::http::header::CONTENT_TYPE, "application/json")];
    match data.cities.get(&city) {
        Some(c) => (axum::http::StatusCode::OK, json_header, data.tilejson(&city, c)),
        None => (axum::http::StatusCode::NOT_FOUND, json_header, String::from("{}")),
    }
}

async fn serve_tile(
    Path((city, z, x, y)): Path<(String, u32, u32, String)>,
    State(data): State<Arc<ServerData>>,
) -> impl IntoResponse {
    let png = [(axum::http::header::CONTENT_TYPE, "image/png")];
    let Ok(y) = y.trim_end_matches(".png").parse::<u32>() else {
        return (axum::http::StatusCode::BAD_REQUEST, png, Vec::new());
    };
    let Some(c) = data.cities.get(&city).cloned() else {
        return (axum::http::StatusCode::NOT_FOUND, png, Vec::new());
    };

    let key = (city.clone(), z, x, y);
    if let Some(bytes) = data.out_cache.lock().unpoison().get(&key).cloned() {
        return (axum::http::StatusCode::OK, png, bytes);
    }
    match data.render(&c, z, x, y).await {
        Some(bytes) => {
            data.out_cache.lock().unpoison().put(key, bytes.clone());
            (axum::http::StatusCode::OK, png, bytes)
        }
        // No coverage here — 204 so the game doesn't cache/retry an error.
        None => (axum::http::StatusCode::NO_CONTENT, png, Vec::new()),
    }
}

// --- Tauri commands ---

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CityInfo {
    pub slug: String,
    pub name: String,
    pub tilejson_url: String,
    pub center: [f64; 2],
    pub min_zoom: u32,
    pub max_zoom: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CartoMetroInfo {
    pub base_url: String,
    pub cities: Vec<CityInfo>,
}

/// Starts the `CartoMetro` proxy (reusing the running one) and returns the catalog
/// of cities with their per-city `TileJSON` URLs to paste into NIMBY Rails.
#[tauri::command]
pub async fn start_cartometro(app: tauri::AppHandle) -> Result<CartoMetroInfo, String> {
    use tauri::Manager;

    let state = app.state::<CartoMetroState>();
    let port = {
        let guard = state.handle.lock().unpoison();
        guard.as_ref().map(|h| h.port)
    };
    let port = if let Some(p) = port {
        p
    } else {
        let handle = start().await?;
        let p = handle.port;
        *state.handle.lock().unpoison() = Some(handle);
        p
    };

    let mut cities: Vec<CityInfo> = load_cities()
        .into_iter()
        .map(|(slug, c)| {
            let (min_zoom, max_zoom) = c.std_zoom_range();
            CityInfo {
                name: friendly_name(&slug),
                tilejson_url: format!("http://127.0.0.1:{port}/{slug}/tilejson.json"),
                center: c.center,
                min_zoom,
                max_zoom,
                slug,
            }
        })
        .collect();
    cities.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(CartoMetroInfo { base_url: format!("http://127.0.0.1:{port}"), cities })
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn stop_cartometro(app: tauri::AppHandle) {
    use tauri::Manager;
    let state = app.state::<CartoMetroState>();
    if let Some(handle) = state.handle.lock().unpoison().take() {
        handle.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_bundled_cities_parse() {
        let cities = load_cities();
        assert!(cities.len() >= 55, "expected the full catalog, got {}", cities.len());
        let london = cities.get("metro-tram-london").expect("london present");
        assert_eq!(london.prefix, "carto_metrolondon");
        assert!(!london.zoom_sizes.is_empty());
    }

    #[test]
    fn mercator_matches_known_extent() {
        // London extent is west/south of the prime meridian & equator crossing at 0.
        let c = load_cities();
        let london = &c["metro-tram-london"];
        let (x0, y0, x1, y1) = london.merc_extent();
        assert!(x0 < 0.0 && x1 > 0.0, "london straddles the prime meridian");
        assert!(y1 > y0 && y0 > 6_000_000.0, "north London is well above the equator");
    }

    #[test]
    fn pick_zoom_prefers_finer_source() {
        let london = &load_cities()["metro-tram-london"];
        // A very coarse output resolution selects the coarsest source zoom.
        assert_eq!(london.pick_zoom(1.0e9), 0);
        // A very fine output resolution selects the finest available source zoom.
        assert_eq!(london.pick_zoom(1.0e-6), london.zoom_sizes.len() - 1);
    }

    #[test]
    fn friendly_name_titlecases_slug() {
        assert_eq!(friendly_name("metro-tram-london"), "Metro Tram London");
        assert_eq!(friendly_name("rer-idf"), "Rer Idf");
    }

    #[test]
    fn std_zoom_range_is_ordered_and_plausible() {
        let london = &load_cities()["metro-tram-london"];
        let (min, max) = london.std_zoom_range();
        assert!(min < max);
        assert!((6..=20).contains(&min) && (10..=22).contains(&max), "got {min}..{max}");
    }
}
