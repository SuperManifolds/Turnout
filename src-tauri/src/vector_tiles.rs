//! Serves railway geometry as Mapbox Vector Tiles (MVT), grouped by OSM vertical
//! `layer` value, for viewing in NIMBY Rails. Each vertical level becomes its own
//! named tile layer, which the game exposes as an independently toggleable layer
//! via the accompanying `TileJSON`. The numeric `layer` tag — which ORM's public
//! tiles do not carry — is read straight from the raw Overpass response.

use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::routing::get;
use lru::LruCache;
use mvt::{GeomEncoder, GeomType, Tile};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tower_http::cors::CorsLayer;
use turnout_core::geo::{latlon_to_tile_pixel, tile_bounds};

use crate::overpass;
use crate::tile_server::UnpoisonExt;

const EXTENT: u32 = 4096;
const PIXELS_PER_TILE: f64 = 256.0;
/// Clamp to ±5 to match the static Workshop mod's level vocabulary and the
/// game's `gameplay_layer` range; rarer outliers bucket into the extremes.
const LAYER_MIN: i32 = -5;
const LAYER_MAX: i32 = 5;
const MIN_ZOOM: u32 = 5;
const MAX_ZOOM: u32 = 16;
const PREFERRED_PORT: u16 = 17971;
const MVT_CONTENT_TYPE: &str = "application/vnd.mapbox-vector-tile";
/// Extent-space padding when clipping geometry to a tile (matches `ST_AsMVTGeom`'s
/// default), so lines crossing tile edges join cleanly at render time.
const TILE_BUFFER: f64 = 64.0;
const TILE_CACHE_CAPACITY: usize = 1024;
/// A just-stopped server can hold the preferred port briefly; retry before
/// falling back to an ephemeral port so the tile URL stays stable across restarts.
const BIND_ATTEMPTS: u32 = 10;
const BIND_RETRY_DELAY: Duration = Duration::from_millis(50);
/// Steam Workshop page for the companion "ORM Vertical Layers" styling mod.
const WORKSHOP_MOD_URL: &str = "https://steamcommunity.com/sharedfiles/filedetails/?id=3763998309";

/// Human-friendly, single-token layer name shown in the game's toggle list and
/// matched by the mod's `source_layer`. Single token (no spaces) because NIMBY's
/// stylesheet parser treats `source_layer` as one token: -2 -> `Underground2`,
/// 0 -> `Ground`, 1 -> `Elevated1`.
fn layer_name(level: i32) -> String {
    match level {
        0 => "Ground".to_string(),
        l if l < 0 => format!("Underground{}", -l),
        l => format!("Elevated{l}"),
    }
}

fn describe_level(level: i32) -> String {
    match level {
        0 => "Ground level".to_string(),
        l if l < 0 => format!("Underground (layer {l})"),
        l => format!("Elevated (layer +{l})"),
    }
}

type Point = (f64, f64);
/// Visible portion of a clipped segment: `(start, end, start_is_original_vertex,
/// end_is_original_vertex)`.
type ClipResult = (Point, Point, bool, bool);

/// Clip segment `a`->`b` to the axis-aligned box `[lo, hi]^2` (Liang-Barsky).
/// The two flags say whether each endpoint is the original (unclipped) vertex,
/// used to stitch consecutive segments into continuous runs.
fn clip_segment(a: Point, b: Point, lo: f64, hi: f64) -> Option<ClipResult> {
    let (mut t0, mut t1) = (0.0_f64, 1.0_f64);
    let dx = b.0 - a.0;
    let dy = b.1 - a.1;
    for (p, q) in [(-dx, a.0 - lo), (dx, hi - a.0), (-dy, a.1 - lo), (dy, hi - a.1)] {
        if p == 0.0 {
            if q < 0.0 {
                return None; // parallel to this edge and outside it
            }
        } else {
            let r = q / p;
            if p < 0.0 {
                if r > t1 {
                    return None;
                }
                t0 = t0.max(r);
            } else {
                if r < t0 {
                    return None;
                }
                t1 = t1.min(r);
            }
        }
    }
    let p0 = (a.0 + t0 * dx, a.1 + t0 * dy);
    let p1 = (a.0 + t1 * dx, a.1 + t1 * dy);
    Some((p0, p1, t0 == 0.0, t1 == 1.0))
}

