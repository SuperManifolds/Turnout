use std::collections::{HashMap, VecDeque};
use std::num::NonZeroUsize;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use image::{ImageBuffer, Rgba};
use lru::LruCache;
use tokio::net::TcpListener;
use tokio::sync::{oneshot, watch};
use tower_http::cors::CorsLayer;

use crate::tile_server::UnpoisonExt;

const PREFERRED_PORT: u16 = 17854;
const TILE_CACHE_CAPACITY: usize = 2048;
const WORKER_COUNT: usize = 4;
const ENCODER_COUNT: usize = 2;
const TILE_SIZE: u32 = 512;
const MAX_NATIVE_ZOOM: u8 = 19;
const RENDER_TIMEOUT_SECS: u64 = 30;
const STATS_INTERVAL: u64 = 50;
const WORKER_RESTART_DELAY_SECS: u64 = 1;
const CACHE_CONTROL: &str = "public, max-age=3600";

const STYLES: &[(&str, &str)] = &[
    ("standard", include_str!("../resources/orm/standard.json")),
    ("speed", include_str!("../resources/orm/speed.json")),
    ("signals", include_str!("../resources/orm/signals.json")),
    ("electrification", include_str!("../resources/orm/electrification.json")),
    ("track", include_str!("../resources/orm/track.json")),
    ("operator", include_str!("../resources/orm/operator.json")),
    ("route", include_str!("../resources/orm/route.json")),
];

type Key = (Arc<str>, u8, u32, u32);
type TileCache = LruCache<Key, Bytes>;
type RawImage = ImageBuffer<Rgba<u8>, Vec<u8>>;

fn canonical_style(name: &str) -> Arc<str> {
    if STYLES.iter().any(|(n, _)| *n == name) {
        Arc::from(name)
    } else {
        Arc::from("standard")
    }
}

fn style_json(name: &str) -> &'static str {
    STYLES.iter().find(|(n, _)| *n == name).map_or(STYLES[0].1, |(_, s)| s)
}

/// Computes the z19 ancestor tile coordinates for a given tile.
/// Returns the tile's own coordinates when z <= `MAX_NATIVE_ZOOM`.
fn parent_coords(z: u8, x: u32, y: u32) -> (u8, u32, u32) {
    if z > MAX_NATIVE_ZOOM {
        let dz = z - MAX_NATIVE_ZOOM;
        (MAX_NATIVE_ZOOM, x >> dz, y >> dz)
    } else {
        (z, x, y)
    }
}

/// Crops the quadrant of a z19 rendered image that corresponds to (z, x, y)
/// and upscales it to `TILE_SIZE`×`TILE_SIZE`.
fn crop_and_upscale(img: &RawImage, z: u8, x: u32, y: u32) -> RawImage {
    let dz = z - MAX_NATIVE_ZOOM;
    let s = 1u32.checked_shl(u32::from(dz)).unwrap_or(u32::MAX);
    let sub_size = TILE_SIZE.checked_div(s).unwrap_or(0);
    if sub_size == 0 {
        return img.clone();
    }
    let off_x = (x % s) * sub_size;
    let off_y = (y % s) * sub_size;
    let cropped = image::imageops::crop_imm(img, off_x, off_y, sub_size, sub_size);
    // SubImage<I> derefs to SubImageInner<I> which implements GenericImageView.
    image::imageops::resize(&*cropped, TILE_SIZE, TILE_SIZE, image::imageops::FilterType::Triangle)
}

fn extract_panic_message(e: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = e.downcast_ref::<&str>() {
        return (*s).to_string();
    }
    if let Some(s) = e.downcast_ref::<String>() {
        return s.clone();
    }
    "unknown panic".to_string()
}

/// Coalesces in-flight render requests. Workers block on `cv`/`queue`; the HTTP
/// handler pushes keys to the back and workers pop from the back (LIFO).
struct Dispatch {
    queue: Mutex<VecDeque<Key>>,
    waiters: Mutex<HashMap<Key, Vec<oneshot::Sender<Option<Bytes>>>>>,
    cv: Condvar,
}

