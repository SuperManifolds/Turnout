#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::fs;
use std::path::PathBuf;
use tauri::menu::{MenuBuilder, SubmenuBuilder, MenuItemBuilder};
use tauri::{Emitter, Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_store::StoreExt;

const SETTINGS_STORE: &str = "settings.json";

#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct Settings {
    mods_dir_override: Option<String>,
    check_for_updates: bool,
    map_theme: String, // "system", "light", "dark"
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mods_dir_override: None,
            check_for_updates: true,
            map_theme: "system".to_string(),
        }
    }
}

// ══════════════════════════════════════════════════════════════════════
// Commands
// ══════════════════════════════════════════════════════════════════════

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
fn get_settings(app: tauri::AppHandle) -> Settings {
    load_settings(&app)
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
fn set_settings(app: tauri::AppHandle, settings: Settings) -> Result<(), String> {
    let store = app.store(SETTINGS_STORE).map_err(|e| e.to_string())?;
    store.set("mods_dir_override", serde_json::json!(settings.mods_dir_override));
    store.set("check_for_updates", serde_json::json!(settings.check_for_updates));
    store.set("map_theme", serde_json::json!(settings.map_theme));
    store.save().map_err(|e| e.to_string())?;
    // Notify all windows that settings changed
    let _ = app.emit("settings-changed", &settings);
    Ok(())
}

#[tauri::command]
async fn pick_folder(app: tauri::AppHandle) -> Option<String> {
    use tauri_plugin_dialog::DialogExt;
    let path = app.dialog().file().blocking_pick_folder()?;
    Some(path.to_string())
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
fn import_orm(app: tauri::AppHandle, json: String, name: String, railway_types: Vec<String>, apply_speed_limits: bool, clip_bbox: Option<(f64, f64, f64, f64)>) -> Result<Vec<u8>, String> {
    let (track_kinds, mod_metas) = resolve_mods_dir(&app)
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
#[allow(clippy::needless_pass_by_value)]
fn get_mods_dir(app: tauri::AppHandle) -> Option<String> {
    resolve_mods_dir(&app).map(|p| p.to_string_lossy().to_string())
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
fn save_blueprint(app: tauri::AppHandle, name: String, data: Vec<u8>) -> Result<String, String> {
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

// ══════════════════════════════════════════════════════════════════════
// Settings persistence
// ══════════════════════════════════════════════════════════════════════

fn load_settings(app: &tauri::AppHandle) -> Settings {
    let Ok(store) = app.store(SETTINGS_STORE) else {
        return Settings::default();
    };
    Settings {
        mods_dir_override: store.get("mods_dir_override")
            .and_then(|v| v.as_str().map(String::from)),
        check_for_updates: store.get("check_for_updates")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
        map_theme: store.get("map_theme")
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| "system".to_string()),
    }
}

fn resolve_mods_dir(app: &tauri::AppHandle) -> Option<PathBuf> {
    let settings = load_settings(app);
    if let Some(ref override_path) = settings.mods_dir_override {
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
        let bottles_dir = home.join("Library/Application Support/CrossOver/Bottles");
        if let Ok(bottles) = fs::read_dir(&bottles_dir) {
            for bottle in bottles.flatten() {
                let drive_c = bottle.path().join("drive_c/users");
                if let Ok(users) = fs::read_dir(&drive_c) {
                    for user in users.flatten() {
                        candidates.push(user.path().join("Saved Games/Weird and Wry/NIMBY Rails/mods"));
                    }
                }
            }
        }
        let whisky_dir = home.join("Library/Containers/com.isaacmarovitz.Whisky/Bottles");
        if let Ok(bottles) = fs::read_dir(&whisky_dir) {
            for bottle in bottles.flatten() {
                let drive_c = bottle.path().join("drive_c/users");
                if let Ok(users) = fs::read_dir(&drive_c) {
                    for user in users.flatten() {
                        candidates.push(user.path().join("Saved Games/Weird and Wry/NIMBY Rails/mods"));
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

// ══════════════════════════════════════════════════════════════════════
// Menu + window management
// ══════════════════════════════════════════════════════════════════════

fn open_settings_window(app: &tauri::AppHandle) {
    if let Some(window) = app.webview_windows().get("settings") {
        let _ = window.set_focus();
        return;
    }
    let _ = WebviewWindowBuilder::new(app, "settings", WebviewUrl::App("index.html".into()))
        .title("Settings")
        .inner_size(480.0, 380.0)
        .resizable(false)
        .maximizable(false)
        .build();
}

fn setup_menu(app: &tauri::AppHandle) -> tauri::Result<()> {
    let app_submenu = SubmenuBuilder::new(app, "Turnout")
        .about(None)
        .separator()
        .item(
            &MenuItemBuilder::new("Settings...")
                .id("settings")
                .accelerator("CmdOrCtrl+,")
                .build(app)?
        )
        .separator()
        .services()
        .separator()
        .hide()
        .hide_others()
        .show_all()
        .separator()
        .quit()
        .build()?;

    let edit_submenu = SubmenuBuilder::new(app, "Edit")
        .undo()
        .redo()
        .separator()
        .cut()
        .copy()
        .paste()
        .select_all()
        .build()?;

    let window_submenu = SubmenuBuilder::new(app, "Window")
        .minimize()
        .separator()
        .close_window()
        .build()?;

    let menu = MenuBuilder::new(app)
        .items(&[&app_submenu, &edit_submenu, &window_submenu])
        .build()?;

    app.set_menu(menu)?;
    Ok(())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_store::Builder::new().build())
        .setup(|app| {
            setup_menu(app.handle())?;
            Ok(())
        })
        .on_menu_event(|app, event| {
            if event.id() == "settings" {
                open_settings_window(app);
            }
        })
        .invoke_handler(tauri::generate_handler![
            import_orm, get_mods_dir, save_blueprint,
            get_settings, set_settings, pick_folder,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
