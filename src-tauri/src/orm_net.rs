//! Shared network file source for the ORM render workers.
//!
//! Registered process-globally as mbgl's `Network` source so every renderer
//! fetches MVT tiles, glyphs, and sprites through one HTTP/1.1 connection pool
//! instead of six independent mbgl `OnlineFileSource` stacks. mbgl's resource
//! loader still consults the sqlite ambient cache first and forwards responses
//! into it, so cache behaviour is unchanged — only the network hop is shared.
//! Identical in-flight fetches are coalesced so neighbouring tile renders that
//! need the same vector tile trigger a single upstream request.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use maplibre_native::file_source::{
    ErrorReason, FileSourceType, ResourceRequest, Response as MlnResponse, TokioFileSource,
    register_tokio_file_source_with_handle,
};
use tokio::sync::{OnceCell, Semaphore};

use crate::server_core::{UnpoisonExt, CONNECT_TIMEOUT, USER_AGENT};

/// Upper bound on concurrent upstream fetches across all render workers. On a
/// cold pan into a new area the renderers request many tiles at once (several
/// vector sources per tile, plus glyphs/sprites); over HTTP/1.1 each in-flight
/// fetch needs its own pooled connection, so this bounds how many rounds a fresh
/// viewport takes. Matches the offline vector download, which streams 64
/// concurrent HTTP/1.1 fetches to the same host without trouble.
const MAX_CONCURRENT_FETCHES: usize = 64;
const REQUEST_TIMEOUT_SECS: u64 = 30;
/// Fallback freshness when the upstream response carries no cache metadata, so
/// mbgl does not re-fetch unconditionally on every ambient-cache revalidation.
const DEFAULT_EXPIRES_SECS: u64 = 3600;
/// Backoff schedule for transient fetch failures (connection, 429, 5xx). One
/// entry per retry; coalesced waiters share the retries with the fetch.
const RETRY_BACKOFF_MS: &[u64] = &[200, 500, 1500];

/// Per-request ceiling on the HTTP/2 client. Normal ORM fetches are well under a
/// second, so a request that runs this long is treated as an HTTP/2 stall — the
/// failure mode that made HTTP/2 unusable for dense ORM tiles. Kept well below
/// the HTTP/1.1 client's `REQUEST_TIMEOUT_SECS` so a hang surfaces quickly.
const H2_STALL_TIMEOUT_SECS: u64 = 10;
/// Consecutive HTTP/2 stalls before the source downgrades to HTTP/1.1 for the
/// rest of the session (one-way — no flapping back to HTTP/2).
const H2_STALL_DOWNGRADE: u32 = 3;

struct OrmNetworkSource {
    /// Preferred client: negotiates HTTP/2 over ALPN where the host offers it
    /// (the Cloudflare-fronted default does), multiplexing a cold viewport's
    /// fetches over one connection. Used until [`downgraded`](Self::downgraded).
    h2_client: reqwest::Client,
    /// Forced HTTP/1.1 client, used once HTTP/2 has stalled too many times —
    /// HTTP/2 caused permanent render hangs on dense ORM tiles on some setups.
    h1_client: reqwest::Client,
    /// One-way latch: set when HTTP/2 stalls repeatedly, after which every fetch
    /// uses `h1_client` for the rest of the session.
    downgraded: AtomicBool,
    /// Consecutive HTTP/2 stalls; any fast success resets it to zero.
    consecutive_stalls: AtomicU32,
    limiter: Arc<Semaphore>,
    /// In-flight coalescing map: URL → shared response cell. Only requests
    /// without cache validators join a cell; conditional re-validations are
    /// answered individually so a `304 Not Modified` never reaches a caller
    /// that has no cached body.
    inflight: Mutex<HashMap<String, Arc<OnceCell<MlnResponse>>>>,
}

impl TokioFileSource for OrmNetworkSource {
    fn can_request(&self, request: &ResourceRequest) -> bool {
        request.url.starts_with("http://") || request.url.starts_with("https://")
    }

    async fn request(&self, request: ResourceRequest) -> MlnResponse {
        if request.prior_etag.is_some() || request.prior_modified.is_some() {
            return self.fetch(&request).await;
        }
        let cell = {
            let mut inflight = self.inflight.lock().unpoison();
            Arc::clone(inflight.entry(request.url.clone()).or_default())
        };
        let response = cell.get_or_init(|| self.fetch(&request)).await.clone();
        let mut inflight = self.inflight.lock().unpoison();
        if inflight.get(&request.url).is_some_and(|c| Arc::ptr_eq(c, &cell)) {
            inflight.remove(&request.url);
        }
        response
    }
}

