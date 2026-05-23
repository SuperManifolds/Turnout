//! Import `OpenRailwayMap` tracks into a Nimby Rails blueprint.
//!
//! Pipeline:
//! 1. Load Overpass JSON → OSM nodes + ways
//! 2. Clip ways to bbox (optional)
//! 3. Merge ways into continuous routes through shared endpoints
//! 4. Simplify routes to control points (direction changes + max spacing)
//! 5. Generate game track nodes with junction topology
//! 6. Serialize to .nrclip

use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};

use crate::hobby;
use crate::nrc1::NrclipFile;
use crate::types::{Collection, Clip, Track, TrackKind, ModMeta};

const MODEL_VERSION: u32 = 226;
const MAX_SPACING: f64 = 200.0;
const MAX_TRACK_NODES: usize = 50_000;
const ALIGNMENT_THRESHOLD: f64 = 2.5; // ~143° — reject near-reversal continuations
const JUNCTION_ENDPOINT_SPACING: f64 = 30.0; // meters — control point near junction endpoints
const SPLINE_TOLERANCE: f64 = 5.0; // meters — max deviation before adding subdivision node
const BRANCH_OFFSET: f64 = 5.0; // meters — nudge branch root away from parent

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
    if let Some(ms) = tags.get("maxspeed").and_then(|v| v.as_str())
        && parse_maxspeed_kmh(ms) >= 200.0 {
            return TRACK_TYPE_HIGH_SPEED;
        }

    TRACK_TYPE_MEDIUM
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

    anyhow::bail!("vanilla track kinds (1,2,3) not found in collections.nrclip")
}

