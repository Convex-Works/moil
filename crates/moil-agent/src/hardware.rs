//! What `hello` says about this computer's hardware.

use std::process::{Command, Stdio};

use moil_proto::Gpu;

/// This computer as services see it. Detected once per process.
#[derive(Debug, Clone)]
pub(crate) struct Hardware {
    /// What the owner calls the computer, e.g. "Ada's MacBook Pro".
    pub computer_name: String,
    pub cpus: u32,
    pub memory_bytes: u64,
    pub gpus: Vec<Gpu>,
}

impl Hardware {
    pub fn detect() -> Self {
        Self {
            computer_name: computer_name().unwrap_or_else(|| "moil machine".to_owned()),
            cpus: std::thread::available_parallelism()
                .map_or(1, |n| u32::try_from(n.get()).unwrap_or(u32::MAX)),
            memory_bytes: memory_bytes().unwrap_or(0),
            gpus: gpus(),
        }
    }
}

/// The trimmed standard output of a command that succeeded and said
/// something.
fn output(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (out.status.success() && !text.is_empty()).then_some(text)
}

fn computer_name() -> Option<String> {
    #[cfg(target_os = "macos")]
    if let Some(name) = output("scutil", &["--get", "ComputerName"]) {
        return Some(name);
    }
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .or_else(|| output("hostname", &[]))
}

#[cfg(target_os = "macos")]
fn memory_bytes() -> Option<u64> {
    output("sysctl", &["-n", "hw.memsize"])?.parse().ok()
}

#[cfg(not(target_os = "macos"))]
fn memory_bytes() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kib: u64 = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))?
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse()
        .ok()?;
    Some(kib * 1024)
}

/// On Apple Silicon the GPU is part of the chip, so the chip's name
/// ("Apple M3 Pro") names the GPU; its memory is the machine's.
#[cfg(target_os = "macos")]
fn gpus() -> Vec<Gpu> {
    output("sysctl", &["-n", "machdep.cpu.brand_string"])
        .filter(|chip| chip.starts_with("Apple"))
        .map(|chip| Gpu {
            name: chip,
            memory_bytes: None,
        })
        .into_iter()
        .collect()
}

/// NVIDIA GPUs, as `nvidia-smi` lists them. Other GPUs aren't detected yet.
#[cfg(not(target_os = "macos"))]
fn gpus() -> Vec<Gpu> {
    let Some(list) = output(
        "nvidia-smi",
        &[
            "--query-gpu=name,memory.total",
            "--format=csv,noheader,nounits",
        ],
    ) else {
        return Vec::new();
    };
    list.lines()
        .filter_map(|line| {
            let (name, mib) = line.rsplit_once(',')?;
            Some(Gpu {
                name: name.trim().to_owned(),
                memory_bytes: mib.trim().parse::<u64>().ok().map(|mib| mib << 20),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_this_computer() {
        let hardware = Hardware::detect();
        assert!(!hardware.computer_name.is_empty());
        assert!(hardware.cpus >= 1);
        assert!(hardware.memory_bytes > 0, "{hardware:?}");
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        assert!(
            hardware.gpus.iter().any(|g| g.name.starts_with("Apple M")),
            "{hardware:?}"
        );
    }
}
