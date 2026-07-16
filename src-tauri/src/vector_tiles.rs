//! Serves railway geometry as Mapbox Vector Tiles (MVT), grouped by OSM vertical
//! `layer` value, for viewing in NIMBY Rails. Each vertical level becomes its own
//! named tile layer, which the game exposes as an independently toggleable layer
//! via the accompanying `TileJSON`. The numeric `layer` tag — which ORM's public
//! tiles do not carry — is read straight from the raw Overpass response.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::routing::get;
use lru::LruCache;
use mvt::{GeomData, GeomEncoder, GeomType, Tile};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tower_http::cors::CorsLayer;
use turnout_core::geo::{latlon_to_tile_pixel, tile_bounds};

use crate::overpass;
use crate::server_core::{self, ServerHandle, UnpoisonExt};

const EXTENT: u32 = 4096;
const PIXELS_PER_TILE: f64 = 256.0;
/// Clamp to ±5 to match the static Workshop mod's level vocabulary and the
/// game's `gameplay_layer` range; rarer outliers bucket into the extremes.
const LAYER_MIN: i32 = -5;
const LAYER_MAX: i32 = 5;
const MIN_ZOOM: u32 = 5;
const MAX_ZOOM: u32 = 16;
/// Reject absurd zooms before the tile-grid bit shifts (`1 << z`) misbehave; well
/// above any real request (the source advertises maxzoom 16).
const MAX_REQUEST_ZOOM: u32 = 24;
/// Fallback bound on concurrent tile encodes when core count is unavailable.
const DEFAULT_ENCODE_CONCURRENCY: usize = 4;
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

/// Encodes a run of tile-space points as an MVT linestring geometry.
fn linestring_geom(run: &[Point]) -> Result<GeomData, String> {
    let mut enc = GeomEncoder::new(GeomType::Linestring);
    for &(px, py) in run {
        enc = enc.point(px, py).map_err(|e| e.to_string())?;
    }
    enc.encode().map_err(|e| e.to_string())
}

/// Encodes a closed ring as a single-ring polygon: drops the duplicate closing
/// vertex and orients it as an exterior ring (positive signed area, which the
/// `mvt` encoder treats as exterior). Returns `None` for a degenerate ring.
fn polygon_geom(pts: &[Point]) -> Result<Option<GeomData>, String> {
    let mut ring: Vec<Point> = pts.to_vec();
    if ring.len() >= 2 && ring.first() == ring.last() {
        ring.pop();
    }
    if ring.len() < 3 {
        return Ok(None);
    }
    if signed_area(&ring) < 0.0 {
        ring.reverse();
    }
    let mut enc = GeomEncoder::new(GeomType::Polygon);
    for &(px, py) in &ring {
        enc = enc.point(px, py).map_err(|e| e.to_string())?;
    }
    enc.encode().map(Some).map_err(|e| e.to_string())
}

/// Twice the signed area (shoelace) of a ring. Positive for a clockwise ring in
/// tile pixel space (y down) — the winding the `mvt` encoder treats as exterior.
fn signed_area(ring: &[Point]) -> f64 {
    let n = ring.len();
    (0..n)
        .map(|i| {
            let (x1, y1) = ring[i];
            let (x2, y2) = ring[(i + 1) % n];
            x1 * y2 - x2 * y1
        })
        .sum()
}

