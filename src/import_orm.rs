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

    // Collect all ways with resolved coordinates (no simplification for now)
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

    // Process ways: create shared nodes, wire prev/next, then fix junctions
    // with proper attached_to branches.
    let mut way_gids: Vec<Vec<i64>> = Vec::new();

    let new_node = |nodes: &mut Vec<TrackNode>, counter: &mut i64, x, y| -> (i64, usize) {
        let gid = *counter; *counter += 100;
        let idx = nodes.len();
        nodes.push(TrackNode {
            id: gid, x, y, layer: 0, prev: 0, next: 0,
            tangential: 0, tangent_delta: 0.0,
            attached_to_id: 0, attached_to_t: 0.0, attached_to_dir: 0,
            attached_by: Vec::new(),
        });
        (gid, idx)
    };

    // Stage 1: create shared nodes and wire through-routes
    for rw in &resolved_ways {
        let gids: Vec<i64> = rw.osm_ids.iter().enumerate().map(|(pi, &osm_nid)| {
            osm_to_game.entry(osm_nid).or_insert_with(|| {
                new_node(&mut track_nodes, &mut node_id_counter, rw.points[pi].0, rw.points[pi].1)
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

    // Stage 2: create branch endpoints at junctions.
    // A junction is any node where a way couldn't claim prev/next.
    // Branch roots get attached_to pointing to the junction node.
    // Junction nodes get tangential=1 and tangent_delta≈2π (matching real blueprints).
    for (wi, rw) in resolved_ways.iter().enumerate() {
        let gids = &mut way_gids[wi];
        if gids.len() < 2 { continue; }

        for i in 0..gids.len() {
            let junc_idx = osm_to_game[&rw.osm_ids[i]].1;
            let want_prev = if i > 0 { gids[i - 1] } else { 0 };
            let want_next = if i + 1 < gids.len() { gids[i + 1] } else { 0 };
            let has_prev = want_prev == 0 || track_nodes[junc_idx].prev == want_prev;
            let has_next = want_next == 0 || track_nodes[junc_idx].next == want_next;

            if has_prev && has_next { continue; }

            // Create branch root at junction position
            let junction_id = gids[i];
            let (jx, jy) = (track_nodes[junc_idx].x, track_nodes[junc_idx].y);
            let (branch_id, _) = new_node(&mut track_nodes, &mut node_id_counter, jx, jy);

            // Compute att_dir: does branch head toward junction's next (+1) or prev (-1)?
            let branch_other = if want_next != 0 { want_next } else { want_prev };
            let bo = track_nodes.iter().find(|n| n.id == branch_other).unwrap();
            let branch_heading = (bo.y - jy).atan2(bo.x - jx);

            let jn = &track_nodes[junc_idx];
            let att_dir = if jn.next != 0 && jn.prev != 0 {
                let nn = track_nodes.iter().find(|n| n.id == jn.next).unwrap();
                let fwd = (nn.y - jy).atan2(nn.x - jx);
                let mut diff = (branch_heading - fwd).abs();
                if diff > std::f64::consts::PI { diff = 2.0 * std::f64::consts::PI - diff; }
                if diff < std::f64::consts::FRAC_PI_2 { 1 } else { -1 }
            } else if jn.next != 0 { 1 } else { -1 };

            // Set branch root fields
            let br_idx = track_nodes.len() - 1;
            track_nodes[br_idx].prev = want_prev;
            track_nodes[br_idx].next = want_next;
            track_nodes[br_idx].attached_to_id = junction_id;
            track_nodes[br_idx].attached_to_t = 0.01; // small nonzero — game needs mid-segment position
            track_nodes[br_idx].attached_to_dir = att_dir;

            // Mark junction as tangential with tangent_delta ≈ 2π
            track_nodes[junc_idx].tangential = 1;
            track_nodes[junc_idx].tangent_delta = std::f32::consts::TAU; // 2π

            // Replace junction with branch in this way's chain
            gids[i] = branch_id;

            // Fix adjacent nodes
            if want_prev != 0 {
                if let Some(pi) = track_nodes.iter().position(|n| n.id == want_prev) {
                    if track_nodes[pi].next == junction_id { track_nodes[pi].next = branch_id; }
                }
            }
            if want_next != 0 {
                if let Some(ni) = track_nodes.iter().position(|n| n.id == want_next) {
                    if track_nodes[ni].prev == junction_id { track_nodes[ni].prev = branch_id; }
                }
            }

            // Add to junction's attached_by
            track_nodes[junc_idx].attached_by.push(branch_id);
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
    tangential: u8,
    tangent_delta: f32,
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
        w.write_f32(t.tangent_delta); // user_tangent_delta (2π at junction nodes)
        w.write_f32(0.5);            // next_spline_t
        w.write_i64z(0);             // station_group_id
        w.write_i32z(0);             // blueprint
        w.write_string("");          // name
        w.write_raw_u8(0);           // station_platform_auto_name
        w.write_raw_u8(0);           // straight
        w.write_raw_u8(t.tangential); // 0=point mode, 1=tangent mode (junction through-routes)
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
