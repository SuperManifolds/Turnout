use anyhow::{Context, Result};
use std::env;

use nimby_gen_core::nrc1::NrclipFile;

fn main() -> Result<()> {
    let path = env::args()
        .nth(1)
        .unwrap_or_else(|| "2949234540/blueprints.nrclip".to_string());

    let data = std::fs::read(&path).with_context(|| format!("open {}", path))?;

    // Show NRC1 header info
    if data.len() >= 32 && &data[0..4] == b"NRC1" {
        let version = u32::from_le_bytes(data[4..8].try_into().unwrap());
        let uncompressed = u64::from_le_bytes(data[8..16].try_into().unwrap());
        let compressed = u64::from_le_bytes(data[16..24].try_into().unwrap());
        let checksum = u64::from_le_bytes(data[24..32].try_into().unwrap());
        println!("=== NRC1 Container ===");
        println!("  Model version:     {}", version);
        println!("  Uncompressed size: {} bytes", uncompressed);
        println!("  Compressed size:   {} bytes", compressed);
        println!("  Checksum:          {:#018X}\n", checksum);
    }

    let file = NrclipFile::from_bytes(&data).context("parsing nrclip")?;
    println!("Decompressed {} collections\n", file.collections.len());

    for (ci, coll) in file.collections.iter().enumerate() {
        println!("=== Collection {} ===", ci);
        println!("  ID: ({}, {})", coll.id_a, coll.id_b);
        println!("  Name: \"{}\"", coll.name);
        if let Some((src, path)) = &coll.mod_source {
            println!("  Mod source: {} \"{}\"", src, path);
        }

        for (cli, clip) in coll.clips.iter().enumerate() {
            println!("\n  --- Clip {} ---", cli);
            println!("    GUID: {}", clip.guid);
            println!("    ID: {}", clip.clip_id);
            println!("    Center: ({:.4}, {:.4})", clip.center_x, clip.center_y);

            println!("    Tracks: {}", clip.tracks.len());
            for t in clip.tracks.iter().take(3) {
                println!("      {}", t);
            }
            if clip.tracks.len() > 6 {
                println!("      ... ({} omitted)", clip.tracks.len() - 6);
                for t in clip.tracks.iter().rev().take(3).collect::<Vec<_>>().into_iter().rev() {
                    println!("      {}", t);
                }
            }

            if !clip.signals.is_empty() {
                println!("    Signals: {}", clip.signals.len());
                for s in clip.signals.iter().take(3) {
                    println!("      Signal id={} kind={:?}", s.id, s.kind);
                }
                if clip.signals.len() > 3 {
                    println!("      ... ({} more)", clip.signals.len() - 3);
                }
            }

            if !clip.station_groups.is_empty() {
                println!("    Stations: {}", clip.station_groups.len());
                for s in &clip.station_groups {
                    println!("      {}", s);
                }
            }

            if !clip.buildings.is_empty() {
                println!("    Buildings: {}", clip.buildings.len());
                for b in clip.buildings.iter().take(3) {
                    println!("      {}", b);
                }
                if clip.buildings.len() > 3 {
                    println!("      ... ({} more)", clip.buildings.len() - 3);
                }
            }

            if !clip.track_kinds.is_empty() {
                println!("    Track kinds: {}", clip.track_kinds.len());
                for (k, v) in &clip.track_kinds {
                    println!("      [{}] {} horizons: [{:.1}, {:.1}, {:.1}] km/h",
                        k, v, v.horizons[0].max_speed, v.horizons[1].max_speed, v.horizons[2].max_speed);
                }
            }

            if !clip.building_kinds.is_empty() {
                println!("    Building kinds: {}", clip.building_kinds.len());
                for (k, v) in &clip.building_kinds {
                    println!("      [{}] {}", k, v);
                }
            }

            if !clip.demands.is_empty() {
                println!("    Demands: {}", clip.demands.len());
            }

            if !clip.mod_metas.is_empty() {
                println!("    Mods: {}", clip.mod_metas.len());
                for m in &clip.mod_metas {
                    println!("      {}", m);
                }
            }
        }
    }

    Ok(())
}
