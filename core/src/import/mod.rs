//! Import `OpenRailwayMap` tracks into a Nimby Rails blueprint.
//!
//! Pipeline:
//! 1. Load Overpass JSON → OSM nodes + ways
//! 2. Clip ways to bbox (optional)
//! 3. Merge ways into continuous routes through shared endpoints
//! 4. Simplify routes to control points (direction changes + max spacing)
//! 5. Generate game track nodes with junction topology
//! 6. Serialize to .nrclip

use crate::error::{Context, CoreError, Result};
use std::collections::{HashMap, HashSet};

use crate::geo::latlon_to_mercator;
use crate::nrc1::NrclipFile;
use crate::types::{Track, TrackKind, TrackKindHorizon, TrackTexture, ModMeta, ModRelFile};

mod build;
mod simplify;

use build::{attach_branches, build_track_nodes, serialize_to_nrclip};
use simplify::{simplify_routes, subdivide_long_segments};

const MODEL_VERSION: u32 = 226;
const MAX_SPACING: f64 = 200.0;
const MAX_TRACK_NODES: usize = 50_000;
const ALIGNMENT_THRESHOLD: f64 = 2.5; // ~143° — reject near-reversal continuations
const JUNCTION_ENDPOINT_SPACING: f64 = 30.0; // meters — control point near junction endpoints
const SPLINE_TOLERANCE: f64 = 5.0; // meters — max deviation before adding subdivision node
const BRANCH_OFFSET: f64 = 5.0; // meters — nudge branch root away from parent
const EARTH_RADIUS: f64 = 6_378_137.0;

type VanillaTrackData = (Vec<(i32, TrackKind)>, Vec<ModMeta>);
type PipelineResult = (Vec<Track>, Vec<Vec<(usize, f64, f64)>>, RouteData, OsmData);

// Vanilla game track type keys
const TRACK_TYPE_HIGH_SPEED: i32 = 1;
const TRACK_TYPE_TRAM: i32 = 2;
const TRACK_TYPE_MEDIUM: i32 = 3;

// ══════════════════════════════════════════════════════════════════════
// Intermediate data structures
// ══════════════════════════════════════════════════════════════════════

struct OsmData {
    nodes: HashMap<u64, (f64, f64)>,
    ways: Vec<Vec<u64>>,
    node_layer: HashMap<u64, i32>,
    node_track_type: HashMap<u64, i32>,
    node_maxspeed: HashMap<u64, f32>,
    node_railway_type: HashMap<u64, String>,
    way_layers: Vec<i32>,
    way_track_types: Vec<i32>,
}

struct RouteData {
    routes: Vec<Vec<u64>>,
    route_coords: Vec<Vec<(f64, f64)>>,
    junction_nodes: HashSet<u64>,
    junction_owner: HashMap<u64, usize>,
}

struct RouteNodeInfo {
    game_id: i64,
}

// ══════════════════════════════════════════════════════════════════════
// Public API
// ══════════════════════════════════════════════════════════════════════

/// Map OSM way tags to a vanilla game track type.
fn osm_to_track_type(tags: &serde_json::Value) -> i32 {
    let railway = tags.get("railway").and_then(|v| v.as_str()).unwrap_or("");

    if railway == "tram" || railway == "light_rail" {
        return TRACK_TYPE_TRAM;
    }
    if tags.get("highspeed").and_then(|v| v.as_str()) == Some("yes") {
        return TRACK_TYPE_HIGH_SPEED;
    }
    if tags.get("usage").and_then(|v| v.as_str()) == Some("highspeed") {
        return TRACK_TYPE_HIGH_SPEED;
    }
    if parse_maxspeed_tag(tags).is_some_and(|kmh| kmh >= 200.0) {
        return TRACK_TYPE_HIGH_SPEED;
    }

    TRACK_TYPE_MEDIUM
}

