/// Inject a generated blueprint into an existing collections.nrclip file.
/// Decodes the original, appends our generated clip as a new collection,
/// re-encodes with fresh checksum.

use anyhow::{Context, Result};
use binrw::BinRead;
use std::io::{Seek, SeekFrom};
use std::{env, fs, fs::File, io::BufReader};

mod encode;
mod wyhash_nrc1;
mod nrclip;

use encode::PayloadWriter;

const MODEL_VERSION: u32 = 226;

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    let collections_path = args.get(1)
        .context("usage: inject <collections.nrclip> [generated.nrclip]")?;
    let generated_path = args.get(2)
        .map(|s| s.as_str())
        .unwrap_or("generated.nrclip");

    // Read and decode original collections
    let orig_data = fs::read(collections_path)
        .with_context(|| format!("read {}", collections_path))?;
    let orig_header = parse_nrc1_header(&orig_data)?;
    let orig_payload = zstd::stream::decode_all(&orig_data[32..])
        .context("decompress original")?;
    let orig_collections = nrclip::parse_payload(&orig_payload, orig_header.version)
        .context("decode original")?;
    println!("Original: {} collections, v{}", orig_collections.len(), orig_header.version);

    // Read and decode generated blueprint
    let gen_data = fs::read(generated_path)
        .with_context(|| format!("read {}", generated_path))?;
    let gen_header = parse_nrc1_header(&gen_data)?;
    let gen_payload = zstd::stream::decode_all(&gen_data[32..])
        .context("decompress generated")?;
    let gen_collections = nrclip::parse_payload(&gen_payload, gen_header.version)
        .context("decode generated")?;
    let gen_clip_count: usize = gen_collections.iter()
        .flat_map(|c| &c.clips)
        .count();
    println!("Generated: {} collections, {} clips", gen_collections.len(), gen_clip_count);

    // Re-encode: original collections + generated collections
    let total_count = orig_collections.len() + gen_collections.len();
    let mut w = PayloadWriter::new();

    // Write combined collection count
    w.write_varint(total_count as u64);

    // Re-encode original collections (byte-level copy from original payload)
    // Instead of re-encoding field by field, we can copy the raw bytes for
    // original collections and only encode the new ones. But that requires
    // knowing the exact byte boundaries. Safer: re-encode everything.
    let ver = MODEL_VERSION;

    for coll in &orig_collections {
        write_collection(&mut w, coll, ver);
    }
    for coll in &gen_collections {
        write_collection(&mut w, coll, ver);
    }

    let payload = w.into_bytes();
    println!("Combined payload: {} bytes", payload.len());

    // Verify it decodes
    let check = nrclip::parse_payload(&payload, ver)?;
    let total_tracks: usize = check.iter()
        .flat_map(|c| &c.clips)
        .map(|c| c.tracks.len())
        .sum();
    println!("Verified: {} collections, {} total tracks", check.len(), total_tracks);

    // Compress and write
    let compressed = {
        let mut enc = zstd::stream::Encoder::new(Vec::new(), 3)?;
        enc.include_contentsize(true)?;
        enc.set_pledged_src_size(Some(payload.len() as u64))?;
        std::io::copy(&mut payload.as_slice(), &mut enc)?;
        enc.finish()?
    };
    let checksum = wyhash_nrc1::checksum(&payload);

    let mut out = Vec::new();
    out.extend_from_slice(b"NRC1");
    out.extend_from_slice(&ver.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    out.extend_from_slice(&(compressed.len() as u64).to_le_bytes());
    out.extend_from_slice(&checksum.to_le_bytes());
    out.extend_from_slice(&compressed);

    fs::write(collections_path, &out)
        .with_context(|| format!("write {}", collections_path))?;
    println!("Wrote {} bytes to {}", out.len(), collections_path);

    Ok(())
}

fn parse_nrc1_header(data: &[u8]) -> Result<NrcHeader> {
    if data.len() < 32 || &data[0..4] != b"NRC1" {
        anyhow::bail!("not an NRC1 file");
    }
    Ok(NrcHeader {
        version: u32::from_le_bytes(data[4..8].try_into().unwrap()),
    })
}

