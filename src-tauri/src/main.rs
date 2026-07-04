#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod arcgis;
mod blueprint;
mod import;
mod mbtiles;
mod overlay;
mod overpass;
mod settings;
mod tile_server;
mod wms;
mod wmts;

use tauri::menu::{MenuBuilder, SubmenuBuilder, MenuItemBuilder};
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

async fn check_for_updates_on_startup(app: tauri::AppHandle) {
    use tauri_plugin_updater::UpdaterExt;

    if !settings::load(&app).check_for_updates {
        return;
    }
    let Ok(updater) = app.updater() else { return };
    let Ok(Some(update)) = updater.check().await else { return };
    prompt_install(&app, update).await;
}

async fn check_for_updates(app: &tauri::AppHandle) {
    use tauri_plugin_dialog::DialogExt;
    use tauri_plugin_updater::UpdaterExt;

    let updater = match app.updater() {
        Ok(u) => u,
        Err(e) => {
            app.dialog().message(format!("Updater unavailable: {e}")).title("Update Error").blocking_show();
            return;
        }
    };
    let update = match updater.check().await {
        Ok(Some(update)) => update,
        Ok(None) => {
            app.dialog().message("You're running the latest version.").title("No Updates Available").blocking_show();
            return;
        }
        Err(e) => {
            app.dialog().message(format!("Failed to check for updates: {e}")).title("Update Error").blocking_show();
            return;
        }
    };
    prompt_install(app, update).await;
}

async fn prompt_install(app: &tauri::AppHandle, update: tauri_plugin_updater::Update) {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};

    let version = update.version.clone();
    let confirmed = app.dialog()
        .message(format!("Version {version} is available. Download and install?"))
        .title("Update Available")
        .buttons(MessageDialogButtons::OkCancelCustom("Install & Restart".into(), "Later".into()))
        .blocking_show();

    if !confirmed {
        return;
    }

    if let Err(e) = update.download_and_install(|_, _| {}, || {}).await {
        app.dialog().message(format!("Update failed: {e}")).title("Update Error").blocking_show();
        return;
    }
    app.restart();
}

fn open_settings_window(app: &tauri::AppHandle) {
    if let Some(window) = app.webview_windows().get("settings") {
        let _ = window.set_focus();
        return;
    }
    let mut builder = WebviewWindowBuilder::new(app, "settings", WebviewUrl::App("index.html".into()))
        .title("Settings")
        .inner_size(480.0, 600.0);

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
            &MenuItemBuilder::new("Check for Updates...")
                .id("check_updates")
                .build(app)?
        )
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
            app.manage(mbtiles::DownloadState::new());
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(check_for_updates_on_startup(handle));
            Ok(())
        })
        .on_menu_event(|app, event| {
            if event.id() == "settings" {
                open_settings_window(app);
            } else if event.id() == "check_updates" {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    check_for_updates(&app).await;
                });
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
            overlay::restore_overlays,
            overlay::pick_kmz_file,
            overlay::create_group,
            overlay::remove_group,
            overlay::reorder_group,
            overlay::rename_group,
            overlay::rename_layer,
            overlay::set_group_visible,
            overlay::add_overlay,
            overlay::fetch_wms_layers,
            overlay::add_wms_layer,
            overlay::fetch_arcgis_services,
            overlay::add_arcgis_layer,
            overlay::add_xyz_layer,
            overlay::add_mbtiles_layer,
            overlay::update_apple_urls,
            overlay::fetch_wmts_layers,
            overlay::move_layer,
            overlay::remove_overlay,
            overlay::reorder_layer,
            overlay::set_layer_visible,
            overlay::set_layer_opacity,
            overlay::get_overlay_status,
            mbtiles::count_tiles,
            mbtiles::start_tile_download,
            mbtiles::cancel_tile_download,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
