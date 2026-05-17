use anyhow::{Context, Result};
use std::fs;

mod encode;
mod wyhash_nrc1;
mod nrclip;

use encode::PayloadWriter;

const MODEL_VERSION: u32 = 226;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let output = args.iter().find(|a| !a.starts_with('-') && *a != &args[0])
        .cloned().unwrap_or_else(|| "generated.nrclip".to_string());

    // Generate tracks
    let empty = args.iter().any(|a| a == "--empty");
    let simple = args.iter().any(|a| a == "--simple");
    let count: usize = args.iter().find_map(|a| a.strip_prefix("--count=").and_then(|s| s.parse().ok())).unwrap_or(170);
    let tracks = if empty {
        Vec::new()
    } else if simple {
        generate_simple_track(count)
    } else {
        generate_double_track()
    };
    println!("Generated {} track nodes", tracks.len());

    // Build payload
    let payload = build_payload(&tracks)?;
    println!("Payload: {} bytes", payload.len());

    // Compress (must include content size in frame header for game compatibility)
    let compressed = {
        let mut encoder = zstd::stream::Encoder::new(Vec::new(), 3)?;
        encoder.include_contentsize(true)?;
        encoder.set_pledged_src_size(Some(payload.len() as u64))?;
        std::io::copy(&mut payload.as_slice(), &mut encoder)?;
        encoder.finish()?
    };
    println!("Compressed: {} bytes", compressed.len());

    // Checksum (wyhash of uncompressed payload)
    let checksum = wyhash_nrc1::checksum(&payload);

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

/// Simple linear track matching the Python generator exactly.
fn generate_simple_track(count: usize) -> Vec<TrackNode> {
    let mut nodes = Vec::new();
    for i in 0..count {
        nodes.push(TrackNode {
            id: (i + 1) as i64,
            x: (i as f64) * 50.0,
            y: 0.0,
            layer: 0,
            prev: if i == 0 { 0 } else { i as i64 },
            next: if i == count - 1 { 0 } else { (i + 2) as i64 },
        });
    }
    nodes
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
    w.write_varint(5641124955619280206u64);  // id_a (v>=71)
    w.write_varint(81985529216486895u64);    // id_b (v>=71)
    w.write_optional_mod_source(&None);      // mod_source (v>=71)
    w.write_string("Rust Test");             // name (v>=66)

    // Clip count = 1
    w.write_varint(1);

    // Clip header
    w.write_string("test");           // GUID (v>=66)
    w.write_varint(0xDEADBEEFu64);    // clip_id (v>=66)
    // Center coords (v>=147)
    w.write_f64(0.0);
    w.write_f64(0.0);

    // vec<Track>
    if tracks.is_empty() {
        w.write_varint(0);
    } else {
        write_tracks(&mut w, tracks, ver);
    }

    // vec<Signal> (v>=198): empty
    w.write_varint(0);
    // vec<StationGroup> (v>=66): empty
    w.write_varint(0);
    // vec<Building> (v>=66): empty
    w.write_varint(0);

    // All remaining sections empty
    w.write_varint(0);  // track_kinds
    w.write_varint(0);  // building_kinds
    w.write_varint(0);  // demands
    w.write_varint(0);  // mod_metas

    Ok(w.into_bytes())
}

