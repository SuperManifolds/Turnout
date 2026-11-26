use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use base64::Engine;
use image::{ImageBuffer, Rgba, RgbaImage};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::Serialize;
use tauri::{Emitter, Manager};

use crate::error::{CommandError, CommandResult};
use crate::settings;

const THUMBNAIL_WIDTH: u32 = 200;
const THUMBNAIL_HEIGHT: u32 = 150;
const THUMBNAIL_PADDING: f64 = 10.0;
const TRACK_COLOR: Rgba<u8> = Rgba([74, 158, 255, 255]); // accent blue
const BG_COLOR: Rgba<u8> = Rgba([26, 26, 26, 255]); // dark background

// ══════════════════════════════════════════════════════════════════════
// Mods directory resolution
// ══════════════════════════════════════════════════════════════════════

pub fn resolve_mods_dir(app: &tauri::AppHandle) -> Option<PathBuf> {
    let s = settings::load(app);
    if let Some(ref override_path) = s.mods_dir_override {
        let p = PathBuf::from(override_path);
        if p.exists() {
            return Some(p);
        }
    }
    find_mods_dir()
}

/// Windows-prefix `drive_c` directories to search on macOS/Linux: `CrossOver` and
/// `Whisky` bottles, a plain wine prefix, and NIMBY Rails' Proton compatdata. Both
/// the Saved-Games mods folder and the Steam install live under one of these, so
/// the mods- and game-dir resolvers share this enumeration. Empty on native
/// Windows, where the game lives on the real filesystem.
#[cfg(not(target_os = "windows"))]
fn wine_drive_c_roots() -> Vec<PathBuf> {
    let Some(home) = dirs_next::home_dir() else { return Vec::new() };
    let mut roots = Vec::new();

    #[cfg(target_os = "macos")]
    for base in &[
        home.join("Library/Application Support/CrossOver/Bottles"),
        home.join("Library/Containers/com.isaacmarovitz.Whisky/Bottles"),
    ] {
        if let Ok(bottles) = fs::read_dir(base) {
            for bottle in bottles.flatten() {
                roots.push(bottle.path().join("drive_c"));
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        roots.push(home.join(".wine/drive_c"));
        roots.push(home.join(".local/share/Steam/steamapps/compatdata/1134710/pfx/drive_c"));
        roots.push(home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam/steamapps/compatdata/1134710/pfx/drive_c"));
    }

    roots
}

#[cfg(target_os = "windows")]
fn wine_drive_c_roots() -> Vec<PathBuf> {
    Vec::new()
}

const MODS_SUBPATH: &str = "Saved Games/Weird and Wry/NIMBY Rails/mods";
/// The game's Steam install directory, relative to a `Steam/` root.
const NIMBY_INSTALL_SUBPATH: &str = "steamapps/common/NIMBY Rails";

fn find_mods_dir() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    #[cfg(target_os = "windows")]
    if let Some(home) = dirs_next::home_dir() {
        candidates.push(home.join(MODS_SUBPATH));
    }

    for drive_c in wine_drive_c_roots() {
        if let Ok(users) = fs::read_dir(drive_c.join("users")) {
            for user in users.flatten() {
                candidates.push(user.path().join(MODS_SUBPATH));
            }
        }
    }

    candidates.into_iter().find(|p| p.exists())
}

/// The game's install directory, honoring the `game_dir_override` setting and
/// falling back to the default Steam install locations (in-bottle Steam on
/// macOS/Linux, native Steam otherwise). Mirrors [`resolve_mods_dir`].
pub fn resolve_game_dir(app: &tauri::AppHandle) -> Option<PathBuf> {
    let s = settings::load(app);
    if let Some(ref override_path) = s.game_dir_override {
        let p = PathBuf::from(override_path);
        if p.exists() {
            return Some(p);
        }
    }
    find_game_dir()
}

fn find_game_dir() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    // In-bottle / Proton-prefix Steam installs (macOS/Linux).
    for drive_c in wine_drive_c_roots() {
        candidates.push(drive_c.join("Program Files (x86)/Steam").join(NIMBY_INSTALL_SUBPATH));
        candidates.push(drive_c.join("Program Files/Steam").join(NIMBY_INSTALL_SUBPATH));
    }

    #[cfg(target_os = "windows")]
    for var in ["ProgramFiles(x86)", "ProgramFiles"] {
        if let Some(pf) = std::env::var_os(var) {
            candidates.push(PathBuf::from(pf).join("Steam").join(NIMBY_INSTALL_SUBPATH));
        }
    }

    #[cfg(target_os = "linux")]
    if let Some(home) = dirs_next::home_dir() {
        candidates.push(home.join(".steam/steam").join(NIMBY_INSTALL_SUBPATH));
        candidates.push(home.join(".local/share/Steam").join(NIMBY_INSTALL_SUBPATH));
        candidates.push(home.join(".var/app/com.valvesoftware.Steam/data/Steam").join(NIMBY_INSTALL_SUBPATH));
    }

    candidates.into_iter().find(|p| p.is_dir())
}

/// Path to NIMBY Rails' population map (`resources/maps/pop400.pmtiles`), if the
/// install can be located and the file is present.
pub fn pop400_path(app: &tauri::AppHandle) -> Option<PathBuf> {
    let path = resolve_game_dir(app)?.join("resources/maps/pop400.pmtiles");
    path.is_file().then_some(path)
}

// ══════════════════════════════════════════════════════════════════════
// Blueprint info
// ══════════════════════════════════════════════════════════════════════

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
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

fn thumbnail_dir(app: &tauri::AppHandle) -> Option<PathBuf> {
    Some(app.path().app_data_dir().ok()?.join("thumbnails"))
}

fn thumbnail_path(app: &tauri::AppHandle, folder_name: &str, clip_index: usize) -> Option<PathBuf> {
    let dir = thumbnail_dir(app)?;
    if clip_index == 0 {
        Some(dir.join(format!("{folder_name}.png")))
    } else {
        Some(dir.join(format!("{folder_name}_{clip_index}.png")))
    }
}

fn mercator_to_latlon(x: f64, y: f64) -> (f64, f64) {
    turnout_core::geo::mercator_to_latlon(x, y)
}

fn file_modified_secs(path: &std::path::Path) -> u64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .map_or(0, |t| t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs())
}

