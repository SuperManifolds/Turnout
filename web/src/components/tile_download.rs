use leptos::{component, view, IntoView, create_signal, create_effect, SignalGet, SignalSet, SignalGetUntracked, spawn_local, ReadSignal, Callback, Callable, CollectView};

use crate::tiles::{download_summary, progress_pct, progress_summary};

const DEFAULT_Z_MIN: u8 = 0;
const DEFAULT_Z_MAX: u8 = 19;
const MAX_TILE_COUNT: u64 = 5_000_000;
const ORM_STANDARD: &str = "https://tiles.openrailwaymap.org/standard/{z}/{x}/{y}.png";
/// Vector download streams 64 concurrent HTTP/1.1 fetches with no pacing floor, and
/// each MVT tile carries all in-range ORM layers, so both size and rate differ from
/// raster.
const ORM_VECTOR_URL: &str = "orm-vector";
/// `OpenRailwayMap`'s sub-z7 line endpoints hang server-side, so vector offline
/// downloads start at z7 (kept in sync with `ORM_MIN_ZOOM` in the backend).
const ORM_VECTOR_MIN_ZOOM: u8 = 7;

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
    let (paused, set_paused) = create_signal(false);
    let (progress_completed, set_progress_completed) = create_signal(0u64);
    let (progress_total, set_progress_total) = create_signal(0u64);
    let (progress_failed, set_progress_failed) = create_signal(0u64);
    let (progress_bytes, set_progress_bytes) = create_signal(0u64);
    let (error, set_error) = create_signal::<Option<String>>(None);
    let (done_message, set_done_message) = create_signal::<Option<String>>(None);
    let (download_start_ms, set_download_start_ms) = create_signal(0.0f64);
    let (offline_active, set_offline_active) = create_signal(false);
    let (offline_path, set_offline_path) = create_signal::<Option<String>>(None);

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

    let effective_z_min = move || {
        if resolved_url() == ORM_VECTOR_URL {
            z_min.get().max(ORM_VECTOR_MIN_ZOOM)
        } else {
            z_min.get()
        }
    };

    create_effect(move |_| {
        let Some((s, w, n, e)) = bbox.get() else {
            set_tile_count.set(0);
            return;
        };
        let z_lo = effective_z_min();
        let z_hi = z_max.get();
        spawn_local(async move {
            match crate::tauri::count_tiles(s, w, n, e, z_lo, z_hi).await {
                Ok(count) => set_tile_count.set(count),
                Err(_) => set_tile_count.set(0),
            }
        });
    });

    crate::tauri::listen_to_events(&["tile-download-progress"], move |payload| {
        let field = |k: &str| {
            js_sys::Reflect::get(&payload, &k.into()).ok().and_then(|v| v.as_f64()).unwrap_or(0.0) as u64
        };
        set_progress_completed.set(field("completed"));
        set_progress_total.set(field("total"));
        set_progress_failed.set(field("failed"));
        set_progress_bytes.set(field("bytes"));
    });

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
        set_paused.set(false);
        set_progress_completed.set(0);
        set_progress_total.set(0);
        set_progress_failed.set(0);
        set_download_start_ms.set(js_sys::Date::now());

        let Some((s, w, n, e)) = bbox.get_untracked() else { return };
        let name_val = name.get_untracked();
        let z_lo = effective_z_min();
        let z_hi = z_max.get_untracked();

        spawn_local(async move {
            if url_val == "orm-vector" {
                match crate::tauri::download_orm_tiles(s, w, n, e, z_lo, z_hi).await {
                    Ok(path) => match crate::tauri::set_orm_offline(Some(&path)).await {
                        Ok(()) => {
                            set_offline_active.set(true);
                            set_offline_path.set(Some(path.clone()));
                            set_done_message.set(Some(format!("ORM offline mode active: {path}")));
                        }
                        Err(e) => set_error.set(Some(format!("Downloaded but failed to activate offline mode: {e}"))),
                    },
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
        set_paused.set(false);
        spawn_local(async move {
            let _ = crate::tauri::cancel_tile_download().await;
        });
    };

    let on_toggle_pause = move |_| {
        let next = !paused.get_untracked();
        set_paused.set(next);
        spawn_local(async move {
            let _ = crate::tauri::set_tile_download_paused(next).await;
        });
    };

    let on_disable_offline = move |_| {
        spawn_local(async move {
            if crate::tauri::set_orm_offline(None).await.is_ok() {
                set_offline_active.set(false);
                set_offline_path.set(None);
                set_done_message.set(Some("ORM offline mode disabled".into()));
            }
        });
    };

    let format_summary = move || {
        download_summary(tile_count.get(), resolved_url() == ORM_VECTOR_URL)
    };

    let progress_pct = move || {
        let pct = progress_pct(progress_completed.get(), progress_failed.get(), progress_total.get());
        format!("width: {pct:.1}%")
    };

    let progress_text = move || {
        let elapsed_s = (js_sys::Date::now() - download_start_ms.get()) / 1000.0;
        progress_summary(
            progress_completed.get(),
            progress_failed.get(),
            progress_total.get(),
            progress_bytes.get(),
            elapsed_s,
        )
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
                            if src.url == ORM_VECTOR_URL && z_min.get_untracked() < ORM_VECTOR_MIN_ZOOM {
                                set_z_min.set(ORM_VECTOR_MIN_ZOOM);
                            }
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

            {move || offline_active.get().then(|| view! {
                <section class="offline-active">
                    <p>
                        <i class="fa-solid fa-circle-check"></i>
                        " Offline ORM rendering active"
                        {move || offline_path.get().map(|p| view! { <small>{p}</small> })}
                    </p>
                    <button on:click=on_disable_offline>"Disable Offline Mode"</button>
                </section>
            })}

            {move || if downloading.get() {
                Some(view! {
                    <div class="download-progress">
                        <div class="progress-bar">
                            <div class="progress-fill" style=progress_pct></div>
                        </div>
                        <p>{move || if paused.get() { "Paused".to_string() } else { progress_text() }}</p>
                        <div class="progress-actions">
                            <button on:click=on_toggle_pause>
                                {move || if paused.get() { "Resume" } else { "Pause" }}
                            </button>
                            <button on:click=on_cancel>"Cancel"</button>
                        </div>
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
