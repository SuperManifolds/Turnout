use anyhow::{Context, Result};
use std::{env, fs};

mod encode;
mod nrclip;

// Re-use inject's write functions
include!("inject_writers.rs");

const MODEL_VERSION: u32 = 226;
const WYHASH_SEED: u64 = 0x9c3805fc2c85cacc;

fn main() -> Result<()> {
    let input = env::args().nth(1).context("usage: roundtrip <input.nrclip> <output.nrclip>")?;
    let output = env::args().nth(2).context("usage: roundtrip <input.nrclip> <output.nrclip>")?;

    let raw = fs::read(&input)?;
    let ver = u32::from_le_bytes(raw[4..8].try_into().unwrap());
    let payload = zstd::stream::decode_all(&raw[32..]).context("decompress")?;
    let collections = nrclip::parse_payload(&payload, ver)?;
    println!("Decoded: {} collections from v{}", collections.len(), ver);

    // Re-encode at v226
    let mut w = encode::PayloadWriter::new();
    w.write_varint(collections.len() as u64);
    for coll in &collections {
        write_collection(&mut w, coll, MODEL_VERSION);
    }
    let new_payload = w.into_bytes();
    
    // Verify
    let check = nrclip::parse_payload(&new_payload, MODEL_VERSION)?;
    println!("Re-encoded: {} collections, verified", check.len());

    let compressed = {
        let mut enc = zstd::stream::Encoder::new(Vec::new(), 3)?;
        enc.include_contentsize(true)?;
        enc.set_pledged_src_size(Some(new_payload.len() as u64))?;
        std::io::copy(&mut new_payload.as_slice(), &mut enc)?;
        enc.finish()?
    };
    let checksum = wyhash::wyhash(&new_payload, WYHASH_SEED);
    let mut out = Vec::new();
    out.extend_from_slice(b"NRC1");
    out.extend_from_slice(&MODEL_VERSION.to_le_bytes());
    out.extend_from_slice(&(new_payload.len() as u64).to_le_bytes());
    out.extend_from_slice(&(compressed.len() as u64).to_le_bytes());
    out.extend_from_slice(&checksum.to_le_bytes());
    out.extend_from_slice(&compressed);
    fs::write(&output, &out)?;
    println!("Wrote {} bytes to {}", out.len(), output);
    Ok(())
}
