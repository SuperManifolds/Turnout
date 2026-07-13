use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
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
const TILE_CACHE_CAPACITY: usize = 4096;
const WORKER_COUNT: usize = 6;
const ENCODER_COUNT: usize = 2;
/// HTTP request-handling threads. Kept modest — handlers are lightweight (lock,
/// enqueue, await); rendering happens on the dedicated worker threads.
const HTTP_WORKER_THREADS: usize = 4;
/// Max pending real (client-driven) render requests. On overflow the oldest is
/// dropped: with LIFO dispatch the front is the stalest, i.e. tiles panned away
/// from, so this keeps the queue focused on the current viewport.
const MAIN_QUEUE_CAP: usize = 64;
/// Max pending speculative neighbour renders. Drained only when the main queue is
/// empty (a pan pause), so they never delay a real request.
const PREFETCH_QUEUE_CAP: usize = 128;
/// mbgl ambient cache size for fetched MVT tiles (online revisits avoid re-fetch).
const MBGL_CACHE_BYTES: u64 = 512 * 1024 * 1024;
const TILE_SIZE: u32 = 512;
const MAX_NATIVE_ZOOM: u8 = 19;
const RENDER_TIMEOUT_SECS: u64 = 30;
const STATS_INTERVAL: u64 = 50;
const WORKER_RESTART_DELAY_SECS: u64 = 1;
const CACHE_CONTROL: &str = "public, max-age=3600";
const OFFLINE_MBTILES_FILE: &str = "tiles.mbtiles";
/// Number of independently locked PNG cache shards. Requests are spread across
/// shards by key hash so lookups and inserts rarely contend on a single lock.
const CACHE_STRIPES: usize = 8;
/// Poll interval and attempt count for `get_orm_port` while the server binds on
/// its background runtime. 40 × 50ms = 2s, comfortably longer than bind latency.
const PORT_WAIT_INTERVAL_MS: u64 = 50;
const PORT_WAIT_ATTEMPTS: u32 = 40;
/// Upper bound for requested zoom; guards the bit shifts in overzoom and TMS
/// row-flip math against absurd zoom values in request paths.
const MAX_REQUEST_ZOOM: u8 = 22;
/// Per-worker memory of recently failed keys, granting each tile one transparent
/// requeue before its waiters get a hard failure.
const FAILED_KEY_MEMORY: usize = 64;

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
type Waiters = HashMap<Key, Vec<oneshot::Sender<TileResult>>>;

/// Outcome delivered to a tile request's waiters. `Evicted` is distinct from
/// `Failed` so the HTTP layer can answer 204 (revisit re-requests) rather than
/// 500 (which GL JS never retries) when a queued request is dropped on overflow.
#[derive(Clone)]
enum TileResult {
    Png(Bytes),
    Evicted,
    Failed,
}

/// A hash-striped PNG cache. Each stripe is an independently locked LRU holding a
/// `TILE_CACHE_CAPACITY / CACHE_STRIPES` slice of the total capacity. A key always
/// maps to one stripe, so no operation ever needs to hold two stripe locks.
struct StripedCache {
    stripes: Vec<Mutex<TileCache>>,
}

impl StripedCache {
    fn new(total_capacity: usize) -> Self {
        let per_stripe = (total_capacity / CACHE_STRIPES).max(1);
        let cap = NonZeroUsize::new(per_stripe).expect("nonzero");
        let stripes = (0..CACHE_STRIPES).map(|_| Mutex::new(LruCache::new(cap))).collect();
        Self { stripes }
    }

    fn stripe(&self, key: &Key) -> &Mutex<TileCache> {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut hasher);
        let idx = (hasher.finish() as usize) % CACHE_STRIPES;
        &self.stripes[idx]
    }

    fn get(&self, key: &Key) -> Option<Bytes> {
        self.stripe(key).lock().unpoison().get(key).cloned()
    }

    fn contains(&self, key: &Key) -> bool {
        self.stripe(key).lock().unpoison().contains(key)
    }

    fn clear(&self) {
        for stripe in &self.stripes {
            stripe.lock().unpoison().clear();
        }
    }
}

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

