use tauri::{Emitter, Manager};
use tauri_plugin_store::StoreExt;

use crate::error::{CommandError, CommandResult};

pub(crate) const SETTINGS_STORE: &str = "settings.json";

/// Store key marking the first-launch tour as finished. Version-suffixed so
/// adding steps to the tour re-runs it for everyone: bump the suffix and every
/// existing user's stale key is ignored, re-showing the tour once. `_v2` added
/// the "auto-launch with NIMBY Rails" step.
const TUTORIAL_COMPLETED_KEY: &str = "tutorial_completed_v2";

/// Bundle identifier (from `tauri.conf.json`). Locates the on-disk settings store
/// before the Tauri store plugin is initialised.
const APP_IDENTIFIER: &str = "io.sorlie.turnout";

/// Read the raw settings store JSON straight off disk, for startup code that runs
/// before the Tauri store plugin exists (crash reporting, GPU selection). `None`
/// when the store is absent (fresh install); logs a warning and returns `None`
/// when it is present but unreadable/corrupt, so callers fall back to defaults.
pub(crate) fn read_store_from_disk() -> Option<serde_json::Value> {
    let path = dirs_next::config_dir()?.join(APP_IDENTIFIER).join(SETTINGS_STORE);
    if !path.exists() {
        return None;
    }
    let parsed = std::fs::read(&path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok());
    if parsed.is_none() {
        tracing::warn!("settings store at {} is unreadable; using defaults", path.display());
    }
    parsed
}

/// The persisted GPU-adapter preference, read directly off disk. Applied to
/// `MLN_VULKAN_DEVICE_NAME` at the very start of `main`, before any threads spawn.
pub(crate) fn stored_gpu_adapter() -> Option<String> {
    read_store_from_disk()?
        .get("gpu_adapter")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// Host serving `OpenRailwayMap` vector tiles, glyphs and sprites. Overridable via
/// [`Settings::orm_base_url`] for users running a self-hosted `OpenRailwayMap` stack.
pub const DEFAULT_ORM_BASE: &str = "https://openrailwaymap.app";

/// Resolves the configured ORM base URL, falling back to [`DEFAULT_ORM_BASE`] and
/// trimming a trailing slash so callers can append `/path` without doubling it.
pub fn resolve_orm_base(base: Option<&str>) -> String {
    let raw = base.map(str::trim).filter(|s| !s.is_empty()).unwrap_or(DEFAULT_ORM_BASE);
    raw.trim_end_matches('/').to_string()
}

/// Features that automatically contact external servers, each an independent
/// opt-in/out flag. Grouped into their own struct to keep [`Settings`] legible.
/// Persisted as flat top-level keys in the store (see [`load`]/[`set_settings`]);
/// crosses the IPC boundary nested under `network`, mirrored on the frontend.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct NetworkSettings {
    /// Check for app updates on launch.
    #[serde(default = "default_true")]
    pub check_for_updates: bool,
    /// Fetch a fresh Apple Maps access key and tile versions before they expire.
    #[serde(default = "default_true")]
    pub apple_auto_refresh: bool,
    /// Send automatic crash reports (native crashes, panics, frontend errors) to
    /// Sentry. Opt-out; read from disk at startup by [`crate::crash_reporting`].
    #[serde(default = "default_true")]
    pub crash_reporting: bool,
}

