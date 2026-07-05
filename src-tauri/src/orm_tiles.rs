use std::collections::{HashMap, VecDeque};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::Instant;

use axum::Router;
use axum::body::{Body, Bytes};
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
const OFFLINE_TILES_PLACEHOLDER: &str = "{{OFFLINE_TILES_URL}}";
const OFFLINE_MBTILES_FILE: &str = "tiles.mbtiles";
/// Upper bound for requested zoom; guards the bit shifts in overzoom and TMS
/// row-flip math against absurd zoom values in request paths.
const MAX_REQUEST_ZOOM: u8 = 22;

pub(crate) const STYLES: &[(&str, &str)] = &[
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

/// State shared across the render workers, PNG encoders, the HTTP layer, and the
/// `OrmHandle`. `generation` is bumped on every offline-mode change so workers can
/// lazily rebuild their renderers and encoders can drop stale cache inserts.
struct OrmShared {
    offline_dir: RwLock<Option<PathBuf>>,
    generation: AtomicU64,
    bound_port: AtomicU16,
}

struct OrmTileState {
    cache: Arc<Mutex<TileCache>>,
    dispatch: Arc<Dispatch>,
    shared: Arc<OrmShared>,
    /// Lazily opened, path-keyed read-only connection to the offline MVT store.
    mbtiles_conn: Mutex<Option<(PathBuf, rusqlite::Connection)>>,
}

pub struct OrmHandle {
    _shutdown_tx: watch::Sender<bool>,
    shared: Arc<OrmShared>,
    cache: Arc<Mutex<TileCache>>,
}

impl OrmHandle {
    /// Switches the renderer between offline (`Some`) and online (`None`) mode.
    ///
    /// Ordering is load-bearing: the directory is published first so any worker
    /// that observes the new generation rebuilds against it, then the generation
    /// is bumped, and only then is the cache cleared. Clearing after the bump
    /// guarantees an in-flight encode tagged with the old generation cannot
    /// re-populate the cache with a stale tile (the encoder drops such inserts).
    pub fn set_offline_dir(&self, dir: Option<PathBuf>) {
        *self.shared.offline_dir.write().unpoison() = dir;
        // Bump and clear under the cache lock so the pair is serialized against the
        // encoder's compare-then-insert: an in-flight encode either inserts before
        // this clear (and is then wiped) or observes the new generation and skips.
        let mut cache = self.cache.lock().unpoison();
        self.shared.generation.fetch_add(1, Ordering::SeqCst);
        cache.clear();
    }
}

pub fn start_blocking() -> Result<OrmHandle, Box<dyn std::error::Error + Send + Sync>> {
    let dispatch = Arc::new(Dispatch {
        queue: Mutex::new(VecDeque::new()),
        waiters: Mutex::new(HashMap::new()),
        cv: Condvar::new(),
    });

    let shared = Arc::new(OrmShared {
        offline_dir: RwLock::new(None),
        generation: AtomicU64::new(0),
        bound_port: AtomicU16::new(0),
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

    let (encode_tx, encode_rx) = std::sync::mpsc::channel::<(Key, RawImage, u64)>();
    let encode_rx = Arc::new(Mutex::new(encode_rx));

    for _ in 0..ENCODER_COUNT {
        let rx = Arc::clone(&encode_rx);
        let cache = Arc::clone(&cache);
        let dispatch = Arc::clone(&dispatch);
        let shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("orm-encode".into())
            .spawn(move || loop {
                let (key, raw, generation) = {
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
                // Compare the render's generation and insert under the cache lock so
                // a concurrent offline toggle (which bumps + clears under the same
                // lock) can never let a stale tile survive the clear.
                if let Some(ref png) = result {
                    let mut cache = cache.lock().unpoison();
                    if generation == shared.generation.load(Ordering::SeqCst) {
                        cache.put(key.clone(), png.clone());
                    }
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
        let shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name(format!("orm-render-{i}"))
            .spawn(move || loop {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    render_worker_inner(i, &dispatch, &cache_path, &encode_tx, &shared);
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

    let state = Arc::new(OrmTileState {
        cache: Arc::clone(&cache),
        dispatch,
        shared: Arc::clone(&shared),
        mbtiles_conn: Mutex::new(None),
    });

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

    Ok(OrmHandle { _shutdown_tx: shutdown_tx, shared, cache })
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
    state.shared.bound_port.store(port, Ordering::SeqCst);
    eprintln!("ORM tile renderer at http://127.0.0.1:{port}/{{style}}/{{z}}/{{y}}/{{x}}.png");

    let router = Router::new()
        .route("/tilejson.json", get(serve_tilejson))
        .route("/{style}/tilejson.json", get(serve_style_tilejson))
        .route("/offline/{z}/{x}/{y}", get(serve_offline_tile))
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
    shared: &OrmShared,
) -> maplibre_native::ImageRenderer<maplibre_native::Tile> {
    use maplibre_native::{ImageRendererBuilder, ResourceOptions};
    use std::num::NonZeroU32;

    let tile_size = NonZeroU32::new(TILE_SIZE).expect("nonzero");
    let resource_opts = ResourceOptions::default().with_cache_path(cache_path.to_path_buf());
    let mut r = ImageRendererBuilder::new()
        .with_size(tile_size, tile_size)
        .with_resource_options(resource_opts)
        .build_tile_renderer();
    r.load_style_from_json_str(resolve_style_source(style, shared));
    r
}

/// Returns the style JSON to load: the on-disk offline variant (with the tiles-URL
/// placeholder substituted for the local `/offline` endpoint) when offline mode is
/// active and readable, otherwise the embedded style.
fn resolve_style_source(style: &str, shared: &OrmShared) -> String {
    let dir = shared.offline_dir.read().unpoison().clone();
    let Some(dir) = dir else {
        return style_json(style).to_string();
    };
    let port = shared.bound_port.load(Ordering::SeqCst);
    match offline_style_source(&dir, style, port) {
        Ok(source) => source,
        Err(e) => {
            eprintln!("[ORM] offline style '{style}' unavailable ({e}); using embedded style");
            style_json(style).to_string()
        }
    }
}

/// Reads `<dir>/styles/{style}_offline.json` and substitutes every
/// `{{OFFLINE_TILES_URL}}` placeholder with the local offline tile endpoint.
///
/// Styles referencing their tiles via an `mbtiles://<abs path>` source url contain
/// no placeholder, so the substitution is a no-op and the style loads unchanged —
/// both offline tile-url forms are handled by this single path.
fn offline_style_source(dir: &std::path::Path, style: &str, bound_port: u16) -> std::io::Result<String> {
    let path = dir.join("styles").join(format!("{style}_offline.json"));
    let raw = std::fs::read_to_string(path)?;
    let base = format!("http://127.0.0.1:{bound_port}/offline");
    Ok(raw.replace(OFFLINE_TILES_PLACEHOLDER, &base))
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
    encode_tx: &std::sync::mpsc::Sender<(Key, RawImage, u64)>,
    shared: &OrmShared,
) {
    eprintln!("[ORM worker {id}] Starting...");

    let mut renderers: HashMap<Arc<str>, maplibre_native::ImageRenderer<maplibre_native::Tile>> =
        HashMap::new();
    let mut render_count: u64 = 0;
    let mut current_gen = shared.generation.load(Ordering::SeqCst);

    // Pre-warm standard renderer so the first pan doesn't pay the style-load cost.
    let standard: Arc<str> = Arc::from("standard");
    renderers.insert(Arc::clone(&standard), build_renderer(cache_path, "standard", shared));
    eprintln!("[ORM worker {id}] Pre-warmed 'standard' style");

    loop {
        let key = pop_key_blocking(dispatch);
        let style = Arc::clone(&key.0);
        let (z, x, y) = (key.1, key.2, key.3);

        // An offline-mode toggle bumps the generation; drop every renderer so they
        // rebuild lazily against the current style source.
        let generation = shared.generation.load(Ordering::SeqCst);
        if generation != current_gen {
            renderers.clear();
            current_gen = generation;
        }

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
            renderers.insert(Arc::clone(&style), build_renderer(cache_path, &style, shared));
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
                let _ = encode_tx.send((key, raw, current_gen));
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

/// Serves an offline MVT tile from `<offline dir>/tiles.mbtiles`.
///
/// The store uses TMS row indexing (matching `tile_server`), so the XYZ `y` is
/// flipped before the lookup. Returns 404 when offline mode is disabled or the
/// tile is absent. gzip-compressed blobs are advertised via `content-encoding`.
async fn serve_offline_tile(
    Path((z, x, y)): Path<(u8, u32, u32)>,
    State(state): State<Arc<OrmTileState>>,
) -> Response {
    if z > MAX_REQUEST_ZOOM {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(dir) = state.shared.offline_dir.read().unpoison().clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mbtiles_path = dir.join(OFFLINE_MBTILES_FILE);
    let tms_y = (1u32 << z).saturating_sub(1).saturating_sub(y);

    let blob = read_offline_tile(&state.mbtiles_conn, &mbtiles_path, z, x, tms_y);
    let Some(blob) = blob else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/x-protobuf");
    if blob.starts_with(&[0x1f, 0x8b]) {
        builder = builder.header("content-encoding", "gzip");
    }
    builder.body(Body::from(blob)).unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Reads a single MVT blob using a cached, path-keyed read-only connection. The
/// connection is reopened whenever the offline directory (hence path) changes.
/// The query is synchronous but sub-millisecond, so briefly holding the mutex on
/// the current-thread runtime is acceptable.
fn read_offline_tile(
    conn_cell: &Mutex<Option<(PathBuf, rusqlite::Connection)>>,
    mbtiles_path: &std::path::Path,
    z: u8,
    x: u32,
    tms_y: u32,
) -> Option<Vec<u8>> {
    let mut guard = conn_cell.lock().unpoison();
    let needs_open = guard.as_ref().is_none_or(|(path, _)| path != mbtiles_path);
    if needs_open {
        match rusqlite::Connection::open_with_flags(
            mbtiles_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) {
            Ok(conn) => *guard = Some((mbtiles_path.to_path_buf(), conn)),
            Err(e) => {
                eprintln!("[ORM offline] open {} failed: {e}", mbtiles_path.display());
                return None;
            }
        }
    }
    let (_, conn) = guard.as_ref()?;
    conn.query_row(
        "SELECT tile_data FROM tiles WHERE zoom_level = ?1 AND tile_column = ?2 AND tile_row = ?3",
        rusqlite::params![z, x, tms_y],
        |row| row.get(0),
    )
    .ok()
}

async fn serve_tile(
    Path((style, z, y, x_png)): Path<(String, u8, u32, String)>,
    State(state): State<Arc<OrmTileState>>,
) -> Response {
    if z > MAX_REQUEST_ZOOM {
        return error_tile(StatusCode::NOT_FOUND, b"zoom out of range");
    }
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

/// Enables offline rendering from `dir` (`Some`) or restores online rendering
/// (`None`). Bumps the render generation and clears the tile cache.
#[tauri::command]
pub async fn set_orm_offline(
    handle: tauri::State<'_, OrmHandle>,
    dir: Option<String>,
) -> Result<(), String> {
    let dir = dir.map(PathBuf::from);
    if let Some(d) = &dir {
        let mbtiles = d.join(OFFLINE_MBTILES_FILE);
        if !mbtiles.is_file() {
            return Err(format!("no offline tile store at {}", mbtiles.display()));
        }
    }
    handle.set_offline_dir(dir);
    Ok(())
}
