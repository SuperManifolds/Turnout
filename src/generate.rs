use anyhow::{Context, Result};
use std::fs;

mod encode;
mod nrclip;

use encode::PayloadWriter;

const MODEL_VERSION: u32 = 226;
const WYHASH_SEED: u64 = 0x9c3805fc2c85cacc;

fn main() -> Result<()> {
    let output = std::env::args().nth(1).unwrap_or_else(|| "generated.nrclip".to_string());

    // Generate a double-track route ~1km with curves and elevation changes
    let tracks = generate_double_track();
    println!("Generated {} track nodes", tracks.len());

    // Build payload
    let payload = build_payload(&tracks)?;
    println!("Payload: {} bytes", payload.len());

    // Compress
    let compressed = zstd::stream::encode_all(payload.as_slice(), 3)
        .context("zstd compress")?;
    println!("Compressed: {} bytes", compressed.len());

    // Checksum (wyhash of uncompressed payload)
    let checksum = wyhash::wyhash(payload.as_slice(), WYHASH_SEED);

    // Write NRC1 container
    let mut file_data = Vec::new();
    file_data.extend_from_slice(b"NRC1");
    file_data.extend_from_slice(&MODEL_VERSION.to_le_bytes());
    file_data.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    file_data.extend_from_slice(&(compressed.len() as u64).to_le_bytes());
    file_data.extend_from_slice(&checksum.to_le_bytes());
    file_data.extend_from_slice(&compressed);

    fs::write(&output, &file_data).with_context(|| format!("write {}", output))?;
    println!("Wrote {} bytes to {}", file_data.len(), output);

    // Verify by decoding
    println!("\nVerifying...");
    let decoded = nrclip::parse_payload(&payload, MODEL_VERSION)?;
    let total_tracks: usize = decoded.iter()
        .flat_map(|c| &c.clips)
        .map(|c| c.tracks.len())
        .sum();
    println!("Decoded {} tracks successfully", total_tracks);

    Ok(())
}

struct TrackNode {
    id: i64,
    x: f64,
    y: f64,
    layer: i32,
    prev: i64,
    next: i64,
}

