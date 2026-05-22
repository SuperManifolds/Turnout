#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[tauri::command]
fn import_orm(json: String, name: String) -> Result<Vec<u8>, String> {
    nimby_gen_core::import::import_orm(&json, &name).map_err(|e| e.to_string())
}

#[tauri::command]
fn get_mods_dir() -> Option<String> {
    // Try common Nimby Rails mods paths
    let candidates = [
        dirs_next::data_dir().map(|d| d.join("Weird and Wry/NIMBY Rails/mods")),
        dirs_next::home_dir().map(|d| {
            d.join("Library/Application Support/CrossOver/Bottles/Steam/drive_c/users/crossover/Saved Games/Weird and Wry/NIMBY Rails/mods")
        }),
    ];
    for candidate in &candidates {
        if let Some(path) = candidate {
            if path.exists() {
                return Some(path.to_string_lossy().to_string());
            }
        }
    }
    None
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![import_orm, get_mods_dir])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
