/// Inject a generated blueprint into an existing collections.nrclip file.
/// Decodes the original, appends our generated clip as a new collection,
/// re-encodes with fresh checksum.

use anyhow::{Context, Result};
use std::{env, fs};

use nimby_gen_core::nrc1::NrclipFile;

const MODEL_VERSION: u32 = 226;

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    let collections_path = args.get(1)
        .context("usage: inject <collections.nrclip> [generated.nrclip]")?;
    let generated_path = args.get(2)
        .map(|s| s.as_str())
        .unwrap_or("generated.nrclip");

    let mut orig = NrclipFile::from_bytes(&fs::read(collections_path)?)?;
    println!("Original: {} collections, v{}", orig.collections.len(), orig.version);

    let generated= NrclipFile::from_bytes(&fs::read(generated_path)?)?;
    let gen_clip_count: usize = generated.collections.iter()
        .flat_map(|c| &c.clips).count();
    println!("Generated: {} collections, {} clips", generated.collections.len(), gen_clip_count);

    orig.collections.extend(generated.collections);
    orig.version = MODEL_VERSION;

    let file_data = orig.to_bytes()?;

    // Verify round-trip
    let check = NrclipFile::from_bytes(&file_data)?;
    let total_tracks: usize = check.collections.iter()
        .flat_map(|c| &c.clips).map(|c| c.tracks.len()).sum();
    println!("Verified: {} collections, {} total tracks", check.collections.len(), total_tracks);

    fs::write(collections_path, &file_data)?;
    println!("Wrote {} bytes to {}", file_data.len(), collections_path);

    Ok(())
}