struct NrcHeader {
    version: u32,
}

fn write_collection(w: &mut PayloadWriter, coll: &nrclip::Collection, ver: u32) {
    // Collection header
    if ver >= 71 {
        w.write_varint(coll.id_a);
        w.write_varint(coll.id_b);
        w.write_optional_mod_source(&coll.mod_source);
    }
    if ver >= 66 { w.write_string(&coll.name); }
    if ver >= 66 { w.write_varint(coll.clips.len() as u64); }
    for clip in &coll.clips {
        write_clip(w, clip, ver);
    }
}

fn write_clip(w: &mut PayloadWriter, clip: &nrclip::Clip, ver: u32) {
    if ver >= 66 { w.write_string(&clip.guid); }
    if ver >= 66 { w.write_varint(clip.clip_id); }
    if ver >= 147 {
        w.write_f64(clip.center_x);
        w.write_f64(clip.center_y);
    }

    // vec<Track>
    if ver >= 66 {
        w.write_varint(clip.tracks.len() as u64);
        for t in &clip.tracks {
            write_track(w, t, ver);
        }
    }
    // vec<Signal> (v>=198)
    if ver >= 198 {
        w.write_varint(clip.signals.len() as u64);
        for s in &clip.signals {
            write_signal(w, s, ver);
        }
    }
    // vec<StationGroup> (v>=66)
    if ver >= 66 {
        w.write_varint(clip.station_groups.len() as u64);
        for sg in &clip.station_groups {
            write_station_group(w, sg, ver);
        }
    }
    // vec<Building> (v>=66)
    if ver >= 66 {
        w.write_varint(clip.buildings.len() as u64);
        for b in &clip.buildings {
            write_building(w, b, ver);
        }
    }
    // map<int, TrackKind> (v>=66)
    if ver >= 66 {
        w.write_varint(clip.track_kinds.len() as u64);
        for (key, tk) in &clip.track_kinds {
            w.write_i32z(*key);
            write_track_kind(w, tk, ver);
        }
    }
    // map<int, BuildingKind> (v>=66)
    if ver >= 66 {
        w.write_varint(clip.building_kinds.len() as u64);
        for (key, bk) in &clip.building_kinds {
            w.write_i32z(*key);
            write_building_kind(w, bk, ver);
        }
    }
    // map<u64, Demand> (v>=158)
    if ver >= 158 {
        w.write_varint(clip.demands.len() as u64);
        for (key, d) in &clip.demands {
            w.write_varint(*key);
            write_demand(w, d, ver);
        }
    }
    // vec<ModMeta> (v>=66)
    if ver >= 66 {
        w.write_varint(clip.mod_metas.len() as u64);
        for m in &clip.mod_metas {
            write_mod_meta(w, m, ver);
        }
    }
}

