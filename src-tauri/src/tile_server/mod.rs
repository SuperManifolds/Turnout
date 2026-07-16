use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use crate::error::{CommandError, CommandResult};
use crate::server_core::{self, UnpoisonExt};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use lru::LruCache;
use tiny_skia::Pixmap;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tower_http::cors::CorsLayer;
use turnout_core::kml::OverlayData;

mod fetch;
mod render;

use fetch::{
    RemoteReq, decode_remote_bytes, decoded_to_pixmap, fetch_arcgis_tile, fetch_wms_tile,
    fetch_xyz_tile, get_remote_cached, mbtiles_conn, put_remote_cached, read_mbtiles_tile,
};
use render::render_tile;

pub(crate) use fetch::xyz_to_quadkey;

const TILE_SIZE: u32 = 256;
const RENDER_CACHE_CAPACITY: usize = 512;
const REMOTE_CACHE_CAPACITY: usize = 2048;
/// Max cached open `MBTiles` read connections; bounds file descriptors when many
/// distinct `MBTiles` layers are added. Evicting closes the connection once unused.
const MBTILES_CONN_CACHE: usize = 16;
pub const PREFERRED_PORT: u16 = 17853;
const MAX_ZOOM: u8 = 22;
const HTTP_TIMEOUT_SECS: u64 = 15;
/// Substring unique to Apple's satellite tile host, used to tell a satellite Apple
/// layer from a standard-map one when a layer is added.
pub const APPLE_SAT_MARKER: &str = "sat-cdn";

const WEB_MERCATOR_EXTENT: (f64, f64, f64, f64) = (-180.0, -85.051_129, 180.0, 85.051_129);


pub(crate) struct DecodedImage {
    pixmap: Pixmap,
    north: f64,
    south: f64,
    east: f64,
    west: f64,
    rotation: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LayerKind {
    Kmz, Shp, GeoJson, Wms, ArcGis, Xyz, Wmts, Apple, Bing, MbTiles,
}

/// A layer's data source: the single, self-describing, serializable definition of
/// where a layer's tiles come from. This is both the live description and the
/// persisted form — there is no parallel "saved" type. Runtime-only data (parsed
/// file geometry + rasterized ground overlays for file layers, and Apple's live
/// tokenized URL) is NOT here; it lives in [`TileState::runtime`], keyed by layer
/// id, so the definition stays serializable and free of secrets.
#[derive(Clone)]
pub enum SourceDef {
    Kmz { path: Option<String> },
    Shp { path: Option<String> },
    GeoJson { path: Option<String> },
    Wms { base_url: String, layer_name: String },
    ArcGis { base_url: String, service_name: String },
    Xyz { url_template: String },
    Wmts { url_template: String },
    /// Apple imagery. Only whether it is satellite is persisted (via `SavedSource`);
    /// the tokenized URL is runtime state in [`TileState::runtime`].
    Apple { sat: bool },
    Bing { url_template: String },
    MbTiles { path: String, max_zoom: u8 },
}

impl SourceDef {
    pub fn kind(&self) -> LayerKind {
        match self {
            SourceDef::Kmz { .. } => LayerKind::Kmz,
            SourceDef::Shp { .. } => LayerKind::Shp,
            SourceDef::GeoJson { .. } => LayerKind::GeoJson,
            SourceDef::Wms { .. } => LayerKind::Wms,
            SourceDef::ArcGis { .. } => LayerKind::ArcGis,
            SourceDef::Xyz { .. } => LayerKind::Xyz,
            SourceDef::Wmts { .. } => LayerKind::Wmts,
            SourceDef::Apple { .. } => LayerKind::Apple,
            SourceDef::Bing { .. } => LayerKind::Bing,
            SourceDef::MbTiles { .. } => LayerKind::MbTiles,
        }
    }

