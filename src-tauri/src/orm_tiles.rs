use std::collections::{HashMap, VecDeque};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use image::{ImageBuffer, Rgba};
use lru::LruCache;
use tokio::sync::{oneshot, watch};
use tower_http::cors::CorsLayer;

use crate::error::{CommandError, CommandResult};
use crate::server_core::{self, UnpoisonExt};

const PREFERRED_PORT: u16 = 17854;
/// A just-stopped server can hold the preferred port briefly; retry before
/// falling back to an ephemeral port so the tile URL stays stable across restarts.
const BIND_ATTEMPTS: u32 = 10;
const BIND_RETRY_DELAY: Duration = Duration::from_millis(50);
const TILE_CACHE_CAPACITY: usize = 4096;
/// Render worker threads, sized from the CPU at startup (half the logical cores,
/// clamped). The floor is the historical default so typical 8-core machines never
/// regress; the ceiling bounds per-worker GPU renderer memory on many-core hosts.
const WORKER_COUNT_MIN: usize = 6;
const WORKER_COUNT_MAX: usize = 12;
/// PNG encoder threads, sized from the CPU at startup. Encoding a tile is cheap
/// (~1-2ms) next to a render, so a small pool keeps encode off the render path's
/// critical path without oversubscribing.
const ENCODER_COUNT_MIN: usize = 2;
const ENCODER_COUNT_MAX: usize = 4;
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
/// Rendered z19 parents kept for overzoom reuse. All children of one parent (4 at
/// z20, 16 at z21, 64 at z22) crop from a single render instead of each triggering
/// their own. A handful of parents cover a deep-zoom viewport plus panning slack.
const PARENT_CACHE_CAPACITY: usize = 32;
const RENDER_TIMEOUT_SECS: u64 = 30;
const STATS_INTERVAL: u64 = 50;
/// A render slower than this (ms) counts toward the sustained-slow signal. Healthy
/// hardware renders a tile in tens of ms; seconds means a software fallback.
const SLOW_RENDER_MS: u128 = 2000;
/// How many slow renders a process takes before one Sentry event flags the install
/// as slow (usually a software Vulkan fallback). High enough to ignore a cold cache
/// or a one-off stall, low enough to catch a user who notices the lag.
const SLOW_RENDER_STREAK: u64 = 20;
const WORKER_RESTART_DELAY_SECS: u64 = 1;
/// Cap on consecutive panic-restarts of a render worker. Past this it stays down
/// rather than spinning forever (each restart burns a `WORKER_RESTART_DELAY_SECS`
/// sleep + a re-render attempt); the renderer degrades to fewer workers.
const MAX_WORKER_RESTARTS: u32 = 10;
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

// One unified ORM overlay style. The view/coloring mode and every display option
// are driven by `global-state` (the `tracks` key selects the mode), matching
// openrailwaymap.app, which serves a single style and only changes global-state.
pub(crate) const STYLES: &[(&str, &str)] = &[
    ("orm", include_str!("../resources/orm/orm.json")),
];

type Key = (Arc<str>, u8, u32, u32);
type RawImage = ImageBuffer<Rgba<u8>, Vec<u8>>;
type Waiters = HashMap<Key, Vec<oneshot::Sender<TileResult>>>;
/// Shared cache of rendered z19 parent images for overzoom reuse, keyed by
/// `(style, parent_x, parent_y)`. Each entry carries the render generation so a
/// parent left over from before an offline/base-URL change is treated as a miss
/// rather than cropped into a stale child (the same guard the encoder applies).
type ParentKey = (Arc<str>, u32, u32);
type ParentCache = Mutex<LruCache<ParentKey, (u64, Arc<RawImage>)>>;

/// Outcome delivered to a tile request's waiters. `Evicted` is distinct from
/// `Failed` so the HTTP layer can answer 204 (revisit re-requests) rather than
/// 500 (which GL JS never retries) when a queued request is dropped on overflow.
#[derive(Clone)]
enum TileResult {
    Png(Bytes),
    Evicted,
    Failed,
}

/// The PNG tile cache: a hash-striped shared [`server_core::TileCache`] keyed by
/// `(style, z, x, y)`, sized to `TILE_CACHE_CAPACITY` across `CACHE_STRIPES` shards.
type TileCache = server_core::TileCache<Key>;

/// The per-mode URLs saved NIMBY Rails sources still use, each a view: the `orm`
/// style with the named [`CLASSIC_PRESETS`] entry over the shared option state.
///
/// [`CLASSIC_PRESETS`]: turnout_core::orm_presets::CLASSIC_PRESETS
const VIEWS: &[(&str, &str)] = &[
    ("standard", "Infrastructure"),
    ("speed", "Speed"),
    ("signals", "Signals"),
    ("electrification", "Electrification"),
    ("track", "Track"),
    ("operator", "Operator"),
    ("route", "Route"),
];

fn canonical_style(name: &str) -> Arc<str> {
    if STYLES.iter().chain(VIEWS).any(|(n, _)| *n == name) {
        Arc::from(name)
    } else {
        Arc::from("orm")
    }
}

/// The [`STYLES`] entry a style or view renders from. Views and unknown names
/// render from `orm`, so views need no style files of their own.
fn source_entry(name: &str) -> &'static (&'static str, &'static str) {
    STYLES.iter().find(|(n, _)| *n == name).unwrap_or(&STYLES[0])
}

/// The name of `name`'s [`source_entry`].
fn source_style(name: &str) -> &'static str {
    source_entry(name).0
}

/// The style JSON of `name`'s [`source_entry`].
fn style_json(name: &str) -> &'static str {
    source_entry(name).1
}

