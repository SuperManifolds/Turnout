use anyhow::{Result, bail};
use std::collections::HashMap;

use crate::kml::{Geometry, KmzData, Placemark, Style};

const DEFAULT_LINE_COLOR: [u8; 4] = [51, 136, 255, 200];
const DEFAULT_FILL_COLOR: [u8; 4] = [51, 136, 255, 80];
const DEFAULT_POINT_COLOR: [u8; 4] = [255, 85, 0, 220];
const DEFAULT_LINE_WIDTH: f32 = 2.0;

pub fn parse_geojson(json: &str) -> Result<KmzData> {
    let root: serde_json::Value = serde_json::from_str(json)?;
    let features = extract_features(&root)?;

    let mut placemarks = Vec::new();
    let mut styles: HashMap<String, Style> = HashMap::new();

    for (i, feature) in features.iter().enumerate() {
        let geometry = parse_geometry(feature.get("geometry").unwrap_or(&serde_json::Value::Null));
        let Some(geometry) = geometry else { continue };

        let props = feature.get("properties").unwrap_or(&serde_json::Value::Null);
        let name = props["name"]
            .as_str()
            .or_else(|| props["NAME"].as_str())
            .or_else(|| props["title"].as_str())
            .map(String::from);

        let style = extract_style(feature);
        let style_id = if style.line_color.is_some() || style.fill_color.is_some() {
            let id = format!("_geojson_{i}");
            styles.insert(id.clone(), style);
            id
        } else {
            let default_id = default_style_id(&geometry);
            if !styles.contains_key(&default_id) {
                styles.insert(default_id.clone(), default_style_for(&geometry));
            }
            default_id
        };

        placemarks.push(Placemark {
            name,
            style_url: Some(format!("#{style_id}")),
            geometry,
        });
    }

    let doc_name = root["name"].as_str().map(String::from);

    Ok(KmzData {
        name: doc_name,
        placemarks,
        ground_overlays: Vec::new(),
        styles,
        images: HashMap::new(),
    })
}

fn extract_features(root: &serde_json::Value) -> Result<Vec<serde_json::Value>> {
    match root["type"].as_str() {
        Some("FeatureCollection") => {
            let Some(arr) = root["features"].as_array() else {
                bail!("FeatureCollection has no features array");
            };
            Ok(arr.clone())
        }
        Some("Feature") => Ok(vec![root.clone()]),
        Some("Point" | "MultiPoint" | "LineString" | "MultiLineString" | "Polygon" | "MultiPolygon" | "GeometryCollection") => {
            Ok(vec![serde_json::json!({
                "type": "Feature",
                "properties": {},
                "geometry": root
            })])
        }
        _ => bail!("not a valid GeoJSON document"),
    }
}

const MAX_GEOMETRY_DEPTH: usize = 10;

fn parse_geometry(val: &serde_json::Value) -> Option<Geometry> {
    parse_geometry_depth(val, 0)
}

fn parse_geometry_depth(val: &serde_json::Value, depth: usize) -> Option<Geometry> {
    if depth > MAX_GEOMETRY_DEPTH { return None; }
    let geom_type = val["type"].as_str()?;
    match geom_type {
        "Point" => {
            let coords = val["coordinates"].as_array()?;
            let lon = coords.first()?.as_f64()?;
            let lat = coords.get(1)?.as_f64()?;
            Some(Geometry::Point { lon, lat })
        }
        "MultiPoint" => {
            let coords = val["coordinates"].as_array()?;
            let points: Vec<Geometry> = coords.iter().filter_map(|c| {
                let arr = c.as_array()?;
                Some(Geometry::Point { lon: arr.first()?.as_f64()?, lat: arr.get(1)?.as_f64()? })
            }).collect();
            if points.is_empty() { None } else { Some(Geometry::Multi(points)) }
        }
        "LineString" => {
            let coords = parse_coord_array(&val["coordinates"])?;
            if coords.len() < 2 { return None; }
            Some(Geometry::LineString { coords })
        }
        "MultiLineString" => {
            let lines: Vec<Geometry> = val["coordinates"].as_array()?
                .iter()
                .filter_map(|ring| {
                    let coords = parse_coord_array(ring)?;
                    if coords.len() < 2 { return None; }
                    Some(Geometry::LineString { coords })
                })
                .collect();
            match lines.len() {
                0 => None,
                1 => Some(lines.into_iter().next().expect("checked")),
                _ => Some(Geometry::Multi(lines)),
            }
        }
        "Polygon" => {
            let rings = val["coordinates"].as_array()?;
            let outer = parse_coord_array(rings.first()?)?;
            if outer.len() < 3 { return None; }
            let inner: Vec<Vec<(f64, f64)>> = rings.iter().skip(1)
                .filter_map(|r| {
                    let coords = parse_coord_array(r)?;
                    if coords.len() < 3 { return None; }
                    Some(coords)
                })
                .collect();
            Some(Geometry::Polygon { outer, inner })
        }
        "MultiPolygon" => {
            let polygons: Vec<Geometry> = val["coordinates"].as_array()?
                .iter()
                .filter_map(|poly| {
                    let rings = poly.as_array()?;
                    let outer = parse_coord_array(rings.first()?)?;
                    if outer.len() < 3 { return None; }
                    let inner: Vec<Vec<(f64, f64)>> = rings.iter().skip(1)
                        .filter_map(parse_coord_array)
                        .filter(|c| c.len() >= 3)
                        .collect();
                    Some(Geometry::Polygon { outer, inner })
                })
                .collect();
            match polygons.len() {
                0 => None,
                1 => Some(polygons.into_iter().next().expect("checked")),
                _ => Some(Geometry::Multi(polygons)),
            }
        }
        "GeometryCollection" => {
            let geoms: Vec<Geometry> = val["geometries"].as_array()?
                .iter()
                .filter_map(|g| parse_geometry_depth(g, depth + 1))
                .collect();
            if geoms.is_empty() { None } else { Some(Geometry::Multi(geoms)) }
        }
        _ => None,
    }
}

