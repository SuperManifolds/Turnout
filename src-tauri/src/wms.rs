use quick_xml::Reader;
use quick_xml::events::Event;
use serde::Serialize;

const USER_AGENT: &str = "Turnout/0.1.0 (+https://github.com/SuperManifolds/Turnout)";

#[derive(Debug, Clone, Serialize)]
pub struct WmsLayerInfo {
    pub name: String,
    pub title: String,
}

pub async fn get_capabilities(base_url: &str) -> Result<Vec<WmsLayerInfo>, String> {
    let sep = if base_url.contains('?') { '&' } else { '?' };
    let url = format!("{base_url}{sep}service=WMS&request=GetCapabilities");

    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .header("User-Agent", USER_AGENT)
        .send()
        .await
        .map_err(|e| format!("Network error: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("WMS server returned HTTP {}", resp.status()));
    }

    let xml = resp.text().await.map_err(|e| format!("Read error: {e}"))?;
    parse_capabilities(&xml)
}

fn parse_capabilities(xml: &str) -> Result<Vec<WmsLayerInfo>, String> {
    let mut reader = Reader::from_str(xml);
    let mut layers = Vec::new();
    let mut path: Vec<String> = Vec::new();
    let mut text_buf = String::new();

    let mut layer_depth: Vec<(Option<String>, Option<String>)> = Vec::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => {
                let tag = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                if tag == "Layer" {
                    layer_depth.push((None, None));
                }
                path.push(tag);
                text_buf.clear();
            }
            Ok(Event::End(ref e)) => {
                let tag = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                let text = text_buf.trim().to_string();
                let parent = path.iter().rev().nth(1).map(String::as_str);

                match tag.as_str() {
                    "Name" if parent == Some("Layer") => {
                        if let Some(cur) = layer_depth.last_mut() {
                            cur.0 = Some(text);
                        }
                    }
                    "Title" if parent == Some("Layer") => {
                        if let Some(cur) = layer_depth.last_mut()
                            && cur.1.is_none()
                        {
                            cur.1 = Some(text);
                        }
                    }
                    "Layer" => {
                        if let Some((Some(name), title)) = layer_depth.pop() {
                            layers.push(WmsLayerInfo {
                                title: title.unwrap_or_else(|| name.clone()),
                                name,
                            });
                        }
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
        return Err("No layers found in WMS capabilities".into());
    }

    Ok(layers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_capabilities() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<WMS_Capabilities version="1.3.0">
  <Capability>
    <Layer>
      <Title>Root</Title>
      <Layer queryable="1">
        <Name>elevation</Name>
        <Title>Elevation Data</Title>
        <Style><Name>default</Name><Title>Default</Title></Style>
      </Layer>
      <Layer queryable="1">
        <Name>hillshade</Name>
        <Title>Hillshade Gray</Title>
        <Style><Name>raster</Name><Title>Raster</Title></Style>
      </Layer>
    </Layer>
  </Capability>
</WMS_Capabilities>"#;
        let layers = parse_capabilities(xml).expect("parse");
        assert_eq!(layers.len(), 2);
        assert_eq!(layers[0].name, "elevation");
        assert_eq!(layers[0].title, "Elevation Data");
        assert_eq!(layers[1].name, "hillshade");
        assert_eq!(layers[1].title, "Hillshade Gray");
    }

}