/// The option state `style` renders with: the shared state, with a view's preset
/// keys (`tracks` and the details) replaced by its preset's values. Every other
/// key (`levels`, `theme`, `date`, `show*`) follows the shared state.
fn view_state(
    shared: &serde_json::Map<String, serde_json::Value>,
    style: &str,
) -> serde_json::Map<String, serde_json::Value> {
    let mut state = shared.clone();
    let preset = VIEWS
        .iter()
        .find(|(name, _)| *name == style)
        .and_then(|(_, preset)| turnout_core::orm_presets::classic_state(preset));
    state.extend(preset.into_iter().flatten());
    state
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

pub(crate) fn extract_panic_message(e: &Box<dyn std::any::Any + Send>) -> String {
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
    /// embedded styles at load, so the default host or a self-hosted
    /// `OpenRailwayMap` serves them.
    base_url: RwLock<String>,
    /// Directory holding the self-hosted SDF glyphs (`<dir>/<fontstack>/<range>.pbf`).
    /// The online styles' `glyphs` is rewritten to a `file://` URL under here because
    /// upstream stopped serving SDF glyph PBFs. `None` in tests, which skips the rewrite.
    glyphs_dir: Option<PathBuf>,
    /// Current ORM option state (the new `OpenRailwayMap` UI's `state` block), substituted
    /// into the embedded styles' `global-state` expressions at load. Changing an option
    /// updates this and bumps `generation` so workers rebuild their renderers.
    orm_state: RwLock<serde_json::Map<String, serde_json::Value>>,
    generation: AtomicU64,
    bound_port: AtomicU16,
    /// Why the render backend became unavailable after start-up: a worker
    /// enumerated a Vulkan device at preflight but then failed logical-device
    /// creation and exited. Surfaced to the UI via `orm_disabled_reason` so the
    /// overlay's absence is explained instead of silent.
    render_failure: Mutex<Option<String>>,
    /// Set once the first worker reports a render failure, so the error is logged
    /// (and captured by Sentry) a single time per process rather than once per
    /// worker per idle warm-up pass.
    render_failure_logged: AtomicBool,
    /// Renders slower than [`SLOW_RENDER_MS`] with no slow or failed tile download
    /// during them, and whether the sustained-slow signal has fired. A persistently
    /// slow renderer (often a software Vulkan fallback) is otherwise invisible: the
    /// overlay works, just slowly, so nothing crashes and nothing reaches Sentry.
    slow_render_count: AtomicU64,
    slow_render_reported: AtomicBool,
}

impl OrmShared {
    /// Records why the render backend could not initialize and returns `true` the
    /// first time only, so the caller logs one Sentry-visible error and stays
    /// quiet after. One unusable GPU otherwise emits an event per worker per pass.
    fn note_render_failure(&self, detail: &str) -> bool {
        let reason = format!(
            "GPU Vulkan device initialization failed ({detail}); the OpenRailwayMap overlay can't render. Updating your graphics drivers usually fixes this."
        );
        *self.render_failure.lock().unpoison() = Some(reason);
        !self.render_failure_logged.swap(true, Ordering::SeqCst)
    }

    fn render_failure(&self) -> Option<String> {
        self.render_failure.lock().unpoison().clone()
    }

    /// Counts a slow render and returns true exactly once, when slow renders first
    /// cross [`SLOW_RENDER_STREAK`] for this process, so the caller emits a single
    /// Sentry event (carrying the GPU scope tags) for a persistently slow install
    /// rather than staying silent or logging on every tile.
    fn note_slow_render(&self) -> bool {
        let n = self.slow_render_count.fetch_add(1, Ordering::Relaxed) + 1;
        n >= SLOW_RENDER_STREAK && !self.slow_render_reported.swap(true, Ordering::SeqCst)
    }
}

struct OrmTileState {
    cache: Arc<TileCache>,
    dispatch: Arc<Dispatch>,
    shared: Arc<OrmShared>,
    /// Number of render workers; the prefetch/wake heuristics in `serve_tile`
    /// scale their thresholds off it.
    worker_count: usize,
}

pub struct OrmHandle {
    _shutdown_tx: watch::Sender<bool>,
    shared: Arc<OrmShared>,
    cache: Arc<TileCache>,
}

impl OrmHandle {
    /// Why the render backend disabled itself after a runtime warm-up failure, or
    /// `None` while rendering works. Distinct from the preflight reason in
    /// [`OrmAvailability`]: this covers a device that passed preflight enumeration
    /// but then failed logical-device creation inside a worker.
    pub fn render_failure(&self) -> Option<String> {
        self.shared.render_failure()
    }

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

    /// Sets one ORM display option (a `state` key the new `OpenRailwayMap` UI exposes:
    /// `signals`, `pois`, `platforms`, lifecycle filters, ...). Like `set_base_url`,
    /// bumps the render generation and clears the tile cache so tiles rendered with
    /// the previous value are dropped. A no-op when the value is unchanged.
    pub fn set_orm_option(&self, key: String, value: serde_json::Value) {
        {
            let mut state = self.shared.orm_state.write().unpoison();
            if state.get(&key) == Some(&value) {
                return;
            }
            state.insert(key, value);
        }
        self.shared.generation.fetch_add(1, Ordering::SeqCst);
        self.cache.clear();
    }

    /// The current ORM option state, for the UI to render its controls from.
    pub fn orm_options(&self) -> serde_json::Map<String, serde_json::Value> {
        self.shared.orm_state.read().unpoison().clone()
    }
}

/// Whether ORM tile rendering started, and if not, why — so the UI can tell the
/// user the rail overlay is unavailable (e.g. no working Vulkan runtime) instead
/// of it silently never appearing.
#[derive(Default)]
pub struct OrmAvailability(std::sync::Mutex<Option<String>>);

impl OrmAvailability {
    pub fn set_disabled(&self, reason: String) {
        *self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reason);
    }

    fn reason(&self) -> Option<String> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }
}

