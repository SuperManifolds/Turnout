use tauri::Emitter;
use tauri_plugin_store::StoreExt;

const SETTINGS_STORE: &str = "settings.json";

#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct Settings {
    pub mods_dir_override: Option<String>,
    pub check_for_updates: bool,
    pub map_theme: String,
    #[serde(default)]
    pub apple_access_key: Option<String>,
    #[serde(default)]
    pub apple_map_version: Option<String>,
    #[serde(default)]
    pub apple_sat_version: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mods_dir_override: None,
            check_for_updates: true,
            map_theme: "system".to_string(),
            apple_access_key: None,
            apple_map_version: None,
            apple_sat_version: None,
        }
    }
}

pub fn load(app: &tauri::AppHandle) -> Settings {
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
        apple_access_key: store.get("apple_access_key")
            .and_then(|v| v.as_str().map(String::from)),
        apple_map_version: store.get("apple_map_version")
            .and_then(|v| v.as_str().map(String::from)),
        apple_sat_version: store.get("apple_sat_version")
            .and_then(|v| v.as_str().map(String::from)),
    }
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn get_settings(app: tauri::AppHandle) -> Settings {
    load(&app)
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn set_settings(app: tauri::AppHandle, settings: Settings) -> Result<(), String> {
    let store = app.store(SETTINGS_STORE).map_err(|e| e.to_string())?;
    store.set("mods_dir_override", serde_json::json!(settings.mods_dir_override));
    store.set("check_for_updates", serde_json::json!(settings.check_for_updates));
    store.set("map_theme", serde_json::json!(settings.map_theme));
    store.set("apple_access_key", serde_json::json!(settings.apple_access_key));
    store.set("apple_map_version", serde_json::json!(settings.apple_map_version));
    store.set("apple_sat_version", serde_json::json!(settings.apple_sat_version));
    store.save().map_err(|e| e.to_string())?;
    let _ = app.emit("settings-changed", &settings);
    Ok(())
}

#[tauri::command]
pub async fn pick_folder(app: tauri::AppHandle) -> Option<String> {
    use tauri_plugin_dialog::DialogExt;
    let path = app.dialog().file().blocking_pick_folder()?;
    Some(path.to_string())
}