/// Extract the best maxspeed value from OSM tags in km/h.
/// Checks `maxspeed`, then falls back to `maxspeed:forward`/`maxspeed:backward`
/// (taking the higher value since Nimby Rails has no directional speed limits).
fn parse_maxspeed_tag(tags: &serde_json::Value) -> Option<f64> {
    // Try plain maxspeed first
    if let Some(s) = tags.get("maxspeed").and_then(|v| v.as_str()) {
        let kmh = parse_maxspeed_kmh(s);
        if kmh > 0.0 { return Some(kmh); }
    }
    // Fall back to directional tags — use the higher value
    let fwd = tags.get("maxspeed:forward").and_then(|v| v.as_str()).map_or(0.0, parse_maxspeed_kmh);
    let bwd = tags.get("maxspeed:backward").and_then(|v| v.as_str()).map_or(0.0, parse_maxspeed_kmh);
    let best = fwd.max(bwd);
    if best > 0.0 { Some(best) } else { None }
}

/// Parse an OSM maxspeed value to km/h. Handles "200", "79 mph", etc.
fn parse_maxspeed_kmh(s: &str) -> f64 {
    let s = s.trim();
    if let Some(mph) = s.strip_suffix("mph") {
        mph.trim().parse::<f64>().unwrap_or(0.0) * 1.60934
    } else if let Some(knots) = s.strip_suffix("knots") {
        knots.trim().parse::<f64>().unwrap_or(0.0) * 1.852
    } else {
        s.parse::<f64>().unwrap_or(0.0)
    }
}

/// Extract vanilla `TrackKind` definitions (keys 1,2,3) and their `ModMeta` from a
/// game collections.nrclip file.
pub fn extract_vanilla_track_kinds(collections_path: &str) -> Result<VanillaTrackData> {
    let data = std::fs::read(collections_path).context("read collections.nrclip")?;
    let file = NrclipFile::from_bytes(&data).context("parse collections.nrclip")?;

    for coll in &file.collections {
        for clip in &coll.clips {
            let has_all = [1, 2, 3].iter().all(|key| {
                clip.track_kinds.iter().any(|(k, _)| k == key)
            });
            if has_all {
                let kinds: Vec<(i32, TrackKind)> = clip.track_kinds.iter()
                    .filter(|(k, _)| *k >= 1 && *k <= 3)
                    .cloned()
                    .collect();
                return Ok((kinds, clip.mod_metas.clone()));
            }
        }
    }

    Err(CoreError::Import(
        "vanilla track kinds (1,2,3) not found in collections.nrclip".into(),
    ))
}

/// Hardcoded vanilla track kind definitions (keys 1–3). These reference the game's
/// built-in textures (`workshop_id=0, path="tracks"`) so they render correctly
/// without needing to read the user's `collections.nrclip`.
#[must_use]
pub fn default_track_kinds() -> Vec<(i32, TrackKind)> {
    let vanilla_tex = || -> ModRelFile {
        ModRelFile { workshop_id: 0, path: "tracks".to_string(), name: String::new() }
    };
    let empty_file = || -> ModRelFile {
        ModRelFile { workshop_id: 0, path: String::new(), name: String::new() }
    };
    let textures = || -> Vec<TrackTexture> {
        (0..6).map(|sc| TrackTexture {
            speed_class: sc,
            files: if sc <= 3 {
                [vanilla_tex(), vanilla_tex(), vanilla_tex(), vanilla_tex()]
            } else {
                [empty_file(), vanilla_tex(), empty_file(), empty_file()]
            },
        }).collect()
    };

    let make_kind = |key: i32, display: &str, internal: &str, max_speeds: [f64; 3]| -> (i32, TrackKind) {
        (key, TrackKind {
            display_name: display.to_string(),
            speed_class_flag: 1,
            speed_class: key,
            internal_name: internal.to_string(),
            secondary_name: format!("{internal}_name"),
            horizons: [
                TrackKindHorizon {
                    speed_class: 0, gauge: 97.222_222_222_222_21, height: 5.21,
                    max_speed: max_speeds[0], width_a: 10.0, width_b: 25.0,
                    spacing: 15.0, offset_a: 2.5, offset_b: 2.0,
                    visual_distance: 125_000, flags: [0, 0, 0, 1, 0],
                    textures: textures(),
                },
                TrackKindHorizon {
                    speed_class: 0, gauge: 97.222_222_222_222_21, height: 5.21,
                    max_speed: max_speeds[1], width_a: 10.0, width_b: 25.0,
                    spacing: 25.0, offset_a: 2.5, offset_b: 2.0,
                    visual_distance: 125_000, flags: [1, 1, 1, 1, 0],
                    textures: textures(),
                },
                TrackKindHorizon {
                    speed_class: 0, gauge: 97.222_222_222_222_21, height: 5.21,
                    max_speed: max_speeds[2], width_a: 10.0, width_b: 25.0,
                    spacing: 15.0, offset_a: 2.5, offset_b: 2.0,
                    visual_distance: 125_000, flags: [0, 0, 0, 0, 0],
                    textures: textures(),
                },
            ],
        })
    };

    vec![
        make_kind(1, "waw_track_hs_1", "High speed", [3300.0, 500.0, 4000.0]),
        make_kind(2, "waw_track_tram_1", "Tram", [500.0, 200.0, 700.0]),
        make_kind(3, "waw_track_med_1", "Medium", [1600.0, 500.0, 2200.0]),
    ]
}

