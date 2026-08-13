//! Automatic crash reporting via Sentry.
//!
//! Captures three classes of failure and uploads them:
//! - **Native crashes** (segfaults, aborts — e.g. an FFI blow-up in the ORM
//!   renderer during startup) via `sentry-rust-minidump`, which re-execs this
//!   binary as a separate crash-reporter process so it outlives the crash.
//! - **Rust panics**, via Sentry's default panic integration.
//! - **Frontend/webview errors**, from the `@sentry/browser` bundle that
//!   `tauri-plugin-sentry` injects into every webview (see [`main`]).
//!
//! The DSN is baked at build time from `TURNOUT_SENTRY_DSN`. Without it — every
//! local dev build — reporting is a no-op. Users can opt out in Settings; that
//! flag is read straight from the on-disk settings store here, before Tauri (and
//! its store plugin) starts, so nothing is sent when the user has disabled it.

/// Sentry ingest endpoint, baked at build time. `None` (or empty) in builds
/// without `TURNOUT_SENTRY_DSN` set — all local dev builds — disabling reporting.
const DSN: Option<&str> = option_env!("TURNOUT_SENTRY_DSN");

/// Settings key gating crash reporting. Absent or `true` means enabled: this is
/// opt-out, so a first-launch crash still reports before the user sees the toggle.
const CRASH_REPORTING_KEY: &str = "crash_reporting";

/// Settings key holding the preferred GPU adapter, attached to events as a tag.
const GPU_ADAPTER_KEY: &str = "gpu_adapter";

/// Settings key holding the app version last seen on disk. Compared against the
/// running version at startup to tell a fresh install from an in-place update.
const LAST_VERSION_KEY: &str = "last_seen_version";

/// Settings key holding how many times the app has launched (with reporting on).
const RUN_COUNT_KEY: &str = "run_count";

/// The running app version (`tauri.conf.json` is kept in sync with this at
/// release time). `sentry::release_name!` also carries it, but the tags below let
/// a report be sliced by *transition* — did this crash only start after an update.
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The GPU backend mbgl (the ORM renderer) is compiled against — Metal on macOS,
/// Vulkan elsewhere, matching `maplibre_native`'s default features. Update this if
/// the crate's backend feature changes.
#[cfg(target_os = "macos")]
const RENDER_BACKEND: &str = "metal";
#[cfg(not(target_os = "macos"))]
const RENDER_BACKEND: &str = "vulkan";

/// Attach the machine's GPU details and the render backend to the Sentry scope, so
/// every event — including native minidumps — records what hardware is present and
/// which backend mbgl is trying to use. No adapter (`gpu = "none"`) is itself the
/// signal for the Vulkan-init crash. Runs before the minidump fork so native
/// crashes carry it.
/// Attach system resources to the Sentry scope: RAM, CPU, and free disk space.
/// Low disk (a near-full drive) and low RAM cause a wide range of otherwise
/// baffling failures — including graphics/driver init — so this helps far beyond
/// the GPU case. Runs before the minidump fork so native crashes carry it.
fn attach_system_scope() {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    sys.refresh_cpu_all();

    let bytes_to_mb = |b: u64| b / (1024 * 1024);
    let cpu = sys.cpus().first().map(|c| c.brand().trim().to_string()).filter(|s| !s.is_empty());

    // Free space on the drive holding our tile cache (writing there fails when
    // it's full) and the smallest free across all drives (a near-full system).
    let cache_dir = dirs_next::cache_dir();
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let (mut min_free, mut cache_free, mut best_prefix) = (None::<u64>, None::<u64>, 0usize);
    for disk in disks.list() {
        let free = disk.available_space();
        min_free = Some(min_free.map_or(free, |m: u64| m.min(free)));
        let mount = disk.mount_point();
        if let Some(dir) = &cache_dir
            && dir.starts_with(mount)
            && mount.as_os_str().len() >= best_prefix
        {
            best_prefix = mount.as_os_str().len();
            cache_free = Some(free);
        }
    }

    sentry::configure_scope(|scope| {
        scope.set_tag("ram_total_mb", bytes_to_mb(sys.total_memory()));
        scope.set_tag("ram_available_mb", bytes_to_mb(sys.available_memory()));
        scope.set_tag("cpu_logical", sys.cpus().len());
        if let Some(cpu) = &cpu {
            scope.set_tag("cpu", cpu.as_str());
        }
        if let Some(physical) = sysinfo::System::physical_core_count() {
            scope.set_tag("cpu_physical", physical);
        }
        if let Some(free) = cache_free {
            scope.set_tag("disk_free_mb", bytes_to_mb(free));
        }
        if let Some(free) = min_free {
            scope.set_tag("disk_min_free_mb", bytes_to_mb(free));
        }
    });
}

