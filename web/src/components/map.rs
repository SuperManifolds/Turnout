use leptos::*;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

const ORM_TILES: &str = "https://tiles.openrailwaymap.org/standard/{z}/{x}/{y}.png";
const OVERPASS_URL: &str = "https://overpass-api.de/api/interpreter";
const BBOX_COLOR: &str = "#4a9eff";
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
    fn map_fly_to(lng: f64, lat: f64, zoom: f64);
    fn map_add_preview_layer();
    fn map_on_mousedown(callback: &Closure<dyn Fn(f64, f64)>);
    fn map_on_mousemove(callback: &Closure<dyn Fn(f64, f64)>);
    fn map_on_mouseup(callback: &Closure<dyn Fn(f64, f64)>);
    fn map_query_features(lng: f64, lat: f64, layer_id: &str) -> JsValue;
}

// Interaction modes
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Idle,
    Drawing,
    Resizing(Handle),
}

#[derive(Clone, Copy, PartialEq)]
enum Handle {
    NW, N, NE, E, SE, S, SW, W,
}

impl Handle {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "nw" => Some(Self::NW), "n" => Some(Self::N), "ne" => Some(Self::NE),
            "e" => Some(Self::E), "se" => Some(Self::SE), "s" => Some(Self::S),
            "sw" => Some(Self::SW), "w" => Some(Self::W), _ => None,
        }
    }

    fn cursor(self) -> &'static str {
        match self {
            Self::NW | Self::SE => "nwse-resize",
            Self::NE | Self::SW => "nesw-resize",
            Self::N | Self::S => "ns-resize",
            Self::E | Self::W => "ew-resize",
        }
    }
}

fn bbox_geojson(w: f64, s: f64, e: f64, n: f64) -> String {
    format!(
        r#"{{"type":"Feature","geometry":{{"type":"Polygon","coordinates":[[[{w},{s}],[{e},{s}],[{e},{n}],[{w},{n}],[{w},{s}]]]}}}}"#
    )
}

fn handles_geojson(w: f64, s: f64, e: f64, n: f64) -> String {
    let mx = (w + e) / 2.0;
    let my = (s + n) / 2.0;
    let pts = [
        (w, n, "nw"), (mx, n, "n"), (e, n, "ne"), (e, my, "e"),
        (e, s, "se"), (mx, s, "s"), (w, s, "sw"), (w, my, "w"),
    ];
    let features: Vec<String> = pts.iter().map(|(lng, lat, h)| {
        format!(
            r#"{{"type":"Feature","properties":{{"handle":"{h}"}},"geometry":{{"type":"Point","coordinates":[{lng},{lat}]}}}}"#
        )
    }).collect();
    format!(r#"{{"type":"FeatureCollection","features":[{}]}}"#, features.join(","))
}

fn empty_geojson() -> &'static str {
    r#"{"type":"FeatureCollection","features":[]}"#
}

fn update_bbox_display(s: f64, w: f64, n: f64, e: f64) {
    map_set_geojson("bbox", &bbox_geojson(w, s, e, n));
    map_set_geojson("handles", &handles_geojson(w, s, e, n));
}

fn clear_bbox_display() {
    map_set_geojson("bbox", empty_geojson());
    map_set_geojson("handles", empty_geojson());
}

fn apply_handle_drag(handle: Handle, bbox: (f64, f64, f64, f64), lng: f64, lat: f64) -> (f64, f64, f64, f64) {
    let (mut s, mut w, mut n, mut e) = bbox;
    match handle {
        Handle::NW => { n = lat; w = lng; }
        Handle::N  => { n = lat; }
        Handle::NE => { n = lat; e = lng; }
        Handle::E  => { e = lng; }
        Handle::SE => { s = lat; e = lng; }
        Handle::S  => { s = lat; }
        Handle::SW => { s = lat; w = lng; }
        Handle::W  => { w = lng; }
    }
    // Ensure min < max
    if s > n { std::mem::swap(&mut s, &mut n); }
    if w > e { std::mem::swap(&mut w, &mut e); }
    (s, w, n, e)
}

fn format_bbox(s: f64, w: f64, n: f64, e: f64) -> String {
    format!("Selected: {s:.4}°, {w:.4}° to {n:.4}°, {e:.4}°")
}