/// Shared import pipeline: parse → clip → merge → simplify → subdivide → build nodes.
fn run_pipeline(
    json: &str,
    railway_types: &[String],
    clip_to_bbox: Option<(f64, f64, f64, f64)>,
    apply_speed_limits: bool,
    tangent_mode: bool,
    type_speed_overrides: &HashMap<String, u32>,
    on_progress: &dyn Fn(&str),
) -> Result<PipelineResult> {
    on_progress("Parsing OSM data...");
    let mut osm = parse_osm_data(json, railway_types)?;
    if let Some(bbox) = clip_to_bbox {
        on_progress("Clipping to selection...");
        clip_ways_to_bbox(&mut osm, bbox);
    }
    on_progress("Merging routes...");
    let route_data = merge_ways_into_routes(&osm);
    on_progress("Simplifying tracks...");
    let simplified = simplify_routes(&route_data, &osm.node_layer);
    let simplified = subdivide_long_segments(simplified, &route_data.route_coords);
    on_progress("Building track nodes...");
    let track_nodes = build_track_nodes(&simplified, &route_data, &osm, apply_speed_limits, tangent_mode, type_speed_overrides);
    Ok((track_nodes, simplified, route_data, osm))
}

/// Run the import pipeline up to track node generation and return the count.
/// Used for dry-run validation before committing to a full import.
pub fn count_track_nodes(
    json: &str,
    railway_types: &[String],
    clip_to_bbox: Option<(f64, f64, f64, f64)>,
    tangent_mode: bool,
) -> Result<usize> {
    let empty = HashMap::new();
    let (track_nodes, _, _, _) = run_pipeline(json, railway_types, clip_to_bbox, false, tangent_mode, &empty, &|_| {})?;
    Ok(track_nodes.len())
}

