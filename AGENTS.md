# Claude Code Preferences

## Git Commits

- Write clear, professional commit messages that focus on what changed and why
- Use conventional commit style when appropriate

## Communication Style

- Be concise and direct
- Avoid unnecessary preamble or postamble

## Code Changes

- Always run `cargo check` and `cargo clippy` after making code changes (cargo clippy --all-targets -- -D warnings)
- When asked to commit, create a clear commit message without asking for confirmation
- Focus on the technical implementation rather than over-explaining what was done
- When implementing a new utility function make sure it is not already implemented elsewhere in the code base

## Rust Code Quality

- Avoid excessive nesting in functions (prefer early returns, extract helper functions)
- Do not silence clippy warnings without expressed consent, address the problem instead
- Keep functions small and focused on a single responsibility
- Follow Rust naming conventions and idiomatic patterns
- Structure the project according to Rust conventions (proper module organization, appropriate use of traits, etc.)
- Address clippy warnings and suggestions when they improve code quality. Do not attempt to silence a lint warning without asking.
- When making a change to existing code that will negatively affect time complexity you must request permission.
- Use constants at the top of the file for magic numbers or layout and style choices like color, width, spacing, etc.
- Prefer declarative over imperative code when sensible (use iterators, functional patterns, etc.)
- Avoid unnecessary suffixes to files or structs like 'view', 'component', 'manager', etc.
- Do not create a new version of an existing function if it makes the old function redundant, just modify the existing function
- Do not use `_` prefixes or `#[allow(dead_code)]` to silence unused code warnings - just remove code that is no longer used

## Verifying UI changes yourself

Frontend changes (Leptos CSR — anything under `web/src`, `style/`, `static/`)
can be verified visually WITHOUT the ~30-min Tauri native build. Drive the
served web app in a headless browser and screenshot it:

1. `trunk serve` — serves the frontend at `http://127.0.0.1:1420/` (see
   `Trunk.toml`) and rebuilds on save. Run it in the background; check its log
   for `✅ success`.
2. Drive it with the `agent-browser` CLI (headless Chrome):
   - `agent-browser open "http://127.0.0.1:1420/" --width 1280 --height 900`
   - `agent-browser snapshot -i` → interactive elements with `@eN` refs
   - `agent-browser click @e15`, `agent-browser screenshot <abs-path>.png`
   - `agent-browser eval "<js>"` → read `getBoundingClientRect()`, computed
     styles, element presence (invaluable for positioning/overlay bugs)
   - Read the PNGs back with the Read tool to actually look at them.

Gotchas learned the hard way:
- **`__TAURI__` is absent in the browser**, so `crate::tauri::*` commands fail
  gracefully (e.g. `load_settings()` → `Settings::default()`). This is why
  first-launch/default-gated UI shows up. Backend-integration behavior still
  needs `cargo tauri dev`.
- **Screenshots need absolute paths** — the shell cwd resets between calls.
- **Refs go stale** after any reload/re-render — re-`snapshot` before clicking.
- **Never nest `agent-browser` inside `$()`** — it hangs; use sequential calls.
- If a session restores an unrelated tab, `agent-browser close --all`, kill
  `Chrome for Testing`, and `rm -rf` the stale `agent-browser-chrome-*`
  user-data-dir before reopening.
- Clean up when done: `agent-browser close --all` and stop `trunk serve`.

Driving the **real Tauri window** (`cargo tauri dev`, so `__TAURI__` and the
backend are live) is the goal for backend-connected flows, but on macOS the
webview is WKWebView — no CDP (agent-browser can't attach) and no WebDriver
(`tauri-driver` doesn't support macOS). The only scriptable path is macOS
Accessibility via `osascript`/System Events (read window bounds → `screencapture
-R`, click via AX), which needs a one-time **Accessibility permission** grant to
the controlling terminal (System Settings → Privacy & Security → Accessibility).
Without that grant it fails with `-25211 not allowed assistive access`. Until
that's set up, use the `trunk serve` + agent-browser route above; it's a faithful
proxy for layout/positioning since the DOM and CSS are identical.

## Components
- Avoid code inside components that is not directly related to UI. Model code should go in core/
- Avoid large amount of function code inside event handlers, extract into functions
- Prefer HTML5 semantic tags like <header> and <section> avoid div soup
- Never use style tags except to apply a temporary reactive effect like moving an element
