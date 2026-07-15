//! Shared scaffolding for the app's local HTTP tile servers: port bind + retry,
//! graceful shutdown, an LRU-cache helper, a reqwest factory, and the
//! poison-recovery lock extension. Each server keeps its own request-handling
//! (raster compositing, MVT encoding, native rendering, resampling) and layers it
//! on top of these primitives.

use std::hash::Hash;
use std::num::NonZeroUsize;
use std::sync::{Mutex, MutexGuard, PoisonError, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration;

use lru::LruCache;
use tokio::net::TcpListener;
use tokio::sync::watch;

/// User-Agent for every outbound HTTP request the app makes.
pub const USER_AGENT: &str =
    concat!("Turnout/", env!("CARGO_PKG_VERSION"), " (+https://github.com/SuperManifolds/Turnout)");

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

/// A reqwest client with the app User-Agent and a single request timeout. Servers
/// needing extra knobs (connect timeout, pool size) build their own with
/// [`USER_AGENT`].
pub fn http_client(timeout: Duration) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder().user_agent(USER_AGENT).timeout(timeout).build()
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
            eprintln!("[tile-server] accept loop exited: {e}");
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