fn write_track(w: &mut PayloadWriter, t: &nrclip::Track, ver: u32) {
    w.write_i64z(t.node_id);
    if ver >= 30 { w.write_raw_u8(t.node_type); }
    if ver < 30 { w.write_i64z(0); }
    if ver >= 30 { w.write_i32z(t.track_type); }
    if ver < 30 { w.write_i64z(0); }
    if ver >= 45 { w.write_i32z(t.layer); }
    if ver >= 122 { w.write_raw_u8(t.winding.unwrap_or(0)); }
    w.write_i64z(t.prev_node);
    w.write_i64z(t.next_node);
    if ver >= 13 { w.write_i64z(t.group_id); }

    if ver >= 72 { w.write_f32(t.user_max_speed.unwrap_or(0.0)); }
    w.write_f64(t.x);
    w.write_f64(t.y);
    if (102..=105).contains(&ver) { w.write_f32(0.0); }
    if ver >= 102 { w.write_f32(t.user_tangent_delta.unwrap_or(0.0)); }
    if ver >= 141 { w.write_f32(t.next_spline_t.unwrap_or(0.5)); }

    // Post-coordinate
    w.write_i64z(t.station_group_id);
    if ver >= 108 { w.write_i32z(t.blueprint.unwrap_or(0)); }
    if ver >= 63 {
        w.write_string(t.name.as_deref().unwrap_or(""));
        w.write_raw_u8(t.station_platform_auto_name.unwrap_or(0));
    }
    if (170..=181).contains(&ver) { w.write_f32(0.0); }
    if (15..=91).contains(&ver) { w.write_raw_u8(0); }
    if ver >= 62 { w.write_raw_u8(t.straight.unwrap_or(0)); }
    if ver >= 143 { w.write_raw_u8(t.tangential.unwrap_or(0)); }
    if ver >= 144 { w.write_raw_u8(t.limited_shapes.unwrap_or(0)); }

    // Conflicts
    if ver >= 28 {
        for cv in &t.conflicts {
            w.write_varint(cv.len() as u64);
            for c in cv {
                w.write_i64z(c.mode as i64);
                w.write_i64z(c.track_id);
                if (28..192).contains(&ver) { w.write_i64z(0); }
                w.write_i32z(c.lat);
                w.write_i32z(c.lon);
                w.write_f32(c.t_self);
                if ver >= 192 {
                    w.write_f32(c.t_other.unwrap_or(0.0));
                    w.write_f32(c.clearance.unwrap_or(0.0));
                    w.write_f32(c.height_delta.unwrap_or(0.0));
                }
                w.write_f32(c.overlap_dist);
            }
        }
    }

    // Signal area
    if (32..=197).contains(&ver) {
        w.write_vec_set_i64(t.embedded_signals.as_deref().unwrap_or(&[]));
    }
    if ver >= 198 {
        w.write_vec_set_i64(t.signal_ids.as_deref().unwrap_or(&[]));
    }

    w.write_i64z(t.attached_to_id);
    w.write_f64(t.attached_to_t);
    if ver >= 30 { w.write_i32z(t.attached_to_direction.unwrap_or(0)); }
    w.write_vec_set_i64(&t.attached_by);
    if ver >= 62 { w.write_vec_set_i64(t.building_attached_by.as_deref().unwrap_or(&[])); }

    if ver >= 33 {
        w.write_i64z(t.parallel_to_id.unwrap_or(0));
        w.write_i64z(t.parallel_kind.unwrap_or(0) as i64);
        w.write_f32(t.parallel_to_t.unwrap_or(0.0));
        w.write_i32z(t.parallel_to_direction.unwrap_or(0));
        w.write_f32(t.parallel_to_offset.unwrap_or(0.0));
    }
    if ver >= 60 { w.write_f32(t.parallel_to_disp.unwrap_or(0.0)); }
    if ver >= 33 { w.write_vec_set_i64(t.parallel_by.as_deref().unwrap_or(&[])); }
    if ver >= 192 { w.write_f32(t.proximity_diamond.unwrap_or(0.0)); }
}

fn write_signal(w: &mut PayloadWriter, s: &nrclip::Signal, ver: u32) {
    w.write_i64z(s.id);
    if ver >= 202 { w.write_raw_u8(s.kind_enum.unwrap_or(0)); }
    if ver >= 202 { w.write_string(s.name.as_deref().unwrap_or("")); }
    if ver >= 32 { w.write_i32z(s.kind); }
    if ver >= 211 { w.write_varint(s.signal_textures_hash.unwrap_or(0)); }
    if ver >= 32 { w.write_i64z(s.pos_track_id); }
    if ver >= 32 { w.write_f64(s.pos_t); }
    if (32..205).contains(&ver) { w.write_i32z(0); }
    if ver >= 205 {
        w.write_raw_u8(s.dir_a.unwrap_or(0) as u8);
        w.write_raw_u8(s.dir_b.unwrap_or(0) as u8);
    }
    if ver >= 32 { w.write_i32z(s.side); }
    if ver >= 214 { w.write_i32z(s.size.unwrap_or(0)); }
    if ver >= 214 { w.write_i32z(s.rotate.unwrap_or(0)); }
    if (44..=50).contains(&ver) { w.write_raw_u8(0); }
    if ver >= 55 {
        w.write_raw_u8(s.custom_alert_wait.unwrap_or(0));
        w.write_i32z(s.alert_wait.unwrap_or(0));
    }
    if ver >= 37 { w.write_raw_u8(s.match_block_facing.unwrap_or(0)); }
    if ver >= 194 { w.write_raw_u8(s.check_beyond_stops.unwrap_or(0)); }
    if ver >= 53 {
        w.write_i32z(s.filter.unwrap_or(0));
        let tags = s.filter_exception_tags.as_deref().unwrap_or(&[]);
        w.write_varint(tags.len() as u64);
        for &t in tags { w.write_varint(t); }
    }
    if ver >= 204 { w.write_varint(s.scripts.unwrap_or(0)); }
}

