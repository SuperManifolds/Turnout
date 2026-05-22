use leptos::*;
use wasm_bindgen::prelude::*;

const ORM_TILES: &str = "https://tiles.openrailwaymap.org/standard/{z}/{x}/{y}.png";
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
pub fn Map() -> impl IntoView {
    let map_ref = create_node_ref::<html::Div>();
    let (bbox, set_bbox) = create_signal::<Option<(f64, f64, f64, f64)>>(None);
    let (status, set_status) = create_signal(String::from("Navigate to an area, then click Select Area"));

    let mode = store_value(Mode::Idle);
    let draw_start = store_value::<Option<(f64, f64)>>(None);

    create_effect(move |_| {
        let Some(div) = map_ref.get() else { return };
        let element: &web_sys::HtmlElement = &div;
        map_init(element);

        let on_load = Closure::new(move || {
            map_add_raster_source("orm", ORM_TILES, "\u{00a9} OpenRailwayMap");
            map_add_raster_layer("orm-layer", "orm", 0.7);
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
        set_bbox.set(None);
        mode.set_value(Mode::Idle);
        set_status.set("Navigate to an area, then click Select Area".into());
    };

    view! {
        <div id="map-wrapper">
            <div node_ref=map_ref id="map-canvas"></div>
            <nav id="map-controls">
                <span id="map-status">{status}</span>
                <Show when=move || bbox.get().is_none()>
                    <button on:click=on_select_area>"Select Area"</button>
                </Show>
                <Show when=move || bbox.get().is_some()>
                    <button class="primary" on:click=move |_| {
                        set_status.set("Importing...".into());
                    }>"Import Tracks"</button>
                    <button on:click=on_select_area>"Redraw"</button>
                    <button on:click=on_clear>"Clear"</button>
                </Show>
            </nav>
        </div>
    }
}
