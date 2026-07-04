use leptos::{wasm_bindgen, component, view, IntoView, create_signal, create_effect, SignalGet, SignalSet, SignalGetUntracked, spawn_local, ReadSignal, Callback, Callable, CollectView};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

const DEFAULT_Z_MIN: u8 = 0;
const DEFAULT_Z_MAX: u8 = 19;
const MAX_TILE_COUNT: u64 = 5_000_000;
const ORM_STANDARD: &str = "https://tiles.openrailwaymap.org/standard/{z}/{x}/{y}.png";

fn format_duration(secs: f64) -> String {
    if secs > 3600.0 {
        let h = secs / 3600.0;
        format!("{h:.1}h")
    } else if secs > 60.0 {
        let m = secs / 60.0;
        format!("{m:.0}m")
    } else {
        format!("{secs:.0}s")
    }
}
const AVG_TILE_KB: f64 = 5.0;
const TILES_PER_SEC: f64 = 120.0;

#[derive(Clone)]
struct TileSource {
    label: String,
    url: String,
}

#[component]
pub fn TileDownload(
    bbox: ReadSignal<Option<(f64, f64, f64, f64)>>,
    on_close: Callback<()>,
) -> impl IntoView {
    let (sources, set_sources) = create_signal::<Vec<TileSource>>(Vec::new());
    let (selected_idx, set_selected_idx) = create_signal::<Option<usize>>(None);
    let (custom_url, set_custom_url) = create_signal(String::new());
    let (name, set_name) = create_signal("tiles".to_string());
    let (z_min, set_z_min) = create_signal(DEFAULT_Z_MIN);
    let (z_max, set_z_max) = create_signal(DEFAULT_Z_MAX);
    let (tile_count, set_tile_count) = create_signal(0u64);
    let (downloading, set_downloading) = create_signal(false);
    let (progress_completed, set_progress_completed) = create_signal(0u64);
    let (progress_total, set_progress_total) = create_signal(0u64);
    let (progress_failed, set_progress_failed) = create_signal(0u64);
    let (progress_bytes, set_progress_bytes) = create_signal(0u64);
    let (error, set_error) = create_signal::<Option<String>>(None);
    let (done_message, set_done_message) = create_signal::<Option<String>>(None);
    let (download_start_ms, set_download_start_ms) = create_signal(0.0f64);

    spawn_local(async move {
        {
            let status = crate::tauri::get_overlay_status().await;
            let mut layer_sources = vec![
                TileSource { label: "ORM Vector (Offline Cache)".into(), url: "orm-vector".into() },
                TileSource { label: "OpenRailwayMap (Raster)".into(), url: ORM_STANDARD.into() },
            ];
            for group in &status.groups {
                for layer in &group.layers {
                    if let Some(ref url) = layer.source_url {
                        layer_sources.push(TileSource {
                            label: format!("{} / {}", group.name, layer.name),
                            url: url.clone(),
                        });
                    }
                }
            }
            set_sources.set(layer_sources);
        }
    });

    let resolved_url = move || {
        if let Some(idx) = selected_idx.get() {
            sources.get().get(idx).map(|s| s.url.clone()).unwrap_or_default()
        } else {
            custom_url.get()
        }
    };

    create_effect(move |_| {
        let Some((s, w, n, e)) = bbox.get() else {
            set_tile_count.set(0);
            return;
        };
        let z_lo = z_min.get();
        let z_hi = z_max.get();
        spawn_local(async move {
            match crate::tauri::count_tiles(s, w, n, e, z_lo, z_hi).await {
                Ok(count) => set_tile_count.set(count),
                Err(_) => set_tile_count.set(0),
            }
        });
    });

    let progress_closure = Closure::wrap(Box::new(move |event: JsValue| {
        if let Ok(payload) = js_sys::Reflect::get(&event, &"payload".into()) {
            let completed = js_sys::Reflect::get(&payload, &"completed".into())
                .ok().and_then(|v| v.as_f64()).unwrap_or(0.0) as u64;
            let total = js_sys::Reflect::get(&payload, &"total".into())
                .ok().and_then(|v| v.as_f64()).unwrap_or(0.0) as u64;
            let failed = js_sys::Reflect::get(&payload, &"failed".into())
                .ok().and_then(|v| v.as_f64()).unwrap_or(0.0) as u64;
            let bytes = js_sys::Reflect::get(&payload, &"bytes".into())
                .ok().and_then(|v| v.as_f64()).unwrap_or(0.0) as u64;
            set_progress_completed.set(completed);
            set_progress_total.set(total);
            set_progress_failed.set(failed);
            set_progress_bytes.set(bytes);
        }
    }) as Box<dyn Fn(JsValue)>);

    if let Ok(tauri) = js_sys::Reflect::get(&js_sys::global(), &"__TAURI__".into())
        && let Ok(event) = js_sys::Reflect::get(&tauri, &"event".into())
        && let Ok(listen) = js_sys::Reflect::get(&event, &"listen".into())
        && let Ok(listen_fn) = listen.dyn_into::<js_sys::Function>()
    {
        let _ = listen_fn.call2(&JsValue::NULL, &"tile-download-progress".into(), progress_closure.as_ref().unchecked_ref());
    }
    progress_closure.forget();

    let on_start = move |_| {
        let url_val = resolved_url();
        if url_val.is_empty() {
            set_error.set(Some("Select a source or enter a URL".into()));
            return;
        }
        if url_val != "orm-vector" {
            let has_xyz = url_val.contains("{z}") && url_val.contains("{x}") && url_val.contains("{y}");
            let has_quadkey = url_val.contains("{q}");
            if !has_xyz && !has_quadkey {
                set_error.set(Some("URL must contain {z}/{x}/{y} or {q} placeholders".into()));
                return;
            }
        }
        let count = tile_count.get_untracked();
        if count > MAX_TILE_COUNT {
            set_error.set(Some(format!("Too many tiles ({count}). Reduce the zoom range or area.")));
            return;
        }
        if count == 0 {
            set_error.set(Some("No tiles to download. Select an area first.".into()));
            return;
        }

        set_error.set(None);
        set_done_message.set(None);
        set_downloading.set(true);
        set_progress_completed.set(0);
        set_progress_total.set(0);
        set_progress_failed.set(0);
        set_download_start_ms.set(js_sys::Date::now());

        let Some((s, w, n, e)) = bbox.get_untracked() else { return };
        let name_val = name.get_untracked();
        let z_lo = z_min.get_untracked();
        let z_hi = z_max.get_untracked();

        spawn_local(async move {
            if url_val == "orm-vector" {
                match crate::tauri::download_orm_tiles(s, w, n, e, z_lo, z_hi).await {
                    Ok(path) => set_done_message.set(Some(format!("ORM offline cache saved to {path}"))),
                    Err(e) => set_error.set(Some(e)),
                }
            } else {
                match crate::tauri::start_tile_download(&url_val, &name_val, s, w, n, e, z_lo, z_hi).await {
                    Ok(path) => {
                        match crate::tauri::add_mbtiles_layer(&path, &name_val, None).await {
                            Ok(_) => set_done_message.set(Some("Saved and added as layer".into())),
                            Err(_) => set_done_message.set(Some(format!("Saved to {path}"))),
                        }
                    }
                    Err(e) => set_error.set(Some(e)),
                }
            }
            set_downloading.set(false);
        });
    };

    let on_cancel = move |_| {
        spawn_local(async move {
            let _ = crate::tauri::cancel_tile_download().await;
        });
    };

    let format_summary = move || {
        let count = tile_count.get();
        let tiles = if count > 1_000_000 {
            format!("{:.1}M tiles", count as f64 / 1_000_000.0)
        } else if count > 1_000 {
            format!("{:.1}K tiles", count as f64 / 1_000.0)
        } else {
            format!("{count} tiles")
        };

        let size_mb = count as f64 * AVG_TILE_KB / 1024.0;
        let size = if size_mb > 1024.0 {
            let gb = size_mb / 1024.0;
            format!("{gb:.1} GB")
        } else {
            format!("{size_mb:.0} MB")
        };
        let time = format_duration(count as f64 / TILES_PER_SEC);
        format!("{tiles} · ~{size} · ~{time}")
    };

    let progress_pct = move || {
        let total = progress_total.get();
        let done = progress_completed.get() + progress_failed.get();
        let pct = if total > 0 { done as f64 / total as f64 * 100.0 } else { 0.0 };
        format!("width: {pct:.1}%")
    };

    let progress_text = move || {
        let total = progress_total.get();
        let completed = progress_completed.get();
        let failed = progress_failed.get();
        let bytes = progress_bytes.get();
        let done = completed + failed;
        let elapsed_s = (js_sys::Date::now() - download_start_ms.get()) / 1000.0;
        let speed = if elapsed_s > 1.0 { done as f64 / elapsed_s } else { 0.0 };
        let remaining = total.saturating_sub(done);
        let eta = if speed > 0.0 { remaining as f64 / speed } else { 0.0 };

        let size_str = if completed > 0 {
            let avg_bytes = bytes as f64 / completed as f64;
            let est_total_mb = avg_bytes * total as f64 / (1024.0 * 1024.0);
            if est_total_mb > 1024.0 {
                format!(" · ~{:.1} GB", est_total_mb / 1024.0)
            } else {
                format!(" · ~{est_total_mb:.0} MB")
            }
        } else {
            String::new()
        };
        let fail_str = if failed > 0 { format!(", {failed} failed") } else { String::new() };
        format!("{done}/{total} · {speed:.0} tiles/s · {} remaining{size_str}{fail_str}", format_duration(eta))
    };

    view! {
        <div class="tile-download-panel">
            <header>
                <h3>"Download Tiles"</h3>
                <button class="icon-btn" on:click=move |_| on_close.call(()) title="Close">
                    <i class="fa-solid fa-xmark"></i>
                </button>
            </header>

            <fieldset disabled=downloading>
                <label>"Source"</label>
                <select on:change=move |ev| {
                    let val = leptos::event_target_value(&ev);
                    if val == "custom" {
                        set_selected_idx.set(None);
                    } else if let Ok(idx) = val.parse::<usize>() {
                        set_selected_idx.set(Some(idx));
                        let srcs = sources.get_untracked();
                        if let Some(src) = srcs.get(idx) {
                            set_name.set(src.label.replace(" / ", "_").replace(' ', "_").to_lowercase());
                        }
                    }
                }>
                    {move || sources.get().iter().enumerate().map(|(i, s)| {
                        view! { <option value=i.to_string() selected=selected_idx.get() == Some(i)>{&s.label}</option> }
                    }).collect_view()}
                    <option value="custom" selected=move || selected_idx.get().is_none()>"Custom URL..."</option>
                </select>

                {move || if selected_idx.get().is_none() {
                    Some(view! {
                        <input
                            type="text"
                            placeholder="https://example.com/{z}/{x}/{y}.png"
                            prop:value=custom_url
                            on:input=move |ev| set_custom_url.set(leptos::event_target_value(&ev))
                        />
                    })
                } else {
                    None
                }}

                <label>"Name"</label>
                <input
                    type="text"
                    prop:value=name
                    on:input=move |ev| set_name.set(leptos::event_target_value(&ev))
                />

                <div class="zoom-range">
                    <div>
                        <label>"Min Zoom"</label>
                        <input
                            type="number" min="0" max="22"
                            prop:value=move || z_min.get().to_string()
                            on:input=move |ev| {
                                if let Ok(v) = leptos::event_target_value(&ev).parse::<u8>() {
                                    set_z_min.set(v.min(22));
                                }
                            }
                        />
                    </div>
                    <div>
                        <label>"Max Zoom"</label>
                        <input
                            type="number" min="0" max="22"
                            prop:value=move || z_max.get().to_string()
                            on:input=move |ev| {
                                if let Ok(v) = leptos::event_target_value(&ev).parse::<u8>() {
                                    set_z_max.set(v.min(22));
                                }
                            }
                        />
                    </div>
                </div>

                <p class="tile-count" class:warning=move || { tile_count.get() > MAX_TILE_COUNT }>
                    {format_summary}
                </p>
            </fieldset>

            {move || error.get().map(|e| view! { <p class="error-text">{e}</p> })}
            {move || done_message.get().map(|m| view! { <p class="success-text">{m}</p> })}

            {move || if downloading.get() {
                Some(view! {
                    <div class="download-progress">
                        <div class="progress-bar">
                            <div class="progress-fill" style=progress_pct></div>
                        </div>
                        <p>{progress_text}</p>
                        <button on:click=on_cancel>"Cancel"</button>
                    </div>
                })
            } else {
                None
            }}

            <nav>
                <button
                    class="primary"
                    on:click=on_start
                    disabled=move || downloading.get() || bbox.get().is_none()
                >
                    <i class="fa-solid fa-download"></i>
                    " Download"
                </button>
            </nav>
        </div>
    }
}
