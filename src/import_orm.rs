/// Import OpenRailwayMap tracks into a Nimby Rails blueprint.
///
/// Pipeline:
/// 1. Load Overpass JSON → OSM nodes + ways
/// 2. Merge ways into continuous routes through shared endpoints
/// 3. Identify junction points (where routes branch)
/// 4. Simplify routes to tangent-mode control points (direction changes + max spacing)
/// 5. For branches: compute attached_to_t along parent route segment
/// 6. Generate .nrclip with proper junction topology

use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::fs;

mod encode;
mod wyhash_nrc1;
mod nrclip;

use encode::PayloadWriter;

const MODEL_VERSION: u32 = 226;
const MAX_SPACING: f64 = 300.0;
const ANGLE_THRESHOLD: f64 = 1.0 * std::f64::consts::PI / 180.0; // 1 degree — keep more curve detail

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let json_path = args.get(1).context("usage: import_orm <tracks.json> [output.nrclip]")?;
    let output = args.get(2).map(|s| s.as_str()).unwrap_or("orm_import.nrclip");

    let raw = fs::read_to_string(json_path).context("read JSON")?;
    let data: serde_json::Value = serde_json::from_str(&raw).context("parse JSON")?;
    let elements = data["elements"].as_array().context("no elements")?;

    // Parse OSM nodes and ways
    let mut osm_nodes: HashMap<u64, (f64, f64)> = HashMap::new();
    let mut ways: Vec<Vec<u64>> = Vec::new();
    for e in elements {
        match e["type"].as_str() {
            Some("node") => {
                let id = e["id"].as_u64().unwrap();
                osm_nodes.insert(id, (e["lat"].as_f64().unwrap(), e["lon"].as_f64().unwrap()));
            }
            Some("way") => {
                let nids: Vec<u64> = e["nodes"].as_array().unwrap()
                    .iter().map(|n| n.as_u64().unwrap()).collect();
                if nids.len() >= 2 { ways.push(nids); }
            }
            _ => {}
        }
    }
    println!("Loaded {} OSM nodes, {} ways", osm_nodes.len(), ways.len());

    // Build node→way index
    let mut node_ways: HashMap<u64, Vec<(usize, usize)>> = HashMap::new(); // nid → [(way_idx, pos)]
    for (wi, way) in ways.iter().enumerate() {
        for (pi, &nid) in way.iter().enumerate() {
            node_ways.entry(nid).or_default().push((wi, pi));
        }
    }

    // Classify shared nodes: continuations vs junctions
    let mut continuations: HashSet<u64> = HashSet::new();
    let mut junction_nodes: HashSet<u64> = HashSet::new();
    for (&nid, refs) in &node_ways {
        let n_ways = refs.iter().map(|&(wi,_)| wi).collect::<HashSet<_>>().len();
        if n_ways < 2 { continue; }
        let all_endpoints = refs.iter().all(|&(wi, pi)| pi == 0 || pi == ways[wi].len() - 1);
        if n_ways == 2 && all_endpoints {
            continuations.insert(nid);
        } else {
            junction_nodes.insert(nid);
        }
    }
    println!("{} continuations, {} junctions", continuations.len(), junction_nodes.len());

    // Merge ways into routes through continuation points
    let mut way_used = vec![false; ways.len()];
    let mut routes: Vec<Vec<u64>> = Vec::new();

    for start_wi in 0..ways.len() {
        if way_used[start_wi] { continue; }
        way_used[start_wi] = true;
        let mut route = ways[start_wi].clone();

        // Extend forward (only if direction is consistent — no near-reversals)
        loop {
            let last = *route.last().unwrap();
            if !continuations.contains(&last) { break; }
            let next = node_ways[&last].iter()
                .find(|&&(wi, _)| !way_used[wi]);
            match next {
                Some(&(wi, pi)) => {
                    let candidate: Vec<u64> = if pi == 0 {
                        ways[wi][1..].to_vec()
                    } else {
                        ways[wi][..ways[wi].len()-1].iter().rev().copied().collect()
                    };
                    // Check direction consistency: does this create a near-reversal?
                    if route.len() >= 2 && !candidate.is_empty() {
                        let a = &osm_nodes[route.get(route.len()-2).unwrap()];
                        let b = &osm_nodes[&last];
                        let c = &osm_nodes[&candidate[0]];
                        let h1 = (b.0 - a.0).atan2(b.1 - a.1);
                        let h2 = (c.0 - b.0).atan2(c.1 - b.1);
                        let mut diff = (h2 - h1).abs();
                        if diff > std::f64::consts::PI { diff = 2.0 * std::f64::consts::PI - diff; }
                        if diff > 2.5 { break; } // >143° = likely not a continuation
                    }
                    way_used[wi] = true;
                    route.extend_from_slice(&candidate);
                }
                None => break,
            }
        }
        // Extend backward (only if direction is consistent)
        loop {
            let first = route[0];
            if !continuations.contains(&first) { break; }
            let prev = node_ways[&first].iter()
                .find(|&&(wi, _)| !way_used[wi]);
            match prev {
                Some(&(wi, pi)) => {
                    let mut prefix: Vec<u64> = if pi == ways[wi].len() - 1 {
                        ways[wi][..ways[wi].len()-1].to_vec()
                    } else {
                        ways[wi][1..].iter().rev().copied().collect()
                    };
                    // Check direction consistency
                    if route.len() >= 2 && !prefix.is_empty() {
                        let a = &osm_nodes[prefix.last().unwrap()];
                        let b = &osm_nodes[&first];
                        let c = &osm_nodes[route.get(1).unwrap()];
                        let h1 = (b.0 - a.0).atan2(b.1 - a.1);
                        let h2 = (c.0 - b.0).atan2(c.1 - b.1);
                        let mut diff = (h2 - h1).abs();
                        if diff > std::f64::consts::PI { diff = 2.0 * std::f64::consts::PI - diff; }
                        if diff > 2.5 { break; }
                    }
                    way_used[wi] = true;
                    prefix.push(first); // re-add the shared node
                    prefix.extend_from_slice(&route[1..]); // skip duplicate
                    route = prefix;
                }
                None => break,
            }
        }
        routes.push(route);
    }

    // Sort routes by length (longest = most important = through-routes)
    routes.sort_by(|a, b| b.len().cmp(&a.len()));
    println!("Merged into {} routes", routes.len());

    // Convert routes to Mercator coordinates
    let route_coords: Vec<Vec<(f64, f64)>> = routes.iter().map(|route| {
        route.iter().filter_map(|nid| {
            osm_nodes.get(nid).map(|&(lat, lon)| latlon_to_mercator(lat, lon))
        }).collect()
    }).collect();

    // Simplify routes: keep endpoints, junctions, direction changes, max spacing
    let simplified: Vec<Vec<(usize, f64, f64)>> = routes.iter().zip(route_coords.iter())
        .map(|(route, coords)| {
            let mut keep = vec![false; coords.len()];
            keep[0] = true;
            *keep.last_mut().unwrap() = true;

            // Keep junction nodes AND all nodes within 100m of a junction
            // (prevents Hobby spline overshoot near junctions)
            let mut junction_indices: Vec<usize> = Vec::new();
            for (i, &nid) in route.iter().enumerate() {
                if junction_nodes.contains(&nid) {
                    keep[i] = true;
                    junction_indices.push(i);
                }
            }
            for &ji in &junction_indices {
                let jx = coords[ji].0;
                let jy = coords[ji].1;
                for i in 0..coords.len() {
                    let dx = coords[i].0 - jx;
                    let dy = coords[i].1 - jy;
                    if dx * dx + dy * dy < 100.0 * 100.0 {
                        keep[i] = true;
                    }
                }
            }

            // Keep at direction changes and max spacing
            let mut last_kept = 0;
            let mut last_heading: Option<f64> = None;
            for i in 1..coords.len() {
                if keep[i] { last_kept = i; last_heading = None; continue; }
                let dx = coords[i].0 - coords[last_kept].0;
                let dy = coords[i].1 - coords[last_kept].1;
                let dist = (dx * dx + dy * dy).sqrt();
                let heading = dy.atan2(dx);
                let angle_change = last_heading.map_or(0.0, |lh| {
                    let d = (heading - lh).abs();
                    if d > std::f64::consts::PI { 2.0 * std::f64::consts::PI - d } else { d }
                });
                if dist >= MAX_SPACING || angle_change >= ANGLE_THRESHOLD {
                    keep[i] = true;
                    last_kept = i;
                    last_heading = Some(heading);
                } else if last_heading.is_none() {
                    last_heading = Some(heading);
                }
            }

            coords.iter().enumerate()
                .filter(|(i, _)| keep[*i])
                .map(|(i, &(x, y))| (i, x, y))
                .collect()
        }).collect();

    let total_before: usize = route_coords.iter().map(|c| c.len()).sum();
    let total_after: usize = simplified.iter().map(|s| s.len()).sum();
    println!("Simplified: {} → {} nodes", total_before, total_after);

    // Build game track nodes
    let mut track_nodes: Vec<TrackNode> = Vec::new();
    let mut node_id_counter: i64 = 100;

    // For each route, create a chain of game nodes
    // Track which game node corresponds to each (route_idx, original_osm_idx) for branch attachment
    let mut route_game_nodes: Vec<Vec<RouteNodeInfo>> = Vec::new();

    // Also map junction OSM nodes to (route_idx, node position in simplified route)
    // so branches can find their parent
    let mut junction_to_route: HashMap<u64, (usize, usize)> = HashMap::new(); // osm_nid → (route_idx, simplified_pos)

    for (ri, simp) in simplified.iter().enumerate() {
        let mut chain: Vec<RouteNodeInfo> = Vec::new();

        for (si, &(orig_idx, x, y)) in simp.iter().enumerate() {
            let gid = node_id_counter;
            node_id_counter += 100;
            let prev = if si > 0 { chain[si - 1].game_id } else { 0 };
            track_nodes.push(TrackNode {
                id: gid, x, y, layer: 0,
                prev,
                next: 0, // filled in next iteration
                tangential: 1,  // tangent mode for through-routes
                tangent_delta: 0.0,
                attached_to_id: 0, attached_to_t: 0.0, attached_to_dir: 0,
                attached_by: Vec::new(),
            });
            // Set previous node's next
            if si > 0 {
                let prev_idx = track_nodes.len() - 2;
                track_nodes[prev_idx].next = gid;
            }
            chain.push(RouteNodeInfo { game_id: gid, orig_idx });

            // If this is a junction node, register it
            let osm_nid = routes[ri][orig_idx];
            if junction_nodes.contains(&osm_nid) {
                junction_to_route.entry(osm_nid).or_insert((ri, si));
            }
        }
        route_game_nodes.push(chain);
    }

    // Now handle branches: for each route that starts or ends at a junction node,
    // if that junction is owned by a DIFFERENT route, create an attachment.
    for (ri, simp) in simplified.iter().enumerate() {
        if simp.is_empty() { continue; }

        // Check start of route
        let start_osm = routes[ri][simp[0].0];
        if junction_nodes.contains(&start_osm) {
            if let Some(&(parent_ri, parent_si)) = junction_to_route.get(&start_osm) {
                if parent_ri != ri {
                    // This route starts at a junction owned by another route
                    // Find the parent segment and compute att_t
                    let parent_chain = &route_game_nodes[parent_ri];
                    let parent_node_id = parent_chain[parent_si].game_id;
                    let branch_node_idx = route_game_nodes[ri][0].game_id;

                    // Compute att_t: fraction along the parent segment
                    // The parent segment goes from parent_si to parent_si+1 (or parent_si-1)
                    let (att_t, att_dir) = compute_att_t(
                        &track_nodes, parent_chain, parent_si,
                        &route_coords[ri][simp[0].0],
                    );

                    // Set attachment on the branch's first node
                    let br_idx = track_nodes.iter().position(|n| n.id == branch_node_idx).unwrap();
                    track_nodes[br_idx].attached_to_id = parent_node_id;
                    track_nodes[br_idx].attached_to_t = att_t;
                    track_nodes[br_idx].attached_to_dir = att_dir;
                    track_nodes[br_idx].tangential = 0; // point mode for branch roots

                    // Add to parent's attached_by
                    let par_idx = track_nodes.iter().position(|n| n.id == parent_node_id).unwrap();
                    track_nodes[par_idx].attached_by.push(branch_node_idx);
                }
            }
        }

        // Check end of route
        let end_osm = routes[ri][simp.last().unwrap().0];
        if junction_nodes.contains(&end_osm) {
            if let Some(&(parent_ri, parent_si)) = junction_to_route.get(&end_osm) {
                if parent_ri != ri {
                    let parent_chain = &route_game_nodes[parent_ri];
                    let parent_node_id = parent_chain[parent_si].game_id;
                    let branch_node_id = route_game_nodes[ri].last().unwrap().game_id;

                    let (att_t, att_dir) = compute_att_t(
                        &track_nodes, parent_chain, parent_si,
                        &route_coords[ri][simp.last().unwrap().0],
                    );

                    let br_idx = track_nodes.iter().position(|n| n.id == branch_node_id).unwrap();
                    track_nodes[br_idx].attached_to_id = parent_node_id;
                    track_nodes[br_idx].attached_to_t = att_t;
                    track_nodes[br_idx].attached_to_dir = att_dir;
                    track_nodes[br_idx].tangential = 0;

                    let par_idx = track_nodes.iter().position(|n| n.id == parent_node_id).unwrap();
                    track_nodes[par_idx].attached_by.push(branch_node_id);
                }
            }
        }
    }

    let n_branches = track_nodes.iter().filter(|n| n.attached_to_id != 0).count();
    let n_junctions = track_nodes.iter().filter(|n| !n.attached_by.is_empty()).count();
    println!("Created {} track nodes, {} branches, {} junction nodes",
             track_nodes.len(), n_branches, n_junctions);

    // Compute center and convert to ground meters
    let cx = track_nodes.iter().map(|t| t.x).sum::<f64>() / track_nodes.len() as f64;
    let cy = track_nodes.iter().map(|t| t.y).sum::<f64>() / track_nodes.len() as f64;
    let center_lat = (cy / 6_378_137.0).sinh().atan();
    let cos_lat = center_lat.cos();
    println!("Center: ({:.2}, {:.2}), cos(lat)={:.6}", cx, cy, cos_lat);
    for t in &mut track_nodes {
        t.x = (t.x - cx) * cos_lat;
        t.y = (t.y - cy) * cos_lat;
    }

    // Build payload
    let payload = build_payload(&track_nodes, cx, cy)?;
    println!("Payload: {} bytes", payload.len());

    let compressed = {
        let mut enc = zstd::stream::Encoder::new(Vec::new(), 3)?;
        enc.include_contentsize(true)?;
        enc.set_pledged_src_size(Some(payload.len() as u64))?;
        std::io::copy(&mut payload.as_slice(), &mut enc)?;
        enc.finish()?
    };
    let checksum = wyhash_nrc1::checksum(&payload);

    let mut file_data = Vec::new();
    file_data.extend_from_slice(b"NRC1");
    file_data.extend_from_slice(&MODEL_VERSION.to_le_bytes());
    file_data.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    file_data.extend_from_slice(&(compressed.len() as u64).to_le_bytes());
    file_data.extend_from_slice(&checksum.to_le_bytes());
    file_data.extend_from_slice(&compressed);

    fs::write(output, &file_data)?;
    println!("Wrote {} bytes to {}", file_data.len(), output);

    let decoded = nrclip::parse_payload(&payload, MODEL_VERSION)?;
    let total: usize = decoded.iter().flat_map(|c| &c.clips).map(|c| c.tracks.len()).sum();
    println!("Verified: {} tracks", total);
    Ok(())
}

