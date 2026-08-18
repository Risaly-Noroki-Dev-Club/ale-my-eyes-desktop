use ale_core::model_scheduler::{GpuBackend, GpuDevice, ModelRuntimeConfig};
use std::process::Command;

const MIB: u64 = 1024 * 1024;

pub fn probe() -> Vec<GpuDevice> {
    let devices = probe_nvidia();
    #[cfg(target_os = "linux")]
    {
        let mut devices = devices;
        devices.extend(probe_linux_amd());
        devices
    }
    #[cfg(not(target_os = "linux"))]
    {
        devices
    }
}

pub fn probe_with_runtime(runtime: Option<&ModelRuntimeConfig>) -> Vec<GpuDevice> {
    let mut devices = probe();
    if let Some(cli) = runtime.and_then(|runtime| runtime.llama_server.as_deref()) {
        let output = Command::new(cli).arg("--list-devices").output();
        if let Ok(output) = output {
            if output.status.success() {
                let text = format!(
                    "{}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                for device in parse_llama_vulkan(&text) {
                    if !devices.iter().any(|existing| existing.id == device.id) {
                        devices.push(device);
                    }
                }
            }
        }
    }
    devices
}

fn parse_llama_vulkan(output: &str) -> Vec<GpuDevice> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let (id, details) = line.split_once(':')?;
            if !id.starts_with("Vulkan") {
                return None;
            }
            let (name, memory) = details.trim().rsplit_once(" (")?;
            let memory = memory.strip_suffix(')')?;
            let (total, free) = memory.split_once(',')?;
            let total_mib = total.trim().strip_suffix(" MiB")?.parse::<u64>().ok()?;
            let free_mib = free.trim().strip_suffix(" MiB free")?.parse::<u64>().ok()?;
            let lower = name.to_ascii_lowercase();
            if !lower.contains("amd") && !lower.contains("radeon") {
                return None;
            }
            Some(GpuDevice {
                id: format!("amd:{}", id.to_ascii_lowercase()),
                name: name.to_string(),
                backend: GpuBackend::Amd,
                total_vram_bytes: total_mib.saturating_mul(MIB),
                available_vram_bytes: free_mib.saturating_mul(MIB),
            })
        })
        .collect()
}

fn probe_nvidia() -> Vec<GpuDevice> {
    let output = Command::new("nvidia-smi")
        .args([
            "--query-gpu=index,name,memory.total,memory.free",
            "--format=csv,noheader,nounits",
        ])
        .output();
    match output {
        Ok(output) if output.status.success() => {
            parse_nvidia(&String::from_utf8_lossy(&output.stdout))
        }
        _ => Vec::new(),
    }
}

fn parse_nvidia(output: &str) -> Vec<GpuDevice> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(',').map(str::trim);
            let index = fields.next()?;
            let name = fields.next()?;
            let total_mib = fields.next()?.parse::<u64>().ok()?;
            let available_mib = fields.next()?.parse::<u64>().ok()?;
            if fields.next().is_some() {
                return None;
            }
            Some(GpuDevice {
                id: format!("nvidia:{index}"),
                name: name.to_string(),
                backend: GpuBackend::Nvidia,
                total_vram_bytes: total_mib.saturating_mul(MIB),
                available_vram_bytes: available_mib.saturating_mul(MIB),
            })
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn probe_linux_amd() -> Vec<GpuDevice> {
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let card = entry.file_name().to_string_lossy().into_owned();
            if !card.starts_with("card") || card.contains('-') {
                return None;
            }
            let device = entry.path().join("device");
            let vendor = std::fs::read_to_string(device.join("vendor")).ok()?;
            if vendor.trim() != "0x1002" {
                return None;
            }
            let total = read_u64(device.join("mem_info_vram_total"))?;
            let available = read_u64(device.join("mem_info_vram_used"))
                .map(|used| total.saturating_sub(used))
                .or_else(|| read_u64(device.join("mem_info_vram_free")))?;
            Some(GpuDevice {
                id: format!("amd:{card}"),
                name: card,
                backend: GpuBackend::Amd,
                total_vram_bytes: total,
                available_vram_bytes: available,
            })
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn read_u64(path: std::path::PathBuf) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nvidia_memory_without_trusting_localized_units() {
        let devices = parse_nvidia("0, NVIDIA RTX Test, 16384, 12288\ninvalid\n");
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].id, "nvidia:0");
        assert_eq!(devices[0].total_vram_bytes, 16_384 * MIB);
        assert_eq!(devices[0].available_vram_bytes, 12_288 * MIB);
        assert!(devices[0].supports_default_models());
        assert!(!devices[0].supports_large_models());
    }

    #[test]
    fn parses_amd_vulkan_memory_from_llama_device_probe() {
        let devices = parse_llama_vulkan(
            "Available devices:\n  Vulkan0: AMD Radeon PRO W6800 (32752 MiB, 31954 MiB free)\n",
        );
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].id, "amd:vulkan0");
        assert_eq!(devices[0].available_vram_bytes, 31_954 * MIB);
        assert!(devices[0].supports_large_models());
    }
}
