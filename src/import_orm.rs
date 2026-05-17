/// Import OpenRailwayMap tracks into a Nimby Rails blueprint.
/// Fetches railway data from the Overpass API and generates a .nrclip file.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs;

mod encode;
mod wyhash_nrc1;
mod nrclip;

use encode::PayloadWriter;

const MODEL_VERSION: u32 = 226;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let json_path = args.get(1).context("usage: import_orm <tracks.json> [output.nrclip]")?;
    let output = args.get(2).map(|s| s.as_str()).unwrap_or("orm_import.nrclip");

    // Load Overpass JSON
    let raw = fs::read_to_string(json_path).context("read JSON")?;
    let data: serde_json::Value = serde_json::from_str(&raw).context("parse JSON")?;

    let elements = data["elements"].as_array().context("no elements")?;

    // Extract OSM nodes (lat/lon)
    let mut osm_nodes: HashMap<u64, (f64, f64)> = HashMap::new();
    for e in elements {
        if e["type"].as_str() == Some("node") {
            let id = e["id"].as_u64().unwrap();
            let lat = e["lat"].as_f64().unwrap();
            let lon = e["lon"].as_f64().unwrap();
            osm_nodes.insert(id, (lat, lon));
        }
    }

    // Extract OSM ways
    let mut ways: Vec<OsmWay> = Vec::new();
    for e in elements {
        if e["type"].as_str() == Some("way") {
            let id = e["id"].as_u64().unwrap();
            let node_ids: Vec<u64> = e["nodes"].as_array().unwrap()
                .iter().map(|n| n.as_u64().unwrap()).collect();
            let tags = e.get("tags").cloned().unwrap_or_default();
            ways.push(OsmWay { id, node_ids, tags });
        }
    }

    println!("Loaded {} OSM nodes, {} ways", osm_nodes.len(), ways.len());

    // Convert to game coordinates (Web Mercator EPSG:3857)
    let mut track_nodes: Vec<TrackNode> = Vec::new();
    let mut node_id_counter: i64 = 100;
    let mut osm_to_game: HashMap<u64, (i64, usize)> = HashMap::new();

    // Collect all ways with resolved coordinates
    struct ResolvedWay {
        osm_ids: Vec<u64>,
        points: Vec<(f64, f64)>,
    }
    let mut resolved_ways: Vec<ResolvedWay> = Vec::new();
    for way in &ways {
        let mut osm_ids = Vec::new();
        let mut points = Vec::new();
        for &nid in &way.node_ids {
            if let Some(&(lat, lon)) = osm_nodes.get(&nid) {
                osm_ids.push(nid);
                points.push(latlon_to_mercator(lat, lon));
            }
        }
        if points.len() >= 2 {
            resolved_ways.push(ResolvedWay { osm_ids, points });
        }
    }

    // For each OSM node, collect (way_idx, position_in_way, heading) from all ways
    struct WayRef { way_idx: usize, pos: usize, heading: f64 }
    let mut node_refs: HashMap<u64, Vec<WayRef>> = HashMap::new();
    for (wi, rw) in resolved_ways.iter().enumerate() {
        for (pi, &osm_nid) in rw.osm_ids.iter().enumerate() {
            let heading = if pi + 1 < rw.points.len() {
                let (dx, dy) = (rw.points[pi+1].0 - rw.points[pi].0, rw.points[pi+1].1 - rw.points[pi].1);
                dy.atan2(dx)
            } else if pi > 0 {
                let (dx, dy) = (rw.points[pi].0 - rw.points[pi-1].0, rw.points[pi].1 - rw.points[pi-1].1);
                dy.atan2(dx)
            } else { 0.0 };
            node_refs.entry(osm_nid).or_default().push(WayRef { way_idx: wi, pos: pi, heading });
        }
    }

    // Process ways in two stages:
    // 1. Create shared game nodes for all OSM nodes, wire prev/next (first claim wins)
    // 2. For junction branches (ways that couldn't claim prev/next on a shared node),
    //    create a separate branch endpoint with attached_to pointing to the junction node.

    // Stage 1: create nodes and wire through-routes
    let mut way_gids: Vec<Vec<i64>> = Vec::new();
    for rw in &resolved_ways {
        let gids: Vec<i64> = rw.osm_ids.iter().enumerate().map(|(pi, &osm_nid)| {
            osm_to_game.entry(osm_nid).or_insert_with(|| {
                let gid = node_id_counter;
                node_id_counter += 100;
                let idx = track_nodes.len();
                let (x, y) = rw.points[pi];
                track_nodes.push(TrackNode {
                    id: gid, x, y, layer: 0, prev: 0, next: 0,
                    attached_to_id: 0, attached_to_t: 0.0, attached_to_dir: 0,
                    attached_by: Vec::new(),
                });
                (gid, idx)
            }).0
        }).collect();

        for i in 0..gids.len() {
            let idx = osm_to_game[&rw.osm_ids[i]].1;
            let node = &mut track_nodes[idx];
            if i > 0 && node.prev == 0 { node.prev = gids[i - 1]; }
            if i + 1 < gids.len() && node.next == 0 { node.next = gids[i + 1]; }
        }
        way_gids.push(gids);
    }

    // Stage 2: fix branches at junctions.
    // For each node in each way, check if it's a shared junction node where
    // this way couldn't claim prev/next. If so, split the chain at the junction
    // by inserting branch endpoints with attached_to.
    for (wi, rw) in resolved_ways.iter().enumerate() {
        let gids = &mut way_gids[wi];
        if gids.len() < 2 { continue; }

        for i in 0..gids.len() {
            let shared_idx = osm_to_game[&rw.osm_ids[i]].1;
            let node = &track_nodes[shared_idx];

            // Check if this way's expected links match the shared node's actual links
            let want_prev = if i > 0 { gids[i - 1] } else { 0 };
            let want_next = if i + 1 < gids.len() { gids[i + 1] } else { 0 };

            let has_prev = want_prev == 0 || node.prev == want_prev;
            let has_next = want_next == 0 || node.next == want_next;

            if has_prev && has_next {
                continue; // This way owns this node, no branch needed
            }

            // This is a junction where we couldn't claim prev/next.
            // Create a branch endpoint at the same position.
            let junction_id = gids[i];
            let branch_id = node_id_counter;
            node_id_counter += 100;
            let (x, y) = (track_nodes[shared_idx].x, track_nodes[shared_idx].y);

            // Compute att_dir from geometry: does the branch head toward
            // the junction's next (+1) or prev (-1)?
            let branch_heading = if want_next != 0 {
                // Branch continues forward — find the next node's position
                let ni = track_nodes.iter().find(|n| n.id == want_next).unwrap();
                (ni.y - y).atan2(ni.x - x)
            } else if want_prev != 0 {
                let pi = track_nodes.iter().find(|n| n.id == want_prev).unwrap();
                (y - pi.y).atan2(x - pi.x) // heading FROM prev toward branch
            } else { 0.0 };

            let jn = &track_nodes[shared_idx];
            let att_dir = if jn.next != 0 {
                let next_node = track_nodes.iter().find(|n| n.id == jn.next).unwrap();
                let fwd_heading = (next_node.y - jn.y).atan2(next_node.x - jn.x);
                let diff = (branch_heading - fwd_heading).abs();
                let diff = if diff > std::f64::consts::PI { 2.0 * std::f64::consts::PI - diff } else { diff };
                if diff < std::f64::consts::FRAC_PI_2 { 1 } else { -1 }
            } else if jn.prev != 0 {
                let prev_node = track_nodes.iter().find(|n| n.id == jn.prev).unwrap();
                let bwd_heading = (prev_node.y - jn.y).atan2(prev_node.x - jn.x);
                let diff = (branch_heading - bwd_heading).abs();
                let diff = if diff > std::f64::consts::PI { 2.0 * std::f64::consts::PI - diff } else { diff };
                if diff < std::f64::consts::FRAC_PI_2 { -1 } else { 1 }
            } else { 1 };

            track_nodes.push(TrackNode {
                id: branch_id, x, y, layer: 0,
                prev: want_prev,
                next: want_next,
                attached_to_id: junction_id,
                attached_to_t: 0.0,
                attached_to_dir: att_dir,
                attached_by: Vec::new(),
            });

            // Replace junction ID with branch ID in this way's chain
            gids[i] = branch_id;

            // Fix adjacent nodes to point to branch instead of junction
            if i > 0 {
                let prev_node_idx = track_nodes.iter().position(|n| n.id == want_prev).unwrap();
                if track_nodes[prev_node_idx].next == junction_id {
                    track_nodes[prev_node_idx].next = branch_id;
                }
            }
            if i + 1 < gids.len() {
                let next_node_idx = track_nodes.iter().position(|n| n.id == want_next).unwrap();
                if track_nodes[next_node_idx].prev == junction_id {
                    track_nodes[next_node_idx].prev = branch_id;
                }
            }

            // Add to junction's attached_by
            track_nodes[shared_idx].attached_by.push(branch_id);
        }
    }

    println!("Created {} track nodes from {} ways", track_nodes.len(), ways.len());

    // Compute centroid in absolute Web Mercator for the clip center,
    // then convert track coordinates to ground-meter offsets.
    // The game stores relative positions in ground meters, not Mercator meters.
    // Ground meters = Mercator meters * cos(latitude_at_center).
    let cx = track_nodes.iter().map(|t| t.x).sum::<f64>() / track_nodes.len() as f64;
    let cy = track_nodes.iter().map(|t| t.y).sum::<f64>() / track_nodes.len() as f64;
    let center_lat = (cy / 6_378_137.0).sinh().atan(); // Mercator Y → latitude in radians
    let cos_lat = center_lat.cos();
    println!("Center: ({:.2}, {:.2}), lat={:.6}°, cos(lat)={:.6}", cx, cy,
             center_lat.to_degrees(), cos_lat);
    for t in &mut track_nodes {
        t.x = (t.x - cx) * cos_lat;
        t.y = (t.y - cy) * cos_lat;
    }

    // Build payload
    let payload = build_payload(&track_nodes, cx, cy)?;
    println!("Payload: {} bytes", payload.len());

    // Compress with content size in frame header
    let compressed = {
        let mut enc = zstd::stream::Encoder::new(Vec::new(), 3)?;
        enc.include_contentsize(true)?;
        enc.set_pledged_src_size(Some(payload.len() as u64))?;
        std::io::copy(&mut payload.as_slice(), &mut enc)?;
        enc.finish()?
    };

    // Checksum
    let checksum = wyhash_nrc1::checksum(&payload);

    // Write NRC1
    let mut file_data = Vec::new();
    file_data.extend_from_slice(b"NRC1");
    file_data.extend_from_slice(&MODEL_VERSION.to_le_bytes());
    file_data.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    file_data.extend_from_slice(&(compressed.len() as u64).to_le_bytes());
    file_data.extend_from_slice(&checksum.to_le_bytes());
    file_data.extend_from_slice(&compressed);

    fs::write(output, &file_data)?;
    println!("Wrote {} bytes to {}", file_data.len(), output);

    // Verify
    let decoded = nrclip::parse_payload(&payload, MODEL_VERSION)?;
    let total: usize = decoded.iter().flat_map(|c| &c.clips).map(|c| c.tracks.len()).sum();
    println!("Verified: {} tracks", total);

    Ok(())
}