fn parse_blueprint_infos(mods_dir: &std::path::Path, folder_name: &str, app: &tauri::AppHandle) -> Vec<BlueprintInfo> {
    let nrclip_path = mods_dir.join(folder_name).join("blueprints.nrclip");
    let Ok(data) = fs::read(&nrclip_path) else { return Vec::new() };
    let file_size = data.len() as u64;
    let modified = file_modified_secs(&nrclip_path);
    let file = match turnout_core::nrc1::NrclipFile::from_bytes(&data) {
        Ok(file) => file,
        Err(e) => {
            // A corrupt blueprint would otherwise vanish from the list silently.
            tracing::warn!("skipping unreadable blueprint '{folder_name}': {e}");
            return Vec::new();
        }
    };

    let mut results = Vec::new();
    let mut clip_index = 0;

    for coll in &file.collections {
        for clip in &coll.clips {
            let (center_lat, center_lon) = mercator_to_latlon(clip.center_x, clip.center_y);
            let has_thumbnail = thumbnail_path(app, folder_name, clip_index)
                .is_some_and(|thumb| thumb.exists() && file_modified_secs(&thumb) >= modified);

            // Use collection name, falling back to folder name
            let clip_name = if coll.name.is_empty() {
                folder_name.to_string()
            } else if coll.clips.len() > 1 {
                format!("{} ({})", coll.name, clip_index + 1)
            } else {
                coll.name.clone()
            };

            results.push(BlueprintInfo {
                folder_name: folder_name.to_string(),
                clip_index,
                clip_name,
                track_count: clip.tracks.len(),
                file_size,
                modified,
                center_lat,
                center_lon,
                has_thumbnail,
            });
            clip_index += 1;
        }
    }

    results
}

// ══════════════════════════════════════════════════════════════════════
// Thumbnail rendering
// ══════════════════════════════════════════════════════════════════════

