fn main() {
    let path = std::env::args().nth(1).expect("usage: check_coords <file.nrclip>");
    let data = std::fs::read(&path).expect("read");
    let file = turnout_core::nrc1::NrclipFile::from_bytes(&data).expect("parse");
    for (ci, coll) in file.collections.iter().enumerate() {
        for (cli, clip) in coll.clips.iter().enumerate() {
            eprintln!("Coll {ci} clip {cli} \"{}\": tracks={} kinds={:?}",
                coll.name, clip.tracks.len(),
                clip.track_kinds.iter().map(|(k,_)| k).collect::<Vec<_>>());

            for (key, kind) in &clip.track_kinds {
                eprintln!("  TrackKind[{key}]: display=\"{}\" internal=\"{}\" secondary=\"{}\" speed_class_flag={} speed_class={}",
                    kind.display_name, kind.internal_name, kind.secondary_name,
                    kind.speed_class_flag, kind.speed_class);
                for (hi, h) in kind.horizons.iter().enumerate() {
                    eprintln!("    horizon[{hi}]: gauge={} height={} max_speed={} widths=({},{}) spacing={} offsets=({},{}) vis_dist={} flags={:?}",
                        h.gauge, h.height, h.max_speed, h.width_a, h.width_b,
                        h.spacing, h.offset_a, h.offset_b, h.visual_distance, h.flags);
                    for (ti, tex) in h.textures.iter().enumerate() {
                        if tex.files.iter().any(|f| !f.path.is_empty()) {
                            eprintln!("      tex[{ti}]: speed_class={} files={:?}",
                                tex.speed_class,
                                tex.files.iter().map(|f| format!("{}:{}", f.workshop_id, &f.path)).collect::<Vec<_>>());
                        }
                    }
                }
            }

            for ti in 0..clip.tracks.len().min(3) {
                let t = &clip.tracks[ti];
                eprintln!("  track[{ti}]: id={} type={} pos=({:.1},{:.1})", t.node_id, t.track_type, t.x, t.y);
            }
        }
    }
}