fn attach_gpu_scope(store: Option<&serde_json::Value>) {
    let gpus = crate::gpu::list_gpus();
    let primary = gpus.first();

    // The adapter the user pinned in Settings, and (resolved from the live list)
    // the backend it renders through.
    let selected = store
        .and_then(|s| s.get(GPU_ADAPTER_KEY))
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty());
    let selected_backend = selected
        .and_then(|name| gpus.iter().find(|g| g.name == name))
        .map(|g| g.backend.clone());

    // Whether the backend mbgl actually renders with is usable. This is the
    // decisive tag: a machine can list a GPU via DX12 (in `gpu` below) while its
    // Vulkan runtime is missing/broken — the exact case that crashes mbgl. macOS
    // (Metal) is always ok.
    let render_ok = if RENDER_BACKEND == "metal" {
        gpus.iter().any(|g| g.backend == "Metal")
    } else {
        gpus.iter().any(|g| g.backend == "Vulkan")
    };

    // The precise state of the Vulkan loader mbgl loads (missing / broken / ok)
    // and the path that resolves — distinguishes a shadowing `vulkan-1.dll`.
    let loader = crate::vulkan::probe_loader();

    // Registered Vulkan drivers (ICDs) and implicit layers: no ICD explains "no
    // Vulkan" on capable hardware; a stale/broken implicit layer explains a crash.
    let (icds, layers) = crate::vulkan::registry_summary();

    // Every adapter, so a hybrid (e.g. NVIDIA dGPU + AMD iGPU) laptop is visible.
    let all_gpus = gpus
        .iter()
        .map(|g| format!("{} [{}/{}]", g.name, g.backend, g.kind))
        .collect::<Vec<_>>()
        .join(" | ");

    // A remote-desktop session has no real display adapter (a common no-Vulkan
    // cause). Windows sets SESSIONNAME to `RDP-Tcp#N` under RDP, `Console` locally.
    let remote_session = std::env::var("SESSIONNAME").is_ok_and(|s| s.starts_with("RDP-"));

    sentry::configure_scope(|scope| {
        scope.set_tag("render_backend", RENDER_BACKEND);
        scope.set_tag("render_backend_ok", if render_ok { "yes" } else { "no" });
        scope.set_tag("vulkan_loader", loader.tag());
        if let Some(path) = &loader.path {
            scope.set_tag("vulkan_loader_path", path.as_str());
        }
        scope.set_tag("vulkan_icds", if icds.is_empty() { "none".to_string() } else { icds.join(",") });
        if !layers.is_empty() {
            scope.set_tag("vulkan_layers", layers.join(","));
        }
        scope.set_tag("remote_session", if remote_session { "yes" } else { "no" });
        if !all_gpus.is_empty() {
            scope.set_tag("gpus", all_gpus.as_str());
        }
        scope.set_tag("gpu", primary.map_or("none", |g| g.name.as_str()));
        scope.set_tag(
            "gpu_backend",
            primary.map_or("none", |g| g.backend.as_str()),
        );
        scope.set_tag("gpu_type", primary.map_or("none", |g| g.kind.as_str()));
        scope.set_tag("gpu_count", gpus.len());
        if let Some(name) = selected {
            scope.set_tag("gpu_adapter", name);
        }
        if let Some(backend) = &selected_backend {
            scope.set_tag("gpu_selected_backend", backend.as_str());
        }
        if let Some(g) = primary {
            scope.set_context(
                "gpu",
                sentry::protocol::GpuContext {
                    name: g.name.clone(),
                    api_type: Some(g.backend.clone()),
                    vendor_name: g.vendor.clone(),
                    driver_version: g.driver.clone(),
                    ..Default::default()
                },
            );
        }
    });
}

