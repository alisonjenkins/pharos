//! Which forward pass runs, and why (spec 039, R10 and R11). The choice is
//! pure over the adapter list so every reason can be tested without a GPU;
//! `probe` reads the real list when the `vulkan` feature is built.

/// `PHAROS_ALIGNER_DEVICE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceMode {
    Auto,
    Vulkan,
    Cpu,
}

impl DeviceMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Vulkan => "vulkan",
            Self::Cpu => "cpu",
        }
    }
}

impl std::str::FromStr for DeviceMode {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, ()> {
        match s {
            "auto" => Ok(Self::Auto),
            "vulkan" => Ok(Self::Vulkan),
            "cpu" => Ok(Self::Cpu),
            _ => Err(()),
        }
    }
}

/// wgpu's adapter types; `Cpu` is a software rasteriser (llvmpipe, lavapipe).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterKind {
    Discrete,
    Integrated,
    Virtual,
    Cpu,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adapter {
    pub name: String,
    pub kind: AdapterKind,
    /// Its position among the adapters of its kind, as wgpu numbers them.
    pub ordinal: usize,
}

/// Where the forward pass runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Device {
    Cpu,
    Vulkan(Adapter),
}

impl Device {
    /// The `aligner.device` value.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Vulkan(_) => "vulkan",
        }
    }
}

/// What was chosen, the R11 reason and, on the GPU, the adapter's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub device: Device,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeviceError {
    #[error("PHAROS_ALIGNER_DEVICE=vulkan but {reason}: adapters seen {seen:?}")]
    NoVulkan { reason: String, seen: Vec<String> },
}

/// Discrete before integrated before virtual; software and unknown never.
fn rank(kind: AdapterKind) -> Option<u8> {
    match kind {
        AdapterKind::Discrete => Some(0),
        AdapterKind::Integrated => Some(1),
        AdapterKind::Virtual => Some(2),
        AdapterKind::Cpu | AdapterKind::Other => None,
    }
}

/// R10 before warm-up: the best hardware adapter, or the CPU and why not.
pub fn choose(mode: DeviceMode, adapters: &[Adapter]) -> Result<Choice, DeviceError> {
    if mode == DeviceMode::Cpu {
        return Ok(Choice {
            device: Device::Cpu,
            reason: "forced_cpu".into(),
        });
    }
    let best = adapters
        .iter()
        .filter_map(|a| rank(a.kind).map(|r| (r, a)))
        .min_by_key(|(r, a)| (*r, a.ordinal))
        .map(|(_, a)| a.clone());
    match (best, mode) {
        (Some(a), DeviceMode::Vulkan) => Ok(Choice {
            device: Device::Vulkan(a),
            reason: "forced_vulkan".into(),
        }),
        (Some(a), _) => Ok(Choice {
            device: Device::Vulkan(a),
            reason: "adapter_found".into(),
        }),
        (None, _) => {
            let reason = if adapters.iter().any(|a| a.kind == AdapterKind::Cpu) {
                "software_adapter_only"
            } else {
                "no_adapter"
            };
            if mode == DeviceMode::Vulkan {
                return Err(DeviceError::NoVulkan {
                    reason: reason.into(),
                    seen: adapters.iter().map(|a| a.name.clone()).collect(),
                });
            }
            Ok(Choice {
                device: Device::Cpu,
                reason: reason.into(),
            })
        }
    }
}

/// R10 after warm-up: a failed GPU warm-up sends `auto` to the CPU and stops
/// a forced `vulkan` from starting.
pub fn after_warmup(
    mode: DeviceMode,
    choice: Choice,
    warmup: Result<(), String>,
) -> Result<Choice, DeviceError> {
    match (warmup, &choice.device) {
        (Err(e), Device::Vulkan(a)) => {
            let reason = format!("warmup_failed:{e}");
            if mode == DeviceMode::Vulkan {
                return Err(DeviceError::NoVulkan {
                    reason,
                    seen: vec![a.name.clone()],
                });
            }
            Ok(Choice {
                device: Device::Cpu,
                reason,
            })
        }
        _ => Ok(choice),
    }
}

/// The Vulkan adapters wgpu can see on this machine.
#[cfg(feature = "vulkan")]
pub fn probe() -> Vec<Adapter> {
    use wgpu::DeviceType;
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let mut seen = std::collections::HashMap::new();
    pollster::block_on(instance.enumerate_adapters(wgpu::Backends::VULKAN))
        .iter()
        .map(|a| {
            let info = a.get_info();
            let kind = match info.device_type {
                DeviceType::DiscreteGpu => AdapterKind::Discrete,
                DeviceType::IntegratedGpu => AdapterKind::Integrated,
                DeviceType::VirtualGpu => AdapterKind::Virtual,
                DeviceType::Cpu => AdapterKind::Cpu,
                DeviceType::Other => AdapterKind::Other,
            };
            let n = seen.entry(kind as u8).or_insert(0usize);
            let ordinal = *n;
            *n += 1;
            Adapter {
                name: info.name,
                kind,
                ordinal,
            }
        })
        .collect()
}

