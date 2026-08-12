//! Enumerating the machine's GPUs for the settings picker.
//!
//! Left to itself, mbgl's Vulkan backend binds the *first* compatible physical
//! device, which can be a software rasterizer (Mesa lavapipe) — the GPU sits idle
//! while the CPU renders and overheats. The fork's Vulkan patch instead prefers
//! real hardware and honors `MLN_VULKAN_DEVICE_NAME` (a device-name substring) so
//! a specific GPU can be pinned. This module only *lists* the adapters (via the
//! `wgpu` crate, which reads the same Vulkan `deviceName` strings mbgl matches
//! against); [`crate::orm_tiles`] sets the env var from the user's choice.

use std::collections::HashMap;

use futures::executor::block_on;

/// One available GPU adapter, surfaced to the settings picker.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuInfo {
    pub name: String,
    pub backend: String,
    /// `discrete` / `integrated` / `virtual` / `cpu` / `other`.
    pub kind: String,
    /// True for adapter types that are (or may be) a software rasterizer rather
    /// than a real GPU — see [`is_software_type`].
    pub is_software: bool,
}

/// Whether an adapter type is a software rasterizer rather than real hardware.
/// `Cpu` covers Mesa lavapipe / D3D WARP / `SwiftShader`, and the catch-all `Other`
/// is where non-classified software implementations land. `VirtualGpu` is treated
/// as hardware — it is a virtualized/passed-through GPU (SR-IOV, virtio-gpu), not
/// a CPU renderer.
fn is_software_type(t: wgpu::DeviceType) -> bool {
    matches!(t, wgpu::DeviceType::Cpu | wgpu::DeviceType::Other)
}

fn instance() -> wgpu::Instance {
    // PRIMARY = Vulkan / Metal / DX12 — the hardware-backed APIs. GL (a common
    // software-fallback path) is deliberately excluded.
    wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::PRIMARY,
        flags: wgpu::InstanceFlags::default(),
        memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
        backend_options: wgpu::BackendOptions::default(),
        display: None,
    })
}

fn kind_str(t: wgpu::DeviceType) -> &'static str {
    match t {
        wgpu::DeviceType::DiscreteGpu => "discrete",
        wgpu::DeviceType::IntegratedGpu => "integrated",
        wgpu::DeviceType::VirtualGpu => "virtual",
        wgpu::DeviceType::Cpu => "cpu",
        wgpu::DeviceType::Other => "other",
    }
}

/// Selection rank: real hardware first (discrete over integrated), software last.
fn rank(t: wgpu::DeviceType) -> u8 {
    match t {
        wgpu::DeviceType::DiscreteGpu => 0,
        wgpu::DeviceType::IntegratedGpu => 1,
        wgpu::DeviceType::VirtualGpu => 2,
        wgpu::DeviceType::Other => 3,
        wgpu::DeviceType::Cpu => 4,
    }
}

/// Preference when one physical device is exposed by several backends: keep the
/// one the ORM renderer actually uses, which also matches `MLN_VULKAN_DEVICE_NAME`.
/// Windows lists every GPU under both Vulkan and DX12, so without this the picker
/// shows each device twice (see [`dedupe_by_device`]).
fn backend_rank(b: wgpu::Backend) -> u8 {
    match b {
        wgpu::Backend::Vulkan => 0,
        wgpu::Backend::Metal => 1,
        wgpu::Backend::Dx12 => 2,
        _ => 3,
    }
}

/// Whether the GPU backend the ORM renderer (mbgl) will use is actually present.
///
/// mbgl uses Metal on macOS (always available) and **Vulkan** everywhere else. On a
/// machine with no Vulkan-capable device, mbgl's Vulkan init dereferences a null
/// loader (`vk::DispatchLoaderDynamic::init`) and takes the whole process down with
/// an uncatchable C++ access violation — it offers no fallible path. `wgpu`, by
/// contrast, enumerates the same devices and simply returns an empty list when
/// Vulkan is unavailable, so we probe with it first and let the caller skip ORM
/// rendering (rather than crash) when nothing turns up. A software Vulkan (lavapipe)
/// still counts — mbgl runs on it, slowly, without crashing.
#[must_use]
pub fn render_backend_available() -> bool {
    #[cfg(target_os = "macos")]
    {
        true
    }
    #[cfg(not(target_os = "macos"))]
    {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..wgpu::InstanceDescriptor::default()
        });
        !block_on(instance.enumerate_adapters(wgpu::Backends::VULKAN)).is_empty()
    }
}

/// Every adapter the primary backends expose, de-duplicated, best-hardware-first.
pub fn list_gpus() -> Vec<GpuInfo> {
    let adapters = block_on(instance().enumerate_adapters(wgpu::Backends::PRIMARY));
    if adapters.is_empty() {
        tracing::warn!(
            "no GPU adapters enumerated (Vulkan/Metal/DX12); ORM rendering may fall back to software"
        );
    }
    let entries: Vec<(u8, u8, GpuInfo)> = adapters
        .iter()
        .map(|a| {
            let info = a.get_info();
            (
                rank(info.device_type),
                backend_rank(info.backend),
                GpuInfo {
                    name: info.name,
                    backend: format!("{:?}", info.backend),
                    kind: kind_str(info.device_type).to_string(),
                    is_software: is_software_type(info.device_type),
                },
            )
        })
        .collect();
    sort_best_first(dedupe_by_device(entries))
}