/// One railway way with the tags we render/style on.
struct RailFeature {
    /// (lon, lat) vertices in order.
    coords: Vec<(f64, f64)>,
    level: i32,
    railway: String,
    /// For platforms, the rail mode served (`rail`, `subway`, `tram`, …) so the
    /// mod can colour them like the matching track type. `None` for track lines.
    mode: Option<String>,
    tunnel: bool,
    bridge: bool,
    /// Lifecycle state for not-yet-built track (`construction`/`proposed`), so the
    /// mod can style it distinctly. `None` for built track (incl. `preserved`).
    lifecycle: Option<String>,
    /// A closed platform way, emitted as a filled polygon rather than a line.
    area: bool,
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

/// Line types the mod colours; used to validate a lifecycle way's target type.
const LINE_TYPES: &[&str] =
    &["rail", "tram", "subway", "light_rail", "narrow_gauge", "monorail", "funicular"];

/// `railway=*` lifecycle values that carry a not-yet-built state (styled faint).
/// `preserved` is deliberately excluded — heritage railways are operational track.
const LIFECYCLE_STATES: &[&str] = &["construction", "proposed"];

/// The eventual line type of a lifecycle way (`construction`/`proposed`/`preserved`),
/// read from the same-named tag when it names a known type, else heavy rail.
fn lifecycle_target(tags: &std::collections::HashMap<String, String>, state: &str) -> String {
    tags.get(state)
        .map(String::as_str)
        .filter(|t| LINE_TYPES.contains(t))
        .unwrap_or("rail")
        .to_string()
}

/// The rail mode a platform serves, from OSM boolean flags (heavy rail first when
/// several apply). A bare `railway=platform` with no flag defaults to heavy rail.
/// Returns `None` for a non-rail platform (e.g. a bus `public_transport=platform`),
/// which the caller drops.
fn platform_mode(
    tags: &std::collections::HashMap<String, String>,
    has_railway_platform: bool,
) -> Option<&'static str> {
    const FLAGS: &[(&str, &str)] = &[
        ("train", "rail"),
        ("subway", "subway"),
        ("light_rail", "light_rail"),
        ("tram", "tram"),
        ("monorail", "monorail"),
        ("funicular", "funicular"),
    ];
    FLAGS
        .iter()
        .find(|(flag, _)| tags.get(*flag).map(String::as_str) == Some("yes"))
        .map(|(_, mode)| *mode)
        .or(has_railway_platform.then_some("rail"))
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
            // Platforms come in via either tagging scheme; normalise both to
            // `railway=platform` so a single style rule matches, and carry the rail
            // mode for colouring. `public_transport=platform` is mode-agnostic (bus
            // stops use it too), so a platform with no rail mode is dropped.
            // Non-platform ways must carry a railway line type (the query ensures it).
            let railway_tag = el.tags.get("railway").map(String::as_str);
            let is_platform = railway_tag == Some("platform")
                || el.tags.get("public_transport").map(String::as_str) == Some("platform");
            let (railway, mode, lifecycle) = if is_platform {
                let Some(m) = platform_mode(&el.tags, railway_tag == Some("platform")) else {
                    continue; // non-rail platform (bus, …)
                };
                ("platform".to_string(), Some(m.to_string()), None)
            } else if railway_tag == Some("preserved") {
                // Heritage railway: operational track, coloured by its target type.
                (lifecycle_target(&el.tags, "preserved"), None, None)
            } else if let Some(state) = railway_tag.filter(|r| LIFECYCLE_STATES.contains(r)) {
                // construction/proposed: colour by eventual type, flag the state.
                (lifecycle_target(&el.tags, state), None, Some(state.to_string()))
            } else if let Some(r) = railway_tag {
                (r.to_string(), None, None)
            } else {
                continue;
            };
            let level = el
                .tags
                .get("layer")
                .and_then(|v| v.trim().parse::<i32>().ok())
                .unwrap_or(0)
                .clamp(LAYER_MIN, LAYER_MAX);

            let coords: Vec<(f64, f64)> = el.geometry.iter().map(|p| (p.lon, p.lat)).collect();
            // A closed platform way (ring) is a filled area; open ways stay lines.
            let area = is_platform && coords.len() >= 4 && coords.first() == coords.last();
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
                mode,
                tunnel: truthy(el.tags.get("tunnel")),
                bridge: truthy(el.tags.get("bridge")),
                lifecycle,
                area,
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
                let tile_pts: Vec<Point> = f
                    .coords
                    .iter()
                    .map(|&(lon, lat)| {
                        let (px, py) = latlon_to_tile_pixel(lat, lon, z, x, y);
                        (f64::from(px) * scale, f64::from(py) * scale)
                    })
                    .collect();

                // Platform areas are emitted whole as polygons (the renderer clips
                // at the buffer); lines are clipped so each is only emitted where it
                // crosses this tile, not in full into every tile it touches.
                let geoms: Vec<GeomData> = if f.area {
                    polygon_geom(&tile_pts)?.into_iter().collect()
                } else {
                    let mut v = Vec::new();
                    for run in clip_polyline(&tile_pts, -TILE_BUFFER, hi) {
                        v.push(linestring_geom(&run)?);
                    }
                    v
                };

