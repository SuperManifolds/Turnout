use crate::error::{CommandError, CommandResult};
use crate::server_core::USER_AGENT;

const OVERPASS_URL: &str = "https://overpass-api.de/api/interpreter";

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub async fn fetch_overpass(query: String) -> CommandResult<String> {
    let client = reqwest::Client::new();
    let resp = client
        .post(OVERPASS_URL)
        .header("User-Agent", USER_AGENT)
        .form(&[("data", query.as_str())])
        .send()
        .await
        .map_err(|e| CommandError::Network(format!("Network error: {e}")))?;

    match resp.status().as_u16() {
        200 => resp.text().await.map_err(|e| CommandError::Network(format!("Read error: {e}"))),
        429 => Err(CommandError::Network("Rate limited by Overpass API — wait a moment and try again".into())),
        504 => Err(CommandError::Network("Query timed out — try a smaller selection area".into())),
        status => Err(CommandError::Network(format!("Overpass API error (HTTP {status})"))),
    }
}
