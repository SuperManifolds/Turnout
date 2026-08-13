#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod apple_token;
mod arcgis;
mod blueprint;
mod cartometro;
mod crash_reporting;
mod error;
mod gpu;
mod orm_import;
mod mbtiles;
mod nimby_launch;
mod orm_net;
mod orm_offline;
mod orm_tiles;
mod overlay;
mod overpass;
mod server_core;
mod settings;
mod tile_server;
mod vector_tiles;
mod vulkan;
mod wms;
mod wmts;

use tauri::menu::{MenuBuilder, SubmenuBuilder, MenuItemBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

/// Show, unminimise, and focus the main window (from the tray icon or a macOS
/// dock reopen).
fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// System-tray icon so closing the window keeps Turnout — and its tile servers —
/// running in the background. NIMBY Rails can then reach the overlay sources
/// regardless of launch order. Quit fully exits, stopping the servers.
fn setup_tray(app: &tauri::AppHandle) -> tauri::Result<()> {
    let show = MenuItemBuilder::new("Show Turnout").id("tray_show").build(app)?;
    let quit = MenuItemBuilder::new("Quit Turnout").id("tray_quit").build(app)?;
    let menu = MenuBuilder::new(app).items(&[&show, &quit]).build()?;
    TrayIconBuilder::with_id("main")
        .icon(app.default_window_icon().cloned().expect("bundled window icon"))
        .tooltip("Turnout — tile server running")
        .menu(&menu)
        // Left-click shows the window (handled below); right-click opens the menu.
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "tray_show" => show_main_window(app),
            "tray_quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

async fn check_for_updates_on_startup(app: tauri::AppHandle) {
    use tauri_plugin_updater::UpdaterExt;

    if !settings::load(&app).network.check_for_updates {
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

    tracing::info!(%version, "installing update");
    if let Err(e) = update.download_and_install(|_, _| {}, || {}).await {
        // error! so a failed auto-update surfaces as a Sentry issue, not just a
        // dialog the user dismisses — update breakage is otherwise invisible.
        tracing::error!("update to {version} failed: {e}");
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

/// Dependency log targets that emit transient or benign operational noise — ORM
/// tile-fetch failures, the absent optional Vulkan validation layer, update-check
/// network blips — rather than actionable bugs. Their events become Sentry
/// breadcrumbs (context on a real crash) instead of an issue per line, which would
/// bury genuine panics and burn the quota.
const NOISY_LOG_TARGETS: &[&str] = &["maplibre_native", "tauri_plugin_updater"];

fn is_noisy_target(target: &str) -> bool {
    NOISY_LOG_TARGETS.iter().any(|prefix| target.starts_with(prefix))
}

/// Pulls the real target out of a `log`-crate record. These reach us via
/// `tracing-log`, which gives the bridged event a generic `metadata().target()`
/// of `"log"` and stashes the true target (e.g. `maplibre_native::bridge`) in a
/// `log.target` field — so a check on the metadata target alone never matches
/// them. maplibre-native and `tauri_plugin_updater` both log via the `log` crate.
struct LogTargetVisitor(Option<String>);

impl tracing::field::Visit for LogTargetVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "log.target" {
            self.0 = Some(value.to_string());
        }
    }

    fn record_debug(&mut self, _field: &tracing::field::Field, _value: &dyn std::fmt::Debug) {}
}

/// Maps `tracing` events to Sentry. Mirrors the default (`error!` → issue,
/// `warn!`/`info!` → breadcrumb, lower → ignore), except events from
/// [`NOISY_LOG_TARGETS`] always become breadcrumbs. A mapper (not an
/// `event_filter`) is required because the filter only sees `Metadata`, whose
/// target is `"log"` for the `log`-bridged records these noisy deps actually emit.
fn sentry_event_mapper<S>(
    event: &tracing::Event<'_>,
    _ctx: tracing_subscriber::layer::Context<'_, S>,
) -> sentry::integrations::tracing::EventMapping
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    use sentry::integrations::tracing::{breadcrumb_from_event, event_from_event, EventMapping};
    use tracing_subscriber::layer::Context;

    let mut visitor = LogTargetVisitor(None);
    event.record(&mut visitor);
    let target = visitor.0.as_deref().unwrap_or_else(|| event.metadata().target());

    let no_ctx = None::<&Context<'_, S>>;
    if is_noisy_target(target) {
        return EventMapping::Breadcrumb(breadcrumb_from_event(event, no_ctx));
    }
    match *event.metadata().level() {
        tracing::Level::ERROR => EventMapping::Event(event_from_event(event, no_ctx)),
        tracing::Level::WARN | tracing::Level::INFO => {
            EventMapping::Breadcrumb(breadcrumb_from_event(event, no_ctx))
        }
        tracing::Level::DEBUG | tracing::Level::TRACE => EventMapping::Ignore,
    }
}

fn main() {
    // Leveled logging to stderr; RUST_LOG overrides the default (info and above).
    // The Sentry layer turns these events into breadcrumbs (the trail before a
    // crash) and forwards `error!` as issues + structured logs. It no-ops until a
    // Sentry client is bound below, and stays inert entirely when reporting is off.
    use tracing_subscriber::prelude::*;
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with(tracing_subscriber::fmt::layer())
        .with(sentry::integrations::tracing::layer().event_mapper(sentry_event_mapper))
        .init();

    // Work around WebKitGTK EGL crashes on some Linux GPU drivers.
    // SAFETY: Called at the very start of main, before any threads are spawned.
    #[cfg(target_os = "linux")]
    unsafe { std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1") };

    // Size mbgl's shared background pool (vector tile layout for all ORM render
    // workers) to the machine instead of its 4-thread default, which is the tile
    // throughput ceiling. Half the logical cores, capped at 8 — more showed no
    // gain in benchmarks. An explicit MLN_BACKGROUND_THREADS wins.
    // SAFETY: Called at the very start of main, before any threads are spawned.
    if std::env::var_os("MLN_BACKGROUND_THREADS").is_none() {
        let cores = std::thread::available_parallelism().map_or(8, std::num::NonZero::get);
        let threads = (cores / 2).clamp(4, 8);
        unsafe { std::env::set_var("MLN_BACKGROUND_THREADS", threads.to_string()) };
    }

    // Pin the ORM renderer to the user's chosen GPU, read by the fork's Vulkan
    // device-selection patch (MLN_VULKAN_DEVICE_NAME). A no-op on Metal (macOS
    // exposes one device). Read straight off disk, since the Tauri store plugin
    // isn't up yet.
    // SAFETY: edition-2024 makes `set_var` unsound only against a concurrent
    // `getenv` on another thread. This runs on the main thread before any thread
    // is spawned — in particular before `crash_reporting::init` starts Sentry's
    // transport thread, and before the app's own threads — so nothing can race it.
    if let Some(name) = settings::stored_gpu_adapter() {
        unsafe { std::env::set_var("MLN_VULKAN_DEVICE_NAME", name); }
    }

    // Crash reporting must init before the Tauri builder: the minidump handler
    // re-execs this binary as a separate crash-reporter process, and everything
    // up to `minidump::init` runs in both the app and reporter processes. Both
    // guards must live until the app exits — dropping them stops the reporter and
    // flushes Sentry. `None` when reporting is disabled (see `crash_reporting`).
    let sentry_guard = crash_reporting::init();
    let _minidump_guard = crash_reporting::arm_minidump_reporter(sentry_guard.as_ref());

    // single-instance must be the first plugin registered. A second launch (e.g.
    // from the taskbar while Turnout is resident in the tray) focuses the existing
    // window instead of starting a duplicate — a duplicate can't bind the fixed
    // tile-server ports the first instance already holds, so it falls back to
    // random ports and breaks NIMBY Rails' saved overlay URLs.
    let mut builder = tauri::Builder::default().plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
        show_main_window(app);
    }));
    if let Some(client) = &sentry_guard {
        // Enriches webview errors with Rust/OS context and merges breadcrumbs
        // across the Rust and browser SDKs.
        builder = builder.plugin(tauri_plugin_sentry::init(client));
    }
    // The webview renders in a separate content process; when it dies the window
    // goes blank with no Rust panic and no minidump (the minidump handler covers
    // only the main process, and `@sentry/browser` only sees JS errors, not a
    // renderer crash). Capture it as an error so it surfaces in Sentry. Tauri only
    // exposes this on macos/ios — a WebView2 renderer crash on Windows has no
    // equivalent hook, so that blind spot remains (tracked separately).
    #[cfg(target_os = "macos")]
    {
        builder = builder.on_web_content_process_terminate(|webview| {
            tracing::error!("webview content process terminated: {}", webview.label());
        });
    }
    builder
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_store::Builder::new().build())
        .setup(|app| {
            setup_menu(app.handle())?;
            setup_tray(app.handle())?;
            // Persist this launch's version + run count for the next startup's
            // install/update fingerprint (see crash_reporting::attach_release_scope).
            crash_reporting::record_launch(app.handle());
            blueprint::start_watcher(app.handle());
            app.manage(overlay::OverlayState::new());
            // Restore persisted overlays synchronously here, before the Apple-token
            // refresher is spawned below. The refresher touches live overlay state
            // at startup; restoring first makes the lifecycle deterministic (the
            // frontend then only reads via get_overlay_status, never re-restores).
            tauri::async_runtime::block_on(overlay::restore_overlays(app.handle().clone()));
            app.manage(mbtiles::DownloadState::new());
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(check_for_updates_on_startup(handle));
            let orm_availability = orm_tiles::OrmAvailability::default();
            match orm_tiles::start_blocking() {
                Ok(h) => {
                    tracing::info!("ORM tile server started");
                    let base = settings::resolve_orm_base(settings::load(app.handle()).orm_base_url.as_deref());
                    if base != settings::DEFAULT_ORM_BASE {
                        h.set_base_url(base);
                    }
                    app.manage(h);
                }
                Err(e) => {
                    tracing::error!("ORM tiles failed: {e}");
                    orm_availability.set_disabled(e.to_string());
                }
            }
            app.manage(orm_availability);
            app.manage(apple_token::AppleRefresh::new());
            apple_token::spawn_auto_refresh(app.handle().clone());
            app.manage(vector_tiles::VectorLayerState::default());
            app.manage(cartometro::CartoMetroState::default());
            tauri::async_runtime::spawn(cartometro::autostart(app.handle().clone()));
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
        .on_window_event(|window, event| {
            // Closing the main window hides it to the tray, keeping the tile
            // servers alive for NIMBY Rails; the tray's Quit item exits for real.
            // The settings window closes normally.
            if window.label() == "main"
                && let tauri::WindowEvent::CloseRequested { api, .. } = event
            {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            gpu::list_gpu_adapters,
            overpass::fetch_overpass,
            orm_import::import_orm,
            orm_import::count_track_nodes,
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
            settings::open_external_url,
            settings::replay_tutorial,
            nimby_launch::nimby_launch_setup,
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
            apple_token::refresh_apple_token,
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
            mbtiles::set_tile_download_paused,
            orm_offline::download_orm_tiles,
            orm_tiles::set_orm_offline,
            orm_tiles::get_orm_port,
            orm_tiles::orm_disabled_reason,
            vector_tiles::start_orm_vector_layers,
            vector_tiles::stop_orm_vector_layers,
            vector_tiles::open_workshop_mod,
            cartometro::start_cartometro,
            cartometro::stop_cartometro,
        ])
        .build(tauri::generate_context!())
        .expect("error while running tauri application")
        .run(|app, event| {
            // macOS: clicking the dock icon while the window is hidden reopens it.
            // `Reopen` is a macOS-only `RunEvent` variant.
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { .. } = event {
                show_main_window(app);
            }
            #[cfg(not(target_os = "macos"))]
            let _ = (&app, &event);
        });
}

#[cfg(test)]
mod tests {
    use super::is_noisy_target;

    #[test]
    fn noisy_targets_match_by_prefix() {
        // The real `log.target` values these deps emit (sub-modules included).
        assert!(is_noisy_target("maplibre_native::bridge"));
        assert!(is_noisy_target("maplibre_native"));
        assert!(is_noisy_target("tauri_plugin_updater::updater"));
    }

    #[test]
    fn our_own_and_unknown_targets_are_not_noise() {
        // Our instrumented errors and the generic bridged target stay issues.
        assert!(!is_noisy_target("turnout_tauri::orm_tiles"));
        assert!(!is_noisy_target("turnout_core"));
        assert!(!is_noisy_target("log"));
        assert!(!is_noisy_target("reqwest"));
    }
}
