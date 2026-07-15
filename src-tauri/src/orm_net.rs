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
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use maplibre_native::file_source::{
    ErrorReason, FileSourceType, ResourceRequest, Response as MlnResponse, TokioFileSource,
    register_tokio_file_source_with_handle,
};
use tokio::sync::{OnceCell, Semaphore};

use crate::server_core::UnpoisonExt;

/// Upper bound on concurrent upstream fetches across all render workers.
const MAX_CONCURRENT_FETCHES: usize = 32;
const CONNECT_TIMEOUT_SECS: u64 = 5;
const REQUEST_TIMEOUT_SECS: u64 = 30;
/// Fallback freshness when the upstream response carries no cache metadata, so
/// mbgl does not re-fetch unconditionally on every ambient-cache revalidation.
const DEFAULT_EXPIRES_SECS: u64 = 3600;
/// Backoff schedule for transient fetch failures (connection, 429, 5xx). One
/// entry per retry; coalesced waiters share the retries with the fetch.
const RETRY_BACKOFF_MS: &[u64] = &[200, 500, 1500];
const USER_AGENT: &str = concat!("turnout/", env!("CARGO_PKG_VERSION"));

struct OrmNetworkSource {
    client: reqwest::Client,
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
        let client = reqwest::Client::builder()
            .http1_only()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .pool_max_idle_per_host(MAX_CONCURRENT_FETCHES)
            .build()?;
        Ok(Self {
            client,
            limiter: Arc::new(Semaphore::new(MAX_CONCURRENT_FETCHES)),
            inflight: Mutex::new(HashMap::new()),
        })
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
        let mut builder = self.client.get(&request.url);
        if let Some(etag) = &request.prior_etag {
            builder = builder.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        let result = builder.send().await;
        drop(permit);

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