struct OrmTileState {
    cache: Arc<Mutex<TileCache>>,
    dispatch: Arc<Dispatch>,
}

pub struct OrmHandle {
    _shutdown_tx: watch::Sender<bool>,
}

pub fn start_blocking() -> Result<OrmHandle, Box<dyn std::error::Error + Send + Sync>> {
    let dispatch = Arc::new(Dispatch {
        queue: Mutex::new(VecDeque::new()),
        waiters: Mutex::new(HashMap::new()),
        cv: Condvar::new(),
    });

    let cache_dir = dirs_next::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("turnout")
        .join("orm_cache");
    let _ = std::fs::create_dir_all(&cache_dir);
    let shared_cache_path = cache_dir.join("shared.db");

    let cache: Arc<Mutex<TileCache>> = Arc::new(Mutex::new(LruCache::new(
        NonZeroUsize::new(TILE_CACHE_CAPACITY).expect("nonzero"),
    )));

    let (encode_tx, encode_rx) = std::sync::mpsc::channel::<(Key, RawImage)>();
    let encode_rx = Arc::new(Mutex::new(encode_rx));

    for _ in 0..ENCODER_COUNT {
        let rx = Arc::clone(&encode_rx);
        let cache = Arc::clone(&cache);
        let dispatch = Arc::clone(&dispatch);
        std::thread::Builder::new()
            .name("orm-encode".into())
            .spawn(move || loop {
                let (key, raw) = {
                    let rx = rx.lock().unpoison();
                    match rx.recv() {
                        Ok(item) => item,
                        Err(_) => return,
                    }
                };

                let mut png_buf = Vec::with_capacity(48 * 1024);
                let encoder = image::codecs::png::PngEncoder::new_with_quality(
                    &mut png_buf,
                    image::codecs::png::CompressionType::Fast,
                    image::codecs::png::FilterType::NoFilter,
                );
                let result: Option<Bytes> = if raw.write_with_encoder(encoder).is_ok() {
                    Some(Bytes::from(png_buf))
                } else {
                    None
                };

                // Lock order: waiters → cache (matches serve_tile double-check order).
                let mut waiters = dispatch.waiters.lock().unpoison();
                if let Some(ref png) = result {
                    cache.lock().unpoison().put(key.clone(), png.clone());
                }
                let senders = waiters.remove(&key);
                drop(waiters);

                if let Some(senders) = senders {
                    for tx in senders {
                        let _ = tx.send(result.clone());
                    }
                }
            })?;
    }

    for i in 0..WORKER_COUNT {
        let dispatch = Arc::clone(&dispatch);
        let cache_path = shared_cache_path.clone();
        let encode_tx = encode_tx.clone();
        std::thread::Builder::new()
            .name(format!("orm-render-{i}"))
            .spawn(move || loop {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    render_worker_inner(i, &dispatch, &cache_path, &encode_tx);
                }));
                match result {
                    Ok(()) => {
                        eprintln!("[ORM worker {i}] Exited cleanly");
                        break;
                    }
                    Err(e) => {
                        let msg = extract_panic_message(&e);
                        eprintln!("[ORM worker {i}] PANIC: {msg} — restarting...");
                        std::thread::sleep(std::time::Duration::from_secs(WORKER_RESTART_DELAY_SECS));
                    }
                }
            })?;
    }

    let state = Arc::new(OrmTileState { cache, dispatch });

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let state2 = Arc::clone(&state);
    std::thread::Builder::new()
        .name("orm-http".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            rt.block_on(run_server(state2, shutdown_rx));
        })?;

    Ok(OrmHandle { _shutdown_tx: shutdown_tx })
}