struct RouteNodeInfo {
    game_id: i64,
    orig_idx: usize,
}

/// Compute attached_to_t: where along the parent's segment does the branch connect?
/// Returns (att_t, att_dir).
fn compute_att_t(
    tracks: &[TrackNode],
    parent_chain: &[RouteNodeInfo],
    parent_si: usize,
    branch_pos: &(f64, f64),
) -> (f64, i32) {
    // Try forward segment (parent_si → parent_si+1)
    if parent_si + 1 < parent_chain.len() {
        let p = tracks.iter().find(|n| n.id == parent_chain[parent_si].game_id).unwrap();
        let q = tracks.iter().find(|n| n.id == parent_chain[parent_si + 1].game_id).unwrap();
        let sx = q.x - p.x;
        let sy = q.y - p.y;
        let seg_len_sq = sx * sx + sy * sy;
        if seg_len_sq > 0.001 {
            let bx = branch_pos.0 - p.x;
            let by = branch_pos.1 - p.y;
            let t = (bx * sx + by * sy) / seg_len_sq;
            let t = t.clamp(0.01, 0.99);
            return (t, 1);
        }
    }
    // Try backward segment (parent_si → parent_si-1)
    if parent_si > 0 {
        let p = tracks.iter().find(|n| n.id == parent_chain[parent_si].game_id).unwrap();
        let q = tracks.iter().find(|n| n.id == parent_chain[parent_si - 1].game_id).unwrap();
        let sx = q.x - p.x;
        let sy = q.y - p.y;
        let seg_len_sq = sx * sx + sy * sy;
        if seg_len_sq > 0.001 {
            let bx = branch_pos.0 - p.x;
            let by = branch_pos.1 - p.y;
            let t = (bx * sx + by * sy) / seg_len_sq;
            let t = t.clamp(0.01, 0.99);
            return (t, -1);
        }
    }
    (0.5, 1) // fallback
}