/// Clip a polyline to the box, returning the contiguous visible runs (each with
/// at least two points). A line that leaves and re-enters the box yields
/// multiple runs.
fn clip_polyline(points: &[Point], lo: f64, hi: f64) -> Vec<Vec<Point>> {
    let mut runs: Vec<Vec<Point>> = Vec::new();
    let mut run: Vec<Point> = Vec::new();
    let mut prev_ended_at_vertex = false;

    let flush = |run: &mut Vec<Point>, runs: &mut Vec<Vec<Point>>| {
        if run.len() >= 2 {
            runs.push(std::mem::take(run));
        } else {
            run.clear();
        }
    };

    for w in points.windows(2) {
        if let Some((p0, p1, starts_at_a, ends_at_b)) = clip_segment(w[0], w[1], lo, hi) {
            // Continue the current run only when this segment starts exactly
            // where the previous one ended (a shared, in-box vertex).
            if run.is_empty() || !(starts_at_a && prev_ended_at_vertex) {
                flush(&mut run, &mut runs);
                run.push(p0);
            }
            run.push(p1);
            prev_ended_at_vertex = ends_at_b;
        } else {
            flush(&mut run, &mut runs);
            prev_ended_at_vertex = false;
        }
    }
    flush(&mut run, &mut runs);
    runs
}

/// One railway way with the tags we render/style on.
struct RailFeature {
    /// (lon, lat) vertices in order.
    coords: Vec<(f64, f64)>,
    level: i32,
    railway: String,
    tunnel: bool,
    bridge: bool,
    /// `(min_lon, min_lat, max_lon, max_lat)` bounding box for tile-intersection.
    bbox: (f64, f64, f64, f64),
}

/// Parsed railway dataset ready to tile.
pub struct RailDataset {
    features: Vec<RailFeature>,
    /// Distinct vertical levels present, ascending.
    levels: Vec<i32>,
    /// (south, west, north, east) covered by the data.
    bounds: (f64, f64, f64, f64),
}

// --- Overpass `[out:json]` subset ---

#[derive(Deserialize)]
struct OverpassJson {
    #[serde(default)]
    elements: Vec<Element>,
}

#[derive(Deserialize)]
struct Element {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    geometry: Vec<LatLon>,
    #[serde(default)]
    tags: std::collections::HashMap<String, String>,
}

#[derive(Deserialize)]
struct LatLon {
    lat: f64,
    lon: f64,
}

fn truthy(v: Option<&String>) -> bool {
    matches!(v.map(String::as_str), Some("yes" | "true" | "1"))
}

impl RailDataset {
    /// Parse the raw Overpass response into per-level railway geometry.
    fn from_overpass_json(json: &str) -> Result<Self, overpass::OverpassError> {
        let parsed: OverpassJson = serde_json::from_str(json)
            .map_err(|e| overpass::OverpassError::BadRequest(format!("unexpected Overpass response: {e}")))?;

        let mut features = Vec::new();
        let mut levels = BTreeSet::new();
        let (mut s, mut w, mut n, mut e) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);

        for el in parsed.elements {
            if el.kind != "way" || el.geometry.len() < 2 {
                continue;
            }
            let Some(railway) = el.tags.get("railway").cloned() else { continue };
            let level = el
                .tags
                .get("layer")
                .and_then(|v| v.trim().parse::<i32>().ok())
                .unwrap_or(0)
                .clamp(LAYER_MIN, LAYER_MAX);

            let coords: Vec<(f64, f64)> = el.geometry.iter().map(|p| (p.lon, p.lat)).collect();
            let (mut fw, mut fs, mut fe, mut fn_) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
            for &(lon, lat) in &coords {
                fw = fw.min(lon);
                fe = fe.max(lon);
                fs = fs.min(lat);
                fn_ = fn_.max(lat);
            }
            s = s.min(fs);
            w = w.min(fw);
            n = n.max(fn_);
            e = e.max(fe);
            levels.insert(level);

            features.push(RailFeature {
                coords,
                level,
                railway,
                tunnel: truthy(el.tags.get("tunnel")),
                bridge: truthy(el.tags.get("bridge")),
                bbox: (fw, fs, fe, fn_),
            });
        }