/// The reason ORM rendering is disabled, or `None` when it is running.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn orm_disabled_reason(app: tauri::AppHandle) -> Option<String> {
    use tauri::Manager;
    // The preflight reason (set before the handle exists) takes precedence; fall
    // back to a runtime warm-up failure recorded on the handle's shared state.
    app.try_state::<OrmAvailability>()
        .and_then(|s| s.reason())
        .or_else(|| app.try_state::<OrmHandle>().and_then(|h| h.render_failure()))
}

pub fn start_blocking() -> Result<OrmHandle, Box<dyn std::error::Error + Send + Sync>> {
    // mbgl's Vulkan backend crashes the whole process (an uncatchable C++ access
    // violation in `vk::DispatchLoaderDynamic::init`) when the Vulkan loader is
    // missing/broken or no Vulkan device exists. Predict both failures safely and
    // bail out so the app runs without ORM rendering instead of crashing a worker:
    //   1. Replicate mbgl's exact loader load and check it exposes the entry point.
    //   2. Confirm wgpu can enumerate a Vulkan adapter (a usable device).
    let loader = crate::vulkan::probe_loader();
    if !loader.usable() {
        return Err(format!(
            "Vulkan loader {} ({}); the OpenRailwayMap overlay can't render. Updating your graphics drivers usually fixes this.",
            loader.tag(),
            loader.path.as_deref().unwrap_or("no path")
        )
        .into());
    }
    if !crate::gpu::render_backend_available() {
        return Err("no working Vulkan GPU found (device creation failed); the OpenRailwayMap overlay can't render. Updating your graphics drivers usually fixes this.".into());
    }

    let dispatch = Arc::new(Dispatch {
        queues: Mutex::new(Queues { main: VecDeque::new(), prefetch: VecDeque::new() }),
        waiters: Mutex::new(HashMap::new()),
        cv: Condvar::new(),
    });

    let cache_dir = dirs_next::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("turnout")
        .join("orm_cache");
    let _ = std::fs::create_dir_all(&cache_dir);

    // Materialize the bundled SDF glyphs so the online styles can reference them via
    // a local file:// URL (upstream no longer serves glyph PBFs). A failure here only
    // costs text labels, so warn and carry on rather than failing the whole renderer.
    let glyphs_dir = match crate::orm_glyphs::materialize(&cache_dir) {
        Ok(dir) => Some(dir),
        Err(e) => {
            tracing::warn!("could not materialize ORM glyphs ({e}); map labels may not render");
            None
        }
    };

    let shared = Arc::new(OrmShared {
        offline_dir: RwLock::new(None),
        base_url: RwLock::new(crate::settings::DEFAULT_ORM_BASE.to_string()),
        glyphs_dir,
        orm_state: RwLock::new(crate::orm_state::default_state(STYLES[0].1)),
        generation: AtomicU64::new(0),
        bound_port: AtomicU16::new(0),
        render_failure: Mutex::new(None),
        render_failure_logged: AtomicBool::new(false),
        slow_render_count: AtomicU64::new(0),
        slow_render_reported: AtomicBool::new(false),
    });

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
        tracing::warn!("shared network source unavailable ({e}); using mbgl default");
    }

    let cache: Arc<TileCache> = Arc::new(TileCache::new(TILE_CACHE_CAPACITY, CACHE_STRIPES));

    // Shared across all workers so a sibling rendered by any worker reuses the
    // parent instead of re-rendering it. Generation-tagged entries make clearing
    // on an offline/base-URL change unnecessary — stale parents miss and re-render.
    let parent_cache: Arc<ParentCache> = Arc::new(Mutex::new(LruCache::new(
        NonZeroUsize::new(PARENT_CACHE_CAPACITY).expect("nonzero"),
    )));

    let cores = std::thread::available_parallelism().map_or(8, std::num::NonZero::get);
    let worker_count = (cores / 2).clamp(WORKER_COUNT_MIN, WORKER_COUNT_MAX);
    let encoder_count = (cores / 4).clamp(ENCODER_COUNT_MIN, ENCODER_COUNT_MAX);

    let (encode_tx, encode_rx) = std::sync::mpsc::channel::<(Key, RawImage, u64)>();
    let encode_rx = Arc::new(Mutex::new(encode_rx));

    for _ in 0..encoder_count {
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
                // Insert only if the render's generation still matches, evaluated
                // under the stripe lock so a concurrent offline toggle (which bumps
                // the generation then clears every stripe) can't let a stale tile
                // survive the clear.
                if let TileResult::Png(ref png) = outcome {
                    cache.put_if(key.clone(), png.clone(), || {
                        generation == shared.generation.load(Ordering::SeqCst)
                    });
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

    for i in 0..worker_count {
        let dispatch = Arc::clone(&dispatch);
        let cache_path = shared_cache_path.clone();
        let encode_tx = encode_tx.clone();
        let shared = Arc::clone(&shared);
        let cache = Arc::clone(&cache);
        let parent_cache = Arc::clone(&parent_cache);
        std::thread::Builder::new()
            .name(format!("orm-render-{i}"))
            .spawn(move || {
                let mut restarts = 0u32;
                loop {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        render_worker_inner(
                            i, &dispatch, &cache, &cache_path, &encode_tx, &shared, &parent_cache,
                        );
                    }));
                    match result {
                        // Clean exit: the dispatch queue was closed (shutdown).
                        Ok(()) => {
                            tracing::debug!("worker {i} exited cleanly");
                            break;
                        }
                        Err(e) => {
                            let msg = extract_panic_message(&e);
                            restarts += 1;
                            if restarts > MAX_WORKER_RESTARTS {
                                // A worker that keeps panicking would otherwise spin
                                // forever burning CPU; give up and let it stay down.
                                // This is a distinct, terminal condition (the renderer
                                // permanently loses a worker) rather than the panic
                                // itself, so surface it as its own Sentry issue.
                                tracing::error!("worker {i} panicked {restarts}× (last: {msg}); giving up");
                                break;
                            }
                            // The process-global sentry panic hook already captured
                            // this panic with a backtrace before catch_unwind returned
                            // Err (catch_unwind doesn't suppress the hook). Log at warn
                            // so it lands as a breadcrumb, not a second, backtrace-less
                            // duplicate issue.
                            tracing::warn!("worker {i} panicked: {msg} — restarting ({restarts}/{MAX_WORKER_RESTARTS})");
                            std::thread::sleep(std::time::Duration::from_secs(WORKER_RESTART_DELAY_SECS));
                        }
                    }
                }
            })?;
    }

    let state = Arc::new(OrmTileState {
        cache: Arc::clone(&cache),
        dispatch,
        shared: Arc::clone(&shared),
        worker_count,
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
    let listener = match server_core::bind_with_retry(PREFERRED_PORT, BIND_ATTEMPTS, BIND_RETRY_DELAY).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("failed to bind: {e}");
            return;
        }
    };
    let port = listener.local_addr().map_or(0, |a| a.port());
    state.shared.bound_port.store(port, Ordering::SeqCst);
    tracing::info!("tile renderer at http://127.0.0.1:{port}/{{style}}/{{z}}/{{y}}/{{x}}.png");

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
        Ok(mode) => tracing::warn!("ambient cache journal_mode is '{mode}', not WAL"),
        Err(e) => tracing::warn!("could not enable WAL on ambient cache: {e}"),
    }
}

