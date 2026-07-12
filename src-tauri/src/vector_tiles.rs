//! Serves railway geometry as Mapbox Vector Tiles (MVT), grouped by OSM vertical
//! `layer` value, for viewing in NIMBY Rails. Each vertical level becomes its own
//! named tile layer, which the game exposes as an independently toggleable layer
//! via the accompanying `TileJSON`. The numeric `layer` tag — which ORM's public
//! tiles do not carry — is read straight from the raw Overpass response.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::routing::get;
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

/// Layer name for a vertical level: -2 -> `rail_layer_m2`, 1 -> `rail_layer_p1`.
/// Underscore/letter form avoids any INI-parsing ambiguity in the mod stylesheet.
fn layer_name(level: i32) -> String {
    match level {
        0 => "rail_layer_0".to_string(),
        l if l < 0 => format!("rail_layer_m{}", -l),
        l => format!("rail_layer_p{l}"),
    }
}

fn describe_level(level: i32) -> String {
    match level {
        0 => "Ground level".to_string(),
        l if l < 0 => format!("Underground (layer {l})"),
        l => format!("Elevated (layer +{l})"),
    }
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

            for f in self.features.iter().filter(|f| f.level == level) {
                let (fw, fs, fe, fn_) = f.bbox;
                if fe < tw || fw > te || fn_ < ts || fs > tn {
                    continue; // feature bbox doesn't touch this tile
                }
                let mut enc = GeomEncoder::new(GeomType::Linestring);
                for &(lon, lat) in &f.coords {
                    let (px, py) = latlon_to_tile_pixel(lat, lon, z, x, y);
                    let scale = f64::from(EXTENT) / PIXELS_PER_TILE;
                    enc = enc
                        .point(f64::from(px) * scale, f64::from(py) * scale)
                        .map_err(|e| e.to_string())?;
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

/// The dataset plus the port-aware tile-URL template, shared with the handlers.
struct ServerData {
    dataset: RailDataset,
    tile_template: String,
}

fn tile_url_template(port: u16) -> String {
    format!("http://127.0.0.1:{port}/{{z}}/{{x}}/{{y}}.pbf")
}

async fn start(dataset: RailDataset) -> Result<ServerHandle, String> {
    let listener = match TcpListener::bind(format!("127.0.0.1:{PREFERRED_PORT}")).await {
        Ok(l) => l,
        Err(_) => TcpListener::bind("127.0.0.1:0").await.map_err(|e| e.to_string())?,
    };
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();

    let data = Arc::new(ServerData { dataset, tile_template: tile_url_template(port) });
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
    match data.dataset.encode_tile(z, x, y) {
        Ok(bytes) => (axum::http::StatusCode::OK, content_type, bytes),
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

    let handle = start(dataset).await?;
    let port = handle.port;

    let state = app.state::<VectorLayerState>();
    let mut slot = state.handle.lock().unpoison();
    if let Some(old) = slot.replace(handle) {
        old.shutdown();
    }
    drop(slot);

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
        assert_eq!(layer_name(-2), "rail_layer_m2");
        assert_eq!(layer_name(0), "rail_layer_0");
        assert_eq!(layer_name(1), "rail_layer_p1");
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
        assert!(ids.contains(&"rail_layer_m2") && ids.contains(&"rail_layer_0") && ids.contains(&"rail_layer_p1"));
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
}