struct OsmWay {
    id: u64,
    node_ids: Vec<u64>,
    tags: serde_json::Value,
}

struct TrackNode {
    id: i64,
    x: f64,
    y: f64,
    layer: i32,
    prev: i64,
    next: i64,
    attached_to_id: i64,
    attached_to_t: f64,
    attached_to_dir: i32,
    attached_by: Vec<i64>,
}

/// Convert WGS84 lat/lon to Web Mercator (EPSG:3857) — the game's coordinate system.
fn latlon_to_mercator(lat: f64, lon: f64) -> (f64, f64) {
    let x = lon.to_radians() * 6_378_137.0;
    let y = (lat.to_radians() / 2.0 + std::f64::consts::FRAC_PI_4).tan().ln() * 6_378_137.0;
    (x, y)
}

fn build_payload(tracks: &[TrackNode], center_x: f64, center_y: f64) -> Result<Vec<u8>> {
    let mut w = PayloadWriter::new();

    // 1 collection
    w.write_varint(1);
    w.write_varint(7777777777u64);          // id_a
    w.write_varint(8888888888u64);          // id_b
    w.write_optional_mod_source(&None);
    w.write_string("ORM Import");

    // 1 clip
    w.write_varint(1);
    w.write_string("orm-import");
    w.write_varint(0x08120001u64);
    w.write_f64(center_x);
    w.write_f64(center_y);

    // Tracks
    w.write_varint(tracks.len() as u64);
    for t in tracks {
        w.write_i64z(t.id);
        w.write_raw_u8(1);           // node_type
        w.write_i32z(0);             // track_type
        w.write_i32z(t.layer);       // layer
        w.write_raw_u8(1);           // winding (1 or 255 in real blueprints, never 0)
        w.write_i64z(t.prev);
        w.write_i64z(t.next);
        w.write_i64z(0);             // group_id
        w.write_f32(0.0);            // user_max_speed
        w.write_f64(t.x);
        w.write_f64(t.y);
        w.write_f32(0.0);            // user_tangent_delta
        w.write_f32(0.5);            // next_spline_t
        w.write_i64z(0);             // station_group_id
        w.write_i32z(0);             // blueprint
        w.write_string("");          // name
        w.write_raw_u8(0);           // station_platform_auto_name
        w.write_raw_u8(0);           // straight
        w.write_raw_u8(0);           // tangential (0=point mode: nodes ARE on the track)
        w.write_raw_u8(0);           // limited_shapes
        for _ in 0..4 { w.write_varint(0); } // conflicts
        w.write_vec_set_i64(&[]);    // signal_ids
        w.write_i64z(t.attached_to_id);
        w.write_f64(t.attached_to_t);
        w.write_i32z(t.attached_to_dir);
        w.write_vec_set_i64(&t.attached_by);
        w.write_vec_set_i64(&[]);    // building_attached_by
        w.write_i64z(0);             // parallel_to_id
        w.write_i64z(0);             // parallel_kind
        w.write_f32(0.0);            // parallel_to_t
        w.write_i32z(0);             // parallel_to_direction
        w.write_f32(0.0);            // parallel_to_offset
        w.write_f32(0.0);            // parallel_to_disp
        w.write_vec_set_i64(&[]);    // parallel_by
        w.write_f32(0.0);            // proximity_diamond
    }

    // Empty sections
    w.write_varint(0);  // signals
    w.write_varint(0);  // station_groups
    w.write_varint(0);  // buildings
    w.write_varint(0);  // track_kinds
    w.write_varint(0);  // building_kinds
    w.write_varint(0);  // demands
    w.write_varint(0);  // mod_metas

    Ok(w.into_bytes())
}
