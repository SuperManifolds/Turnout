use anyhow::{Context, Result};
use std::fs;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let json_path = args.get(1).context("usage: import_orm <tracks.json> [output.nrclip]")?;
    let output = args.get(2).map(|s| s.as_str()).unwrap_or("orm_import.nrclip");
    let blueprint_name = std::path::Path::new(output)
        .file_stem().and_then(|s| s.to_str()).unwrap_or("orm_import")
        .replace('_', " ");

    let json = fs::read_to_string(json_path).context("read JSON")?;
    let file_data = nimby_gen_core::import::import_orm(&json, &blueprint_name)?;

    fs::write(output, &file_data)?;
    println!("Wrote {} bytes to {}", file_data.len(), output);

    // Verify round-trip
    let decoded = nimby_gen_core::nrc1::NrclipFile::from_bytes(&file_data)?;
    let total: usize = decoded.collections.iter().flat_map(|c| &c.clips).map(|c| c.tracks.len()).sum();
    println!("Verified: {} tracks", total);

    // Run comparison
    match std::process::Command::new("cargo")
        .args(["run", "--bin", "compare_orm", "--", output, json_path.as_str()])
        .status() {
        Ok(s) if s.success() => {},
        _ => eprintln!("Warning: compare_orm failed"),
    }

    Ok(())
}