/// Process start, stamped the first time [`attach_uptime_processor`] runs. Lets
/// every event carry an `uptime_s` tag, which separates startup crashes (init,
/// GPU, config) from steady-state ones (leaks, the tile pipeline).
static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// Tag events with the install/update transition, derived from the version last
/// seen on disk versus the running one. Read-only: it runs in both the app and
/// the re-exec'd reporter (before the fork), so native crashes carry it too; the
/// new version is written once, later, by [`record_launch`] in the app process.
///
/// `install_kind` is `fresh` (no prior version — first launch), `updated` (a
/// different prior version — the prime "did the release do it?" signal, paired
/// with `updated_from`), or `same`. `run_count` includes the current launch.
fn attach_release_scope(store: Option<&serde_json::Value>) {
    let last = store
        .and_then(|s| s.get(LAST_VERSION_KEY))
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty());
    let run_count = store
        .and_then(|s| s.get(RUN_COUNT_KEY))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let install_kind = match last {
        None => "fresh",
        Some(v) if v == APP_VERSION => "same",
        Some(_) => "updated",
    };

    sentry::configure_scope(|scope| {
        scope.set_tag("install_kind", install_kind);
        scope.set_tag("run_count", run_count + 1);
        if install_kind == "updated"
            && let Some(prev) = last
        {
            scope.set_tag("updated_from", prev);
        }
    });
}

/// Add a scope event processor that stamps every outgoing event with seconds
/// since process start. Covers Rust panics and `error!` events (native minidumps
/// are uploaded by the reporter process and carry their own timestamps instead).
fn attach_uptime_processor() {
    START.get_or_init(std::time::Instant::now);
    sentry::configure_scope(|scope| {
        scope.add_event_processor(|mut event| {
            if let Some(start) = START.get() {
                event
                    .tags
                    .insert("uptime_s".to_string(), start.elapsed().as_secs().to_string());
            }
            Some(event)
        });
    });
}

/// Persist the running version and bump the launch counter, so the next startup
/// can detect an update and report the run count. Uses the Tauri store plugin, so
/// it must run in the app process **after** that plugin is initialised (the setup
/// hook), not in [`init`] which predates Tauri. Best-effort: a store failure just
/// means the next launch sees a slightly stale fingerprint.
pub fn record_launch(app: &tauri::AppHandle) {
    use tauri_plugin_store::StoreExt;
    let Ok(store) = app.store(crate::settings::SETTINGS_STORE) else {
        return;
    };
    let prev = store.get(RUN_COUNT_KEY).and_then(|v| v.as_u64()).unwrap_or(0);
    store.set(RUN_COUNT_KEY, serde_json::json!(prev + 1));
    store.set(LAST_VERSION_KEY, serde_json::json!(APP_VERSION));
    let _ = store.save();
}

/// Whether the store permits crash reporting. Absent key, absent store, a
/// non-bool value, or `true` all mean enabled (opt-out, fail-open) so a
/// first-launch or corrupt-store crash still reports.
fn reporting_enabled(store: Option<&serde_json::Value>) -> bool {
    store
        .and_then(|s| s.get(CRASH_REPORTING_KEY))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true)
}