async fn run_server(state: Arc<OrmTileState>, mut shutdown_rx: watch::Receiver<bool>) {
    let listener = match TcpListener::bind(format!("127.0.0.1:{PREFERRED_PORT}")).await {
        Ok(l) => l,
        Err(_) => match TcpListener::bind("127.0.0.1:0").await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("ORM tiles: failed to bind: {e}");
                return;
            }
        },
    };
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    eprintln!("ORM tile renderer at http://127.0.0.1:{port}/{{style}}/{{z}}/{{y}}/{{x}}.png");

    let router = Router::new()
        .route("/tilejson.json", get(serve_tilejson))
        .route("/{style}/tilejson.json", get(serve_style_tilejson))
        .route("/{style}/{z}/{y}/{x_png}", get(serve_tile))
        .layer(CorsLayer::permissive())
        .with_state(state);

    axum::serve(listener, router)
        .with_graceful_shutdown(async move { let _ = shutdown_rx.changed().await; })
        .await
        .ok();
}

fn build_renderer(
    cache_path: &std::path::Path,
    style: &str,
) -> maplibre_native::ImageRenderer<maplibre_native::Tile> {
    use maplibre_native::{ImageRendererBuilder, ResourceOptions};
    use std::num::NonZeroU32;

    let tile_size = NonZeroU32::new(TILE_SIZE).expect("nonzero");
    let resource_opts = ResourceOptions::default().with_cache_path(cache_path.to_path_buf());
    let mut r = ImageRendererBuilder::new()
        .with_size(tile_size, tile_size)
        .with_resource_options(resource_opts)
        .build_tile_renderer();
    r.load_style_from_json_str(style_json(style));
    r
}

fn pop_key_blocking(dispatch: &Dispatch) -> Key {
    let mut queue = dispatch.queue.lock().unpoison();
    loop {
        if let Some(k) = queue.pop_back() {
            return k;
        }
        queue = dispatch.cv.wait(queue).unpoison();
    }
}

fn fail_waiters(dispatch: &Dispatch, key: &Key) {
    let senders = dispatch.waiters.lock().unpoison().remove(key);
    if let Some(senders) = senders {
        for tx in senders {
            let _ = tx.send(None);
        }
    }
}

fn render_worker_inner(
    id: usize,
    dispatch: &Dispatch,
    cache_path: &std::path::Path,
    encode_tx: &std::sync::mpsc::Sender<(Key, RawImage)>,
) {
    eprintln!("[ORM worker {id}] Starting...");

    let mut renderers: HashMap<Arc<str>, maplibre_native::ImageRenderer<maplibre_native::Tile>> =
        HashMap::new();
    let mut render_count: u64 = 0;

    // Pre-warm standard renderer so the first pan doesn't pay the style-load cost.
    let standard: Arc<str> = Arc::from("standard");
    renderers.insert(Arc::clone(&standard), build_renderer(cache_path, "standard"));
    eprintln!("[ORM worker {id}] Pre-warmed 'standard' style");

    loop {
        let key = pop_key_blocking(dispatch);
        let style = Arc::clone(&key.0);
        let (z, x, y) = (key.1, key.2, key.3);

        // Skip the render if every waiting client has already disconnected.
        {
            let mut waiters = dispatch.waiters.lock().unpoison();
            match waiters.get(&key) {
                None => continue,
                Some(senders) if senders.iter().all(oneshot::Sender::is_closed) => {
                    waiters.remove(&key);
                    continue;
                }
                Some(_) => {}
            }
        }

        if !renderers.contains_key(style.as_ref()) {
            eprintln!("[ORM worker {id}] Loading style '{style}'...");
            renderers.insert(Arc::clone(&style), build_renderer(cache_path, &style));
            eprintln!("[ORM worker {id}] Style '{style}' loaded");
        }

        let (render_z, render_x, render_y) = parent_coords(z, x, y);
        let start = Instant::now();
        let result = renderers
            .get_mut(style.as_ref())
            .expect("just inserted")
            .render_tile(render_z, render_x, render_y);
        let render_ms = start.elapsed().as_millis();
        render_count += 1;

        match result {
            Ok(image) => {
                let raw = if z > MAX_NATIVE_ZOOM {
                    crop_and_upscale(image.as_image(), z, x, y)
                } else {
                    image.as_image().clone()
                };
                if render_count.is_multiple_of(STATS_INTERVAL) || render_ms > 2000 {
                    eprintln!(
                        "[ORM worker {id}] Rendered {render_count} tiles (last: {render_ms}ms)"
                    );
                }
                let _ = encode_tx.send((key, raw));
            }
            Err(e) => {
                eprintln!(
                    "[ORM worker {id}] Failed z={render_z} x={render_x} y={render_y}: \
                     {e} ({render_ms}ms)"
                );
                renderers.remove(style.as_ref());
                fail_waiters(dispatch, &key);
            }
        }
    }
}