                for geom in geoms {
                    let mut feature = layer.into_feature(geom);
                    feature.add_tag_string("railway", &f.railway);
                    if let Some(mode) = &f.mode {
                        feature.add_tag_string("mode", mode);
                    }
                    if let Some(lifecycle) = &f.lifecycle {
                        feature.add_tag_string("lifecycle", lifecycle);
                    }
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
                        "railway": "Railway type (rail, tram, subway, …, or platform)",
                        "mode": "For platforms, the rail mode served (rail, subway, tram, …)",
                        "lifecycle": "\"construction\" or \"proposed\" for not-yet-built track",
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

/// The dataset, the port-aware tile-URL template, an LRU of encoded tiles, and a
/// permit pool bounding how many tile encodes run on the blocking pool at once.
struct ServerData {
    dataset: RailDataset,
    tile_template: String,
    tile_cache: Mutex<LruCache<(u32, u32, u32), Vec<u8>>>,
    encode_limit: Semaphore,
}

fn tile_url_template(port: u16) -> String {
    format!("http://127.0.0.1:{port}/{{z}}/{{x}}/{{y}}.pbf")
}

async fn start(dataset: RailDataset) -> Result<ServerHandle, String> {
    let listener = server_core::bind_with_retry(PREFERRED_PORT, BIND_ATTEMPTS, BIND_RETRY_DELAY).await?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();

    // Bound concurrent encodes to the core count so a burst of tile requests can't
    // spawn a runaway number of blocking tasks.
    let concurrency = std::thread::available_parallelism().map_or(DEFAULT_ENCODE_CONCURRENCY, std::num::NonZero::get);
    let data = Arc::new(ServerData {
        dataset,
        tile_template: tile_url_template(port),
        tile_cache: server_core::lru_cache(TILE_CACHE_CAPACITY),
        encode_limit: Semaphore::new(concurrency),
    });
    let app = Router::new()
        .route("/tilejson.json", get(serve_tilejson))
        .route("/{z}/{x}/{y}", get(serve_tile))
        .layer(CorsLayer::permissive())
        .with_state(data);

    Ok(server_core::spawn_server(listener, app))
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

    if z > MAX_REQUEST_ZOOM {
        return (axum::http::StatusCode::NO_CONTENT, content_type, Vec::new());
    }

    let key = (z, x, y);
    if let Some(bytes) = data.tile_cache.lock().unpoison().get(&key).cloned() {
        return (axum::http::StatusCode::OK, content_type, bytes);
    }

    // Tile encoding is CPU-bound (a full pass over the level's features + MVT
    // encoding). Run it on the blocking pool, bounded by a permit, so a burst of
    // tile requests during a pan can't block the shared async runtime (which also
    // serves IPC, Overpass fetches, and the other tile servers).
    let Ok(_permit) = data.encode_limit.acquire().await else {
        return (axum::http::StatusCode::SERVICE_UNAVAILABLE, content_type, Vec::new());
    };
    let d = Arc::clone(&data);
    let encoded = tokio::task::spawn_blocking(move || d.dataset.encode_tile(z, x, y)).await;
    match encoded {
        Ok(Ok(bytes)) => {
            data.tile_cache.lock().unpoison().put(key, bytes.clone());
            (axum::http::StatusCode::OK, content_type, bytes)
        }
        // An encode error, or a panic in the blocking task, fails just this tile —
        // the server keeps serving other requests.
        _ => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, content_type, Vec::new()),
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
    fn encoded_tile_names_the_level_layer_and_tags() {
        // Beyond "non-empty": MVT stores layer names and attribute keys verbatim,
        // so scan the bytes to confirm the ground-level railway line landed in the
        // `Ground` layer with a `railway` tag.
        let d = RailDataset::from_overpass_json(FIXTURE).expect("valid fixture");
        let (x, y) = turnout_core::geo::latlon_to_tile_xy(52.502, 13.42, 12);
        let near = d.encode_tile(12, x, y).expect("encode near");
        assert!(near.windows(6).any(|w| w == b"Ground"), "expected the 'Ground' layer");
        assert!(near.windows(7).any(|w| w == b"railway"), "expected the 'railway' key");
        // The far tile has no coverage, so it must not name a level layer.
        let far = d.encode_tile(12, 0, 0).expect("encode far");
        assert!(!far.windows(6).any(|w| w == b"Ground"), "far tile should be empty");
    }

    const PLATFORMS: &str = r#"{"elements":[
        {"type":"way","tags":{"railway":"platform","subway":"yes","layer":"0"},
         "geometry":[{"lat":52.5020,"lon":13.4200},{"lat":52.5022,"lon":13.4200},{"lat":52.5022,"lon":13.4205},{"lat":52.5020,"lon":13.4205},{"lat":52.5020,"lon":13.4200}]},
        {"type":"way","tags":{"public_transport":"platform","tram":"yes"},
         "geometry":[{"lat":52.5030,"lon":13.4210},{"lat":52.5032,"lon":13.4215}]},
        {"type":"way","tags":{"public_transport":"platform","highway":"platform","bus":"yes"},
         "geometry":[{"lat":52.5040,"lon":13.4220},{"lat":52.5042,"lon":13.4225}]}
    ]}"#;

