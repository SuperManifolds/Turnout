use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

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
const RENDER_CACHE_CAPACITY: usize = 512;
const REMOTE_CACHE_CAPACITY: usize = 2048;
pub const PREFERRED_PORT: u16 = 17853;
const MAX_ZOOM: u8 = 22;
const HTTP_TIMEOUT_SECS: u64 = 15;

const DEFAULT_LINE_COLOR: [u8; 4] = [255, 100, 0, 200];
const DEFAULT_LINE_WIDTH: f32 = 2.0;
const DEFAULT_FILL_COLOR: [u8; 4] = [255, 100, 0, 80];
const POINT_RADIUS: f32 = 5.0;
const POINT_COLOR: [u8; 4] = [255, 60, 0, 220];
const MIN_OVERLAY_PIXEL_SIZE: f32 = 0.01;
const ROTATION_EPSILON: f64 = 0.001;
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
    Xyz { url_template: String },
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

type RenderCache = LruCache<(u8, u32, u32), Vec<u8>>;
type DecodedTile = (Vec<u8>, u32, u32);
type RemoteCache = LruCache<(u32, u8, u32, u32), DecodedTile>;

pub struct TileState {
    pub(crate) layers: RwLock<Vec<Layer>>,
    render_cache: Mutex<RenderCache>,
    remote_cache: Mutex<RemoteCache>,
    pub(crate) error_layers: Mutex<std::collections::HashSet<u32>>,
    next_id: Mutex<u32>,
    port: u16,
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

    pub fn add_xyz_layer(&self, url_template: String, display_name: String) -> u32 {
        self.add_xyz_layer_with_kind(url_template, display_name, "xyz")
    }

    pub fn add_xyz_layer_with_kind(&self, url_template: String, display_name: String, kind: &str) -> u32 {
        let id = self.next_id();
        let kind_static = match kind {
            "wmts" => "wmts",
            "apple" => "apple",
            "bing" => "bing",
            _ => "xyz",
        };

        let mut layers = self.state.layers.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        layers.push(Layer {
            id, name: display_name, bbox: WMS_BBOX, kind: kind_static, visible: true, opacity: 1.0,
            source: LayerSource::Xyz { url_template },
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
            self.evict_remote_cache(id);
            self.clear_cache();
        }
        removed
    }

    pub fn take_layer(&self, id: u32) -> Option<Layer> {
        let mut layers = self.state.layers.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        let idx = layers.iter().position(|l| l.id == id)?;
        let layer = layers.remove(idx);
        drop(layers);
        self.clear_cache();
        Some(layer)
    }

    pub fn move_layer_up(&self, id: u32) -> bool {
        let mut layers = self.state.layers.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(idx) = layers.iter().position(|l| l.id == id) else { return false };
        if idx == 0 { return false; }
        layers.swap(idx, idx - 1);
        drop(layers);
        self.clear_cache();
        true
    }

    pub fn move_layer_down(&self, id: u32) -> bool {
        let mut layers = self.state.layers.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(idx) = layers.iter().position(|l| l.id == id) else { return false };
        if idx + 1 >= layers.len() { return false; }
        layers.swap(idx, idx + 1);
        drop(layers);
        self.clear_cache();
        true
    }

    pub fn insert_layer(&self, layer: Layer) {
        let mut next = self.state.next_id.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if layer.id >= *next {
            *next = layer.id + 1;
        }
        drop(next);
        let mut layers = self.state.layers.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        layers.push(layer);
        drop(layers);
        self.clear_cache();
    }

    pub fn update_xyz_url(&self, id: u32, url_template: String) {
        let mut layers = self.state.layers.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(layer) = layers.iter_mut().find(|l| l.id == id)
            && let LayerSource::Xyz { url_template: ref mut tpl } = layer.source
        {
            *tpl = url_template;
        }
        drop(layers);
        self.clear_cache();
        self.evict_remote_cache(id);
    }

    pub fn rename_layer(&self, id: u32, name: String) {
        let mut layers = self.state.layers.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(layer) = layers.iter_mut().find(|l| l.id == id) {
            layer.name = name;
        }
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
        let mut cache = self.state.render_cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.clear();
    }

    pub fn evict_remote_cache(&self, layer_id: u32) {
        let mut cache = self.state.remote_cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
        render_cache: Mutex::new(LruCache::new(
            NonZeroUsize::new(RENDER_CACHE_CAPACITY).expect("nonzero"),
        )),
        remote_cache: Mutex::new(LruCache::new(
            NonZeroUsize::new(REMOTE_CACHE_CAPACITY).expect("nonzero"),
        )),
        error_layers: Mutex::new(std::collections::HashSet::new()),
        next_id: Mutex::new(0),
        port,
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_default(),
    });

