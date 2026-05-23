//! Tauri command invocation bridge for WASM frontend.

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

pub async fn invoke(cmd: &str, args: &JsValue) -> Result<JsValue, String> {
    let tauri = js_sys::Reflect::get(&js_sys::global(), &"__TAURI__".into())
        .map_err(|_| "Tauri not available")?;
    let core = js_sys::Reflect::get(&tauri, &"core".into())
        .map_err(|_| "Tauri core not available")?;
    let invoke_fn: js_sys::Function = js_sys::Reflect::get(&core, &"invoke".into())
        .map_err(|_| "invoke not available")?
        .unchecked_into();
    let promise = invoke_fn.call2(&JsValue::NULL, &cmd.into(), args)
        .map_err(|e| format!("{e:?}"))?;
    JsFuture::from(js_sys::Promise::from(promise))
        .await
        .map_err(|e| format!("{e:?}"))
}

pub fn js_set(obj: &js_sys::Object, key: &str, val: &JsValue) -> Result<(), String> {
    js_sys::Reflect::set(obj, &key.into(), val)
        .map(|_| ())
        .map_err(|e| format!("Failed to set {key}: {e:?}"))
}

fn build_bbox_args(clip_bbox: Option<(f64, f64, f64, f64)>) -> JsValue {
    match clip_bbox {
        Some((s, w, n, e)) => {
            let arr = js_sys::Array::new();
            arr.push(&JsValue::from_f64(s));
            arr.push(&JsValue::from_f64(w));
            arr.push(&JsValue::from_f64(n));
            arr.push(&JsValue::from_f64(e));
            arr.into()
        }
        None => JsValue::NULL,
    }
}

fn build_railway_types_array(railway_types: &[String]) -> js_sys::Array {
    let arr = js_sys::Array::new();
    for t in railway_types {
        arr.push(&JsValue::from_str(t));
    }
    arr
}

pub async fn fetch_overpass(query: &str) -> Result<String, String> {
    let args = js_sys::Object::new();
    js_set(&args, "query", &query.into())?;
    invoke("fetch_overpass", &args).await?
        .as_string()
        .ok_or("unexpected response".into())
}

pub async fn import_orm(
    json: &str, name: &str, railway_types: &[String],
    apply_speed_limits: bool, clip_bbox: Option<(f64, f64, f64, f64)>,
    tangent_mode: bool,
) -> Result<(Vec<u8>, usize), String> {
    let args = js_sys::Object::new();
    js_set(&args, "json", &json.into())?;
    js_set(&args, "name", &name.into())?;
    js_set(&args, "railwayTypes", &build_railway_types_array(railway_types).into())?;
    js_set(&args, "applySpeedLimits", &JsValue::from_bool(apply_speed_limits))?;
    js_set(&args, "clipBbox", &build_bbox_args(clip_bbox))?;
    js_set(&args, "tangentMode", &JsValue::from_bool(tangent_mode))?;
    let result = invoke("import_orm", &args).await?;
    let tuple = js_sys::Array::from(&result);
    let bytes = js_sys::Uint8Array::new(&tuple.get(0)).to_vec();
    let node_count = tuple.get(1).as_f64().unwrap_or(0.0) as usize;
    Ok((bytes, node_count))
}

pub async fn count_track_nodes(
    json: &str, railway_types: &[String], clip_bbox: Option<(f64, f64, f64, f64)>,
    tangent_mode: bool,
) -> Result<usize, String> {
    let args = js_sys::Object::new();
    js_set(&args, "json", &json.into())?;
    js_set(&args, "railwayTypes", &build_railway_types_array(railway_types).into())?;
    js_set(&args, "clipBbox", &build_bbox_args(clip_bbox))?;
    js_set(&args, "tangentMode", &JsValue::from_bool(tangent_mode))?;
    let result = invoke("count_track_nodes", &args).await?;
    Ok(result.as_f64().unwrap_or(0.0) as usize)
}

pub async fn save_blueprint(name: &str, data: &[u8]) -> Result<String, String> {
    let args = js_sys::Object::new();
    js_set(&args, "name", &name.into())?;
    let js_data = js_sys::Uint8Array::from(data);
    js_set(&args, "data", &js_data.into())?;
    let result = invoke("save_blueprint", &args).await?;
    result.as_string().ok_or("unexpected response".into())
}

pub async fn get_settings() -> Result<JsValue, String> {
    invoke("get_settings", &JsValue::NULL).await
}

pub async fn set_settings(settings: &JsValue) -> Result<(), String> {
    let wrapper = js_sys::Object::new();
    js_set(&wrapper, "settings", settings)?;
    invoke("set_settings", &wrapper).await.map(|_| ())
}

pub async fn pick_folder() -> Option<String> {
    let result = invoke("pick_folder", &JsValue::NULL).await.ok()?;
    result.as_string()
}

pub async fn get_mods_dir() -> Option<String> {
    let result = invoke("get_mods_dir", &JsValue::NULL).await.ok()?;
    result.as_string()
}
