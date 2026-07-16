use std::collections::HashMap;
use tauri::Emitter;

use crate::error::CommandResult;

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
) -> CommandResult<(Vec<u8>, usize)> {
    let track_kinds = turnout_core::import::default_track_kinds();
    let mod_metas = Vec::new();

    let on_progress = |stage: &str| {
        let _ = app.emit("import-progress", stage);
    };

    Ok(turnout_core::import::import_orm(
        &json, &name, &railway_types, apply_speed_limits, clip_bbox, tangent_mode,
        &type_speed_overrides, track_kinds, mod_metas, &on_progress,
    )?)
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn count_track_nodes(
    json: String,
    railway_types: Vec<String>,
    clip_bbox: Option<(f64, f64, f64, f64)>,
    tangent_mode: bool,
) -> CommandResult<usize> {
    Ok(turnout_core::import::count_track_nodes(&json, &railway_types, clip_bbox, tangent_mode)?)
}
