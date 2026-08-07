//! Tauri command invocation bridge for WASM frontend.

use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{JsFuture, spawn_local};

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_name = __turnout_report_error)]
    fn report_error_js(context: &str, detail: &str);
}

/// Forward a non-fatal command/IPC failure to Sentry as an error-level message.
/// A no-op when the Sentry plugin global is absent (plain browser / dev).
pub fn report_error(context: &str, detail: &str) {
    report_error_js(context, detail);
}

pub async fn invoke(cmd: &str, args: &JsValue) -> Result<JsValue, String> {
    let invoke_fn = tauri_namespace_fn("core", "invoke").ok_or("Tauri not available")?;
    let promise = invoke_fn.call2(&JsValue::NULL, &cmd.into(), args)
        .map_err(|e| format!("{e:?}"))?;
    await_promise(promise).await
}

/// Invoke a command whose arguments are an args object, taking ownership so the
/// converted [`JsValue`] outlives the await without a borrow dance at call sites.
async fn invoke_obj(cmd: &str, args: js_sys::Object) -> Result<JsValue, String> {
    invoke(cmd, &args.into()).await
}

/// Set a property on an args object. Setting a property on the fresh objects
/// built here cannot fail (they are never frozen), so the result is ignored —
/// centralizing the one unavoidable ignore instead of scattering `let _ =`.
fn set(obj: &js_sys::Object, key: &str, val: impl Into<JsValue>) {
    let _ = js_sys::Reflect::set(obj, &JsValue::from_str(key), &val.into());
}

/// Deserialize a Tauri result (a JS value) into a Rust type, or `None` on shape
/// mismatch. Symmetric with the backend's `serde` serialization.
fn from_js<T: serde::de::DeserializeOwned>(val: JsValue) -> Option<T> {
    serde_wasm_bindgen::from_value(val).ok()
}

/// Resolve `__TAURI__.<namespace>.<method>` as a callable JS function.
fn tauri_namespace_fn(namespace: &str, method: &str) -> Option<js_sys::Function> {
    let tauri = js_sys::Reflect::get(&js_sys::global(), &"__TAURI__".into()).ok()?;
    let ns = js_sys::Reflect::get(&tauri, &namespace.into()).ok()?;
    js_sys::Reflect::get(&ns, &method.into()).ok()?.dyn_into().ok()
}

/// Await a JS `Promise` returned by a Tauri call.
async fn await_promise(promise: JsValue) -> Result<JsValue, String> {
    JsFuture::from(js_sys::Promise::from(promise)).await.map_err(|e| format!("{e:?}"))
}

/// Register `callback` for each named Tauri event. The callback receives the
/// event's `payload` (or `JsValue::UNDEFINED` when absent). The listener lives
/// for the app's lifetime — the closure is leaked, matching the prior `.forget()`.
pub fn listen_to_events(events: &[&str], callback: impl Fn(JsValue) + 'static) {
    let events: Vec<String> = events.iter().map(|s| (*s).to_string()).collect();
    spawn_local(async move {
        let Some(listen_fn) = tauri_namespace_fn("event", "listen") else { return };
        let closure = Closure::wrap(Box::new(move |event: JsValue| {
            let payload = js_sys::Reflect::get(&event, &"payload".into()).unwrap_or(JsValue::UNDEFINED);
            callback(payload);
        }) as Box<dyn Fn(JsValue)>);
        for event in &events {
            let _ = listen_fn.call2(&JsValue::NULL, &JsValue::from_str(event), closure.as_ref().unchecked_ref());
        }
        closure.forget();
    });
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
    set(&args, "query", query);
    invoke_obj("fetch_overpass", args).await?
        .as_string()
        .ok_or("unexpected response".into())
}