    #[test]
    fn rail_platforms_normalise_and_carry_mode_bus_dropped() {
        let d = RailDataset::from_overpass_json(PLATFORMS).expect("valid platforms");
        assert_eq!(d.features.len(), 2, "the bus platform must be dropped");
        assert!(d.features.iter().all(|f| f.railway == "platform"));
        let modes: BTreeSet<&str> = d.features.iter().filter_map(|f| f.mode.as_deref()).collect();
        assert_eq!(modes, BTreeSet::from(["subway", "tram"]));
        // The closed railway=platform ring is an area; the open PT way stays a line.
        assert_eq!(d.features.iter().filter(|f| f.area).count(), 1);
    }

    #[test]
    fn platform_mode_prefers_heavy_rail_and_defaults_for_bare_platform() {
        let mut both = std::collections::HashMap::new();
        both.insert("subway".to_string(), "yes".to_string());
        both.insert("train".to_string(), "yes".to_string());
        assert_eq!(platform_mode(&both, true), Some("rail"));
        let bare = std::collections::HashMap::new();
        assert_eq!(platform_mode(&bare, true), Some("rail")); // railway=platform, no flag
        assert_eq!(platform_mode(&bare, false), None); // not a rail platform
    }

    const LIFECYCLE: &str = r#"{"elements":[
        {"type":"way","tags":{"railway":"construction","construction":"subway"},
         "geometry":[{"lat":52.50,"lon":13.40},{"lat":52.51,"lon":13.41}]},
        {"type":"way","tags":{"railway":"proposed","proposed":"tram"},
         "geometry":[{"lat":52.50,"lon":13.42},{"lat":52.51,"lon":13.43}]},
        {"type":"way","tags":{"railway":"preserved","preserved":"narrow_gauge"},
         "geometry":[{"lat":52.50,"lon":13.44},{"lat":52.51,"lon":13.45}]},
        {"type":"way","tags":{"railway":"construction"},
         "geometry":[{"lat":52.50,"lon":13.46},{"lat":52.51,"lon":13.47}]}
    ]}"#;

    #[test]
    fn lifecycle_ways_resolve_to_target_type_and_flag() {
        let d = RailDataset::from_overpass_json(LIFECYCLE).expect("valid");
        let by = |r: &str| d.features.iter().find(|f| f.railway == r).expect(r);
        // construction=subway -> subway line, flagged construction.
        assert_eq!(by("subway").lifecycle.as_deref(), Some("construction"));
        // proposed=tram -> tram line, flagged proposed.
        assert_eq!(by("tram").lifecycle.as_deref(), Some("proposed"));
        // preserved -> its target type, treated as built (no lifecycle flag).
        assert_eq!(by("narrow_gauge").lifecycle, None);
        // Bare railway=construction with no target -> defaults to heavy rail.
        assert_eq!(by("rail").lifecycle.as_deref(), Some("construction"));
    }

    #[test]
    fn lifecycle_target_validates_type() {
        let mut tags = std::collections::HashMap::new();
        tags.insert("construction".to_string(), "light_rail".to_string());
        assert_eq!(lifecycle_target(&tags, "construction"), "light_rail");
        tags.insert("construction".to_string(), "not_a_type".to_string());
        assert_eq!(lifecycle_target(&tags, "construction"), "rail"); // unknown -> rail
        assert_eq!(lifecycle_target(&std::collections::HashMap::new(), "proposed"), "rail");
    }

    #[test]
    fn encodes_platform_polygon_tile() {
        let d = RailDataset::from_overpass_json(PLATFORMS).expect("valid platforms");
        let (x, y) = turnout_core::geo::latlon_to_tile_xy(52.5021, 13.4202, 16);
        assert!(!d.encode_tile(16, x, y).expect("encode platform tile").is_empty());
    }

    #[test]
    fn exterior_ring_winding_is_normalised() {
        // A clockwise and a counter-clockwise square must both encode (the encoder
        // treats positive signed area as the exterior ring, so we orient to it).
        let cw = [(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)];
        let ccw = [(0.0, 0.0), (0.0, 10.0), (10.0, 10.0), (10.0, 0.0)];
        assert!(polygon_geom(&cw).expect("cw").is_some());
        assert!(polygon_geom(&ccw).expect("ccw").is_some());
        assert!(polygon_geom(&[(0.0, 0.0), (1.0, 1.0)]).expect("degenerate").is_none());
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
