//! Automatic Apple Maps tile authorization.
//!
//! Apple's map/satellite tiles require a short-lived `accessKey` plus a per-source
//! version (`v=`), both of which expire (~30 min). Rather than have the user paste
//! them by hand, this module reproduces what Apple's `MapKit` JS does on the web:
//!
//! 1. `GET https://duckduckgo.com/local.js?get_mk_token=1` → a `MapKit` JWT (`DuckDuckGo`
//!    hands this token to `MapKit` JS as the authorization token).
//! 2. `GET https://cdn.apple-mapkit.com/ma/bootstrap` with `Authorization: Bearer <jwt>`
//!    → JSON carrying the session `accessKey`, `expiresInSeconds`, and a `tileSources`
//!    list whose `standard` and `satellite` entries embed the `v=` version for each.
//!
//! The credentials are applied to every live Apple overlay and persisted to settings,
//! and a background task re-runs the flow shortly before each token expires so tiles
//! never fail on a stale key.

use std::time::Duration;

use serde::Deserialize;
use tauri::{Emitter, Manager};
use tauri_plugin_store::StoreExt;

use crate::overlay::apply_apple_credentials;
use crate::settings;

const TOKEN_URL: &str = "https://duckduckgo.com/local.js?get_mk_token=1";
const BOOTSTRAP_URL: &str =
    "https://cdn.apple-mapkit.com/ma/bootstrap?apiVersion=2&mkjsVersion=5.78.158&poi=1";
const ORIGIN: &str = "https://duckduckgo.com";
const REFERER: &str = "https://duckduckgo.com/";
/// A desktop-browser UA; the token endpoint gates on a browser-like agent.
const USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15";
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// Refresh this long before the reported expiry, absorbing clock skew and the round
/// trip so a request never goes out with an already-dead key.
const REFRESH_LEAD: Duration = Duration::from_mins(2);
/// Floor on the wait between refreshes, guarding against a tiny/zero `expiresInSeconds`.
const MIN_REFRESH_INTERVAL: Duration = Duration::from_mins(1);
/// First delay after a failed refresh; doubles each consecutive failure up to
/// `MAX_RETRY_INTERVAL`.
const BASE_RETRY_INTERVAL: Duration = Duration::from_secs(30);
const MAX_RETRY_INTERVAL: Duration = Duration::from_mins(5);
/// Assumed token lifetime when the bootstrap response omits `expiresInSeconds`.
const DEFAULT_TOKEN_LIFETIME_SECS: u64 = 1800;
/// Poll interval while auto-refresh is disabled (only reached if the wake signal
/// is missed).
const DISABLED_POLL_INTERVAL: Duration = Duration::from_mins(5);
/// Consecutive failures before the frontend is notified so the UI can surface it.
const FAILURE_NOTIFY_THRESHOLD: u32 = 3;
const SETTINGS_STORE: &str = "settings.json";
const REFRESHED_EVENT: &str = "apple-token-refreshed";
const FAILED_EVENT: &str = "apple-token-refresh-failed";

/// Shared coordination state for the refresher. `lock` serializes credential
/// application so a manual "Refresh now" and the background timer never write
/// concurrently; `wake` lets a settings change interrupt the loop's sleep (e.g.
/// re-enabling auto-refresh triggers an immediate fetch).
pub struct AppleRefresh {
    lock: tokio::sync::Mutex<()>,
    wake: tokio::sync::Notify,
}

impl AppleRefresh {
    pub fn new() -> Self {
        Self { lock: tokio::sync::Mutex::new(()), wake: tokio::sync::Notify::new() }
    }

    /// Wakes the refresh loop out of its sleep so it re-evaluates immediately.
    pub fn wake(&self) {
        self.wake.notify_one();
    }
}

impl Default for AppleRefresh {
    fn default() -> Self {
        Self::new()
    }
}

/// Credentials extracted from an Apple `MapKit` bootstrap response.
pub struct AppleCredentials {
    pub access_key: String,
    pub map_version: String,
    pub sat_version: String,
    pub expires_in: Duration,
}

#[derive(Deserialize)]
struct Bootstrap {
    #[serde(rename = "accessKey")]
    access_key: String,
    #[serde(rename = "expiresInSeconds")]
    expires_in_seconds: Option<u64>,
    #[serde(rename = "tileSources")]
    tile_sources: Vec<TileSource>,
}

#[derive(Deserialize)]
struct TileSource {
    #[serde(rename = "tileSource")]
    name: String,
    path: String,
}