/// Two LIFO queues drained newest-first. `main` holds client-driven requests;
/// `prefetch` holds speculative neighbour tiles and is only drained when `main` is
/// empty. Both are bounded (see the `*_QUEUE_CAP` constants).
struct Queues {
    main: VecDeque<Key>,
    prefetch: VecDeque<Key>,
}

/// Coalesces in-flight render requests. Workers block on `cv`/`queues`; the HTTP
/// handler pushes keys to the back and workers pop from the back (LIFO).
struct Dispatch {
    queues: Mutex<Queues>,
    waiters: Mutex<Waiters>,
    cv: Condvar,
}

/// State shared across the render workers, PNG encoders, the HTTP layer, and the
/// `OrmHandle`. `generation` is bumped on every offline-mode change so workers can
/// lazily rebuild their renderers and encoders can drop stale cache inserts.
struct OrmShared {
    offline_dir: RwLock<Option<PathBuf>>,
    /// Base URL for online tile/glyph/sprite requests. Substituted into the
    /// embedded styles at load so a self-hosted `OpenRailwayMap` can replace the
    /// default `openrailwaymap.app`.
    base_url: RwLock<String>,
    generation: AtomicU64,
    bound_port: AtomicU16,
}

struct OrmTileState {
    cache: Arc<StripedCache>,
    dispatch: Arc<Dispatch>,
    shared: Arc<OrmShared>,
}

pub struct OrmHandle {
    _shutdown_tx: watch::Sender<bool>,
    shared: Arc<OrmShared>,
    cache: Arc<StripedCache>,
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
        // Bump before clearing so the pair is serialized against the encoder's
        // compare-then-insert (which loads the generation under its stripe lock):
        // an in-flight encode either inserts before this clear reaches its stripe
        // (and is then wiped) or observes the new generation and skips.
        self.shared.generation.fetch_add(1, Ordering::SeqCst);
        self.cache.clear();
    }

    /// Points online rendering at a different ORM base URL. Like `set_offline_dir`,
    /// this bumps the render generation (so workers rebuild against the new host)
    /// and clears the tile cache. A no-op when the URL is unchanged.
    pub fn set_base_url(&self, base: String) {
        {
            let mut current = self.shared.base_url.write().unpoison();
            if *current == base {
                return;
            }
            *current = base;
        }
        self.shared.generation.fetch_add(1, Ordering::SeqCst);
        self.cache.clear();
    }
}

