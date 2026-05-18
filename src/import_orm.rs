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
mod hobby;
mod wyhash_nrc1;
mod nrclip;

use encode::PayloadWriter;

const MODEL_VERSION: u32 = 226;
const MAX_SPACING: f64 = 200.0;

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

    // Identify shared/junction nodes
    let mut shared_nodes: HashSet<u64> = HashSet::new();
    let mut junction_nodes: HashSet<u64> = HashSet::new();
    for (&nid, refs) in &node_ways {
        let n_ways = refs.iter().map(|&(wi,_)| wi).collect::<HashSet<_>>().len();
        if n_ways >= 2 { shared_nodes.insert(nid); }
        if n_ways >= 3 || (n_ways == 2 && refs.iter().any(|&(wi, pi)| pi > 0 && pi < ways[wi].len() - 1)) {
            junction_nodes.insert(nid);
        }
    }

    // Merge ways into routes. Through-routes extend through shared nodes
    // (including junctions) by picking the MOST ALIGNED continuation.
    // This makes junctions interior to through-routes so branches can
    // attach mid-segment with proper attached_to_t.
    let mut way_used = vec![false; ways.len()];
    let mut routes: Vec<Vec<u64>> = Vec::new();

    // Process longest ways first to build through-routes
    let mut way_order: Vec<usize> = (0..ways.len()).collect();
    way_order.sort_by(|&a, &b| ways[b].len().cmp(&ways[a].len()));

    for &start_wi in &way_order {
        if way_used[start_wi] { continue; }
        way_used[start_wi] = true;
        let mut route = ways[start_wi].clone();

        // Extend forward — at each shared node, pick most aligned unused way
        loop {
            let last = *route.last().unwrap();
            if !shared_nodes.contains(&last) { break; }
            let cur_heading = if route.len() >= 2 {
                let a = &osm_nodes[&route[route.len()-2]];
                let b = &osm_nodes[&last];
                (b.0 - a.0).atan2(b.1 - a.1)
            } else { 0.0 };

            // Find best-aligned unused way. The continuation way's OUTGOING
            // direction should match our current heading (not be opposite).
            let mut best: Option<(usize, usize, f64)> = None;
            for &(wi, pi) in &node_ways[&last] {
                if way_used[wi] { continue; }
                // The continuation will be appended after the shared node.
                // Its first node after the shared node determines the outgoing direction.
                let cont_first = if pi == 0 { ways[wi].get(1) } else { ways[wi].len().checked_sub(2).and_then(|i| ways[wi].get(i)) };
                let Some(&cont_nid) = cont_first else { continue };
                let c = &osm_nodes[&cont_nid];
                let b = &osm_nodes[&last];
                // Heading FROM shared node TOWARD the continuation
                let h = (c.0 - b.0).atan2(c.1 - b.1);
                let mut diff = (h - cur_heading).abs();
                if diff > std::f64::consts::PI { diff = 2.0 * std::f64::consts::PI - diff; }
                if best.is_none() || diff < best.unwrap().2 { best = Some((wi, pi, diff)); }
            }
            let Some((wi, pi, diff)) = best else { break };
            if diff > 2.5 { break; } // >143° = near-reversal, not a continuation

            way_used[wi] = true;
            if pi == 0 { route.extend_from_slice(&ways[wi][1..]); }
            else { route.extend(ways[wi][..ways[wi].len()-1].iter().rev()); }
        }

        // Extend backward
        loop {
            let first = route[0];
            if !shared_nodes.contains(&first) { break; }
            let cur_heading = if route.len() >= 2 {
                let a = &osm_nodes[&route[1]];
                let b = &osm_nodes[&first];
                (b.0 - a.0).atan2(b.1 - a.1)
            } else { 0.0 };

            let mut best: Option<(usize, usize, f64)> = None;
            for &(wi, pi) in &node_ways[&first] {
                if way_used[wi] { continue; }
                // Heading FROM the shared node TOWARD the prepended way's interior
                let prev_nid = if pi == ways[wi].len()-1 { ways[wi].len().checked_sub(2).and_then(|i| ways[wi].get(i)) } else { ways[wi].get(1) };
                let Some(&prev_nid) = prev_nid else { continue };
                let c = &osm_nodes[&prev_nid];
                let b = &osm_nodes[&first];
                let h = (c.0 - b.0).atan2(c.1 - b.1);
                let mut diff = (h - cur_heading).abs();
                if diff > std::f64::consts::PI { diff = 2.0 * std::f64::consts::PI - diff; }
                if best.is_none() || diff < best.unwrap().2 { best = Some((wi, pi, diff)); }
            }
            let Some((wi, pi, diff)) = best else { break };
            if diff > 2.5 { break; }

            way_used[wi] = true;
            let mut prefix: Vec<u64> = if pi == ways[wi].len()-1 { ways[wi][..ways[wi].len()-1].to_vec() }
                else { ways[wi][1..].iter().rev().copied().collect() };
            prefix.push(first);
            prefix.extend_from_slice(&route[1..]);
            route = prefix;
        }

        routes.push(route);
    }

    routes.sort_by(|a, b| b.len().cmp(&a.len()));
    let interior_junctions: usize = routes.iter().map(|r| r[1..r.len().saturating_sub(1)].iter().filter(|n| junction_nodes.contains(n)).count()).sum();
    println!("Merged into {} routes ({} interior junctions)", routes.len(), interior_junctions);

    // Convert routes to Mercator coordinates
    let route_coords: Vec<Vec<(f64, f64)>> = routes.iter().map(|route| {
        route.iter().filter_map(|nid| {
            osm_nodes.get(nid).map(|&(lat, lon)| latlon_to_mercator(lat, lon))
        }).collect()
    }).collect();

    // Compute junction ownership: first (longest) route through each junction owns it.
    // Other routes will branch from it. Through-routes must avoid nodes near foreign junctions.
    let mut junction_owner: HashMap<u64, usize> = HashMap::new(); // osm_nid → route_idx
    for (ri, route) in routes.iter().enumerate() {
        for &nid in route {
            if junction_nodes.contains(&nid) {
                junction_owner.entry(nid).or_insert(ri); // first route wins (longest)
            }
        }
    }
    // For each route, collect junction positions where OTHER routes will branch from it.
    // The owning route must avoid nodes near these so branches get mid-segment attached_to_t.
    let branch_junctions: Vec<Vec<(f64, f64)>> = routes.iter().enumerate().map(|(ri, route)| {
        route.iter().enumerate().filter_map(|(i, &nid)| {
            // This route OWNS this junction AND other routes also pass through it
            if junction_nodes.contains(&nid) && junction_owner.get(&nid) == Some(&ri) {
                Some(route_coords[ri][i])
            } else {
                None
            }
        }).collect()
    }).collect();

    // Simplify routes
    let simplified: Vec<Vec<(usize, f64, f64)>> = routes.iter().zip(route_coords.iter()).enumerate()
        .map(|(ri, (route, coords))| {
            let mut keep = vec![false; coords.len()];
            keep[0] = true;
            *keep.last_mut().unwrap() = true;

            // DON'T keep junction nodes on through-routes — branches attach mid-segment.
            // But DO keep a node close (~30m) to each endpoint that's at a junction,
            // so the branch spline stays tight near the switch instead of curving early.
            let start_is_junction = junction_nodes.contains(&route[0]);
            let end_is_junction = junction_nodes.contains(route.last().unwrap());
            if start_is_junction && coords.len() > 2 {
                // Find the first node that's ~30m from the start
                for i in 1..coords.len() - 1 {
                    let dx = coords[i].0 - coords[0].0;
                    let dy = coords[i].1 - coords[0].1;
                    if dx * dx + dy * dy >= 30.0 * 30.0 {
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
                    if dx * dx + dy * dy >= 30.0 * 30.0 {
                        keep[i] = true;
                        break;
                    }
                }
            }

            // Enforce max spacing (but not near foreign junctions)
            let mut last_kept = 0;
            for i in 1..coords.len() {
                if keep[i] { last_kept = i; continue; }
                let dx = coords[i].0 - coords[last_kept].0;
                let dy = coords[i].1 - coords[last_kept].1;
                if dx * dx + dy * dy >= MAX_SPACING * MAX_SPACING {
                    let near_foreign = branch_junctions[ri].iter().any(|&(fx, fy)| {
                        let ddx = coords[i].0 - fx;
                        let ddy = coords[i].1 - fy;
                        ddx * ddx + ddy * ddy < 50.0 * 50.0
                    });
                    if !near_foreign {
                        keep[i] = true;
                        last_kept = i;
                    }
                }
            }

            // Spline-first simplification: start with just endpoints/junctions/spacing,
            // then iteratively add nodes only where the SPLINE deviates from the
            // original OSM polyline. Straight sections need 0 extra nodes (spline
            // through 2 aligned points = straight). Curves get the minimum nodes
            // needed for the spline to track within tolerance.
            for _ in 0..20 {
                let kept_pts: Vec<(f64, f64)> = (0..coords.len())
                    .filter(|&i| keep[i]).map(|i| coords[i]).collect();
                let kept_idx: Vec<usize> = (0..coords.len())
                    .filter(|&i| keep[i]).collect();

                if kept_pts.len() < 2 { break; }
                let segs = hobby::hobby_spline(&kept_pts, 0.0);
                let mut added = false;

                for (si, seg) in segs.iter().enumerate() {
                    let orig_start = kept_idx[si];
                    let orig_end = kept_idx[si + 1];
                    if orig_end - orig_start <= 1 { continue; }

                    // Find the original node that deviates most from this spline segment
                    let mut worst_dev = 0.0f64;
                    let mut worst_orig = orig_start;
                    for oi in (orig_start + 1)..orig_end {
                        let (ox, oy) = coords[oi];
                        let mut best_d = f64::MAX;
                        for s in 0..=32 {
                            let pt = hobby::bezier_point(seg, s as f64 / 32.0);
                            let d = ((ox - pt.0).powi(2) + (oy - pt.1).powi(2)).sqrt();
                            if d < best_d { best_d = d; }
                        }
                        if best_d > worst_dev {
                            worst_dev = best_d;
                            worst_orig = oi;
                        }
                    }

                    if worst_dev > 1.0 {
                        // Don't add nodes near foreign junctions — branches attach mid-segment there
                        let near_foreign = branch_junctions[ri].iter().any(|&(fx, fy)| {
                            let dx = coords[worst_orig].0 - fx;
                            let dy = coords[worst_orig].1 - fy;
                            dx * dx + dy * dy < 50.0 * 50.0
                        });
                        if !near_foreign {
                            keep[worst_orig] = true;
                            added = true;
                        }
                    }
                }

                if !added { break; }
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
                tangential: 0,  // point mode: nodes ARE on the track
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

            // Register junction nodes with their original coordinate index
            let osm_nid = routes[ri][orig_idx];
            if junction_nodes.contains(&osm_nid) {
                junction_to_route.entry(osm_nid).or_insert((ri, orig_idx));
            }
        }
        route_game_nodes.push(chain);
    }

    // Register ALL junction nodes across ALL routes (not just simplified ones).
    // Some junctions are excluded from simplified chains by the exclusion zone,
    // but branches still need to find them for attachment.
    for (ri, route) in routes.iter().enumerate() {
        for (i, &nid) in route.iter().enumerate() {
            if junction_nodes.contains(&nid) {
                junction_to_route.entry(nid).or_insert((ri, i));
            }
        }
    }

    // Handle branches: for each route that starts or ends at a junction node
    // owned by a DIFFERENT route, attach it mid-segment on the parent route.
    // The branch root sits at the junction position (where tracks diverge).
    // attached_to_id points to the parent node BEFORE the junction,
    // attached_to_t is the fraction along that segment where the junction falls.
    for (ri, simp) in simplified.iter().enumerate() {
        if simp.is_empty() { continue; }

        for &is_start in &[true, false] {
            let endpoint_orig_idx = if is_start { simp[0].0 } else { simp.last().unwrap().0 };
            let endpoint_osm = routes[ri][endpoint_orig_idx];
            if !junction_nodes.contains(&endpoint_osm) { continue; }

            let Some(&(parent_ri, junction_orig_idx)) = junction_to_route.get(&endpoint_osm) else { continue };
            if parent_ri == ri || simp.len() < 2 { continue; }

            // Find which simplified segment of the parent contains the junction point
            let parent_chain = &route_game_nodes[parent_ri];
            let parent_simp = &simplified[parent_ri];
            let junction_pos = route_coords[parent_ri][junction_orig_idx];

            // Search parent's simplified segments for the one containing the junction
            let mut best_seg = 0usize;
            let mut best_t = 0.5f64;
            let mut best_dist = f64::MAX;
            for si in 0..parent_simp.len().saturating_sub(1) {
                let (_, ax, ay) = parent_simp[si];
                let (_, bx, by) = parent_simp[si + 1];
                let sx = bx - ax;
                let sy = by - ay;
                let len_sq = sx * sx + sy * sy;
                if len_sq < 1e-10 { continue; }
                let t = ((junction_pos.0 - ax) * sx + (junction_pos.1 - ay) * sy) / len_sq;
                let t = t.clamp(0.01, 0.99);
                let px = ax + t * sx;
                let py = ay + t * sy;
                let d = ((junction_pos.0 - px).powi(2) + (junction_pos.1 - py).powi(2)).sqrt();
                if d < best_dist {
                    best_dist = d;
                    best_seg = si;
                    best_t = t;
                }
            }

            let parent_node_id = parent_chain[best_seg].game_id;
            let branch_node_id = if is_start {
                route_game_nodes[ri][0].game_id
            } else {
                route_game_nodes[ri].last().unwrap().game_id
            };

            // Branch root position = the junction point (where tracks diverge)
            let br_idx = track_nodes.iter().position(|n| n.id == branch_node_id).unwrap();
            track_nodes[br_idx].x = junction_pos.0;
            track_nodes[br_idx].y = junction_pos.1;
            track_nodes[br_idx].attached_to_id = parent_node_id;
            track_nodes[br_idx].attached_to_t = best_t;
            track_nodes[br_idx].attached_to_dir = 1; // game derives direction geometrically
            track_nodes[br_idx].tangential = 0;

            let par_idx = track_nodes.iter().position(|n| n.id == parent_node_id).unwrap();
            track_nodes[par_idx].attached_by.push(branch_node_id);
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

    // Run comparison (prints deviation stats + renders overlay)
    match std::process::Command::new("cargo")
        .args(["run", "--bin", "compare_orm", "--", output, json_path])
        .status() {
        Ok(s) if s.success() => {},
        _ => eprintln!("Warning: compare_orm failed"),
    }

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

/// Douglas-Peucker polyline simplification. Marks points to keep in `keep[]`.
/// Recursively finds the point farthest from the line between start and end;
/// if it exceeds `tolerance`, keeps it and recurses on both halves.
fn douglas_peucker(coords: &[(f64, f64)], keep: &mut [bool], start: usize, end: usize, tolerance: f64) {
    if end <= start + 1 { return; }

    let (ax, ay) = coords[start];
    let (bx, by) = coords[end];
    let dx = bx - ax;
    let dy = by - ay;
    let len_sq = dx * dx + dy * dy;

    let mut max_dist = 0.0f64;
    let mut max_idx = start;

    for i in (start + 1)..end {
        // Skip already-kept nodes (junctions) — they split the recursion naturally
        let dist = if len_sq < 1e-10 {
            let px = coords[i].0 - ax;
            let py = coords[i].1 - ay;
            (px * px + py * py).sqrt()
        } else {
            let t = ((coords[i].0 - ax) * dx + (coords[i].1 - ay) * dy) / len_sq;
            let t = t.clamp(0.0, 1.0);
            let proj_x = ax + t * dx;
            let proj_y = ay + t * dy;
            ((coords[i].0 - proj_x).powi(2) + (coords[i].1 - proj_y).powi(2)).sqrt()
        };
        if dist > max_dist {
            max_dist = dist;
            max_idx = i;
        }
    }

    if max_dist > tolerance {
        keep[max_idx] = true;
        douglas_peucker(coords, keep, start, max_idx, tolerance);
        douglas_peucker(coords, keep, max_idx, end, tolerance);
    }
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
        w.write_raw_u8(0);            // limited_shapes
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