    /// True for the file-backed kinds, whose geometry is rendered locally from
    /// [`RuntimeData::File`] rather than fetched as remote raster tiles.
    fn is_file(&self) -> bool {
        matches!(self, SourceDef::Kmz { .. } | SourceDef::Shp { .. } | SourceDef::GeoJson { .. })
    }
}

/// Runtime-only companion to a [`Layer`], held in [`TileState::runtime`] keyed by
/// layer id. Never persisted: it is rebuilt on restore (re-parse the file, or
/// rebuild Apple's URL from credentials).
pub enum RuntimeData {
    /// Parsed geometry + rasterized ground overlays for a file layer.
    File { data: OverlayData, images: Vec<DecodedImage> },
    /// The live tokenized Apple tile URL.
    Apple { url_template: String },
}

/// A layer: its data-source definition plus display state. The persisted form
/// lives in `overlay::SavedSource`, mapped 1:1 from `source` on save.
#[derive(Clone)]
pub struct Layer {
    pub id: u32,
    pub name: String,
    pub bbox: (f64, f64, f64, f64),
    pub visible: bool,
    pub opacity: f32,
    pub source: SourceDef,
}

impl Layer {
    pub fn kind(&self) -> LayerKind {
        self.source.kind()
    }
}

type DecodedTile = (Vec<u8>, u32, u32);
type RemoteCache = LruCache<(u32, u8, u32, u32), DecodedTile>;

pub struct TileState {
    pub(crate) layers: RwLock<Vec<Layer>>,
    /// Runtime-only per-layer data (parsed file geometry / rasterized overlays, and
    /// Apple's live URL), keyed by layer id. Rebuilt on restore; never persisted.
    runtime: RwLock<HashMap<u32, RuntimeData>>,
    /// Composited PNG tiles keyed by `(z, x, y)`. Single-stripe (this server sees
    /// modest traffic); `Bytes` so a hit shares the buffer instead of copying.
    render_cache: server_core::TileCache<(u8, u32, u32)>,
    remote_cache: Mutex<RemoteCache>,
    pub(crate) error_layers: Mutex<std::collections::HashSet<u32>>,
    next_id: Mutex<u32>,
    port: u16,
    http: reqwest::Client,
    /// Path-keyed read-only `MBTiles` connections, reused across tile requests to
    /// avoid reopening the `SQLite` file on every fetch.
    mbtiles_conns: Mutex<LruCache<String, Arc<Mutex<rusqlite::Connection>>>>,
}

pub struct ServerHandle {
    pub port: u16,
    pub state: Arc<TileState>,
    pub shutdown_tx: watch::Sender<bool>,
}

impl ServerHandle {
    /// Adds a KMZ/Shp/GeoJson layer, returning its new id, or `None` when the file
    /// carries no drawable geometry (no bounding box) and nothing was added.
    pub fn add_kmz_layer(&self, data: OverlayData, path: Option<String>, kind: LayerKind) -> Option<u32> {
        let bbox = data.bbox()?;
        let name = data.name.clone().unwrap_or_else(|| "Overlay".to_string());
        let images = decode_images(&data);
        let id = self.next_id();
        let source = match kind {
            LayerKind::Shp => SourceDef::Shp { path },
            LayerKind::GeoJson => SourceDef::GeoJson { path },
            _ => SourceDef::Kmz { path },
        };
        self.state.runtime.write().unpoison().insert(id, RuntimeData::File { data, images });
        self.push_layer(Layer { id, name, bbox, visible: true, opacity: 1.0, source });
        Some(id)
    }

    pub fn add_wms_layer(&self, base_url: String, layer_name: String, display_name: String) -> u32 {
        let id = self.next_id();
        self.push_layer(Layer {
            id, name: display_name, bbox: WEB_MERCATOR_EXTENT, visible: true, opacity: 1.0,
            source: SourceDef::Wms { base_url, layer_name },
        });
        id
    }

    pub fn add_xyz_layer_with_kind(&self, url_template: String, display_name: String, kind: LayerKind) -> u32 {
        let id = self.next_id();
        let source = match kind {
            LayerKind::Wmts => SourceDef::Wmts { url_template },
            LayerKind::Bing => SourceDef::Bing { url_template },
            LayerKind::Apple => {
                let sat = url_template.contains(APPLE_SAT_MARKER);
                self.state.runtime.write().unpoison().insert(id, RuntimeData::Apple { url_template });
                SourceDef::Apple { sat }
            }
            _ => SourceDef::Xyz { url_template },
        };
        self.push_layer(Layer {
            id, name: display_name, bbox: WEB_MERCATOR_EXTENT, visible: true, opacity: 1.0, source,
        });
        id
    }

    pub fn add_arcgis_layer(&self, base_url: String, service_name: String, display_name: String) -> u32 {
        let id = self.next_id();
        self.push_layer(Layer {
            id, name: display_name, bbox: WEB_MERCATOR_EXTENT, visible: true, opacity: 1.0,
            source: SourceDef::ArcGis { base_url, service_name },
        });
        id
    }