/// Runs the token → bootstrap flow and returns the parsed credentials.
pub async fn fetch_credentials() -> Result<AppleCredentials, String> {
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|e| format!("http client: {e}"))?;

    let token = client
        .get(TOKEN_URL)
        .header(reqwest::header::REFERER, REFERER)
        .send()
        .await
        .map_err(|e| format!("token request: {e}"))?
        .error_for_status()
        .map_err(|e| format!("token status: {e}"))?
        .text()
        .await
        .map_err(|e| format!("token body: {e}"))?;
    let token = token.trim();
    if token.is_empty() {
        return Err("token endpoint returned an empty token".into());
    }

    let body = client
        .get(BOOTSTRAP_URL)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(reqwest::header::ORIGIN, ORIGIN)
        .send()
        .await
        .map_err(|e| format!("bootstrap request: {e}"))?
        .error_for_status()
        .map_err(|e| format!("bootstrap status: {e}"))?
        .text()
        .await
        .map_err(|e| format!("bootstrap body: {e}"))?;
    let bootstrap: Bootstrap =
        serde_json::from_str(&body).map_err(|e| format!("bootstrap parse: {e}"))?;

    let map_version = version_for(&bootstrap.tile_sources, "standard")
        .ok_or("bootstrap missing standard tile version")?;
    let sat_version = version_for(&bootstrap.tile_sources, "satellite")
        .ok_or("bootstrap missing satellite tile version")?;

    let expires_in_seconds = bootstrap.expires_in_seconds.unwrap_or_else(|| {
        eprintln!(
            "[apple] bootstrap omitted expiresInSeconds; assuming {DEFAULT_TOKEN_LIFETIME_SECS}s \
             — tiles may 403 if the real lifetime is shorter"
        );
        DEFAULT_TOKEN_LIFETIME_SECS
    });

    Ok(AppleCredentials {
        access_key: encode_access_key(&bootstrap.access_key),
        map_version,
        sat_version,
        expires_in: Duration::from_secs(expires_in_seconds),
    })
}

/// Percent-encodes the query-unsafe characters in Apple's access key. The key is a
/// base64 signature (`_`-delimited), so only `/`, `+`, and `=` need escaping —
/// leaving them raw corrupts the query string and Apple returns 403. This matches
/// the pre-encoded form the manual-paste path receives (copied from a tile URL).
fn encode_access_key(raw: &str) -> String {
    raw.replace('%', "%25")
        .replace('/', "%2F")
        .replace('+', "%2B")
        .replace('=', "%3D")
}

/// Extracts the `v=` value from the named tile source's URL template. Only a
/// numeric version is accepted — anything else is treated as absent so a
/// malformed or tampered `path` can never inject extra query parameters (or a `#`
/// fragment) into the tile URL the value is later interpolated into.
fn version_for(sources: &[TileSource], name: &str) -> Option<String> {
    let path = &sources.iter().find(|s| s.name == name)?.path;
    let version = parse_query_value(path, "v")?;
    (!version.is_empty() && version.bytes().all(|b| b.is_ascii_digit())).then_some(version)
}

/// Reads a query parameter value out of a URL or path (no percent-decoding needed
/// for the numeric version values Apple returns).
fn parse_query_value(url: &str, key: &str) -> Option<String> {
    let query = url.split('?').nth(1)?;
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| v.to_string())
    })
}

/// Applies credentials to live overlays and persists them to the settings store so
/// newly added Apple layers and the next launch pick them up, then signals the
/// frontend to reload. The store is the single source of truth for auto-managed
/// credentials (`set_settings` leaves the `apple_*` keys alone while auto-refresh
/// is on), so this is the only writer and manual saves cannot clobber it.
fn apply_and_persist(app: &tauri::AppHandle, creds: &AppleCredentials) -> Result<(), String> {
    apply_apple_credentials(
        app,
        &creds.access_key,
        Some(&creds.map_version),
        Some(&creds.sat_version),
    );

    let store = app.store(SETTINGS_STORE).map_err(|e| format!("open settings store: {e}"))?;
    store.set("apple_access_key", serde_json::json!(creds.access_key));
    store.set("apple_map_version", serde_json::json!(creds.map_version));
    store.set("apple_sat_version", serde_json::json!(creds.sat_version));
    store.save().map_err(|e| format!("save settings store: {e}"))?;

    // Signal only — the payload carries no access key (the frontend re-reads it
    // from the store), so the credential never crosses the IPC boundary.
    let _ = app.emit(REFRESHED_EVENT, ());
    Ok(())
}

/// Fetches fresh credentials and applies them, serialized against any other
/// in-flight refresh so the timer and a manual "Refresh now" never write at once.
/// Returns the token lifetime for scheduling the next run.
async fn refresh_once(app: &tauri::AppHandle) -> Result<Duration, String> {
    let state = app.state::<AppleRefresh>();
    let _guard = state.lock.lock().await;
    let creds = fetch_credentials().await?;
    let lifetime = creds.expires_in;
    apply_and_persist(app, &creds)?;
    Ok(lifetime)
}

/// Fetches fresh credentials on demand (the "Refresh now" button).
#[tauri::command]
pub async fn refresh_apple_token(app: tauri::AppHandle) -> Result<(), String> {
    refresh_once(&app).await.map(|_| ())
}

