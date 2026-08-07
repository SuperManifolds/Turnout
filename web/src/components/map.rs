use leptos::{wasm_bindgen, component, view, web_sys, WriteSignal, ReadSignal, IntoView, create_node_ref, html, create_signal, store_value, create_effect, SignalGet, SignalGetUntracked, SignalSet, SignalUpdate, spawn_local, Callback, Show};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

const ORM_DEFAULT_STYLE: &str = "standard";
const MAX_BBOX_AREA_DEG2: f64 = 1.0;
const BBOX_COLOR: &str = "#4a9eff";
const BBOX_ERROR_COLOR: &str = "#d32f2f";
const HANDLE_COLOR: &str = "#4a9eff";
const HANDLE_RADIUS: f64 = 6.0;

#[wasm_bindgen]
extern "C" {
    fn map_init(container: &web_sys::HtmlElement) -> JsValue;
    fn map_on_load(callback: &Closure<dyn Fn()>);
    fn map_add_raster_source(id: &str, url: &str, attribution: &str);
    fn map_add_raster_layer(id: &str, source: &str, opacity: f64);
    fn map_add_geojson_source(id: &str);
    fn map_add_fill_layer(id: &str, source: &str, color: &str, opacity: f64);
    fn map_add_line_layer(id: &str, source: &str, color: &str, width: f64);
    fn map_add_circle_layer(id: &str, source: &str, color: &str, radius: f64);
    fn map_set_geojson(source_id: &str, geojson_str: &str);
    fn map_set_cursor(cursor: &str);
    fn map_set_drag_pan(enabled: bool);
    fn map_set_rotate(enabled: bool);
    fn map_fly_to(lng: f64, lat: f64, zoom: f64);
    fn map_add_preview_layer();
    fn map_on_mousedown(callback: &Closure<dyn Fn(f64, f64)>);
    fn map_on_mousemove(callback: &Closure<dyn Fn(f64, f64)>);
    fn map_on_mouseup(callback: &Closure<dyn Fn(f64, f64)>);
    fn map_query_features(lng: f64, lat: f64, layer_id: &str) -> JsValue;
    fn map_set_bbox_color(color: &str);
    fn map_set_orm_style(style_name: &str);
    fn map_set_orm_port(port: u16);
}

use crate::geo::{Handle, apply_handle_drag, bbox_geojson, empty_geojson, format_bbox, handles_geojson};

// Interaction modes
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Idle,
    Drawing,
    Resizing(Handle),
}

fn update_bbox_display(s: f64, w: f64, n: f64, e: f64) {
    map_set_geojson("bbox", &bbox_geojson(w, s, e, n));
    map_set_geojson("handles", &handles_geojson(w, s, e, n));
}

fn clear_bbox_display() {
    map_set_geojson("bbox", empty_geojson());
    map_set_geojson("handles", empty_geojson());
}

