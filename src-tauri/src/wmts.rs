use quick_xml::Reader;
use quick_xml::events::Event;
use serde::Serialize;

const USER_AGENT: &str = "Turnout/0.1.0 (+https://github.com/SuperManifolds/Turnout)";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WmtsLayerInfo {
    pub identifier: String,
    pub title: String,
    pub tile_url: String,
}

pub async fn get_capabilities(base_url: &str) -> Result<Vec<WmtsLayerInfo>, String> {
    let sep = if base_url.contains('?') { '&' } else { '?' };
    let url = format!("{base_url}{sep}service=WMTS&request=GetCapabilities");

    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .header("User-Agent", USER_AGENT)
        .send()
        .await
        .map_err(|e| format!("Network error: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("WMTS server returned HTTP {}", resp.status()));
    }

    let xml = resp.text().await.map_err(|e| format!("Read error: {e}"))?;
    parse_capabilities(&xml, base_url)
}

fn parse_capabilities(xml: &str, base_url: &str) -> Result<Vec<WmtsLayerInfo>, String> {
    let mut reader = Reader::from_str(xml);
    let mut layers = Vec::new();
    let mut path: Vec<String> = Vec::new();
    let mut text_buf = String::new();

    let mut in_layer = false;
    let mut cur_identifier: Option<String> = None;
    let mut cur_title: Option<String> = None;
    let mut cur_template: Option<String> = None;
    let mut cur_format: Option<String> = None;
    let mut cur_tms: Option<String> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e) | Event::Empty(ref e)) => {
                let tag = String::from_utf8_lossy(e.local_name().as_ref()).to_string();

                if tag == "Layer" && path.last().map(String::as_str) == Some("Contents") {
                    in_layer = true;
                    cur_identifier = None;
                    cur_title = None;
                    cur_template = None;
                    cur_format = None;
                    cur_tms = None;
                } else if tag == "ResourceURL" && in_layer {
                    let attrs = xml_attrs(e);
                    if attrs.get("resourceType").map(String::as_str) == Some("tile") {
                        cur_template = attrs.get("template").cloned();
                        if cur_format.is_none() {
                            cur_format = attrs.get("format").cloned();
                        }
                    }
                } else if tag == "TileMatrixSet" && in_layer {
                    // Will capture text on End event
                }

                path.push(tag);
                text_buf.clear();
            }
            Ok(Event::End(ref e)) => {
                let tag = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                let text = text_buf.trim().to_string();
                let parent = path.iter().rev().nth(1).map(String::as_str);

                match tag.as_str() {
                    "Identifier" if parent == Some("Layer") && in_layer => {
                        cur_identifier = Some(text);
                    }
                    "Title" if parent == Some("Layer") && in_layer && cur_title.is_none() => {
                        cur_title = Some(text);
                    }
                    "TileMatrixSet" if in_layer && parent == Some("TileMatrixSetLink") => {
                        if cur_tms.is_none() {
                            cur_tms = Some(text);
                        }
                    }
                    "Format" if in_layer && cur_format.is_none() => {
                        cur_format = Some(text);
                    }
                    "Layer" if in_layer => {
                        if let Some(identifier) = cur_identifier.take() {
                            let title = cur_title.take().unwrap_or_else(|| identifier.clone());
                            let tile_url = build_tile_url(
                                base_url,
                                &identifier,
                                cur_template.as_deref(),
                                cur_tms.as_deref(),
                                cur_format.as_deref(),
                            );
                            layers.push(WmtsLayerInfo { identifier, title, tile_url });
                        }
                        in_layer = false;
                    }
                    _ => {}
                }

                path.pop();
                text_buf.clear();
            }
            Ok(Event::Text(ref e)) => {
                if let Ok(s) = e.unescape() {
                    text_buf.push_str(&s);
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }

    if layers.is_empty() {
        return Err("No layers found in WMTS capabilities".into());
    }

    Ok(layers)
}

fn build_tile_url(
    base_url: &str,
    layer: &str,
    template: Option<&str>,
    tms: Option<&str>,
    format: Option<&str>,
) -> String {
    if let Some(tpl) = template {
        return tpl
            .replace("{TileMatrix}", "{z}")
            .replace("{TileCol}", "{x}")
            .replace("{TileRow}", "{y}")
            .replace("{style}", "default")
            .replace("{Style}", "default");
    }

    let tms = tms.unwrap_or("GoogleMapsCompatible");
    let format = format.unwrap_or("image/png");
    let sep = if base_url.contains('?') { '&' } else { '?' };
    format!(
        "{base_url}{sep}service=WMTS&request=GetTile&version=1.0.0\
         &layer={layer}&style=default&tilematrixset={tms}\
         &tilematrix={{z}}&tilerow={{y}}&tilecol={{x}}&format={format}"
    )
}

fn xml_attrs(e: &quick_xml::events::BytesStart) -> std::collections::HashMap<String, String> {
    e.attributes()
        .filter_map(Result::ok)
        .map(|a| (
            String::from_utf8_lossy(a.key.as_ref()).to_string(),
            String::from_utf8_lossy(&a.value).to_string(),
        ))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_restful() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<Capabilities xmlns="http://www.opengis.net/wmts/1.0" xmlns:ows="http://www.opengis.net/ows/1.1">
  <Contents>
    <Layer>
      <ows:Title>Elevation</ows:Title>
      <ows:Identifier>elevation</ows:Identifier>
      <Style isDefault="true"><ows:Identifier>default</ows:Identifier></Style>
      <TileMatrixSetLink><TileMatrixSet>GoogleMapsCompatible</TileMatrixSet></TileMatrixSetLink>
      <ResourceURL format="image/png" resourceType="tile"
        template="https://tiles.example.com/elevation/{TileMatrix}/{TileCol}/{TileRow}.png"/>
    </Layer>
  </Contents>
</Capabilities>"#;
        let layers = parse_capabilities(xml, "https://example.com/wmts").expect("parse");
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].identifier, "elevation");
        assert_eq!(layers[0].title, "Elevation");
        assert_eq!(layers[0].tile_url, "https://tiles.example.com/elevation/{z}/{x}/{y}.png");
    }

    #[test]
    fn test_parse_kvp_fallback() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<Capabilities xmlns="http://www.opengis.net/wmts/1.0" xmlns:ows="http://www.opengis.net/ows/1.1">
  <Contents>
    <Layer>
      <ows:Title>Ortho</ows:Title>
      <ows:Identifier>ortho</ows:Identifier>
      <Format>image/jpeg</Format>
      <TileMatrixSetLink><TileMatrixSet>EPSG:3857</TileMatrixSet></TileMatrixSetLink>
    </Layer>
  </Contents>
</Capabilities>"#;
        let layers = parse_capabilities(xml, "https://maps.example.com/wmts").expect("parse");
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].identifier, "ortho");
        assert!(layers[0].tile_url.contains("service=WMTS"));
        assert!(layers[0].tile_url.contains("layer=ortho"));
        assert!(layers[0].tile_url.contains("format=image/jpeg"));
        assert!(layers[0].tile_url.contains("{z}"));
    }

    #[test]
    fn test_empty_capabilities() {
        let xml = r#"<?xml version="1.0"?><Capabilities><Contents></Contents></Capabilities>"#;
        assert!(parse_capabilities(xml, "https://example.com").is_err());
    }
}
