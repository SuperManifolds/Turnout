//! Shared scaffolding for the app's local HTTP tile servers: port bind + retry,
//! graceful shutdown, an LRU-cache helper, a reqwest factory, and the
//! poison-recovery lock extension. Each server keeps its own request-handling
//! (raster compositing, MVT encoding, native rendering, resampling) and layers it
//! on top of these primitives.

use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;
use std::sync::{Mutex, MutexGuard, PoisonError, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration;

use axum::body::Bytes;
use lru::LruCache;
use tokio::net::TcpListener;
use tokio::sync::watch;

/// User-Agent for every outbound HTTP request the app makes.
pub const USER_AGENT: &str =
    concat!("Turnout/", env!("CARGO_PKG_VERSION"), " (+https://github.com/SuperManifolds/Turnout)");

/// Connection-establishment timeout shared by every outbound client. Separate
/// from the per-request timeout so a dead host fails fast rather than hanging
/// for the full request budget.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Retry policy for the shared tile-fetch helper: attempt counts and the
/// exponential-backoff bounds applied to transient failures.
const MAX_RETRIES: u32 = 5;
const INITIAL_RETRY_DELAY: Duration = Duration::from_millis(500);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(10);
/// Fallback pause when a `429`/`503` response omits a parseable `Retry-After`.
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(5);

/// Recover a poisoned lock guard instead of panicking: a worker that panicked
/// mid-update must not wedge every later request, so we take the (possibly stale)
/// guard and carry on.
pub trait UnpoisonExt<T> {
    fn unpoison(self) -> T;
}

impl<'a, T> UnpoisonExt<MutexGuard<'a, T>> for Result<MutexGuard<'a, T>, PoisonError<MutexGuard<'a, T>>> {
    fn unpoison(self) -> MutexGuard<'a, T> {
        self.unwrap_or_else(PoisonError::into_inner)
    }
}

impl<'a, T> UnpoisonExt<RwLockReadGuard<'a, T>>
    for Result<RwLockReadGuard<'a, T>, PoisonError<RwLockReadGuard<'a, T>>>
{
    fn unpoison(self) -> RwLockReadGuard<'a, T> {
        self.unwrap_or_else(PoisonError::into_inner)
    }
}

impl<'a, T> UnpoisonExt<RwLockWriteGuard<'a, T>>
    for Result<RwLockWriteGuard<'a, T>, PoisonError<RwLockWriteGuard<'a, T>>>
{
    fn unpoison(self) -> RwLockWriteGuard<'a, T> {
        self.unwrap_or_else(PoisonError::into_inner)
    }
}

/// Bind `127.0.0.1:port`, retrying briefly (a just-stopped server can hold the
/// port for a moment), then fall back to an ephemeral port so startup always
/// succeeds and the URL stays stable across restarts when possible.
pub async fn bind_with_retry(port: u16, attempts: u32, delay: Duration) -> Result<TcpListener, String> {
    for _ in 0..attempts {
        if let Ok(listener) = TcpListener::bind(format!("127.0.0.1:{port}")).await {
            return Ok(listener);
        }
        tokio::time::sleep(delay).await;
    }
    TcpListener::bind("127.0.0.1:0").await.map_err(|e| e.to_string())
}

/// A `Mutex<LruCache>` of the given capacity.
pub fn lru_cache<K: Hash + Eq, V>(capacity: usize) -> Mutex<LruCache<K, V>> {
    Mutex::new(LruCache::new(NonZeroUsize::new(capacity).expect("nonzero cache capacity")))
}

/// A hash-striped LRU cache of encoded tile bytes, shared by the local tile
/// servers. `stripes == 1` is a plain single-lock cache; a higher stripe count
/// spreads lock contention for a high-throughput server (a key always maps to one
/// stripe, so no op ever holds two stripe locks). Values are [`Bytes`] so a get or
/// put shares the buffer by refcount instead of copying the tile.
pub struct TileCache<K> {
    stripes: Vec<Mutex<LruCache<K, Bytes>>>,
}

impl<K: Hash + Eq> TileCache<K> {
    /// A cache holding up to `total_capacity` tiles, split evenly across `stripes`
    /// independently-locked shards (at least one).
    pub fn new(total_capacity: usize, stripes: usize) -> Self {
        let stripes = stripes.max(1);
        let per_stripe = (total_capacity / stripes).max(1);
        let cap = NonZeroUsize::new(per_stripe).expect("nonzero cache capacity");
        Self { stripes: (0..stripes).map(|_| Mutex::new(LruCache::new(cap))).collect() }
    }