    pub fn add_mbtiles_layer(&self, path: String, display_name: String) -> CommandResult<u32> {
        let conn = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ).map_err(|e| CommandError::Io(format!("Failed to open MBTiles: {e}")))?;

        let bbox = conn.query_row(
            "SELECT value FROM metadata WHERE name = 'bounds'",
            [],
            |row| row.get::<_, String>(0),
        ).ok().and_then(|s| {
            let parts: Vec<f64> = s.split(',').filter_map(|p| p.parse().ok()).collect();
            if parts.len() == 4 { Some((parts[0], parts[1], parts[2], parts[3])) } else { None }
        }).unwrap_or(WEB_MERCATOR_EXTENT);

        let max_zoom = conn.query_row(
            "SELECT MAX(zoom_level) FROM tiles", [],
            |row| row.get::<_, u8>(0),
        ).unwrap_or(22);

        let id = self.next_id();
        self.push_layer(Layer {
            id, name: display_name, bbox, visible: true, opacity: 1.0,
            source: SourceDef::MbTiles { path, max_zoom },
        });
        Ok(id)
    }

    /// Append a layer and invalidate the render cache.
    fn push_layer(&self, layer: Layer) {
        self.state.layers.write().unpoison().push(layer);
        self.clear_cache();
    }

    pub fn remove_layer(&self, id: u32) -> bool {
        let mut layers = self.state.layers.write().unpoison();
        let before = layers.len();
        layers.retain(|l| l.id != id);
        let removed = layers.len() < before;
        drop(layers);
        if removed {
            self.state.runtime.write().unpoison().remove(&id);
            self.evict_remote_cache(id);
            self.clear_cache();
        }
        removed
    }

    /// Remove a layer and hand back its definition plus any runtime data, so the
    /// caller can move both to another group's server.
    pub fn take_layer(&self, id: u32) -> Option<(Layer, Option<RuntimeData>)> {
        let mut layers = self.state.layers.write().unpoison();
        let idx = layers.iter().position(|l| l.id == id)?;
        let layer = layers.remove(idx);
        drop(layers);
        let runtime = self.state.runtime.write().unpoison().remove(&id);
        self.clear_cache();
        Some((layer, runtime))
    }

    pub fn move_layer_up(&self, id: u32) -> bool {
        let mut layers = self.state.layers.write().unpoison();
        let Some(idx) = layers.iter().position(|l| l.id == id) else { return false };
        if idx == 0 { return false; }
        layers.swap(idx, idx - 1);
        drop(layers);
        self.clear_cache();
        true
    }

    pub fn move_layer_down(&self, id: u32) -> bool {
        let mut layers = self.state.layers.write().unpoison();
        let Some(idx) = layers.iter().position(|l| l.id == id) else { return false };
        if idx + 1 >= layers.len() { return false; }
        layers.swap(idx, idx + 1);
        drop(layers);
        self.clear_cache();
        true
    }

    pub fn insert_layer(&self, layer: Layer, runtime: Option<RuntimeData>) {
        let mut next = self.state.next_id.lock().unpoison();
        if layer.id >= *next {
            *next = layer.id + 1;
        }
        drop(next);
        if let Some(rt) = runtime {
            self.state.runtime.write().unpoison().insert(layer.id, rt);
        }
        self.state.layers.write().unpoison().push(layer);
        self.clear_cache();
    }

    /// Set an Apple layer's live tokenized URL. The URL is runtime-only (never
    /// persisted), so it is stored in the runtime cache rather than the definition.
    pub fn update_apple_url(&self, id: u32, url_template: String) {
        self.state.runtime.write().unpoison().insert(id, RuntimeData::Apple { url_template });
        self.clear_cache();
        self.evict_remote_cache(id);
    }

    pub fn rename_layer(&self, id: u32, name: String) {
        let mut layers = self.state.layers.write().unpoison();
        if let Some(layer) = layers.iter_mut().find(|l| l.id == id) {
            layer.name = name;
        }
    }

    pub fn set_layer_visible(&self, id: u32, visible: bool) -> bool {
        let mut layers = self.state.layers.write().unpoison();
        if let Some(layer) = layers.iter_mut().find(|l| l.id == id) {
            layer.visible = visible;
            drop(layers);
            self.clear_cache();
            true
        } else {
            false
        }
    }