#[component]
pub fn Map(
    set_available_types: WriteSignal<Vec<String>>,
    enabled_types: ReadSignal<Vec<String>>,
    set_has_selection: WriteSignal<bool>,
    apply_speed_limits: ReadSignal<bool>,
    clip_to_selection: ReadSignal<bool>,
    tangent_mode: ReadSignal<bool>,
    set_drawer_open: WriteSignal<bool>,
    show_vector_layers: ReadSignal<bool>,
    set_show_vector_layers: WriteSignal<bool>,
) -> impl IntoView {
    let map_ref = create_node_ref::<html::Div>();
    let (bbox, set_bbox) = create_signal::<Option<(f64, f64, f64, f64)>>(None);
    let (status, set_status) = create_signal(String::from("Navigate to an area, then click Select Area"));
    let (over_limit, set_over_limit) = create_signal(false);

    // Reactively update bbox color when over_limit changes
    create_effect(move |_| {
        let color = if over_limit.get() { BBOX_ERROR_COLOR } else { BBOX_COLOR };
        map_set_bbox_color(color);
    });

    let mode = store_value(Mode::Idle);
    let draw_start = store_value::<Option<(f64, f64)>>(None);
    let fetch_generation = store_value(0u32);

    // Listen for paste events on the window to handle ORM links
    create_effect(move |_| {
        let closure = Closure::<dyn Fn(web_sys::Event)>::new(move |ev: web_sys::Event| {
            let Some(clipboard_ev) = ev.dyn_ref::<web_sys::ClipboardEvent>() else { return };
            let Some(data) = clipboard_ev.clipboard_data() else { return };
            let Ok(text) = data.get_data("text/plain") else { return };
            if let Some((zoom, lat, lng)) = parse_orm_link(&text) {
                ev.prevent_default();
                map_fly_to(lng, lat, zoom);
            }
        });
        let window = web_sys::window().expect("window");
        let _ = window.add_event_listener_with_callback("paste", closure.as_ref().unchecked_ref());
        closure.forget();
    });

    create_effect(move |_| {
        let Some(div) = map_ref.get() else { return };
        let element: &web_sys::HtmlElement = &div;
        map_init(element);

        let on_load = Closure::new(move || {
            map_add_preview_layer();
            map_add_geojson_source("bbox");
            map_add_fill_layer("bbox-fill", "bbox", BBOX_COLOR, 0.15);
            map_add_line_layer("bbox-outline", "bbox", BBOX_COLOR, 2.0);
            map_add_geojson_source("handles");
            map_add_circle_layer("handles-layer", "handles", HANDLE_COLOR, HANDLE_RADIUS);
            // Defer ORM style load slightly — MapLibre needs to complete its
            // first render cycle before raster tile sources trigger fetching.
            // Resolve the tile server's actual bound port first; every ORM tile
            // URL is built from it.
            let cb = Closure::once(move || {
                spawn_local(async move {
                    match crate::tauri::get_orm_port().await {
                        Ok(port) => {
                            map_set_orm_port(port);
                            map_set_orm_style(ORM_DEFAULT_STYLE);
                        }
                        Err(err) => {
                            crate::tauri::report_error("ORM tile server bootstrap", &err);
                        }
                    }
                });
            });
            if let Some(win) = web_sys::window() {
                let _ = win.set_timeout_with_callback_and_timeout_and_arguments_0(
                    cb.as_ref().unchecked_ref(), 500,
                );
            }
            cb.forget();
        });
        map_on_load(&on_load);
        on_load.forget();

        let on_mousedown = Closure::new(move |lng: f64, lat: f64| {
            match mode.get_value() {
                Mode::Drawing => {
                    draw_start.set_value(Some((lng, lat)));
                }
                Mode::Idle => {
                    // Check if clicking on a handle
                    let hit = map_query_features(lng, lat, "handles-layer");
                    if let Some(handle_str) = hit.as_string()
                        && let Some(handle) = Handle::from_str(&handle_str) {
                            mode.set_value(Mode::Resizing(handle));
                            map_set_cursor(handle.cursor());
                            map_set_drag_pan(false);
                        }
                }
                Mode::Resizing(_) => {}
            }
        });
        map_on_mousedown(&on_mousedown);
        on_mousedown.forget();

        let on_mousemove = Closure::new(move |lng: f64, lat: f64| {
            match mode.get_value() {
                Mode::Drawing => {
                    let Some((start_lng, start_lat)) = draw_start.get_value() else { return };
                    let w = start_lng.min(lng);
                    let s = start_lat.min(lat);
                    let e = start_lng.max(lng);
                    let n = start_lat.max(lat);
                    update_bbox_display(s, w, n, e);
                }
                Mode::Resizing(handle) => {
                    let Some(current) = bbox.get_untracked() else { return };
                    let new_bbox = apply_handle_drag(handle, current, lng, lat);
                    update_bbox_display(new_bbox.0, new_bbox.1, new_bbox.2, new_bbox.3);
                }
                Mode::Idle => {
                    // Show resize cursor when hovering handles
                    if bbox.get_untracked().is_some() {
                        let hit = map_query_features(lng, lat, "handles-layer");
                        if let Some(handle_str) = hit.as_string()
                            && let Some(handle) = Handle::from_str(&handle_str) {
                                map_set_cursor(handle.cursor());
                                return;
                            }
                        map_set_cursor("");
                    }
                }
            }
        });
        map_on_mousemove(&on_mousemove);
        on_mousemove.forget();

        let on_mouseup = Closure::new(move |lng: f64, lat: f64| {
            match mode.get_value() {
                Mode::Drawing => {
                    let Some((start_lng, start_lat)) = draw_start.get_value() else { return };
                    let w = start_lng.min(lng);
                    let s = start_lat.min(lat);
                    let e = start_lng.max(lng);
                    let n = start_lat.max(lat);

                    draw_start.set_value(None);
                    mode.set_value(Mode::Idle);
                    map_set_cursor("");
                    map_set_drag_pan(true);
                    set_bbox.set(Some((s, w, n, e)));
                    update_bbox_display(s, w, n, e);
                    set_status.set(format_bbox(s, w, n, e));
                }
                Mode::Resizing(handle) => {
                    let Some(current) = bbox.get_untracked() else { return };
                    let new_bbox = apply_handle_drag(handle, current, lng, lat);
                    mode.set_value(Mode::Idle);
                    map_set_cursor("");
                    map_set_drag_pan(true);
                    set_bbox.set(Some(new_bbox));
                    update_bbox_display(new_bbox.0, new_bbox.1, new_bbox.2, new_bbox.3);
                    set_status.set(format_bbox(new_bbox.0, new_bbox.1, new_bbox.2, new_bbox.3));
                }
                Mode::Idle => {}
            }
        });
        map_on_mouseup(&on_mouseup);
        on_mouseup.forget();
    });

    // Debounced preview: when bbox changes, fetch ORM data and show on map
    let preview_timeout = store_value::<Option<i32>>(None);
    let cached_json = store_value::<Option<String>>(None);
    let cached_bbox = store_value::<Option<(f64, f64, f64, f64)>>(None);

    create_effect(move |_| {
        let current_bbox = bbox.get();
        if current_bbox.is_none() {
            map_set_geojson("preview", empty_geojson());
            cached_json.set_value(None);
            cached_bbox.set_value(None);
            return;
        }
        let (s, w, n, e) = current_bbox.expect("bbox is Some after is_none() check");
        // Skip if bbox unchanged
        if cached_bbox.get_value() == current_bbox { return; }

        // Cancel previous debounce
        if let Some(h) = preview_timeout.get_value() {
            web_sys::window().expect("window").clear_timeout_with_handle(h);
        }
        let fetch_gen = fetch_generation.get_value().wrapping_add(1);
        fetch_generation.set_value(fetch_gen);
        let cb = Closure::once(move || {
            spawn_local(async move {
                // Guard against oversized bbox
                let area = (n - s).abs() * (e - w).abs();
                if area > MAX_BBOX_AREA_DEG2 {
                    set_status.set("Selection too large — reduce the area".into());
                    set_over_limit.set(true);
                    return;
                }
                set_status.set("Fetching tracks...".into());
                let timeout = crate::components::app_settings::load_settings().await.overpass_timeout;
                let query = format!(
                    "[out:json][timeout:{timeout}];(way[\"railway\"]({s},{w},{n},{e}););(._;>;);out body;"
                );
                match crate::tauri::fetch_overpass(&query).await {
                    Ok(json) => {
                        // Discard stale response if a newer fetch was started
                        if fetch_generation.get_value() != fetch_gen { return; }
                        let enabled = enabled_types.get_untracked();
                        let stats = analyze_overpass_json(&json, &enabled);
                        set_available_types.set(stats.railway_types);
                        set_has_selection.set(true);
                        let clip = if clip_to_selection.get_untracked() { Some((s, w, n, e)) } else { None };
                        let geojson = osm_json_to_geojson(&json, &enabled, clip);
                        map_set_geojson("preview", &geojson);
                        cached_json.set_value(Some(json.clone()));
                        cached_bbox.set_value(Some((s, w, n, e)));

                        if stats.total_nodes > BAIL_NODE_THRESHOLD {
                            set_over_limit.set(true);
                            set_status.set(format!(
                                "{} ways (~{} nodes) — exceeds limit, reduce selection",
                                stats.way_count, stats.total_nodes
                            ));
                        } else {
                            set_status.set(format!("{} ways — counting nodes...", stats.way_count));
                            let clip_bbox = if clip_to_selection.get_untracked() { Some((s, w, n, e)) } else { None };
                            let tangent = tangent_mode.get_untracked();
                            match crate::tauri::count_track_nodes(&json, &enabled, clip_bbox, tangent).await {
                                Ok(exact) => {
                                    set_over_limit.set(exact > MAX_TRACK_NODES);
                                    if exact > MAX_TRACK_NODES {
                                        set_status.set(format!(
                                            "{} ways, {exact} nodes — exceeds {MAX_TRACK_NODES} limit",
                                            stats.way_count
                                        ));
                                    } else {
                                        set_status.set(format!(
                                            "{} ways, {exact} / {MAX_TRACK_NODES} nodes",
                                            stats.way_count
                                        ));
                                    }
                                }
                                Err(err) => {
                                    set_over_limit.set(false);
                                    set_status.set(format!("{} ways (count failed: {err})", stats.way_count));
                                }
                            }
                        }
                    }
                    Err(err) => {
                        set_status.set(format!("Fetch failed: {err}"));
                    }
                }
            });
        });
        let h = web_sys::window().expect("window")
            .set_timeout_with_callback_and_timeout_and_arguments_0(
                cb.as_ref().unchecked_ref(), 500
            ).unwrap_or(0);
        cb.forget();
        preview_timeout.set_value(Some(h));
    });

    // Re-filter preview when enabled types or clip setting change
    create_effect(move |_| {
        let enabled = enabled_types.get();
        let clip_on = clip_to_selection.get();
        if let Some(json) = cached_json.get_value() {
            let clip = if clip_on { cached_bbox.get_value() } else { None };
            let geojson = osm_json_to_geojson(&json, &enabled, clip);
            map_set_geojson("preview", &geojson);
        }
    });

    let on_select_area = move |_| {
        set_status.set("Click and drag to draw a rectangle...".into());
        set_bbox.set(None);
        clear_bbox_display();
        mode.set_value(Mode::Drawing);
        map_set_cursor("crosshair");
        map_set_drag_pan(false);
        map_set_rotate(false);
    };

    let on_clear = move |_| {
        clear_bbox_display();
        map_set_geojson("preview", empty_geojson());
        set_bbox.set(None);
        cached_json.set_value(None);
        cached_bbox.set_value(None);
        set_has_selection.set(false);
        set_available_types.set(vec![]);
        mode.set_value(Mode::Idle);
        map_set_rotate(true);
        set_status.set("Navigate to an area, then click Select Area".into());
    };

    let (show_tile_download, set_show_tile_download) = create_signal(false);
    let (show_name_prompt, set_show_name_prompt) = create_signal(false);
    let (success_message, set_success_message) = create_signal::<Option<String>>(None);

    let on_import_click = move |_| {
        set_show_name_prompt.set(true);
    };

    let on_name_confirm = Callback::new(move |name: String| {
        set_show_name_prompt.set(false);
        let Some((s, w, n, e)) = bbox.get_untracked() else { return };
        let cached = cached_json.get_value();
        let types = enabled_types.get_untracked();
        let speed = apply_speed_limits.get_untracked();
        let clip = clip_to_selection.get_untracked();
        let tangent = tangent_mode.get_untracked();
        spawn_local(async move {
            do_import(s, w, n, e, &name, cached.as_deref(), &types, speed, clip, tangent, set_status, set_success_message).await;
        });
    });

    let on_name_cancel = Callback::new(move |()| {
        set_show_name_prompt.set(false);
    });

    view! {
        <div id="map-wrapper">
            <div node_ref=map_ref id="map-canvas"></div>
            <nav id="map-controls">
                <span id="map-status" class:error=move || over_limit.get()>{status}</span>
                <Show when=move || bbox.get().is_none()>
                    <button on:click=on_select_area>"Select Area"</button>
                </Show>
                <Show when=move || bbox.get().is_some()>
                    <button class="primary" on:click=on_import_click disabled=move || over_limit.get() || cached_json.get_value().is_none()>"Import Tracks"</button>
                    <button on:click=move |_| set_show_tile_download.update(|v| *v = !*v)>
                        <i class="fa-solid fa-download"></i>
                        " Download Tiles"
                    </button>
                    <button on:click=move |_| set_show_vector_layers.update(|v| *v = !*v)>
                        <i class="fa-solid fa-layer-group"></i>
                        " Rail Layers"
                    </button>
                    <button on:click=on_select_area>"Redraw"</button>
                    <button on:click=on_clear>"Clear"</button>
                </Show>
                <button on:click=move |_| set_drawer_open.update(|v| *v = !*v)>"Blueprints"</button>
            </nav>
            <Show when=move || show_tile_download.get()>
                <super::tile_download::TileDownload
                    bbox=bbox
                    on_close=Callback::new(move |()| set_show_tile_download.set(false))
                />
            </Show>
            <Show when=move || show_vector_layers.get()>
                <super::vector_layers::VectorLayers
                    bbox=bbox
                    on_close=Callback::new(move |()| set_show_vector_layers.set(false))
                />
            </Show>
            <Show when=move || show_name_prompt.get()>
                <super::NamePrompt
                    default_name="import".to_string()
                    on_confirm=on_name_confirm
                    on_cancel=on_name_cancel
                />
            </Show>
            <Show when=move || success_message.get().is_some()>
                <div id="success-modal" on:click=move |_| set_success_message.set(None)>
                    <div class="success-content" on:click=move |ev| ev.stop_propagation()>
                        <h2>"Blueprint Saved"</h2>
                        <p>{move || success_message.get().unwrap_or_default()}</p>
                        <button class="primary" on:click=move |_| set_success_message.set(None)>"OK"</button>
                    </div>
                </div>
            </Show>
        </div>
    }
}