pub async fn import_orm(
    json: &str, name: &str, railway_types: &[String],
    apply_speed_limits: bool, clip_bbox: Option<(f64, f64, f64, f64)>,
    tangent_mode: bool, type_speed_overrides: &std::collections::HashMap<String, u32>,
) -> Result<(Vec<u8>, usize), String> {
    let args = js_sys::Object::new();
    set(&args, "json", json);
    set(&args, "name", name);
    set(&args, "railwayTypes", build_railway_types_array(railway_types));
    set(&args, "applySpeedLimits", apply_speed_limits);
    set(&args, "clipBbox", build_bbox_args(clip_bbox));
    set(&args, "tangentMode", tangent_mode);
    let overrides_obj = js_sys::Object::new();
    for (k, v) in type_speed_overrides {
        set(&overrides_obj, k, f64::from(*v));
    }
    set(&args, "typeSpeedOverrides", overrides_obj);
    let result = invoke_obj("import_orm", args).await?;
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
    set(&args, "json", json);
    set(&args, "railwayTypes", build_railway_types_array(railway_types));
    set(&args, "clipBbox", build_bbox_args(clip_bbox));
    set(&args, "tangentMode", tangent_mode);
    let result = invoke_obj("count_track_nodes", args).await?;
    Ok(result.as_f64().unwrap_or(0.0) as usize)
}

pub async fn save_blueprint(name: &str, data: &[u8]) -> Result<String, String> {
    let args = js_sys::Object::new();
    set(&args, "name", name);
    set(&args, "data", js_sys::Uint8Array::from(data));
    invoke_obj("save_blueprint", args).await?
        .as_string()
        .ok_or("unexpected response".into())
}

pub async fn get_settings() -> Result<JsValue, String> {
    invoke("get_settings", &JsValue::NULL).await
}

pub async fn list_gpu_adapters() -> Result<JsValue, String> {
    invoke("list_gpu_adapters", &JsValue::NULL).await
}

pub async fn set_settings(settings: &JsValue) -> Result<(), String> {
    let wrapper = js_sys::Object::new();
    set(&wrapper, "settings", settings.clone());
    invoke_obj("set_settings", wrapper).await.map(|_| ())
}

pub async fn pick_folder() -> Option<String> {
    let result = invoke("pick_folder", &JsValue::NULL).await.ok()?;
    result.as_string()
}

pub async fn open_external_url(url: &str) -> Result<(), String> {
    let wrapper = js_sys::Object::new();
    set(&wrapper, "url", url);
    invoke_obj("open_external_url", wrapper).await.map(|_| ())
}

pub async fn replay_tutorial() -> Result<(), String> {
    invoke("replay_tutorial", &JsValue::NULL).await.map(|_| ())
}

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NimbyLaunchSetup {
    pub os: String,
    #[serde(default)]
    pub launch_options: Option<String>,
    #[serde(default)]
    pub nimby_detected: bool,
}

/// The Steam Launch Options string (and NIMBY-detection result) for auto-launching
/// Turnout with NIMBY Rails. `None` outside Tauri (e.g. the browser dev server).
pub async fn nimby_launch_setup() -> Option<NimbyLaunchSetup> {
    let result = invoke("nimby_launch_setup", &JsValue::NULL).await.ok()?;
    from_js(result)
}