    pub fn set_all_visible(&self, visible: bool) {
        let mut layers = self.state.layers.write().unpoison();
        for layer in layers.iter_mut() {
            layer.visible = visible;
        }
        drop(layers);
        self.clear_cache();
    }

    pub fn set_layer_opacity(&self, id: u32, opacity: f32) -> bool {
        let mut layers = self.state.layers.write().unpoison();
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
        self.state.layers.read().unpoison().len()
    }

    fn next_id(&self) -> u32 {
        let mut next = self.state.next_id.lock().unpoison();
        let id = *next;
        *next += 1;
        id
    }

    pub fn clear_cache(&self) {
        self.state.render_cache.clear();
    }

    pub fn evict_remote_cache(&self, layer_id: u32) {
        let mut cache = self.state.remote_cache.lock().unpoison();
        let keys: Vec<_> = cache.iter()
            .filter(|((lid, ..), _)| *lid == layer_id)
            .map(|(k, _)| *k)
            .collect();
        for key in keys {
            cache.pop(&key);
        }
    }
}

pub async fn start(port_hint: u16) -> Result<ServerHandle, Box<dyn std::error::Error + Send + Sync>> {
    let listener = if port_hint > 0 {
        match TcpListener::bind(format!("127.0.0.1:{port_hint}")).await {
            Ok(l) => l,
            Err(_) => TcpListener::bind("127.0.0.1:0").await?,
        }
    } else {
        TcpListener::bind("127.0.0.1:0").await?
    };
    let port = listener.local_addr()?.port();

    let state = Arc::new(TileState {
        layers: RwLock::new(Vec::new()),
        runtime: RwLock::new(HashMap::new()),
        render_cache: server_core::TileCache::new(RENDER_CACHE_CAPACITY, 1),
        remote_cache: server_core::lru_cache(REMOTE_CACHE_CAPACITY),
        error_layers: Mutex::new(std::collections::HashSet::new()),
        next_id: Mutex::new(0),
        port,
        http: reqwest::Client::builder()
            .user_agent(server_core::USER_AGENT)
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .connect_timeout(server_core::CONNECT_TIMEOUT)
            .build()
            .unwrap_or_default(),
        mbtiles_conns: server_core::lru_cache(MBTILES_CONN_CACHE),
    });

    let app = Router::new()
        .route("/tilejson.json", get(serve_tilejson))
        .route("/{z}/{x}/{y}", get(serve_tile))
        .layer(CorsLayer::permissive())
        .with_state(Arc::clone(&state));

    let shutdown_tx = server_core::spawn_with_shutdown(listener, app);

    Ok(ServerHandle { port, state, shutdown_tx })
}