/// Shared import pipeline: parse → clip → merge → simplify → subdivide → build nodes.
fn run_pipeline(
    json: &str,
    railway_types: &[String],
    clip_to_bbox: Option<(f64, f64, f64, f64)>,
    apply_speed_limits: bool,
    tangent_mode: bool,
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
    let track_nodes = build_track_nodes(&simplified, &route_data, &osm, apply_speed_limits, tangent_mode);
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
    let (track_nodes, _, _, _) = run_pipeline(json, railway_types, clip_to_bbox, false, tangent_mode, &|_| {})?;
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
    track_kinds: Vec<(i32, TrackKind)>,
    mod_metas: Vec<ModMeta>,
    on_progress: &dyn Fn(&str),
) -> Result<(Vec<u8>, usize)> {
    let (mut track_nodes, simplified, route_data, osm) =
        run_pipeline(json, railway_types, clip_to_bbox, apply_speed_limits, tangent_mode, on_progress)?;

    let node_count = track_nodes.len();
    if node_count > MAX_TRACK_NODES {
        anyhow::bail!(
            "Blueprint has {node_count} track nodes, exceeding the {MAX_TRACK_NODES} limit. \
             Reduce the selection area or disable some track types."
        );
    }

    on_progress("Attaching junctions...");
    attach_branches(
        &mut track_nodes, &simplified, &route_data, &osm.nodes,
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
    let data: serde_json::Value = serde_json::from_str(json).context("parse JSON")?;
    let elements = data["elements"].as_array().context("no elements")?;

    let mut osm = OsmData {
        nodes: HashMap::new(),
        ways: Vec::new(),
        node_layer: HashMap::new(),
        node_track_type: HashMap::new(),
        node_maxspeed: HashMap::new(),
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
                let layer: i32 = tags.get("layer")
                    .and_then(|l| l.as_str())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                let tt = osm_to_track_type(&tags);
                let maxspeed_ms = tags.get("maxspeed")
                    .and_then(|v| v.as_str())
                    .map(|s| (parse_maxspeed_kmh(s) / 3.6) as f32)
                    .filter(|&v| v > 0.0);

                if nids.len() >= 2 {
                    for &nid in &nids {
                        osm.node_layer.entry(nid)
                            .and_modify(|existing| {
                                if layer.abs() > existing.abs() { *existing = layer; }
                            })
                            .or_insert(layer);
                        osm.node_track_type.entry(nid).or_insert(tt);
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

fn simplify_routes(
    rd: &RouteData,
    node_layer: &HashMap<u64, i32>,
) -> Vec<Vec<(usize, f64, f64)>> {
    rd.routes.iter().zip(rd.route_coords.iter())
        .map(|(route, coords)| {
            let mut keep = vec![false; coords.len()];
            keep[0] = true;
            *keep.last_mut().expect("non-empty coords") = true;

            // Force keep junction nodes and layer-change boundaries
            for (i, &nid) in route.iter().enumerate() {
                if rd.junction_nodes.contains(&nid) {
                    keep[i] = true;
                }
                if i > 0 {
                    let prev_layer = node_layer.get(&route[i - 1]).copied().unwrap_or(0);
                    let cur_layer = node_layer.get(&nid).copied().unwrap_or(0);
                    if prev_layer != cur_layer {
                        keep[i - 1] = true;
                        keep[i] = true;
                    }
                }
            }

            // Keep control point near junction endpoints
            keep_near_junction_endpoints(route, coords, &rd.junction_nodes, &mut keep);

            // Enforce max spacing
            enforce_max_spacing(coords, &mut keep);

            // Spline-first simplification
            spline_simplify(coords, &mut keep);

            coords.iter().enumerate()
                .filter(|(i, _)| keep[*i])
                .map(|(i, &(x, y))| (i, x, y))
                .collect()
        }).collect()
}

fn keep_near_junction_endpoints(
    route: &[u64],
    coords: &[(f64, f64)],
    junction_nodes: &HashSet<u64>,
    keep: &mut [bool],
) {
    let start_is_junction = junction_nodes.contains(&route[0]);
    let end_is_junction = junction_nodes.contains(route.last().expect("non-empty route"));

    if start_is_junction && coords.len() > 2 {
        for i in 1..coords.len() - 1 {
            let dx = coords[i].0 - coords[0].0;
            let dy = coords[i].1 - coords[0].1;
            if dx * dx + dy * dy >= JUNCTION_ENDPOINT_SPACING * JUNCTION_ENDPOINT_SPACING {
                keep[i] = true;
                break;
            }
        }
    }
    if end_is_junction && coords.len() > 2 {
        let last = coords.len() - 1;
        for i in (1..last).rev() {
            let dx = coords[i].0 - coords[last].0;
            let dy = coords[i].1 - coords[last].1;
            if dx * dx + dy * dy >= JUNCTION_ENDPOINT_SPACING * JUNCTION_ENDPOINT_SPACING {
                keep[i] = true;
                break;
            }
        }
    }
}

fn enforce_max_spacing(coords: &[(f64, f64)], keep: &mut [bool]) {
    let mut last_kept = 0;
    for i in 1..coords.len() {
        if keep[i] { last_kept = i; continue; }
        let dx = coords[i].0 - coords[last_kept].0;
        let dy = coords[i].1 - coords[last_kept].1;
        if dx * dx + dy * dy >= MAX_SPACING * MAX_SPACING {
            keep[i] = true;
            last_kept = i;
        }
    }
}

fn spline_simplify(coords: &[(f64, f64)], keep: &mut [bool]) {
    for _ in 0..20 {
        let kept_pts: Vec<(f64, f64)> = (0..coords.len())
            .filter(|&i| keep[i]).map(|i| coords[i]).collect();
        let kept_idx: Vec<usize> = (0..coords.len())
            .filter(|&i| keep[i]).collect();

        if kept_pts.len() < 2 { break; }
        let segs = hobby::hobby_spline(&kept_pts);
        let mut added = false;

        for (si, seg) in segs.iter().enumerate() {
            let orig_start = kept_idx[si];
            let orig_end = kept_idx[si + 1];
            if orig_end - orig_start <= 1 { continue; }

            let mut worst_dev = 0.0f64;
            let mut worst_orig = orig_start;
            for (oi, &(ox, oy)) in coords.iter().enumerate().take(orig_end).skip(orig_start + 1) {
                let mut best_d = f64::MAX;
                for s in 0..=32 {
                    let pt = hobby::bezier_point(seg, f64::from(s) / 32.0);
                    let d = ((ox - pt.0).powi(2) + (oy - pt.1).powi(2)).sqrt();
                    if d < best_d { best_d = d; }
                }
                if best_d > worst_dev {
                    worst_dev = best_d;
                    worst_orig = oi;
                }
            }

            if worst_dev > SPLINE_TOLERANCE {
                keep[worst_orig] = true;
                added = true;
            }
        }

        if !added { break; }
    }
}

fn subdivide_long_segments(
    simplified: Vec<Vec<(usize, f64, f64)>>,
    route_coords: &[Vec<(f64, f64)>],
) -> Vec<Vec<(usize, f64, f64)>> {
    simplified.into_iter().zip(route_coords.iter())
        .map(|(simp, coords)| {
            let mut result = Vec::new();
            for i in 0..simp.len() {
                result.push(simp[i]);
                if i + 1 >= simp.len() { continue; }
                let (idx0, x0, y0) = simp[i];
                let (idx1, x1, y1) = simp[i + 1];
                let seg_dist = ((x1 - x0).powi(2) + (y1 - y0).powi(2)).sqrt();
                if seg_dist <= MAX_SPACING { continue; }

                if idx0 != usize::MAX && idx1 != usize::MAX && idx0 < idx1 {
                    interpolate_along_polyline(&mut result, coords, idx0, idx1);
                } else {
                    // Straight-line fallback for interpolated endpoints
                    let n = (seg_dist / MAX_SPACING).ceil() as usize;
                    for j in 1..n {
                        let t = j as f64 / n as f64;
                        result.push((usize::MAX, x0 + (x1 - x0) * t, y0 + (y1 - y0) * t));
                    }
                }
            }
            result
        }).collect()
}

fn interpolate_along_polyline(
    result: &mut Vec<(usize, f64, f64)>,
    coords: &[(f64, f64)],
    start: usize,
    end: usize,
) {
    let mut cum_len = vec![0.0f64];
    for k in start..end {
        let dx = coords[k + 1].0 - coords[k].0;
        let dy = coords[k + 1].1 - coords[k].1;
        cum_len.push(cum_len.last().expect("non-empty cum_len") + (dx * dx + dy * dy).sqrt());
    }
    let total_len = *cum_len.last().expect("non-empty cum_len");
    if total_len < 1.0 { return; }

    let n = (total_len / MAX_SPACING).ceil() as usize;
    for j in 1..n {
        let target = total_len * j as f64 / n as f64;
        let seg = cum_len.partition_point(|&l| l < target).min(cum_len.len() - 1).max(1) - 1;
        let seg_start_len = cum_len[seg];
        let seg_end_len = cum_len[seg + 1];
        let local_t = if seg_end_len > seg_start_len {
            (target - seg_start_len) / (seg_end_len - seg_start_len)
        } else { 0.0 };
        let oi = start + seg;
        let px = coords[oi].0 + (coords[oi + 1].0 - coords[oi].0) * local_t;
        let py = coords[oi].1 + (coords[oi + 1].1 - coords[oi].1) * local_t;
        result.push((usize::MAX, px, py));
    }
}

fn build_track_nodes(
    simplified: &[Vec<(usize, f64, f64)>],
    rd: &RouteData,
    osm: &OsmData,
    apply_speed_limits: bool,
    tangent_mode: bool,
) -> Vec<Track> {
    let mut track_nodes: Vec<Track> = Vec::new();
    let mut node_id_counter: i64 = 100;

    for (ri, simp) in simplified.iter().enumerate() {
        let mut last_layer: i32 = 0;
        let mut last_track_type: i32 = TRACK_TYPE_MEDIUM;
        let mut last_maxspeed: Option<f32> = None;

        for (si, &(orig_idx, x, y)) in simp.iter().enumerate() {
            let gid = node_id_counter;
            node_id_counter += 100;
            let prev = if si > 0 { track_nodes[track_nodes.len() - 1].node_id } else { 0 };

            let (layer, track_type, max_speed) = if orig_idx == usize::MAX {
                (last_layer, last_track_type, last_maxspeed)
            } else {
                let osm_nid = rd.routes[ri][orig_idx];
                let l = osm.node_layer.get(&osm_nid).copied().unwrap_or(0);
                let tt = osm.node_track_type.get(&osm_nid).copied().unwrap_or(TRACK_TYPE_MEDIUM);
                let ms = osm.node_maxspeed.get(&osm_nid).copied();
                last_layer = l;
                last_track_type = tt;
                last_maxspeed = ms;
                (l, tt, ms)
            };

            track_nodes.push(Track {
                node_id: gid, x, y, layer, track_type,
                user_max_speed: if apply_speed_limits { max_speed.or(Some(0.0)) } else { Some(0.0) },
                prev_node: prev,
                tangential: Some(u8::from(tangent_mode)),
                ..Track::default()
            });
            if si > 0 {
                let prev_idx = track_nodes.len() - 2;
                track_nodes[prev_idx].next_node = gid;
            }
        }
    }

    track_nodes
}

fn attach_branches(
    track_nodes: &mut [Track],
    simplified: &[Vec<(usize, f64, f64)>],
    rd: &RouteData,
    osm_nodes: &HashMap<u64, (f64, f64)>,
) {
    // Build route → game node chains and junction game ID map
    let mut route_game_nodes: Vec<Vec<RouteNodeInfo>> = Vec::new();
    let mut junction_game_ids: HashMap<u64, i64> = HashMap::new();
    let mut idx = 0;
    for (ri, simp) in simplified.iter().enumerate() {
        let mut chain = Vec::new();
        for (si, &(orig_idx, _, _)) in simp.iter().enumerate() {
            let gid = track_nodes[idx + si].node_id;
            chain.push(RouteNodeInfo { game_id: gid });
            if orig_idx != usize::MAX {
                let osm_nid = rd.routes[ri][orig_idx];
                if rd.junction_nodes.contains(&osm_nid) && rd.junction_owner.get(&osm_nid) == Some(&ri) {
                    junction_game_ids.insert(osm_nid, gid);
                }
            }
        }
        idx += simp.len();
        route_game_nodes.push(chain);
    }

    // Build O(1) node_id → index lookup
    let node_idx: HashMap<i64, usize> = track_nodes.iter()
        .enumerate()
        .map(|(i, t)| (t.node_id, i))
        .collect();

    for (ri, simp) in simplified.iter().enumerate() {
        if simp.len() < 2 { continue; }

        for &is_start in &[true, false] {
            let endpoint_orig_idx = if is_start { simp[0].0 } else { simp.last().expect("non-empty simp").0 };
            // Skip synthetic interpolated nodes — they can't be junctions
            if endpoint_orig_idx == usize::MAX { continue; }
            let endpoint_osm = rd.routes[ri][endpoint_orig_idx];
            if !rd.junction_nodes.contains(&endpoint_osm) { continue; }

            let Some(&owner_ri) = rd.junction_owner.get(&endpoint_osm) else { continue };
            if owner_ri == ri { continue; }

            let branch_gid = if is_start {
                route_game_nodes[ri][0].game_id
            } else {
                route_game_nodes[ri].last().expect("non-empty chain").game_id
            };

            let junction_orig = rd.routes[ri][endpoint_orig_idx];
            let junction_pos = osm_nodes.get(&junction_orig)
                .map_or((0.0, 0.0), |&(lat, lon)| latlon_to_mercator(lat, lon));

            let parent_chain = &route_game_nodes[owner_ri];
            if parent_chain.len() < 2 { continue; }

            // Find nearest segment on parent chain
            let (parent_seg_idx, t) = find_parent_segment(
                junction_pos, parent_chain, track_nodes, &node_idx,
            );
            let parent_node_id = parent_chain[parent_seg_idx].game_id;

            // Determine branch direction relative to parent
            let br_idx = node_idx[&branch_gid];
            let pi = node_idx[&parent_node_id];
            let next_idx = if parent_seg_idx + 1 < parent_chain.len() { parent_seg_idx + 1 }
                else if parent_seg_idx > 0 { parent_seg_idx - 1 }
                else { continue; };
            let qi = node_idx[&parent_chain[next_idx].game_id];
            let seg_dx = track_nodes[qi].x - track_nodes[pi].x;
            let seg_dy = track_nodes[qi].y - track_nodes[pi].y;

            let neighbor_gid = if is_start {
                if route_game_nodes[ri].len() > 1 { route_game_nodes[ri][1].game_id } else { continue; }
            } else {
                let len = route_game_nodes[ri].len();
                if len > 1 { route_game_nodes[ri][len - 2].game_id } else { continue; }
            };
            let ni = node_idx[&neighbor_gid];
            let br_dx = track_nodes[ni].x - track_nodes[br_idx].x;
            let br_dy = track_nodes[ni].y - track_nodes[br_idx].y;
            let dot = br_dx * seg_dx + br_dy * seg_dy;
            let dir = if dot >= 0.0 { 1 } else { -1 };

            // Nudge branch root away from parent
            let br_len = (br_dx * br_dx + br_dy * br_dy).sqrt().max(1e-10);
            track_nodes[br_idx].x += (br_dx / br_len) * BRANCH_OFFSET;
            track_nodes[br_idx].y += (br_dy / br_len) * BRANCH_OFFSET;

            // Set attachment fields
            track_nodes[br_idx].attached_to_id = parent_node_id;
            track_nodes[br_idx].attached_to_t = t;
            track_nodes[br_idx].attached_to_direction = Some(dir);

            let par_idx = node_idx[&parent_node_id];
            track_nodes[par_idx].attached_by.push(branch_gid);
        }
    }
}

fn find_parent_segment(
    junction_pos: (f64, f64),
    parent_chain: &[RouteNodeInfo],
    track_nodes: &[Track],
    node_idx: &HashMap<i64, usize>,
) -> (usize, f64) {
    let mut best_seg = 0usize;
    let mut best_t = 0.5f64;
    let mut best_dist = f64::MAX;

    for si in 0..parent_chain.len() - 1 {
        let pi = node_idx[&parent_chain[si].game_id];
        let qi = node_idx[&parent_chain[si + 1].game_id];
        let (px, py) = (track_nodes[pi].x, track_nodes[pi].y);
        let (qx, qy) = (track_nodes[qi].x, track_nodes[qi].y);
        let sx = qx - px;
        let sy = qy - py;
        let seg_len_sq = sx * sx + sy * sy;
        if seg_len_sq < 0.01 { continue; }

        let bx = junction_pos.0 - px;
        let by = junction_pos.1 - py;
        let t_raw = (bx * sx + by * sy) / seg_len_sq;
        let t = t_raw.clamp(0.0, 1.0);
        let proj_x = px + t * sx;
        let proj_y = py + t * sy;
        let perp = ((junction_pos.0 - proj_x).powi(2) + (junction_pos.1 - proj_y).powi(2)).sqrt();
        let overshoot = (t_raw - t_raw.clamp(0.0, 1.0)).abs() * seg_len_sq.sqrt();
        let d = perp + overshoot;

        if d < best_dist {
            best_dist = d;
            best_seg = si;
            best_t = t;
        }
    }

    // Adjust t near segment boundaries to avoid degenerate cases
    if best_t > 0.95 && best_seg + 1 < parent_chain.len() - 1 {
        (best_seg + 1, 0.5)
    } else if best_t < 0.05 && best_seg > 0 {
        (best_seg - 1, 0.5)
    } else {
        (best_seg, best_t.clamp(0.05, 0.95))
    }
}

fn serialize_to_nrclip(
    mut track_nodes: Vec<Track>,
    name: &str,
    track_kinds: Vec<(i32, TrackKind)>,
    mod_metas: Vec<ModMeta>,
) -> Result<Vec<u8>> {
    let cx = track_nodes.iter().map(|t| t.x).sum::<f64>() / track_nodes.len() as f64;
    let cy = track_nodes.iter().map(|t| t.y).sum::<f64>() / track_nodes.len() as f64;
    let center_lat = (cy / 6_378_137.0).sinh().atan();
    let cos_lat = center_lat.cos();
    for t in &mut track_nodes {
        t.x = (t.x - cx) * cos_lat;
        t.y = (t.y - cy) * cos_lat;
    }

    let name_hash = name.bytes().fold(0x0012_3456_7890_u64, |h, b| h.wrapping_mul(31).wrapping_add(u64::from(b)));
    let file = NrclipFile {
        version: MODEL_VERSION,
        collections: vec![Collection {
            id_a: name_hash,
            id_b: name_hash.wrapping_mul(7),
            name: name.to_string(),
            clips: vec![Clip {
                guid: name.to_string(),
                clip_id: name_hash.wrapping_mul(13),
                center_x: cx,
                center_y: cy,
                tracks: track_nodes,
                track_kinds,
                mod_metas,
                ..Clip::default()
            }],
            ..Collection::default()
        }],
    };

    file.to_bytes()
}

// ══════════════════════════════════════════════════════════════════════
// Geometry helpers
// ══════════════════════════════════════════════════════════════════════

fn latlon_to_mercator(lat: f64, lon: f64) -> (f64, f64) {
    let x = lon.to_radians() * 6_378_137.0;
    let y = (lat.to_radians() / 2.0 + std::f64::consts::FRAC_PI_4).tan().ln() * 6_378_137.0;
    (x, y)
}

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