/// Import `OpenRailwayMap` Overpass JSON into a Nimby Rails .nrclip file.
/// Returns (`file_bytes`, `track_node_count`).
pub fn import_orm(
    json: &str,
    name: &str,
    railway_types: &[String],
    apply_speed_limits: bool,
    clip_to_bbox: Option<(f64, f64, f64, f64)>,
    tangent_mode: bool,
    type_speed_overrides: &HashMap<String, u32>,
    track_kinds: Vec<(i32, TrackKind)>,
    mod_metas: Vec<ModMeta>,
    on_progress: &dyn Fn(&str),
) -> Result<(Vec<u8>, usize)> {
    let (mut track_nodes, simplified, route_data, _) =
        run_pipeline(json, railway_types, clip_to_bbox, apply_speed_limits, tangent_mode, type_speed_overrides, on_progress)?;

    let node_count = track_nodes.len();
    if node_count > MAX_TRACK_NODES {
        return Err(CoreError::Import(format!(
            "Blueprint has {node_count} track nodes, exceeding the {MAX_TRACK_NODES} limit. \
             Reduce the selection area or disable some track types."
        )));
    }

    on_progress("Attaching junctions...");
    attach_branches(
        &mut track_nodes, &simplified, &route_data,
    );

    // Branch roots at junctions must remain point mode — their tangent is
    // inherited from the parent track's shape polyline.
    if tangent_mode {
        for track in &mut track_nodes {
            if track.attached_to_id != 0 {
                track.tangential = Some(0);
            }
        }
    }

    on_progress("Serializing blueprint...");
    let bytes = serialize_to_nrclip(track_nodes, name, track_kinds, mod_metas)?;
    Ok((bytes, node_count))
}

// ══════════════════════════════════════════════════════════════════════
// Pipeline stages
// ══════════════════════════════════════════════════════════════════════

fn parse_osm_data(json: &str, railway_types: &[String]) -> Result<OsmData> {
    let data: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| CoreError::Parse { format: "osm", detail: e.to_string() })?;
    let elements = data["elements"]
        .as_array()
        .ok_or_else(|| CoreError::Parse { format: "osm", detail: "no elements".into() })?;

    let mut osm = OsmData {
        nodes: HashMap::new(),
        ways: Vec::new(),
        node_layer: HashMap::new(),
        node_track_type: HashMap::new(),
        node_maxspeed: HashMap::new(),
        node_railway_type: HashMap::new(),
        way_layers: Vec::new(),
        way_track_types: Vec::new(),
    };

    for e in elements {
        match e["type"].as_str() {
            Some("node") => {
                let Some(id) = e["id"].as_u64() else { continue };
                let Some(lat) = e["lat"].as_f64() else { continue };
                let Some(lon) = e["lon"].as_f64() else { continue };
                osm.nodes.insert(id, (lat, lon));
            }
            Some("way") => {
                if !railway_types.is_empty() {
                    let rt = e.get("tags")
                        .and_then(|t| t.get("railway"))
                        .and_then(|r| r.as_str())
                        .unwrap_or("");
                    if !railway_types.iter().any(|t| t == rt) { continue; }
                }
                let Some(node_arr) = e["nodes"].as_array() else { continue };
                let nids: Vec<u64> = node_arr.iter()
                    .filter_map(serde_json::Value::as_u64)
                    .collect();
                let tags = e.get("tags").cloned().unwrap_or_default();
                let base_type = tags.get("railway")
                    .and_then(|r| r.as_str())
                    .unwrap_or("rail");
                let service = tags.get("service").and_then(|s| s.as_str());
                let usage = tags.get("usage").and_then(|s| s.as_str());
                // Build a specific type key: "rail:yard", "rail:siding", or just "rail"
                let railway_type = match (service, usage) {
                    (Some(s), _) => format!("{base_type}:{s}"),
                    (None, Some(u)) => format!("{base_type}:{u}"),
                    _ => base_type.to_string(),
                };
                let layer: i32 = tags.get("layer")
                    .and_then(|l| l.as_str())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                let tt = osm_to_track_type(&tags);
                let maxspeed_ms = parse_maxspeed_tag(&tags)
                    .map(|kmh| (kmh / 3.6) as f32)
                    .filter(|&v| v > 0.0);

                if nids.len() >= 2 {
                    for &nid in &nids {
                        osm.node_layer.entry(nid)
                            .and_modify(|existing| {
                                if layer.abs() > existing.abs() { *existing = layer; }
                            })
                            .or_insert(layer);
                        osm.node_track_type.entry(nid).or_insert(tt);
                        osm.node_railway_type.entry(nid).or_insert_with(|| railway_type.clone());
                        if let Some(ms) = maxspeed_ms {
                            osm.node_maxspeed.entry(nid).or_insert(ms);
                        }
                    }
                    osm.way_layers.push(layer);
                    osm.way_track_types.push(tt);
                    osm.ways.push(nids);
                }
            }
            _ => {}
        }
    }

    Ok(osm)
}