fn decode_images(data: &OverlayData) -> Vec<DecodedImage> {
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


async fn serve_tilejson(
    State(state): State<Arc<TileState>>,
) -> impl IntoResponse {
    let port = state.port;
    let tile_url = format!("http://127.0.0.1:{port}/{{z}}/{{x}}/{{y}}");

    let (min_zoom, max_zoom) = (0, MAX_ZOOM);

    let bounds = {
        let layers = state.layers.read().unpoison();
        if layers.is_empty() {
            [-180.0, -85.051_129, 180.0, 85.051_129]
        } else {
            let mut w = f64::MAX;
            let mut s = f64::MAX;
            let mut e = f64::MIN;
            let mut n = f64::MIN;
            for l in layers.iter() {
                w = w.min(l.bbox.0);
                s = s.min(l.bbox.1);
                e = e.max(l.bbox.2);
                n = n.max(l.bbox.3);
            }
            [w, s, e, n]
        }
    };

    let center_lon = (bounds[0] + bounds[2]) / 2.0;
    let center_lat = (bounds[1] + bounds[3]) / 2.0;

    let json = serde_json::json!({
        "tilejson": "3.0.0",
        "tiles": [tile_url],
        "minzoom": min_zoom,
        "maxzoom": max_zoom,
        "bounds": bounds,
        "center": [center_lon, center_lat, 10],
        "format": "png",
        "scheme": "xyz",
    });

    (
        StatusCode::OK,
        [("content-type", "application/json")],
        json.to_string().into_bytes(),
    )
}

async fn serve_tile(
    Path((z, x, y)): Path<(u8, u32, u32)>,
    State(state): State<Arc<TileState>>,
) -> impl IntoResponse {
    let max_coord = 1u32 << z.min(MAX_ZOOM);
    if z > MAX_ZOOM || x >= max_coord || y >= max_coord {
        return (StatusCode::BAD_REQUEST, [("content-type", "text/plain")], Bytes::from_static(b"invalid tile coordinates"));
    }

    if let Some(png) = state.render_cache.get(&(z, x, y)) {
        return (StatusCode::OK, [("content-type", "image/png")], png);
    }

    let (remote_requests, mbtiles_tiles): (Vec<(u32, RemoteReq)>, HashMap<u32, Pixmap>) = {
        let layers = state.layers.read().unpoison();
        let runtime = state.runtime.read().unpoison();
        let mut reqs = Vec::new();
        let mut local = HashMap::new();
        let tms_y = turnout_core::geo::tms_y(z, y);
        for l in layers.iter() {
            if !l.visible { continue; }
            match &l.source {
                SourceDef::Kmz { .. } | SourceDef::Shp { .. } | SourceDef::GeoJson { .. } => {}
                SourceDef::Wms { base_url, layer_name } =>
                    reqs.push((l.id, RemoteReq::Wms(base_url.clone(), layer_name.clone()))),
                SourceDef::ArcGis { base_url, service_name } =>
                    reqs.push((l.id, RemoteReq::ArcGis(base_url.clone(), service_name.clone()))),
                SourceDef::Xyz { url_template }
                | SourceDef::Wmts { url_template }
                | SourceDef::Bing { url_template } =>
                    reqs.push((l.id, RemoteReq::Xyz(url_template.clone()))),
                SourceDef::Apple { .. } => {
                    if let Some(RuntimeData::Apple { url_template }) = runtime.get(&l.id) {
                        reqs.push((l.id, RemoteReq::Xyz(url_template.clone())));
                    }
                }
                SourceDef::MbTiles { path, max_zoom } => {
                    if let Some(cached) = get_remote_cached(&state, l.id, z, x, y) {
                        if let Some(pm) = decoded_to_pixmap(&cached) {
                            local.insert(l.id, pm);
                        }
                    } else if let Some(conn) = mbtiles_conn(&state, path) {
                        let pm = read_mbtiles_tile(&conn.lock().unpoison(), z, x, tms_y, *max_zoom);
                        if let Some(pm) = pm {
                            let decoded = (pm.data().to_vec(), pm.width(), pm.height());
                            put_remote_cached(&state, l.id, z, x, y, decoded);
                            local.insert(l.id, pm);
                        }
                    }
                }
            }
        }
        (reqs, local)
    };

    let fetches = remote_requests.iter().map(|(id, req)| {
        let client = &state.http;
        let id = *id;
        let state_ref = &state;
        async move {
            if let Some(cached) = get_remote_cached(state_ref, id, z, x, y) {
                return (id, decoded_to_pixmap(&cached));
            }
            let bytes = match req {
                RemoteReq::Wms(url, layer) => fetch_wms_tile(client, url, layer, z.into(), x, y).await,
                RemoteReq::ArcGis(url, svc) => fetch_arcgis_tile(client, url, svc, z.into(), x, y).await,
                RemoteReq::Xyz(tpl) => fetch_xyz_tile(client, tpl, z.into(), x, y).await,
            };
            if let Some(b) = bytes {
                state_ref.error_layers.lock().unpoison().remove(&id);
                if let Some(decoded) = decode_remote_bytes(&b) {
                    let pixmap = decoded_to_pixmap(&decoded);
                    put_remote_cached(state_ref, id, z, x, y, decoded);
                    (id, pixmap)
                } else {
                    (id, None)
                }
            } else {
                state_ref.error_layers.lock().unpoison().insert(id);
                (id, None)
            }
        }
    });
    let fetch_results: Vec<(u32, Option<Pixmap>)> = futures::future::join_all(fetches)
        .await
        .into_iter()
        .collect();
    let any_failed = fetch_results.iter().any(|(_, pm)| pm.is_none());
    let mut remote_tiles: HashMap<u32, Pixmap> = fetch_results
        .into_iter()
        .filter_map(|(id, pm)| pm.map(|p| (id, p)))
        .collect();
    remote_tiles.extend(mbtiles_tiles);

    let png = Bytes::from(render_tile(&state, &remote_tiles, z.into(), x, y));

    if !any_failed {
        state.render_cache.put((z, x, y), png.clone());
    }

    (StatusCode::OK, [("content-type", "image/png")], png)
}