fn write_station_group(w: &mut PayloadWriter, sg: &nrclip::StationGroup, ver: u32) {
    w.write_i64z(sg.id);
    if ver >= 11 { w.write_i32z(sg.created_on.unwrap_or(0)); }
    if ver >= 182 { w.write_raw_u8(sg.use_automatic_point.unwrap_or(0)); }
    if ver >= 182 {
        let (px, py) = sg.position.unwrap_or((0.0, 0.0));
        w.write_f64(px);
        w.write_f64(py);
    }
    w.write_string(&sg.name);
    w.write_raw_u8(sg.use_automatic_name);
    if ver >= 57 { w.write_i32z(sg.geo_name_pick.unwrap_or(0)); }
    if ver >= 182 { w.write_vec_set_i64(sg.tags.as_deref().unwrap_or(&[])); }
    w.write_vec_set_i64(&sg.track_ids);
    if ver >= 167 { w.write_vec_set_i64(sg.building_ids.as_deref().unwrap_or(&[])); }
    if ver >= 195 { w.write_vec_set_i64(sg.extra_ids.as_deref().unwrap_or(&[])); }
    if ver >= 4 { w.write_f32(sg.size_factor.unwrap_or(1.0)); }
    if ver >= 163 { w.write_f32(sg.walk_factor.unwrap_or(1.0)); }
    if ver >= 165 {
        w.write_varint(sg.max_platform_pax.unwrap_or(0) as u64);
        w.write_varint(sg.transfer_overflow_into_hall.unwrap_or(0) as u64);
    }
    if ver >= 94 { w.write_i32z(sg.label_mode.unwrap_or(0)); }
    if ver >= 208 { w.write_varint(sg.scripts.unwrap_or(0)); }
}

fn write_building(w: &mut PayloadWriter, b: &nrclip::Building, ver: u32) {
    w.write_i64z(b.id);
    w.write_i32z(b.kind_idx);
    if ver >= 69 { w.write_i32z(b.kind_decal_idx.unwrap_or(0)); }
    w.write_i64z(b.owner);
    w.write_i32z(b.created_on);
    w.write_i32z(b.layer);
    if ver >= 63 { w.write_i32z(b.draw_layer.unwrap_or(0)); }
    w.write_raw_u8(b.blueprint);
    w.write_f64(b.x);
    w.write_f64(b.y);
    w.write_f32(b.rotation_sin);
    w.write_f32(b.rotation_cos);
    w.write_f32(b.size_x);
    w.write_f32(b.size_y);
    w.write_varint(b.color as u64);
    if ver >= 69 { w.write_varint(b.decal_color.unwrap_or(0) as u64); }

    // POI
    if ver >= 67 {
        match &b.poi {
            Some(poi) => {
                w.write_raw_u8(1);
                w.write_string(&poi.name);
                w.write_i32z(poi.font_size);
                if ver >= 68 { w.write_i32z(poi.max_zoom); }
                w.write_raw_u8(poi.fill_background);
                if ver >= 158 {
                    match poi.demand_curve {
                        Some(v) => { w.write_raw_u8(1); w.write_varint(v); }
                        None => w.write_raw_u8(0),
                    }
                    w.write_varint(poi.population.unwrap_or(0) as u64);
                }
            }
            None => w.write_raw_u8(0),
        }
    }

    w.write_i64z(b.attached_to_track_id);
    w.write_f32(b.start_t);
    w.write_f32(b.end_t);
    w.write_f32(b.bottom_side);
    w.write_f32(b.top_side);
    w.write_raw_u8(b.attached_curved);
}

