const OVERPASS_URL: &str = "https://overpass-api.de/api/interpreter";
const USER_AGENT: &str = "Turnout/0.1.0 (+https://github.com/SuperManifolds/Turnout)";

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