fn clip_ways_to_bbox(osm: &mut OsmData, (s_lat, w_lon, n_lat, e_lon): (f64, f64, f64, f64)) {
    let mut clipped_ways = Vec::new();
    let mut next_synthetic_id: u64 = 0xFFFF_FFFF_0000_0000;

    for (wi, way) in osm.ways.iter().enumerate() {
        let layer = osm.way_layers[wi];
        let tt = osm.way_track_types[wi];
        let mut current_segment: Vec<u64> = Vec::new();

        for i in 0..way.len() {
            let nid = way[i];
            let Some(&(lat, lon)) = osm.nodes.get(&nid) else { continue };
            let inside = lat >= s_lat && lat <= n_lat && lon >= w_lon && lon <= e_lon;

            if i > 0 {
                let prev_nid = way[i - 1];
                let Some(&(prev_lat, prev_lon)) = osm.nodes.get(&prev_nid) else { continue };
                let prev_inside = prev_lat >= s_lat && prev_lat <= n_lat && prev_lon >= w_lon && prev_lon <= e_lon;

                if prev_inside && !inside {
                    if let Some((ix_lat, ix_lon)) = line_rect_intersect(
                        prev_lat, prev_lon, lat, lon, s_lat, w_lon, n_lat, e_lon
                    ) {
                        let syn_id = next_synthetic_id;
                        next_synthetic_id += 1;
                        osm.nodes.insert(syn_id, (ix_lat, ix_lon));
                        osm.node_layer.insert(syn_id, layer);
                        osm.node_track_type.insert(syn_id, tt);
                        if let Some(&ms) = osm.node_maxspeed.get(&prev_nid) {
                            osm.node_maxspeed.insert(syn_id, ms);
                        }
                        current_segment.push(syn_id);
                    }
                    if current_segment.len() >= 2 {
                        clipped_ways.push(current_segment.clone());
                    }
                    current_segment.clear();
                } else if !prev_inside && inside
                    && let Some((ix_lat, ix_lon)) = line_rect_intersect(
                        lat, lon, prev_lat, prev_lon, s_lat, w_lon, n_lat, e_lon
                    ) {
                        let syn_id = next_synthetic_id;
                        next_synthetic_id += 1;
                        osm.nodes.insert(syn_id, (ix_lat, ix_lon));
                        osm.node_layer.insert(syn_id, layer);
                        osm.node_track_type.insert(syn_id, tt);
                        if let Some(&ms) = osm.node_maxspeed.get(&nid) {
                            osm.node_maxspeed.insert(syn_id, ms);
                        }
                        current_segment.push(syn_id);
                    }
            }

            if inside {
                current_segment.push(nid);
            }
        }

        if current_segment.len() >= 2 {
            clipped_ways.push(current_segment);
        }
    }

    osm.ways = clipped_ways;
}