pub async fn blueprint_exists(name: &str) -> bool {
    let args = js_sys::Object::new();
    set(&args, "name", name);
    invoke_obj("blueprint_exists", args).await
        .ok()
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

pub async fn get_app_version() -> String {
    let Some(get_version) = tauri_namespace_fn("app", "getVersion") else { return String::new() };
    let Ok(promise) = get_version.call0(&JsValue::NULL) else { return String::new() };
    await_promise(promise).await.ok().and_then(|v| v.as_string()).unwrap_or_default()
}

pub async fn check_for_update() -> Result<Option<String>, String> {
    let check_fn = tauri_namespace_fn("updater", "check").ok_or("Updater not available")?;
    let promise = check_fn.call0(&JsValue::NULL).map_err(|e| format!("{e:?}"))?;
    let result = await_promise(promise).await?;
    if result.is_null() || result.is_undefined() {
        return Ok(None);
    }
    Ok(js_sys::Reflect::get(&result, &"version".into()).ok().and_then(|v| v.as_string()))
}

pub async fn download_and_install_update() -> Result<(), String> {
    let check_fn = tauri_namespace_fn("updater", "check").ok_or("Updater not available")?;
    let promise = check_fn.call0(&JsValue::NULL).map_err(|e| format!("{e:?}"))?;
    let update = await_promise(promise).await?;
    if update.is_null() || update.is_undefined() {
        return Err("No update available".into());
    }
    let install_fn: js_sys::Function = js_sys::Reflect::get(&update, &"downloadAndInstall".into())
        .map_err(|_| "downloadAndInstall not available".to_string())?
        .dyn_into()
        .map_err(|_| "downloadAndInstall is not a function".to_string())?;
    let promise = install_fn.call0(&update).map_err(|e| format!("{e:?}"))?;
    await_promise(promise).await.map(|_| ())
}

pub async fn get_mods_dir() -> Option<String> {
    let result = invoke("get_mods_dir", &JsValue::NULL).await.ok()?;
    result.as_string()
}

/// Resize the current webview window to `(width, height)` logical pixels via the
/// Tauri window API. A no-op if any part of the API is unavailable.
pub fn set_window_logical_size(width: f64, height: f64) {
    let Some(get_current) = tauri_namespace_fn("window", "getCurrentWindow") else { return };
    let Ok(win) = get_current.call0(&JsValue::NULL) else { return };
    let Some(set_size) = js_sys::Reflect::get(&win, &"setSize".into())
        .ok()
        .and_then(|v| v.dyn_into::<js_sys::Function>().ok())
    else {
        return;
    };
    let Some(logical_size) = tauri_namespace_fn("window", "LogicalSize") else { return };
    let Ok(size) = js_sys::Reflect::construct(
        &logical_size,
        &js_sys::Array::of2(&JsValue::from_f64(width), &JsValue::from_f64(height)),
    ) else {
        return;
    };
    let _ = set_size.call1(&win, &size);
}

#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlueprintInfo {
    pub folder_name: String,
    #[serde(default)]
    pub clip_index: usize,
    #[serde(default)]
    pub clip_name: String,
    pub track_count: usize,
    pub file_size: u64,
    pub modified: u64,
    #[serde(default)]
    pub center_lat: f64,
    #[serde(default)]
    pub center_lon: f64,
    #[serde(default)]
    pub has_thumbnail: bool,
}

pub async fn list_blueprints() -> Result<Vec<BlueprintInfo>, String> {
    let result = invoke("list_blueprints", &JsValue::NULL).await?;
    from_js(result).ok_or_else(|| "unexpected response".into())
}

pub async fn generate_thumbnail(folder_name: &str, clip_index: usize) -> Result<String, String> {
    let args = js_sys::Object::new();
    set(&args, "folderName", folder_name);
    set(&args, "clipIndex", clip_index as f64);
    invoke_obj("generate_thumbnail", args).await?
        .as_string()
        .ok_or("unexpected response".into())
}

pub async fn delete_blueprint(folder_name: &str) -> Result<(), String> {
    let args = js_sys::Object::new();
    set(&args, "folderName", folder_name);
    invoke_obj("delete_blueprint", args).await.map(|_| ())
}

pub async fn rename_blueprint(old_name: &str, new_name: &str) -> Result<(), String> {
    let args = js_sys::Object::new();
    set(&args, "oldName", old_name);
    set(&args, "newName", new_name);
    invoke_obj("rename_blueprint", args).await.map(|_| ())
}

pub async fn open_blueprint_folder(folder_name: &str) -> Result<(), String> {
    let args = js_sys::Object::new();
    set(&args, "folderName", folder_name);
    invoke_obj("open_blueprint_folder", args).await.map(|_| ())
}

fn default_true() -> bool {
    true
}

fn default_opacity() -> f32 {
    1.0
}

fn default_kind() -> String {
    "kmz".to_string()
}

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LayerInfo {
    pub id: u32,
    pub name: String,
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default = "default_true")]
    pub visible: bool,
    #[serde(default = "default_opacity")]
    pub opacity: f32,
    pub bbox: [f64; 4],
    #[serde(default)]
    pub has_errors: bool,
    #[serde(default)]
    pub source_url: Option<String>,
}

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupInfo {
    pub id: u32,
    pub name: String,
    pub tile_url: String,
    #[serde(default)]
    pub tilejson_url: String,
    #[serde(default)]
    pub layers: Vec<LayerInfo>,
}

#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverlayStatus {
    #[serde(default)]
    pub groups: Vec<GroupInfo>,
}

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WmsLayerInfo {
    pub name: String,
    pub title: String,
}

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArcGisServiceInfo {
    pub name: String,
    #[serde(rename = "type")]
    pub service_type: String,
}

/// The empty overlay status returned when an infallible overlay command fails.
fn overlay_failed() -> OverlayStatus {
    web_sys::console::warn_1(&"overlay command failed".into());
    report_error("overlay command", "returned no status");
    OverlayStatus { groups: Vec::new() }
}