fn write_track_kind(w: &mut PayloadWriter, tk: &nrclip::TrackKind, _ver: u32) {
    w.write_string(&tk.display_name);
    w.write_raw_u8(tk.speed_class_flag);
    w.write_i32z(tk.speed_class);
    w.write_string(&tk.internal_name);
    w.write_string(&tk.secondary_name);
    for h in &tk.horizons {
        w.write_i32z(h.speed_class);
        w.write_f64(h.gauge);
        w.write_f64(h.height);
        w.write_f64(h.max_speed);
        w.write_f64(h.width_a);
        w.write_f64(h.width_b);
        w.write_f64(h.spacing);
        w.write_f64(h.offset_a);
        w.write_f64(h.offset_b);
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
    w.write_f32(bk.size_x);
    w.write_f32(bk.size_y);
    if (62..=181).contains(&ver) { w.write_raw_u8(0); }
    if (167..=181).contains(&ver) { w.write_raw_u8(0); }
    if ver >= 65 { w.write_raw_u8(bk.curved.unwrap_or(0)); }
    w.write_raw_u8(bk.recolor);
    if ver >= 67 { w.write_raw_u8(bk.is_poi.unwrap_or(0)); }
    if ver >= 69 {
        w.write_raw_u8(bk.has_default_size.unwrap_or(0));
        w.write_raw_u8(bk.decal_count.unwrap_or(0));
    }
    w.write_i32z(bk.border_x);
    w.write_i32z(bk.border_x); // read twice (game quirk)
    if ver >= 69 {
        w.write_f32(bk.lod_x);
        w.write_f32(bk.lod_y);
        w.write_varint(bk.sentinel as u64);
        w.write_f32(bk.offset_neg);
        w.write_f32(bk.offset_pos);
        w.write_raw_u8(bk.decal_count.unwrap_or(0)); // re-read (game quirk)
        w.write_varint(bk.scripts.len() as u64);
        for s in &bk.scripts { w.write_string(s); }
    }
    w.write_i32z(bk.rule_x);
    w.write_i32z(bk.rule_y);
    if ver >= 63 {
        w.write_raw_u8(bk.partial_repeat_x.unwrap_or(1));
        w.write_raw_u8(bk.partial_repeat_y.unwrap_or(1));
    }
    w.write_i32z(bk.default_draw_layer);
    w.write_mod_source_pair(bk.texture.workshop_id, &bk.texture.path);
    w.write_string(&bk.model_path);
    if ver == 62 { w.write_mod_source_pair(0, ""); w.write_string(""); }
}

fn write_demand(w: &mut PayloadWriter, d: &nrclip::Demand, ver: u32) {
    w.write_varint(d.poi_layer_id);
    if ver >= 159 {
        w.write_raw_u8(d.is_mod.unwrap_or(0));
        w.write_optional_mod_source(&d.mod_source);
    }
    w.write_string(&d.name);
    for &v in &d.time_a { w.write_f32(v); }
    for &v in &d.time_b { w.write_f32(v); }
    w.write_varint(d.distance_ranges.len() as u64);
    for r in &d.distance_ranges {
        w.write_i32z(r.min_distance);
        w.write_i32z(r.max_distance);
        w.write_i32z(r.step);
        w.write_varint(r.values.len() as u64);
        for &v in &r.values { w.write_f32(v); }
    }
}

fn write_mod_meta(w: &mut PayloadWriter, m: &nrclip::ModMeta, ver: u32) {
    w.write_i64z(m.source_id);
    w.write_string(&m.source_path);
    w.write_string(&m.folder);
    w.write_string(&m.display_name);
    w.write_string(&m.author);
    w.write_string(&m.description);
    w.write_string(&m.version);
    w.write_string(&m.tag);
    w.write_vec_set_i64(&m.provides);
    if ver >= 117 {
        w.write_varint(m.content_items.len() as u64);
        for (kt, kn, vn) in &m.content_items {
            w.write_i32z(*kt);
            w.write_string(kn);
            w.write_string(vn);
        }
    }
    w.write_raw_u8(m.content_loaded);
    w.write_raw_u8(m.has_local_data);
}