        if features.is_empty() {
            return Err(overpass::OverpassError::Empty);
        }
        Ok(Self { features, levels: levels.into_iter().collect(), bounds: (s, w, n, e) })
    }

    /// Encode one MVT tile: one named layer per vertical level, holding the
    /// railway lines at that level that intersect the tile.
    fn encode_tile(&self, z: u32, x: u32, y: u32) -> Result<Vec<u8>, String> {
        let (tw, ts, te, tn) = tile_bounds(z, x, y);
        let mut tile = Tile::new(EXTENT);

        for &level in &self.levels {
            let mut layer = tile.create_layer(&layer_name(level));
            let mut any = false;

            let scale = f64::from(EXTENT) / PIXELS_PER_TILE;
            let hi = f64::from(EXTENT) + TILE_BUFFER;
            for f in self.features.iter().filter(|f| f.level == level) {
                let (fw, fs, fe, fn_) = f.bbox;
                if fe < tw || fw > te || fn_ < ts || fs > tn {
                    continue; // feature bbox doesn't touch this tile
                }
                let tile_pts: Vec<(f64, f64)> = f
                    .coords
                    .iter()
                    .map(|&(lon, lat)| {
                        let (px, py) = latlon_to_tile_pixel(lat, lon, z, x, y);
                        (f64::from(px) * scale, f64::from(py) * scale)
                    })
                    .collect();

                // Clip to the tile (with buffer) so a line is only emitted where
                // it actually crosses this tile, not in full into every tile.
                for run in clip_polyline(&tile_pts, -TILE_BUFFER, hi) {
                    let mut enc = GeomEncoder::new(GeomType::Linestring);
                    for &(px, py) in &run {
                        enc = enc.point(px, py).map_err(|e| e.to_string())?;
                    }
                    let geom = enc.encode().map_err(|e| e.to_string())?;
                    let mut feature = layer.into_feature(geom);
                    feature.add_tag_string("railway", &f.railway);
                    if f.tunnel {
                        feature.add_tag_string("tunnel", "yes");
                    }
                    if f.bridge {
                        feature.add_tag_string("bridge", "yes");
                    }
                    layer = feature.into_layer();
                    any = true;
                }
            }

            if any {
                tile.add_layer(layer).map_err(|e| e.to_string())?;
            }
        }

        tile.to_bytes().map_err(|e| e.to_string())
    }

    /// `TileJSON` advertising each vertical level as a toggleable `vector_layer`.
    fn tilejson(&self, tile_url_template: &str) -> String {
        let (s, w, n, e) = self.bounds;
        let vector_layers: Vec<serde_json::Value> = self
            .levels
            .iter()
            .map(|&level| {
                serde_json::json!({
                    "id": layer_name(level),
                    "description": describe_level(level),
                    "fields": {
                        "railway": "Railway type (rail, tram, subway, …)",
                        "tunnel": "\"yes\" if in a tunnel",
                        "bridge": "\"yes\" if on a bridge"
                    }
                })
            })
            .collect();

        serde_json::json!({
            "tilejson": "3.0.0",
            "name": "ORM vertical layers",
            "tiles": [tile_url_template],
            "minzoom": MIN_ZOOM,
            "maxzoom": MAX_ZOOM,
            "bounds": [w, s, e, n],
            "vector_layers": vector_layers
        })
        .to_string()
    }

    /// The vertical levels present, with their tile-layer names (for the mod).
    pub fn level_layers(&self) -> Vec<(i32, String)> {
        self.levels.iter().map(|&l| (l, layer_name(l))).collect()
    }
}

// --- HTTP server ---

pub struct ServerHandle {
    pub port: u16,
    shutdown_tx: watch::Sender<bool>,
}

impl ServerHandle {
    fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }
}

