use anyhow::{Context, Result};
use std::fs;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let json_path = args.get(1).context("usage: import_orm <tracks.json> [output.nrclip]")?;
    let output = args.get(2).map_or("orm_import.nrclip", std::string::String::as_str);
    let blueprint_name = std::path::Path::new(output)
        .file_stem().and_then(|s| s.to_str()).unwrap_or("orm_import")
        .replace('_', " ");

    let json = fs::read_to_string(json_path).context("read JSON")?;
    // Try to find vanilla track kinds from the game's collections.nrclip
    let (track_kinds, mod_metas) = find_collections_nrclip()
        .and_then(|path| turnout_core::import::extract_vanilla_track_kinds(&path).ok())
        .unwrap_or_default();

    let (file_data, node_count) = turnout_core::import::import_orm(&json, &blueprint_name, &[], true, None, false, track_kinds, mod_metas)?;

    fs::write(output, &file_data)?;
    println!("Wrote {} bytes to {} ({node_count} / 50000 nodes)", file_data.len(), output);

    // Verify round-trip
    let decoded = turnout_core::nrc1::NrclipFile::from_bytes(&file_data)?;
    let total: usize = decoded.collections.iter().flat_map(|c| &c.clips).map(|c| c.tracks.len()).sum();
    println!("Verified: {total} tracks");

    // Run comparison
    match std::process::Command::new("cargo")
        .args(["run", "--bin", "compare_orm", "--", output, json_path.as_str()])
        .status() {
        Ok(s) if s.success() => {},
        _ => eprintln!("Warning: compare_orm failed"),
    }

    Ok(())
}

fn find_collections_nrclip() -> Option<String> {
    let home = dirs_next::home_dir()?;

    let candidates = vec![
        // macOS CrossOver
        {
            let mut paths = vec![];
            let bottles = home.join("Library/Application Support/CrossOver/Bottles");
            if let Ok(entries) = std::fs::read_dir(&bottles) {
                for entry in entries.flatten() {
                    let users = entry.path().join("drive_c/users");
                    if let Ok(u) = std::fs::read_dir(&users) {
                        for user in u.flatten() {
                            paths.push(user.path().join("Saved Games/Weird and Wry/NIMBY Rails/collections.nrclip"));
                        }
                    }
                }
            }
            paths
        },
        // Windows
        vec![home.join("Saved Games/Weird and Wry/NIMBY Rails/collections.nrclip")],
    ];

    for group in candidates {
        for path in group {
            if path.exists() {
                return Some(path.to_string_lossy().to_string());
            }
        }
    }
    None
}