impl OrmNetworkSource {
    fn new() -> Result<Self, reqwest::Error> {
        let base = || {
            reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .connect_timeout(CONNECT_TIMEOUT)
                .pool_max_idle_per_host(MAX_CONCURRENT_FETCHES)
        };
        // Default builder negotiates HTTP/2 via ALPN (and transparently uses
        // HTTP/1.1 against a host that doesn't offer h2), so a cold viewport
        // multiplexes over one connection. The shorter timeout surfaces a stall.
        let h2_client = base().timeout(Duration::from_secs(H2_STALL_TIMEOUT_SECS)).build()?;
        let h1_client = base()
            .http1_only()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()?;
        Ok(Self {
            h2_client,
            h1_client,
            downgraded: AtomicBool::new(false),
            consecutive_stalls: AtomicU32::new(0),
            limiter: Arc::new(Semaphore::new(MAX_CONCURRENT_FETCHES)),
            inflight: Mutex::new(HashMap::new()),
        })
    }

    /// Records an HTTP/2 fetch outcome; after [`H2_STALL_DOWNGRADE`] consecutive
    /// stalls it latches [`downgraded`](Self::downgraded), so the rest of the
    /// session runs on HTTP/1.1. Any fast success resets the streak.
    fn note_h2_health(&self, stalled: bool) {
        if !stalled {
            self.consecutive_stalls.store(0, Ordering::Relaxed);
            return;
        }
        let streak = self.consecutive_stalls.fetch_add(1, Ordering::Relaxed) + 1;
        if streak >= H2_STALL_DOWNGRADE && !self.downgraded.swap(true, Ordering::Relaxed) {
            tracing::warn!("ORM HTTP/2 stalled {streak}x; downgrading this session to HTTP/1.1");
        }
    }

    /// Fetches with retries on transient failures. mbgl's built-in
    /// `OnlineFileSource` (which this source replaces) retried failed requests
    /// with backoff; without that, a single connection blip or 429 on any of a
    /// tile's composite sources fails the whole tile render — in Tile mode
    /// mbgl aborts the render, and the client shows a blank tile.
    async fn fetch(&self, request: &ResourceRequest) -> MlnResponse {
        let mut response = self.fetch_once(request).await;
        for delay_ms in RETRY_BACKOFF_MS {
            if !is_transient(&response) {
                return response;
            }
            tokio::time::sleep(Duration::from_millis(*delay_ms)).await;
            response = self.fetch_once(request).await;
        }
        response
    }

