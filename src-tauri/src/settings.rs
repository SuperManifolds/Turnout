use tauri::{Emitter, Manager};
use tauri_plugin_store::StoreExt;

const SETTINGS_STORE: &str = "settings.json";

/// Host serving `OpenRailwayMap` vector tiles, glyphs and sprites. Overridable via
/// [`Settings::orm_base_url`] for users running a self-hosted `OpenRailwayMap` stack.
pub const DEFAULT_ORM_BASE: &str = "https://openrailwaymap.app";

/// Resolves the configured ORM base URL, falling back to [`DEFAULT_ORM_BASE`] and
/// trimming a trailing slash so callers can append `/path` without doubling it.
pub fn resolve_orm_base(base: Option<&str>) -> String {
    let raw = base.map(str::trim).filter(|s| !s.is_empty()).unwrap_or(DEFAULT_ORM_BASE);
    raw.trim_end_matches('/').to_string()
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct Settings {
    pub mods_dir_override: Option<String>,
    pub check_for_updates: bool,
    pub map_theme: String,
    #[serde(default = "default_overpass_timeout")]
    pub overpass_timeout: u32,
    #[serde(default)]
    pub type_speed_overrides: std::collections::HashMap<String, u32>,
    #[serde(default)]
    pub apple_access_key: Option<String>,
    #[serde(default)]
    pub apple_map_version: Option<String>,
    #[serde(default)]
    pub apple_sat_version: Option<String>,
    #[serde(default = "default_true")]
    pub apple_auto_refresh: bool,
    #[serde(default)]
    pub orm_base_url: Option<String>,
}

fn default_true() -> bool {
    true
}

fn default_overpass_timeout() -> u32 {
    60
}

/// Sane bounds for the Overpass query timeout (seconds). A stored 0 would make
/// every query fail instantly; an absurd value would hang the UI on a bad server.
const OVERPASS_TIMEOUT_MIN: u32 = 5;
const OVERPASS_TIMEOUT_MAX: u32 = 600;

impl Default for Settings {
    fn default() -> Self {
        Self {
            mods_dir_override: None,
            check_for_updates: true,
            map_theme: "system".to_string(),
            overpass_timeout: 60,
            type_speed_overrides: std::collections::HashMap::new(),
            apple_access_key: None,
            apple_map_version: None,
            apple_sat_version: None,
            apple_auto_refresh: true,
            orm_base_url: None,
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
        overpass_timeout: store.get("overpass_timeout")
            .and_then(|v| u32::try_from(v.as_u64()?).ok())
            .unwrap_or(60)
            .clamp(OVERPASS_TIMEOUT_MIN, OVERPASS_TIMEOUT_MAX),
        type_speed_overrides: store.get("type_speed_overrides")
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default(),
        apple_access_key: store.get("apple_access_key")
            .and_then(|v| v.as_str().map(String::from)),
        apple_map_version: store.get("apple_map_version")
            .and_then(|v| v.as_str().map(String::from)),
        apple_sat_version: store.get("apple_sat_version")
            .and_then(|v| v.as_str().map(String::from)),
        apple_auto_refresh: store.get("apple_auto_refresh")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
        orm_base_url: store.get("orm_base_url")
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
    store.set("overpass_timeout", serde_json::json!(settings.overpass_timeout));
    store.set("type_speed_overrides", serde_json::json!(settings.type_speed_overrides));
    store.set("apple_auto_refresh", serde_json::json!(settings.apple_auto_refresh));
    store.set("orm_base_url", serde_json::json!(settings.orm_base_url));
    // While auto-refresh owns the Apple credentials, leave them untouched so a
    // manual save from the settings window (which may hold stale, auto-populated
    // values) can't overwrite the fresher key the refresher just stored.
    if !settings.apple_auto_refresh {
        store.set("apple_access_key", serde_json::json!(settings.apple_access_key));
        store.set("apple_map_version", serde_json::json!(settings.apple_map_version));
        store.set("apple_sat_version", serde_json::json!(settings.apple_sat_version));
    }
    store.save().map_err(|e| e.to_string())?;
    // Re-point the live ORM renderer at the (possibly changed) base URL without a
    // restart; it rebuilds its renderers and clears cached tiles.
    if let Some(handle) = app.try_state::<crate::orm_tiles::OrmHandle>() {
        handle.set_base_url(resolve_orm_base(settings.orm_base_url.as_deref()));
    }
    let _ = app.emit("settings-changed", &settings);
    // Re-evaluate auto-refresh now (e.g. it was just re-enabled) instead of
    // waiting out the loop's current sleep.
    if let Some(refresh) = app.try_state::<crate::apple_token::AppleRefresh>() {
        refresh.wake();
    }
    Ok(())
}

#[tauri::command]
pub async fn pick_folder(app: tauri::AppHandle) -> Option<String> {
    use tauri_plugin_dialog::DialogExt;
    let path = app.dialog().file().blocking_pick_folder()?;
    Some(path.to_string())
}