    fn stripe(&self, key: &K) -> &Mutex<LruCache<K, Bytes>> {
        if self.stripes.len() == 1 {
            return &self.stripes[0];
        }
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut hasher);
        &self.stripes[(hasher.finish() as usize) % self.stripes.len()]
    }

    pub fn get(&self, key: &K) -> Option<Bytes> {
        self.stripe(key).lock().unpoison().get(key).cloned()
    }

    pub fn contains(&self, key: &K) -> bool {
        self.stripe(key).lock().unpoison().contains(key)
    }

    pub fn put(&self, key: K, value: Bytes) {
        self.stripe(&key).lock().unpoison().put(key, value);
    }

    /// Insert only if `keep()` — evaluated while the stripe lock is held — returns
    /// true. Lets a caller gate the insert on state (e.g. a generation counter)
    /// that a concurrent [`clear`](Self::clear) also mutates under the same lock,
    /// so a stale tile can't survive the clear.
    pub fn put_if(&self, key: K, value: Bytes, keep: impl FnOnce() -> bool) {
        let stripe = self.stripe(&key);
        let mut guard = stripe.lock().unpoison();
        if keep() {
            guard.put(key, value);
        }
    }

    pub fn clear(&self) {
        for stripe in &self.stripes {
            stripe.lock().unpoison().clear();
        }
    }
}

/// A reqwest client with the app User-Agent, the shared [`CONNECT_TIMEOUT`], and
/// the given per-request timeout. Servers needing extra knobs (pool size, HTTP
/// version) build their own with [`USER_AGENT`] and [`CONNECT_TIMEOUT`].
pub fn http_client(timeout: Duration) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(timeout)
        .build()
}

/// Why a [`send_with_retry`] call gave up. Callers map these to their own
/// domain: `NotFound` typically means "no such tile" (skip, don't retry),
/// `Throttled` means the server kept returning `429` past the retry budget,
/// `Forbidden` is a `403` (an expired/rejected credential — retrying won't help,
/// so it short-circuits and the caller may re-auth), and `Failed` covers
/// exhausted transient errors and other non-retryable responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchError {
    NotFound,
    Forbidden,
    Throttled,
    Failed,
}

/// Parse a `Retry-After` header expressed in delta-seconds (the form tile
/// servers use). HTTP-date values are not supported and yield `None`.
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

/// Send a request with the shared retry policy and return the successful
/// response for the caller to read. `build` is invoked once per attempt so
/// query parameters and headers are reapplied on retry. Transient network
/// errors and non-success responses back off exponentially; `404` short-circuits
/// to [`FetchError::NotFound`]; `429` honors `Retry-After` and, once the budget
/// is spent, resolves to [`FetchError::Throttled`].
pub async fn send_with_retry<F>(build: F) -> Result<reqwest::Response, FetchError>
where
    F: Fn() -> reqwest::RequestBuilder,
{
    let mut delay = INITIAL_RETRY_DELAY;
    for attempt in 0..=MAX_RETRIES {
        match build().send().await {
            Ok(resp) if resp.status().is_success() => return Ok(resp),
            Ok(resp) if resp.status() == reqwest::StatusCode::NOT_FOUND => {
                return Err(FetchError::NotFound);
            }
            // A 403 won't succeed on retry (expired/rejected credential); short-
            // circuit so the caller can re-authenticate instead of hammering.
            Ok(resp) if resp.status() == reqwest::StatusCode::FORBIDDEN => {
                return Err(FetchError::Forbidden);
            }
            Ok(resp) if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS => {
                if attempt == MAX_RETRIES {
                    return Err(FetchError::Throttled);
                }
                let wait = parse_retry_after(resp.headers()).unwrap_or(DEFAULT_RETRY_AFTER);
                tokio::time::sleep(wait).await;
            }
            _ if attempt < MAX_RETRIES => {
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(MAX_RETRY_DELAY);
            }
            _ => return Err(FetchError::Failed),
        }
    }
    Err(FetchError::Failed)
}

/// A running local tile server: its bound port and a graceful-shutdown signal.
pub struct ServerHandle {
    pub port: u16,
    shutdown_tx: watch::Sender<bool>,
}

impl ServerHandle {
    /// Signal the server to stop accepting connections and drain in-flight ones.
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }
}

/// Spawn `axum::serve` with graceful shutdown on the current tokio runtime and
/// return the shutdown sender. For servers that build their own handle type.
/// A silently-dead accept loop looks like a hang, so an unexpected exit is logged.
pub fn spawn_with_shutdown(listener: TcpListener, app: axum::Router) -> watch::Sender<bool> {
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.changed().await;
            })
            .await
        {
            tracing::error!("tile-server accept loop exited: {e}");
        }
    });
    shutdown_tx
}

/// Spawn a server on the current runtime and wrap it in the shared [`ServerHandle`].
pub fn spawn_server(listener: TcpListener, app: axum::Router) -> ServerHandle {
    let port = listener.local_addr().map_or(0, |a| a.port());
    let shutdown_tx = spawn_with_shutdown(listener, app);
    ServerHandle { port, shutdown_tx }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};

    #[test]
    fn parse_retry_after_reads_delta_seconds() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("12"));
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(12)));
    }

    #[test]
    fn parse_retry_after_ignores_http_dates_and_missing() {
        assert_eq!(parse_retry_after(&HeaderMap::new()), None);
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("Wed, 21 Oct 2025 07:28:00 GMT"));
        assert_eq!(parse_retry_after(&headers), None);
    }
}
