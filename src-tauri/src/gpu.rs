//! Enumerating the machine's GPUs for the settings picker.
//!
//! Left to itself, mbgl's Vulkan backend binds the *first* compatible physical
//! device, which can be a software rasterizer (Mesa lavapipe) — the GPU sits idle
//! while the CPU renders and overheats. The fork's Vulkan patch instead prefers
//! real hardware and honors `MLN_VULKAN_DEVICE_NAME` (a device-name substring) so
//! a specific GPU can be pinned. This module only *lists* the adapters (via the
//! `wgpu` crate, which reads the same Vulkan `deviceName` strings mbgl matches
//! against); [`crate::orm_tiles`] sets the env var from the user's choice.

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

/// Every adapter the primary backends expose, best-hardware-first.
pub fn list_gpus() -> Vec<GpuInfo> {
    let adapters = block_on(instance().enumerate_adapters(wgpu::Backends::PRIMARY));
    if adapters.is_empty() {
        tracing::warn!(
            "no GPU adapters enumerated (Vulkan/Metal/DX12); ORM rendering may fall back to software"
        );
    }
    let ranked: Vec<(u8, GpuInfo)> = adapters
        .iter()
        .map(|a| {
            let info = a.get_info();
            (rank(info.device_type), GpuInfo {
                name: info.name,
                backend: format!("{:?}", info.backend),
                kind: kind_str(info.device_type).to_string(),
                is_software: is_software_type(info.device_type),
            })
        })
        .collect();
    sort_best_first(ranked)
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
            name: name.into(), backend: "Vulkan".into(), kind: "x".into(), is_software: false,
        };
        // Deliberately unsorted input; sort_best_first must order lowest-rank first.
        let ranked = vec![
            (rank(Cpu), g("sw")),
            (rank(DiscreteGpu), g("discrete")),
            (rank(IntegratedGpu), g("integrated")),
            (rank(Other), g("other")),
        ];
        let names: Vec<String> = super::sort_best_first(ranked).into_iter().map(|g| g.name).collect();
        assert_eq!(names, ["discrete", "integrated", "other", "sw"]);
    }

    #[test]
    fn list_gpus_never_panics() {
        // Whatever this machine has (or nothing, on headless CI), enumeration and
        // ranking must not panic.
        let _ = list_gpus();
    }
}
