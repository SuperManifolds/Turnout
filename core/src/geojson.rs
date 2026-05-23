//! OSM JSON → `GeoJSON` conversion and geometry utilities for preview rendering.

use std::collections::{HashMap, HashSet};

const GEOJSON_TAG_KEYS: &[&str] = &[
    "usage", "service", "name", "maxspeed", "electrified", "gauge", "layer", "bridge", "tunnel",
];

/// Convert Overpass JSON to `GeoJSON` for map preview, filtering by railway type
/// and optionally clipping to a bbox (s, w, n, e in lat/lon).
#[must_use]
pub fn osm_json_to_geojson(
    json: &str,
    enabled_types: &[String],
    clip_bbox: Option<(f64, f64, f64, f64)>,
) -> String {
    let data: serde_json::Value = match serde_json::from_str(json) {
        Ok(d) => d,
        Err(_) => return EMPTY_GEOJSON.to_string(),
    };
    let Some(elements) = data["elements"].as_array() else {
        return EMPTY_GEOJSON.to_string();
    };

    let mut nodes: HashMap<u64, (f64, f64)> = HashMap::new();
    for e in elements {
        if e["type"].as_str() == Some("node")
            && let (Some(id), Some(lat), Some(lon)) = (e["id"].as_u64(), e["lat"].as_f64(), e["lon"].as_f64()) {
                nodes.insert(id, (lon, lat));
            }
    }

    let mut features = Vec::new();
    for e in elements {
        if e["type"].as_str() != Some("way") { continue; }
        let Some(node_ids) = e["nodes"].as_array() else { continue };
        let raw_coords: Vec<(f64, f64)> = node_ids.iter()
            .filter_map(serde_json::Value::as_u64)
            .filter_map(|id| nodes.get(&id).copied())
            .collect();
        if raw_coords.len() < 2 { continue; }

        let coord_groups = if let Some((s, w, n, e)) = clip_bbox {
            clip_linestring(&raw_coords, s, w, n, e)
        } else {
            vec![raw_coords]
        };

        for coords_group in &coord_groups {
            if coords_group.len() < 2 { continue; }
            let coords: Vec<String> = coords_group.iter()
                .map(|(lon, lat)| format!("[{lon},{lat}]"))
                .collect();

            let tags = &e["tags"];
            let railway = tags["railway"].as_str().unwrap_or("rail");
            if !enabled_types.iter().any(|t| t == railway) { continue; }
            let mut props = vec![format!(r#""railway":"{railway}""#)];
            for key in GEOJSON_TAG_KEYS {
                if let Some(val) = tags[*key].as_str() {
                    let escaped = val.replace('\\', "\\\\").replace('"', "\\\"");
                    props.push(format!(r#""{key}":"{escaped}""#));
                }
            }

            features.push(format!(
                r#"{{"type":"Feature","properties":{{{}}},"geometry":{{"type":"LineString","coordinates":[{}]}}}}"#,
                props.join(","),
                coords.join(",")
            ));
        }
    }

    format!(r#"{{"type":"FeatureCollection","features":[{}]}}"#, features.join(","))
}

/// Summary statistics from a single pass over Overpass JSON.
pub struct PreviewStats {
    pub railway_types: Vec<String>,
    pub way_count: usize,
    pub total_nodes: usize,
}

/// Extract railway types, way count, and filtered node count in a single JSON parse.
#[must_use]
pub fn analyze_overpass_json(json: &str, enabled_types: &[String]) -> PreviewStats {
    let data: serde_json::Value = match serde_json::from_str(json) {
        Ok(d) => d,
        Err(_) => return PreviewStats { railway_types: vec![], way_count: 0, total_nodes: 0 },
    };
    let Some(elements) = data["elements"].as_array() else {
        return PreviewStats { railway_types: vec![], way_count: 0, total_nodes: 0 };
    };

    let mut types = HashSet::new();
    let mut way_count = 0;
    let mut total_nodes = 0;

    for e in elements {
        if e["type"].as_str() != Some("way") { continue; }
        way_count += 1;
        if let Some(rt) = e["tags"]["railway"].as_str() {
            types.insert(rt.to_string());
            if enabled_types.iter().any(|t| t == rt)
                && let Some(nodes) = e["nodes"].as_array() {
                    total_nodes += nodes.len();
            }
        }
    }

    let mut railway_types: Vec<String> = types.into_iter().collect();
    railway_types.sort();
    PreviewStats { railway_types, way_count, total_nodes }
}

/// Clip a linestring (as lon/lat pairs) to a bbox (s,w,n,e in lat/lon).
/// Returns one or more clipped linestrings.
#[must_use]
pub fn clip_linestring(coords: &[(f64, f64)], s: f64, w: f64, n: f64, e: f64) -> Vec<Vec<(f64, f64)>> {
    let is_inside = |lon: f64, lat: f64| lat >= s && lat <= n && lon >= w && lon <= e;
    let mut result = Vec::new();
    let mut current: Vec<(f64, f64)> = Vec::new();

    for i in 0..coords.len() {
        let (lon, lat) = coords[i];
        let inside = is_inside(lon, lat);

        if i > 0 {
            let (prev_lon, prev_lat) = coords[i - 1];
            let prev_inside = is_inside(prev_lon, prev_lat);

            if prev_inside && !inside {
                if let Some(pt) = line_rect_intersect_lonlat(prev_lon, prev_lat, lon, lat, s, w, n, e) {
                    current.push(pt);
                }
                if current.len() >= 2 { result.push(std::mem::take(&mut current)); }
                current.clear();
            } else if !prev_inside && inside
                && let Some(pt) = line_rect_intersect_lonlat(lon, lat, prev_lon, prev_lat, s, w, n, e) {
                    current.push(pt);
            }
        }

        if inside { current.push((lon, lat)); }
    }

    if current.len() >= 2 { result.push(current); }
    result
}

/// Parse an ORM link like `https://openrailwaymap.app/#view=9.49/34.1997/-117.2839`.
/// Returns (zoom, lat, lng) if valid.
#[must_use]
pub fn parse_orm_link(text: &str) -> Option<(f64, f64, f64)> {
    let text = text.trim();
    let hash_pos = text.find("#view=")?;
    let fragment = &text[hash_pos + 6..];
    let parts: Vec<&str> = fragment.split('/').collect();
    if parts.len() >= 3 {
        let zoom = parts[0].parse::<f64>().ok()?;
        let lat = parts[1].parse::<f64>().ok()?;
        let lng = parts[2].parse::<f64>().ok()?;
        Some((zoom, lat, lng))
    } else {
        None
    }
}

fn line_rect_intersect_lonlat(
    in_lon: f64, in_lat: f64, out_lon: f64, out_lat: f64,
    s: f64, w: f64, n: f64, e: f64,
) -> Option<(f64, f64)> {
    let dx = out_lon - in_lon;
    let dy = out_lat - in_lat;
    let mut best_t = f64::MAX;
    let mut best_pt = (0.0, 0.0);

    if dy.abs() > 1e-12 {
        let t = (s - in_lat) / dy;
        if t > 0.0 && t < best_t { let x = in_lon + t * dx; if x >= w && x <= e { best_t = t; best_pt = (x, s); } }
    }
    if dy.abs() > 1e-12 {
        let t = (n - in_lat) / dy;
        if t > 0.0 && t < best_t { let x = in_lon + t * dx; if x >= w && x <= e { best_t = t; best_pt = (x, n); } }
    }
    if dx.abs() > 1e-12 {
        let t = (w - in_lon) / dx;
        if t > 0.0 && t < best_t { let y = in_lat + t * dy; if y >= s && y <= n { best_t = t; best_pt = (w, y); } }
    }
    if dx.abs() > 1e-12 {
        let t = (e - in_lon) / dx;
        if t > 0.0 && t < best_t { let y = in_lat + t * dy; if y >= s && y <= n { best_t = t; best_pt = (e, y); } }
    }

    if best_t < f64::MAX { Some(best_pt) } else { None }
}

const EMPTY_GEOJSON: &str = r#"{"type":"FeatureCollection","features":[]}"#;
