use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::Manager;
use tauri_plugin_dialog::DialogExt;

use crate::arcgis;
use crate::tile_server::{self, LayerKind, LayerSource, UnpoisonExt};
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
    name: String,
    visible: bool,
    opacity: f32,
    #[serde(flatten)]
    source: SavedSource,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum SavedSource {
    Kmz { path: String },
    Shp { path: String },
    GeoJson { path: String },
    Wms { wms_url: String, wms_layer: String },
    ArcGis { arcgis_url: String, arcgis_service: String },
    Xyz { xyz_url: String },
    Wmts { xyz_url: String },
    Apple { xyz_url: String },
    Bing { xyz_url: String },
    MbTiles { path: String },
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct LayerInfo {
    pub id: u32,
    pub name: String,
    pub kind: LayerKind,
    pub visible: bool,
    pub opacity: f32,
    pub bbox: [f64; 4],
    pub has_errors: bool,
    pub source_url: Option<String>,
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
        let mut id = self.next_group_id.lock().unpoison();
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
        .add_filter("Overlay files", &["kmz", "kml", "shp", "geojson", "json", "mbtiles"])
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

    let mut groups = state.groups.lock().unpoison();
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
    let mut groups = state.groups.lock().unpoison();
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
    let mut groups = state.groups.lock().unpoison();
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
    let mut groups = state.groups.lock().unpoison();
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

    if ext == "mbtiles" {
        let display_name = std::path::Path::new(&path)
            .file_stem()
            .map_or_else(|| "MBTiles".into(), |s| s.to_string_lossy().to_string());
        return add_mbtiles_layer(app, path, display_name, group_id).await;
    }

    let kind = kind_for_extension(&path);
    let mut data = parse_overlay_file(&path, kind)?;

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

    let groups = state.groups.lock().unpoison();
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

    let groups = state.groups.lock().unpoison();
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

    let groups = state.groups.lock().unpoison();
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
    kind: Option<String>,
) -> Result<OverlayStatus, String> {
    let state = app.state::<OverlayState>();
    ensure_group_exists(&state, &app, group_id).await?;

    let layer_kind = match kind.as_deref() {
        Some("wmts") => LayerKind::Wmts,
        Some("apple") => LayerKind::Apple,
        Some("bing") => LayerKind::Bing,
        _ => LayerKind::Xyz,
    };
    let groups = state.groups.lock().unpoison();
    let group = find_group(&groups, group_id)?;
    group.handle.add_xyz_layer_with_kind(url_template, display_name, layer_kind);
    let status = build_status(&groups);
    drop(groups);
    save_groups(&app);
    Ok(status)
}

#[tauri::command]
pub async fn add_mbtiles_layer(
    app: tauri::AppHandle,
    path: String,
    display_name: String,
    group_id: Option<u32>,
) -> Result<OverlayStatus, String> {
    let state = app.state::<OverlayState>();
    ensure_group_exists(&state, &app, group_id).await?;

    let groups = state.groups.lock().unpoison();
    let group = find_group(&groups, group_id)?;
    group.handle.add_mbtiles_layer(path, display_name)?;
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
    let groups = state.groups.lock().unpoison();

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
    let mut groups = state.groups.lock().unpoison();
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
    let groups = state.groups.lock().unpoison();
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
    let groups = state.groups.lock().unpoison();
    if let Some(group) = groups.iter().find(|g| g.id == group_id) {
        group.handle.set_all_visible(visible);
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
    let groups = state.groups.lock().unpoison();
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
    let groups = state.groups.lock().unpoison();
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
    let groups = state.groups.lock().unpoison();
    if let Some(group) = groups.iter().find(|g| g.id == group_id) {
        group.handle.set_layer_opacity(layer_id, opacity);
    }
    let status = build_status(&groups);
    drop(groups);
    status
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn update_apple_urls(
    app: tauri::AppHandle,
    access_key: String,
    map_version: Option<String>,
    sat_version: Option<String>,
) -> OverlayStatus {
    apply_apple_credentials(&app, &access_key, map_version.as_deref(), sat_version.as_deref())
}

/// Rewrites every live Apple layer's tile URL with a fresh access key and per-kind
/// version, then persists the overlay set. Shared by the manual `update_apple_urls`
/// command and the automatic token refresher.
pub(crate) fn apply_apple_credentials(
    app: &tauri::AppHandle,
    access_key: &str,
    map_version: Option<&str>,
    sat_version: Option<&str>,
) -> OverlayStatus {
    let state = app.state::<OverlayState>();
    let groups = state.groups.lock().unpoison();
    for group in groups.iter() {
        let layers = group.handle.state.layers.read().unpoison();
        let apple_layers: Vec<(u32, bool)> = layers.iter().filter_map(|l| {
            if l.kind != LayerKind::Apple { return None; }
            if let tile_server::LayerSource::Xyz { url_template } = &l.source {
                let is_sat = url_template.contains("sat-cdn");
                Some((l.id, is_sat))
            } else { None }
        }).collect();
        drop(layers);

        for (id, is_sat) in apple_layers {
            let ver = if is_sat { sat_version } else { map_version };
            let Some(ver) = ver else { continue };
            let url = turnout_core::geo::apple_tile_url(access_key, ver, is_sat);
            group.handle.update_xyz_url(id, url);
        }
    }
    let status = build_status(&groups);
    drop(groups);
    save_groups(app);
    status
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn get_overlay_status(app: tauri::AppHandle) -> OverlayStatus {
    let state = app.state::<OverlayState>();
    let groups = state.groups.lock().unpoison();
    build_status(&groups)
}

#[tauri::command]
pub async fn restore_overlays(app: tauri::AppHandle) -> OverlayStatus {
    let state = app.state::<OverlayState>();

    {
        let mut groups = state.groups.lock().unpoison();
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
        let mut groups = state.groups.lock().unpoison();
        groups.push(TileGroup {
            id: group_id,
            name: saved_group.name.clone(),
            handle,
        });
    }

    let groups = state.groups.lock().unpoison();
    build_status(&groups)
}

// --- Helpers ---

async fn ensure_group_exists(
    state: &OverlayState,
    _app: &tauri::AppHandle,
    group_id: Option<u32>,
) -> Result<(), String> {
    let has_target = {
        let groups = state.groups.lock().unpoison();
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
    let mut groups = state.groups.lock().unpoison();
    if group_id.is_none() && groups.is_empty() || group_id.is_some() && !groups.iter().any(|g| g.id == group_id.expect("checked")) {
        groups.push(TileGroup { id: gid, name: "Default".to_string(), handle });
    } else {
        let _ = handle.shutdown_tx.send(true);
    }
    Ok(())
}

fn parse_overlay_file(path: &str, kind: LayerKind) -> Result<turnout_core::kml::OverlayData, String> {
    match kind {
        LayerKind::Shp => turnout_core::shapefile_reader::parse_shapefile(std::path::Path::new(path))
            .map_err(|e| e.to_string()),
        LayerKind::GeoJson => {
            let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
            turnout_core::geojson_reader::parse_geojson(&text).map_err(|e| e.to_string())
        }
        _ => {
            let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
            turnout_core::kml::parse_kmz(&bytes).map_err(|e| e.to_string())
        }
    }
}

fn kind_for_extension(path: &str) -> LayerKind {
    match std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "shp" => LayerKind::Shp,
        "geojson" | "json" => LayerKind::GeoJson,
        _ => LayerKind::Kmz,
    }
}

fn restore_layer(handle: &tile_server::ServerHandle, layer: &SavedLayer) {
    match &layer.source {
        SavedSource::Kmz { path } | SavedSource::Shp { path } | SavedSource::GeoJson { path } => {
            let kind = match &layer.source {
                SavedSource::Shp { .. } => LayerKind::Shp,
                SavedSource::GeoJson { .. } => LayerKind::GeoJson,
                _ => LayerKind::Kmz,
            };
            let Ok(data) = parse_overlay_file(path, kind) else {
                eprintln!("Restore: failed to parse {path}");
                return;
            };
            handle.add_kmz_layer(data, Some(path.clone()), kind);
        }
        SavedSource::Wms { wms_url, wms_layer } => {
            handle.add_wms_layer(wms_url.clone(), wms_layer.clone(), layer.name.clone());
        }
        SavedSource::ArcGis { arcgis_url, arcgis_service } => {
            handle.add_arcgis_layer(arcgis_url.clone(), arcgis_service.clone(), layer.name.clone());
        }
        SavedSource::Xyz { xyz_url }
        | SavedSource::Wmts { xyz_url }
        | SavedSource::Apple { xyz_url }
        | SavedSource::Bing { xyz_url } => {
            handle.add_xyz_layer(xyz_url.clone(), layer.name.clone());
        }
        SavedSource::MbTiles { path } => {
            if let Err(e) = handle.add_mbtiles_layer(path.clone(), layer.name.clone()) {
                eprintln!("Restore: failed to open MBTiles {path}: {e}");
                return;
            }
        }
    }

    let layers = handle.state.layers.read().unpoison();
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
            let layers = g.handle.state.layers.read().unpoison();
            let errors = g.handle.state.error_layers.lock().unpoison();
            GroupInfo {
                id: g.id,
                name: g.name.clone(),
                tile_url: format!("http://127.0.0.1:{}/{{z}}/{{x}}/{{y}}", g.handle.port),
                tilejson_url: format!("http://127.0.0.1:{}/tilejson.json", g.handle.port),
                layers: layers.iter().map(|l| {
                    let source_url = match &l.source {
                        LayerSource::Xyz { url_template } => Some(url_template.clone()),
                        LayerSource::Wms { base_url, layer_name } => Some(format!(
                            "{base_url}?SERVICE=WMS&VERSION=1.1.1&REQUEST=GetMap&LAYERS={layer_name}\
                             &SRS=EPSG:3857&FORMAT=image/png&WIDTH=256&HEIGHT=256&BBOX={{bbox}}"
                        )),
                        _ => None,
                    };
                    LayerInfo {
                        id: l.id,
                        name: l.name.clone(),
                        kind: l.kind,
                        visible: l.visible,
                        opacity: l.opacity,
                        bbox: [l.bbox.1, l.bbox.0, l.bbox.3, l.bbox.2],
                        has_errors: errors.contains(&l.id),
                        source_url,
                    }
                }).collect(),
            }
        }).collect(),
    }
}

// --- Persistence ---

fn save_groups(app: &tauri::AppHandle) {
    use tauri_plugin_store::StoreExt;

    let state = app.state::<OverlayState>();
    let groups = state.groups.lock().unpoison();
    let saved: Vec<SavedGroup> = groups.iter().map(|g| {
        let layers = g.handle.state.layers.read().unpoison();
        SavedGroup {
            name: g.name.clone(),
            layers: layers.iter().filter_map(|l| {
                let source = match (&l.source, l.kind) {
                    (LayerSource::Kmz { path: Some(p), .. }, LayerKind::Kmz) =>
                        SavedSource::Kmz { path: p.clone() },
                    (LayerSource::Kmz { path: Some(p), .. }, LayerKind::Shp) =>
                        SavedSource::Shp { path: p.clone() },
                    (LayerSource::Kmz { path: Some(p), .. }, LayerKind::GeoJson) =>
                        SavedSource::GeoJson { path: p.clone() },
                    (LayerSource::Wms { base_url, layer_name }, _) =>
                        SavedSource::Wms { wms_url: base_url.clone(), wms_layer: layer_name.clone() },
                    (LayerSource::ArcGis { base_url, service_name }, _) =>
                        SavedSource::ArcGis { arcgis_url: base_url.clone(), arcgis_service: service_name.clone() },
                    (LayerSource::Xyz { url_template }, kind) => {
                        let url = url_template.clone();
                        match kind {
                            LayerKind::Wmts => SavedSource::Wmts { xyz_url: url },
                            LayerKind::Apple => SavedSource::Apple { xyz_url: url },
                            LayerKind::Bing => SavedSource::Bing { xyz_url: url },
                            _ => SavedSource::Xyz { xyz_url: url },
                        }
                    }
                    (LayerSource::MbTiles { path, .. }, _) =>
                        SavedSource::MbTiles { path: path.clone() },
                    _ => return None,
                };
                Some(SavedLayer {
                    name: l.name.clone(),
                    visible: l.visible,
                    opacity: l.opacity,
                    source,
                })
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