fn parse_coord_array(val: &serde_json::Value) -> Option<Vec<(f64, f64)>> {
    let arr = val.as_array()?;
    let coords: Vec<(f64, f64)> = arr.iter().filter_map(|c| {
        let pair = c.as_array()?;
        Some((pair.first()?.as_f64()?, pair.get(1)?.as_f64()?))
    }).collect();
    if coords.is_empty() { None } else { Some(coords) }
}

fn extract_style(feature: &serde_json::Value) -> Style {
    let props = &feature["properties"];
    let mut style = Style::default();

    if let Some(color) = props["stroke"].as_str().and_then(parse_css_color) {
        style.line_color = Some(color);
    }
    if let Some(color) = props["marker-color"].as_str().and_then(parse_css_color) {
        style.line_color = Some(color);
    }
    if let Some(w) = props["stroke-width"].as_f64() {
        style.line_width = Some(w as f32);
    }
    if let Some(opacity) = props["stroke-opacity"].as_f64()
        && let Some(ref mut c) = style.line_color
    {
        c[3] = (opacity.clamp(0.0, 1.0) * 255.0) as u8;
    }
    if let Some(color) = props["fill"].as_str().and_then(parse_css_color) {
        style.fill_color = Some(color);
        style.poly_fill = Some(true);
    }
    if let Some(opacity) = props["fill-opacity"].as_f64()
        && let Some(ref mut c) = style.fill_color
    {
        c[3] = (opacity.clamp(0.0, 1.0) * 255.0) as u8;
    }

    style
}

fn parse_css_color(s: &str) -> Option<[u8; 4]> {
    let s = s.strip_prefix('#')?;
    match s.len() {
        6 => {
            let r = u8::from_str_radix(&s[0..2], 16).ok()?;
            let g = u8::from_str_radix(&s[2..4], 16).ok()?;
            let b = u8::from_str_radix(&s[4..6], 16).ok()?;
            Some([r, g, b, 255])
        }
        3 => {
            let r = u8::from_str_radix(&s[0..1], 16).ok()? * 17;
            let g = u8::from_str_radix(&s[1..2], 16).ok()? * 17;
            let b = u8::from_str_radix(&s[2..3], 16).ok()? * 17;
            Some([r, g, b, 255])
        }
        _ => None,
    }
}

fn default_style_id(geom: &Geometry) -> String {
    match geom {
        Geometry::Point { .. } => "_geojson_default_point".to_string(),
        Geometry::LineString { .. } => "_geojson_default_line".to_string(),
        Geometry::Polygon { .. } => "_geojson_default_polygon".to_string(),
        Geometry::Multi(gs) => gs.first().map_or("_geojson_default_line".to_string(), default_style_id),
    }
}

