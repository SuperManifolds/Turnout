use leptos::{wasm_bindgen, component, view, IntoView, create_signal, create_effect, SignalGet, SignalSet, SignalUpdate, spawn_local, store_value, web_sys, CollectView};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

/// Mirror of the backend `NetworkSettings` — features that automatically contact
/// external servers. Nested under `network` on the wire (not flattened, so
/// `serde_wasm_bindgen` serializes it as a plain object, not an ES `Map`).
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct NetworkSettings {
    #[serde(default = "default_true")]
    pub check_for_updates: bool,
    #[serde(default = "default_true")]
    pub apple_auto_refresh: bool,
    #[serde(default = "default_true")]
    pub crash_reporting: bool,
}

impl Default for NetworkSettings {
    fn default() -> Self {
        Self { check_for_updates: true, apple_auto_refresh: true, crash_reporting: true }
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Settings {
    pub mods_dir_override: Option<String>,
    #[serde(default)]
    pub game_dir_override: Option<String>,
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
    #[serde(default)]
    pub gpu_adapter: Option<String>,
    #[serde(default)]
    pub tutorial_completed: bool,
    #[serde(default)]
    pub network: NetworkSettings,
}

fn default_overpass_timeout() -> u32 { 60 }
fn default_true() -> bool { true }

impl Default for Settings {
    fn default() -> Self {
        Self {
            mods_dir_override: None,
            game_dir_override: None,
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

/// One GPU the app can render on, from `list_gpu_adapters`.
#[derive(Clone, serde::Deserialize)]
pub struct GpuInfo {
    pub name: String,
    pub backend: String,
    pub kind: String,
    #[serde(rename = "isSoftware")]
    pub is_software: bool,
}

const DEFAULT_ORM_BASE: &str = "https://openrailwaymap.app";

/// Settings-window auto-fit geometry: fixed width, content-height padding, and a
/// ceiling so a long panel scrolls rather than growing off-screen.
const SETTINGS_WINDOW_WIDTH: f64 = 480.0;
const SETTINGS_WINDOW_PADDING: f64 = 40.0;
const SETTINGS_WINDOW_MAX_HEIGHT: f64 = 900.0;

const SPEED_TRACK_TYPES: &[(&str, &str, u32)] = &[
    ("rail", "Rail", 160),
    ("rail:yard", "Rail — Yard", 40),
    ("rail:siding", "Rail — Siding", 60),
    ("rail:crossover", "Rail — Crossover", 60),
    ("rail:spur", "Rail — Spur", 60),
    ("rail:main", "Rail — Main", 160),
    ("rail:branch", "Rail — Branch", 160),
    ("rail:industrial", "Rail — Industrial", 60),
    ("light_rail", "Light Rail", 90),
    ("tram", "Tram", 60),
    ("subway", "Subway", 100),
    ("narrow_gauge", "Narrow Gauge", 120),
    ("monorail", "Monorail", 120),
    ("funicular", "Funicular", 80),
    ("preserved", "Preserved", 160),
    ("construction", "Construction", 160),
    ("proposed", "Proposed", 160),
    ("miniature", "Miniature", 160),
    ("disused", "Disused", 160),
    ("abandoned", "Abandoned", 160),
    ("razed", "Razed", 160),
];

const THEME_OPTIONS: &[(&str, &str)] = &[
    ("system", "Follow System"),
    ("light", "Light"),
    ("dark", "Dark"),
];

pub async fn load_settings() -> Settings {
    match crate::tauri::get_settings().await {
        Ok(val) => serde_wasm_bindgen::from_value(val).unwrap_or_default(),
        Err(_) => Settings::default(),
    }
}

async fn save_settings(settings: &Settings) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(settings).map_err(|e| e.to_string())?;
    crate::tauri::set_settings(&args).await
}

/// Persist that the first-launch tutorial has been finished, preserving every
/// other stored setting.
pub async fn mark_tutorial_completed() {
    let mut settings = load_settings().await;
    settings.tutorial_completed = true;
    if let Err(e) = save_settings(&settings).await {
        crate::tauri::report_error("save settings (tutorial completed)", &e);
    }
}

/// Returns true if this webview is the settings window.
pub fn is_settings_window() -> bool {
    let window = web_sys::window().expect("window");
    let Ok(_tauri) = js_sys::Reflect::get(&window, &"__TAURI__".into()) else { return false };
    let Ok(internals) = js_sys::Reflect::get(&window, &"__TAURI_INTERNALS__".into()) else { return false };
    let Ok(metadata) = js_sys::Reflect::get(&internals, &"metadata".into()) else { return false };
    let Ok(label) = js_sys::Reflect::get(&metadata, &"currentWindow".into()) else { return false };
    let Ok(label_obj) = js_sys::Reflect::get(&label, &"label".into()) else { return false };
    label_obj.as_string().as_deref() == Some("settings")
}

fn fit_window_to_content() {
    spawn_local(async {
        // Wait a frame for content to render
        let promise = js_sys::Promise::new(&mut |resolve, _| {
            let _ = web_sys::window().expect("window")
                .request_animation_frame(&resolve);
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;

        let Some(body) = web_sys::window().and_then(|w| w.document()).and_then(|d| d.body()) else { return };
        let height = (f64::from(body.scroll_height()) + SETTINGS_WINDOW_PADDING).min(SETTINGS_WINDOW_MAX_HEIGHT);
        crate::tauri::set_window_logical_size(SETTINGS_WINDOW_WIDTH, height);
    });
}

#[component]
pub fn AppSettings() -> impl IntoView {
    let (mods_dir, set_mods_dir) = create_signal::<Option<String>>(None);
    let (detected_dir, set_detected_dir) = create_signal::<Option<String>>(None);
    let (game_dir, set_game_dir) = create_signal::<Option<String>>(None);
    let (game_detected, set_game_detected) = create_signal::<Option<String>>(None);
    let (check_updates, set_check_updates) = create_signal(true);
    let (theme, set_theme) = create_signal("system".to_string());
    let (timeout, set_timeout) = create_signal(60u32);
    let (speed_overrides, set_speed_overrides) = create_signal(std::collections::HashMap::<String, u32>::new());
    let (apple_key, set_apple_key) = create_signal::<Option<String>>(None);
    let (apple_map_ver, set_apple_map_ver) = create_signal::<Option<String>>(None);
    let (apple_sat_ver, set_apple_sat_ver) = create_signal::<Option<String>>(None);
    let (apple_auto, set_apple_auto) = create_signal(true);
    let (orm_base, set_orm_base) = create_signal::<Option<String>>(None);
    let (gpu_adapter, set_gpu_adapter) = create_signal::<Option<String>>(None);
    let (crash_reports, set_crash_reports) = create_signal(true);
    let (gpus, set_gpus) = create_signal::<Vec<GpuInfo>>(Vec::new());
    let (apple_refresh_status, set_apple_refresh_status) = create_signal(String::new());
    let (status, set_status) = create_signal(String::new());
    let (app_version, set_app_version) = create_signal(String::new());
    let (update_status, set_update_status) = create_signal(String::new());
    let (loaded, set_loaded) = create_signal(false);

    // Load current settings and version
    create_effect(move |_| {
        spawn_local(async move {
            let settings = load_settings().await;
            set_mods_dir.set(settings.mods_dir_override);
            set_game_dir.set(settings.game_dir_override);
            set_check_updates.set(settings.network.check_for_updates);
            set_theme.set(settings.map_theme);
            set_timeout.set(settings.overpass_timeout);
            set_speed_overrides.set(settings.type_speed_overrides);
            set_apple_key.set(settings.apple_access_key);
            set_apple_map_ver.set(settings.apple_map_version);
            set_apple_sat_ver.set(settings.apple_sat_version);
            set_apple_auto.set(settings.network.apple_auto_refresh);
            set_orm_base.set(settings.orm_base_url);
            set_gpu_adapter.set(settings.gpu_adapter);
            set_crash_reports.set(settings.network.crash_reporting);
            if let Ok(js) = crate::tauri::list_gpu_adapters().await
                && let Ok(list) = serde_wasm_bindgen::from_value::<Vec<GpuInfo>>(js)
            {
                set_gpus.set(list);
            }
            set_loaded.set(true);
            fit_window_to_content();

            if let Some(dir) = crate::tauri::get_mods_dir().await {
                set_detected_dir.set(Some(dir));
            }

            if let Some(dir) = crate::tauri::get_game_dir().await {
                set_game_detected.set(Some(dir));
            }

            set_app_version.set(crate::tauri::get_app_version().await);
        });
    });

    // Debounced auto-save whenever any setting changes
    let save_timer = store_value::<Option<i32>>(None);
    create_effect(move |_| {
        let mods = mods_dir.get();
        let game = game_dir.get();
        let updates = check_updates.get();
        let t = theme.get();
        let tout = timeout.get();
        let speeds = speed_overrides.get();
        let ak = apple_key.get();
        let amv = apple_map_ver.get();
        let asv = apple_sat_ver.get();
        let auto = apple_auto.get();
        let orm = orm_base.get();
        let gpu = gpu_adapter.get();
        let crash = crash_reports.get();
        if !loaded.get() { return; }

        if let Some(handle) = save_timer.get_value() {
            web_sys::window().expect("window").clear_timeout_with_handle(handle);
        }
        let cb = Closure::once(move || {
            let settings = Settings {
                mods_dir_override: mods,
                game_dir_override: game,
                map_theme: t,
                overpass_timeout: tout,
                type_speed_overrides: speeds,
                apple_access_key: ak,
                apple_map_version: amv,
                apple_sat_version: asv,
                orm_base_url: orm,
                gpu_adapter: gpu,
                // The settings window never completes the tutorial; the backend
                // preserves a stored `true`, so this can't un-complete it.
                tutorial_completed: false,
                network: NetworkSettings {
                    check_for_updates: updates,
                    apple_auto_refresh: auto,
                    crash_reporting: crash,
                },
            };
            spawn_local(async move {
                if let Err(e) = save_settings(&settings).await {
                    set_status.set(format!("Failed to save: {e}"));
                }
            });
        });
        let handle = web_sys::window().expect("window")
            .set_timeout_with_callback_and_timeout_and_arguments_0(
                cb.as_ref().unchecked_ref(), 200,
            ).unwrap_or(0);
        cb.forget();
        save_timer.set_value(Some(handle));
    });

    // Pull the current (auto-managed) Apple credentials from the store into the
    // displayed signals. Called after a manual refresh and on the background
    // refresher's event so the settings window always shows live values.
    let sync_apple_from_store = move || {
        spawn_local(async move {
            let settings = load_settings().await;
            set_apple_key.set(settings.apple_access_key);
            set_apple_map_ver.set(settings.apple_map_version);
            set_apple_sat_ver.set(settings.apple_sat_version);
        });
    };

    let on_refresh_apple = move |_| {
        set_apple_refresh_status.set("Refreshing…".to_string());
        spawn_local(async move {
            match crate::tauri::refresh_apple_token().await {
                Ok(()) => {
                    sync_apple_from_store();
                    set_apple_refresh_status.set("Token refreshed".to_string());
                }
                Err(e) => set_apple_refresh_status.set(format!("Refresh failed: {e}")),
            }
        });
    };

    // React to the background refresher: pull fresh values on success, surface the
    // error on repeated failure.
    crate::tauri::listen_to_events(&["apple-token-refreshed"], move |_| {
        sync_apple_from_store();
        set_apple_refresh_status.set("Token refreshed".to_string());
    });
    crate::tauri::listen_to_events(&["apple-token-refresh-failed"], move |payload| {
        let msg = payload.as_string().unwrap_or_else(|| "unknown error".to_string());
        set_apple_refresh_status.set(format!("Auto-refresh failing: {msg}"));
    });

    let on_browse = move |_| {
        spawn_local(async move {
            if let Some(path) = crate::tauri::pick_folder().await {
                set_mods_dir.set(Some(path));
            }
        });
    };

    let on_auto_detect = move |_| {
        set_mods_dir.set(None);
    };

    let display_path = move || {
        if let Some(path) = mods_dir.get() {
            path
        } else if let Some(detected) = detected_dir.get() {
            format!("{detected} (auto-detected)")
        } else {
            "Not found — click Browse to set manually".to_string()
        }
    };

    let on_browse_game = move |_| {
        spawn_local(async move {
            if let Some(path) = crate::tauri::pick_folder().await {
                set_game_dir.set(Some(path));
            }
        });
    };

    let on_auto_detect_game = move |_| {
        set_game_dir.set(None);
    };

    let display_game_path = move || {
        if let Some(path) = game_dir.get() {
            path
        } else if let Some(detected) = game_detected.get() {
            format!("{detected} (auto-detected)")
        } else {
            "Not found — click Browse to set the game install folder".to_string()
        }
    };

    view! {
        <section id="app-settings">
            <h2>"Settings"</h2>

            <fieldset>
                <legend>"Nimby Rails Folder"</legend>
                <p class="path-display">{display_path}</p>
                <nav>
                    <button type="button" on:click=on_browse>
                        <i class="fa-solid fa-folder-open"></i>
                        " Browse"
                    </button>
                    <button type="button" on:click=on_auto_detect>
                        <i class="fa-solid fa-rotate"></i>
                        " Auto-detect"
                    </button>
                </nav>
            </fieldset>

            <fieldset>
                <legend>"Game Install Folder"</legend>
                <p class="setting-hint">"The Steam install (containing resources/), used for the population overlay."</p>
                <p class="path-display">{display_game_path}</p>
                <nav>
                    <button type="button" on:click=on_browse_game>
                        <i class="fa-solid fa-folder-open"></i>
                        " Browse"
                    </button>
                    <button type="button" on:click=on_auto_detect_game>
                        <i class="fa-solid fa-rotate"></i>
                        " Auto-detect"
                    </button>
                </nav>
            </fieldset>

            <fieldset>
                <legend>"Map Theme"</legend>
                <nav class="theme-options">
                    {THEME_OPTIONS.iter().map(|&(id, label)| {
                        let id_owned = id.to_string();
                        let id_check = id.to_string();
                        view! {
                            <button
                                type="button"
                                class:active=move || theme.get() == id_check
                                on:click=move |_| set_theme.set(id_owned.clone())
                            >
                                {label}
                            </button>
                        }
                    }).collect_view()}
                </nav>
            </fieldset>

            <fieldset>
                <legend>"Overpass API"</legend>
                <label>
                    "Query timeout: "
                    <input
                        type="number"
                        min="10"
                        max="300"
                        prop:value=move || timeout.get().to_string()
                        on:change=move |ev| {
                            if let Ok(v) = leptos::event_target_value(&ev).parse::<u32>() {
                                set_timeout.set(v.clamp(10, 300));
                            }
                        }
                    />
                    " seconds"
                </label>
            </fieldset>

            <fieldset>
                <legend>"OpenRailwayMap Server"</legend>
                <label>
                    "Base URL"
                    <input
                        type="text"
                        placeholder=DEFAULT_ORM_BASE
                        prop:value=move || orm_base.get().unwrap_or_default()
                        on:input=move |ev| {
                            let v = leptos::event_target_value(&ev);
                            let v = v.trim();
                            set_orm_base.set(if v.is_empty() { None } else { Some(v.to_string()) });
                        }
                    />
                </label>
                <p class="hint">
                    "Where OpenRailwayMap tiles, glyphs and sprites are fetched from. Leave blank to use "
                    {DEFAULT_ORM_BASE}
                    ". Set this to your own server if you self-host the OpenRailwayMap stack."
                </p>
            </fieldset>

            <fieldset>
                <legend>"Rendering GPU"</legend>
                <label>
                    "GPU"
                    <select
                        prop:value=move || gpu_adapter.get().unwrap_or_default()
                        on:change=move |ev| {
                            let v = leptos::event_target_value(&ev);
                            set_gpu_adapter.set(if v.is_empty() { None } else { Some(v) });
                        }
                    >
                        <option value="">
                            {move || match gpus.get().iter().find(|g| !g.is_software) {
                                Some(g) => format!("Auto — {}", g.name),
                                None => "Auto (best available GPU)".to_string(),
                            }}
                        </option>
                        // A saved GPU that is no longer present (driver change, eGPU
                        // unplugged) gets its own option so the select shows it —
                        // labelled "not found" — instead of a blank selection, while
                        // keeping the preference. The backend falls back to Auto.
                        {move || {
                            let list = gpus.get();
                            gpu_adapter.get()
                                // Skip empty (its value="" would collide with the Auto option).
                                .filter(|name| !name.is_empty() && !list.is_empty() && !list.iter().any(|g| &g.name == name))
                                .map(|name| view! {
                                    <option value={name.clone()}>{format!("{name} (not found)")}</option>
                                })
                        }}
                        {move || gpus.get().into_iter().map(|g| {
                            // Show the render backend (Vulkan/Metal) so each row is
                            // clearly a distinct device+API, not a bare duplicate.
                            let label = if g.is_software {
                                format!("{} — {} ({} · software)", g.name, g.backend, g.kind)
                            } else {
                                format!("{} — {} ({})", g.name, g.backend, g.kind)
                            };
                            view! { <option value={g.name.clone()}>{label}</option> }
                        }).collect_view()}
                    </select>
                </label>
                <p class="hint">
                    "Which GPU renders the OpenRailwayMap layers. \"Auto\" avoids software renderers (which peg the CPU and leave the GPU idle). Takes effect after restarting the app."
                </p>
            </fieldset>

            <fieldset>
                <legend>"Speed Overrides"</legend>
                <p class="path-display">"Set a fixed speed limit per track type. Overrides OSM data."</p>
                {SPEED_TRACK_TYPES.iter().map(|&(id, label, default_speed)| {
                    let id_check = id.to_string();
                    let id_toggle = id.to_string();
                    let id_value = id.to_string();
                    let id_disabled = id.to_string();
                    let id_input = id.to_string();
                    view! {
                        <div class="speed-override-row">
                            <label on:click={
                                let id = id_toggle;
                                move |_| {
                                    let id = id.clone();
                                    set_speed_overrides.update(move |m| {
                                        use std::collections::hash_map::Entry;
                                        match m.entry(id) {
                                            Entry::Occupied(e) => { e.remove(); }
                                            Entry::Vacant(e) => { e.insert(default_speed); }
                                        }
                                    });
                                }
                            }>
                                <i class=move || {
                                    if speed_overrides.get().contains_key(&id_check) {
                                        "fa-solid fa-square-check"
                                    } else {
                                        "fa-regular fa-square"
                                    }
                                }></i>
                                " "{label}
                            </label>
                            <input
                                type="number"
                                min="1"
                                max="500"
                                prop:value=move || speed_overrides.get().get(&id_value).copied().unwrap_or(default_speed).to_string()
                                disabled=move || !speed_overrides.get().contains_key(&id_disabled)
                                on:change={
                                    let id = id_input;
                                    move |ev| {
                                        if let Ok(v) = leptos::event_target_value(&ev).parse::<u32>() {
                                            let id = id.clone();
                                            set_speed_overrides.update(move |m| {
                                                m.insert(id, v.clamp(1, 500));
                                            });
                                        }
                                    }
                                }
                            />
                            <span class="speed-unit">"km/h"</span>
                        </div>
                    }
                }).collect_view()}
            </fieldset>

            <fieldset>
                <legend>"Apple Maps"</legend>
                <label on:click=move |_| set_apple_auto.update(|v| *v = !*v)>
                    <i class=move || if apple_auto.get() { "fa-solid fa-square-check" } else { "fa-regular fa-square" }></i>
                    " Refresh access token automatically"
                </label>
                <p class="hint">"Fetches a fresh Apple access key and tile versions before each one expires. Turn off to enter them manually."</p>
                <div class="settings-row">
                    <button type="button" on:click=on_refresh_apple>
                        "Refresh now"
                    </button>
                    <span class="hint">{move || apple_refresh_status.get()}</span>
                </div>
                {move || (!apple_auto.get()).then(|| view! {
                    <label>
                        "Access Key"
                        <input
                            type="text"
                            placeholder="Access key"
                            prop:value=move || apple_key.get().unwrap_or_default()
                            on:input=move |ev| {
                                let v = leptos::event_target_value(&ev);
                                set_apple_key.set(if v.is_empty() { None } else { Some(v) });
                            }
                        />
                    </label>
                    <label>
                        "Map version"
                        <input
                            type="text"
                            placeholder="v= from map tiles"
                            prop:value=move || apple_map_ver.get().unwrap_or_default()
                            on:input=move |ev| {
                                let v = leptos::event_target_value(&ev);
                                set_apple_map_ver.set(if v.is_empty() { None } else { Some(v) });
                            }
                        />
                    </label>
                    <label>
                        "Satellite version"
                        <input
                            type="text"
                            placeholder="v= from satellite tiles"
                            prop:value=move || apple_sat_ver.get().unwrap_or_default()
                            on:input=move |ev| {
                                let v = leptos::event_target_value(&ev);
                                set_apple_sat_ver.set(if v.is_empty() { None } else { Some(v) });
                            }
                        />
                    </label>
                })}
            </fieldset>

            <fieldset>
                <legend>"Help"</legend>
                <nav>
                    <button type="button" on:click=move |_| {
                        spawn_local(async move {
                            let _ = crate::tauri::replay_tutorial().await;
                        });
                    }>
                        <i class="fa-solid fa-graduation-cap"></i>
                        " Replay tutorial"
                    </button>
                </nav>
                <p class="path-display">"Reopens the guided tour in the main window."</p>
            </fieldset>

            <fieldset>
                <legend>"Privacy"</legend>
                <label on:click=move |_| set_crash_reports.update(|v| *v = !*v)>
                    <i class=move || if crash_reports.get() { "fa-solid fa-square-check" } else { "fa-regular fa-square" }></i>
                    " Send automatic crash reports"
                </label>
                <p class="hint">"Sends anonymous crash and error diagnostics (no personal data) so startup crashes and bugs can be found and fixed. Nothing else is collected."</p>
            </fieldset>

            <fieldset>
                <legend>"Updates"</legend>
                <label on:click=move |_| set_check_updates.update(|v| *v = !*v)>
                    <i class=move || if check_updates.get() { "fa-solid fa-square-check" } else { "fa-regular fa-square" }></i>
                    " Check for updates on launch"
                </label>
                <nav>
                    <button type="button" on:click=move |_| {
                        set_update_status.set("Checking...".into());
                        spawn_local(async move {
                            match crate::tauri::check_for_update().await {
                                Ok(Some(version)) => set_update_status.set(format!("update:v{version}")),
                                Ok(None) => set_update_status.set("You're up to date".into()),
                                Err(e) => set_update_status.set(format!("Check failed: {e}")),
                            }
                        });
                    }>
                        <i class="fa-solid fa-arrows-rotate"></i>
                        " Check Now"
                    </button>
                </nav>
                {move || {
                    let s = update_status.get();
                    if s.is_empty() {
                        None
                    } else if let Some(version) = s.strip_prefix("update:") {
                        let version = version.to_string();
                        Some(view! {
                            <p class="path-display">
                                {format!("Update available: {version}")}
                                " "
                                <button type="button" on:click=move |_| {
                                    set_update_status.set("Downloading...".into());
                                    spawn_local(async move {
                                        match crate::tauri::download_and_install_update().await {
                                            Ok(()) => set_update_status.set("Restarting...".into()),
                                            Err(e) => set_update_status.set(format!("Update failed: {e}")),
                                        }
                                    });
                                }>
                                    <i class="fa-solid fa-download"></i>
                                    " Install & Restart"
                                </button>
                            </p>
                        }.into_any())
                    } else {
                        Some(view! { <p class="path-display">{s}</p> }.into_any())
                    }
                }}
                <p class="path-display">{move || {
                    let v = app_version.get();
                    if v.is_empty() { String::new() } else { format!("Turnout v{v}") }
                }}</p>
            </fieldset>

            {move || {
                let s = status.get();
                if s.is_empty() { None } else { Some(view! { <p class="error">{s}</p> }) }
            }}
        </section>
    }
}