/// Invoke an overlay command whose signature is infallible: parse the returned
/// status, or warn and return an empty one.
async fn overlay_command(cmd: &str, args: js_sys::Object) -> OverlayStatus {
    invoke_obj(cmd, args).await.ok().and_then(from_js::<OverlayStatus>).unwrap_or_else(overlay_failed)
}

/// Invoke an overlay command that propagates failure: parse the returned status,
/// or surface a `Result` error.
async fn overlay_command_result(cmd: &str, args: js_sys::Object) -> Result<OverlayStatus, String> {
    let result = invoke_obj(cmd, args).await?;
    from_js(result).ok_or_else(|| "unexpected response".into())
}

pub async fn update_apple_urls(access_key: &str, map_version: Option<&str>, sat_version: Option<&str>) -> OverlayStatus {
    let args = js_sys::Object::new();
    set(&args, "accessKey", access_key);
    if let Some(v) = map_version { set(&args, "mapVersion", v); }
    if let Some(v) = sat_version { set(&args, "satVersion", v); }
    overlay_command("update_apple_urls", args).await
}

pub async fn refresh_apple_token() -> Result<(), String> {
    invoke("refresh_apple_token", &JsValue::NULL).await.map(|_| ())
}

pub async fn pick_kmz_file() -> Option<String> {
    let result = invoke("pick_kmz_file", &JsValue::NULL).await.ok()?;
    result.as_string()
}

pub async fn get_overlay_status() -> OverlayStatus {
    overlay_command("get_overlay_status", js_sys::Object::new()).await
}

pub async fn create_group(name: &str) -> Result<OverlayStatus, String> {
    let args = js_sys::Object::new();
    set(&args, "name", name);
    overlay_command_result("create_group", args).await
}

pub async fn remove_group(group_id: u32) -> OverlayStatus {
    let args = js_sys::Object::new();
    set(&args, "groupId", f64::from(group_id));
    overlay_command("remove_group", args).await
}

pub async fn reorder_group(group_id: u32, direction: &str) -> OverlayStatus {
    let args = js_sys::Object::new();
    set(&args, "groupId", f64::from(group_id));
    set(&args, "direction", direction);
    overlay_command("reorder_group", args).await
}

pub async fn rename_group(group_id: u32, name: &str) -> OverlayStatus {
    let args = js_sys::Object::new();
    set(&args, "groupId", f64::from(group_id));
    set(&args, "name", name);
    overlay_command("rename_group", args).await
}

pub async fn add_overlay(path: &str, group_id: Option<u32>) -> Result<OverlayStatus, String> {
    let args = js_sys::Object::new();
    set(&args, "path", path);
    if let Some(gid) = group_id {
        set(&args, "groupId", f64::from(gid));
    }
    overlay_command_result("add_overlay", args).await
}

pub async fn fetch_wms_layers(url: &str) -> Result<Vec<WmsLayerInfo>, String> {
    let args = js_sys::Object::new();
    set(&args, "url", url);
    let result = invoke_obj("fetch_wms_layers", args).await?;
    from_js(result).ok_or_else(|| "unexpected response".into())
}

pub async fn add_wms_layer(url: &str, layer_name: &str, display_name: &str, group_id: Option<u32>) -> Result<OverlayStatus, String> {
    let args = js_sys::Object::new();
    set(&args, "url", url);
    set(&args, "layerName", layer_name);
    set(&args, "displayName", display_name);
    if let Some(gid) = group_id {
        set(&args, "groupId", f64::from(gid));
    }
    overlay_command_result("add_wms_layer", args).await
}

pub async fn fetch_arcgis_services(url: &str) -> Result<Vec<ArcGisServiceInfo>, String> {
    let args = js_sys::Object::new();
    set(&args, "url", url);
    let result = invoke_obj("fetch_arcgis_services", args).await?;
    from_js(result).ok_or_else(|| "unexpected response".into())
}

pub async fn add_arcgis_layer(url: &str, service_name: &str, display_name: &str, group_id: Option<u32>) -> Result<OverlayStatus, String> {
    let args = js_sys::Object::new();
    set(&args, "url", url);
    set(&args, "serviceName", service_name);
    set(&args, "displayName", display_name);
    if let Some(gid) = group_id {
        set(&args, "groupId", f64::from(gid));
    }
    overlay_command_result("add_arcgis_layer", args).await
}

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WmtsLayerInfo {
    pub identifier: String,
    pub title: String,
    pub tile_url: String,
}