fn default_style_for(geom: &Geometry) -> Style {
    match geom {
        Geometry::Point { .. } => Style {
            line_color: Some(DEFAULT_POINT_COLOR),
            line_width: Some(DEFAULT_LINE_WIDTH),
            ..Style::default()
        },
        Geometry::LineString { .. } => Style {
            line_color: Some(DEFAULT_LINE_COLOR),
            line_width: Some(DEFAULT_LINE_WIDTH),
            ..Style::default()
        },
        Geometry::Polygon { .. } => Style {
            fill_color: Some(DEFAULT_FILL_COLOR),
            line_color: Some(DEFAULT_LINE_COLOR),
            line_width: Some(1.0),
            poly_fill: Some(true),
            poly_outline: Some(true),
        },
        Geometry::Multi(gs) => gs.first().map_or(Style::default(), default_style_for),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_feature_collection() {
        let json = r##"{
            "type": "FeatureCollection",
            "features": [
                {
                    "type": "Feature",
                    "properties": { "name": "A line", "stroke": "#ff0000", "stroke-width": 3 },
                    "geometry": { "type": "LineString", "coordinates": [[0,0],[1,1],[2,0]] }
                },
                {
                    "type": "Feature",
                    "properties": { "fill": "#00ff00", "fill-opacity": 0.5 },
                    "geometry": { "type": "Polygon", "coordinates": [[[0,0],[1,0],[1,1],[0,1],[0,0]]] }
                }
            ]
        }"##;
        let data = parse_geojson(json).expect("parse");
        assert_eq!(data.placemarks.len(), 2);
        assert_eq!(data.placemarks[0].name.as_deref(), Some("A line"));

        let style = data.resolve_style(&data.placemarks[0]);
        assert_eq!(style.line_color, Some([255, 0, 0, 255]));
        assert_eq!(style.line_width, Some(3.0));

        let fill_style = data.resolve_style(&data.placemarks[1]);
        assert_eq!(fill_style.fill_color, Some([0, 255, 0, 127]));
    }

    #[test]
    fn test_single_geometry() {
        let json = r#"{ "type": "Point", "coordinates": [10.5, 48.2] }"#;
        let data = parse_geojson(json).expect("parse");
        assert_eq!(data.placemarks.len(), 1);
        match &data.placemarks[0].geometry {
            Geometry::Point { lon, lat } => {
                assert!((lon - 10.5).abs() < 1e-10);
                assert!((lat - 48.2).abs() < 1e-10);
            }
            _ => panic!("expected Point"),
        }
    }

    #[test]
    fn test_multi_polygon() {
        let json = r#"{
            "type": "Feature",
            "properties": {},
            "geometry": {
                "type": "MultiPolygon",
                "coordinates": [
                    [[[0,0],[1,0],[1,1],[0,0]]],
                    [[[2,2],[3,2],[3,3],[2,2]]]
                ]
            }
        }"#;
        let data = parse_geojson(json).expect("parse");
        assert_eq!(data.placemarks.len(), 1);
        match &data.placemarks[0].geometry {
            Geometry::Multi(gs) => assert_eq!(gs.len(), 2),
            _ => panic!("expected Multi"),
        }
    }

    #[test]
    fn test_geometry_collection() {
        let json = r#"{
            "type": "Feature",
            "properties": {},
            "geometry": {
                "type": "GeometryCollection",
                "geometries": [
                    { "type": "Point", "coordinates": [1, 2] },
                    { "type": "LineString", "coordinates": [[0,0],[1,1]] }
                ]
            }
        }"#;
        let data = parse_geojson(json).expect("parse");
        assert_eq!(data.placemarks.len(), 1);
        match &data.placemarks[0].geometry {
            Geometry::Multi(gs) => assert_eq!(gs.len(), 2),
            _ => panic!("expected Multi"),
        }
    }

    #[test]
    fn test_multi_linestring() {
        let json = r#"{
            "type": "Feature",
            "properties": {},
            "geometry": {
                "type": "MultiLineString",
                "coordinates": [[[0,0],[1,1]], [[2,2],[3,3],[4,4]]]
            }
        }"#;
        let data = parse_geojson(json).expect("parse");
        match &data.placemarks[0].geometry {
            Geometry::Multi(gs) => {
                assert_eq!(gs.len(), 2);
                match &gs[1] {
                    Geometry::LineString { coords } => assert_eq!(coords.len(), 3),
                    _ => panic!("expected LineString"),
                }
            }
            _ => panic!("expected Multi"),
        }
    }

    #[test]
    fn test_malformed_geojson() {
        assert!(parse_geojson("not json").is_err());
        assert!(parse_geojson(r#"{"type":"Unknown"}"#).is_err());
        assert!(parse_geojson(r#"{"type":"FeatureCollection"}"#).is_err());
    }

    #[test]
    fn test_nested_geometry_collection_depth_limit() {
        let mut json = r#"{"type":"GeometryCollection","geometries":["#.to_string();
        for _ in 0..20 {
            json.push_str(r#"{"type":"GeometryCollection","geometries":["#);
        }
        json.push_str(r#"{"type":"Point","coordinates":[0,0]}"#);
        for _ in 0..20 {
            json.push_str("]}");
        }
        json.push_str("]}");
        let data = parse_geojson(&json).expect("parse without stack overflow");
        assert_eq!(data.placemarks.len(), 0);
    }
}
