#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod blueprint;
mod import;
mod overpass;
mod settings;

use tauri::menu::{MenuBuilder, SubmenuBuilder, MenuItemBuilder};
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

fn open_settings_window(app: &tauri::AppHandle) {
    if let Some(window) = app.webview_windows().get("settings") {
        let _ = window.set_focus();
        return;
    }
    let _ = WebviewWindowBuilder::new(app, "settings", WebviewUrl::App("index.html".into()))
        .title("Settings")
        .inner_size(480.0, 450.0)
        .resizable(false)
        .maximizable(false)
        .build();
}

fn setup_menu(app: &tauri::AppHandle) -> tauri::Result<()> {
    let app_submenu = SubmenuBuilder::new(app, "Turnout")
        .about(None)
        .separator()
        .item(
            &MenuItemBuilder::new("Settings...")
                .id("settings")
                .accelerator("CmdOrCtrl+,")
                .build(app)?
        )
        .separator()
        .services()
        .separator()
        .hide()
        .hide_others()
        .show_all()
        .separator()
        .quit()
        .build()?;

    let edit_submenu = SubmenuBuilder::new(app, "Edit")
        .undo()
        .redo()
        .separator()
        .cut()
        .copy()
        .paste()
        .select_all()
        .build()?;

    let window_submenu = SubmenuBuilder::new(app, "Window")
        .minimize()
        .separator()
        .close_window()
        .build()?;

    let menu = MenuBuilder::new(app)
        .items(&[&app_submenu, &edit_submenu, &window_submenu])
        .build()?;

    app.set_menu(menu)?;
    Ok(())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_store::Builder::new().build())
        .setup(|app| {
            setup_menu(app.handle())?;
            Ok(())
        })
        .on_menu_event(|app, event| {
            if event.id() == "settings" {
                open_settings_window(app);
            }
        })
        .invoke_handler(tauri::generate_handler![
            overpass::fetch_overpass,
            import::import_orm,
            import::count_track_nodes,
            blueprint::get_mods_dir,
            blueprint::save_blueprint,
            settings::get_settings,
            settings::set_settings,
            settings::pick_folder,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
