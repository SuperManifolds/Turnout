use anyhow::{Context, Result};
use std::{env, fs};

use turnout_core::nrc1::NrclipFile;

const MODEL_VERSION: u32 = 226;

fn main() -> Result<()> {
    let input = env::args().nth(1).context("usage: roundtrip <input.nrclip> <output.nrclip>")?;
    let output = env::args().nth(2).context("usage: roundtrip <input.nrclip> <output.nrclip>")?;

    let mut file = NrclipFile::from_bytes(&fs::read(&input)?)?;
    println!("Decoded: {} collections from v{}", file.collections.len(), file.version);

    file.version = MODEL_VERSION;
    let out_data = file.to_bytes()?;

    // Verify
    let check = NrclipFile::from_bytes(&out_data)?;
    println!("Re-encoded: {} collections, verified", check.collections.len());

    fs::write(&output, &out_data)?;
    println!("Wrote {} bytes to {}", out_data.len(), output);
    Ok(())
}
