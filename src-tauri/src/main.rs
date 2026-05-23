#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::fs;
use std::path::PathBuf;

#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri deserializes owned types
fn import_orm(json: String, name: String, railway_types: Vec<String>, apply_speed_limits: bool, clip_bbox: Option<(f64, f64, f64, f64)>) -> Result<Vec<u8>, String> {
    // Extract vanilla track kinds from game files
    let (track_kinds, mod_metas) = find_mods_dir()
        .and_then(|mods| {
            let collections = mods.parent()?.join("collections.nrclip");
            if collections.exists() { Some(collections) } else { None }
        })
        .and_then(|path| {
            turnout_core::import::extract_vanilla_track_kinds(&path.to_string_lossy()).ok()
        })
        .unwrap_or_default();

    turnout_core::import::import_orm(&json, &name, &railway_types, apply_speed_limits, clip_bbox, track_kinds, mod_metas)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn get_mods_dir() -> Option<String> {
    find_mods_dir().map(|p| p.to_string_lossy().to_string())
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri deserializes owned types
fn save_blueprint(name: String, data: Vec<u8>) -> Result<String, String> {
    let mods_dir = find_mods_dir()
        .ok_or_else(|| "Could not find Nimby Rails mods folder".to_string())?;

    let blueprint_dir = mods_dir.join(&name);
    fs::create_dir_all(&blueprint_dir)
        .map_err(|e| format!("Failed to create directory: {e}"))?;

    let path = blueprint_dir.join("blueprints.nrclip");
    fs::write(&path, &data)
        .map_err(|e| format!("Failed to write file: {e}"))?;

    Ok(path.to_string_lossy().to_string())
}

fn find_mods_dir() -> Option<PathBuf> {
    let home = dirs_next::home_dir()?;

    // Platform-specific search paths
    let mut candidates: Vec<PathBuf> = Vec::new();

    // Windows: %USERPROFILE%/Saved Games/Weird and Wry/NIMBY Rails/mods
    #[cfg(target_os = "windows")]
    {
        candidates.push(home.join("Saved Games/Weird and Wry/NIMBY Rails/mods"));
    }

    // macOS: CrossOver bottles
    #[cfg(target_os = "macos")]
    {
        let bottles_dir = home.join("Library/Application Support/CrossOver/Bottles");
        if let Ok(bottles) = fs::read_dir(&bottles_dir) {
            for bottle in bottles.flatten() {
                let drive_c = bottle.path().join("drive_c/users");
                if let Ok(users) = fs::read_dir(&drive_c) {
                    for user in users.flatten() {
                        let mods = user.path().join("Saved Games/Weird and Wry/NIMBY Rails/mods");
                        candidates.push(mods);
                    }
                }
            }
        }
        // Whisky
        let whisky_dir = home.join("Library/Containers/com.isaacmarovitz.Whisky/Bottles");
        if let Ok(bottles) = fs::read_dir(&whisky_dir) {
            for bottle in bottles.flatten() {
                let drive_c = bottle.path().join("drive_c/users");
                if let Ok(users) = fs::read_dir(&drive_c) {
                    for user in users.flatten() {
                        let mods = user.path().join("Saved Games/Weird and Wry/NIMBY Rails/mods");
                        candidates.push(mods);
                    }
                }
            }
        }
    }

    // Linux: Wine prefixes + Proton
    #[cfg(target_os = "linux")]
    {
        // Standard Wine
        let wine_users = home.join(".wine/drive_c/users");
        if let Ok(users) = fs::read_dir(&wine_users) {
            for user in users.flatten() {
                candidates.push(user.path().join("Saved Games/Weird and Wry/NIMBY Rails/mods"));
            }
        }
        // Proton (native Steam)
        candidates.push(home.join(".local/share/Steam/steamapps/compatdata/1134710/pfx/drive_c/users/steamuser/Saved Games/Weird and Wry/NIMBY Rails/mods"));
        // Proton (Flatpak Steam)
        candidates.push(home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam/steamapps/compatdata/1134710/pfx/drive_c/users/steamuser/Saved Games/Weird and Wry/NIMBY Rails/mods"));
    }

    candidates.into_iter().find(|p| p.exists())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![import_orm, get_mods_dir, save_blueprint])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
