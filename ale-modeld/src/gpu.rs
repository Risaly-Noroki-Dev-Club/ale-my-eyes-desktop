use ale_core::model_scheduler::{GpuBackend, GpuDevice};
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
}
