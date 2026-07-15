use std::fmt;
use std::time::Duration;

const OVERPASS_URL: &str = "https://overpass-api.de/api/interpreter";
const USER_AGENT: &str = "Turnout/0.1.0 (+https://github.com/SuperManifolds/Turnout)";

/// Railway `railway=*` values fetched for the vertical-layers tiler. The lifecycle
/// values — `construction`/`proposed` (with the eventual type in the same-named
/// tag) and `preserved` (operational heritage track) — are resolved to their target
/// type by the tiler.
const RAILWAY_TYPES: &[&str] = &[
    "rail", "tram", "subway", "light_rail", "narrow_gauge", "monorail", "funicular",
    "construction", "proposed", "preserved",
];
/// Reject selections beyond this many km² up front: larger areas overwhelm the
/// public Overpass servers (timeouts / fair-use limits). ~200 km on a side.
const MAX_AREA_KM2: f64 = 40_000.0;
const MAX_RETRIES: u32 = 3;
const RETRY_BASE: Duration = Duration::from_secs(2);
const RETRY_MAX: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Client read-timeout headroom above the server-side `[timeout:]`.
const READ_TIMEOUT_HEADROOM: Duration = Duration::from_secs(30);

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub async fn fetch_overpass(query: String) -> Result<String, String> {
    let client = reqwest::Client::new();
    let resp = client
        .post(OVERPASS_URL)
        .header("User-Agent", USER_AGENT)
        .form(&[("data", query.as_str())])
        .send()
        .await
        .map_err(|e| format!("Network error: {e}"))?;

    match resp.status().as_u16() {
        200 => resp.text().await.map_err(|e| format!("Read error: {e}")),
        429 => Err("Rate limited by Overpass API — wait a moment and try again".into()),
        504 => Err("Query timed out — try a smaller selection area".into()),
        status => Err(format!("Overpass API error (HTTP {status})")),
    }
}

/// A classified Overpass failure with an actionable, user-facing message.
#[derive(Debug, PartialEq, Eq)]
pub enum OverpassError {
    AreaTooLarge { area_km2: u32, max_km2: u32 },
    RateLimited,
    Timeout,
    ServerError(u16),
    BadRequest(String),
    Network(String),
    /// Query succeeded but returned no railway features.
    Empty,
}

impl fmt::Display for OverpassError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AreaTooLarge { area_km2, max_km2 } => write!(
                f,
                "Selected area is too large (~{area_km2} km²; limit {max_km2} km²). Zoom in or pick a smaller region."
            ),
            Self::RateLimited => {
                write!(f, "Overpass is rate-limiting requests. Wait a minute and try again.")
            }
            Self::Timeout => write!(f, "The Overpass query timed out — try a smaller area."),
            Self::ServerError(s) => {
                write!(f, "Overpass server error (HTTP {s}). Please try again shortly.")
            }
            Self::BadRequest(m) => write!(f, "Overpass rejected the request: {m}"),
            Self::Network(m) => write!(f, "Could not reach Overpass: {m}"),
            Self::Empty => write!(f, "No railway data found in the selected area."),
        }
    }
}

impl std::error::Error for OverpassError {}

/// Approximate area of a lat/lon bbox in km² (latitude-corrected).
fn bbox_area_km2(south: f64, west: f64, north: f64, east: f64) -> f64 {
    const KM_PER_DEG_LAT: f64 = 110.574;
    const KM_PER_DEG_LON_EQ: f64 = 111.320;
    let mid_lat = (south + north) / 2.0;
    let width = (east - west).abs() * KM_PER_DEG_LON_EQ * mid_lat.to_radians().cos().abs();
    let height = (north - south).abs() * KM_PER_DEG_LAT;
    width * height
}

fn build_railway_query(south: f64, west: f64, north: f64, east: f64, timeout_secs: u32) -> String {
    let types = RAILWAY_TYPES.join("|");
    let bbox = format!("{south},{west},{north},{east}");
    // `out geom;` inlines coordinates and returns all tags (incl. `layer`),
    // avoiding a separate node fetch + assembly. The union also pulls platforms
    // (both the classic `railway=platform` and the newer `public_transport`
    // tagging) so they can be rendered alongside the tracks.
    format!(
        "[out:json][timeout:{timeout_secs}];\
         (way[\"railway\"~\"^({types})$\"]({bbox});\
         way[\"railway\"=\"platform\"]({bbox});\
         way[\"public_transport\"=\"platform\"]({bbox}););\
         out geom;"
    )
}

fn is_transient(err: &OverpassError) -> bool {
    matches!(
        err,
        OverpassError::RateLimited
            | OverpassError::Timeout
            | OverpassError::ServerError(_)
            | OverpassError::Network(_)
    )
}

fn backoff_delay(attempt: u32, retry_after: Option<Duration>) -> Duration {
    retry_after
        .unwrap_or_else(|| RETRY_BASE.saturating_mul(2u32.saturating_pow(attempt)))
        .min(RETRY_MAX)
}

/// Pull an error `remark` out of an Overpass JSON body. Overpass returns some
/// runtime failures (e.g. exceeding maxsize/timeout) as HTTP 200 with a
/// top-level `remark` rather than an error status.
fn error_remark(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let remark = value.get("remark")?.as_str()?;
    let low = remark.to_lowercase();
    (low.contains("error") || low.contains("timed out") || low.contains("timeout"))
        .then(|| remark.to_string())
}