pub async fn fetch_wmts_layers(url: &str) -> Result<Vec<WmtsLayerInfo>, String> {
    let args = js_sys::Object::new();
    set(&args, "url", url);
    let result = invoke_obj("fetch_wmts_layers", args).await?;
    from_js(result).ok_or_else(|| "unexpected response".into())
}

pub async fn add_xyz_layer(url_template: &str, display_name: &str, group_id: Option<u32>) -> Result<OverlayStatus, String> {
    add_xyz_layer_with_kind(url_template, display_name, group_id, None).await
}

pub async fn add_xyz_layer_with_kind(url_template: &str, display_name: &str, group_id: Option<u32>, kind: Option<&str>) -> Result<OverlayStatus, String> {
    let args = js_sys::Object::new();
    set(&args, "urlTemplate", url_template);
    set(&args, "displayName", display_name);
    if let Some(gid) = group_id {
        set(&args, "groupId", f64::from(gid));
    }
    if let Some(k) = kind {
        set(&args, "kind", k);
    }
    overlay_command_result("add_xyz_layer", args).await
}

pub async fn move_layer(layer_id: u32, from_group_id: u32, to_group_id: u32) -> Result<OverlayStatus, String> {
    let args = js_sys::Object::new();
    set(&args, "layerId", f64::from(layer_id));
    set(&args, "fromGroupId", f64::from(from_group_id));
    set(&args, "toGroupId", f64::from(to_group_id));
    overlay_command_result("move_layer", args).await
}

pub async fn rename_layer(group_id: u32, layer_id: u32, name: &str) -> OverlayStatus {
    let args = js_sys::Object::new();
    set(&args, "groupId", f64::from(group_id));
    set(&args, "layerId", f64::from(layer_id));
    set(&args, "name", name);
    overlay_command("rename_layer", args).await
}

pub async fn set_group_visible(group_id: u32, visible: bool) -> OverlayStatus {
    let args = js_sys::Object::new();
    set(&args, "groupId", f64::from(group_id));
    set(&args, "visible", visible);
    overlay_command("set_group_visible", args).await
}

pub async fn reorder_layer(group_id: u32, layer_id: u32, direction: &str) -> OverlayStatus {
    let args = js_sys::Object::new();
    set(&args, "groupId", f64::from(group_id));
    set(&args, "layerId", f64::from(layer_id));
    set(&args, "direction", direction);
    overlay_command("reorder_layer", args).await
}

pub async fn remove_overlay(group_id: u32, layer_id: u32) -> OverlayStatus {
    let args = js_sys::Object::new();
    set(&args, "groupId", f64::from(group_id));
    set(&args, "layerId", f64::from(layer_id));
    overlay_command("remove_overlay", args).await
}

pub async fn set_layer_visible(group_id: u32, layer_id: u32, visible: bool) -> OverlayStatus {
    let args = js_sys::Object::new();
    set(&args, "groupId", f64::from(group_id));
    set(&args, "layerId", f64::from(layer_id));
    set(&args, "visible", visible);
    overlay_command("set_layer_visible", args).await
}

pub async fn set_layer_opacity(group_id: u32, layer_id: u32, opacity: f32) -> OverlayStatus {
    let args = js_sys::Object::new();
    set(&args, "groupId", f64::from(group_id));
    set(&args, "layerId", f64::from(layer_id));
    set(&args, "opacity", f64::from(opacity));
    overlay_command("set_layer_opacity", args).await
}

// --- Tile Download ---

pub async fn count_tiles(south: f64, west: f64, north: f64, east: f64, z_min: u8, z_max: u8) -> Result<u64, String> {
    let args = js_sys::Object::new();
    set(&args, "south", south);
    set(&args, "west", west);
    set(&args, "north", north);
    set(&args, "east", east);
    set(&args, "zMin", f64::from(z_min));
    set(&args, "zMax", f64::from(z_max));
    let result = invoke_obj("count_tiles", args).await?;
    result.as_f64().map(|v| v as u64).ok_or_else(|| "unexpected response".into())
}