#[component]
pub fn Map(
    set_available_types: WriteSignal<Vec<String>>,
    enabled_types: ReadSignal<Vec<String>>,
    set_has_selection: WriteSignal<bool>,
) -> impl IntoView {
    let map_ref = create_node_ref::<html::Div>();
    let (bbox, set_bbox) = create_signal::<Option<(f64, f64, f64, f64)>>(None);
    let (status, set_status) = create_signal(String::from("Navigate to an area, then click Select Area"));

    let mode = store_value(Mode::Idle);
    let draw_start = store_value::<Option<(f64, f64)>>(None);

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
        let window = web_sys::window().unwrap();
        let _ = window.add_event_listener_with_callback("paste", closure.as_ref().unchecked_ref());
        closure.forget();
    });

    create_effect(move |_| {
        let Some(div) = map_ref.get() else { return };
        let element: &web_sys::HtmlElement = &div;
        map_init(element);

        let on_load = Closure::new(move || {
            map_add_raster_source("orm", ORM_TILES, "\u{00a9} OpenRailwayMap");
            map_add_raster_layer("orm-layer", "orm", 0.7);
            map_add_preview_layer();
            map_add_geojson_source("bbox");
            map_add_fill_layer("bbox-fill", "bbox", BBOX_COLOR, 0.15);
            map_add_line_layer("bbox-outline", "bbox", BBOX_COLOR, 2.0);
            map_add_geojson_source("handles");
            map_add_circle_layer("handles-layer", "handles", HANDLE_COLOR, HANDLE_RADIUS);
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
                    if let Some(handle_str) = hit.as_string() {
                        if let Some(handle) = Handle::from_str(&handle_str) {
                            mode.set_value(Mode::Resizing(handle));
                            map_set_cursor(handle.cursor());
                            map_set_drag_pan(false);
                        }
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
                        if let Some(handle_str) = hit.as_string() {
                            if let Some(handle) = Handle::from_str(&handle_str) {
                                map_set_cursor(handle.cursor());
                                return;
                            }
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
        let (s, w, n, e) = current_bbox.unwrap();
        // Skip if bbox unchanged
        if cached_bbox.get_value() == current_bbox { return; }

        // Cancel previous debounce
        if let Some(h) = preview_timeout.get_value() {
            web_sys::window().unwrap().clear_timeout_with_handle(h);
        }
        let cb = Closure::once(move || {
            spawn_local(async move {
                set_status.set("Fetching tracks...".into());
                let query = format!(
                    "[out:json][timeout:30];(way[\"railway\"]({s},{w},{n},{e}););(._;>;);out body;"
                );
                match fetch_overpass(&query).await {
                    Ok(json) => {
                        let types = extract_railway_types(&json);
                        let count = count_ways(&json);
                        set_available_types.set(types);
                        set_has_selection.set(true);
                        let enabled = enabled_types.get_untracked();
                        let geojson = osm_json_to_geojson(&json, &enabled);
                        map_set_geojson("preview", &geojson);
                        cached_json.set_value(Some(json));
                        cached_bbox.set_value(Some((s, w, n, e)));
                        set_status.set(format!("{count} ways in selection"));
                    }
                    Err(err) => {
                        set_status.set(format!("Fetch failed: {err}"));
                    }
                }
            });
        });
        let h = web_sys::window().unwrap()
            .set_timeout_with_callback_and_timeout_and_arguments_0(
                cb.as_ref().unchecked_ref(), 500
            ).unwrap_or(0);
        cb.forget();
        preview_timeout.set_value(Some(h));
    });

    // Re-filter preview when enabled types change
    create_effect(move |_| {
        let enabled = enabled_types.get();
        if let Some(json) = cached_json.get_value() {
            let geojson = osm_json_to_geojson(&json, &enabled);
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
        set_status.set("Navigate to an area, then click Select Area".into());
    };

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
        spawn_local(async move {
            do_import(s, w, n, e, &name, cached.as_deref(), &types, set_status, set_success_message).await;
        });
    });

    let on_name_cancel = Callback::new(move |()| {
        set_show_name_prompt.set(false);
    });

    view! {
        <div id="map-wrapper">
            <div node_ref=map_ref id="map-canvas"></div>
            <nav id="map-controls">
                <span id="map-status">{status}</span>
                <Show when=move || bbox.get().is_none()>
                    <button on:click=on_select_area>"Select Area"</button>
                </Show>
                <Show when=move || bbox.get().is_some()>
                    <button class="primary" on:click=on_import_click>"Import Tracks"</button>
                    <button on:click=on_select_area>"Redraw"</button>
                    <button on:click=on_clear>"Clear"</button>
                </Show>
            </nav>
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

async fn do_import(s: f64, w: f64, n: f64, e: f64, name: &str, cached_json: Option<&str>, railway_types: &[String], set_status: WriteSignal<String>, set_success: WriteSignal<Option<String>>) {
    // Step 1: Use cached JSON or fetch
    let json = if let Some(cached) = cached_json {
        cached.to_string()
    } else {
        set_status.set("Fetching railway data...".into());
        let query = format!(
            "[out:json][timeout:60];(way[\"railway\"=\"rail\"]({s},{w},{n},{e}););(._;>;);out body;"
        );
        match fetch_overpass(&query).await {
            Ok(j) => j,
            Err(err) => {
                set_status.set(format!("Fetch failed: {err}"));
                return;
            }
        }
    };

    // Step 2: Import via Tauri backend
    set_status.set("Processing tracks...".into());
    let data = match tauri_import_orm(&json, name, railway_types).await {
        Ok(d) => d,
        Err(err) => {
            set_status.set(format!("Import failed: {err}"));
            return;
        }
    };

    // Step 3: Save via Tauri backend
    set_status.set("Saving blueprint...".into());
    match tauri_save_blueprint(name, &data).await {
        Ok(path) => {
            set_status.set("Ready".into());
            set_success.set(Some(path));
        }
        Err(err) => {
            set_status.set(format!("Save failed: {err}"));
        }
    }
}

async fn fetch_overpass(query: &str) -> Result<String, String> {
    let window = web_sys::window().ok_or("no window")?;
    let body = format!("data={}", urlencoding(query));
    let opts = web_sys::RequestInit::new();
    opts.set_method("POST");
    opts.set_body(&JsValue::from_str(&body));
    let headers = web_sys::Headers::new().map_err(|e| format!("{e:?}"))?;
    headers.set("Content-Type", "application/x-www-form-urlencoded").map_err(|e| format!("{e:?}"))?;
    opts.set_headers(&headers);
    let request = web_sys::Request::new_with_str_and_init(OVERPASS_URL, &opts)
        .map_err(|e| format!("{e:?}"))?;
    let resp = JsFuture::from(window.fetch_with_request(&request))
        .await
        .map_err(|e| format!("{e:?}"))?;
    let resp: web_sys::Response = resp.unchecked_into();
    if !resp.ok() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let text = JsFuture::from(resp.text().map_err(|e| format!("{e:?}"))?)
        .await
        .map_err(|e| format!("{e:?}"))?;
    text.as_string().ok_or("no text".into())
}

fn urlencoding(s: &str) -> String {
    s.chars().map(|c| match c {
        ' ' => "+".to_string(),
        c if c.is_alphanumeric() || "-_.~".contains(c) => c.to_string(),
        c => format!("%{:02X}", c as u32),
    }).collect()
}

async fn tauri_import_orm(json: &str, name: &str, railway_types: &[String]) -> Result<Vec<u8>, String> {
    let args = js_sys::Object::new();
    js_sys::Reflect::set(&args, &"json".into(), &json.into()).ok();
    js_sys::Reflect::set(&args, &"name".into(), &name.into()).ok();
    let js_types = js_sys::Array::new();
    for t in railway_types {
        js_types.push(&JsValue::from_str(t));
    }
    js_sys::Reflect::set(&args, &"railwayTypes".into(), &js_types.into()).ok();
    let result = tauri_invoke("import_orm", &args).await?;
    // Result is a JS array of u8
    let arr = js_sys::Uint8Array::new(&result);
    Ok(arr.to_vec())
}

async fn tauri_save_blueprint(name: &str, data: &[u8]) -> Result<String, String> {
    let args = js_sys::Object::new();
    js_sys::Reflect::set(&args, &"name".into(), &name.into()).ok();
    let js_data = js_sys::Uint8Array::from(data);
    js_sys::Reflect::set(&args, &"data".into(), &js_data.into()).ok();
    let result = tauri_invoke("save_blueprint", &args).await?;
    result.as_string().ok_or("unexpected response".into())
}

async fn tauri_invoke(cmd: &str, args: &JsValue) -> Result<JsValue, String> {
    let window = js_sys::Reflect::get(&js_sys::global(), &"__TAURI__".into())
        .map_err(|_| "Tauri not available")?;
    let core = js_sys::Reflect::get(&window, &"core".into())
        .map_err(|_| "Tauri core not available")?;
    let invoke = js_sys::Reflect::get(&core, &"invoke".into())
        .map_err(|_| "invoke not available")?;
    let invoke_fn: js_sys::Function = invoke.unchecked_into();
    let promise = invoke_fn.call2(&JsValue::NULL, &cmd.into(), args)
        .map_err(|e| format!("{e:?}"))?;
    JsFuture::from(js_sys::Promise::from(promise))
        .await
        .map_err(|e| format!("{e:?}"))
}

fn osm_json_to_geojson(json: &str, enabled_types: &[String]) -> String {
    let data: serde_json::Value = match serde_json::from_str(json) {
        Ok(d) => d,
        Err(_) => return empty_geojson().to_string(),
    };
    let elements = match data["elements"].as_array() {
        Some(e) => e,
        None => return empty_geojson().to_string(),
    };

    // Build node lookup
    let mut nodes: std::collections::HashMap<u64, (f64, f64)> = std::collections::HashMap::new();
    for e in elements {
        if e["type"].as_str() == Some("node") {
            if let (Some(id), Some(lat), Some(lon)) = (e["id"].as_u64(), e["lat"].as_f64(), e["lon"].as_f64()) {
                nodes.insert(id, (lon, lat));
            }
        }
    }

    // Build features from ways
    let mut features = Vec::new();
    for e in elements {
        if e["type"].as_str() != Some("way") { continue; }
        let Some(node_ids) = e["nodes"].as_array() else { continue };
        let coords: Vec<String> = node_ids.iter()
            .filter_map(|n| n.as_u64())
            .filter_map(|id| nodes.get(&id))
            .map(|(lon, lat)| format!("[{lon},{lat}]"))
            .collect();
        if coords.len() < 2 { continue; }

        // Extract railway-relevant tags
        let tags = &e["tags"];
        let railway = tags["railway"].as_str().unwrap_or("rail");
        if !enabled_types.iter().any(|t| t == railway) { continue; }
        let mut props = vec![format!(r#""railway":"{railway}""#)];
        for key in &["usage", "service", "name", "maxspeed", "electrified", "gauge", "layer", "bridge", "tunnel"] {
            if let Some(val) = tags[*key].as_str() {
                let escaped = val.replace('\\', "\\\\").replace('"', "\\\"");
                props.push(format!(r#""{key}":"{escaped}""#));
            }
        }

        features.push(format!(
            r#"{{"type":"Feature","properties":{{{}}},"geometry":{{"type":"LineString","coordinates":[{}]}}}}"#,
            props.join(","),
            coords.join(",")
        ));
    }

    format!(r#"{{"type":"FeatureCollection","features":[{}]}}"#, features.join(","))
}

fn extract_railway_types(json: &str) -> Vec<String> {
    let data: serde_json::Value = match serde_json::from_str(json) {
        Ok(d) => d,
        Err(_) => return vec![],
    };
    let mut types = std::collections::HashSet::new();
    if let Some(elements) = data["elements"].as_array() {
        for e in elements {
            if e["type"].as_str() == Some("way") {
                if let Some(rt) = e["tags"]["railway"].as_str() {
                    types.insert(rt.to_string());
                }
            }
        }
    }
    let mut sorted: Vec<String> = types.into_iter().collect();
    sorted.sort();
    sorted
}

fn count_ways(json: &str) -> usize {
    let data: serde_json::Value = match serde_json::from_str(json) {
        Ok(d) => d,
        Err(_) => return 0,
    };
    data["elements"].as_array()
        .map(|e| e.iter().filter(|el| el["type"].as_str() == Some("way")).count())
        .unwrap_or(0)
}

/// Parse an OpenRailwayMap link like:
/// `https://openrailwaymap.app/#view=9.49/34.1997/-117.2839`
/// Returns (zoom, lat, lng) if valid.
fn parse_orm_link(text: &str) -> Option<(f64, f64, f64)> {
    let text = text.trim();
    // Match #view=zoom/lat/lng pattern anywhere in the string
    let hash_pos = text.find("#view=")?;
    let fragment = &text[hash_pos + 6..];
    let parts: Vec<&str> = fragment.split('/').collect();
    if parts.len() >= 3 {
        let zoom = parts[0].parse::<f64>().ok()?;
        let lat = parts[1].parse::<f64>().ok()?;
        let lng = parts[2].parse::<f64>().ok()?;
        Some((zoom, lat, lng))
    } else {
        None
    }
}