/// Initialise the Sentry client, or return `None` when reporting is disabled (no
/// DSN baked, an unparseable DSN, or the user opted out).
///
/// The returned guard must be kept alive for the whole program: dropping it
/// flushes and shuts down the transport. Runs in **both** the app and the
/// re-exec'd crash-reporter process (up to `minidump::init`), so both agree on
/// the opt-out and tags. `sentry::init` does spawn a background transport thread.
pub fn init() -> Option<sentry::ClientInitGuard> {
    let dsn = DSN.filter(|dsn| !dsn.trim().is_empty())?;
    let store = crate::settings::read_store_from_disk();
    if !reporting_enabled(store.as_ref()) {
        return None;
    }
    // Parse the DSN ourselves — `sentry::init` panics on a malformed one, which
    // would be a worse startup crash than the ones we're trying to capture.
    let dsn: sentry::types::Dsn = match dsn.trim().parse() {
        Ok(dsn) => dsn,
        Err(err) => {
            tracing::warn!("invalid Sentry DSN; crash reporting disabled: {err}");
            return None;
        }
    };
    let guard = sentry::init((
        dsn,
        sentry::ClientOptions {
            release: sentry::release_name!(),
            // Set by the release/nightly workflows ("production"/"nightly") so
            // nightly crashes don't pollute shipped-version stats. Unset → None.
            environment: option_env!("TURNOUT_SENTRY_ENVIRONMENT").map(std::borrow::Cow::Borrowed),
            // Release health: one session per app run, so the dashboard reports
            // crash-free-session/user rates and per-release adoption.
            auto_session_tracking: true,
            session_mode: sentry::SessionMode::Application,
            // Forward `tracing` events as structured logs (see main's subscriber).
            enable_logs: true,
            // Attach a stack trace to messages/logs, not just captured errors.
            attach_stacktrace: true,
            // Crash diagnostics only — no IP addresses or request headers.
            send_default_pii: false,
            ..Default::default()
        },
    ));
    // Attach GPU details so render/startup crashes can be sliced by hardware — the
    // prime suspect (the mbgl Vulkan-init crash happens precisely when no adapter
    // is found). Set before `minidump::init` forks (both processes run this), so
    // native-crash events carry it too. Device/OS come free via `contexts`.
    attach_gpu_scope(store.as_ref());
    attach_system_scope();
    attach_release_scope(store.as_ref());
    attach_uptime_processor();
    Some(guard)
}

/// The flag `minidumper-child` appends to the re-exec'd crash-reporter process
/// (its default `server_arg`). Its presence is how that crate — and we — tell the
/// reporter process apart from the main app process.
const CRASH_REPORTER_SERVER_ARG: &str = "--crash-reporter-server";

/// How many times the **main** process tries to start the native reporter before
/// giving up. Each attempt spawns a fresh reporter with a new socket name, so a
/// transient bind collision (`EADDRINUSE`, os error 10048) with a leftover socket
/// file resolves on the next try.
const MINIDUMP_INIT_ATTEMPTS: u32 = 3;

/// Delay between reporter start attempts.
const MINIDUMP_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(300);

/// Only leftover reporter socket files older than this are swept, so a live
/// reporter's freshly created socket is never removed.
const REPORTER_SOCKET_MAX_AGE: std::time::Duration = std::time::Duration::from_hours(1);

/// Whether this process is the re-exec'd crash-reporter server (it carries
/// minidumper-child's server flag) rather than the main app process. Mirrors
/// `minidumper_child`'s own argv check.
fn is_crash_reporter_process() -> bool {
    std::env::args().any(|arg| arg.starts_with(CRASH_REPORTER_SERVER_ARG))
}

/// Name of a leftover reporter IPC socket file (minidumper's `temp-socket-<uuid>`).
fn is_reporter_socket_name(name: &str) -> bool {
    name.starts_with("temp-socket-")
}

/// Remove leftover crash-reporter IPC socket files from the temp dir. On
/// Windows/macOS minidumper binds an `AF_UNIX` socket at `<temp>/temp-socket-<uuid>`
/// and never deletes it, so they accumulate; a leftover file also makes a
/// reporter's `bind` fail with `EADDRINUSE`. Only files older than
/// [`REPORTER_SOCKET_MAX_AGE`] are removed, so a live reporter's socket is left
/// alone. Best-effort — errors are ignored. (Linux uses abstract sockets, so its
/// temp dir has none of these and this is a no-op there.)
fn sweep_stale_reporter_sockets() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        if !entry.file_name().to_str().is_some_and(is_reporter_socket_name) {
            continue;
        }
        let too_old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > REPORTER_SOCKET_MAX_AGE);
        if too_old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Arms the native minidump handler: a re-exec'd reporter process that outlives a
