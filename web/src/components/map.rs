use leptos::*;
use wasm_bindgen::prelude::*;

#[wasm_bindgen(inline_js = r#"
export function init_map(container) {
    if (!window.maplibregl) {
        console.error("MapLibre GL JS not loaded");
        return null;
    }
    const map = new maplibregl.Map({
        container: container,
        style: "https://basemaps.cartocdn.com/gl/positron-gl-style/style.json",
        center: [-4.254, 55.859],
        zoom: 13,
    });
    map.on("load", () => {
        map.addSource("orm-tiles", {
            type: "raster",
            tiles: ["https://tiles.openrailwaymap.org/standard/{z}/{x}/{y}.png"],
            tileSize: 256,
            attribution: "&copy; OpenRailwayMap",
        });
        map.addLayer({
            id: "orm-layer",
            type: "raster",
            source: "orm-tiles",
            paint: { "raster-opacity": 0.7 },
        });
    });
    return map;
}
"#)]
extern "C" {
    fn init_map(container: &web_sys::HtmlElement) -> JsValue;
}

#[component]
pub fn Map() -> impl IntoView {
    let map_ref = create_node_ref::<html::Div>();

    create_effect(move |_| {
        if let Some(div) = map_ref.get() {
            let element: &web_sys::HtmlElement = &div;
            let _map = init_map(element);
        }
    });

    view! {
        <div node_ref=map_ref style="width: 100%; height: 100%;"></div>
    }
}
