use anyhow::{Context, Result, bail};

const KML_COLOR_HEX_LENGTH: usize = 8;
use quick_xml::Reader;
use quick_xml::events::Event;
use std::collections::HashMap;
use std::io::{Read, Seek};

#[derive(Debug, Clone, Default)]
pub struct OverlayData {
    pub name: Option<String>,
    pub placemarks: Vec<Placemark>,
    pub ground_overlays: Vec<GroundOverlay>,
    pub styles: HashMap<String, Style>,
    pub images: HashMap<String, Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct Placemark {
    pub name: Option<String>,
    pub style_url: Option<String>,
    pub geometry: Geometry,
}

#[derive(Debug, Clone)]
pub enum Geometry {
    Point { lon: f64, lat: f64 },
    LineString { coords: Vec<(f64, f64)> },
    Polygon { outer: Vec<(f64, f64)>, inner: Vec<Vec<(f64, f64)>> },
    Multi(Vec<Geometry>),
}

#[derive(Debug, Clone)]
pub struct GroundOverlay {
    pub name: Option<String>,
    pub href: String,
    pub north: f64,
    pub south: f64,
    pub east: f64,
    pub west: f64,
    pub rotation: f64,
}

#[derive(Debug, Clone, Default)]
pub struct Style {
    pub line_color: Option<[u8; 4]>,
    pub line_width: Option<f32>,
    pub fill_color: Option<[u8; 4]>,
    pub poly_fill: Option<bool>,
    pub poly_outline: Option<bool>,
}

impl OverlayData {
    #[must_use]
    pub fn bbox(&self) -> Option<(f64, f64, f64, f64)> {
        let mut min_lon = f64::MAX;
        let mut min_lat = f64::MAX;
        let mut max_lon = f64::MIN;
        let mut max_lat = f64::MIN;
        let mut any = false;

        for pm in &self.placemarks {
            geometry_visit_coords(&pm.geometry, &mut |lon, lat| {
                min_lon = min_lon.min(lon);
                min_lat = min_lat.min(lat);
                max_lon = max_lon.max(lon);
                max_lat = max_lat.max(lat);
                any = true;
            });
        }
        for go in &self.ground_overlays {
            min_lon = min_lon.min(go.west);
            min_lat = min_lat.min(go.south);
            max_lon = max_lon.max(go.east);
            max_lat = max_lat.max(go.north);
            any = true;
        }

        any.then_some((min_lon, min_lat, max_lon, max_lat))
    }

    #[must_use]
    pub fn resolve_style(&self, pm: &Placemark) -> Style {
        pm.style_url
            .as_ref()
            .and_then(|url| {
                let id = url.strip_prefix('#').unwrap_or(url);
                self.styles.get(id)
            })
            .cloned()
            .unwrap_or_default()
    }
}

fn geometry_visit_coords(geom: &Geometry, f: &mut impl FnMut(f64, f64)) {
    match geom {
        Geometry::Point { lon, lat } => f(*lon, *lat),
        Geometry::LineString { coords } => {
            for &(lon, lat) in coords { f(lon, lat); }
        }
        Geometry::Polygon { outer, inner } => {
            for &(lon, lat) in outer { f(lon, lat); }
            for ring in inner {
                for &(lon, lat) in ring { f(lon, lat); }
            }
        }
        Geometry::Multi(geoms) => {
            for g in geoms { geometry_visit_coords(g, f); }
        }
    }
}

/// Parse a KMZ (zipped KML + images) or plain KML file.
pub fn parse_kmz(data: &[u8]) -> Result<OverlayData> {
    let cursor = std::io::Cursor::new(data);
    if let Ok(mut archive) = zip::ZipArchive::new(cursor) {
        parse_kmz_archive(&mut archive)
    } else {
        let xml = std::str::from_utf8(data).context("file is neither valid KMZ nor KML")?;
        parse_kml(xml, &HashMap::new())
    }
}

fn parse_kmz_archive<R: Read + Seek>(archive: &mut zip::ZipArchive<R>) -> Result<OverlayData> {
    let mut kml_xml = None;
    let mut images = HashMap::new();

    let image_exts: &[&str] = &["png", "jpg", "jpeg", "bmp", "gif"];

    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let name = file.name().to_string();
        let ext = std::path::Path::new(&name)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();

        if ext == "kml" && kml_xml.is_none() {
            let mut s = String::new();
            file.read_to_string(&mut s)?;
            kml_xml = Some(s);
        } else if image_exts.contains(&ext.as_str()) {
            let mut buf = Vec::new();
            file.read_to_end(&mut buf)?;
            images.insert(name, buf);
        }
    }

