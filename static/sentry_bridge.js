// Forwards wasm panics and IPC/command failures to Sentry when the
// tauri-plugin-sentry global (window.Sentry) is present. Every function is a
// no-op outside the packaged app (plain browser / dev), where window.Sentry is
// absent, and never throws.

window.__turnout_report_panic = function(msg) {
    try {
        if (window.Sentry && window.Sentry.captureException) {
            window.Sentry.captureException(new Error(msg));
        }
    } catch (e) {}
};

window.__turnout_report_error = function(context, detail) {
    try {
        if (window.Sentry && window.Sentry.captureMessage) {
            window.Sentry.captureMessage(context + ": " + detail, "error");
        }
    } catch (e) {}
};