/// Map an HTTP status (+ body for 200) to a classified result.
fn classify(status: u16, body: &str) -> Result<(), OverpassError> {
    match status {
        200 => match error_remark(body) {
            // Resource-exhaustion remarks read as "too big" → advise a smaller area.
            Some(_) => Err(OverpassError::Timeout),
            None => Ok(()),
        },
        400 => Err(OverpassError::BadRequest(body.lines().next().unwrap_or("bad query").trim().to_string())),
        429 => Err(OverpassError::RateLimited),
        504 => Err(OverpassError::Timeout),
        s if (500..600).contains(&s) => Err(OverpassError::ServerError(s)),
        s => Err(OverpassError::BadRequest(format!("HTTP {s}"))),
    }
}

fn parse_retry_after(resp: &reqwest::Response) -> Option<Duration> {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

/// Fetch every railway way in the bbox from Overpass, with an up-front size
/// guard, bounded retries with exponential backoff (honoring `Retry-After`),
/// and classified, user-actionable errors. Returns the raw `[out:json]` body.
pub async fn fetch_railways(
    south: f64,
    west: f64,
    north: f64,
    east: f64,
    timeout_secs: u32,
) -> Result<String, OverpassError> {
    let area = bbox_area_km2(south, west, north, east);
    if area > MAX_AREA_KM2 {
        return Err(OverpassError::AreaTooLarge {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            area_km2: area as u32,
            max_km2: MAX_AREA_KM2 as u32,
        });
    }

    let query = build_railway_query(south, west, north, east, timeout_secs);
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(Duration::from_secs(u64::from(timeout_secs)) + READ_TIMEOUT_HEADROOM)
        .build()
        .map_err(|e| OverpassError::Network(e.to_string()))?;

    let mut attempt = 0;
    loop {
        let (result, retry_after) = attempt_fetch(&client, &query).await;
        match result {
            Ok(body) => return Ok(body),
            Err(err) if is_transient(&err) && attempt < MAX_RETRIES => {
                tokio::time::sleep(backoff_delay(attempt, retry_after)).await;
                attempt += 1;
            }
            Err(err) => return Err(err),
        }
    }
}

/// One Overpass attempt. Returns the classified result plus any `Retry-After`.
async fn attempt_fetch(
    client: &reqwest::Client,
    query: &str,
) -> (Result<String, OverpassError>, Option<Duration>) {
    let resp = match client.post(OVERPASS_URL).form(&[("data", query)]).send().await {
        Ok(resp) => resp,
        Err(e) => {
            let err = if e.is_timeout() { OverpassError::Timeout } else { OverpassError::Network(e.to_string()) };
            return (Err(err), None);
        }
    };

    let status = resp.status().as_u16();
    let retry_after = parse_retry_after(&resp);
    let body = match resp.text().await {
        Ok(b) => b,
        Err(e) => return (Err(OverpassError::Network(e.to_string())), retry_after),
    };

    match classify(status, &body) {
        Ok(()) => (Ok(body), retry_after),
        Err(err) => (Err(err), retry_after),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn area_guard_matches_bbox_size() {
        // ~55 km (lat) x ~27 km (lon at 52.5°) ≈ 1500 km²; well under the cap.
        let a = bbox_area_km2(52.3, 13.2, 52.8, 13.6);
        assert!((1000.0..2500.0).contains(&a), "unexpected area {a}");
    }

    #[test]
    fn query_has_all_types_bbox_and_geometry() {
        let q = build_railway_query(52.3, 13.2, 52.8, 13.6, 90);
        assert!(q.contains("out geom;"));
        assert!(q.contains("timeout:90"));
        assert!(q.contains("(52.3,13.2,52.8,13.6)"));
        for t in RAILWAY_TYPES {
            assert!(q.contains(t), "missing type {t}");
        }
        assert!(q.contains("\"railway\"=\"platform\""), "missing railway platforms");
        assert!(q.contains("\"public_transport\"=\"platform\""), "missing PT platforms");
    }

    #[test]
    fn transient_errors_retry_others_dont() {
        assert!(is_transient(&OverpassError::RateLimited));
        assert!(is_transient(&OverpassError::Timeout));
        assert!(is_transient(&OverpassError::ServerError(503)));
        assert!(is_transient(&OverpassError::Network("x".into())));
        assert!(!is_transient(&OverpassError::BadRequest("x".into())));
        assert!(!is_transient(&OverpassError::AreaTooLarge { area_km2: 1, max_km2: 1 }));
        assert!(!is_transient(&OverpassError::Empty));
    }

    #[test]
    fn backoff_is_exponential_capped_and_honors_retry_after() {
        assert_eq!(backoff_delay(0, None), RETRY_BASE);
        assert_eq!(backoff_delay(1, None), RETRY_BASE * 2);
        assert_eq!(backoff_delay(2, None), RETRY_BASE * 4);
        assert_eq!(backoff_delay(10, None), RETRY_MAX); // capped
        assert_eq!(backoff_delay(0, Some(Duration::from_secs(5))), Duration::from_secs(5));
        assert_eq!(backoff_delay(0, Some(Duration::from_secs(999))), RETRY_MAX);
    }

    #[test]
    fn status_classification() {
        assert!(classify(200, r#"{"elements":[]}"#).is_ok());
        assert_eq!(classify(200, r#"{"remark":"runtime error: Query timed out"}"#), Err(OverpassError::Timeout));
        assert_eq!(classify(429, ""), Err(OverpassError::RateLimited));
        assert_eq!(classify(504, ""), Err(OverpassError::Timeout));
        assert_eq!(classify(503, ""), Err(OverpassError::ServerError(503)));
        assert!(matches!(classify(400, "line1\nline2"), Err(OverpassError::BadRequest(_))));
    }

    #[test]
    fn remark_only_flags_errors() {
        assert!(error_remark(r#"{"remark":"runtime error: out of memory"}"#).is_some());
        assert!(error_remark(r#"{"elements":[]}"#).is_none());
        assert!(error_remark("not json").is_none());
    }
}
