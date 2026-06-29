#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod blueprint;
mod import;
mod overlay;
mod overpass;
mod settings;
mod tile_server;

use tauri::menu::{MenuBuilder, SubmenuBuilder, MenuItemBuilder};
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

fn open_settings_window(app: &tauri::AppHandle) {
    if let Some(window) = app.webview_windows().get("settings") {
        let _ = window.set_focus();
        return;
    }
    let mut builder = WebviewWindowBuilder::new(app, "settings", WebviewUrl::App("index.html".into()))
        .title("Settings")
        .inner_size(480.0, 400.0);

    // Inherit system theme so CSS prefers-color-scheme works
    if let Some(theme) = app.get_webview_window("main").and_then(|w| w.theme().ok()) {
        builder = builder.theme(Some(theme));
    }

    let _ = builder.build();
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
    // Work around WebKitGTK EGL crashes on some Linux GPU drivers.
    // SAFETY: Called at the very start of main, before any threads are spawned.
    #[cfg(target_os = "linux")]
    unsafe { std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1") };

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_store::Builder::new().build())
        .setup(|app| {
            setup_menu(app.handle())?;
            blueprint::start_watcher(app.handle());
            app.manage(overlay::OverlayState::new());
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
            blueprint::blueprint_exists,
            blueprint::list_blueprints,
            blueprint::generate_thumbnail,
            blueprint::delete_blueprint,
            blueprint::rename_blueprint,
            blueprint::open_blueprint_folder,
            settings::get_settings,
            settings::set_settings,
            settings::pick_folder,
            overlay::pick_kmz_file,
            overlay::add_overlay,
            overlay::remove_overlay,
            overlay::get_overlay_status,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