async fn serve_tilejson(State(_state): State<Arc<OrmTileState>>) -> impl IntoResponse {
    serve_tilejson_for("standard")
}

async fn serve_style_tilejson(
    Path(style): Path<String>,
    State(_state): State<Arc<OrmTileState>>,
) -> impl IntoResponse {
    serve_tilejson_for(&style)
}

fn serve_tilejson_for(style: &str) -> (StatusCode, [(&'static str, &'static str); 1], String) {
    let json = serde_json::json!({
        "tilejson": "3.0.0",
        "name": format!("OpenRailwayMap {style}"),
        "tiles": [format!("http://127.0.0.1:{PREFERRED_PORT}/{style}/{{z}}/{{y}}/{{x}}.png")],
        "minzoom": 0,
        "maxzoom": 19,
        "format": "png",
        "bounds": [-180.0, -85.051_129, 180.0, 85.051_129],
    });
    (StatusCode::OK, [("content-type", "application/json")], serde_json::to_string(&json).unwrap_or_default())
}

async fn serve_tile(
    Path((style, z, y, x_png)): Path<(String, u8, u32, String)>,
    State(state): State<Arc<OrmTileState>>,
) -> Response {
    let style = canonical_style(&style);
    let x: u32 = x_png.strip_suffix(".png").unwrap_or(&x_png).parse().unwrap_or(0);
    let key: Key = (Arc::clone(&style), z, x, y);

    // Fast path: check cache before acquiring the waiters lock.
    if let Some(png) = state.cache.lock().unpoison().get(&key).cloned() {
        return ok_tile(png);
    }

    let (tx, rx) = oneshot::channel();
    {
        // Lock order: waiters → cache (consistent with encoder completion).
        let mut waiters = state.dispatch.waiters.lock().unpoison();
        // Double-check under the waiters lock to close the race with a just-finished encoder.
        if let Some(png) = state.cache.lock().unpoison().get(&key).cloned() {
            return ok_tile(png);
        }
        if let Some(senders) = waiters.get_mut(&key) {
            senders.push(tx);
        } else {
            waiters.insert(key.clone(), vec![tx]);
            state.dispatch.queue.lock().unpoison().push_back(key.clone());
            state.dispatch.cv.notify_one();
        }
    }

    match tokio::time::timeout(std::time::Duration::from_secs(RENDER_TIMEOUT_SECS), rx).await {
        Ok(Ok(Some(png))) => ok_tile(png),
        Ok(Ok(None)) => error_tile(StatusCode::INTERNAL_SERVER_ERROR, b"render failed"),
        _ => {
            eprintln!("[ORM http] TIMEOUT z={z} x={x} y={y}");
            error_tile(StatusCode::GATEWAY_TIMEOUT, b"render timeout")
        }
    }
}

fn ok_tile(png: Bytes) -> Response {
    (
        StatusCode::OK,
        [("content-type", "image/png"), ("cache-control", CACHE_CONTROL)],
        png,
    )
        .into_response()
}

fn error_tile(status: StatusCode, body: &'static [u8]) -> Response {
    (status, [("content-type", "text/plain")], Bytes::from_static(body)).into_response()
}
