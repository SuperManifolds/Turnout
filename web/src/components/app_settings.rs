use leptos::{wasm_bindgen, component, view, IntoView, create_signal, create_effect, SignalGet, SignalSet, SignalUpdate, spawn_local, store_value, web_sys, CollectView};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Settings {
    pub mods_dir_override: Option<String>,
    pub check_for_updates: bool,
    pub map_theme: String,
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

#[component]
pub fn AppSettings() -> impl IntoView {
    let (mods_dir, set_mods_dir) = create_signal::<Option<String>>(None);
    let (detected_dir, set_detected_dir) = create_signal::<Option<String>>(None);
    let (check_updates, set_check_updates) = create_signal(true);
    let (theme, set_theme) = create_signal("system".to_string());
    let (status, set_status) = create_signal(String::new());
    let (app_version, set_app_version) = create_signal(String::new());
    let (update_status, set_update_status) = create_signal(String::new());
    let (loaded, set_loaded) = create_signal(false);

    // Load current settings and version
    create_effect(move |_| {
        spawn_local(async move {
            let settings = load_settings().await;
            set_mods_dir.set(settings.mods_dir_override);
            set_check_updates.set(settings.check_for_updates);
            set_theme.set(settings.map_theme);
            set_loaded.set(true);

            if let Some(dir) = crate::tauri::get_mods_dir().await {
                set_detected_dir.set(Some(dir));
            }

            set_app_version.set(crate::tauri::get_app_version().await);
        });
    });

    // Debounced auto-save whenever any setting changes
    let save_timer = store_value::<Option<i32>>(None);
    create_effect(move |_| {
        let mods = mods_dir.get();
        let updates = check_updates.get();
        let t = theme.get();
        if !loaded.get() { return; }

        if let Some(handle) = save_timer.get_value() {
            web_sys::window().expect("window").clear_timeout_with_handle(handle);
        }
        let cb = Closure::once(move || {
            let settings = Settings {
                mods_dir_override: mods,
                check_for_updates: updates,
                map_theme: t,
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
                                Ok(Some(version)) => set_update_status.set(format!("Update available: v{version}")),
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
                    if s.is_empty() { None } else { Some(view! { <p class="path-display">{s}</p> }) }
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
