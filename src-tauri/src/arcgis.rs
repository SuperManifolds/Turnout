use serde::{Deserialize, Serialize};

const USER_AGENT: &str = "Turnout/0.1.0 (+https://github.com/SuperManifolds/Turnout)";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArcGisServiceInfo {
    pub name: String,
    #[serde(rename = "type")]
    pub service_type: String,
}

#[derive(Deserialize)]
struct ServicesResponse {
    #[serde(default)]
    services: Vec<ArcGisServiceInfo>,
}

pub async fn list_services(base_url: &str) -> Result<Vec<ArcGisServiceInfo>, String> {
    let client = reqwest::Client::new();
    let resp = client
        .get(base_url)
        .query(&[("f", "json")])
        .header("User-Agent", USER_AGENT)
        .send()
        .await
        .map_err(|e| format!("Network error: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("ArcGIS server returned HTTP {}", resp.status()));
    }

    let text = resp.text().await.map_err(|e| format!("Read error: {e}"))?;
    let body: ServicesResponse =
        serde_json::from_str(&text).map_err(|e| format!("Failed to parse response: {e}"))?;

    let services: Vec<ArcGisServiceInfo> = body
        .services
        .into_iter()
        .filter(|s| s.service_type == "MapServer")
        .collect();

    if services.is_empty() {
        return Err("No MapServer services found".into());
    }

    Ok(services)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_services() {
        let json = r#"{"services":[
            {"name":"Elevation/USGS","type":"MapServer"},
            {"name":"Features/Points","type":"FeatureServer"},
            {"name":"Imagery/Basemap","type":"MapServer"}
        ]}"#;
        let resp: ServicesResponse = serde_json::from_str(json).expect("parse");
        let filtered: Vec<_> = resp.services.into_iter().filter(|s| s.service_type == "MapServer").collect();
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].name, "Elevation/USGS");
        assert_eq!(filtered[1].name, "Imagery/Basemap");
    }
}
