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

use crate::hobby;
use crate::nrc1::NrclipFile;
use crate::types::{Collection, Clip, Track};

const MODEL_VERSION: u32 = 226;
const MAX_SPACING: f64 = 200.0;

/// Import OpenRailwayMap Overpass JSON into a Nimby Rails .nrclip file.
/// Returns the raw file bytes ready to write to disk.
pub fn import_orm(json: &str, name: &str) -> Result<Vec<u8>> {
    let data: serde_json::Value = serde_json::from_str(json).context("parse JSON")?;
    let elements = data["elements"].as_array().context("no elements")?;
    let blueprint_name = name.to_string();

    // Parse OSM nodes and ways, including layer tags for elevation
    let mut osm_nodes: HashMap<u64, (f64, f64)> = HashMap::new();
    let mut ways: Vec<Vec<u64>> = Vec::new();
    let mut way_layers: Vec<i32> = Vec::new(); // OSM layer tag per way
    let mut node_layer: HashMap<u64, i32> = HashMap::new(); // node → layer
    for e in elements {
        match e["type"].as_str() {
            Some("node") => {
                let id = e["id"].as_u64().unwrap();
                osm_nodes.insert(id, (e["lat"].as_f64().unwrap(), e["lon"].as_f64().unwrap()));
            }
            Some("way") => {
                let nids: Vec<u64> = e["nodes"].as_array().unwrap()
                    .iter().map(|n| n.as_u64().unwrap()).collect();
                let layer: i32 = e.get("tags")
                    .and_then(|t| t.get("layer"))
                    .and_then(|l| l.as_str())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                if nids.len() >= 2 {
                    // Assign layer to all nodes in this way
                    for &nid in &nids {
                        node_layer.entry(nid)
                            .and_modify(|existing| {
                                // Keep the layer with highest absolute value
                                if layer.abs() > existing.abs() { *existing = layer; }
                            })
                            .or_insert(layer);
                    }
                    way_layers.push(layer);
                    ways.push(nids);
                }
            }
            _ => {}
        }
    }
    let n_elevated: usize = node_layer.values().filter(|&&l| l != 0).count();
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

            // Check for fold-back: does the continuation duplicate any node
            // already in the route? This prevents routes that double back.
            let new_nodes: Vec<u64> = if pi == 0 {
                ways[wi][1..].to_vec()
            } else {
                ways[wi][..ways[wi].len()-1].iter().rev().copied().collect()
            };
            let route_set: HashSet<u64> = route.iter().copied().collect();
            if new_nodes.iter().any(|n| route_set.contains(n)) { break; }

            way_used[wi] = true;
            route.extend_from_slice(&new_nodes);
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

            // Check for fold-back
            let new_nodes: Vec<u64> = if pi == ways[wi].len()-1 {
                ways[wi][..ways[wi].len()-1].to_vec()
            } else {
                ways[wi][1..].iter().rev().copied().collect()
            };
            let route_set: HashSet<u64> = route.iter().copied().collect();
            if new_nodes.iter().any(|n| route_set.contains(n)) { break; }

            way_used[wi] = true;
            let mut prefix = new_nodes;
            prefix.push(first);
            prefix.extend_from_slice(&route[1..]);
            route = prefix;
        }

        routes.push(route);
    }

    routes.sort_by(|a, b| b.len().cmp(&a.len()));
    let interior_junctions: usize = routes.iter().map(|r| r[1..r.len().saturating_sub(1)].iter().filter(|n| junction_nodes.contains(n)).count()).sum();
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
    // Simplify routes
    let simplified: Vec<Vec<(usize, f64, f64)>> = routes.iter().zip(route_coords.iter()).enumerate()
        .map(|(_, (route, coords))| {
            let mut keep = vec![false; coords.len()];
            keep[0] = true;
            *keep.last_mut().unwrap() = true;

            // Force keep junction nodes and layer-change boundaries.
            for (i, &nid) in route.iter().enumerate() {
                if junction_nodes.contains(&nid) {
                    keep[i] = true;
                }
                // Keep nodes at layer transitions (bridge/tunnel boundaries)
                if i > 0 {
                    let prev_layer = node_layer.get(&route[i - 1]).copied().unwrap_or(0);
                    let cur_layer = node_layer.get(&nid).copied().unwrap_or(0);
                    if prev_layer != cur_layer {
                        keep[i - 1] = true; // last node of old layer
                        keep[i] = true;     // first node of new layer
                    }
                }
            }

            // Keep a node ~30m from junction endpoints for tight splines
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
                    keep[i] = true;
                    last_kept = i;
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

                    if worst_dev > 5.0 {
                        keep[worst_orig] = true;
                        added = true;
                    }
                }

                if !added { break; }
            }

            coords.iter().enumerate()
                .filter(|(i, _)| keep[*i])
                .map(|(i, &(x, y))| (i, x, y))
                .collect()
        }).collect();

    // Subdivide long segments by interpolating along the original OSM polyline.
    // This preserves curve shape instead of inserting straight-line midpoints.
    let simplified: Vec<Vec<(usize, f64, f64)>> = simplified.into_iter().zip(route_coords.iter())
        .map(|(simp, coords)| {
            let mut result = Vec::new();
            for i in 0..simp.len() {
                result.push(simp[i]);
                if i + 1 >= simp.len() { continue; }
                let (idx0, _, _) = simp[i];
                let (idx1, _, _) = simp[i + 1];
                let (_, x0, y0) = simp[i];
                let (_, x1, y1) = simp[i + 1];
                let seg_dist = ((x1 - x0).powi(2) + (y1 - y0).powi(2)).sqrt();
                if seg_dist <= MAX_SPACING { continue; }

                // Walk the original OSM polyline between idx0 and idx1, compute arc lengths
                let (start, end) = if idx0 != usize::MAX && idx1 != usize::MAX && idx0 < idx1 {
                    (idx0, idx1)
                } else {
                    // Fallback to straight-line interpolation for interpolated endpoints
                    let n = (seg_dist / MAX_SPACING).ceil() as usize;
                    for j in 1..n {
                        let t = j as f64 / n as f64;
                        result.push((usize::MAX, x0 + (x1 - x0) * t, y0 + (y1 - y0) * t));
                    }
                    continue;
                };

                // Build cumulative arc-length along original OSM nodes
                let mut cum_len = vec![0.0f64];
                for k in start..end {
                    let dx = coords[k + 1].0 - coords[k].0;
                    let dy = coords[k + 1].1 - coords[k].1;
                    cum_len.push(cum_len.last().unwrap() + (dx * dx + dy * dy).sqrt());
                }
                let total_len = *cum_len.last().unwrap();
                if total_len < 1.0 { continue; }

                let n = (total_len / MAX_SPACING).ceil() as usize;
                for j in 1..n {
                    let target = total_len * j as f64 / n as f64;
                    // Find the OSM segment containing this arc-length position
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
            result
        }).collect();

    let total_before: usize = route_coords.iter().map(|c| c.len()).sum();
    let total_after: usize = simplified.iter().map(|s| s.len()).sum();
    // Build game track nodes
    let mut track_nodes: Vec<Track> = Vec::new();
    let mut node_id_counter: i64 = 100;

    // For each route, create a chain of game nodes
    // Track which game node corresponds to each (route_idx, original_osm_idx) for branch attachment
    let mut route_game_nodes: Vec<Vec<RouteNodeInfo>> = Vec::new();

    // Map junction OSM nodes → game node IDs (populated during node creation)
    let mut junction_game_ids: HashMap<u64, i64> = HashMap::new();

    for (ri, simp) in simplified.iter().enumerate() {
        let mut chain: Vec<RouteNodeInfo> = Vec::new();
        let mut last_layer: i32 = 0; // for interpolated nodes

        for (si, &(orig_idx, x, y)) in simp.iter().enumerate() {
            let gid = node_id_counter;
            node_id_counter += 100;
            let prev = if si > 0 { chain[si - 1].game_id } else { 0 };

            // Look up elevation layer from OSM data
            let layer = if orig_idx != usize::MAX {
                let osm_nid = routes[ri][orig_idx];
                let l = node_layer.get(&osm_nid).copied().unwrap_or(0);
                last_layer = l;
                l
            } else {
                last_layer // interpolated node inherits from previous
            };

            track_nodes.push(Track {
                node_id: gid, x, y, layer,
                prev_node: prev,
                ..Track::default()
            });
            if si > 0 {
                let prev_idx = track_nodes.len() - 2;
                track_nodes[prev_idx].next_node = gid;
            }
            chain.push(RouteNodeInfo { game_id: gid });

            // Record junction game IDs for owning route (skip interpolated nodes)
            if orig_idx != usize::MAX {
                let osm_nid = routes[ri][orig_idx];
                if junction_nodes.contains(&osm_nid) && junction_owner.get(&osm_nid) == Some(&ri) {
                    junction_game_ids.insert(osm_nid, gid);
                }
            }
        }
        route_game_nodes.push(chain);
    }

    // Attach branches mid-segment on parent route (like depot blueprint).
    // Find the parent segment closest to the junction point, compute linear t.
    for (ri, simp) in simplified.iter().enumerate() {
        if simp.len() < 2 { continue; }

        for &is_start in &[true, false] {
            let endpoint_orig_idx = if is_start { simp[0].0 } else { simp.last().unwrap().0 };
            let endpoint_osm = routes[ri][endpoint_orig_idx];
            if !junction_nodes.contains(&endpoint_osm) { continue; }

            let Some(&owner_ri) = junction_owner.get(&endpoint_osm) else { continue };
            if owner_ri == ri { continue; }

            let branch_gid = if is_start {
                route_game_nodes[ri][0].game_id
            } else {
                route_game_nodes[ri].last().unwrap().game_id
            };

            // Junction position in Mercator
            let junction_orig = routes[ri][endpoint_orig_idx];
            let junction_pos = osm_nodes.get(&junction_orig)
                .map(|&(lat, lon)| latlon_to_mercator(lat, lon))
                .unwrap_or((0.0, 0.0));

            // Find nearest segment on parent route's simplified chain
            let parent_chain = &route_game_nodes[owner_ri];
            if parent_chain.len() < 2 { continue; }

            let mut best_seg = 0usize;
            let mut best_t = 0.5f64;
            let mut best_dist = f64::MAX;

            for si in 0..parent_chain.len() - 1 {
                let pi = track_nodes.iter().position(|n| n.node_id == parent_chain[si].game_id).unwrap();
                let qi = track_nodes.iter().position(|n| n.node_id == parent_chain[si + 1].game_id).unwrap();
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
                // Penalize segments where the projection falls outside [0,1]
                // (junction is past the segment end — likely wrong segment)
                let overshoot = (t_raw - t_raw.clamp(0.0, 1.0)).abs() * seg_len_sq.sqrt();
                let d = perp + overshoot;

                if d < best_dist {
                    best_dist = d;
                    best_seg = si;
                    best_t = t;
                }
            }

            // Debug: show junction attachment quality
            if best_dist > 5.0 || best_t > 0.98 || best_t < 0.02 {
                let pi = track_nodes.iter().position(|n| n.node_id == parent_chain[best_seg].game_id).unwrap();
                let qi = track_nodes.iter().position(|n| n.node_id == parent_chain[best_seg + 1].game_id).unwrap();            }

            // No reject — always create junction, even if geometry is imperfect.
            // The game recomputes spline parameters on load.

            // Junction often coincides with a parent node (shared OSM node).
            // In that case best_t≈0 or best_t≈1. Use the segment where the junction
            // is truly mid-segment. Pick a t of ~0.5 to place branch mid-segment.
            // The game's stored_t is a spline parameter (not linear), but 0.5 is a
            // reasonable default that puts the branch tangent at the segment midpoint.
            let parent_seg_idx;
            let t;
            if best_t > 0.95 && best_seg + 1 < parent_chain.len() - 1 {
                // At segment end → use NEXT segment, t=0.5
                parent_seg_idx = best_seg + 1;
                t = 0.5;
            } else if best_t < 0.05 && best_seg > 0 {
                // At segment start → use PREV segment, t=0.5
                parent_seg_idx = best_seg - 1;
                t = 0.5;
            } else {
                // Truly mid-segment
                parent_seg_idx = best_seg;
                t = best_t.clamp(0.05, 0.95);
            }
            let parent_node_id = parent_chain[parent_seg_idx].game_id;

            // Determine direction: does branch go forward or backward along parent?
            let br_idx = track_nodes.iter().position(|n| n.node_id == branch_gid).unwrap();
            let pi = track_nodes.iter().position(|n| n.node_id == parent_node_id).unwrap();
            // Parent segment direction: from parent_seg_idx toward next node
            let next_idx = if parent_seg_idx + 1 < parent_chain.len() { parent_seg_idx + 1 }
                else if parent_seg_idx > 0 { parent_seg_idx - 1 }
                else { continue; };
            let qi = track_nodes.iter().position(|n| n.node_id == parent_chain[next_idx].game_id).unwrap();
            let seg_dx = track_nodes[qi].x - track_nodes[pi].x;
            let seg_dy = track_nodes[qi].y - track_nodes[pi].y;

            // Branch's outgoing direction (from branch root toward its chain)
            let neighbor_gid = if is_start {
                if route_game_nodes[ri].len() > 1 { route_game_nodes[ri][1].game_id } else { continue; }
            } else {
                let len = route_game_nodes[ri].len();
                if len > 1 { route_game_nodes[ri][len - 2].game_id } else { continue; }
            };
            let ni = track_nodes.iter().position(|n| n.node_id == neighbor_gid).unwrap();
            let br_dx = track_nodes[ni].x - track_nodes[br_idx].x;
            let br_dy = track_nodes[ni].y - track_nodes[br_idx].y;
            let dot = br_dx * seg_dx + br_dy * seg_dy;
            let dir = if dot >= 0.0 { 1 } else { -1 };

            // Nudge branch root 5m along its outgoing direction
            let br_len = (br_dx * br_dx + br_dy * br_dy).sqrt().max(1e-10);
            track_nodes[br_idx].x += (br_dx / br_len) * 5.0;
            track_nodes[br_idx].y += (br_dy / br_len) * 5.0;

            // Set branch attachment
            track_nodes[br_idx].attached_to_id = parent_node_id;
            track_nodes[br_idx].attached_to_t = t as f64;
            track_nodes[br_idx].attached_to_direction = Some(dir);

            // Register in parent's attached_by
            let par_idx = track_nodes.iter().position(|n| n.node_id == parent_node_id).unwrap();
            track_nodes[par_idx].attached_by.push(branch_gid);
        }
    }

    // Debug branch detection
    let short = simplified.iter().filter(|s| s.len() < 2).count();
    let mut dbg_no_junc = 0; let mut dbg_same = 0; let mut dbg_found = 0;
    for (ri, simp) in simplified.iter().enumerate() {
        if simp.is_empty() { continue; }
        for &is_start in &[true, false] {
            let idx = if is_start { simp[0].0 } else { simp.last().unwrap().0 };
            let osm = routes[ri][idx];
            if !junction_nodes.contains(&osm) { dbg_no_junc += 1; continue; }
            match junction_owner.get(&osm) {
                None => { dbg_no_junc += 1; }
                Some(&pri) if pri == ri => { dbg_same += 1; }
                _ => { dbg_found += 1; }
            }
        }
    }
    let n_branches = track_nodes.iter().filter(|n| n.attached_to_id != 0).count();
    let n_junctions = track_nodes.iter().filter(|n| !n.attached_by.is_empty()).count();
    // Compute center and convert to ground meters
    let cx = track_nodes.iter().map(|t| t.x).sum::<f64>() / track_nodes.len() as f64;
    let cy = track_nodes.iter().map(|t| t.y).sum::<f64>() / track_nodes.len() as f64;
    let center_lat = (cy / 6_378_137.0).sinh().atan();
    let cos_lat = center_lat.cos();    for t in &mut track_nodes {
        t.x = (t.x - cx) * cos_lat;
        t.y = (t.y - cy) * cos_lat;
    }

    // Build NrclipFile and serialize
    let name_hash = blueprint_name.bytes().fold(0x1234567890u64, |h, b| h.wrapping_mul(31).wrapping_add(b as u64));
    let file = NrclipFile {
        version: MODEL_VERSION,
        collections: vec![Collection {
            id_a: name_hash,
            id_b: name_hash.wrapping_mul(7),
            name: blueprint_name.clone(),
            clips: vec![Clip {
                guid: blueprint_name.clone(),
                clip_id: name_hash.wrapping_mul(13),
                center_x: cx,
                center_y: cy,
                tracks: track_nodes,
                ..Clip::default()
            }],
            ..Collection::default()
        }],
    };

    file.to_bytes()
}

struct RouteNodeInfo {
    game_id: i64,
}

fn latlon_to_mercator(lat: f64, lon: f64) -> (f64, f64) {
    let x = lon.to_radians() * 6_378_137.0;
    let y = (lat.to_radians() / 2.0 + std::f64::consts::FRAC_PI_4).tan().ln() * 6_378_137.0;
    (x, y)
}