    let xml = kml_xml.context("KMZ archive contains no .kml file")?;
    parse_kml(&xml, &images)
}

fn parse_kml(xml: &str, images: &HashMap<String, Vec<u8>>) -> Result<OverlayData> {
    let mut reader = Reader::from_str(xml);
    let mut data = OverlayData {
        images: images.clone(),
        ..Default::default()
    };
    let mut path: Vec<String> = Vec::new();
    let mut text_buf = String::new();

    let mut cur_style_id: Option<String> = None;
    let mut cur_style = Style::default();
    let mut in_line_style = false;
    let mut in_poly_style = false;
    let mut in_icon_style = false;
    let mut in_style_map = false;
    let mut cur_pair_key: Option<String> = None;
    let mut style_map_refs: HashMap<String, String> = HashMap::new();

    let mut cur_pm_name: Option<String> = None;
    let mut cur_pm_style_url: Option<String> = None;
    let mut cur_pm_inline_style: Option<Style> = None;
    let mut cur_pm_geoms: Vec<Geometry> = Vec::new();
    let mut in_placemark = false;
    let mut in_outer = false;
    let mut in_inner = false;
    let mut inner_rings: Vec<Vec<(f64, f64)>> = Vec::new();

    let mut cur_go_name: Option<String> = None;
    let mut cur_go_href: Option<String> = None;
    let mut cur_go_north = 0.0_f64;
    let mut cur_go_south = 0.0_f64;
    let mut cur_go_east = 0.0_f64;
    let mut cur_go_west = 0.0_f64;
    let mut cur_go_rotation = 0.0_f64;
    let mut in_ground_overlay = false;
    let mut in_latlonbox = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => {
                let tag_bytes = e.local_name();
                let tag_bytes = tag_bytes.as_ref();

                if tag_bytes == b"Style" {
                    cur_style = Style::default();
                    cur_style_id = e.attributes().filter_map(Result::ok).find_map(|a| {
                        if a.key.as_ref() == b"id" {
                            Some(String::from_utf8_lossy(&a.value).to_string())
                        } else {
                            None
                        }
                    });
                } else if tag_bytes == b"StyleMap" {
                    in_style_map = true;
                    cur_style_id = e.attributes().filter_map(Result::ok).find_map(|a| {
                        if a.key.as_ref() == b"id" {
                            Some(String::from_utf8_lossy(&a.value).to_string())
                        } else {
                            None
                        }
                    });
                } else if tag_bytes == b"Pair" {
                    cur_pair_key = None;
                } else if tag_bytes == b"LineStyle" {
                    in_line_style = true;
                } else if tag_bytes == b"PolyStyle" {
                    in_poly_style = true;
                } else if tag_bytes == b"IconStyle" {
                    in_icon_style = true;
                } else if tag_bytes == b"Placemark" {
                    in_placemark = true;
                    cur_pm_name = None;
                    cur_pm_style_url = None;
                    cur_pm_inline_style = None;
                    cur_pm_geoms.clear();
                    inner_rings.clear();
                } else if tag_bytes == b"GroundOverlay" {
                    in_ground_overlay = true;
                    cur_go_name = None;
                    cur_go_href = None;
                    cur_go_north = 0.0;
                    cur_go_south = 0.0;
                    cur_go_east = 0.0;
                    cur_go_west = 0.0;
                    cur_go_rotation = 0.0;
                } else if tag_bytes == b"LatLonBox" && in_ground_overlay {
                    in_latlonbox = true;
                } else if tag_bytes == b"outerBoundaryIs" {
                    in_outer = true;
                } else if tag_bytes == b"innerBoundaryIs" {
                    in_inner = true;
                }

                path.push(String::from_utf8_lossy(tag_bytes).to_string());
                text_buf.clear();
            }
            Ok(Event::End(ref e)) => {
                let tag_bytes = e.local_name();
                let tag_bytes = tag_bytes.as_ref();
                let text = text_buf.trim().to_string();

                match tag_bytes {
                    b"Style" => {
                        if in_placemark {
                            cur_pm_inline_style = Some(cur_style.clone());
                        } else if let Some(id) = cur_style_id.take() {
                            data.styles.insert(id, cur_style.clone());
                        }
                        cur_style = Style::default();
                    }
                    b"StyleMap" => {
                        in_style_map = false;
                        cur_style_id = None;
                    }
                    b"Pair" => { cur_pair_key = None; }
                    b"key" if in_style_map => {
                        cur_pair_key = Some(text.clone());
                    }
                    b"LineStyle" => in_line_style = false,
                    b"PolyStyle" => in_poly_style = false,
                    b"IconStyle" => in_icon_style = false,
                    b"color" => {
                        if let Some(rgba) = parse_kml_color(&text) {
                            if in_line_style {
                                cur_style.line_color = Some(rgba);
                            } else if in_poly_style {
                                cur_style.fill_color = Some(rgba);
                            }
                        }
                    }
                    b"width" if in_line_style => {
                        if let Ok(w) = text.parse::<f32>() {
                            cur_style.line_width = Some(w);
                        }
                    }
                    b"fill" if in_poly_style => {
                        cur_style.poly_fill = Some(&text != "0");
                    }
                    b"outline" if in_poly_style => {
                        cur_style.poly_outline = Some(&text != "0");
                    }
                    b"name" => {
                        if in_placemark {
                            cur_pm_name = Some(text);
                        } else if in_ground_overlay {
                            cur_go_name = Some(text);
                        } else if data.name.is_none() {
                            data.name = Some(text);
                        }
                    }
                    b"styleUrl" if in_style_map => {
                        if cur_pair_key.as_deref() == Some("normal")
                            && let Some(ref map_id) = cur_style_id
                        {
                            let target = text.strip_prefix('#').unwrap_or(&text);
                            style_map_refs.insert(map_id.clone(), target.to_string());
                        }
                    }
                    b"styleUrl" if in_placemark => {
                        cur_pm_style_url = Some(text);
                    }
                    b"coordinates" => {
                        let coords = parse_coordinates(&text);
                        let parent = path.iter().rev().nth(1).map(String::as_str);
                        match parent {
                            Some("Point") if !coords.is_empty() => {
                                cur_pm_geoms.push(Geometry::Point {
                                    lon: coords[0].0,
                                    lat: coords[0].1,
                                });
                            }
                            Some("LineString") => {
                                cur_pm_geoms.push(Geometry::LineString { coords });
                            }
                            Some("LinearRing") if in_inner => {
                                inner_rings.push(coords);
                            }
                            Some("LinearRing") if in_outer => {
                                cur_pm_geoms.push(Geometry::Polygon {
                                    outer: coords,
                                    inner: Vec::new(),
                                });
                            }
                            _ => {}
                        }
                    }
                    b"innerBoundaryIs" => in_inner = false,
                    b"outerBoundaryIs" => in_outer = false,
                    b"Polygon" => {
                        if !inner_rings.is_empty()
                            && let Some(Geometry::Polygon { inner, .. }) =
                                cur_pm_geoms.last_mut()
                        {
                            *inner = std::mem::take(&mut inner_rings);
                        }
                    }
                    b"Placemark" => {
                        in_placemark = false;
                        let geometry = match cur_pm_geoms.len() {
                            0 => None,
                            1 => Some(cur_pm_geoms.remove(0)),
                            _ => Some(Geometry::Multi(std::mem::take(&mut cur_pm_geoms))),
                        };
                        if let Some(geometry) = geometry {
                            let style_url = if let Some(inline) = cur_pm_inline_style.take() {
                                let inline_id = format!("_inline_{}", data.placemarks.len());
                                data.styles.insert(inline_id.clone(), inline);
                                Some(format!("#{inline_id}"))
                            } else {
                                cur_pm_style_url.take()
                            };
                            data.placemarks.push(Placemark {
                                name: cur_pm_name.take(),
                                style_url,
                                geometry,
                            });
                        }
                    }
                    b"href" if in_ground_overlay && !in_icon_style => {
                        cur_go_href = Some(text);
                    }
                    b"north" if in_latlonbox => {
                        cur_go_north = text.parse().unwrap_or(0.0);
                    }
                    b"south" if in_latlonbox => {
                        cur_go_south = text.parse().unwrap_or(0.0);
                    }
                    b"east" if in_latlonbox => {
                        cur_go_east = text.parse().unwrap_or(0.0);
                    }
                    b"west" if in_latlonbox => {
                        cur_go_west = text.parse().unwrap_or(0.0);
                    }
                    b"rotation" if in_latlonbox => {
                        cur_go_rotation = text.parse().unwrap_or(0.0);
                    }
                    b"LatLonBox" => in_latlonbox = false,
                    b"GroundOverlay" => {
                        in_ground_overlay = false;
                        if let Some(href) = cur_go_href.take() {
                            data.ground_overlays.push(GroundOverlay {
                                name: cur_go_name.take(),
                                href,
                                north: cur_go_north,
                                south: cur_go_south,
                                east: cur_go_east,
                                west: cur_go_west,
                                rotation: cur_go_rotation,
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
            Ok(Event::Eof) => break,
            Err(e) => bail!("KML parse error at position {}: {e}", reader.error_position()),
            _ => {}
        }
    }

    for (map_id, style_id) in &style_map_refs {
        if let Some(style) = data.styles.get(style_id).cloned() {
            data.styles.insert(map_id.clone(), style);
        }
    }

    Ok(data)
}

fn parse_kml_color(s: &str) -> Option<[u8; 4]> {
    if s.len() != KML_COLOR_HEX_LENGTH {
        return None;
    }
    let bytes: Vec<u8> = (0..4)
        .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16))
        .collect::<Result<_, _>>()
        .ok()?;
    // KML is AABBGGRR → convert to RGBA
    Some([bytes[3], bytes[2], bytes[1], bytes[0]])
}

fn parse_coordinates(s: &str) -> Vec<(f64, f64)> {
    s.split_whitespace()
        .filter_map(|token| {
            let mut parts = token.split(',');
            let lon: f64 = parts.next()?.parse().ok()?;
            let lat: f64 = parts.next()?.parse().ok()?;
            Some((lon, lat))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_kml_color() {
        assert_eq!(parse_kml_color("ff0000ff"), Some([255, 0, 0, 255]));
        assert_eq!(parse_kml_color("8000ff00"), Some([0, 255, 0, 128]));
        assert_eq!(parse_kml_color("ffffff00"), Some([0, 255, 255, 255]));
        assert_eq!(parse_kml_color("short"), None);
    }

    #[test]
    fn test_parse_coordinates() {
        let coords = parse_coordinates("11.5,48.1,0 11.6,48.2,100");
        assert_eq!(coords.len(), 2);
        assert!((coords[0].0 - 11.5).abs() < 1e-10);
        assert!((coords[0].1 - 48.1).abs() < 1e-10);
        assert!((coords[1].0 - 11.6).abs() < 1e-10);
        assert!((coords[1].1 - 48.2).abs() < 1e-10);
    }

    #[test]
    fn test_parse_minimal_kml() {
        let kml = r#"<?xml version="1.0" encoding="UTF-8"?>
<kml xmlns="http://www.opengis.net/kml/2.2">
  <Document>
    <name>Test</name>
    <Style id="line1">
      <LineStyle><color>ff0000ff</color><width>3</width></LineStyle>
    </Style>
    <Placemark>
      <name>A Point</name>
      <styleUrl>#line1</styleUrl>
      <Point><coordinates>11.5,48.1,0</coordinates></Point>
    </Placemark>
    <Placemark>
      <name>A Line</name>
      <LineString><coordinates>11.5,48.1,0 11.6,48.2,0 11.7,48.15,0</coordinates></LineString>
    </Placemark>
    <GroundOverlay>
      <name>Image</name>
      <Icon><href>overlay.png</href></Icon>
      <LatLonBox>
        <north>48.5</north><south>48.4</south>
        <east>11.8</east><west>11.7</west>
        <rotation>0</rotation>
      </LatLonBox>
    </GroundOverlay>
  </Document>
</kml>"#;

        let data = parse_kml(kml, &HashMap::new()).expect("parse KML");
        assert_eq!(data.name.as_deref(), Some("Test"));
        assert_eq!(data.placemarks.len(), 2);
        assert_eq!(data.ground_overlays.len(), 1);
        assert!(data.styles.contains_key("line1"));

        let style = &data.styles["line1"];
        assert_eq!(style.line_color, Some([255, 0, 0, 255]));
        assert_eq!(style.line_width, Some(3.0));

        match &data.placemarks[0].geometry {
            Geometry::Point { lon, lat } => {
                assert!((lon - 11.5).abs() < 1e-10);
                assert!((lat - 48.1).abs() < 1e-10);
            }
            _ => panic!("expected Point"),
        }

        match &data.placemarks[1].geometry {
            Geometry::LineString { coords } => assert_eq!(coords.len(), 3),
            _ => panic!("expected LineString"),
        }

        let go = &data.ground_overlays[0];
        assert_eq!(go.href, "overlay.png");
        assert!((go.north - 48.5).abs() < 1e-10);
    }

    #[test]
    fn test_style_map_resolution() {
        let kml = r#"<?xml version="1.0" encoding="UTF-8"?>
<kml xmlns="http://www.opengis.net/kml/2.2">
  <Document>
    <Style id="s_blue"><LineStyle><color>ffff0000</color><width>4</width></LineStyle></Style>
    <Style id="s_blue_hl"><LineStyle><color>ffff0000</color><width>6</width></LineStyle></Style>
    <StyleMap id="m_blue">
      <Pair><key>normal</key><styleUrl>#s_blue</styleUrl></Pair>
      <Pair><key>highlight</key><styleUrl>#s_blue_hl</styleUrl></Pair>
    </StyleMap>
    <Placemark>
      <styleUrl>#m_blue</styleUrl>
      <LineString><coordinates>0,0 1,1</coordinates></LineString>
    </Placemark>
  </Document>
</kml>"#;
        let data = parse_kml(kml, &HashMap::new()).expect("parse");
        let style = data.resolve_style(&data.placemarks[0]);
        assert_eq!(style.line_color, Some([0, 0, 255, 255]));
        assert_eq!(style.line_width, Some(4.0));
    }

    #[test]
    fn test_inline_style() {
        let kml = r#"<?xml version="1.0" encoding="UTF-8"?>
<kml xmlns="http://www.opengis.net/kml/2.2">
  <Document>
    <Placemark>
      <Style><LineStyle><color>ff00ff00</color><width>5</width></LineStyle></Style>
      <LineString><coordinates>0,0 1,1</coordinates></LineString>
    </Placemark>
  </Document>
</kml>"#;
        let data = parse_kml(kml, &HashMap::new()).expect("parse");
        let style = data.resolve_style(&data.placemarks[0]);
        assert_eq!(style.line_color, Some([0, 255, 0, 255]));
        assert_eq!(style.line_width, Some(5.0));
    }

    #[test]
    fn test_bbox() {
        let data = OverlayData {
            placemarks: vec![Placemark {
                name: None,
                style_url: None,
                geometry: Geometry::Point { lon: 10.0, lat: 50.0 },
            }],
            ground_overlays: vec![GroundOverlay {
                name: None,
                href: String::new(),
                north: 51.0,
                south: 49.0,
                east: 12.0,
                west: 9.0,
                rotation: 0.0,
            }],
            ..Default::default()
        };
        let (w, s, e, n) = data.bbox().expect("has bbox");
        assert!((w - 9.0).abs() < 1e-10);
        assert!((s - 49.0).abs() < 1e-10);
        assert!((e - 12.0).abs() < 1e-10);
        assert!((n - 51.0).abs() < 1e-10);
    }

    #[test]
    fn test_bbox_empty() {
        let data = OverlayData::default();
        assert!(data.bbox().is_none());
    }

    #[test]
    fn test_malformed_kml() {
        let result = parse_kml("<not-kml>broken", &HashMap::new());
        assert!(result.is_ok());
        let data = result.expect("should parse without panic");
        assert!(data.placemarks.is_empty());
    }

    #[test]
    fn test_polygon_with_inner_rings() {
        let kml = r#"<?xml version="1.0" encoding="UTF-8"?>
<kml xmlns="http://www.opengis.net/kml/2.2">
  <Document>
    <Placemark>
      <Polygon>
        <outerBoundaryIs><LinearRing><coordinates>0,0 10,0 10,10 0,10 0,0</coordinates></LinearRing></outerBoundaryIs>
        <innerBoundaryIs><LinearRing><coordinates>2,2 8,2 8,8 2,8 2,2</coordinates></LinearRing></innerBoundaryIs>
      </Polygon>
    </Placemark>
  </Document>
</kml>"#;
        let data = parse_kml(kml, &HashMap::new()).expect("parse");
        assert_eq!(data.placemarks.len(), 1);
        match &data.placemarks[0].geometry {
            Geometry::Polygon { outer, inner } => {
                assert_eq!(outer.len(), 5);
                assert_eq!(inner.len(), 1);
                assert_eq!(inner[0].len(), 5);
            }
            _ => panic!("expected Polygon"),
        }
    }
}