/// Without the `vulkan` feature there is no GPU to find.
#[cfg(not(feature = "vulkan"))]
pub fn probe() -> Vec<Adapter> {
    Vec::new()
}

/// The Burn device for an adapter `choose` picked.
#[cfg(feature = "vulkan")]
pub fn burn_device(a: &Adapter) -> burn::tensor::Device {
    use burn::backend::{WgpuBackend, WgpuDevice, WgpuDeviceKind};
    let kind = match a.kind {
        AdapterKind::Integrated => WgpuDeviceKind::IntegratedGpu(a.ordinal),
        AdapterKind::Virtual => WgpuDeviceKind::VirtualGpu(a.ordinal),
        _ => WgpuDeviceKind::DiscreteGpu(a.ordinal),
    };
    WgpuDevice::new(kind).on(WgpuBackend::Vulkan).into()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn a(name: &str, kind: AdapterKind, ordinal: usize) -> Adapter {
        Adapter {
            name: name.into(),
            kind,
            ordinal,
        }
    }

    #[test]
    fn every_reason_comes_from_its_case() {
        let gpu = a(
            "AMD Radeon RX 9070 (RADV GFX1201)",
            AdapterKind::Discrete,
            0,
        );
        let soft = a("llvmpipe (LLVM 21.1.0, 256 bits)", AdapterKind::Cpu, 0);

        let c = choose(DeviceMode::Cpu, std::slice::from_ref(&gpu)).unwrap();
        assert_eq!((c.device, c.reason.as_str()), (Device::Cpu, "forced_cpu"));

        let c = choose(DeviceMode::Vulkan, &[soft.clone(), gpu.clone()]).unwrap();
        assert_eq!(
            (c.device, c.reason.as_str()),
            (Device::Vulkan(gpu.clone()), "forced_vulkan")
        );

        let c = choose(DeviceMode::Auto, &[soft.clone(), gpu.clone()]).unwrap();
        assert_eq!(
            (c.device, c.reason.as_str()),
            (Device::Vulkan(gpu.clone()), "adapter_found")
        );

        let c = choose(DeviceMode::Auto, &[]).unwrap();
        assert_eq!((c.device, c.reason.as_str()), (Device::Cpu, "no_adapter"));

        let c = choose(DeviceMode::Auto, std::slice::from_ref(&soft)).unwrap();
        assert_eq!(
            (c.device, c.reason.as_str()),
            (Device::Cpu, "software_adapter_only")
        );

        let found = choose(DeviceMode::Auto, std::slice::from_ref(&gpu)).unwrap();
        let c = after_warmup(DeviceMode::Auto, found, Err("device lost".into())).unwrap();
        assert_eq!(
            (c.device, c.reason.as_str()),
            (Device::Cpu, "warmup_failed:device lost")
        );
    }

    #[test]
    fn forced_vulkan_without_hardware_refuses_to_start() {
        let soft = a("llvmpipe (LLVM 21.1.0, 256 bits)", AdapterKind::Cpu, 0);
        let e = choose(DeviceMode::Vulkan, std::slice::from_ref(&soft)).unwrap_err();
        assert_eq!(
            e.to_string(),
            r#"PHAROS_ALIGNER_DEVICE=vulkan but software_adapter_only: adapters seen ["llvmpipe (LLVM 21.1.0, 256 bits)"]"#
        );
        assert!(choose(DeviceMode::Vulkan, &[]).is_err());
        let gpu = a("gpu", AdapterKind::Discrete, 0);
        let forced = choose(DeviceMode::Vulkan, std::slice::from_ref(&gpu)).unwrap();
        let e = after_warmup(DeviceMode::Vulkan, forced, Err("oom".into())).unwrap_err();
        assert!(e.to_string().contains("warmup_failed:oom"), "{e}");
    }

    #[test]
    fn discrete_beats_integrated_and_unknown_kinds_are_skipped() {
        let list = [
            a("igpu", AdapterKind::Integrated, 0),
            a("odd", AdapterKind::Other, 0),
            a("dgpu1", AdapterKind::Discrete, 1),
            a("dgpu0", AdapterKind::Discrete, 0),
        ];
        let c = choose(DeviceMode::Auto, &list).unwrap();
        assert_eq!(c.device, Device::Vulkan(list[3].clone()));
        let c = choose(DeviceMode::Auto, &[a("odd", AdapterKind::Other, 0)]).unwrap();
        assert_eq!((c.device, c.reason.as_str()), (Device::Cpu, "no_adapter"));
    }

    #[test]
    fn modes_parse() {
        for m in [DeviceMode::Auto, DeviceMode::Vulkan, DeviceMode::Cpu] {
            assert_eq!(m.label().parse(), Ok(m));
        }
        assert_eq!("gpu".parse::<DeviceMode>(), Err(()));
    }
}
