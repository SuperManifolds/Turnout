use std::collections::HashMap;
use tauri::Emitter;

use crate::blueprint;

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn import_orm(
    app: tauri::AppHandle,
    json: String,
    name: String,
    railway_types: Vec<String>,
    apply_speed_limits: bool,
    clip_bbox: Option<(f64, f64, f64, f64)>,
    tangent_mode: bool,
    type_speed_overrides: HashMap<String, u32>,
) -> Result<(Vec<u8>, usize), String> {
    let (track_kinds, mod_metas) = blueprint::resolve_mods_dir(&app)
        .and_then(|mods| {
            let collections = mods.parent()?.join("collections.nrclip");
            if collections.exists() { Some(collections) } else { None }
        })
        .and_then(|path| {
            turnout_core::import::extract_vanilla_track_kinds(&path.to_string_lossy()).ok()
        })
        .unwrap_or_default();

    let on_progress = |stage: &str| {
        let _ = app.emit("import-progress", stage);
    };

    turnout_core::import::import_orm(
        &json, &name, &railway_types, apply_speed_limits, clip_bbox, tangent_mode,
        &type_speed_overrides, track_kinds, mod_metas, &on_progress,
    )
    .map_err(|e| e.to_string())
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn count_track_nodes(
    json: String,
    railway_types: Vec<String>,
    clip_bbox: Option<(f64, f64, f64, f64)>,
    tangent_mode: bool,
) -> Result<usize, String> {
    turnout_core::import::count_track_nodes(&json, &railway_types, clip_bbox, tangent_mode)
        .map_err(|e| e.to_string())
}