/// Live server state managed by Tauri (at most one running at a time).
#[derive(Default)]
pub struct VectorLayerState {
    handle: Mutex<Option<ServerHandle>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VectorLayersInfo {
    pub tilejson_url: String,
    pub tile_url: String,
    pub levels: Vec<LevelInfo>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LevelInfo {
    pub level: i32,
    pub layer_name: String,
    pub description: String,
}

/// The dataset, the port-aware tile-URL template, and an LRU of encoded tiles.
struct ServerData {
    dataset: RailDataset,
    tile_template: String,
    tile_cache: Mutex<LruCache<(u32, u32, u32), Vec<u8>>>,
}

fn tile_url_template(port: u16) -> String {
    format!("http://127.0.0.1:{port}/{{z}}/{{x}}/{{y}}.pbf")
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

async fn start(dataset: RailDataset) -> Result<ServerHandle, String> {
    let listener = bind_local().await?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();

    let cache = LruCache::new(NonZeroUsize::new(TILE_CACHE_CAPACITY).expect("nonzero capacity"));
    let data = Arc::new(ServerData {
        dataset,
        tile_template: tile_url_template(port),
        tile_cache: Mutex::new(cache),
    });
    let app = Router::new()
        .route("/tilejson.json", get(serve_tilejson))
        .route("/{z}/{x}/{y}", get(serve_tile))
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

async fn serve_tilejson(State(data): State<Arc<ServerData>>) -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        data.dataset.tilejson(&data.tile_template),
    )
}

async fn serve_tile(
    Path((z, x, y)): Path<(u32, u32, String)>,
    State(data): State<Arc<ServerData>>,
) -> impl IntoResponse {
    let content_type = [(axum::http::header::CONTENT_TYPE, MVT_CONTENT_TYPE)];
    let Ok(y) = y.trim_end_matches(".pbf").trim_end_matches(".mvt").parse::<u32>() else {
        return (axum::http::StatusCode::BAD_REQUEST, content_type, Vec::new());
    };

    let key = (z, x, y);
    if let Some(bytes) = data.tile_cache.lock().unpoison().get(&key).cloned() {
        return (axum::http::StatusCode::OK, content_type, bytes);
    }
    match data.dataset.encode_tile(z, x, y) {
        Ok(bytes) => {
            data.tile_cache.lock().unpoison().put(key, bytes.clone());
            (axum::http::StatusCode::OK, content_type, bytes)
        }
        Err(_) => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, content_type, Vec::new()),
    }
}

// --- Tauri commands ---

#[tauri::command]
pub async fn start_orm_vector_layers(
    app: tauri::AppHandle,
    south: f64,
    west: f64,
    north: f64,
    east: f64,
    timeout_secs: u32,
) -> Result<VectorLayersInfo, String> {
    use tauri::Manager;

    let body = overpass::fetch_railways(south, west, north, east, timeout_secs)
        .await
        .map_err(|e| e.to_string())?;
    let dataset = RailDataset::from_overpass_json(&body).map_err(|e| e.to_string())?;

    let levels: Vec<LevelInfo> = dataset
        .level_layers()
        .into_iter()
        .map(|(level, layer_name)| LevelInfo { level, layer_name, description: describe_level(level) })
        .collect();

    // Stop any running server first so the new one can reuse the preferred port
    // (keeping the URL the user pasted into NIMBY Rails stable across restarts).
    let state = app.state::<VectorLayerState>();
    if let Some(old) = state.handle.lock().unpoison().take() {
        old.shutdown();
    }

    let handle = start(dataset).await?;
    let port = handle.port;
    *state.handle.lock().unpoison() = Some(handle);

    Ok(VectorLayersInfo {
        tilejson_url: format!("http://127.0.0.1:{port}/tilejson.json"),
        tile_url: tile_url_template(port),
        levels,
    })
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn stop_orm_vector_layers(app: tauri::AppHandle) {
    use tauri::Manager;
    let state = app.state::<VectorLayerState>();
    if let Some(handle) = state.handle.lock().unpoison().take() {
        handle.shutdown();
    }
}

/// Open the companion styling mod's Steam Workshop page in the default browser.
#[tauri::command]
pub fn open_workshop_mod() -> Result<(), String> {
    tauri_plugin_opener::open_url(WORKSHOP_MOD_URL, None::<&str>)
        .map_err(|e| format!("Failed to open Workshop page: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{"elements":[
        {"type":"way","tags":{"railway":"rail","layer":"-2","tunnel":"yes"},
         "geometry":[{"lat":52.500,"lon":13.400},{"lat":52.505,"lon":13.410}]},
        {"type":"way","tags":{"railway":"rail"},
         "geometry":[{"lat":52.500,"lon":13.420},{"lat":52.505,"lon":13.430}]},
        {"type":"way","tags":{"railway":"tram","layer":"1","bridge":"yes"},
         "geometry":[{"lat":52.500,"lon":13.440},{"lat":52.505,"lon":13.450}]},
        {"type":"node","id":1}
    ]}"#;

    #[test]
    fn parses_distinct_levels_with_default_zero() {
        let d = RailDataset::from_overpass_json(FIXTURE).expect("valid fixture");
        assert_eq!(d.levels, vec![-2, 0, 1]);
    }

    #[test]
    fn layer_names_are_stylesheet_safe() {
        assert_eq!(layer_name(-2), "Underground2");
        assert_eq!(layer_name(0), "Ground");
        assert_eq!(layer_name(1), "Elevated1");
    }

    #[test]
    fn empty_result_is_classified() {
        assert!(matches!(
            RailDataset::from_overpass_json(r#"{"elements":[]}"#),
            Err(overpass::OverpassError::Empty)
        ));
    }

    #[test]
    fn tilejson_lists_one_vector_layer_per_level() {
        let d = RailDataset::from_overpass_json(FIXTURE).expect("valid fixture");
        let tj: serde_json::Value = serde_json::from_str(&d.tilejson("http://x/{z}/{x}/{y}.pbf")).expect("valid tilejson");
        let vls = tj["vector_layers"].as_array().expect("vector_layers array");
        assert_eq!(vls.len(), 3);
        let ids: Vec<&str> = vls.iter().map(|v| v["id"].as_str().expect("id string")).collect();
        assert!(ids.contains(&"Underground2") && ids.contains(&"Ground") && ids.contains(&"Elevated1"));
        assert_eq!(tj["tilejson"], "3.0.0");
    }

    #[test]
    fn encodes_populated_tile_and_empty_far_tile() {
        let d = RailDataset::from_overpass_json(FIXTURE).expect("valid fixture");
        let (x, y) = turnout_core::geo::latlon_to_tile_xy(52.502, 13.42, 12);
        let near = d.encode_tile(12, x, y).expect("encode near");
        let far = d.encode_tile(12, 0, 0).expect("encode far");
        assert!(!near.is_empty());
        assert!(far.len() < near.len(), "far tile should carry no layers");
    }

    #[test]
    fn clip_keeps_an_inside_line_whole() {
        let line = [(10.0, 10.0), (50.0, 50.0), (90.0, 90.0)];
        assert_eq!(clip_polyline(&line, 0.0, 100.0), vec![line.to_vec()]);
    }

    #[test]
    fn clip_drops_a_fully_outside_line() {
        assert!(clip_polyline(&[(-50.0, 50.0), (-10.0, 50.0)], 0.0, 100.0).is_empty());
    }

    #[test]
    fn clip_splits_a_line_that_leaves_and_reenters() {
        let line = [(10.0, 50.0), (50.0, 50.0), (200.0, 50.0), (90.0, 50.0)];
        let runs = clip_polyline(&line, 0.0, 100.0);
        assert_eq!(runs.len(), 2);
        for run in &runs {
            for &(x, y) in run {
                assert!((0.0..=100.0).contains(&x) && (0.0..=100.0).contains(&y));
            }
        }
    }

    #[test]
    fn mod_source_layers_match_tiler_names() {
        // The committed mod's `source_layer` names must equal the tiler's layer
        // names for every level, or the mod silently renders nothing.
        let mod_txt = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../mods/orm-vertical-layers/mod.txt"
        ))
        .expect("read committed mod.txt");
        let expected: std::collections::BTreeSet<String> =
            (LAYER_MIN..=LAYER_MAX).map(layer_name).collect();
        let used: std::collections::BTreeSet<String> = mod_txt
            .lines()
            .filter_map(|l| l.trim().strip_prefix("source_layer = "))
            .map(|s| s.trim().to_string())
            .collect();
        assert_eq!(
            used, expected,
            "mod source_layers drifted from layer_name — rerun scripts/gen_orm_layers_mod.py"
        );
    }
}
