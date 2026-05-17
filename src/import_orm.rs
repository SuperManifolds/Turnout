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

    for way in &ways {
        let points: Vec<(f64, f64)> = way.node_ids.iter()
            .filter_map(|nid| osm_nodes.get(nid).map(|&(lat, lon)| latlon_to_mercator(lat, lon)))
            .collect();

        if points.len() < 2 {
            continue;
        }

        // Each way gets its own independent chain of nodes.
        // No sharing — junctions would need the game's junction system
        // which we don't implement yet. Each way is a standalone chain.
        let mut way_ids: Vec<i64> = Vec::new();
        for _ in 0..points.len() {
            way_ids.push(node_id_counter);
            node_id_counter += 100;
        }

        for (i, &game_id) in way_ids.iter().enumerate() {
            let prev = if i == 0 { 0 } else { way_ids[i - 1] };
            let next = if i == way_ids.len() - 1 { 0 } else { way_ids[i + 1] };

            track_nodes.push(TrackNode {
                id: game_id,
                x: points[i].0,
                y: points[i].1,
                layer: 0,
                prev,
                next,
            });
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
        w.write_raw_u8(0);           // winding
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
        w.write_raw_u8(0);           // tangential
        w.write_raw_u8(0);           // limited_shapes
        for _ in 0..4 { w.write_varint(0); } // conflicts
        w.write_vec_set_i64(&[]);    // signal_ids
        w.write_i64z(0);             // attached_to_id
        w.write_f64(0.0);            // attached_to_t
        w.write_i32z(0);             // attached_to_direction
        w.write_vec_set_i64(&[]);    // attached_by
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