pub fn start_blocking() -> Result<OrmHandle, Box<dyn std::error::Error + Send + Sync>> {
    let dispatch = Arc::new(Dispatch {
        queues: Mutex::new(Queues { main: VecDeque::new(), prefetch: VecDeque::new() }),
        waiters: Mutex::new(HashMap::new()),
        cv: Condvar::new(),
    });

    let shared = Arc::new(OrmShared {
        offline_dir: RwLock::new(None),
        base_url: RwLock::new(crate::settings::DEFAULT_ORM_BASE.to_string()),
        generation: AtomicU64::new(0),
        bound_port: AtomicU16::new(0),
    });

    let cache_dir = dirs_next::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("turnout")
        .join("orm_cache");
    let _ = std::fs::create_dir_all(&cache_dir);
    let shared_cache_path = cache_dir.join("shared.db");
    enable_wal(&shared_cache_path);

    // The runtime serves HTTP and hosts the shared network file source the render
    // workers' mbgl instances fetch through, so it must exist — and the source must
    // be registered — before the first worker can build a renderer.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(HTTP_WORKER_THREADS)
        .enable_all()
        .build()?;
    if let Err(e) = crate::orm_net::register(rt.handle().clone()) {
        eprintln!("ORM tiles: shared network source unavailable ({e}); using mbgl default");
    }

    let cache: Arc<StripedCache> = Arc::new(StripedCache::new(TILE_CACHE_CAPACITY));

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
                let outcome = if raw.write_with_encoder(encoder).is_ok() {
                    TileResult::Png(Bytes::from(png_buf))
                } else {
                    TileResult::Failed
                };

                // Lock order: waiters → cache stripe (matches serve_tile double-check order).
                let mut waiters = dispatch.waiters.lock().unpoison();
                // Compare the render's generation and insert under the stripe lock so
                // a concurrent offline toggle (which bumps then clears every stripe)
                // can never let a stale tile survive the clear.
                if let TileResult::Png(ref png) = outcome {
                    let stripe = cache.stripe(&key);
                    let mut stripe = stripe.lock().unpoison();
                    if generation == shared.generation.load(Ordering::SeqCst) {
                        stripe.put(key.clone(), png.clone());
                    }
                }
                let senders = waiters.remove(&key);
                drop(waiters);

                if let Some(senders) = senders {
                    for tx in senders {
                        let _ = tx.send(outcome.clone());
                    }
                }
            })?;
    }

    for i in 0..WORKER_COUNT {
        let dispatch = Arc::clone(&dispatch);
        let cache_path = shared_cache_path.clone();
        let encode_tx = encode_tx.clone();
        let shared = Arc::clone(&shared);
        let cache = Arc::clone(&cache);
        std::thread::Builder::new()
            .name(format!("orm-render-{i}"))
            .spawn(move || loop {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    render_worker_inner(i, &dispatch, &cache, &cache_path, &encode_tx, &shared);
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
    });

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let state2 = Arc::clone(&state);
    std::thread::Builder::new()
        .name("orm-http".into())
        .spawn(move || {
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
    let port = listener.local_addr().map_or(0, |a| a.port());
    state.shared.bound_port.store(port, Ordering::SeqCst);
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

/// Switches the shared mbgl ambient cache to WAL so the render workers'
/// concurrent reads and writes don't serialize on a rollback journal. WAL is a
/// persistent, file-level sqlite property; setting it once before any renderer
/// opens the database covers every worker. Purely an optimization — on failure
/// the cache still works in its default journal mode.
fn enable_wal(path: &std::path::Path) {
    let mode: Result<String, _> = rusqlite::Connection::open(path)
        .and_then(|conn| conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0)));
    match mode {
        Ok(mode) if mode.eq_ignore_ascii_case("wal") => {}
        Ok(mode) => eprintln!("ORM tiles: ambient cache journal_mode is '{mode}', not WAL"),
        Err(e) => eprintln!("ORM tiles: could not enable WAL on ambient cache: {e}"),
    }
}

fn build_renderer(
    cache_path: &std::path::Path,
    style: &str,
    shared: &OrmShared,
) -> maplibre_native::ImageRenderer<maplibre_native::Tile> {
    use maplibre_native::{ImageRendererBuilder, ResourceOptions};
    use std::num::NonZeroU32;

    let tile_size = NonZeroU32::new(TILE_SIZE).expect("nonzero");
    let resource_opts = ResourceOptions::default()
        .with_cache_path(cache_path.to_path_buf())
        .with_maximum_cache_size(MBGL_CACHE_BYTES);
    let mut r = ImageRendererBuilder::new()
        .with_size(tile_size, tile_size)
        .with_resource_options(resource_opts)
        .build_tile_renderer();
    r.load_style_from_json_str(resolve_style_source(style, shared));
    r
}

/// Returns the style JSON to load: the on-disk offline variant when offline mode is
/// active and readable, otherwise the embedded style. Offline styles reference their
/// tiles via an `mbtiles://<abs path>` source url that mbgl reads directly.
///
/// The embedded (online) style is rewritten to the configured base URL so a
/// self-hosted ORM replaces the default host; offline styles are used verbatim.
fn resolve_style_source(style: &str, shared: &OrmShared) -> String {
    let embedded = || {
        let base = shared.base_url.read().unpoison();
        rebase_style(style_json(style), &base)
    };
    let dir = shared.offline_dir.read().unpoison().clone();
    let Some(dir) = dir else {
        return embedded();
    };
    match offline_style_source(&dir, style) {
        Ok(source) => source,
        Err(e) => {
            eprintln!("[ORM] offline style '{style}' unavailable ({e}); using embedded style");
            embedded()
        }
    }
}

/// Substitutes the default ORM host in an embedded style with `base`. A no-op when
/// `base` is the default, so the common case allocates only the base copy.
fn rebase_style(json: &str, base: &str) -> String {
    if base == crate::settings::DEFAULT_ORM_BASE {
        json.to_string()
    } else {
        json.replace(crate::settings::DEFAULT_ORM_BASE, base)
    }
}

/// Reads `<dir>/styles/{style}_offline.json`.
fn offline_style_source(dir: &std::path::Path, style: &str) -> std::io::Result<String> {
    let path = dir.join("styles").join(format!("{style}_offline.json"));
    std::fs::read_to_string(path)
}

/// Blocks until a key is available, returning `(key, is_prefetch)`. Real requests
/// (`main`, newest-first) take priority over speculative prefetches.
fn pop_key_blocking(dispatch: &Dispatch) -> (Key, bool) {
    let mut q = dispatch.queues.lock().unpoison();
    loop {
        if let Some(k) = q.main.pop_back() {
            return (k, false);
        }
        if let Some(k) = q.prefetch.pop_back() {
            return (k, true);
        }
        q = dispatch.cv.wait(q).unpoison();
    }
}

/// Non-blocking variant of `pop_key_blocking`: returns `None` when both queues are
/// empty so the caller can do idle work (style warming) before parking.
fn try_pop_key(dispatch: &Dispatch) -> Option<(Key, bool)> {
    let mut q = dispatch.queues.lock().unpoison();
    if let Some(k) = q.main.pop_back() {
        return Some((k, false));
    }
    q.prefetch.pop_back().map(|k| (k, true))
}

/// Queues the 8 tiles surrounding `key` for speculative rendering, skipping any
/// already cached, already awaited, or already queued. Prefetch entries carry no
/// waiters; they only warm the cache so a subsequent pan resolves from it. Bounded
/// to `PREFETCH_QUEUE_CAP`. Returns how many keys were enqueued.
fn enqueue_prefetch_ring(q: &mut Queues, waiters: &Waiters, cache: &StripedCache, key: &Key) -> usize {
    let (style, z, x, y) = (&key.0, key.1, key.2, key.3);
    let max = i64::from(1u32 << z);
    let mut added = 0;
    for dy in -1i64..=1 {
        for dx in -1i64..=1 {
            if dx == 0 && dy == 0 {
                continue;
            }
            let (nx, ny) = (i64::from(x) + dx, i64::from(y) + dy);
            if nx < 0 || ny < 0 || nx >= max || ny >= max {
                continue;
            }
            let nkey: Key = (Arc::clone(style), z, nx as u32, ny as u32);
            if cache.contains(&nkey) || waiters.contains_key(&nkey) || q.prefetch.contains(&nkey) {
                continue;
            }
            q.prefetch.push_back(nkey);
            added += 1;
        }
    }
    while q.prefetch.len() > PREFETCH_QUEUE_CAP {
        q.prefetch.pop_front();
    }
    added
}

/// Drops queued prefetch entries whose style or zoom no longer matches the current
/// request. During a zoom animation the previous zoom's ring is dead weight.
fn drop_stale_prefetch(q: &mut Queues, style: &Arc<str>, z: u8) {
    q.prefetch.retain(|(pstyle, pz, ..)| pstyle == style && *pz == z);
}

fn fail_waiters(dispatch: &Dispatch, key: &Key) {
    let senders = dispatch.waiters.lock().unpoison().remove(key);
    if let Some(senders) = senders {
        for tx in senders {
            let _ = tx.send(TileResult::Failed);
        }
    }
}

/// Rebuilds every renderer when a generation bump (offline toggle) is observed.
fn refresh_generation(
    renderers: &mut HashMap<Arc<str>, maplibre_native::ImageRenderer<maplibre_native::Tile>>,
    shared: &OrmShared,
    current_gen: &mut u64,
) {
    let generation = shared.generation.load(Ordering::SeqCst);
    if generation != *current_gen {
        renderers.clear();
        *current_gen = generation;
    }
}

/// Builds one not-yet-built style renderer (styles in list order, `standard` first)
/// for the current generation. Returns `false` when every style is already built.
fn warm_one_style(
    renderers: &mut HashMap<Arc<str>, maplibre_native::ImageRenderer<maplibre_native::Tile>>,
    cache_path: &std::path::Path,
    shared: &OrmShared,
) -> bool {
    for (name, _) in STYLES {
        if !renderers.contains_key(*name) {
            renderers.insert(Arc::from(*name), build_renderer(cache_path, name, shared));
            return true;
        }
    }
    false
}

fn render_worker_inner(
    id: usize,
    dispatch: &Dispatch,
    cache: &Arc<StripedCache>,
    cache_path: &std::path::Path,
    encode_tx: &std::sync::mpsc::Sender<(Key, RawImage, u64)>,
    shared: &OrmShared,
) {
    eprintln!("[ORM worker {id}] Starting...");

    let mut renderers: HashMap<Arc<str>, maplibre_native::ImageRenderer<maplibre_native::Tile>> =
        HashMap::new();
    let mut render_count: u64 = 0;
    let mut current_gen = shared.generation.load(Ordering::SeqCst);
    let mut failed_recently: LruCache<Key, ()> =
        LruCache::new(NonZeroUsize::new(FAILED_KEY_MEMORY).expect("nonzero"));

    loop {
        // When both queues are empty, warm one unbuilt style (all 7 over successive
        // idle passes, `standard` first) then re-check so a real request never waits
        // behind a style build. Park only once every style for this generation is built.
        let (key, is_prefetch) = if let Some(k) = try_pop_key(dispatch) {
            k
        } else {
            refresh_generation(&mut renderers, shared, &mut current_gen);
            if warm_one_style(&mut renderers, cache_path, shared) {
                continue;
            }
            pop_key_blocking(dispatch)
        };
        let style = Arc::clone(&key.0);
        let (z, x, y) = (key.1, key.2, key.3);

        // An offline-mode toggle bumps the generation; drop every renderer so they
        // rebuild lazily against the current style source.
        refresh_generation(&mut renderers, shared, &mut current_gen);

        // Already rendered (a duplicate queue entry, or produced by another worker /
        // an earlier prefetch): serve any waiters from cache and move on.
        if let Some(png) = cache.get(&key) {
            let senders = dispatch.waiters.lock().unpoison().remove(&key);
            if let Some(senders) = senders {
                for tx in senders {
                    let _ = tx.send(TileResult::Png(png.clone()));
                }
            }
            continue;
        }

        // A real request whose clients have all disconnected is skipped. Prefetch
        // keys have no waiters and are rendered speculatively.
        if !is_prefetch {
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
                    image.into_inner()
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
                // A render error aborts the whole tile (mbgl Tile mode is strict),
                // and the webview never re-requests a tile it saw an error for —
                // so grant each key one requeue before failing its waiters.
                let first_failure = failed_recently.put(key.clone(), ()).is_none();
                if !is_prefetch && first_failure {
                    let mut q = dispatch.queues.lock().unpoison();
                    q.main.push_back(key);
                    drop(q);
                    dispatch.cv.notify_one();
                } else {
                    fail_waiters(dispatch, &key);
                }
            }
        }
    }
}

