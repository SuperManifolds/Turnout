//! Helper for auto-launching Turnout when NIMBY Rails starts, via Steam's
//! per-game Launch Options (`%command%` wrapper). Turnout can't safely write
//! Steam's config (the client overwrites `localconfig.vdf` on exit), so instead
//! it hands the user a ready-to-paste string pre-filled with its own path — the
//! same copy-and-paste flow the tutorial uses for tile-source URLs.

use std::path::PathBuf;

/// NIMBY Rails' Steam application id (Weird and Wry).
const NIMBY_APPID: &str = "1134710";

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NimbyLaunchSetup {
    /// Target OS: `"windows"`, `"linux"`, or `"macos"`.
    pub os: &'static str,
    /// The exact string to paste into NIMBY Rails' Steam Launch Options, or
    /// `None` on macOS (NIMBY has no native macOS build) or if the path is
    /// unavailable.
    pub launch_options: Option<String>,
    /// Whether NIMBY Rails was found installed in a Steam library.
    pub nimby_detected: bool,
}

/// Candidate Steam roots across platforms and install methods.
fn steam_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = dirs_next::home_dir() {
        roots.push(home.join(".steam/steam")); // Linux (default)
        roots.push(home.join(".local/share/Steam")); // Linux (alt)
        roots.push(home.join(".var/app/com.valvesoftware.Steam/data/Steam")); // Linux (flatpak)
        roots.push(home.join("Library/Application Support/Steam")); // macOS
    }
    for var in ["ProgramFiles(x86)", "ProgramFiles"] {
        if let Some(pf) = std::env::var_os(var) {
            roots.push(PathBuf::from(pf).join("Steam")); // Windows
        }
    }
    roots
}

/// Extract library paths from a `libraryfolders.vdf` body. Each library is a
/// `"path"  "<dir>"` line; other keys are ignored.
fn library_paths(vdf: &str) -> Vec<PathBuf> {
    vdf.lines()
        .filter_map(|line| {
            let mut tokens = line.split('"').map(str::trim).filter(|s| !s.is_empty());
            if tokens.next()? != "path" {
                return None;
            }
            // Windows paths are stored with escaped separators (`C:\\Lib`).
            tokens
                .next()
                .map(|p| PathBuf::from(p.replace("\\\\", "\\")))
        })
        .collect()
}

/// True if `appmanifest_<NIMBY_APPID>.acf` exists in any known Steam library.
fn nimby_installed() -> bool {
    let manifest = format!("appmanifest_{NIMBY_APPID}.acf");
    steam_roots().into_iter().any(|root| {
        let steamapps = root.join("steamapps");
        if steamapps.join(&manifest).exists() {
            return true;
        }
        std::fs::read_to_string(steamapps.join("libraryfolders.vdf"))
            .map(|vdf| {
                library_paths(&vdf)
                    .into_iter()
                    .any(|lib| lib.join("steamapps").join(&manifest).exists())
            })
            .unwrap_or(false)
    })
}

/// Build the Launch Options string for `exe` on the current platform. On Linux
/// this writes a small wrapper script next to Turnout's config and points the
/// Launch Options at it; Steam can't reliably parse an inline `&`.
fn launch_options(exe: &str) -> Option<String> {
    if cfg!(target_os = "windows") {
        // `cmd /c start "" "<exe>" & start "" %COMMAND%` launches Turnout and
        // then the game, both from the normal Play button.
        Some(format!(
            "cmd /c start \"\" \"{exe}\" & start \"\" %COMMAND%"
        ))
    } else if cfg!(target_os = "linux") {
        write_linux_wrapper(exe).map(|script| format!("\"{}\" %command%", script.display()))
    } else {
        None // macOS: NIMBY Rails has no native build here.
    }
}

/// Write (or refresh) the Linux launch wrapper: start Turnout detached, then
/// exec the game command Steam passes in. Returns its path.
#[cfg(target_os = "linux")]
fn write_linux_wrapper(exe: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let dir = dirs_next::config_dir()?.join("io.sorlie.turnout");
    std::fs::create_dir_all(&dir).ok()?;
    let script = dir.join("launch-nimby.sh");
    std::fs::write(&script, format!("#!/bin/sh\n\"{exe}\" &\nexec \"$@\"\n")).ok()?;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).ok()?;
    Some(script)
}

#[cfg(not(target_os = "linux"))]
fn write_linux_wrapper(_exe: &str) -> Option<PathBuf> {
    None
}

#[tauri::command]
pub fn nimby_launch_setup() -> NimbyLaunchSetup {
    let os = if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "macos"
    };
    let launch_options = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_owned))
        .and_then(|exe| launch_options(&exe));
    NimbyLaunchSetup {
        os,
        launch_options,
        nimby_detected: nimby_installed(),
    }
}

#[cfg(test)]
mod tests {
    use super::library_paths;

    #[test]
    fn parses_library_paths() {
        let vdf = r#"
"libraryfolders"
{
	"0"
	{
		"path"		"/home/u/.local/share/Steam"
	}
	"1"
	{
		"path"		"/mnt/games/SteamLibrary"
		"label"		""
	}
}
"#;
        let paths = library_paths(vdf);
        assert_eq!(paths.len(), 2);
        assert!(paths.iter().any(|p| p.ends_with("SteamLibrary")));
    }
}