/// Spawns the background refresher: fetch, apply, then sleep until just before the
/// token expires and repeat. Consecutive failures back off exponentially and are
/// surfaced to the frontend once past a threshold. The loop honors the
/// `apple_auto_refresh` setting and can be woken early (e.g. when the setting is
/// toggled back on) instead of waiting out its sleep.
pub fn spawn_auto_refresh(app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut consecutive_failures: u32 = 0;
        loop {
            let wait = if settings::load(&app).apple_auto_refresh {
                match refresh_once(&app).await {
                    Ok(lifetime) => {
                        consecutive_failures = 0;
                        lifetime.saturating_sub(REFRESH_LEAD).max(MIN_REFRESH_INTERVAL)
                    }
                    Err(e) => {
                        consecutive_failures += 1;
                        eprintln!("[apple-token] refresh failed (attempt {consecutive_failures}): {e}");
                        if consecutive_failures >= FAILURE_NOTIFY_THRESHOLD {
                            let _ = app.emit(FAILED_EVENT, e);
                        }
                        backoff(consecutive_failures)
                    }
                }
            } else {
                DISABLED_POLL_INTERVAL
            };

            let state = app.state::<AppleRefresh>();
            let _ = tokio::time::timeout(wait, state.wake.notified()).await;
        }
    });
}

/// Exponential backoff for the Nth consecutive failure, capped.
fn backoff(failures: u32) -> Duration {
    let shift = failures.saturating_sub(1).min(5);
    (BASE_RETRY_INTERVAL * 2u32.pow(shift)).min(MAX_RETRY_INTERVAL)
}

#[cfg(test)]
mod tests {
    use super::*;

    const STANDARD_PATH: &str = "/ti/tile?style=0&size={{tileSizeIndex}}&x={{x}}&y={{y}}&z={{z}}&scale={{resolution}}&lang={{lang}}&v=2607052&poi={{poi}}&accessKey=abc";
    const SATELLITE_PATH: &str = "/tile?style=7&size={{tileSizeIndex}}&scale={{resolution}}&z={{z}}&x={{x}}&y={{y}}&v=10411&accessKey=abc";

    fn sources() -> Vec<TileSource> {
        vec![
            TileSource { name: "standard".into(), path: STANDARD_PATH.into() },
            TileSource { name: "hybrid-overlay".into(), path: "/ti/tile?style=46&v=2607052".into() },
            TileSource { name: "satellite".into(), path: SATELLITE_PATH.into() },
        ]
    }

    #[test]
    fn extracts_distinct_map_and_satellite_versions() {
        let s = sources();
        assert_eq!(version_for(&s, "standard").as_deref(), Some("2607052"));
        assert_eq!(version_for(&s, "satellite").as_deref(), Some("10411"));
    }

    #[test]
    fn missing_source_yields_none() {
        assert_eq!(version_for(&sources(), "terrain"), None);
    }

    #[test]
    fn rejects_non_numeric_version() {
        let s = vec![
            TileSource { name: "standard".into(), path: "/ti/tile?v=1&accessKey=evil".into() },
            TileSource { name: "satellite".into(), path: "/tile?v=10a#frag".into() },
            TileSource { name: "empty".into(), path: "/tile?v=".into() },
        ];
        // Extra params after the value are stripped at '&', so this stays clean.
        assert_eq!(version_for(&s, "standard").as_deref(), Some("1"));
        // Non-digit and empty versions are rejected outright.
        assert_eq!(version_for(&s, "satellite"), None);
        assert_eq!(version_for(&s, "empty"), None);
    }

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(backoff(1), BASE_RETRY_INTERVAL);
        assert_eq!(backoff(2), BASE_RETRY_INTERVAL * 2);
        assert_eq!(backoff(3), BASE_RETRY_INTERVAL * 4);
        assert_eq!(backoff(100), MAX_RETRY_INTERVAL);
    }

    #[test]
    fn encodes_query_unsafe_access_key_chars() {
        let raw = "1783358862_2373249116464053255_/_reCby2SN1rmytNcjUdwCJw/JNgPvpe47B3WmpS9+yRo=";
        let encoded = encode_access_key(raw);
        assert!(!encoded.contains('/') && !encoded.contains('+') && !encoded.contains('='));
        assert_eq!(
            encoded,
            "1783358862_2373249116464053255_%2F_reCby2SN1rmytNcjUdwCJw%2FJNgPvpe47B3WmpS9%2ByRo%3D"
        );
    }

    #[test]
    fn parse_query_value_handles_absent_and_present_keys() {
        assert_eq!(parse_query_value("/tile?a=1&v=42&b=2", "v").as_deref(), Some("42"));
        assert_eq!(parse_query_value("/tile?a=1", "v"), None);
        assert_eq!(parse_query_value("/tile", "v"), None);
    }

    #[test]
    fn parses_bootstrap_json() {
        let json = serde_json::json!({
            "accessKey": "1783358355_1303046746466142155_/_sig",
            "expiresInSeconds": 1800,
            "tileSources": [
                { "tileSource": "standard", "path": STANDARD_PATH },
                { "tileSource": "satellite", "path": SATELLITE_PATH },
            ]
        });
        let boot: Bootstrap = serde_json::from_value(json).expect("valid bootstrap json");
        assert_eq!(boot.access_key, "1783358355_1303046746466142155_/_sig");
        assert_eq!(boot.expires_in_seconds, Some(1800));
        assert_eq!(version_for(&boot.tile_sources, "standard").as_deref(), Some("2607052"));
        assert_eq!(version_for(&boot.tile_sources, "satellite").as_deref(), Some("10411"));
    }
}