fn generate_double_track() -> Vec<TrackNode> {
    let mut nodes = Vec::new();
    let track_spacing = 15.0;
    let node_spacing = 25.0; // ~25m between nodes for smooth curves

    // Design the route as a heading (angle) that changes gradually.
    // Real railways have minimum curve radii — we use large gentle arcs.
    //
    // Route plan (~1.5km):
    //   0-300m:    straight heading east, ground
    //   300-500m:  gentle curve left (15°), rising to elevated
    //   500-700m:  straight on elevated bridge
    //   700-900m:  gentle curve right (20°), descending to ground
    //   900-1100m: straight, ground
    //   1100-1300m: gentle S-curve entering tunnel (underground)
    //   1300-1500m: straight underground, then exit

    struct Segment {
        length: f64,
        curvature: f64, // radians per meter (positive = left turn)
        layer: i32,
    }

    let segments = vec![
        Segment { length: 300.0, curvature: 0.0,       layer: 0 },   // straight east
        Segment { length: 200.0, curvature: 0.0013,    layer: 0 },   // gentle left, 15° over 200m
        Segment { length: 100.0, curvature: 0.0,       layer: 1 },   // transition to elevated
        Segment { length: 200.0, curvature: 0.0,       layer: 1 },   // straight bridge
        Segment { length: 100.0, curvature: 0.0,       layer: 1 },   // end of bridge
        Segment { length: 200.0, curvature: -0.0015,   layer: 0 },   // gentle right, descend
        Segment { length: 200.0, curvature: 0.0,       layer: 0 },   // straight
        Segment { length: 150.0, curvature: 0.001,     layer: 0 },   // left into tunnel
        Segment { length: 100.0, curvature: -0.0008,   layer: -1 },  // right underground
        Segment { length: 200.0, curvature: 0.0,       layer: -1 },  // straight tunnel
        Segment { length: 150.0, curvature: -0.0005,   layer: 0 },   // exit, slight right
        Segment { length: 200.0, curvature: 0.0,       layer: 0 },   // final straight
    ];

    // Walk along the route, placing nodes at regular intervals
    let mut route_points: Vec<(f64, f64, i32)> = Vec::new();
    let mut x = 0.0f64;
    let mut y = 0.0f64;
    let mut heading = 0.0f64; // radians, 0 = east

    for seg in &segments {
        let n_steps = (seg.length / node_spacing).ceil() as usize;
        let step = seg.length / n_steps as f64;
        for _ in 0..n_steps {
            route_points.push((x, y, seg.layer));
            heading += seg.curvature * step;
            x += heading.cos() * step;
            y += heading.sin() * step;
        }
    }
    route_points.push((x, y, segments.last().unwrap().layer));

    let n = route_points.len();
    let id_base_1 = 1i64;
    let id_base_2 = (n as i64) + 1;

    // Track 1 (main)
    for i in 0..n {
        let id = id_base_1 + i as i64;
        nodes.push(TrackNode {
            id,
            x: route_points[i].0,
            y: route_points[i].1,
            layer: route_points[i].2,
            prev: if i == 0 { 0 } else { id - 1 },
            next: if i == n - 1 { 0 } else { id + 1 },
        });
    }

    // Track 2 (parallel, perpendicular offset)
    for i in 0..n {
        let id = id_base_2 + i as i64;

        // Tangent direction for perpendicular offset
        let (dx, dy) = if i + 1 < n {
            (route_points[i + 1].0 - route_points[i].0,
             route_points[i + 1].1 - route_points[i].1)
        } else {
            (route_points[i].0 - route_points[i - 1].0,
             route_points[i].1 - route_points[i - 1].1)
        };
        let len = (dx * dx + dy * dy).sqrt().max(0.001);
        let nx = dy / len * track_spacing;
        let ny = -dx / len * track_spacing;

        nodes.push(TrackNode {
            id,
            x: route_points[i].0 + nx,
            y: route_points[i].1 + ny,
            layer: route_points[i].2,
            prev: if i == 0 { 0 } else { id - 1 },
            next: if i == n - 1 { 0 } else { id + 1 },
        });
    }

    nodes
}

fn build_payload(tracks: &[TrackNode]) -> Result<Vec<u8>> {
    let ver = MODEL_VERSION;
    let mut w = PayloadWriter::new();

    // Collections count = 1
    w.write_varint(1);

    // Collection header
    w.write_varint(0); w.write_varint(0);  // id_a, id_b (v>=71)
    w.write_optional_mod_source(&None);     // mod_source (v>=71)
    w.write_string("Generated");            // name (v>=66)

    // Clip count = 1
    w.write_varint(1);

    // Clip header
    w.write_string("generated-test");  // GUID (v>=66)
    w.write_varint(0);                 // clip_id (v>=66)
    // Center coords (v>=147): average of all track positions
    let cx: f64 = tracks.iter().map(|t| t.x).sum::<f64>() / tracks.len() as f64;
    let cy: f64 = tracks.iter().map(|t| t.y).sum::<f64>() / tracks.len() as f64;
    w.write_f64(cx);
    w.write_f64(cy);

    // vec<Track>
    write_tracks(&mut w, tracks, ver);

    // vec<Signal> (v>=198): empty
    w.write_varint(0);
    // vec<StationGroup> (v>=66): empty
    w.write_varint(0);
    // vec<Building> (v>=66): empty
    w.write_varint(0);

    // map<int, TrackKind> (v>=66): 1 entry with key matching track_type
    write_track_kind_map(&mut w);

    // map<int, BuildingKind> (v>=66): empty
    w.write_varint(0);
    // map<u64, Demand> (v>=158): empty
    w.write_varint(0);
    // vec<ModMeta> (v>=66): empty
    w.write_varint(0);

    Ok(w.into_bytes())
}

