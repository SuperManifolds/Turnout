//! A faithful, *safe* probe of whether mbgl's Vulkan loader path will succeed.
//!
//! mbgl loads `vulkan-1.dll` at runtime with `vk::DynamicLoader` and calls
//! `vkGetInstanceProcAddr` through it **without a null check**
//! (`renderer_backend.cpp`: `dispatcher.init(dynamicLoader)`), so a missing or
//! broken loader is an uncatchable access violation that takes the whole process
//! down. We replicate that exact load — `LoadLibrary` + `GetProcAddress` — but
//! check every result, so the app can skip ORM rendering (and report precisely
//! *why*) instead of crashing. This runs in the same process with the same DLL
//! search order mbgl uses, so it predicts mbgl's line-384 outcome faithfully,
//! including a stray `vulkan-1.dll` shadowing the real one (reported via `path`).
//!
//! The probe only loads the library and reads a symbol — it never creates an
//! instance or touches a device, so it is safe even where a broken ICD or
//! overlay layer would crash a real `vkCreateInstance`.

/// Result of loading the Vulkan runtime the way mbgl does.
pub struct LoaderProbe {
    /// Whether this platform uses the Vulkan loader at all (`false` on macOS,
    /// which renders with Metal).
    pub applicable: bool,
    /// `vulkan-1.dll` (or equivalent) loaded successfully.
    pub loaded: bool,
    /// The loaded library exposes `vkGetInstanceProcAddr` — mbgl's entry point.
    pub has_entry: bool,
    /// The full path that actually resolved, when loaded (reveals a shadowing
    /// `vulkan-1.dll` earlier in the DLL search order than `System32`).
    pub path: Option<String>,
}

impl LoaderProbe {
    /// Whether mbgl's Vulkan init can proceed without crashing.
    #[must_use]
    pub fn usable(&self) -> bool {
        !self.applicable || (self.loaded && self.has_entry)
    }

    /// Short status tag for Sentry / logs: `ok` / `broken` / `missing` / `n/a`.
    #[must_use]
    pub fn tag(&self) -> &'static str {
        if !self.applicable {
            "n/a"
        } else if !self.loaded {
            "missing"
        } else if !self.has_entry {
            "broken" // loaded but no vkGetInstanceProcAddr — corrupt/wrong runtime
        } else {
            "ok"
        }
    }

    /// A platform that renders with a backend other than the Vulkan loader.
    fn not_applicable() -> Self {
        Self { applicable: false, loaded: false, has_entry: false, path: None }
    }
}

/// Probe the Vulkan loader mbgl would use on this platform.
#[must_use]
pub fn probe_loader() -> LoaderProbe {
    #[cfg(target_os = "macos")]
    {
        // mbgl uses Metal on macOS; the Vulkan loader is irrelevant.
        LoaderProbe::not_applicable()
    }
    #[cfg(windows)]
    {
        windows_probe()
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // On Linux the wgpu adapter probe (`gpu::render_backend_available`) is the
        // guard; a dedicated dlopen probe would need extra linkage, so we defer to
        // it here.
        LoaderProbe::not_applicable()
    }
}

#[cfg(windows)]
fn windows_probe() -> LoaderProbe {
    use core::ffi::{c_char, c_void};

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LoadLibraryA(name: *const c_char) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const c_char) -> *const c_void;
        fn FreeLibrary(module: *mut c_void) -> i32;
        fn GetModuleFileNameA(module: *mut c_void, filename: *mut c_char, size: u32) -> u32;
    }

    // SAFETY: standard Win32 calls; the returned handle is null-checked and freed
    // before returning, and we only read a symbol address, never call through it.
    unsafe {
        let handle = LoadLibraryA(c"vulkan-1.dll".as_ptr());
        if handle.is_null() {
            return LoaderProbe { applicable: true, loaded: false, has_entry: false, path: None };
        }
        let proc = GetProcAddress(handle, c"vkGetInstanceProcAddr".as_ptr());

        let mut buf = [0i8; 260];
        let len = GetModuleFileNameA(handle, buf.as_mut_ptr(), buf.len() as u32) as usize;
        let path = (len > 0).then(|| {
            let bytes = core::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), len);
            String::from_utf8_lossy(bytes).into_owned()
        });

        FreeLibrary(handle);

        LoaderProbe { applicable: true, loaded: true, has_entry: !proc.is_null(), path }
    }
}

/// Registered Vulkan drivers (ICDs) and implicit layers, by JSON basename — read
/// from the registry so crash reports show which drivers exist (none/stale
/// explains "no Vulkan") and which overlay layers are injected (Steam/Discord/
/// RTSS implicit layers are a classic cause of loader/init crashes on otherwise
/// capable machines). Empty off Windows.
#[must_use]
pub fn registry_summary() -> (Vec<String>, Vec<String>) {
    #[cfg(windows)]
    {
        use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
        use winreg::RegKey;

        fn basenames(root: &RegKey, path: &str, out: &mut Vec<String>) {
            let Ok(key) = root.open_subkey(path) else { return };
            for (name, _value) in key.enum_values().flatten() {
                let base = std::path::Path::new(&name)
                    .file_name()
                    .map_or(name.clone(), |s| s.to_string_lossy().into_owned());
                if !out.contains(&base) {
                    out.push(base);
                }
            }
        }

        let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (mut icds, mut layers) = (Vec::new(), Vec::new());
        for root in [&hklm, &hkcu] {
            basenames(root, r"SOFTWARE\Khronos\Vulkan\Drivers", &mut icds);
            basenames(root, r"SOFTWARE\Khronos\Vulkan\ImplicitLayers", &mut layers);
        }
        (icds, layers)
    }
    #[cfg(not(windows))]
    {
        (Vec::new(), Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::probe_loader;

    #[test]
    fn probe_never_panics_and_tag_matches_usable() {
        let p = probe_loader();
        assert_eq!(p.usable(), p.tag() == "ok" || p.tag() == "n/a");
        #[cfg(target_os = "macos")]
        {
            assert!(!p.applicable);
            assert_eq!(p.tag(), "n/a");
            assert!(p.usable());
        }
    }
}
