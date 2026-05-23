use std::fs;
use std::path::PathBuf;

use crate::settings;

pub fn resolve_mods_dir(app: &tauri::AppHandle) -> Option<PathBuf> {
    let s = settings::load(app);
    if let Some(ref override_path) = s.mods_dir_override {
        let p = PathBuf::from(override_path);
        if p.exists() {
            return Some(p);
        }
    }
    find_mods_dir()
}

fn find_mods_dir() -> Option<PathBuf> {
    let home = dirs_next::home_dir()?;
    let mut candidates: Vec<PathBuf> = Vec::new();

    #[cfg(target_os = "windows")]
    {
        candidates.push(home.join("Saved Games/Weird and Wry/NIMBY Rails/mods"));
    }

    #[cfg(target_os = "macos")]
    {
        for base in &[
            home.join("Library/Application Support/CrossOver/Bottles"),
            home.join("Library/Containers/com.isaacmarovitz.Whisky/Bottles"),
        ] {
            if let Ok(bottles) = fs::read_dir(base) {
                for bottle in bottles.flatten() {
                    let users = bottle.path().join("drive_c/users");
                    if let Ok(entries) = fs::read_dir(&users) {
                        for user in entries.flatten() {
                            candidates.push(user.path().join("Saved Games/Weird and Wry/NIMBY Rails/mods"));
                        }
                    }
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        let wine_users = home.join(".wine/drive_c/users");
        if let Ok(users) = fs::read_dir(&wine_users) {
            for user in users.flatten() {
                candidates.push(user.path().join("Saved Games/Weird and Wry/NIMBY Rails/mods"));
            }
        }
        candidates.push(home.join(".local/share/Steam/steamapps/compatdata/1134710/pfx/drive_c/users/steamuser/Saved Games/Weird and Wry/NIMBY Rails/mods"));
        candidates.push(home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam/steamapps/compatdata/1134710/pfx/drive_c/users/steamuser/Saved Games/Weird and Wry/NIMBY Rails/mods"));
    }

    candidates.into_iter().find(|p| p.exists())
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn get_mods_dir(app: tauri::AppHandle) -> Option<String> {
    resolve_mods_dir(&app).map(|p| p.to_string_lossy().to_string())
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn save_blueprint(app: tauri::AppHandle, name: String, data: Vec<u8>) -> Result<String, String> {
    let mods_dir = resolve_mods_dir(&app)
        .ok_or_else(|| "Could not find Nimby Rails mods folder. Set it in Settings.".to_string())?;

    let blueprint_dir = mods_dir.join(&name);
    fs::create_dir_all(&blueprint_dir)
        .map_err(|e| format!("Failed to create directory: {e}"))?;

    let path = blueprint_dir.join("blueprints.nrclip");
    fs::write(&path, &data)
        .map_err(|e| format!("Failed to write file: {e}"))?;

    Ok(path.to_string_lossy().to_string())
}