/// crash of the main process and uploads a minidump. Returns the handle, which
/// **must be kept alive for the whole program** — dropping it stops the reporter.
///
/// `None` when reporting is off (no Sentry client) *or* the reporter failed to
/// start after [`MINIDUMP_INIT_ATTEMPTS`]. The reporter's IPC socket bind can hit
/// `EADDRINUSE` (a leftover socket file); the main process sweeps stale sockets
/// and retries with a fresh reporter to recover. A final failure is logged at
/// `error!` so it becomes a Sentry event in its own right: without this we can't
/// tell a machine where native capture is silently unavailable from one that
/// simply never crashed. Only covers the **main** process — a `WebView2` (Windows)
/// or `WKWebView` renderer crash is a separate process and is not caught here (see
/// the macOS-only `on_web_content_process_terminate` hook in `main`).
pub fn arm_minidump_reporter(
    client: Option<&sentry::ClientInitGuard>,
) -> Option<tauri_plugin_sentry::minidump::Handle> {
    let client = client?;
    let reporter_process = is_crash_reporter_process();

    // The reporter process gets a single attempt — its socket name is fixed by the
    // parent's flag, so a retry would only re-bind the same failing name. The main
    // process sweeps stale sockets once, then retries with fresh reporters.
    let attempts = if reporter_process {
        1
    } else {
        sweep_stale_reporter_sockets();
        MINIDUMP_INIT_ATTEMPTS
    };

    for attempt in 1..=attempts {
        match tauri_plugin_sentry::minidump::init(client) {
            Ok(handle) => {
                tracing::info!("native crash reporter armed");
                return Some(handle);
            }
            Err(err) if attempt < attempts => {
                tracing::warn!(
                    "native crash reporter start attempt {attempt}/{attempts} failed: {err}; retrying"
                );
                std::thread::sleep(MINIDUMP_RETRY_DELAY);
            }
            Err(err) => {
                tracing::error!(
                    "native crash reporter failed to start; native crashes will not be captured: {err}"
                );
                // In the reporter process a failed start means minidumper-child
                // returned before its own `exit(0)`, so `main` would go on to run a
                // second, half-initialised copy of the app. Exit instead.
                if reporter_process {
                    std::process::exit(1);
                }
                return None;
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{is_reporter_socket_name, reporting_enabled};
    use serde_json::json;

    #[test]
    fn reporter_socket_names_are_recognized() {
        assert!(is_reporter_socket_name(
            "temp-socket-2f1a9c0b4d5e4f6a8b7c1d2e3f405162"
        ));
        // Don't sweep unrelated temp files.
        assert!(!is_reporter_socket_name("orm-bench-cache.db"));
        assert!(!is_reporter_socket_name("settings.json"));
        assert!(!is_reporter_socket_name(""));
    }

    #[test]
    fn opt_out_is_honored() {
        let store = json!({ "crash_reporting": false });
        assert!(!reporting_enabled(Some(&store)));
    }

    #[test]
    fn enabled_by_default() {
        // Absent store (fresh install), absent key, and explicit true all enable.
        assert!(reporting_enabled(None));
        assert!(reporting_enabled(Some(&json!({}))));
        assert!(reporting_enabled(Some(&json!({ "crash_reporting": true }))));
    }

    #[test]
    fn malformed_store_fails_open() {
        // A non-bool value or a non-object store must not silently disable
        // reporting (fail-open), matching the disk-read fallback.
        assert!(reporting_enabled(Some(
            &json!({ "crash_reporting": "false" })
        )));
        assert!(reporting_enabled(Some(&json!([1, 2, 3]))));
    }
}
