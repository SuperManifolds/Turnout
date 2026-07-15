use serde::{Deserialize, Serialize};

use crate::server_core::USER_AGENT;

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

    fn parse_and_filter(json: &str) -> Result<Vec<ArcGisServiceInfo>, String> {
        let body: ServicesResponse = serde_json::from_str(json).map_err(|e| e.to_string())?;
        let services: Vec<ArcGisServiceInfo> = body.services.into_iter()
            .filter(|s| s.service_type == "MapServer").collect();
        if services.is_empty() { return Err("No MapServer services found".into()); }
        Ok(services)
    }

    #[test]
    fn test_filters_to_map_server() {
        let json = r#"{"services":[
            {"name":"Elevation/USGS","type":"MapServer"},
            {"name":"Features/Points","type":"FeatureServer"},
            {"name":"Imagery/Basemap","type":"MapServer"}
        ]}"#;
        let services = parse_and_filter(json).expect("should have MapServers");
        assert_eq!(services.len(), 2);
        assert_eq!(services[0].name, "Elevation/USGS");
        assert_eq!(services[1].name, "Imagery/Basemap");
    }

    #[test]
    fn test_no_map_servers() {
        let json = r#"{"services":[
            {"name":"Features/Points","type":"FeatureServer"}
        ]}"#;
        assert!(parse_and_filter(json).is_err());
    }

    #[test]
    fn test_empty_services() {
        let json = r#"{"services":[]}"#;
        assert!(parse_and_filter(json).is_err());
    }
}