type RendererResult =
    Result<maplibre_native::ImageRenderer<maplibre_native::Tile>, Box<dyn std::error::Error + Send + Sync>>;

fn build_renderer(cache_path: &std::path::Path, style: &str, shared: &OrmShared) -> RendererResult {
    use maplibre_native::{ImageRendererBuilder, ResourceOptions};
    use std::num::NonZeroU32;

    let tile_size = NonZeroU32::new(TILE_SIZE).expect("nonzero");
    let resource_opts = ResourceOptions::default()
        .with_cache_path(cache_path.to_path_buf())
        .with_maximum_cache_size(MBGL_CACHE_BYTES);
    // The graphics backend can fail to initialize (no working Vulkan runtime / no
    // GPU); it now returns an error instead of crashing the process.
    let mut r = ImageRendererBuilder::new()
        .with_size(tile_size, tile_size)
        .with_resource_options(resource_opts)
        .build_tile_renderer()?;
    // global-state can only be set once the style has finished loading (mbgl throws
    // otherwise), so wait for the load before pushing the ORM options in.
    r.load_style_from_json_str(resolve_style_source(style, shared))
        .wait()
        .map_err(|e| format!("ORM style '{style}' failed to load: {e}"))?;
    apply_global_state(&mut r, shared, style);
    Ok(r)
}

/// Pushes `name`'s ORM option values ([`view_state`]) into the renderer's style as
/// native `global-state`, so the style's `global-state` expressions evaluate against
/// the user's selections. maplibre-native evaluates these directly (core #4516), so
/// no expression rewriting is needed.
fn apply_global_state(
    renderer: &mut maplibre_native::ImageRenderer<maplibre_native::Tile>,
    shared: &OrmShared,
    name: &str,
) {
    use maplibre_native::GlobalStateValue;
    let state = view_state(&shared.orm_state.read().unpoison(), name);
    let mut style = renderer.style();
    for (key, value) in &state {
        let resolved = match value {
            serde_json::Value::String(s) => GlobalStateValue::String(s.clone()),
            serde_json::Value::Bool(b) => GlobalStateValue::Bool(*b),
            serde_json::Value::Number(n) => GlobalStateValue::Number(n.as_f64().unwrap_or(0.0)),
            serde_json::Value::Null => GlobalStateValue::Null,
            serde_json::Value::Array(items) => GlobalStateValue::StringList(
                items.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect(),
            ),
            // ORM options are never object-valued.
            serde_json::Value::Object(_) => continue,
        };
        style.set_global_state(key, resolved);
    }
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
        // Keep the style's `global-state` expressions intact; they are applied to the
        // renderer natively via `apply_global_state` rather than rewritten here.
        rebase_style(style_json(style), &base, shared.glyphs_dir.as_deref())
    };
    let dir = shared.offline_dir.read().unpoison().clone();
    let Some(dir) = dir else {
        return embedded();
    };
    match offline_style_source(&dir, style) {
        Ok(source) => source,
        Err(e) => {
            tracing::warn!("offline style '{style}' unavailable ({e}); using embedded style");
            embedded()
        }
    }
}

