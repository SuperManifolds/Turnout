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
    /// True for a `Cpu` (software) adapter — the case we want to avoid.
    pub is_software: bool,
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
    let mut gpus: Vec<(u8, GpuInfo)> = adapters
        .iter()
        .map(|a| {
            let info = a.get_info();
            let rank = rank(info.device_type);
            (rank, GpuInfo {
                name: info.name,
                backend: format!("{:?}", info.backend),
                kind: kind_str(info.device_type).to_string(),
                is_software: info.device_type == wgpu::DeviceType::Cpu,
            })
        })
        .collect();
    gpus.sort_by_key(|(rank, _)| *rank);
    gpus.into_iter().map(|(_, g)| g).collect()
}

/// Lists the available GPUs for the settings picker, best-hardware-first.
#[tauri::command]
pub fn list_gpu_adapters() -> Vec<GpuInfo> {
    list_gpus()
}

#[cfg(test)]
mod tests {
    #[test]
    fn enumerates_at_least_one_hardware_gpu() {
        let gpus = super::list_gpus();
        for g in &gpus {
            eprintln!("gpu: {} [{}] {} software={}", g.name, g.backend, g.kind, g.is_software);
        }
        assert!(!gpus.is_empty(), "no GPU adapters enumerated");
        assert!(gpus.iter().any(|g| !g.is_software), "no hardware (non-software) GPU found");
    }
}