fn write_tracks(w: &mut PayloadWriter, tracks: &[TrackNode], ver: u32) {
    w.write_varint(tracks.len() as u64);

    for t in tracks {
        // Pre-coordinate
        w.write_i64z(t.id);               // node_id
        w.write_raw_u8(1);                 // node_type (v>=30)
        w.write_i32z(0);                   // track_type = 0 (default, no specific TrackKind needed)
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

/// Load TrackKind, BuildingKind, Demand, and ModMeta from a reference blueprint,
/// then re-encode them into our output. This ensures valid game asset references.
/// Returns the track_type key to use for our generated tracks.
fn write_borrowed_sections(w: &mut PayloadWriter, _tracks: &[TrackNode]) -> Result<()> {
    use crate::nrclip;

    let ref_paths = [
        "/Users/alex/Library/Application Support/CrossOver/Bottles/Steam/drive_c/Program Files (x86)/Steam/steamapps/workshop/content/1134710/3667051683/blueprints.nrclip",
        "testblueprint_backup.nrclip",
        "2949234540/blueprints.nrclip",
    ];

    // Find a clip with a TrackKind
    let mut donor: Option<nrclip::Clip> = None;
    for path in &ref_paths {
        let Ok(raw) = std::fs::read(path) else { continue };
        let ver = u32::from_le_bytes(raw[4..8].try_into().unwrap());
        let Ok(payload) = zstd::stream::decode_all(&raw[32..]) else { continue };
        let Ok(colls) = nrclip::parse_payload(&payload, ver) else { continue };
        for coll in colls {
            for clip in coll.clips {
                if !clip.track_kinds.is_empty() {
                    eprintln!("  Borrowing TrackKind/ModMeta from: {}", path);
                    donor = Some(clip);
                    break;
                }
            }
            if donor.is_some() { break; }
        }
        if donor.is_some() { break; }
    }

    let clip = donor.expect("No reference blueprint found with TrackKind");

    // Re-encode the borrowed sections byte-for-byte through our encoder
    // (reuse the write functions from inject.rs via include)

    // map<int, TrackKind>
    w.write_varint(clip.track_kinds.len() as u64);
    for (key, tk) in &clip.track_kinds {
        w.write_i32z(*key);
        write_track_kind(w, tk);
    }

    // map<int, BuildingKind>
    w.write_varint(clip.building_kinds.len() as u64);
    for (key, bk) in &clip.building_kinds {
        w.write_i32z(*key);
        write_building_kind(w, bk, MODEL_VERSION);
    }

    // map<u64, Demand>: empty (we don't need demand data)
    w.write_varint(0);

    // vec<ModMeta>
    w.write_varint(clip.mod_metas.len() as u64);
    for m in &clip.mod_metas {
        write_mod_meta(w, m, MODEL_VERSION);
    }

    Ok(())
}

// Include the writer functions from inject.rs
// (These are the same functions used for round-trip encoding)
fn write_track_kind(w: &mut PayloadWriter, tk: &nrclip::TrackKind) {
    w.write_string(&tk.display_name);
    w.write_raw_u8(tk.speed_class_flag);
    w.write_i32z(tk.speed_class);
    w.write_string(&tk.internal_name);
    w.write_string(&tk.secondary_name);
    for h in &tk.horizons {
        w.write_i32z(h.speed_class);
        w.write_f64(h.gauge); w.write_f64(h.height); w.write_f64(h.max_speed);
        w.write_f64(h.width_a); w.write_f64(h.width_b); w.write_f64(h.spacing);
        w.write_f64(h.offset_a); w.write_f64(h.offset_b);
        w.write_i64z(h.visual_distance);
        for &f in &h.flags { w.write_raw_u8(f); }
        for tex in &h.textures {
            w.write_i32z(tex.speed_class);
            for file in &tex.files {
                w.write_mod_rel_file(file.workshop_id, &file.path, &file.name);
            }
        }
    }
}

fn write_building_kind(w: &mut PayloadWriter, bk: &nrclip::BuildingKind, ver: u32) {
    w.write_string(&bk.display_name);
    w.write_raw_u8(bk.speed_class_flag);
    w.write_i32z(bk.speed_class);
    w.write_string(&bk.internal_name);
    w.write_string(&bk.secondary_name);
    if ver == 62 { w.write_i32z(0); }
    w.write_vec_set_i64(&bk.tags);
    w.write_f32(bk.size_x); w.write_f32(bk.size_y);
    if (62..=181).contains(&ver) { w.write_raw_u8(0); }
    if (167..=181).contains(&ver) { w.write_raw_u8(0); }
    if ver >= 65 { w.write_raw_u8(bk.curved.unwrap_or(0)); }
    w.write_raw_u8(bk.recolor);
    if ver >= 67 { w.write_raw_u8(bk.is_poi.unwrap_or(0)); }
    if ver >= 69 { w.write_raw_u8(bk.has_default_size.unwrap_or(0)); w.write_raw_u8(bk.decal_count.unwrap_or(0)); }
    w.write_i32z(bk.border_x);
    w.write_i32z(bk.border_x); // read twice
    if ver >= 69 {
        w.write_f32(bk.lod_x); w.write_f32(bk.lod_y);
        w.write_varint(bk.sentinel as u64);
        w.write_f32(bk.offset_neg); w.write_f32(bk.offset_pos);
        w.write_raw_u8(bk.decal_count.unwrap_or(0)); // re-read
        w.write_varint(bk.scripts.len() as u64);
        for s in &bk.scripts { w.write_string(s); }
    }
    w.write_i32z(bk.rule_x); w.write_i32z(bk.rule_y);
    if ver >= 63 { w.write_raw_u8(bk.partial_repeat_x.unwrap_or(1)); w.write_raw_u8(bk.partial_repeat_y.unwrap_or(1)); }
    w.write_i32z(bk.default_draw_layer);
    w.write_mod_source_pair(bk.texture.workshop_id, &bk.texture.path);
    w.write_string(&bk.model_path);
    if ver == 62 { w.write_mod_source_pair(0, ""); w.write_string(""); }
}

fn write_mod_meta(w: &mut PayloadWriter, m: &nrclip::ModMeta, ver: u32) {
    w.write_i64z(m.source_id);
    w.write_string(&m.source_path);
    w.write_string(&m.folder); w.write_string(&m.display_name);
    w.write_string(&m.author); w.write_string(&m.description);
    w.write_string(&m.version); w.write_string(&m.tag);
    w.write_vec_set_i64(&m.provides);
    if ver >= 117 {
        w.write_varint(m.content_items.len() as u64);
        for (kt, kn, vn) in &m.content_items {
            w.write_i32z(*kt); w.write_string(kn); w.write_string(vn);
        }
    }
    w.write_raw_u8(m.content_loaded);
    w.write_raw_u8(m.has_local_data);
}