fn render_thumbnail(tracks: &[turnout_core::types::Track]) -> RgbaImage {
    let mut img: RgbaImage = ImageBuffer::from_pixel(THUMBNAIL_WIDTH, THUMBNAIL_HEIGHT, BG_COLOR);

    if tracks.is_empty() {
        return img;
    }

    // Compute bounding box
    let mut min_x = f64::MAX;
    let mut min_y = f64::MAX;
    let mut max_x = f64::MIN;
    let mut max_y = f64::MIN;
    for t in tracks {
        min_x = min_x.min(t.x);
        min_y = min_y.min(t.y);
        max_x = max_x.max(t.x);
        max_y = max_y.max(t.y);
    }

    let data_w = max_x - min_x;
    let data_h = max_y - min_y;
    if data_w < 1.0 && data_h < 1.0 {
        return img;
    }

    let canvas_w = f64::from(THUMBNAIL_WIDTH) - 2.0 * THUMBNAIL_PADDING;
    let canvas_h = f64::from(THUMBNAIL_HEIGHT) - 2.0 * THUMBNAIL_PADDING;
    let scale = if data_w < 1.0 {
        canvas_h / data_h
    } else if data_h < 1.0 {
        canvas_w / data_w
    } else {
        (canvas_w / data_w).min(canvas_h / data_h)
    };

    // Center the drawing
    let offset_x = THUMBNAIL_PADDING + (canvas_w - data_w * scale) / 2.0;
    let offset_y = THUMBNAIL_PADDING + (canvas_h - data_h * scale) / 2.0;

    // Build node_id → (px, py) lookup
    let positions: HashMap<i64, (i32, i32)> = tracks.iter().map(|t| {
        let px = ((t.x - min_x) * scale + offset_x) as i32;
        // Flip Y — track Y increases northward, pixel Y increases downward
        let py = ((max_y - t.y) * scale + offset_y) as i32;
        (t.node_id, (px, py))
    }).collect();

    // Draw lines along prev/next chains
    for t in tracks {
        if t.next_node == 0 {
            continue;
        }
        let Some(&(x0, y0)) = positions.get(&t.node_id) else { continue };
        let Some(&(x1, y1)) = positions.get(&t.next_node) else { continue };
        draw_line(&mut img, x0, y0, x1, y1, TRACK_COLOR);
    }

    img
}