    let app = Router::new()
        .route("/tilejson.json", get(serve_tilejson))
        .route("/{z}/{x}/{y}", get(serve_tile))
        .layer(CorsLayer::permissive())
        .with_state(Arc::clone(&state));

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

fn decode_remote_bytes(bytes: &[u8]) -> Option<DecodedTile> {
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

fn decoded_to_pixmap(decoded: &DecodedTile) -> Option<Pixmap> {
    let (data, w, h) = decoded;
    Pixmap::from_vec(data.clone(), tiny_skia::IntSize::from_wh(*w, *h)?)
}

fn get_remote_cached(state: &TileState, layer_id: u32, z: u8, x: u32, y: u32) -> Option<DecodedTile> {
    let mut cache = state.remote_cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    cache.get(&(layer_id, z, x, y)).cloned()
}

fn put_remote_cached(state: &TileState, layer_id: u32, z: u8, x: u32, y: u32, decoded: DecodedTile) {
    let mut cache = state.remote_cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    cache.put((layer_id, z, x, y), decoded);
}

async fn fetch_wms_tile(
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

    Some(resp.bytes().await.ok()?.to_vec())
}

fn xyz_to_quadkey(z: u32, x: u32, y: u32) -> String {
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

async fn fetch_xyz_tile(
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
        .map_err(|e| eprintln!("XYZ fetch error for {url}: {e}"))
        .ok()?;

    if !resp.status().is_success() {
        eprintln!("XYZ returned HTTP {} for {url}", resp.status());
        return None;
    }

    Some(resp.bytes().await.ok()?.to_vec())
}

async fn fetch_arcgis_tile(
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
        .map_err(|e| eprintln!("ArcGIS fetch error: {e}"))
        .ok()?;

    if !resp.status().is_success() {
        return None;
    }

    Some(resp.bytes().await.ok()?.to_vec())
}

enum RemoteReq {
    Wms(String, String),
    ArcGis(String, String),
    Xyz(String),
}

async fn serve_tilejson(
    State(state): State<Arc<TileState>>,
) -> impl IntoResponse {
    let port = state.port;
    let tile_url = format!("http://127.0.0.1:{port}/{{z}}/{{x}}/{{y}}");

    let (min_zoom, max_zoom) = (0, MAX_ZOOM);

    let bounds = {
        let layers = state.layers.read().unwrap_or_else(std::sync::PoisonError::into_inner);
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
        return (StatusCode::BAD_REQUEST, [("content-type", "text/plain")], b"invalid tile coordinates".to_vec());
    }

    {
        let mut cache = state.render_cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(png) = cache.get(&(z, x, y)) {
            return (StatusCode::OK, [("content-type", "image/png")], png.clone());
        }
    }

    let remote_requests: Vec<(u32, RemoteReq)> = {
        let layers = state.layers.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        layers.iter().filter_map(|l| {
            if !l.visible { return None; }
            match &l.source {
                LayerSource::Wms { base_url, layer_name } =>
                    Some((l.id, RemoteReq::Wms(base_url.clone(), layer_name.clone()))),
                LayerSource::ArcGis { base_url, service_name } =>
                    Some((l.id, RemoteReq::ArcGis(base_url.clone(), service_name.clone()))),
                LayerSource::Xyz { url_template } =>
                    Some((l.id, RemoteReq::Xyz(url_template.clone()))),
                LayerSource::Kmz { .. } => None,
            }
        }).collect()
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
                state_ref.error_layers.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&id);
                if let Some(decoded) = decode_remote_bytes(&b) {
                    let pixmap = decoded_to_pixmap(&decoded);
                    put_remote_cached(state_ref, id, z, x, y, decoded);
                    (id, pixmap)
                } else {
                    (id, None)
                }
            } else {
                state_ref.error_layers.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(id);
                (id, None)
            }
        }
    });
    let fetch_results: Vec<(u32, Option<Pixmap>)> = futures::future::join_all(fetches)
        .await
        .into_iter()
        .collect();
    let any_failed = fetch_results.iter().any(|(_, pm)| pm.is_none());
    let remote_tiles: Vec<(u32, Pixmap)> = fetch_results
        .into_iter()
        .filter_map(|(id, pm)| pm.map(|p| (id, p)))
        .collect();

    let png = render_tile(&state, &remote_tiles, z.into(), x, y);

    if !any_failed {
        let mut cache = state.render_cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.put((z, x, y), png.clone());
    }

    (StatusCode::OK, [("content-type", "image/png")], png)
}

