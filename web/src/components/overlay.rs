use leptos::{wasm_bindgen, component, view, IntoView, ReadSignal, WriteSignal, create_signal, SignalGet, SignalSet, spawn_local, Show, CollectView};
use wasm_bindgen::prelude::*;

use crate::tauri;

const DEFAULT_OPACITY: f64 = 0.7;
const SOURCE_ID: &str = "kmz-overlay";

#[wasm_bindgen]
extern "C" {
    fn map_add_overlay_layer(id: &str, url: &str, opacity: f64);
    fn map_remove_overlay_layer(id: &str);
    fn map_set_overlay_opacity(id: &str, opacity: f64);
    fn map_fit_bounds(west: f64, south: f64, east: f64, north: f64);
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
    let (opacity, set_opacity) = create_signal(DEFAULT_OPACITY);
    let (copied, set_copied) = create_signal(false);

    let apply_status = move |status: &tauri::OverlayStatus| {
        set_layers.set(status.layers.clone());
        map_remove_overlay_layer(SOURCE_ID);
        map_add_overlay_layer(SOURCE_ID, &status.tile_url, opacity.get());
        set_tile_url.set(Some(status.tile_url.clone()));
    };

    let on_add = move |_| {
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

    let on_opacity = move |ev: web_sys::Event| {
        let Some(target) = ev.target() else { return };
        let input: web_sys::HtmlInputElement = target.unchecked_into();
        let val: f64 = input.value().parse().unwrap_or(DEFAULT_OPACITY);
        set_opacity.set(val);
        map_set_overlay_opacity(SOURCE_ID, val);
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
                    <button on:click=on_add disabled=move || loading.get()>
                        <i class="fa-solid fa-plus"></i>
                        {move || if loading.get() { " Loading\u{2026}" } else { " Add KMZ" }}
                    </button>
                    <Show when=move || tile_url.get().is_some()>
                        <button on:click=on_copy title="Copy tile URL for Nimby Rails">
                            <i class=move || if copied.get() { "fa-solid fa-check" } else { "fa-solid fa-copy" }></i>
                            {move || if copied.get() { " Copied" } else { " Copy URL" }}
                        </button>
                    </Show>
                </section>

                {move || error.get().map(|e| view! {
                    <p class="error">{e}</p>
                })}

                <Show when=move || layers.get().is_empty() && !loading.get()>
                    <p class="empty">"No overlays loaded"</p>
                </Show>

                <ul>
                    {move || layers.get().iter().map(|l| {
                        let id = l.id;
                        let name = l.name.clone();
                        view! {
                            <li class="overlay-item">
                                <i class="fa-solid fa-layer-group overlay-icon"></i>
                                <span class="overlay-name">{name}</span>
                                <button class="icon-btn danger" title="Remove" on:click=move |_| on_remove(id)>
                                    <i class="fa-solid fa-trash"></i>
                                </button>
                            </li>
                        }
                    }).collect_view()}
                </ul>

                <Show when=move || !layers.get().is_empty()>
                    <section class="overlay-opacity">
                        <label>"Opacity"</label>
                        <input
                            type="range"
                            min="0" max="1" step="0.05"
                            prop:value=move || opacity.get().to_string()
                            on:input=on_opacity
                        />
                    </section>
                </Show>
            </aside>
        </Show>
    }
}
