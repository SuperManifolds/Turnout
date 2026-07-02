use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::Manager;
use tauri_plugin_dialog::DialogExt;

use crate::arcgis;
use crate::tile_server::{self, LayerSource};
use crate::wms;
use crate::wmts;

const STORE_KEY: &str = "overlay_groups";

pub struct OverlayState {
    groups: Mutex<Vec<TileGroup>>,
    next_group_id: Mutex<u32>,
}

struct TileGroup {
    id: u32,
    name: String,
    handle: tile_server::ServerHandle,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct SavedGroup {
    name: String,
    layers: Vec<SavedLayer>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct SavedLayer {
    kind: String,
    name: String,
    visible: bool,
    opacity: f32,
    path: Option<String>,
    wms_url: Option<String>,
    wms_layer: Option<String>,
    arcgis_url: Option<String>,
    arcgis_service: Option<String>,
    xyz_url: Option<String>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct LayerInfo {
    pub id: u32,
    pub name: String,
    pub kind: &'static str,
    pub visible: bool,
    pub opacity: f32,
    pub bbox: [f64; 4],
    pub has_errors: bool,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct GroupInfo {
    pub id: u32,
    pub name: String,
    pub tile_url: String,
    pub tilejson_url: String,
    pub layers: Vec<LayerInfo>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct OverlayStatus {
    pub groups: Vec<GroupInfo>,
}

impl OverlayState {
    pub fn new() -> Self {
        Self {
            groups: Mutex::new(Vec::new()),
            next_group_id: Mutex::new(0),
        }
    }

    fn next_group_id(&self) -> u32 {
        let mut id = self.next_group_id.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let val = *id;
        *id += 1;
        val
    }
}

fn port_for_id(id: u32) -> u16 {
    tile_server::PREFERRED_PORT.saturating_add(id as u16)
}

// --- Tauri commands ---

#[tauri::command]
pub async fn pick_kmz_file(app: tauri::AppHandle) -> Option<String> {
    let path = app
        .dialog()
        .file()
        .add_filter("Overlay files", &["kmz", "kml", "shp", "geojson", "json"])
        .blocking_pick_file()?;
    Some(path.to_string())
}

#[tauri::command]
pub async fn create_group(app: tauri::AppHandle, name: String) -> Result<OverlayStatus, String> {
    let state = app.state::<OverlayState>();
    let group_id = state.next_group_id();
    let port = port_for_id(group_id);

    let handle = tile_server::start(port)
        .await
        .map_err(|e| format!("Failed to start tile server: {e}"))?;

    let mut groups = state.groups.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    groups.push(TileGroup { id: group_id, name, handle });
    let status = build_status(&groups);
    drop(groups);
    save_groups(&app);
    Ok(status)
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn remove_group(app: tauri::AppHandle, group_id: u32) -> OverlayStatus {
    let state = app.state::<OverlayState>();
    let mut groups = state.groups.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(idx) = groups.iter().position(|g| g.id == group_id) {
        let group = groups.remove(idx);
        let _ = group.handle.shutdown_tx.send(true);
    }
    let status = build_status(&groups);
    drop(groups);
    save_groups(&app);
    status
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn reorder_group(app: tauri::AppHandle, group_id: u32, direction: String) -> OverlayStatus {
    let state = app.state::<OverlayState>();
    let mut groups = state.groups.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(idx) = groups.iter().position(|g| g.id == group_id) {
        match direction.as_str() {
            "up" if idx > 0 => { groups.swap(idx, idx - 1); }
            "down" if idx + 1 < groups.len() => { groups.swap(idx, idx + 1); }
            _ => {}
        }
    }
    let status = build_status(&groups);
    drop(groups);
    save_groups(&app);
    status
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn rename_group(app: tauri::AppHandle, group_id: u32, name: String) -> OverlayStatus {
    let state = app.state::<OverlayState>();
    let mut groups = state.groups.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(group) = groups.iter_mut().find(|g| g.id == group_id) {
        group.name = name;
    }
    let status = build_status(&groups);
    drop(groups);
    save_groups(&app);
    status
}

#[tauri::command]
pub async fn add_overlay(
    app: tauri::AppHandle,
    path: String,
    group_id: Option<u32>,
) -> Result<OverlayStatus, String> {
    let ext = std::path::Path::new(&path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let (mut data, kind) = match ext.as_str() {
        "shp" => {
            let d = turnout_core::shapefile_reader::parse_shapefile(std::path::Path::new(&path))
                .map_err(|e| format!("Parse error: {e}"))?;
            (d, "shp")
        }
        "geojson" | "json" => {
            let text = std::fs::read_to_string(&path).map_err(|e| format!("Failed to read file: {e}"))?;
            let d = turnout_core::geojson_reader::parse_geojson(&text)
                .map_err(|e| format!("Parse error: {e}"))?;
            (d, "geojson")
        }
        _ => {
            let bytes = std::fs::read(&path).map_err(|e| format!("Failed to read file: {e}"))?;
            let d = turnout_core::kml::parse_kmz(&bytes).map_err(|e| format!("Parse error: {e}"))?;
            (d, "kmz")
        }
    };

    if data.bbox().is_none() {
        return Err("File contains no geometry or overlays".into());
    }

    if data.name.is_none() {
        data.name = std::path::Path::new(&path)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string());
    }

    let state = app.state::<OverlayState>();
    ensure_group_exists(&state, &app, group_id).await?;

    let groups = state.groups.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let group = find_group(&groups, group_id)?;
    group.handle.add_kmz_layer(data, Some(path), kind);
    let status = build_status(&groups);
    drop(groups);
    save_groups(&app);
    Ok(status)
}

#[tauri::command]
pub async fn fetch_wms_layers(url: String) -> Result<Vec<wms::WmsLayerInfo>, String> {
    wms::get_capabilities(&url).await
}

#[tauri::command]
pub async fn add_wms_layer(
    app: tauri::AppHandle,
    url: String,
    layer_name: String,
    display_name: String,
    group_id: Option<u32>,
) -> Result<OverlayStatus, String> {
    let state = app.state::<OverlayState>();
    ensure_group_exists(&state, &app, group_id).await?;

    let groups = state.groups.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let group = find_group(&groups, group_id)?;
    group.handle.add_wms_layer(url, layer_name, display_name);
    let status = build_status(&groups);
    drop(groups);
    save_groups(&app);
    Ok(status)
}

#[tauri::command]
pub async fn fetch_arcgis_services(url: String) -> Result<Vec<arcgis::ArcGisServiceInfo>, String> {
    arcgis::list_services(&url).await
}

#[tauri::command]
pub async fn add_arcgis_layer(
    app: tauri::AppHandle,
    url: String,
    service_name: String,
    display_name: String,
    group_id: Option<u32>,
) -> Result<OverlayStatus, String> {
    let state = app.state::<OverlayState>();
    ensure_group_exists(&state, &app, group_id).await?;

    let groups = state.groups.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let group = find_group(&groups, group_id)?;
    group.handle.add_arcgis_layer(url, service_name, display_name);
    let status = build_status(&groups);
    drop(groups);
    save_groups(&app);
    Ok(status)
}

#[tauri::command]
pub async fn fetch_wmts_layers(url: String) -> Result<Vec<wmts::WmtsLayerInfo>, String> {
    wmts::get_capabilities(&url).await
}

#[tauri::command]
pub async fn add_xyz_layer(
    app: tauri::AppHandle,
    url_template: String,
    display_name: String,
    group_id: Option<u32>,
) -> Result<OverlayStatus, String> {
    let state = app.state::<OverlayState>();
    ensure_group_exists(&state, &app, group_id).await?;

    let groups = state.groups.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let group = find_group(&groups, group_id)?;
    group.handle.add_xyz_layer(url_template, display_name);
    let status = build_status(&groups);
    drop(groups);
    save_groups(&app);
    Ok(status)
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn move_layer(
    app: tauri::AppHandle,
    layer_id: u32,
    from_group_id: u32,
    to_group_id: u32,
) -> Result<OverlayStatus, String> {
    let state = app.state::<OverlayState>();
    let groups = state.groups.lock().unwrap_or_else(std::sync::PoisonError::into_inner);

    let from = groups.iter().find(|g| g.id == from_group_id)
        .ok_or("Source group not found")?;
    let to = groups.iter().find(|g| g.id == to_group_id)
        .ok_or("Destination group not found")?;

    let layer = from.handle.take_layer(layer_id)
        .ok_or("Layer not found in source group")?;
    to.handle.insert_layer(layer);

    let status = build_status(&groups);
    drop(groups);
    save_groups(&app);
    Ok(status)
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn remove_overlay(app: tauri::AppHandle, group_id: u32, layer_id: u32) -> OverlayStatus {
    let state = app.state::<OverlayState>();
    let mut groups = state.groups.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(group) = groups.iter().find(|g| g.id == group_id) {
        group.handle.remove_layer(layer_id);
        if group.handle.layer_count() == 0 {
            let idx = groups.iter().position(|g| g.id == group_id).expect("just found");
            let removed = groups.remove(idx);
            let _ = removed.handle.shutdown_tx.send(true);
        }
    }
    let status = build_status(&groups);
    drop(groups);
    save_groups(&app);
    status
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn rename_layer(app: tauri::AppHandle, group_id: u32, layer_id: u32, name: String) -> OverlayStatus {
    let state = app.state::<OverlayState>();
    let groups = state.groups.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(group) = groups.iter().find(|g| g.id == group_id) {
        group.handle.rename_layer(layer_id, name);
    }
    let status = build_status(&groups);
    drop(groups);
    save_groups(&app);
    status
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn set_group_visible(app: tauri::AppHandle, group_id: u32, visible: bool) -> OverlayStatus {
    let state = app.state::<OverlayState>();
    let groups = state.groups.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(group) = groups.iter().find(|g| g.id == group_id) {
        let mut layers = group.handle.state.layers.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        for layer in layers.iter_mut() {
            layer.visible = visible;
        }
        drop(layers);
        group.handle.clear_cache();
    }
    let status = build_status(&groups);
    drop(groups);
    save_groups(&app);
    status
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn reorder_layer(app: tauri::AppHandle, group_id: u32, layer_id: u32, direction: String) -> OverlayStatus {
    let state = app.state::<OverlayState>();
    let groups = state.groups.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(group) = groups.iter().find(|g| g.id == group_id) {
        match direction.as_str() {
            "up" => { group.handle.move_layer_up(layer_id); }
            "down" => { group.handle.move_layer_down(layer_id); }
            _ => {}
        }
    }
    let status = build_status(&groups);
    drop(groups);
    save_groups(&app);
    status
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn set_layer_visible(app: tauri::AppHandle, group_id: u32, layer_id: u32, visible: bool) -> OverlayStatus {
    let state = app.state::<OverlayState>();
    let groups = state.groups.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(group) = groups.iter().find(|g| g.id == group_id) {
        group.handle.set_layer_visible(layer_id, visible);
    }
    let status = build_status(&groups);
    drop(groups);
    status
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn set_layer_opacity(app: tauri::AppHandle, group_id: u32, layer_id: u32, opacity: f32) -> OverlayStatus {
    let state = app.state::<OverlayState>();
    let groups = state.groups.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(group) = groups.iter().find(|g| g.id == group_id) {
        group.handle.set_layer_opacity(layer_id, opacity);
    }
    let status = build_status(&groups);
    drop(groups);
    status
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn get_overlay_status(app: tauri::AppHandle) -> OverlayStatus {
    let state = app.state::<OverlayState>();
    let groups = state.groups.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    build_status(&groups)
}

#[tauri::command]
pub async fn restore_overlays(app: tauri::AppHandle) -> OverlayStatus {
    let state = app.state::<OverlayState>();

    {
        let mut groups = state.groups.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        for g in groups.drain(..) {
            let _ = g.handle.shutdown_tx.send(true);
        }
    }

    let saved = load_saved(&app);
    if saved.is_empty() {
        return OverlayStatus { groups: Vec::new() };
    }

    for saved_group in &saved {
        let group_id = state.next_group_id();
        let port = port_for_id(group_id);
        let Ok(handle) = tile_server::start(port).await else { continue };

        for layer in &saved_group.layers {
            restore_layer(&handle, layer);
        }
        let mut groups = state.groups.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        groups.push(TileGroup {
            id: group_id,
            name: saved_group.name.clone(),
            handle,
        });
    }

    let groups = state.groups.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    build_status(&groups)
}

// --- Helpers ---

async fn ensure_group_exists(
    state: &OverlayState,
    _app: &tauri::AppHandle,
    group_id: Option<u32>,
) -> Result<(), String> {
    let has_target = {
        let groups = state.groups.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        match group_id {
            Some(id) => groups.iter().any(|g| g.id == id),
            None => !groups.is_empty(),
        }
    };

    if has_target {
        return Ok(());
    }

    let gid = state.next_group_id();
    let port = port_for_id(gid);

    let handle = tile_server::start(port)
        .await
        .map_err(|e| format!("Failed to start tile server: {e}"))?;
    let mut groups = state.groups.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if group_id.is_none() && groups.is_empty() || group_id.is_some() && !groups.iter().any(|g| g.id == group_id.expect("checked")) {
        groups.push(TileGroup { id: gid, name: "Default".to_string(), handle });
    } else {
        let _ = handle.shutdown_tx.send(true);
    }
    Ok(())
}

fn restore_layer(handle: &tile_server::ServerHandle, layer: &SavedLayer) {
    match layer.kind.as_str() {
        "kmz" => {
            let Some(ref path) = layer.path else { return };
            let Ok(bytes) = std::fs::read(path) else {
                eprintln!("Restore: failed to read {path}");
                return;
            };
            let Ok(mut data) = turnout_core::kml::parse_kmz(&bytes) else {
                eprintln!("Restore: failed to parse {path}");
                return;
            };
            if data.name.is_none() {
                data.name = Some(layer.name.clone());
            }
            handle.add_kmz_layer(data, Some(path.clone()), "kmz");
        }
        "shp" => {
            let Some(ref path) = layer.path else { return };
            let Ok(data) = turnout_core::shapefile_reader::parse_shapefile(std::path::Path::new(path)) else {
                eprintln!("Restore: failed to parse shapefile {path}");
                return;
            };
            handle.add_kmz_layer(data, Some(path.clone()), "shp");
        }
        "geojson" => {
            let Some(ref path) = layer.path else { return };
            let Ok(text) = std::fs::read_to_string(path) else {
                eprintln!("Restore: failed to read {path}");
                return;
            };
            let Ok(data) = turnout_core::geojson_reader::parse_geojson(&text) else {
                eprintln!("Restore: failed to parse GeoJSON {path}");
                return;
            };
            handle.add_kmz_layer(data, Some(path.clone()), "geojson");
        }
        "wms" => {
            let (Some(url), Some(wms_layer)) = (&layer.wms_url, &layer.wms_layer) else { return };
            handle.add_wms_layer(url.clone(), wms_layer.clone(), layer.name.clone());
        }
        "arcgis" => {
            let (Some(url), Some(svc)) = (&layer.arcgis_url, &layer.arcgis_service) else { return };
            handle.add_arcgis_layer(url.clone(), svc.clone(), layer.name.clone());
        }
        "xyz" => {
            let Some(url) = &layer.xyz_url else { return };
            handle.add_xyz_layer(url.clone(), layer.name.clone());
        }
        _ => return,
    }

    let layers = handle.state.layers.read().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(last) = layers.last() {
        let id = last.id;
        drop(layers);
        handle.rename_layer(id, layer.name.clone());
        handle.set_layer_visible(id, layer.visible);
        handle.set_layer_opacity(id, layer.opacity);
    }
}

fn find_group(groups: &[TileGroup], group_id: Option<u32>) -> Result<&TileGroup, String> {
    match group_id {
        Some(id) => groups.iter().find(|g| g.id == id).ok_or("Group not found".into()),
        None => groups.first().ok_or("No groups exist".into()),
    }
}

fn build_status(groups: &[TileGroup]) -> OverlayStatus {
    OverlayStatus {
        groups: groups.iter().map(|g| {
            let layers = g.handle.state.layers.read().unwrap_or_else(std::sync::PoisonError::into_inner);
            let errors = g.handle.state.error_layers.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            GroupInfo {
                id: g.id,
                name: g.name.clone(),
                tile_url: format!("http://127.0.0.1:{}/{{z}}/{{x}}/{{y}}", g.handle.port),
                tilejson_url: format!("http://127.0.0.1:{}/tilejson.json", g.handle.port),
                layers: layers.iter().map(|l| LayerInfo {
                    id: l.id,
                    name: l.name.clone(),
                    kind: l.kind,
                    visible: l.visible,
                    opacity: l.opacity,
                    bbox: [l.bbox.1, l.bbox.0, l.bbox.3, l.bbox.2],
                    has_errors: errors.contains(&l.id),
                }).collect(),
            }
        }).collect(),
    }
}

// --- Persistence ---

fn save_groups(app: &tauri::AppHandle) {
    use tauri_plugin_store::StoreExt;

    let state = app.state::<OverlayState>();
    let groups = state.groups.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let saved: Vec<SavedGroup> = groups.iter().map(|g| {
        let layers = g.handle.state.layers.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        SavedGroup {
            name: g.name.clone(),
            layers: layers.iter().map(|l| {
                let (path, wms_url, wms_layer, arcgis_url, arcgis_service, xyz_url) = match &l.source {
                    LayerSource::Kmz { path, .. } => (path.clone(), None, None, None, None, None),
                    LayerSource::Wms { base_url, layer_name } =>
                        (None, Some(base_url.clone()), Some(layer_name.clone()), None, None, None),
                    LayerSource::ArcGis { base_url, service_name } =>
                        (None, None, None, Some(base_url.clone()), Some(service_name.clone()), None),
                    LayerSource::Xyz { url_template } =>
                        (None, None, None, None, None, Some(url_template.clone())),
                };
                SavedLayer {
                    kind: l.kind.to_string(),
                    name: l.name.clone(),
                    visible: l.visible,
                    opacity: l.opacity,
                    path,
                    wms_url,
                    wms_layer,
                    arcgis_url,
                    arcgis_service,
                    xyz_url,
                }
            }).collect(),
        }
    }).collect();
    drop(groups);

    if let Ok(store) = app.store("settings.json") {
        store.set(STORE_KEY, serde_json::json!(saved));
        let _ = store.save();
    }
}

fn load_saved(app: &tauri::AppHandle) -> Vec<SavedGroup> {
    use tauri_plugin_store::StoreExt;

    let Ok(store) = app.store("settings.json") else {
        return Vec::new();
    };
    store
        .get(STORE_KEY)
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}
