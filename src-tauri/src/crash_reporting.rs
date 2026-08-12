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

    sentry::configure_scope(|scope| {
        scope.set_tag("render_backend", RENDER_BACKEND);
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
    Some(guard)
}

/// Arms the native minidump handler: a re-exec'd reporter process that outlives a
/// crash of the main process and uploads a minidump. Returns the handle, which
/// **must be kept alive for the whole program** — dropping it stops the reporter.
///
/// `None` when reporting is off (no Sentry client) *or* the reporter failed to
/// start. A start failure is logged at `error!` so it becomes a Sentry event in
/// its own right: without this we can't tell a machine where native capture is
/// silently unavailable (a failed re-exec, sandbox, or temp-dir permission) from
/// one that simply never crashed. Only covers the **main** process — a `WebView2`
/// (Windows) or `WKWebView` renderer crash is a separate process and is not caught
/// here (see the macOS-only `on_web_content_process_terminate` hook in `main`).
pub fn arm_minidump_reporter(
    client: Option<&sentry::ClientInitGuard>,
) -> Option<tauri_plugin_sentry::minidump::Handle> {
    let client = client?;
    match tauri_plugin_sentry::minidump::init(client) {
        Ok(handle) => {
            tracing::info!("native crash reporter armed");
            Some(handle)
        }
        Err(err) => {
            tracing::error!(
                "native crash reporter failed to start; native crashes will not be captured: {err}"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::reporting_enabled;
    use serde_json::json;

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