impl Default for NetworkSettings {
    fn default() -> Self {
        Self { check_for_updates: true, apple_auto_refresh: true, crash_reporting: true }
    }
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct Settings {
    pub mods_dir_override: Option<String>,
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
    #[serde(default)]
    pub orm_base_url: Option<String>,
    /// Preferred GPU for ORM rendering, by adapter name (from the settings
    /// picker). `None` = auto-select the best hardware device. Applied via the
    /// `MLN_VULKAN_DEVICE_NAME` env var at startup, so a change needs a restart.
    #[serde(default)]
    pub gpu_adapter: Option<String>,
    /// Whether the user has finished the first-launch tutorial. Gates whether the
    /// guided tour auto-opens on startup; the tour can still be replayed manually.
    #[serde(default)]
    pub tutorial_completed: bool,
    #[serde(default)]
    pub network: NetworkSettings,
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
            map_theme: "system".to_string(),
            overpass_timeout: 60,
            type_speed_overrides: std::collections::HashMap::new(),
            apple_access_key: None,
            apple_map_version: None,
            apple_sat_version: None,
            orm_base_url: None,
            gpu_adapter: None,
            tutorial_completed: false,
            network: NetworkSettings::default(),
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
        orm_base_url: store.get("orm_base_url")
            .and_then(|v| v.as_str().map(String::from)),
        gpu_adapter: store.get("gpu_adapter")
            .and_then(|v| v.as_str().map(String::from)),
        tutorial_completed: store.get(TUTORIAL_COMPLETED_KEY)
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        network: NetworkSettings {
            check_for_updates: store.get("check_for_updates")
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
            apple_auto_refresh: store.get("apple_auto_refresh")
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
            crash_reporting: store.get("crash_reporting")
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
        },
    }
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn get_settings(app: tauri::AppHandle) -> Settings {
    load(&app)
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn set_settings(app: tauri::AppHandle, settings: Settings) -> CommandResult<()> {
    let store = app.store(SETTINGS_STORE).map_err(|e| CommandError::Io(e.to_string()))?;
    store.set("mods_dir_override", serde_json::json!(settings.mods_dir_override));
    store.set("check_for_updates", serde_json::json!(settings.network.check_for_updates));
    store.set("map_theme", serde_json::json!(settings.map_theme));
    store.set("overpass_timeout", serde_json::json!(settings.overpass_timeout));
    store.set("type_speed_overrides", serde_json::json!(settings.type_speed_overrides));
    store.set("apple_auto_refresh", serde_json::json!(settings.network.apple_auto_refresh));
    store.set("orm_base_url", serde_json::json!(settings.orm_base_url));
    store.set("gpu_adapter", serde_json::json!(settings.gpu_adapter));
    store.set("crash_reporting", serde_json::json!(settings.network.crash_reporting));
    // Completion only ever goes false→true (the tour finishing). OR against the
    // stored value so a save from a settings window that loaded a stale `false`
    // can't un-complete the tutorial.
    let tutorial_done = settings.tutorial_completed
        || store.get(TUTORIAL_COMPLETED_KEY).and_then(|v| v.as_bool()).unwrap_or(false);
    store.set(TUTORIAL_COMPLETED_KEY, serde_json::json!(tutorial_done));
    // While auto-refresh owns the Apple credentials, leave them untouched so a
    // manual save from the settings window (which may hold stale, auto-populated
    // values) can't overwrite the fresher key the refresher just stored.
    if !settings.network.apple_auto_refresh {
        store.set("apple_access_key", serde_json::json!(settings.apple_access_key));
        store.set("apple_map_version", serde_json::json!(settings.apple_map_version));
        store.set("apple_sat_version", serde_json::json!(settings.apple_sat_version));
    }
    store.save().map_err(|e| CommandError::Io(e.to_string()))?;
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

/// Open an external URL (issue tracker, mailto, etc.) in the OS default handler.
#[tauri::command]
pub fn open_external_url(url: String) -> CommandResult<()> {
    tauri_plugin_opener::open_url(url, None::<&str>)
        .map_err(|e| CommandError::Other(format!("Failed to open URL: {e}")))
}

/// Ask the main window to (re)start the first-launch tutorial. Emitted from the
/// separate settings window's "Replay tutorial" button.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn replay_tutorial(app: tauri::AppHandle) -> CommandResult<()> {
    app.emit("start-tutorial", ()).map_err(|e| CommandError::Other(e.to_string()))?;
    Ok(())
}

#[tauri::command]
pub async fn pick_folder(app: tauri::AppHandle) -> Option<String> {
    use tauri_plugin_dialog::DialogExt;
    let path = app.dialog().file().blocking_pick_folder()?;
    Some(path.to_string())
}