/// Rewrites an embedded style for runtime use: glyphs always point at the local
/// self-hosted SDF directory (upstream dropped the glyph-PBF endpoint), while tiles
/// and sprites move from the style's baked-in [`STYLE_SOURCE_BASE`] host to the
/// configured `base`. The glyph rewrite runs first so the host swap never touches
/// the now-`file://` glyphs URL.
///
/// [`STYLE_SOURCE_BASE`]: crate::settings::STYLE_SOURCE_BASE
fn rebase_style(json: &str, base: &str, glyphs_dir: Option<&std::path::Path>) -> String {
    let source = crate::settings::STYLE_SOURCE_BASE;
    let glyph_rewritten = match glyphs_dir {
        Some(dir) => json.replace(
            &format!("{source}/font/"),
            &format!("file://{}/", dir.display()),
        ),
        None => json.to_string(),
    };
    if base == source {
        glyph_rewritten
    } else {
        glyph_rewritten.replace(source, base)
    }
}

/// Reads `<dir>/styles/{source}_offline.json`, where `source` is `style`'s
/// [`source_style`].
fn offline_style_source(dir: &std::path::Path, style: &str) -> std::io::Result<String> {
    let path = dir.join("styles").join(format!("{}_offline.json", source_style(style)));
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
fn enqueue_prefetch_ring(q: &mut Queues, waiters: &Waiters, cache: &TileCache, key: &Key) -> usize {
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
/// Builds one not-yet-built style. `Ok(true)` warmed one, `Ok(false)` all warm;
/// `Err` means the graphics backend can't initialize (ORM is unavailable).
fn warm_one_style(
    renderers: &mut HashMap<Arc<str>, maplibre_native::ImageRenderer<maplibre_native::Tile>>,
    cache_path: &std::path::Path,
    shared: &OrmShared,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    for (name, _) in STYLES {
        if !renderers.contains_key(*name) {
            renderers.insert(Arc::from(*name), build_renderer(cache_path, name, shared)?);
            return Ok(true);
        }
    }
    Ok(false)
}

fn render_worker_inner(
    id: usize,
    dispatch: &Dispatch,
    cache: &Arc<TileCache>,
    cache_path: &std::path::Path,
    encode_tx: &std::sync::mpsc::Sender<(Key, RawImage, u64)>,
    shared: &OrmShared,
    parent_cache: &ParentCache,
) {
    tracing::debug!("worker {id} starting");

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
            match warm_one_style(&mut renderers, cache_path, shared) {
                Ok(true) => continue,
                Ok(false) => pop_key_blocking(dispatch),
                Err(e) => {
                    // The graphics backend can't initialize; ORM can't render.
                    // Record why so the UI can tell the user, exit cleanly (no
                    // crash, no restart spin — catch_unwind treats a normal return
                    // as shutdown), and log the error only for the first worker so
                    // one unusable GPU doesn't flood Sentry with an event each.
                    if shared.note_render_failure(&e.to_string()) {
                        tracing::error!("worker {id}: ORM renderer init failed, disabling: {e}");
                    } else {
                        tracing::debug!("worker {id}: ORM renderer init failed (already reported): {e}");
                    }
                    return;
                }
            }
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
            tracing::debug!("worker {id} loading style '{style}'");
            match build_renderer(cache_path, &style, shared) {
                Ok(r) => {
                    renderers.insert(Arc::clone(&style), r);
                    tracing::debug!("worker {id} style '{style}' loaded");
                }
                Err(e) => {
                    if shared.note_render_failure(&e.to_string()) {
                        tracing::error!("worker {id}: ORM renderer init failed, disabling: {e}");
                    } else {
                        tracing::debug!("worker {id}: ORM renderer init failed (already reported): {e}");
                    }
                    return;
                }
            }
        }

        let (render_z, render_x, render_y) = parent_coords(z, x, y);
        let is_overzoom = z > MAX_NATIVE_ZOOM;

        // Overzoom siblings share one z19 parent render. Reuse a cached parent from
        // the current generation instead of re-rendering it for every child; a
        // stale-generation entry is ignored so it can't crop into a stale tile.
        let cached_parent = if is_overzoom {
            let key = (Arc::clone(&style), render_x, render_y);
            parent_cache
                .lock()
                .unpoison()
                .get(&key)
                .and_then(|(entry_gen, img)| (*entry_gen == current_gen).then(|| Arc::clone(img)))
        } else {
            None
        };

        let start = Instant::now();
        let fetches_before = crate::orm_net::fetch_stats();
        let raw_result = if let Some(parent) = cached_parent {
            Ok(crop_and_upscale(&parent, z, x, y))
        } else {
            renderers
                .get_mut(style.as_ref())
                .expect("just inserted")
                .render_tile(render_z, render_x, render_y)
                .map(|image| {
                    if is_overzoom {
                        let parent = Arc::new(image.into_inner());
                        parent_cache.lock().unpoison().put(
                            (Arc::clone(&style), render_x, render_y),
                            (current_gen, Arc::clone(&parent)),
                        );
                        crop_and_upscale(&parent, z, x, y)
                    } else {
                        image.into_inner()
                    }
                })
        };
        let render_ms = start.elapsed().as_millis();
        render_count += 1;
        // mbgl downloads a tile's sources inside the render, so a render is only
        // GPU/CPU-bound when no download during it was slow or failed.
        let render_bound = !fetches_before.degraded_since(crate::orm_net::fetch_stats());

        match raw_result {
            Ok(raw) => {
                if render_ms > SLOW_RENDER_MS && render_bound && shared.note_slow_render() {
                    // One Sentry-visible event per process. The GPU scope tags
                    // (render_device, render_device_software, gpu, vulkan_icds)
                    // travel with it, so a "Turnout ORM is slow" report becomes
                    // diagnosable instead of relying on the user to inspect their GPU.
                    let net = crate::orm_net::fetch_stats();
                    tracing::error!(
                        "ORM rendering is persistently slow: worker {id} tile '{style}' z{z} took {render_ms}ms ({render_count} rendered) with no slow tile downloads during it. Session downloads: {} total, {} slow, {} failed. Check the render_device and render_device_software tags.",
                        net.total, net.slow, net.failed
                    );
                } else if render_count.is_multiple_of(STATS_INTERVAL) || render_ms > SLOW_RENDER_MS {
                    tracing::debug!(
                        "worker {id} rendered {render_count} tiles (last: {render_ms}ms)"
                    );
                }
                let _ = encode_tx.send((key, raw, current_gen));
            }
            Err(e) => {
                tracing::warn!(
                    "worker {id} failed z={render_z} x={render_x} y={render_y}: \
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
    serve_tilejson_for("orm", state.shared.bound_port.load(Ordering::SeqCst))
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
            let prefetched = if q.main.len() <= state.worker_count {
                enqueue_prefetch_ring(&mut q, &waiters, &state.cache, &key)
            } else {
                0
            };
            drop(q);
            // Wake one worker per newly enqueued key (main + prefetch), capped at the
            // worker count, instead of stampeding all workers on every request.
            let wake = (1 + prefetched).min(state.worker_count);
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
            tracing::warn!("http timeout z={z} x={x} y={y}");
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
) -> CommandResult<()> {
    let dir = dir.map(PathBuf::from);
    if let Some(d) = &dir {
        let mbtiles = d.join(OFFLINE_MBTILES_FILE);
        if !mbtiles.is_file() {
            return Err(CommandError::NotFound(format!("no offline tile store at {}", mbtiles.display())));
        }
    }
    handle.set_offline_dir(dir);
    Ok(())
}

/// Returns the port the ORM tile server actually bound to. The server binds on a
/// background runtime, so this polls the shared value (published at bind time)
/// until it is set rather than assuming `PREFERRED_PORT`.
/// Sets one ORM display option and persists it. The renderer rebuilds against the
/// new value; the frontend reloads the ORM layer afterwards to pull fresh tiles.
#[tauri::command]
pub async fn set_orm_option(
    app: tauri::AppHandle,
    handle: tauri::State<'_, OrmHandle>,
    key: String,
    value: serde_json::Value,
) -> CommandResult<()> {
    handle.set_orm_option(key, value);
    crate::settings::save_orm_options(&app, &handle.orm_options());
    Ok(())
}

/// The current ORM option values, for the UI to initialize its controls.
#[tauri::command]
pub async fn get_orm_options(
    handle: tauri::State<'_, OrmHandle>,
) -> CommandResult<serde_json::Map<String, serde_json::Value>> {
    Ok(handle.orm_options())
}

#[tauri::command]
pub async fn get_orm_port(handle: tauri::State<'_, OrmHandle>) -> CommandResult<u16> {
    for _ in 0..PORT_WAIT_ATTEMPTS {
        let port = handle.shared.bound_port.load(Ordering::SeqCst);
        if port != 0 {
            return Ok(port);
        }
        tokio::time::sleep(std::time::Duration::from_millis(PORT_WAIT_INTERVAL_MS)).await;
    }
    Err(CommandError::Server("ORM tile server not bound".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_zoom_tiles_are_their_own_parent() {
        assert_eq!(parent_coords(MAX_NATIVE_ZOOM, 100, 200), (MAX_NATIVE_ZOOM, 100, 200));
        assert_eq!(parent_coords(0, 0, 0), (0, 0, 0));
    }

    #[test]
    fn overzoom_siblings_share_one_parent() {
        // The four z20 children of z19 parent (10, 20) must all resolve to it, so
        // the parent cache renders it once and crops four times.
        let parent = (MAX_NATIVE_ZOOM, 10, 20);
        for (cx, cy) in [(20, 40), (21, 40), (20, 41), (21, 41)] {
            assert_eq!(parent_coords(MAX_NATIVE_ZOOM + 1, cx, cy), parent);
        }
        // A z22 tile resolves three levels up to the same parent.
        assert_eq!(parent_coords(MAX_NATIVE_ZOOM + 3, 10 << 3, 20 << 3), parent);
    }

    #[test]
    fn crop_and_upscale_returns_full_tile_size() {
        let parent = RawImage::from_pixel(TILE_SIZE, TILE_SIZE, Rgba([1, 2, 3, 4]));
        let child = crop_and_upscale(&parent, MAX_NATIVE_ZOOM + 1, 21, 41);
        assert_eq!(child.dimensions(), (TILE_SIZE, TILE_SIZE));
    }

    fn test_shared() -> OrmShared {
        OrmShared {
            offline_dir: RwLock::new(None),
            base_url: RwLock::new(crate::settings::DEFAULT_ORM_BASE.to_string()),
            glyphs_dir: None,
            orm_state: RwLock::new(crate::orm_state::default_state(STYLES[0].1)),
            generation: AtomicU64::new(0),
            bound_port: AtomicU16::new(0),
            render_failure: Mutex::new(None),
            render_failure_logged: AtomicBool::new(false),
            slow_render_count: AtomicU64::new(0),
            slow_render_reported: AtomicBool::new(false),
        }
    }

    #[test]
    fn rebase_style_points_glyphs_at_local_dir() {
        let json = r#"{"glyphs":"https://openrailwaymap.app/font/{fontstack}/{range}.pbf","sprite":"https://openrailwaymap.app/sprite/symbols","sources":{"high":{"tiles":["https://openrailwaymap.app/railway_line_high,railway_text_km/{z}/{x}/{y}"]}}}"#;
        let out = rebase_style(json, crate::settings::STYLE_SOURCE_BASE, Some(std::path::Path::new("/cache/glyphs")));
        // Glyphs go local, with the fontstack/range placeholders preserved.
        assert!(out.contains("\"glyphs\":\"file:///cache/glyphs/{fontstack}/{range}.pbf\""), "{out}");
        // Tiles and sprite stay on the style's own host when that is the base.
        assert!(out.contains("https://openrailwaymap.app/railway_line_high"));
        assert!(out.contains("https://openrailwaymap.app/sprite/symbols"));
        assert!(!out.contains("openrailwaymap.app/font/"));
    }

    /// maplibre-native takes `layout.visibility` only as a literal, and decodes every
    /// vector source as MVT. The style build moves option gates into filters and drops
    /// upstream's `encoding: mlt`; this guards both.
    #[test]
    fn embedded_styles_are_native_compatible() {
        for (name, json) in STYLES {
            let style: serde_json::Value = serde_json::from_str(json).expect("embedded style parses");
            for layer in style["layers"].as_array().expect("layers") {
                let visibility = &layer["layout"]["visibility"];
                assert!(
                    visibility.is_null() || visibility.is_string(),
                    "{name} layer {} has an expression visibility",
                    layer["id"]
                );
            }
            for (source, def) in style["sources"].as_object().expect("sources") {
                assert!(def.get("encoding").is_none(), "{name} source {source} sets an encoding");
            }
        }
    }

    /// Whether `needle` occurs anywhere in `expr`, including inside objects such as
    /// a whole layer.
    fn mentions(expr: &serde_json::Value, needle: &serde_json::Value) -> bool {
        expr == needle
            || expr.as_array().is_some_and(|items| items.iter().any(|e| mentions(e, needle)))
            || expr.as_object().is_some_and(|fields| fields.values().any(|e| mentions(e, needle)))
    }

    /// The expressions `filter` ANDs together: its `all` arguments, flattened.
    fn conjuncts(filter: &serde_json::Value) -> Vec<&serde_json::Value> {
        match filter.as_array().map(Vec::as_slice) {
            Some([all, rest @ ..]) if all == "all" => rest.iter().flat_map(conjuncts).collect(),
            _ => vec![filter],
        }
    }

    /// Whether `expr` is the Levels clause
    /// `["any", ["<", ["zoom"], LEVELS_MIN_ZOOM], ["in", band, ["global-state", LEVELS_KEY]]]`.
    fn is_level_clause(expr: &serde_json::Value, band: &serde_json::Value) -> bool {
        use turnout_core::orm_levels::{LEVELS_KEY, LEVELS_MIN_ZOOM};
        *expr == serde_json::json!(["any", ["<", ["zoom"], LEVELS_MIN_ZOOM], ["in", band, ["global-state", LEVELS_KEY]]])
    }

    /// Whether `filter` ANDs in the Levels clause with `band` exactly once: alone,
    /// or as an argument of `all` (nested `all`s too, since the dasharray split
    /// wraps filters after the clause is added).
    fn is_level_gated(filter: &serde_json::Value, band: &serde_json::Value) -> bool {
        conjuncts(filter).into_iter().filter(|e| is_level_clause(e, band)).count() == 1
    }

    /// The band expressions the style build gives line layers and platform (and
    /// platform edge) layers: the band from a numeric `layer` when the feature has
    /// one, else lines fall back to their tunnel / bridge flags and platforms to
    /// ground.
    fn level_bands() -> (serde_json::Value, serde_json::Value) {
        use serde_json::json;
        let [underground, ground, elevated] = turnout_core::orm_levels::LEVEL_BANDS;
        let by_layer = json!([
            "case",
            ["<", ["number", ["get", "layer"], 0], 0], underground,
            [">", ["number", ["get", "layer"], 0], 0], elevated,
            ground
        ]);
        let line = json!([
            "case",
            ["has", "layer"], by_layer,
            ["==", ["get", "tunnel"], true], underground,
            ["==", ["get", "bridge"], true], elevated,
            ground
        ]);
        let platform = json!(["case", ["has", "layer"], by_layer, ground]);
        (line, platform)
    }

    /// The Levels option gates every line and platform layer on the `levels`
    /// global-state with its source layer's band. Every other layer, including the
    /// low-zoom lines and the kilometre labels (which carry no level data), stays
    /// ungated.
    #[test]
    fn level_filter_gates_line_and_platform_layers() {
        const LEVEL_SOURCE_LAYERS: [&str; 3] =
            ["railway_line_high", "standard_railway_platforms", "standard_railway_platform_edges"];
        let (line_band, platform_band) = level_bands();
        let bands = [&line_band, &platform_band, &platform_band];
        let gate = serde_json::json!(["global-state", turnout_core::orm_levels::LEVELS_KEY]);
        let style: serde_json::Value = serde_json::from_str(STYLES[0].1).expect("embedded style parses");
        let (mut gated, mut km, mut low_zoom) = ([0; LEVEL_SOURCE_LAYERS.len()], 0, 0);
        for layer in style["layers"].as_array().expect("layers") {
            let source_layer = layer["source-layer"].as_str().unwrap_or_default();
            if let Some(i) = LEVEL_SOURCE_LAYERS.iter().position(|s| *s == source_layer) {
                assert!(
                    is_level_gated(&layer["filter"], bands[i]),
                    "layer {} has no {source_layer} levels clause: {}",
                    layer["id"],
                    layer["filter"]
                );
                gated[i] += 1;
                continue;
            }
            assert!(!mentions(layer, &gate), "ungated layer {} mentions the levels state", layer["id"]);
            km += usize::from(source_layer == "railway_text_km");
            low_zoom += usize::from(source_layer.ends_with("_railway_line_low"));
        }
        assert!(
            gated.iter().all(|n| *n > 0) && km > 0 && low_zoom > 0,
            "{gated:?} gated per {LEVEL_SOURCE_LAYERS:?}, {km} km, {low_zoom} low-zoom"
        );
    }

    #[test]
    fn rebase_style_swaps_host_but_keeps_glyphs_local() {
        let json = r#"{"glyphs":"https://openrailwaymap.app/font/{fontstack}/{range}.pbf","sources":{"high":{"tiles":["https://openrailwaymap.app/railway_line_high/{z}/{x}/{y}"]}}}"#;
        let out = rebase_style(json, "http://localhost:8000", Some(std::path::Path::new("/c/glyphs")));
        // A self-hosted base moves tiles, but glyphs still come from the local dir.
        assert!(out.contains("\"glyphs\":\"file:///c/glyphs/{fontstack}/{range}.pbf\""), "{out}");
        assert!(out.contains("http://localhost:8000/railway_line_high"));
        assert!(!out.contains("openrailwaymap.app"));
    }

    #[test]
    fn rebase_style_moves_bundled_style_to_default_base() {
        let out = rebase_style(STYLES[0].1, crate::settings::DEFAULT_ORM_BASE, Some(std::path::Path::new("/c/glyphs")));
        let style: serde_json::Value = serde_json::from_str(&out).expect("rebased style parses");
        assert_eq!(style["glyphs"], "file:///c/glyphs/{fontstack}/{range}.pbf");
        let tiles: Vec<&str> = style["sources"]
            .as_object()
            .expect("sources")
            .values()
            .filter_map(|s| s["tiles"].as_array())
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .collect();
        assert!(!tiles.is_empty());
        assert!(tiles.iter().all(|t| t.starts_with("https://orm.sorlie.io/")), "{tiles:?}");
        assert!(!out.contains(crate::settings::STYLE_SOURCE_BASE));
    }

    #[test]
    fn views_and_styles_keep_their_name_and_others_fall_back_to_orm() {
        assert_eq!(&*canonical_style("signals"), "signals");
        assert_eq!(&*canonical_style("orm"), "orm");
        assert_eq!(&*canonical_style("nope"), "orm");
    }

    #[test]
    fn views_render_from_the_orm_style() {
        assert_eq!(source_style("speed"), "orm");
        assert_eq!(source_style("nope"), "orm");
        assert_eq!(style_json("speed"), STYLES[0].1);
    }

    #[test]
    fn every_view_names_a_classic_preset() {
        for (view, preset) in VIEWS {
            assert!(turnout_core::orm_presets::classic_state(preset).is_some(), "{view}: no preset {preset}");
        }
    }

    #[test]
    fn view_state_sets_preset_keys_and_keeps_the_rest_shared() {
        use serde_json::json;
        use turnout_core::orm_levels::LEVELS_KEY;
        let mut shared = crate::orm_state::default_state(STYLES[0].1);
        shared.insert(LEVELS_KEY.to_owned(), json!(["underground"]));
        shared.insert("theme".to_owned(), json!("dark"));
        shared.insert("platforms".to_owned(), json!("height"));

        let state = view_state(&shared, "signals");
        assert_eq!(state.get("tracks"), Some(&json!("train_protection")));
        assert_eq!(state.get("boxes"), Some(&json!("plain")));
        assert_eq!(state.get("platforms"), Some(&json!(turnout_core::orm_presets::NONE)));
        assert_eq!(state.get(LEVELS_KEY), Some(&json!(["underground"])));
        assert_eq!(state.get("theme"), Some(&json!("dark")));
        assert_eq!(state.len(), shared.len());
    }

    #[test]
    fn view_state_of_a_style_is_the_shared_state() {
        let mut shared = crate::orm_state::default_state(STYLES[0].1);
        shared.insert("tracks".to_owned(), serde_json::json!("gauge"));
        assert_eq!(view_state(&shared, "orm"), shared);
    }

    #[test]
    fn offline_views_read_the_orm_offline_style() {
        let dir = std::env::temp_dir().join(format!("turnout-orm-views-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("styles")).expect("styles dir");
        std::fs::write(dir.join("styles").join("orm_offline.json"), "{}").expect("offline style");
        let source = offline_style_source(&dir, "electrification");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(source.expect("view reads the orm offline style"), "{}");
    }

    #[test]
    fn render_failure_is_recorded_and_logged_once() {
        let shared = test_shared();
        assert_eq!(shared.render_failure(), None);
        // First failure logs (returns true) and records a user-facing reason.
        assert!(shared.note_render_failure("ErrorInitializationFailed"));
        let reason = shared.render_failure().expect("reason recorded");
        assert!(reason.contains("ErrorInitializationFailed"));
        assert!(reason.contains("graphics drivers"));
        // Every later failure (one per worker per pass) stays quiet.
        assert!(!shared.note_render_failure("ErrorInitializationFailed"));
        assert!(!shared.note_render_failure("ErrorInitializationFailed"));
    }

    #[test]
    fn slow_render_reports_once_after_streak() {
        let shared = test_shared();
        // Below the streak: a few slow renders don't fire (cold cache, one-off stall).
        for _ in 0..SLOW_RENDER_STREAK - 1 {
            assert!(!shared.note_slow_render());
        }
        // Crossing the streak fires exactly once.
        assert!(shared.note_slow_render());
        // Every later slow render stays quiet.
        assert!(!shared.note_slow_render());
        assert!(!shared.note_slow_render());
    }
}
