use leptos::{wasm_bindgen, component, view, mount_to_body, IntoView, create_signal, create_effect, Callback, SignalSet, SignalGet, Show, spawn_local};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

mod components;
mod geo;
pub mod tauri;
mod tiles;
mod utils;

#[wasm_bindgen]
extern "C" {
    fn map_set_theme(theme: &str);
    fn map_on_load(callback: &js_sys::Function);
    #[wasm_bindgen(js_name = __turnout_report_panic)]
    fn report_panic(msg: &str);
}

/// Installs a panic hook that keeps the standard console output and additionally
/// forwards the panic (message plus `file:line` location) to Sentry. The Sentry
/// forwarding is a no-op when the plugin global is absent.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        console_error_panic_hook::hook(info);
        report_panic(&info.to_string());
    }));
}

fn setup_theme_handling() {
    let on_load = Closure::once(move || {
        spawn_local(async {
            let settings = components::app_settings::load_settings().await;
            map_set_theme(&settings.map_theme);
        });
    });
    map_on_load(on_load.as_ref().unchecked_ref());
    on_load.forget();

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
    install_panic_hook();
    mount_to_body(App);
}

#[component]
fn App() -> impl IntoView {
    let is_settings = components::app_settings::is_settings_window();

    if !is_settings {
        setup_theme_handling();
    }

    // Restore persisted session state
    let (available_types, set_available_types) = create_signal::<Vec<String>>(vec![]);
    let (enabled_types, set_enabled_types) = create_signal(
        utils::load_string_vec("enabled_types")
            .unwrap_or_else(components::track_filter::default_enabled_types)
    );
    let (has_selection, set_has_selection) = create_signal(false);
    let (apply_speed_limits, set_apply_speed_limits) = create_signal(
        utils::load_bool("apply_speed_limits", true)
    );
    let (clip_to_selection, set_clip_to_selection) = create_signal(
        utils::load_bool("clip_to_selection", false)
    );
    let (tangent_mode, set_tangent_mode) = create_signal(
        utils::load_bool("tangent_mode", false)
    );
    // Persist on change
    create_effect(move |_| {
        utils::save_bool("apply_speed_limits", apply_speed_limits.get());
    });
    create_effect(move |_| {
        utils::save_bool("clip_to_selection", clip_to_selection.get());
    });
    create_effect(move |_| {
        utils::save_bool("tangent_mode", tangent_mode.get());
    });
    create_effect(move |_| {
        let types = enabled_types.get();
        utils::save_string_vec("enabled_types", &types);
    });

    let on_filter_change = Callback::new(move |types: Vec<String>| {
        set_enabled_types.set(types);
    });

    let (drawer_open, set_drawer_open) = create_signal(false);
    let (overlay_open, set_overlay_open) = create_signal(false);
    // Lifted out of Map so the guided tour can open the Rail Layers panel.
    let (show_vector_layers, set_show_vector_layers) = create_signal(false);
    // Shared ORM overlay state: the switcher owns the active mode, the overlay
    // drawer owns visibility, and each reads the other's signal.
    let (orm_style, set_orm_style) = create_signal("standard".to_string());
    let (orm_visible, set_orm_visible) = create_signal(true);
    // Population brush: the overlay drawer owns the tools, the map paints with them.
    let (pop_editor_open, set_pop_editor_open) = create_signal(false);
    let (pop_tool, set_pop_tool) = create_signal(components::PopTool::None);
    let (pop_brush_radius, set_pop_brush_radius) = create_signal(30.0_f64);
    let (pop_brush_strength, set_pop_brush_strength) = create_signal(60_u32);
    // The active selection bbox (south, west, north, east), mirrored out of Map so
    // the population region tool can read it.
    let (selection_bbox, set_selection_bbox) = create_signal::<Option<(f64, f64, f64, f64)>>(None);

    // First-launch guided tour: auto-opens until completed, and re-openable from
    // the settings window via the `start-tutorial` event.
    let (tour_active, set_tour_active) = create_signal(false);
    let (tour_replay, set_tour_replay) = create_signal(false);
    if !is_settings {
        spawn_local(async move {
            if !components::app_settings::load_settings().await.tutorial_completed {
                set_tour_active.set(true);
            }
        });
        tauri::listen_to_events(&["start-tutorial"], move |_| {
            set_tour_replay.set(true);
            set_tour_active.set(true);
        });
    }

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
                        tangent_mode=tangent_mode
                        set_drawer_open=set_drawer_open
                        show_vector_layers=show_vector_layers
                        set_show_vector_layers=set_show_vector_layers
                        pop_tool=pop_tool
                        pop_brush_radius=pop_brush_radius
                        pop_brush_strength=pop_brush_strength
                        pop_editor_open=pop_editor_open
                        set_pop_editor_open=set_pop_editor_open
                        set_selection_bbox=set_selection_bbox
                    />
                    <components::Search />
                    <components::LayerSwitcher
                        orm_style=orm_style
                        set_orm_style=set_orm_style
                        orm_visible=orm_visible
                    />
                    <button id="overlay-toggle" on:click=move |_| set_overlay_open.set(!overlay_open.get()) title="Overlays">
                        <i class="fa-solid fa-layer-group"></i>
                    </button>
                    <Show when=move || has_selection.get()>
                        <aside id="sidebar">
                            <components::TrackFilter
                                available=available_types
                                on_change=on_filter_change
                            />
                            <components::Settings
                                apply_speed_limits=apply_speed_limits
                                set_apply_speed_limits=set_apply_speed_limits
                                clip_to_selection=clip_to_selection
                                set_clip_to_selection=set_clip_to_selection
                                tangent_mode=tangent_mode
                                set_tangent_mode=set_tangent_mode
                            />
                        </aside>
                    </Show>
                    <components::OverlayDrawer
                        open=overlay_open
                        set_open=set_overlay_open
                        orm_style=orm_style
                        orm_visible=orm_visible
                        set_orm_visible=set_orm_visible
                    />
                    <components::Population
                        open=pop_editor_open
                        set_open=set_pop_editor_open
                        tool=pop_tool
                        set_tool=set_pop_tool
                        brush_radius=pop_brush_radius
                        set_brush_radius=set_pop_brush_radius
                        brush_strength=pop_brush_strength
                        set_brush_strength=set_pop_brush_strength
                        selection_bbox=selection_bbox
                    />
                    <components::BlueprintDrawer
                        open=drawer_open
                        set_open=set_drawer_open
                    />
                    <Show when=move || tour_active.get()>
                        <components::Tutorial
                            set_active=set_tour_active
                            replay=tour_replay
                            set_drawer_open=set_drawer_open
                            set_overlay_open=set_overlay_open
                            set_show_vector_layers=set_show_vector_layers
                        />
                    </Show>
                </section>
            </main>
        </Show>
    }
}