struct TrackNode {
    id: i64,
    x: f64,
    y: f64,
    layer: i32,
    prev: i64,
    next: i64,
    tangential: u8,
    tangent_delta: f32,
    attached_to_id: i64,
    attached_to_t: f64,
    attached_to_dir: i32,
    attached_by: Vec<i64>,
}

fn latlon_to_mercator(lat: f64, lon: f64) -> (f64, f64) {
    let x = lon.to_radians() * 6_378_137.0;
    let y = (lat.to_radians() / 2.0 + std::f64::consts::FRAC_PI_4).tan().ln() * 6_378_137.0;
    (x, y)
}

fn build_payload(tracks: &[TrackNode], center_x: f64, center_y: f64) -> Result<Vec<u8>> {
    let mut w = PayloadWriter::new();

    w.write_varint(1);
    w.write_varint(7777777777u64);
    w.write_varint(8888888888u64);
    w.write_optional_mod_source(&None);
    w.write_string("ORM Import");

    w.write_varint(1);
    w.write_string("orm-import");
    w.write_varint(0x08120001u64);
    w.write_f64(center_x);
    w.write_f64(center_y);

    w.write_varint(tracks.len() as u64);
    for t in tracks {
        w.write_i64z(t.id);
        w.write_raw_u8(1);
        w.write_i32z(0);
        w.write_i32z(t.layer);
        w.write_raw_u8(1);            // winding
        w.write_i64z(t.prev);
        w.write_i64z(t.next);
        w.write_i64z(0);
        w.write_f32(0.0);
        w.write_f64(t.x);
        w.write_f64(t.y);
        w.write_f32(t.tangent_delta);
        w.write_f32(0.5);
        w.write_i64z(0);
        w.write_i32z(0);
        w.write_string("");
        w.write_raw_u8(0);
        w.write_raw_u8(0);            // straight
        w.write_raw_u8(t.tangential);
        w.write_raw_u8(if t.tangential == 1 { 1 } else { 0 }); // limited_shapes for tangent mode
        for _ in 0..4 { w.write_varint(0); }
        w.write_vec_set_i64(&[]);
        w.write_i64z(t.attached_to_id);
        w.write_f64(t.attached_to_t);
        w.write_i32z(t.attached_to_dir);
        w.write_vec_set_i64(&t.attached_by);
        w.write_vec_set_i64(&[]);
        w.write_i64z(0);
        w.write_i64z(0);
        w.write_f32(0.0);
        w.write_i32z(0);
        w.write_f32(0.0);
        w.write_f32(0.0);
        w.write_vec_set_i64(&[]);
        w.write_f32(0.0);
    }

    w.write_varint(0); // signals
    w.write_varint(0); // station_groups
    w.write_varint(0); // buildings
    w.write_varint(0); // track_kinds
    w.write_varint(0); // building_kinds
    w.write_varint(0); // demands
    w.write_varint(0); // mod_metas

    Ok(w.into_bytes())
}