pub async fn start_tile_download(
    url: &str, name: &str,
    south: f64, west: f64, north: f64, east: f64,
    z_min: u8, z_max: u8,
) -> Result<String, String> {
    let args = js_sys::Object::new();
    set(&args, "url", url);
    set(&args, "name", name);
    set(&args, "south", south);
    set(&args, "west", west);
    set(&args, "north", north);
    set(&args, "east", east);
    set(&args, "zMin", f64::from(z_min));
    set(&args, "zMax", f64::from(z_max));
    let result = invoke_obj("start_tile_download", args).await?;
    result.as_string().ok_or_else(|| "unexpected response".into())
}

pub async fn add_mbtiles_layer(path: &str, name: &str, group_id: Option<u32>) -> Result<OverlayStatus, String> {
    let args = js_sys::Object::new();
    set(&args, "path", path);
    set(&args, "displayName", name);
    if let Some(gid) = group_id {
        set(&args, "groupId", f64::from(gid));
    }
    overlay_command_result("add_mbtiles_layer", args).await
}

pub async fn cancel_tile_download() -> Result<(), String> {
    invoke("cancel_tile_download", &JsValue::NULL).await?;
    Ok(())
}

pub async fn set_tile_download_paused(paused: bool) -> Result<(), String> {
    let args = js_sys::Object::new();
    set(&args, "paused", paused);
    invoke_obj("set_tile_download_paused", args).await.map(|_| ())
}

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VectorLayerLevel {
    pub level: i32,
    pub layer_name: String,
    pub description: String,
}

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VectorLayersInfo {
    pub tilejson_url: String,
    pub tile_url: String,
    pub levels: Vec<VectorLayerLevel>,
}

pub async fn start_orm_vector_layers(
    south: f64, west: f64, north: f64, east: f64, timeout_secs: u32,
) -> Result<VectorLayersInfo, String> {
    let args = js_sys::Object::new();
    set(&args, "south", south);
    set(&args, "west", west);
    set(&args, "north", north);
    set(&args, "east", east);
    set(&args, "timeoutSecs", f64::from(timeout_secs));
    let result = invoke_obj("start_orm_vector_layers", args).await?;
    from_js(result).ok_or_else(|| "Unexpected response from the tile server".to_string())
}

pub async fn stop_orm_vector_layers() {
    let _ = invoke("stop_orm_vector_layers", &JsValue::NULL).await;
}

pub async fn open_workshop_mod() -> Result<(), String> {
    invoke("open_workshop_mod", &JsValue::NULL).await.map(|_| ())
}

pub async fn download_orm_tiles(
    south: f64, west: f64, north: f64, east: f64,
    z_min: u8, z_max: u8,
) -> Result<String, String> {
    let args = js_sys::Object::new();
    set(&args, "south", south);
    set(&args, "west", west);
    set(&args, "north", north);
    set(&args, "east", east);
    set(&args, "zMin", f64::from(z_min));
    set(&args, "zMax", f64::from(z_max));
    let result = invoke_obj("download_orm_tiles", args).await?;
    result.as_string().ok_or_else(|| "unexpected response".into())
}

pub async fn get_orm_port() -> Result<u16, String> {
    let result = invoke("get_orm_port", &JsValue::NULL).await?;
    result.as_f64().map(|v| v as u16).ok_or_else(|| "unexpected response".into())
}

pub async fn set_orm_offline(dir: Option<&str>) -> Result<(), String> {
    let args = js_sys::Object::new();
    match dir {
        Some(d) => set(&args, "dir", d),
        None => set(&args, "dir", JsValue::NULL),
    }
    invoke_obj("set_orm_offline", args).await.map(|_| ())
}

#[derive(Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CartoCity {
    pub slug: String,
    pub name: String,
    pub tile_url: String,
    pub tilejson_url: String,
    pub center: [f64; 2],
    pub min_zoom: u32,
    pub max_zoom: u32,
}

#[derive(Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CartoMetroInfo {
    pub base_url: String,
    pub cities: Vec<CartoCity>,
}

pub async fn start_cartometro() -> Result<CartoMetroInfo, String> {
    let result = invoke("start_cartometro", &JsValue::NULL).await?;
    serde_wasm_bindgen::from_value(result).map_err(|e| e.to_string())
}

pub async fn stop_cartometro() {
    let _ = invoke("stop_cartometro", &JsValue::NULL).await;
}
