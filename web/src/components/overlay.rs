use leptos::{wasm_bindgen, component, view, IntoView, ReadSignal, WriteSignal, create_signal, SignalGet, SignalSet, spawn_local, Show, CollectView};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

use crate::tauri;

const SOURCE_ID: &str = "kmz-overlay";

#[wasm_bindgen]
extern "C" {
    fn map_add_overlay_layer(id: &str, url: &str, opacity: f64);
    fn map_remove_overlay_layer(id: &str);
    fn map_fit_bounds(west: f64, south: f64, east: f64, north: f64);
}

fn refresh_map_layer(status: &tauri::OverlayStatus) {
    map_remove_overlay_layer(SOURCE_ID);
    map_add_overlay_layer(SOURCE_ID, &status.tile_url, 1.0);
}

#[derive(Clone, Copy, PartialEq)]
enum ServiceForm { None, Wms, ArcGis }

#[derive(Clone)]
struct ServiceEntry {
    name: String,
    display: String,
}

#[component]
pub fn OverlayDrawer(
    open: ReadSignal<bool>,
    set_open: WriteSignal<bool>,
) -> impl IntoView {
    let (layers, set_layers) = create_signal::<Vec<tauri::LayerInfo>>(Vec::new());
    let (tile_url, set_tile_url) = create_signal::<Option<String>>(None);
    let (loading, set_loading) = create_signal(false);
    let (error, set_error) = create_signal::<Option<String>>(None);
    let (copied, set_copied) = create_signal(false);

    let (menu_open, set_menu_open) = create_signal(false);
    let (active_form, set_active_form) = create_signal(ServiceForm::None);
    let (service_url, set_service_url) = create_signal(String::new());
    let (service_loading, set_service_loading) = create_signal(false);
    let (service_entries, set_service_entries) = create_signal::<Vec<ServiceEntry>>(Vec::new());

    let apply_status = move |status: &tauri::OverlayStatus| {
        set_layers.set(status.layers.clone());
        refresh_map_layer(status);
        set_tile_url.set(Some(status.tile_url.clone()));
    };

    spawn_local(async move {
        if let Some(status) = tauri::restore_overlays().await {
            apply_status(&status);
        }
    });

    let on_add_kmz_action = move || {
        set_error.set(None);
        spawn_local(async move {
            let Some(path) = tauri::pick_kmz_file().await else { return };
            set_loading.set(true);
            match tauri::add_overlay(&path).await {
                Ok(status) => {
                    if let Some(last) = status.layers.last() {
                        let [south, west, north, east] = last.bbox;
                        map_fit_bounds(west, south, east, north);
                    }
                    apply_status(&status);
                }
                Err(e) => set_error.set(Some(e)),
            }
            set_loading.set(false);
        });
    };

    let on_remove = move |id: u32| {
        spawn_local(async move {
            if let Some(status) = tauri::remove_overlay(id).await {
                apply_status(&status);
            } else {
                map_remove_overlay_layer(SOURCE_ID);
                set_layers.set(Vec::new());
                set_tile_url.set(None);
            }
        });
    };

    let on_toggle_visible = move |id: u32, visible: bool| {
        spawn_local(async move {
            if let Some(status) = tauri::set_layer_visible(id, visible).await {
                apply_status(&status);
            }
        });
    };

    let on_layer_opacity = move |id: u32, val: f32| {
        spawn_local(async move {
            if let Some(status) = tauri::set_layer_opacity(id, val).await {
                apply_status(&status);
            }
        });
    };

    let on_copy = move |_| {
        if let Some(url) = tile_url.get() {
            spawn_local(async move {
                let Some(window) = web_sys::window() else { return };
                let clipboard = window.navigator().clipboard();
                let _ = wasm_bindgen_futures::JsFuture::from(clipboard.write_text(&url)).await;
                set_copied.set(true);
                let _ = wasm_bindgen_futures::JsFuture::from(
                    js_sys::Promise::new(&mut |resolve, _| {
                        let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 2000);
                    }),
                ).await;
                set_copied.set(false);
            });
        }
    };

    let toggle_form = move |form: ServiceForm| {
        if active_form.get() == form {
            set_active_form.set(ServiceForm::None);
        } else {
            set_active_form.set(form);
            set_service_url.set(String::new());
            set_service_entries.set(Vec::new());
        }
    };

    let do_fetch = move || {
        let url = service_url.get();
        if url.trim().is_empty() { return; }
        let form = active_form.get();
        set_error.set(None);
        set_service_loading.set(true);
        set_service_entries.set(Vec::new());
        spawn_local(async move {
            let result = match form {
                ServiceForm::Wms => tauri::fetch_wms_layers(&url).await.map(|layers| {
                    layers.into_iter().map(|l| ServiceEntry { name: l.name, display: l.title }).collect()
                }),
                ServiceForm::ArcGis => tauri::fetch_arcgis_services(&url).await.map(|services| {
                    services.into_iter().map(|s| ServiceEntry { name: s.name.clone(), display: s.name }).collect()
                }),
                ServiceForm::None => return,
            };
            match result {
                Ok(entries) => set_service_entries.set(entries),
                Err(e) => set_error.set(Some(e)),
            }
            set_service_loading.set(false);
        });
    };

    let on_service_select = move |name: String, display: String| {
        let url = service_url.get();
        let form = active_form.get();
        set_service_loading.set(true);
        spawn_local(async move {
            let result = match form {
                ServiceForm::Wms => tauri::add_wms_layer(&url, &name, &display).await,
                ServiceForm::ArcGis => tauri::add_arcgis_layer(&url, &name, &display).await,
                ServiceForm::None => return,
            };
            match result {
                Ok(status) => {
                    apply_status(&status);
                    set_active_form.set(ServiceForm::None);
                    set_service_url.set(String::new());
                    set_service_entries.set(Vec::new());
                }
                Err(e) => set_error.set(Some(e)),
            }
            set_service_loading.set(false);
        });
    };

    let on_url_keydown = move |ev: web_sys::KeyboardEvent| {
        if ev.key() == "Enter" { do_fetch(); }
    };

    view! {
        <Show when=move || open.get()>
            <aside id="overlay-drawer">
                <header>
                    <h3>"Overlays"</h3>
                    <Show when=move || tile_url.get().is_some()>
                        <button class="copy-url-btn" on:click=on_copy title="Copy tile URL for Nimby Rails">
                            <i class=move || if copied.get() { "fa-solid fa-check" } else { "fa-solid fa-copy" }></i>
                            {move || if copied.get() { " Copied" } else { " URL" }}
                        </button>
                    </Show>
                    <button class="close-btn" on:click=move |_| set_open.set(false) title="Close">
                        <i class="fa-solid fa-xmark"></i>
                    </button>
                </header>

                <section class="overlay-actions">
                    <div class="add-menu">
                        <button on:click=move |_| set_menu_open.set(!menu_open.get()) disabled=move || loading.get()>
                            <i class="fa-solid fa-plus"></i>
                            {move || if loading.get() { " Loading\u{2026}" } else { " Add layer" }}
                        </button>
                        <Show when=move || menu_open.get()>
                            <ul class="add-menu-list">
                                <li on:click=move |_| { set_menu_open.set(false); on_add_kmz_action(); }>
                                    <i class="fa-solid fa-file"></i>" KMZ / KML file"
                                </li>
                                <li on:click=move |_| { set_menu_open.set(false); on_add_kmz_action(); }>
                                    <i class="fa-solid fa-shapes"></i>" Shapefile"
                                </li>
                                <li on:click=move |_| { set_menu_open.set(false); toggle_form(ServiceForm::Wms); }>
                                    <i class="fa-solid fa-globe"></i>" WMS server"
                                </li>
                                <li on:click=move |_| { set_menu_open.set(false); toggle_form(ServiceForm::ArcGis); }>
                                    <i class="fa-solid fa-server"></i>" ArcGIS MapServer"
                                </li>
                            </ul>
                        </Show>
                    </div>
                </section>

                <Show when=move || active_form.get() != ServiceForm::None>
                    <section class="service-form">
                        <input
                            type="text"
                            placeholder=move || match active_form.get() {
                                ServiceForm::Wms => "WMS server URL",
                                ServiceForm::ArcGis => "ArcGIS services URL",
                                ServiceForm::None => "",
                            }
                            prop:value=move || service_url.get()
                            on:input=move |ev| set_service_url.set(leptos::event_target_value(&ev))
                            on:keydown=on_url_keydown
                        />
                        <button on:click=move |_| do_fetch() disabled=move || service_loading.get() || service_url.get().trim().is_empty()>
                            {move || if service_loading.get() { "Loading\u{2026}" } else { "Fetch" }}
                        </button>
                    </section>

                    <Show when=move || !service_entries.get().is_empty()>
                        <ul class="service-list">
                            {move || service_entries.get().iter().map(|e| {
                                let name = e.name.clone();
                                let display = e.display.clone();
                                let label = display.clone();
                                view! {
                                    <li on:click=move |_| on_service_select(name.clone(), display.clone())>
                                        {label}
                                    </li>
                                }
                            }).collect_view()}
                        </ul>
                    </Show>
                </Show>

                {move || error.get().map(|e| view! {
                    <p class="error">{e}</p>
                })}

                <Show when=move || layers.get().is_empty() && !loading.get() && active_form.get() == ServiceForm::None>
                    <p class="empty">"No overlays loaded"</p>
                </Show>

                <ul>
                    {move || layers.get().iter().map(|l| {
                        let id = l.id;
                        let name = l.name.clone();
                        let visible = l.visible;
                        let layer_opacity = l.opacity;
                        let icon = match l.kind.as_str() {
                            "wms" => "fa-solid fa-globe",
                            "arcgis" => "fa-solid fa-server",
                            "shp" => "fa-solid fa-shapes",
                            _ => "fa-solid fa-layer-group",
                        };
                        view! {
                            <li class="overlay-item">
                                <button class="icon-btn visibility-toggle"
                                    on:click=move |_| on_toggle_visible(id, !visible)
                                    title=move || if visible { "Hide" } else { "Show" }
                                >
                                    <i class=if visible { "fa-solid fa-eye" } else { "fa-solid fa-eye-slash" }></i>
                                </button>
                                <i class=format!("{icon} overlay-icon")></i>
                                <span class="overlay-name">{name}</span>
                                <button class="icon-btn danger" title="Remove" on:click=move |_| on_remove(id)>
                                    <i class="fa-solid fa-trash"></i>
                                </button>
                                <input
                                    type="range"
                                    class="layer-opacity"
                                    min="0" max="1" step="0.05"
                                    prop:value=layer_opacity.to_string()
                                    on:change=move |ev: web_sys::Event| {
                                        let Some(target) = ev.target() else { return };
                                        let input: web_sys::HtmlInputElement = target.unchecked_into();
                                        let val: f32 = input.value().parse().unwrap_or(1.0);
                                        on_layer_opacity(id, val);
                                    }
                                    title="Layer opacity"
                                />
                            </li>
                        }
                    }).collect_view()}
                </ul>
            </aside>
        </Show>
    }
}
