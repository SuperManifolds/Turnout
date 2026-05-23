use leptos::{wasm_bindgen, component, view, IntoView, create_signal, create_effect, SignalGet, SignalGetUntracked, SignalSet, SignalUpdate, spawn_local, web_sys, CollectView};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

#[wasm_bindgen]
extern "C" {
    fn map_set_theme(theme: &str);
}

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
    match tauri_invoke("get_settings", JsValue::NULL).await {
        Ok(val) => serde_wasm_bindgen::from_value(val).unwrap_or_default(),
        Err(_) => Settings::default(),
    }
}

async fn save_settings(settings: &Settings) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(settings).map_err(|e| e.to_string())?;
    let wrapper = js_sys::Object::new();
    js_sys::Reflect::set(&wrapper, &"settings".into(), &args).map_err(|e| format!("{e:?}"))?;
    tauri_invoke("set_settings", &wrapper).await.map(|_| ()).map_err(|e| format!("{e:?}"))
}

async fn pick_folder() -> Option<String> {
    let result = tauri_invoke("pick_folder", JsValue::NULL).await.ok()?;
    result.as_string()
}

async fn detect_mods_dir() -> Option<String> {
    let result = tauri_invoke("get_mods_dir", JsValue::NULL).await.ok()?;
    result.as_string()
}

async fn tauri_invoke(cmd: &str, args: impl Into<JsValue>) -> Result<JsValue, JsValue> {
    let window = web_sys::window().expect("window");
    let tauri = js_sys::Reflect::get(&window, &"__TAURI__".into())?;
    let core = js_sys::Reflect::get(&tauri, &"core".into())?;
    let invoke = js_sys::Reflect::get(&core, &"invoke".into())?
        .dyn_into::<js_sys::Function>()?;
    let promise = invoke.call2(&core, &cmd.into(), &args.into())?;
    JsFuture::from(js_sys::Promise::from(promise)).await
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

/// Apply the saved map theme on startup (called from main window).
pub fn apply_saved_theme() {
    spawn_local(async {
        let settings = load_settings().await;
        map_set_theme(&settings.map_theme);
    });
}

#[component]
pub fn AppSettings() -> impl IntoView {
    let (mods_dir, set_mods_dir) = create_signal::<Option<String>>(None);
    let (detected_dir, set_detected_dir) = create_signal::<Option<String>>(None);
    let (check_updates, set_check_updates) = create_signal(true);
    let (theme, set_theme) = create_signal("system".to_string());
    let (status, set_status) = create_signal(String::new());

    create_effect(move |_| {
        spawn_local(async move {
            let settings = load_settings().await;
            set_mods_dir.set(settings.mods_dir_override);
            set_check_updates.set(settings.check_for_updates);
            set_theme.set(settings.map_theme);

            if let Some(dir) = detect_mods_dir().await {
                set_detected_dir.set(Some(dir));
            }
        });
    });

    let on_browse = move |_| {
        spawn_local(async move {
            if let Some(path) = pick_folder().await {
                set_mods_dir.set(Some(path));
            }
        });
    };

    let on_auto_detect = move |_| {
        set_mods_dir.set(None);
    };

    let on_save = move |_| {
        let settings = Settings {
            mods_dir_override: mods_dir.get_untracked(),
            check_for_updates: check_updates.get_untracked(),
            map_theme: theme.get_untracked(),
        };
        spawn_local(async move {
            match save_settings(&settings).await {
                Ok(()) => {
                    let window = web_sys::window().expect("window");
                    let _ = js_sys::Reflect::get(&window, &"close".into())
                        .and_then(JsCast::dyn_into::<js_sys::Function>)
                        .map(|f| f.call0(&window));
                }
                Err(e) => set_status.set(format!("Failed to save: {e}")),
            }
        });
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
            </fieldset>

            {move || {
                let s = status.get();
                if s.is_empty() { None } else { Some(view! { <p class="error">{s}</p> }) }
            }}

            <nav class="actions">
                <button type="button" class="primary" on:click=on_save>"Save"</button>
            </nav>
        </section>
    }
}