async fn do_import(s: f64, w: f64, n: f64, e: f64, name: &str, cached_json: Option<&str>, railway_types: &[String], apply_speed_limits: bool, clip: bool, tangent_mode: bool, set_status: WriteSignal<String>, set_success: WriteSignal<Option<String>>) {
    // Step 1: Use cached JSON or fetch (preview should always have cached it)
    let json = if let Some(cached) = cached_json {
        cached.to_string()
    } else {
        set_status.set("Fetching railway data...".into());
        let timeout = crate::components::app_settings::load_settings().await.overpass_timeout;
        let query = format!(
            "[out:json][timeout:{timeout}];(way[\"railway\"]({s},{w},{n},{e}););(._;>;);out body;"
        );
        match crate::tauri::fetch_overpass(&query).await {
            Ok(j) => j,
            Err(err) => {
                set_status.set(format!("Fetch failed: {err}"));
                return;
            }
        }
    };

    // Step 2: Import via Tauri backend — listen for progress events
    set_status.set("Processing tracks...".into());
    let unlisten = listen_for_progress(set_status).await;
    let clip_bbox = if clip { Some((s, w, n, e)) } else { None };
    let type_speed_overrides = crate::components::app_settings::load_settings().await.type_speed_overrides;
    let (data, node_count) = match crate::tauri::import_orm(&json, name, railway_types, apply_speed_limits, clip_bbox, tangent_mode, &type_speed_overrides).await {
        Ok(d) => { unlisten(); d }
        Err(err) => {
            unlisten();
            crate::tauri::report_error("blueprint import", &err);
            set_status.set(format!("Import failed: {err}"));
            return;
        }
    };

    // Step 3: Save via Tauri backend
    set_status.set("Saving blueprint...".into());
    match crate::tauri::save_blueprint(name, &data).await {
        Ok(path) => {
            set_status.set("Ready".into());
            set_success.set(Some(format!("{path}\n{node_count} / 50,000 nodes")));
        }
        Err(err) => {
            set_status.set(format!("Save failed: {err}"));
        }
    }
}