fn write_tracks(w: &mut PayloadWriter, tracks: &[TrackNode], ver: u32) {
    w.write_varint(tracks.len() as u64);

    for t in tracks {
        // Pre-coordinate
        w.write_i64z(t.id);               // node_id
        w.write_raw_u8(1);                 // node_type (v>=30)
        w.write_i32z(1);                   // track_type = 1 (index into TrackKind map)
        w.write_i32z(t.layer);             // layer (v>=45)
        w.write_raw_u8(0);                 // winding (v>=122)
        w.write_i64z(t.prev);             // prev_node
        w.write_i64z(t.next);             // next_node
        w.write_i64z(0);                   // group_id (v>=13)

        // Coordinate block
        w.write_f32(0.0);                  // user_max_speed (v>=72)
        w.write_f64(t.x);                 // x
        w.write_f64(t.y);                 // y
        // v102-105 migration: not at v226
        w.write_f32(0.0);                  // user_tangent_delta (v>=102)
        w.write_f32(0.5);                  // next_spline_t (v>=141), default 0.5

        // Post-coordinate fields
        w.write_i64z(0);                   // station_group_id
        w.write_i32z(0);                   // blueprint (v>=108)
        w.write_string("");                // name (v>=63)
        w.write_raw_u8(0);                 // station_platform_auto_name (v>=63)
        // F20 migration: not at v226
        // F21 migration: not at v226
        w.write_raw_u8(0);                 // straight (v>=62)
        w.write_raw_u8(0);                 // tangential (v>=143)
        w.write_raw_u8(0);                 // limited_shapes (v>=144)

        // 4× conflict vectors (v>=28): all empty
        for _ in 0..4 { w.write_varint(0); }

        // Signal area: v>=198 → vec_140 only (signal_ids)
        w.write_vec_set_i64(&[]);          // signal_ids

        // Attached/parallel constraint fields
        w.write_i64z(0);                   // attached_to_id
        w.write_f64(0.0);                  // attached_to_t
        w.write_i32z(0);                   // attached_to_direction (v>=30)
        w.write_vec_set_i64(&[]);          // attached_by
        w.write_vec_set_i64(&[]);          // building_attached_by (v>=62)

        // Parallel constraint (v>=33)
        w.write_i64z(0);                   // parallel_to_id
        w.write_i64z(0);                   // parallel_kind (i64→i32 truncated)
        w.write_f32(0.0);                  // parallel_to_t
        w.write_i32z(0);                   // parallel_to_direction
        w.write_f32(0.0);                  // parallel_to_offset

        w.write_f32(0.0);                  // parallel_to_disp (v>=60)
        w.write_vec_set_i64(&[]);          // parallel_by (v>=33)
        w.write_f32(0.0);                  // proximity_diamond (v>=192)
    }
}

fn write_track_kind_map(w: &mut PayloadWriter) {
    // 1 entry: key=1, matching track_type=1 in our tracks
    w.write_varint(1);
    w.write_i32z(1); // key

    // TrackKind
    w.write_string("Generated Track");     // display_name
    w.write_raw_u8(1);                     // speed_class_flag
    w.write_i32z(1);                       // speed_class
    w.write_string("generated_track");     // internal_name
    w.write_string("generated_track_name");// secondary_name

    // 3 Horizons (required)
    for i in 0..3 {
        let max_speed = match i { 0 => 200.0, 1 => 100.0, _ => 300.0 };
        write_horizon(w, max_speed);
    }
}

fn write_horizon(w: &mut PayloadWriter, max_speed: f64) {
    w.write_i32z(1);           // speed_class
    w.write_f64(22.222);       // gauge
    w.write_f64(4.68);         // height
    w.write_f64(max_speed);    // max_speed
    w.write_f64(3.0);          // width_a
    w.write_f64(2.0);          // width_b
    w.write_f64(15.0);         // spacing
    w.write_f64(2.5);          // offset_a
    w.write_f64(2.0);          // offset_b
    w.write_i64z(125000);      // visual_distance
    for _ in 0..5 { w.write_raw_u8(0); } // flags

    // 6 texture groups, each with 4 ModRelFiles
    for _ in 0..6 {
        w.write_i32z(0);       // texture speed_class
        for _ in 0..4 {
            w.write_mod_rel_file(0, "", ""); // empty texture
        }
    }
}
