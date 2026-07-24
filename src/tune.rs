//! CPU tuning levers that matter for link behaviour: frequency governor,
//! energy-performance preference, and cpuidle state gating. Root is needed
//! to apply; reading is unprivileged. Profiles are defined by exit-latency
//! thresholds rather than state numbers, so they generalize across machines.
//!
//! Everything here is runtime-only and resets on reboot.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// powersave governor, balanced EPP, all idle states enabled.
    Default,
    /// performance governor, idle states with >100 µs exit latency off
    /// (kills the multi-hundred-µs wake-up penalty, keeps boost headroom).
    Balanced,
    /// performance governor, idle states with >10 µs exit latency off
    /// (fastest wake-up; costs some hot-path boost — for bursty RPC).
    Latency,
}

impl Profile {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "default" => Some(Self::Default),
            "balanced" => Some(Self::Balanced),
            "latency" => Some(Self::Latency),
            _ => None,
        }
    }
}

impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Default => "default",
            Self::Balanced => "balanced",
            Self::Latency => "latency",
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdleStateInfo {
    pub name: String,
    pub latency_us: u64,
    pub disabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuTuneState {
    pub cpus: usize,
    pub governor: String,
    pub epp: String,
    pub idle_states: Vec<IdleStateInfo>,
    /// net.core.busy_read µs (socket busy-polling; 0 = off).
    #[serde(default)]
    pub busy_read_us: u32,
}

impl CpuTuneState {
    /// Compact one-line form for run records: "performance/performance C3:off".
    pub fn summary(&self) -> String {
        let off: Vec<String> = self
            .idle_states
            .iter()
            .filter(|s| s.disabled)
            .map(|s| s.name.clone())
            .collect();
        let idle = if off.is_empty() {
            "all-idle-on".to_string()
        } else {
            format!("{}:off", off.join("+"))
        };
        let epp = if self.epp.is_empty() { "-" } else { &self.epp };
        let busy = if self.busy_read_us > 0 {
            format!(" busy:{}µs", self.busy_read_us)
        } else {
            String::new()
        };
        format!("{}/{} {}{busy}", self.governor, epp, idle)
    }
}

const CPU_ROOT: &str = "/sys/devices/system/cpu";

fn cpu_dirs() -> Result<Vec<PathBuf>> {
    let mut v = Vec::new();
    for e in std::fs::read_dir(CPU_ROOT).context("read /sys cpu dir")?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with("cpu") && name[3..].chars().all(|c| c.is_ascii_digit()) {
            v.push(e.path());
        }
    }
    v.sort();
    Ok(v)
}

fn read_str(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default().trim().to_string()
}

pub fn read_state() -> Result<CpuTuneState> {
    let cpus = cpu_dirs()?;
    let cpu0 = cpus.first().context("no cpus found in sysfs")?;
    let governor = read_str(&cpu0.join("cpufreq/scaling_governor"));
    let epp = read_str(&cpu0.join("cpufreq/energy_performance_preference"));
    let mut idle_states = Vec::new();
    let idle_dir = cpu0.join("cpuidle");
    if let Ok(rd) = std::fs::read_dir(&idle_dir) {
        let mut states: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        states.sort();
        for s in states {
            idle_states.push(IdleStateInfo {
                name: read_str(&s.join("name")),
                latency_us: read_str(&s.join("latency")).parse().unwrap_or(0),
                disabled: read_str(&s.join("disable")) == "1",
            });
        }
    }
    let busy_read_us = std::fs::read_to_string("/proc/sys/net/core/busy_read")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    Ok(CpuTuneState { cpus: cpus.len(), governor, epp, idle_states, busy_read_us })
}

fn write_all_cpus(rel: &str, value: &str) -> Result<usize> {
    let mut written = 0;
    for cpu in cpu_dirs()? {
        let p = cpu.join(rel);
        if !p.exists() {
            continue;
        }
        std::fs::write(&p, value).with_context(|| {
            format!(
                "write {} to {} — applying tuning needs root (sudo linkbench tune …)",
                value,
                p.display()
            )
        })?;
        written += 1;
    }
    Ok(written)
}

/// Best-effort write (EPP rejects writes while governor=performance on
/// amd-pstate; that is fine — performance is what we wanted).
fn write_all_cpus_soft(rel: &str, value: &str) {
    if let Ok(cpus) = cpu_dirs() {
        for cpu in cpus {
            let p = cpu.join(rel);
            if p.exists() {
                let _ = std::fs::write(&p, value);
            }
        }
    }
}

pub fn apply(profile: Profile) -> Result<CpuTuneState> {
    let state = read_state()?;
    let (governor, epp, idle_off_above_us, busy_us) = match profile {
        Profile::Default => ("powersave", "balance_performance", u64::MAX, "0"),
        Profile::Balanced => ("performance", "performance", 100, "50"),
        Profile::Latency => ("performance", "performance", 10, "50"),
    };
    write_all_cpus("cpufreq/scaling_governor", governor)?;
    write_all_cpus_soft("cpufreq/energy_performance_preference", epp);
    for (i, s) in state.idle_states.iter().enumerate() {
        let disable = if s.latency_us > idle_off_above_us { "1" } else { "0" };
        write_all_cpus(&format!("cpuidle/state{i}/disable"), disable)?;
    }
    // Socket busy-polling: blocking reads spin briefly before sleeping,
    // taking C-state exits and IRQ latency off the hot receive path.
    for f in ["/proc/sys/net/core/busy_read", "/proc/sys/net/core/busy_poll"] {
        let _ = std::fs::write(f, busy_us);
    }
    read_state()
}

/// Apply per-profile NIC interrupt coalescing to `iface` (via ethtool).
/// Measured on mlx5: rx-usecs 3 / rx-frames 32 keeps line-rate bandwidth
/// while collapsing small-message p99 (adaptive coalescing's ramp-up is
/// what tail-latency pays for).
pub fn apply_nic(profile: Profile, iface: &str) -> Result<String> {
    let args: &[&str] = match profile {
        Profile::Default => &[
            "adaptive-rx", "on", "adaptive-tx", "on", "rx-usecs", "8", "rx-frames", "128",
            "tx-usecs", "8", "tx-frames", "128",
        ],
        Profile::Balanced | Profile::Latency => &[
            "adaptive-rx", "off", "adaptive-tx", "off", "rx-usecs", "3", "rx-frames", "32",
            "tx-usecs", "8", "tx-frames", "32",
        ],
    };
    let out = Command::new("ethtool")
        .arg("-C")
        .arg(iface)
        .args(args)
        .output()
        .context("run ethtool (needs root)")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        // Some drivers (tbnet) have no coalescing at all — not an error.
        if err.contains("not supported") {
            return Ok(format!("{iface}: coalescing not supported by driver"));
        }
        anyhow::bail!("ethtool -C {iface} failed: {}", err.trim());
    }
    Ok(format!("{iface}: coalescing set for profile {profile}"))
}
