use leptos::{wasm_bindgen, component, view, mount_to_body, IntoView, create_signal, Callback, SignalSet, Show, SignalGet, spawn_local};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

mod components;
mod utils;

#[wasm_bindgen]
extern "C" {
    fn map_set_theme(theme: &str);
    fn map_on_load(callback: &js_sys::Function);
}

fn setup_theme_handling() {
    // Apply saved theme once the map is loaded
    let on_load = Closure::once(move || {
        spawn_local(async {
            let settings = components::app_settings::load_settings().await;
            map_set_theme(&settings.map_theme);
        });
    });
    map_on_load(on_load.as_ref().unchecked_ref());
    on_load.forget();

    // Listen for settings changes from the settings window
    spawn_local(async {
        let window = web_sys::window().expect("window");
        let Ok(tauri) = js_sys::Reflect::get(&window, &"__TAURI__".into()) else { return };
        let Ok(event_mod) = js_sys::Reflect::get(&tauri, &"event".into()) else { return };
        let Ok(listen_fn) = js_sys::Reflect::get(&event_mod, &"listen".into()) else { return };
        let Ok(listen_fn) = listen_fn.dyn_into::<js_sys::Function>() else { return };

        let callback = Closure::wrap(Box::new(move |event: JsValue| {
            let Ok(payload) = js_sys::Reflect::get(&event, &"payload".into()) else { return };
            let Ok(theme) = js_sys::Reflect::get(&payload, &"map_theme".into()) else { return };
            if let Some(theme_str) = theme.as_string() {
                map_set_theme(&theme_str);
            }
        }) as Box<dyn Fn(JsValue)>);

        let _ = listen_fn.call2(&event_mod, &"settings-changed".into(), callback.as_ref().unchecked_ref());
        callback.forget();
    });
}

fn main() {
    console_error_panic_hook::set_once();
    mount_to_body(App);
}

#[component]
fn App() -> impl IntoView {
    let is_settings = components::app_settings::is_settings_window();

    if !is_settings {
        setup_theme_handling();
    }

    let (available_types, set_available_types) = create_signal::<Vec<String>>(vec![]);
    let (enabled_types, set_enabled_types) = create_signal(
        components::track_filter::default_enabled_types()
    );
    let (has_selection, set_has_selection) = create_signal(false);
    let (apply_speed_limits, set_apply_speed_limits) = create_signal(true);
    let (clip_to_selection, set_clip_to_selection) = create_signal(false);

    let on_filter_change = Callback::new(move |types: Vec<String>| {
        set_enabled_types.set(types);
    });

    view! {
        <Show when=move || is_settings>
            <components::AppSettings />
        </Show>
        <Show when=move || !is_settings>
            <main>
                <section id="map-container">
                    <components::Map
                        set_available_types=set_available_types
                        enabled_types=enabled_types
                        set_has_selection=set_has_selection
                        apply_speed_limits=apply_speed_limits
                        clip_to_selection=clip_to_selection
                    />
                    <components::Search />
                    <components::LayerSwitcher />
                    <Show when=move || has_selection.get()>
                        <div id="sidebar">
                            <components::TrackFilter
                                available=available_types
                                on_change=on_filter_change
                            />
                            <components::Settings
                                apply_speed_limits=apply_speed_limits
                                set_apply_speed_limits=set_apply_speed_limits
                                clip_to_selection=clip_to_selection
                                set_clip_to_selection=set_clip_to_selection
                            />
                        </div>
                    </Show>
                </section>
            </main>
        </Show>
    }
}