async fn serve_tilejson(State(state): State<Arc<OrmTileState>>) -> impl IntoResponse {
    serve_tilejson_for("standard", state.shared.bound_port.load(Ordering::SeqCst))
}

async fn serve_style_tilejson(
    Path(style): Path<String>,
    State(state): State<Arc<OrmTileState>>,
) -> impl IntoResponse {
    serve_tilejson_for(&style, state.shared.bound_port.load(Ordering::SeqCst))
}

fn serve_tilejson_for(style: &str, port: u16) -> (StatusCode, [(&'static str, &'static str); 1], String) {
    let json = serde_json::json!({
        "tilejson": "3.0.0",
        "name": format!("OpenRailwayMap {style}"),
        "tiles": [format!("http://127.0.0.1:{port}/{style}/{{z}}/{{y}}/{{x}}.png")],
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
    if z > MAX_REQUEST_ZOOM {
        return error_tile(StatusCode::NOT_FOUND, b"zoom out of range");
    }
    let style = canonical_style(&style);
    let x: u32 = x_png.strip_suffix(".png").unwrap_or(&x_png).parse().unwrap_or(0);
    let key: Key = (Arc::clone(&style), z, x, y);

    // Fast path: check cache before acquiring the waiters lock.
    if let Some(png) = state.cache.get(&key) {
        return ok_tile(png);
    }

    let (tx, rx) = oneshot::channel();
    {
        // Lock order: waiters → queues → cache stripe (consistent with encoder completion).
        let mut waiters = state.dispatch.waiters.lock().unpoison();
        // Double-check under the waiters lock to close the race with a just-finished encoder.
        if let Some(png) = state.cache.get(&key) {
            return ok_tile(png);
        }
        if let Some(senders) = waiters.get_mut(&key) {
            senders.push(tx);
        } else {
            waiters.insert(key.clone(), vec![tx]);
            let mut q = state.dispatch.queues.lock().unpoison();
            // A real request for (style, z) makes any queued prefetch for a different
            // style or zoom dead weight — drop it before enqueuing this request.
            drop_stale_prefetch(&mut q, &style, z);
            q.main.push_back(key.clone());
            // Bound the main queue: drop the stalest (front) request and resolve its
            // waiters as `Evicted` so the client retries rather than waiting out the timeout.
            if q.main.len() > MAIN_QUEUE_CAP
                && let Some(stale) = q.main.pop_front()
                && let Some(senders) = waiters.remove(&stale)
            {
                for s in senders {
                    let _ = s.send(TileResult::Evicted);
                }
            }
            // Only prefetch once the workers have caught up, so it never competes
            // with real requests during an active pan.
            let prefetched = if q.main.len() <= WORKER_COUNT {
                enqueue_prefetch_ring(&mut q, &waiters, &state.cache, &key)
            } else {
                0
            };
            drop(q);
            // Wake one worker per newly enqueued key (main + prefetch), capped at the
            // worker count, instead of stampeding all workers on every request.
            let wake = (1 + prefetched).min(WORKER_COUNT);
            for _ in 0..wake {
                state.dispatch.cv.notify_one();
            }
        }
    }

    match tokio::time::timeout(std::time::Duration::from_secs(RENDER_TIMEOUT_SECS), rx).await {
        Ok(Ok(TileResult::Png(png))) => ok_tile(png),
        Ok(Ok(TileResult::Evicted)) => StatusCode::NO_CONTENT.into_response(),
        Ok(Ok(TileResult::Failed)) => error_tile(StatusCode::INTERNAL_SERVER_ERROR, b"render failed"),
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

/// Returns the port the ORM tile server actually bound to. The server binds on a
/// background runtime, so this polls the shared value (published at bind time)
/// until it is set rather than assuming `PREFERRED_PORT`.
#[tauri::command]
pub async fn get_orm_port(handle: tauri::State<'_, OrmHandle>) -> Result<u16, String> {
    for _ in 0..PORT_WAIT_ATTEMPTS {
        let port = handle.shared.bound_port.load(Ordering::SeqCst);
        if port != 0 {
            return Ok(port);
        }
        tokio::time::sleep(std::time::Duration::from_millis(PORT_WAIT_INTERVAL_MS)).await;
    }
    Err("ORM tile server not bound".into())
}
