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
    tangent_mode: bool, type_speed_overrides: &std::collections::HashMap<String, u32>,
) -> Result<(Vec<u8>, usize), String> {
    let args = js_sys::Object::new();
    js_set(&args, "json", &json.into())?;
    js_set(&args, "name", &name.into())?;
    js_set(&args, "railwayTypes", &build_railway_types_array(railway_types).into())?;
    js_set(&args, "applySpeedLimits", &JsValue::from_bool(apply_speed_limits))?;
    js_set(&args, "clipBbox", &build_bbox_args(clip_bbox))?;
    js_set(&args, "tangentMode", &JsValue::from_bool(tangent_mode))?;
    let overrides_obj = js_sys::Object::new();
    for (k, v) in type_speed_overrides {
        js_set(&overrides_obj, k, &JsValue::from_f64(f64::from(*v)))?;
    }
    js_set(&args, "typeSpeedOverrides", &overrides_obj.into())?;
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

pub async fn blueprint_exists(name: &str) -> bool {
    let args = js_sys::Object::new();
    let Ok(()) = js_set(&args, "name", &name.into()) else { return false };
    invoke("blueprint_exists", &args).await
        .ok()
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

pub async fn get_app_version() -> String {
    let Ok(tauri) = js_sys::Reflect::get(&js_sys::global(), &"__TAURI__".into()) else { return String::new() };
    let Ok(app) = js_sys::Reflect::get(&tauri, &"app".into()) else { return String::new() };
    let Ok(get_version) = js_sys::Reflect::get(&app, &"getVersion".into()) else { return String::new() };
    let Ok(get_version) = get_version.dyn_into::<js_sys::Function>() else { return String::new() };
    let Ok(promise) = get_version.call0(&JsValue::NULL) else { return String::new() };
    let Ok(result) = wasm_bindgen_futures::JsFuture::from(js_sys::Promise::from(promise)).await else { return String::new() };
    result.as_string().unwrap_or_default()
}

pub async fn check_for_update() -> Result<Option<String>, String> {
    let tauri = js_sys::Reflect::get(&js_sys::global(), &"__TAURI__".into())
        .map_err(|_| "Tauri not available")?;
    let updater = js_sys::Reflect::get(&tauri, &"updater".into())
        .map_err(|_| "Updater not available")?;
    let check_fn = js_sys::Reflect::get(&updater, &"check".into())
        .map_err(|_| "check not available")?
        .dyn_into::<js_sys::Function>()
        .map_err(|_| "check is not a function")?;
    let promise = check_fn.call0(&JsValue::NULL)
        .map_err(|e| format!("{e:?}"))?;
    let result = wasm_bindgen_futures::JsFuture::from(js_sys::Promise::from(promise))
        .await
        .map_err(|e| format!("{e:?}"))?;
    if result.is_null() || result.is_undefined() {
        return Ok(None);
    }
    let version = js_sys::Reflect::get(&result, &"version".into())
        .ok()
        .and_then(|v| v.as_string());
    Ok(version)
}

pub async fn get_mods_dir() -> Option<String> {
    let result = invoke("get_mods_dir", &JsValue::NULL).await.ok()?;
    result.as_string()
}

#[derive(Clone, Debug, Default)]
pub struct BlueprintInfo {
    pub folder_name: String,
    pub clip_index: usize,
    pub clip_name: String,
    pub track_count: usize,
    pub file_size: u64,
    pub modified: u64,
    pub center_lat: f64,
    pub center_lon: f64,
    pub has_thumbnail: bool,
}

impl BlueprintInfo {
    fn from_js(val: &JsValue) -> Option<Self> {
        let get_str = |k: &str| js_sys::Reflect::get(val, &k.into()).ok()?.as_string();
        let get_f64 = |k: &str| js_sys::Reflect::get(val, &k.into()).ok()?.as_f64();
        let get_bool = |k: &str| js_sys::Reflect::get(val, &k.into()).ok()?.as_bool();
        Some(BlueprintInfo {
            folder_name: get_str("folderName")?,
            clip_index: get_f64("clipIndex").unwrap_or(0.0) as usize,
            clip_name: get_str("clipName").unwrap_or_default(),
            track_count: get_f64("trackCount")? as usize,
            file_size: get_f64("fileSize")? as u64,
            modified: get_f64("modified")? as u64,
            center_lat: get_f64("centerLat").unwrap_or(0.0),
            center_lon: get_f64("centerLon").unwrap_or(0.0),
            has_thumbnail: get_bool("hasThumbnail").unwrap_or(false),
        })
    }
}

pub async fn list_blueprints() -> Result<Vec<BlueprintInfo>, String> {
    let result = invoke("list_blueprints", &JsValue::NULL).await?;
    let arr = js_sys::Array::from(&result);
    Ok(arr.iter().filter_map(|v| BlueprintInfo::from_js(&v)).collect())
}

pub async fn generate_thumbnail(folder_name: &str, clip_index: usize) -> Result<String, String> {
    let args = js_sys::Object::new();
    js_set(&args, "folderName", &folder_name.into())?;
    js_set(&args, "clipIndex", &JsValue::from_f64(clip_index as f64))?;
    invoke("generate_thumbnail", &args).await?
        .as_string()
        .ok_or("unexpected response".into())
}

pub async fn delete_blueprint(folder_name: &str) -> Result<(), String> {
    let args = js_sys::Object::new();
    js_set(&args, "folderName", &folder_name.into())?;
    invoke("delete_blueprint", &args).await.map(|_| ())
}

pub async fn rename_blueprint(old_name: &str, new_name: &str) -> Result<(), String> {
    let args = js_sys::Object::new();
    js_set(&args, "oldName", &old_name.into())?;
    js_set(&args, "newName", &new_name.into())?;
    invoke("rename_blueprint", &args).await.map(|_| ())
}

pub async fn open_blueprint_folder(folder_name: &str) -> Result<(), String> {
    let args = js_sys::Object::new();
    js_set(&args, "folderName", &folder_name.into())?;
    invoke("open_blueprint_folder", &args).await.map(|_| ())
}

#[derive(Clone, Debug)]
pub struct LayerInfo {
    pub id: u32,
    pub name: String,
    pub kind: String,
    pub visible: bool,
    pub opacity: f32,
    pub bbox: [f64; 4],
}

#[derive(Clone, Debug)]
pub struct WmsLayerInfo {
    pub name: String,
    pub title: String,
}

#[derive(Clone, Debug)]
pub struct OverlayStatus {
    pub tile_url: String,
    pub layers: Vec<LayerInfo>,
}

fn parse_bbox(val: &JsValue) -> Option<[f64; 4]> {
    let arr = js_sys::Array::from(val);
    Some([arr.get(0).as_f64()?, arr.get(1).as_f64()?, arr.get(2).as_f64()?, arr.get(3).as_f64()?])
}

fn parse_layer(val: &JsValue) -> Option<LayerInfo> {
    let get_str = |k: &str| js_sys::Reflect::get(val, &k.into()).ok()?.as_string();
    let get_f64 = |k: &str| js_sys::Reflect::get(val, &k.into()).ok()?.as_f64();
    let bbox_val = js_sys::Reflect::get(val, &"bbox".into()).ok()?;
    Some(LayerInfo {
        id: get_f64("id")? as u32,
        name: get_str("name")?,
        kind: get_str("kind").unwrap_or_else(|| "kmz".into()),
        visible: js_sys::Reflect::get(val, &"visible".into()).ok()?.as_bool().unwrap_or(true),
        opacity: get_f64("opacity").unwrap_or(1.0) as f32,
        bbox: parse_bbox(&bbox_val)?,
    })
}

fn parse_status(val: &JsValue) -> Option<OverlayStatus> {
    let tile_url = js_sys::Reflect::get(val, &"tileUrl".into()).ok()?.as_string()?;
    let layers_val = js_sys::Reflect::get(val, &"layers".into()).ok()?;
    let layers_arr = js_sys::Array::from(&layers_val);
    let layers = layers_arr.iter().filter_map(|v| parse_layer(&v)).collect();
    Some(OverlayStatus { tile_url, layers })
}

pub async fn pick_kmz_file() -> Option<String> {
    let result = invoke("pick_kmz_file", &JsValue::NULL).await.ok()?;
    result.as_string()
}

pub async fn add_overlay(path: &str) -> Result<OverlayStatus, String> {
    let args = js_sys::Object::new();
    js_set(&args, "path", &path.into())?;
    let result = invoke("add_overlay", &args).await?;
    parse_status(&result).ok_or_else(|| "unexpected response".into())
}

pub async fn remove_overlay(id: u32) -> Option<OverlayStatus> {
    let args = js_sys::Object::new();
    js_set(&args, "id", &JsValue::from_f64(f64::from(id))).ok()?;
    let result = invoke("remove_overlay", &args).await.ok()?;
    if result.is_null() || result.is_undefined() {
        return None;
    }
    parse_status(&result)
}

pub async fn get_overlay_status() -> Option<OverlayStatus> {
    let result = invoke("get_overlay_status", &JsValue::NULL).await.ok()?;
    if result.is_null() || result.is_undefined() {
        return None;
    }
    parse_status(&result)
}

pub async fn fetch_wms_layers(url: &str) -> Result<Vec<WmsLayerInfo>, String> {
    let args = js_sys::Object::new();
    js_set(&args, "url", &url.into())?;
    let result = invoke("fetch_wms_layers", &args).await?;
    let arr = js_sys::Array::from(&result);
    Ok(arr.iter().filter_map(|v| {
        let get_str = |k: &str| js_sys::Reflect::get(&v, &k.into()).ok()?.as_string();
        Some(WmsLayerInfo {
            name: get_str("name")?,
            title: get_str("title")?,
        })
    }).collect())
}

pub async fn add_wms_layer(url: &str, layer_name: &str, display_name: &str) -> Result<OverlayStatus, String> {
    let args = js_sys::Object::new();
    js_set(&args, "url", &url.into())?;
    js_set(&args, "layerName", &layer_name.into())?;
    js_set(&args, "displayName", &display_name.into())?;
    let result = invoke("add_wms_layer", &args).await?;
    parse_status(&result).ok_or_else(|| "unexpected response".into())
}

pub async fn set_layer_visible(id: u32, visible: bool) -> Option<OverlayStatus> {
    let args = js_sys::Object::new();
    js_set(&args, "id", &JsValue::from_f64(f64::from(id))).ok()?;
    js_set(&args, "visible", &JsValue::from_bool(visible)).ok()?;
    let result = invoke("set_layer_visible", &args).await.ok()?;
    if result.is_null() || result.is_undefined() { return None; }
    parse_status(&result)
}

pub async fn set_layer_opacity(id: u32, opacity: f32) -> Option<OverlayStatus> {
    let args = js_sys::Object::new();
    js_set(&args, "id", &JsValue::from_f64(f64::from(id))).ok()?;
    js_set(&args, "opacity", &JsValue::from_f64(f64::from(opacity))).ok()?;
    let result = invoke("set_layer_opacity", &args).await.ok()?;
    if result.is_null() || result.is_undefined() { return None; }
    parse_status(&result)
}