fn draw_line(img: &mut RgbaImage, x0: i32, y0: i32, x1: i32, y1: i32, color: Rgba<u8>) {
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    let mut x = x0;
    let mut y = y0;
    let w = img.width() as i32;
    let h = img.height() as i32;

    loop {
        if x >= 0 && x < w && y >= 0 && y < h {
            img.put_pixel(x as u32, y as u32, color);
        }
        if x == x1 && y == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}

// ══════════════════════════════════════════════════════════════════════
// Tauri commands
// ══════════════════════════════════════════════════════════════════════

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn get_mods_dir(app: tauri::AppHandle) -> Option<String> {
    resolve_mods_dir(&app).map(|p| p.to_string_lossy().to_string())
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn get_game_dir(app: tauri::AppHandle) -> Option<String> {
    resolve_game_dir(&app).map(|p| p.to_string_lossy().to_string())
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn save_blueprint(app: tauri::AppHandle, name: String, data: Vec<u8>) -> CommandResult<String> {
    let mods_dir = resolve_mods_dir(&app).ok_or_else(|| {
        CommandError::NotFound("Could not find Nimby Rails mods folder. Set it in Settings.".into())
    })?;

    let folder = sanitize_folder_name(&name);
    if folder.is_empty() {
        return Err(CommandError::Invalid("Invalid blueprint name".into()));
    }
    let blueprint_dir = mods_dir.join(&folder);
    fs::create_dir_all(&blueprint_dir)
        .map_err(|e| CommandError::Io(format!("Failed to create directory: {e}")))?;

    let path = blueprint_dir.join("blueprints.nrclip");
    fs::write(&path, &data)
        .map_err(|e| CommandError::Io(format!("Failed to write file: {e}")))?;

    Ok(path.to_string_lossy().to_string())
}

fn sanitize_folder_name(name: &str) -> String {
    name.trim()
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect::<String>()
        .trim_matches('_')
        .to_lowercase()
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn blueprint_exists(app: tauri::AppHandle, name: String) -> bool {
    let folder = sanitize_folder_name(&name);
    resolve_mods_dir(&app)
        .is_some_and(|mods| mods.join(&folder).join("blueprints.nrclip").exists())
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn list_blueprints(app: tauri::AppHandle) -> CommandResult<Vec<BlueprintInfo>> {
    let mods_dir = resolve_mods_dir(&app).ok_or_else(|| {
        CommandError::NotFound("Could not find Nimby Rails mods folder. Set it in Settings.".into())
    })?;

    let entries = fs::read_dir(&mods_dir)
        .map_err(|e| CommandError::Io(format!("Failed to read mods directory: {e}")))?;

    let mut blueprints: Vec<BlueprintInfo> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|ft| ft.is_dir()))
        .flat_map(|e| {
            let folder_name = e.file_name().to_string_lossy().to_string();
            parse_blueprint_infos(&mods_dir, &folder_name, &app)
        })
        .collect();

    blueprints.sort_by_key(|b| std::cmp::Reverse(b.modified));
    Ok(blueprints)
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn generate_thumbnail(app: tauri::AppHandle, folder_name: String, clip_index: usize) -> CommandResult<String> {
    let mods_dir = resolve_mods_dir(&app)
        .ok_or_else(|| CommandError::NotFound("Mods folder not found".into()))?;

    let nrclip_path = mods_dir.join(&folder_name).join("blueprints.nrclip");
    let data = fs::read(&nrclip_path)
        .map_err(|e| CommandError::Io(format!("Failed to read blueprint: {e}")))?;
    let file = turnout_core::nrc1::NrclipFile::from_bytes(&data)
        .map_err(|e| CommandError::Parse(format!("Failed to parse blueprint: {e}")))?;

    // Find the clip at the given index across all collections
    let clip = file.collections.iter()
        .flat_map(|c| &c.clips)
        .nth(clip_index)
        .ok_or_else(|| CommandError::Invalid(format!("Clip index {clip_index} out of range")))?;

    let img = render_thumbnail(&clip.tracks);

    // Encode as PNG to memory, then base64 data URL
    let mut png_bytes = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut png_bytes);
    img.write_with_encoder(encoder)
        .map_err(|e| CommandError::Other(format!("Failed to encode thumbnail: {e}")))?;

    let b64 = base64::engine::general_purpose::STANDARD.encode(&png_bytes);
    let data_url = format!("data:image/png;base64,{b64}");

    // Cache to disk for has_thumbnail checks
    if let Some(thumb_dir) = thumbnail_dir(&app) {
        let _ = fs::create_dir_all(&thumb_dir);
    }
    if let Some(path) = thumbnail_path(&app, &folder_name, clip_index) {
        let _ = fs::write(path, &png_bytes);
    }

    Ok(data_url)
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn delete_blueprint(app: tauri::AppHandle, folder_name: String) -> CommandResult<()> {
    let mods_dir = resolve_mods_dir(&app)
        .ok_or_else(|| CommandError::NotFound("Mods folder not found".into()))?;

    let blueprint_dir = mods_dir.join(&folder_name);
    if !blueprint_dir.exists() {
        return Err(CommandError::NotFound(format!("Blueprint '{folder_name}' not found")));
    }

    fs::remove_dir_all(&blueprint_dir)
        .map_err(|e| CommandError::Io(format!("Failed to delete: {e}")))?;

    // Remove cached thumbnails (all clip indices)
    remove_thumbnails_for(&app, &folder_name);

    Ok(())
}

/// Remove every cached thumbnail for a blueprint folder. Thumbnails are named
/// `{folder}.png` (clip 0) and `{folder}_{clip_index}.png`, so match on the exact
/// name or the `{folder}_` prefix — never a bare `starts_with(folder)`, which would
/// also delete a sibling blueprint's thumbnails (e.g. folder "train" vs "train yard").
fn remove_thumbnails_for(app: &tauri::AppHandle, folder_name: &str) {
    let Some(thumb_dir) = thumbnail_dir(app) else { return };
    let Ok(entries) = fs::read_dir(&thumb_dir) else { return };
    let exact = format!("{folder_name}.png");
    let indexed_prefix = format!("{folder_name}_");
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let is_png = std::path::Path::new(&name).extension().is_some_and(|ext| ext.eq_ignore_ascii_case("png"));
        if is_png && (name == exact || name.starts_with(&indexed_prefix)) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn rename_blueprint(app: tauri::AppHandle, old_name: String, new_name: String) -> CommandResult<()> {
    let mods_dir = resolve_mods_dir(&app)
        .ok_or_else(|| CommandError::NotFound("Mods folder not found".into()))?;

    let old_dir = mods_dir.join(&old_name);
    let new_dir = mods_dir.join(&new_name);

    if !old_dir.exists() {
        return Err(CommandError::NotFound(format!("Blueprint '{old_name}' not found")));
    }
    if new_dir.exists() {
        return Err(CommandError::Invalid(format!("Blueprint '{new_name}' already exists")));
    }

    // Update collection name inside the .nrclip file
    let nrclip_path = old_dir.join("blueprints.nrclip");
    if nrclip_path.exists() {
        let data = fs::read(&nrclip_path)
            .map_err(|e| CommandError::Io(format!("Failed to read blueprint: {e}")))?;
        let mut file = turnout_core::nrc1::NrclipFile::from_bytes(&data)
            .map_err(|e| CommandError::Parse(format!("Failed to parse blueprint: {e}")))?;
        for coll in &mut file.collections {
            coll.name.clone_from(&new_name);
        }
        let new_data = file.to_bytes()
            .map_err(|e| CommandError::Other(format!("Failed to serialize blueprint: {e}")))?;
        fs::write(&nrclip_path, new_data)
            .map_err(|e| CommandError::Io(format!("Failed to write blueprint: {e}")))?;
    }

    // Rename folder
    fs::rename(&old_dir, &new_dir)
        .map_err(|e| CommandError::Io(format!("Failed to rename: {e}")))?;

    // Remove old thumbnails
    remove_thumbnails_for(&app, &old_name);

    Ok(())
}

#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn open_blueprint_folder(app: tauri::AppHandle, folder_name: String) -> CommandResult<()> {
    let mods_dir = resolve_mods_dir(&app)
        .ok_or_else(|| CommandError::NotFound("Mods folder not found".into()))?;

    let path = mods_dir.join(&folder_name);
    if !path.exists() {
        return Err(CommandError::NotFound(format!("Blueprint folder '{folder_name}' not found")));
    }

    tauri_plugin_opener::reveal_item_in_dir(&path)
        .map_err(|e| CommandError::Other(format!("Failed to open folder: {e}")))
}

// ══════════════════════════════════════════════════════════════════════
// Filesystem watcher
// ══════════════════════════════════════════════════════════════════════

/// Start watching the mods directory for changes. Emits "blueprints-changed"
/// events to the frontend. The watcher is stored in Tauri managed state so
/// it lives for the app's lifetime.
pub fn start_watcher(app: &tauri::AppHandle) {
    let Some(mods_dir) = resolve_mods_dir(app) else { return };

    let handle = app.clone();
    let watcher = notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
        if let Ok(event) = res
            && matches!(
                event.kind,
                notify::EventKind::Create(_) | notify::EventKind::Remove(_) | notify::EventKind::Modify(_)
            )
        {
            let _ = handle.emit("blueprints-changed", ());
        }
    });

    let Ok(mut watcher) = watcher else { return };
    if watcher.watch(&mods_dir, RecursiveMode::Recursive).is_ok() {
        // Store in managed state to keep alive
        app.manage(ModsWatcher(Mutex::new(Some(watcher))));
    }
}

/// Kept in managed state to prevent the watcher from being dropped.
struct ModsWatcher(#[expect(dead_code)] Mutex<Option<RecommendedWatcher>>);
