use leptos::*;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = maplibregl, js_name = Map)]
    type MapLibreMap;

    #[wasm_bindgen(constructor, js_namespace = maplibregl, js_name = Map)]
    fn new(options: &JsValue) -> MapLibreMap;
}

#[component]
pub fn Map() -> impl IntoView {
    let map_ref = create_node_ref::<html::Div>();

    create_effect(move |_| {
        if let Some(div) = map_ref.get() {
            let options = js_sys::Object::new();
            js_sys::Reflect::set(&options, &"container".into(), &div.into_any()).ok();
            js_sys::Reflect::set(
                &options,
                &"style".into(),
                &"https://basemaps.cartocdn.com/gl/positron-gl-style/style.json".into(),
            ).ok();
            js_sys::Reflect::set(&options, &"center".into(), &{
                let arr = js_sys::Array::new();
                arr.push(&JsValue::from_f64(-117.514));
                arr.push(&JsValue::from_f64(34.012));
                arr
            }.into()).ok();
            js_sys::Reflect::set(&options, &"zoom".into(), &JsValue::from_f64(13.0)).ok();

            let _map = MapLibreMap::new(&options);
        }
    });

    view! {
        <div node_ref=map_ref style="width: 100%; height: 80vh;"></div>
    }
}
