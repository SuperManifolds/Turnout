use leptos::{wasm_bindgen, component, view, IntoView, ReadSignal, WriteSignal, create_signal, SignalGet, SignalSet, spawn_local, Show, CollectView};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

use crate::tauri;

const SOURCE_ID: &str = "kmz-overlay";

#[wasm_bindgen]
extern "C" {
    fn map_add_overlay_layer(id: &str, url: &str, opacity: f64);
    fn map_remove_overlay_layer(id: &str);
    fn map_set_overlay_opacity(id: &str, opacity: f64);
    fn map_fit_bounds(west: f64, south: f64, east: f64, north: f64);
}

fn refresh_map_layer(status: &tauri::OverlayStatus) {
    map_remove_overlay_layer(SOURCE_ID);
    map_add_overlay_layer(SOURCE_ID, &status.tile_url, 1.0);
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

    let (wms_open, set_wms_open) = create_signal(false);
    let (wms_url, set_wms_url) = create_signal(String::new());
    let (wms_loading, set_wms_loading) = create_signal(false);
    let (wms_layers, set_wms_layers) = create_signal::<Vec<tauri::WmsLayerInfo>>(Vec::new());

    let apply_status = move |status: &tauri::OverlayStatus| {
        set_layers.set(status.layers.clone());
        refresh_map_layer(status);
        set_tile_url.set(Some(status.tile_url.clone()));
    };

    spawn_local(async move {
        if let Some(status) = tauri::get_overlay_status().await {
            apply_status(&status);
        }
    });

    let on_add_kmz = move |_| {
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
                let _ = wasm_bindgen_futures::JsFuture::from(
                    clipboard.write_text(&url),
                ).await;
                set_copied.set(true);
                let _ = wasm_bindgen_futures::JsFuture::from(
                    js_sys::Promise::new(&mut |resolve, _| {
                        let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                            &resolve, 2000,
                        );
                    }),
                ).await;
                set_copied.set(false);
            });
        }
    };

    let do_wms_fetch = move || {
        let url = wms_url.get();
        if url.trim().is_empty() {
            return;
        }
        set_error.set(None);
        set_wms_loading.set(true);
        set_wms_layers.set(Vec::new());
        spawn_local(async move {
            match tauri::fetch_wms_layers(&url).await {
                Ok(layers) => set_wms_layers.set(layers),
                Err(e) => set_error.set(Some(e)),
            }
            set_wms_loading.set(false);
        });
    };

    let on_wms_select = move |name: String, title: String| {
        let url = wms_url.get();
        set_wms_loading.set(true);
        spawn_local(async move {
            match tauri::add_wms_layer(&url, &name, &title).await {
                Ok(status) => {
                    apply_status(&status);
                    set_wms_open.set(false);
                    set_wms_url.set(String::new());
                    set_wms_layers.set(Vec::new());
                }
                Err(e) => set_error.set(Some(e)),
            }
            set_wms_loading.set(false);
        });
    };

    let on_wms_url_keydown = move |ev: web_sys::KeyboardEvent| {
        if ev.key() == "Enter" {
            do_wms_fetch();
        }
    };

    view! {
        <Show when=move || open.get()>
            <aside id="overlay-drawer">
                <header>
                    <h3>"Overlays"</h3>
                    <button class="close-btn" on:click=move |_| set_open.set(false) title="Close">
                        <i class="fa-solid fa-xmark"></i>
                    </button>
                </header>

                <section class="overlay-actions">
                    <button on:click=on_add_kmz disabled=move || loading.get()>
                        <i class="fa-solid fa-file"></i>
                        {move || if loading.get() { " Loading\u{2026}" } else { " KMZ" }}
                    </button>
                    <button on:click=move |_| set_wms_open.set(!wms_open.get())
                        class:active=move || wms_open.get()
                    >
                        <i class="fa-solid fa-globe"></i>
                        " WMS"
                    </button>
                    <Show when=move || tile_url.get().is_some()>
                        <button on:click=on_copy title="Copy tile URL for Nimby Rails">
                            <i class=move || if copied.get() { "fa-solid fa-check" } else { "fa-solid fa-copy" }></i>
                        </button>
                    </Show>
                </section>

                <Show when=move || wms_open.get()>
                    <section class="wms-form">
                        <input
                            type="text"
                            placeholder="WMS server URL"
                            prop:value=move || wms_url.get()
                            on:input=move |ev| set_wms_url.set(leptos::event_target_value(&ev))
                            on:keydown=on_wms_url_keydown
                        />
                        <button on:click=move |_| do_wms_fetch() disabled=move || wms_loading.get() || wms_url.get().trim().is_empty()>
                            {move || if wms_loading.get() { "Loading\u{2026}" } else { "Fetch" }}
                        </button>
                    </section>

                    <Show when=move || !wms_layers.get().is_empty()>
                        <ul class="wms-layer-list">
                            {move || wms_layers.get().iter().map(|l| {
                                let name = l.name.clone();
                                let title = l.title.clone();
                                let display = title.clone();
                                view! {
                                    <li on:click=move |_| on_wms_select(name.clone(), title.clone())>
                                        {display}
                                    </li>
                                }
                            }).collect_view()}
                        </ul>
                    </Show>
                </Show>

                {move || error.get().map(|e| view! {
                    <p class="error">{e}</p>
                })}

                <Show when=move || layers.get().is_empty() && !loading.get() && !wms_open.get()>
                    <p class="empty">"No overlays loaded"</p>
                </Show>

                <ul>
                    {move || layers.get().iter().map(|l| {
                        let id = l.id;
                        let name = l.name.clone();
                        let visible = l.visible;
                        let layer_opacity = l.opacity;
                        let icon = if l.kind == "wms" { "fa-solid fa-globe" } else { "fa-solid fa-layer-group" };
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