fn render_tile(state: &TileState, remote_tiles: &[(u32, Pixmap)], z: u32, x: u32, y: u32) -> Vec<u8> {
    let mut pixmap = Pixmap::new(TILE_SIZE, TILE_SIZE).expect("256x256 pixmap");
    let (tile_w, tile_s, tile_e, tile_n) = tile_bounds(z, x, y);

    {
        let layers = state.layers.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        for layer in layers.iter() {
            if !layer.visible {
                continue;
            }
            let opacity = layer.opacity;
            match &layer.source {
                LayerSource::Kmz { data, images, .. } => {
                    render_ground_overlays(&mut pixmap, images, opacity, z, x, y, tile_w, tile_s, tile_e, tile_n);
                    render_geometry(&mut pixmap, data, opacity, z, x, y, tile_w, tile_s, tile_e, tile_n);
                }
                LayerSource::Wms { .. } | LayerSource::ArcGis { .. } | LayerSource::Xyz { .. } => {
                    if let Some((_, remote_pixmap)) = remote_tiles.iter().find(|(id, _)| *id == layer.id) {
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
        if dest_w.abs() < MIN_OVERLAY_PIXEL_SIZE || dest_h.abs() < MIN_OVERLAY_PIXEL_SIZE {
            continue;
        }

        let sx = dest_w / ov.pixmap.width() as f32;
        let sy = dest_h / ov.pixmap.height() as f32;

        let transform = if ov.rotation.abs() > ROTATION_EPSILON {
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
    [rgba[0], rgba[1], rgba[2], (f32::from(rgba[3]) * opacity).clamp(0.0, 255.0) as u8]
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

const MAX_RENDER_DEPTH: usize = 10;

fn render_single_geometry(
    pixmap: &mut Pixmap,
    geom: &Geometry,
    opacity: f32,
    z: u32, x: u32, y: u32,
    ew: f64, es: f64, ee: f64, en: f64,
    style: &Style,
) {
    render_geometry_depth(pixmap, geom, opacity, z, x, y, ew, es, ee, en, style, 0);
}

fn render_geometry_depth(
    pixmap: &mut Pixmap,
    geom: &Geometry,
    opacity: f32,
    z: u32, x: u32, y: u32,
    ew: f64, es: f64, ee: f64, en: f64,
    style: &Style,
    depth: usize,
) {
    if depth > MAX_RENDER_DEPTH { return; }
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
                render_geometry_depth(pixmap, g, opacity, z, x, y, ew, es, ee, en, style, depth + 1);
            }
        }
    }
}

fn coords_intersect(coords: &[(f64, f64)], w: f64, s: f64, e: f64, n: f64) -> bool {
    if coords.iter().any(|(lon, lat)| *lon >= w && *lon <= e && *lat >= s && *lat <= n) {
        return true;
    }
    coords.windows(2).any(|seg| {
        turnout_core::geo::segment_rect_intersect(
            seg[0].0, seg[0].1, seg[1].0, seg[1].1, w, s, e, n,
        ).is_some()
    })
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
