use anyhow::{Context, Result};
use binrw::BinRead;
use std::io::{Read, Seek, SeekFrom};
use std::{env, fs::File, io::BufReader};

mod nrclip;
use nrclip::parse_payload;

#[derive(BinRead, Debug)]
#[br(little, magic = b"NRC1")]
struct NrcHeader {
    version: u32,
    uncompressed_size: u64,
    compressed_size: u64,
    checksum: u64,
}

const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

fn decompress_zstd<R: Read + Seek>(mut r: R, header: &NrcHeader) -> Result<Vec<u8>> {
    let zstd_offset = r.stream_position()?;
    let mut magic = [0u8; 4];
    r.read_exact(&mut magic).context("reading zstd magic")?;
    if magic != ZSTD_MAGIC {
        anyhow::bail!("zstd magic not found at 0x{:X} (got {:02X?})", zstd_offset, magic);
    }
    r.seek(SeekFrom::Start(zstd_offset))?;
    let data = zstd::stream::decode_all(&mut r).context("zstd decode_all")?;
    if data.len() != header.uncompressed_size as usize {
        eprintln!("warning: decompressed {} bytes, header says {}", data.len(), header.uncompressed_size);
    }
    Ok(data)
}

fn main() -> Result<()> {
    let path = env::args()
        .nth(1)
        .unwrap_or_else(|| "2949234540/blueprints.nrclip".to_string());

    let f = File::open(&path).with_context(|| format!("open {}", path))?;
    let mut r = BufReader::new(f);
    let header: NrcHeader = BinRead::read(&mut r).context("parse NRC1 header")?;

    println!("=== NRC1 Container ===");
    println!("  Model version:     {}", header.version);
    println!("  Uncompressed size: {} bytes", header.uncompressed_size);
    println!("  Compressed size:   {} bytes", header.compressed_size);
    println!("  Checksum:          {:#018X}\n", header.checksum);

    let buf = decompress_zstd(&mut r, &header)?;
    println!("Decompressed {} bytes\n", buf.len());

    let collections = parse_payload(&buf, header.version)
        .context("parsing payload")?;

    for (ci, coll) in collections.iter().enumerate() {
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

            // Tracks
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

            // Signals
            if !clip.signals.is_empty() {
                println!("    Signals: {}", clip.signals.len());
                for s in clip.signals.iter().take(3) {
                    println!("      Signal id={} kind={:?}", s.id, s.kind);
                }
                if clip.signals.len() > 3 {
                    println!("      ... ({} more)", clip.signals.len() - 3);
                }
            }

            // Stations
            if !clip.station_groups.is_empty() {
                println!("    Stations: {}", clip.station_groups.len());
                for s in &clip.station_groups {
                    println!("      {}", s);
                }
            }

            // Buildings
            if !clip.buildings.is_empty() {
                println!("    Buildings: {}", clip.buildings.len());
                for b in clip.buildings.iter().take(3) {
                    println!("      {}", b);
                }
                if clip.buildings.len() > 3 {
                    println!("      ... ({} more)", clip.buildings.len() - 3);
                }
            }

            // Track kinds
            if !clip.track_kinds.is_empty() {
                println!("    Track kinds: {}", clip.track_kinds.len());
                for (k, v) in &clip.track_kinds {
                    println!("      [{}] {} horizons: [{:.1}, {:.1}, {:.1}] km/h",
                        k, v, v.horizons[0].max_speed, v.horizons[1].max_speed, v.horizons[2].max_speed);
                }
            }

            // Building kinds
            if !clip.building_kinds.is_empty() {
                println!("    Building kinds: {}", clip.building_kinds.len());
                for (k, v) in &clip.building_kinds {
                    println!("      [{}] {}", k, v);
                }
            }

            // Demands
            if !clip.demands.is_empty() {
                println!("    Demands: {}", clip.demands.len());
            }

            // Mods
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