fn merge_ways_into_routes(osm: &OsmData) -> RouteData {
    // Build node→way index
    let mut node_ways: HashMap<u64, Vec<(usize, usize)>> = HashMap::new();
    for (wi, way) in osm.ways.iter().enumerate() {
        for (pi, &nid) in way.iter().enumerate() {
            node_ways.entry(nid).or_default().push((wi, pi));
        }
    }

    // Identify shared/junction nodes
    let mut shared_nodes: HashSet<u64> = HashSet::new();
    let mut junction_nodes: HashSet<u64> = HashSet::new();
    for (&nid, refs) in &node_ways {
        let n_ways = refs.iter().map(|&(wi, _)| wi).collect::<HashSet<_>>().len();
        if n_ways >= 2 { shared_nodes.insert(nid); }
        if n_ways >= 3 || (n_ways == 2 && refs.iter().any(|&(wi, pi)| pi > 0 && pi < osm.ways[wi].len() - 1)) {
            junction_nodes.insert(nid);
        }
    }

    // Merge ways into routes by extending through shared nodes
    let mut way_used = vec![false; osm.ways.len()];
    let mut routes: Vec<Vec<u64>> = Vec::new();

    let mut way_order: Vec<usize> = (0..osm.ways.len()).collect();
    way_order.sort_by(|&a, &b| osm.ways[b].len().cmp(&osm.ways[a].len()));

    for &start_wi in &way_order {
        if way_used[start_wi] { continue; }
        way_used[start_wi] = true;
        let mut route = osm.ways[start_wi].clone();

        extend_route_forward(&mut route, &osm.ways, &osm.nodes, &shared_nodes, &node_ways, &mut way_used);
        extend_route_backward(&mut route, &osm.ways, &osm.nodes, &shared_nodes, &node_ways, &mut way_used);

        routes.push(route);
    }

    routes.sort_by_key(|r| std::cmp::Reverse(r.len()));

    let route_coords: Vec<Vec<(f64, f64)>> = routes.iter().map(|route| {
        route.iter().filter_map(|nid| {
            osm.nodes.get(nid).map(|&(lat, lon)| latlon_to_mercator(lat, lon))
        }).collect()
    }).collect();

    // Junction ownership: first (longest) route through each junction owns it
    let mut junction_owner: HashMap<u64, usize> = HashMap::new();
    for (ri, route) in routes.iter().enumerate() {
        for &nid in route {
            if junction_nodes.contains(&nid) {
                junction_owner.entry(nid).or_insert(ri);
            }
        }
    }

    RouteData { routes, route_coords, junction_nodes, junction_owner }
}

fn extend_route_forward(
    route: &mut Vec<u64>,
    ways: &[Vec<u64>],
    osm_nodes: &HashMap<u64, (f64, f64)>,
    shared_nodes: &HashSet<u64>,
    node_ways: &HashMap<u64, Vec<(usize, usize)>>,
    way_used: &mut [bool],
) {
    let mut route_set: HashSet<u64> = route.iter().copied().collect();
    loop {
        let last = *route.last().expect("non-empty route");
        if !shared_nodes.contains(&last) { break; }
        let cur_heading = if route.len() >= 2 {
            let a = &osm_nodes[&route[route.len() - 2]];
            let b = &osm_nodes[&last];
            (b.0 - a.0).atan2(b.1 - a.1)
        } else { 0.0 };

        let Some((wi, pi, diff)) = find_best_continuation(last, cur_heading, ways, osm_nodes, node_ways, way_used) else { break };
        if diff > ALIGNMENT_THRESHOLD { break; }

        let new_nodes: Vec<u64> = if pi == 0 {
            ways[wi][1..].to_vec()
        } else {
            ways[wi][..ways[wi].len() - 1].iter().rev().copied().collect()
        };
        if new_nodes.iter().any(|n| route_set.contains(n)) { break; }

        way_used[wi] = true;
        for &n in &new_nodes { route_set.insert(n); }
        route.extend_from_slice(&new_nodes);
    }
}

fn extend_route_backward(
    route: &mut Vec<u64>,
    ways: &[Vec<u64>],
    osm_nodes: &HashMap<u64, (f64, f64)>,
    shared_nodes: &HashSet<u64>,
    node_ways: &HashMap<u64, Vec<(usize, usize)>>,
    way_used: &mut [bool],
) {
    let mut route_set: HashSet<u64> = route.iter().copied().collect();
    loop {
        let first = route[0];
        if !shared_nodes.contains(&first) { break; }
        let cur_heading = if route.len() >= 2 {
            let a = &osm_nodes[&route[1]];
            let b = &osm_nodes[&first];
            (b.0 - a.0).atan2(b.1 - a.1)
        } else { 0.0 };

        let Some((wi, pi, diff)) = find_best_continuation(first, cur_heading, ways, osm_nodes, node_ways, way_used) else { break };
        if diff > ALIGNMENT_THRESHOLD { break; }

        let new_nodes: Vec<u64> = if pi == ways[wi].len() - 1 {
            ways[wi][..ways[wi].len() - 1].to_vec()
        } else {
            ways[wi][1..].iter().rev().copied().collect()
        };
        if new_nodes.iter().any(|n| route_set.contains(n)) { break; }

        way_used[wi] = true;
        for &n in &new_nodes { route_set.insert(n); }
        let mut prefix = new_nodes;
        prefix.push(first);
        prefix.extend_from_slice(&route[1..]);
        *route = prefix;
    }
}