    async fn fetch_once(&self, request: &ResourceRequest) -> MlnResponse {
        let Ok(permit) = Arc::clone(&self.limiter).acquire_owned().await else {
            return MlnResponse::error(ErrorReason::Other, "fetch limiter closed");
        };
        let on_h2 = !self.downgraded.load(Ordering::Relaxed);
        let client = if on_h2 { &self.h2_client } else { &self.h1_client };
        let mut builder = client.get(&request.url);
        if let Some(etag) = &request.prior_etag {
            builder = builder.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        let result = builder.send().await;
        drop(permit);

        // While still on HTTP/2, watch for the stall failure mode (a transport
        // timeout or connection error — the h2 client's short timeout turns a hang
        // into one) and downgrade the source after enough consecutive stalls.
        if on_h2 {
            let stalled = result.as_ref().err().is_some_and(|e| e.is_timeout() || e.is_connect());
            self.note_h2_health(stalled);
        }

        let response = match result {
            Ok(response) => response,
            Err(e) => {
                let reason = if e.is_timeout() || e.is_connect() {
                    ErrorReason::Connection
                } else {
                    ErrorReason::Other
                };
                return MlnResponse::error(reason, e.to_string());
            }
        };
        into_mln_response(request, response).await
    }
}

/// Whether a response represents a failure worth retrying: rate limiting,
/// server errors, and transport failures. Not-found and other client errors
/// are final.
fn is_transient(response: &MlnResponse) -> bool {
    response.error.as_ref().is_some_and(|e| {
        matches!(
            e.reason,
            ErrorReason::Connection | ErrorReason::RateLimit | ErrorReason::Server
        )
    })
}

async fn into_mln_response(
    request: &ResourceRequest,
    response: reqwest::Response,
) -> MlnResponse {
    let status = response.status();
    match status.as_u16() {
        200..=203 | 205..=299 => {
            let (etag, expires, must_revalidate) = cache_metadata(response.headers());
            match response.bytes().await {
                Ok(body) => {
                    let mut mln = MlnResponse::data(body.to_vec())
                        .with_expires(expires)
                        .with_must_revalidate(must_revalidate);
                    if let Some(etag) = etag {
                        mln = mln.with_etag(etag);
                    }
                    mln
                }
                Err(e) => MlnResponse::error(ErrorReason::Connection, e.to_string()),
            }
        }
        204 => MlnResponse::no_content(),
        304 => {
            let (etag, expires, must_revalidate) = cache_metadata(response.headers());
            // Mirrors mbgl's OnlineFileSource contract: `prior_data` present means
            // the requester is still waiting for a body (the cached copy was stale),
            // so the cached bytes are replayed as a plain data response; without it
            // the requester already has the body and a bare not-modified suffices.
            let mut mln = match request.prior_data.clone() {
                Some(data) => MlnResponse::data(data),
                None => MlnResponse::not_modified(),
            };
            mln = mln.with_expires(expires).with_must_revalidate(must_revalidate);
            mln.modified = request.prior_modified;
            match etag.or_else(|| request.prior_etag.clone()) {
                Some(etag) => mln.with_etag(etag),
                None => mln,
            }
        }
        // mbgl convention: a missing tile is an empty tile, not an error.
        404 if request.tile.is_some() => MlnResponse::no_content(),
        404 => MlnResponse::error(ErrorReason::NotFound, format!("404 for {}", request.url)),
        429 => {
            let mln = MlnResponse::error(ErrorReason::RateLimit, "HTTP 429");
            match retry_after(response.headers()) {
                Some(after) => mln.with_retry_after(SystemTime::now() + after),
                None => mln,
            }
        }
        500..=599 => MlnResponse::error(ErrorReason::Server, format!("HTTP {status}")),
        _ => MlnResponse::error(ErrorReason::Other, format!("HTTP {status}")),
    }
}

/// Extracts (`ETag`, `Expires`, must-revalidate) for mbgl's ambient cache. The
/// expiry comes from `Cache-Control: max-age`, falling back to a fixed window
/// so uncached upstream responses still get a sane revalidation interval.
fn cache_metadata(headers: &reqwest::header::HeaderMap) -> (Option<String>, SystemTime, bool) {
    let etag = headers
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let cache_control = headers
        .get(reqwest::header::CACHE_CONTROL)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let max_age = cache_control
        .split(',')
        .filter_map(|d| d.trim().strip_prefix("max-age="))
        .find_map(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_EXPIRES_SECS);
    let must_revalidate =
        cache_control.contains("must-revalidate") || cache_control.contains("no-cache");
    (etag, SystemTime::now() + Duration::from_secs(max_age), must_revalidate)
}

fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

/// Registers the shared source as the process-global mbgl `Network` file source.
/// Must run before any renderer is constructed; `handle`'s runtime must outlive
/// every renderer.
pub(crate) fn register(handle: tokio::runtime::Handle) -> Result<(), reqwest::Error> {
    register_tokio_file_source_with_handle(
        FileSourceType::Network,
        handle,
        OrmNetworkSource::new()?,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn h2_downgrades_after_consecutive_stalls_and_resets_on_success() {
        let src = OrmNetworkSource::new().expect("client build");
        assert!(!src.downgraded.load(Ordering::Relaxed));

        // Stalls interrupted by a fast success never reach the threshold.
        src.note_h2_health(true);
        src.note_h2_health(true);
        src.note_h2_health(false);
        assert!(!src.downgraded.load(Ordering::Relaxed), "success should reset the streak");

        // A run of consecutive stalls latches the one-way downgrade.
        for _ in 0..H2_STALL_DOWNGRADE {
            src.note_h2_health(true);
        }
        assert!(src.downgraded.load(Ordering::Relaxed));

        // A later success does not flap back to HTTP/2.
        src.note_h2_health(false);
        assert!(src.downgraded.load(Ordering::Relaxed), "downgrade is permanent for the session");
    }
}