/// Collapses adapters that are the same device exposed by multiple backends
/// (Windows lists each GPU under both Vulkan and DX12), keyed by device name and
/// keeping the lowest [`backend_rank`] — the backend the renderer uses. Preserves
/// first-seen order so listing is stable. Split out to test without a real GPU.
fn dedupe_by_device(entries: Vec<(u8, u8, GpuInfo)>) -> Vec<(u8, GpuInfo)> {
    let mut order: Vec<(u8, u8, GpuInfo)> = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    for (device_rank, backend_rank, gpu) in entries {
        match seen.get(&gpu.name) {
            Some(&idx) if backend_rank >= order[idx].1 => {}
            Some(&idx) => order[idx] = (device_rank, backend_rank, gpu),
            None => {
                seen.insert(gpu.name.clone(), order.len());
                order.push((device_rank, backend_rank, gpu));
            }
        }
    }
    order
        .into_iter()
        .map(|(device_rank, _, gpu)| (device_rank, gpu))
        .collect()
}

/// Orders `(rank, adapter)` pairs best-hardware-first (lowest rank), dropping the
/// rank. Split out so the ordering is unit-testable without a real GPU.
fn sort_best_first(mut ranked: Vec<(u8, GpuInfo)>) -> Vec<GpuInfo> {
    ranked.sort_by_key(|(rank, _)| *rank);
    ranked.into_iter().map(|(_, g)| g).collect()
}

/// Lists the available GPUs for the settings picker, best-hardware-first.
#[tauri::command]
pub fn list_gpu_adapters() -> Vec<GpuInfo> {
    list_gpus()
}

#[cfg(test)]
mod tests {
    use super::{is_software_type, list_gpus, rank};
    use wgpu::DeviceType::{Cpu, DiscreteGpu, IntegratedGpu, Other, VirtualGpu};

    // The selection logic is pure and GPU-independent — tested directly so this
    // runs on headless CI (no adapter present) without a hardware assertion.

    #[test]
    fn rank_orders_hardware_before_software() {
        assert!(rank(DiscreteGpu) < rank(IntegratedGpu));
        assert!(rank(IntegratedGpu) < rank(VirtualGpu));
        assert!(rank(VirtualGpu) < rank(Other));
        assert!(rank(Other) < rank(Cpu));
    }

    #[test]
    fn software_types_are_cpu_and_other() {
        assert!(is_software_type(Cpu));
        assert!(is_software_type(Other));
        assert!(!is_software_type(DiscreteGpu));
        assert!(!is_software_type(IntegratedGpu));
        // A virtualized/passed-through GPU is hardware, not a CPU renderer.
        assert!(!is_software_type(VirtualGpu));
    }

    #[test]
    fn sort_best_first_orders_by_rank() {
        let g = |name: &str| super::GpuInfo {
            name: name.into(),
            backend: "Vulkan".into(),
            kind: "x".into(),
            is_software: false,
        };
        // Deliberately unsorted input; sort_best_first must order lowest-rank first.
        let ranked = vec![
            (rank(Cpu), g("sw")),
            (rank(DiscreteGpu), g("discrete")),
            (rank(IntegratedGpu), g("integrated")),
            (rank(Other), g("other")),
        ];
        let names: Vec<String> = super::sort_best_first(ranked)
            .into_iter()
            .map(|g| g.name)
            .collect();
        assert_eq!(names, ["discrete", "integrated", "other", "sw"]);
    }

    #[test]
    fn list_gpus_never_panics() {
        // Whatever this machine has (or nothing, on headless CI), enumeration and
        // ranking must not panic.
        let _ = list_gpus();
    }

    #[test]
    fn render_backend_probe_never_panics() {
        // The probe must fail gracefully on any machine — that is the whole point
        // (it stands in for mbgl's Vulkan init, which does not).
        let _ = super::render_backend_available();
    }

    #[test]
    fn dedupe_collapses_multi_backend_devices_keeping_preferred() {
        let g = |name: &str, backend: &str| super::GpuInfo {
            name: name.into(),
            backend: backend.into(),
            kind: "discrete".into(),
            is_software: false,
        };
        // The Windows case: one GPU under Vulkan (backend_rank 0) and DX12
        // (backend_rank 2), plus a distinct second GPU. DX12 comes first to prove
        // the lower backend_rank wins regardless of order.
        let entries = vec![
            (rank(DiscreteGpu), 2, g("NVIDIA RTX 4070", "Dx12")),
            (rank(DiscreteGpu), 0, g("NVIDIA RTX 4070", "Vulkan")),
            (rank(IntegratedGpu), 0, g("Intel UHD", "Vulkan")),
        ];
        let out = super::dedupe_by_device(entries);
        assert_eq!(out.len(), 2, "the duplicated device must collapse to one");
        let nvidia = out
            .iter()
            .find(|(_, g)| g.name == "NVIDIA RTX 4070")
            .expect("deduped list keeps the device");
        assert_eq!(nvidia.1.backend, "Vulkan", "keeps the renderer's backend");
    }
}