fn find_best_continuation(
    node: u64,
    cur_heading: f64,
    ways: &[Vec<u64>],
    osm_nodes: &HashMap<u64, (f64, f64)>,
    node_ways: &HashMap<u64, Vec<(usize, usize)>>,
    way_used: &[bool],
) -> Option<(usize, usize, f64)> {
    let mut best: Option<(usize, usize, f64)> = None;
    for &(wi, pi) in &node_ways[&node] {
        if way_used[wi] { continue; }
        let cont_first = if pi == 0 {
            ways[wi].get(1)
        } else {
            ways[wi].len().checked_sub(2).and_then(|i| ways[wi].get(i))
        };
        let Some(&cont_nid) = cont_first else { continue };
        let c = &osm_nodes[&cont_nid];
        let b = &osm_nodes[&node];
        let h = (c.0 - b.0).atan2(c.1 - b.1);
        let mut diff = (h - cur_heading).abs();
        if diff > std::f64::consts::PI { diff = std::f64::consts::TAU - diff; }
        if best.is_none_or(|b| diff < b.2) { best = Some((wi, pi, diff)); }
    }
    best
}



// ══════════════════════════════════════════════════════════════════════
// Geometry helpers
// ══════════════════════════════════════════════════════════════════════


/// Find where a line segment (inside → outside) crosses a bbox. Returns (lat, lon).
fn line_rect_intersect(
    in_lat: f64, in_lon: f64, out_lat: f64, out_lon: f64,
    s: f64, w: f64, n: f64, e: f64,
) -> Option<(f64, f64)> {
    // geo::segment_rect_intersect uses (x, y) = (lon, lat)
    let (lon, lat) = crate::geo::segment_rect_intersect(
        in_lon, in_lat, out_lon, out_lat,
        w, s, e, n,
    )?;
    Some((lat, lon))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maxspeed_parses_units_and_defaults_to_zero() {
        assert_eq!(parse_maxspeed_kmh("80"), 80.0);
        assert_eq!(parse_maxspeed_kmh("  100 "), 100.0);
        assert!((parse_maxspeed_kmh("50 mph") - 50.0 * 1.60934).abs() < 1e-6);
        assert!((parse_maxspeed_kmh("50mph") - 50.0 * 1.60934).abs() < 1e-6);
        assert!((parse_maxspeed_kmh("10 knots") - 10.0 * 1.852).abs() < 1e-6);
        // Unparseable tags (e.g. "none", "RU:rural") fall back to 0, not a panic.
        assert_eq!(parse_maxspeed_kmh("none"), 0.0);
        assert_eq!(parse_maxspeed_kmh(""), 0.0);
    }

    #[test]
    fn line_rect_intersect_crosses_the_north_edge_with_latlon_order() {
        // Rect lat[0,10] lon[0,10]; segment from inside (lat5,lon5) north to (lat15,lon5)
        // must cross the north edge at lat=10, lon=5 — and return (lat, lon), not (lon, lat).
        let hit = line_rect_intersect(5.0, 5.0, 15.0, 5.0, 0.0, 0.0, 10.0, 10.0)
            .expect("segment crosses the rect boundary");
        assert!((hit.0 - 10.0).abs() < 1e-9, "lat should be the north edge, got {}", hit.0);
        assert!((hit.1 - 5.0).abs() < 1e-9, "lon should be unchanged, got {}", hit.1);
    }
}
