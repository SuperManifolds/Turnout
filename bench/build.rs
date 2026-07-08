//! Links the Windows system libraries that maplibre-native's vendored `libuv`
//! and `icu` reference (registry, COM, shell, IP helper, sockets). The full
//! Tauri app pulls these in transitively via wry/windows-rs; this standalone
//! benchmark binary does not, so it must request them explicitly.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        for lib in [
            "advapi32", "ole32", "shell32", "iphlpapi", "user32", "userenv",
            "ws2_32", "dbghelp", "psapi", "secur32", "crypt32",
        ] {
            println!("cargo:rustc-link-lib=dylib={lib}");
        }
    }
}