use turnout_core::geojson::{osm_json_to_geojson, analyze_overpass_json, parse_orm_link};

/// Listen for `import-progress` events from the backend and update the status bar.
/// Returns a closure that removes the listener when called.
async fn listen_for_progress(set_status: WriteSignal<String>) -> impl FnOnce() {
    let window = web_sys::window().expect("window");
    let tauri = js_sys::Reflect::get(&window, &"__TAURI__".into()).ok();
    let event_mod = tauri.as_ref().and_then(|t| js_sys::Reflect::get(t, &"event".into()).ok());
    let listen_fn = event_mod.as_ref()
        .and_then(|e| js_sys::Reflect::get(e, &"listen".into()).ok())
        .and_then(|f| f.dyn_into::<js_sys::Function>().ok());

    let unlisten_fn: Option<js_sys::Function> = if let Some(listen) = listen_fn {
        let callback = Closure::wrap(Box::new(move |event: JsValue| {
            if let Ok(payload) = js_sys::Reflect::get(&event, &"payload".into())
                && let Some(stage) = payload.as_string()
            {
                set_status.set(stage);
            }
        }) as Box<dyn Fn(JsValue)>);

        let promise = listen.call2(
            &JsValue::NULL,
            &"import-progress".into(),
            callback.as_ref().unchecked_ref(),
        );
        callback.forget();

        if let Ok(promise) = promise {
            wasm_bindgen_futures::JsFuture::from(js_sys::Promise::from(promise))
                .await
                .ok()
                .and_then(|v| v.dyn_into::<js_sys::Function>().ok())
        } else {
            None
        }
    } else {
        None
    };

    move || {
        if let Some(unlisten) = unlisten_fn {
            let _ = unlisten.call0(&JsValue::NULL);
        }
    }
}

const MAX_TRACK_NODES: usize = 50_000;
const BAIL_NODE_THRESHOLD: usize = 100_000;
