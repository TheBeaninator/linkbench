//! Node interrogation: everything another node (or an orchestrating GUI /
//! agent) wants to know before benchmarking — sudo state, CPU tuning,
//! every network interface classified by physical kind, RDMA devices,
//! and thunderbolt bus peers. Emitted as JSON by `linkbench probe --json`.

use crate::proto::RdmaDeviceInfo;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum IfKind {
    Wifi,
    Ethernet,
    Thunderbolt,
    Connectx,
    Loopback,
    Virtual,
    Other,
}

impl std::fmt::Display for IfKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            IfKind::Wifi => "wifi",
            IfKind::Ethernet => "ethernet",
            IfKind::Thunderbolt => "thunderbolt",
            IfKind::Connectx => "connectx",
            IfKind::Loopback => "loopback",
            IfKind::Virtual => "virtual",
            IfKind::Other => "other",
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetIf {
    pub name: String,
    pub kind: IfKind,
    pub driver: String,
    pub oper_up: bool,
    pub mtu: u32,
    /// Link speed in Mb/s where the kernel knows it (-1/unknown → None).
    pub speed_mbps: Option<i64>,
    pub ipv4: Vec<String>,
    pub mac: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TbDevice {
    pub id: String,
    pub vendor: String,
    pub device: String,
    /// An XDomain entry = another host connected over thunderbolt.
    pub is_host_peer: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeProbe {
    pub hostname: String,
    pub version: String,
    pub sudo_passwordless: bool,
    pub tuning: String,
    pub interfaces: Vec<NetIf>,
    pub rdma: Vec<RdmaDeviceInfo>,
    pub thunderbolt: Vec<TbDevice>,
    /// Software RoCE (rdma_rxe) is available — the RDMA path for nodes with
    /// no hardware RDMA NIC (`linkbench roce-up` enables it on an interface).
    #[serde(default)]
    pub softroce_available: bool,
}

fn read_str(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default().trim().to_string()
}

fn classify(name: &str, dev_dir: &Path, driver: &str) -> IfKind {
    if name == "lo" {
        return IfKind::Loopback;
    }
    if dev_dir.join("wireless").exists() || driver.contains("iwl") || driver.contains("ath") {
        return IfKind::Wifi;
    }
    if driver.contains("thunderbolt") || driver.contains("tbnet") {
        return IfKind::Thunderbolt;
    }
    if driver.contains("mlx") {
        return IfKind::Connectx;
    }
    // Physical devices sit on a bus (pci/usb); anything without a device
    // dir is a bridge/veth/tun/etc.
    if !dev_dir.join("device").exists() {
        return IfKind::Virtual;
    }
    IfKind::Ethernet
}

/// IPv4 addresses per interface, via `ip -j addr` (present on any modern
/// distro); missing tool → empty lists.
fn ipv4_map() -> std::collections::HashMap<String, Vec<String>> {
    let mut map = std::collections::HashMap::new();
    let Ok(out) = Command::new("ip").args(["-j", "addr", "show"]).output() else {
        return map;
    };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
        return map;
    };
    for iface in v.as_array().into_iter().flatten() {
        let name = iface["ifname"].as_str().unwrap_or_default().to_string();
        let mut addrs = Vec::new();
        for a in iface["addr_info"].as_array().into_iter().flatten() {
            if a["family"].as_str() == Some("inet") {
                if let Some(ip) = a["local"].as_str() {
                    addrs.push(format!("{}/{}", ip, a["prefixlen"].as_u64().unwrap_or(32)));
                }
            }
        }
        map.insert(name, addrs);
    }
    map
}

pub fn interfaces() -> Vec<NetIf> {
    let ips = ipv4_map();
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/sys/class/net") else {
        return out;
    };
    let mut entries: Vec<_> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        let driver = std::fs::read_link(p.join("device/driver"))
            .ok()
            .and_then(|l| l.file_name().map(|f| f.to_string_lossy().into_owned()))
            .unwrap_or_default();
        let kind = classify(&name, &p, &driver);
        let speed: i64 = read_str(&p.join("speed")).parse().unwrap_or(-1);
        out.push(NetIf {
            oper_up: read_str(&p.join("operstate")) == "up",
            mtu: read_str(&p.join("mtu")).parse().unwrap_or(0),
            speed_mbps: (speed > 0).then_some(speed),
            ipv4: ips.get(&name).cloned().unwrap_or_default(),
            mac: read_str(&p.join("address")),
            name,
            kind,
            driver,
        });
    }
    out
}

pub fn thunderbolt_devices() -> Vec<TbDevice> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/sys/bus/thunderbolt/devices") else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        let id = e.file_name().to_string_lossy().into_owned();
        let vendor = read_str(&p.join("vendor_name"));
        let device = read_str(&p.join("device_name"));
        if vendor.is_empty() && device.is_empty() {
            continue; // domains / retimers / service entries
        }
        // XDomain (host-to-host) entries expose a unique_id and are not
        // regular routers with an authorized flag.
        let is_host_peer =
            p.join("unique_id").exists() && !p.join("authorized").exists();
        out.push(TbDevice { id, vendor, device, is_host_peer });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

// --------------------------------------------------------- Soft-RoCE (rxe)

/// Is the software RoCE stack (rdma_rxe) available on this kernel? This is
/// the RDMA path for machines with no hardware RDMA NIC — any ordinary
/// ethernet interface can carry RoCE through it.
pub fn softroce_available() -> bool {
    // module built-in, already loaded, or loadable
    if Path::new("/sys/module/rdma_rxe").exists() {
        return true;
    }
    Command::new("modinfo")
        .arg("rdma_rxe")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Bring up software RoCE on `iface`: load rdma_rxe and add an rxe link
/// bound to the interface, so the RDMA transport works without a hardware
/// RDMA NIC. Idempotent. Needs root. Mirrors `tb_up` for thunderbolt.
pub fn roce_up(iface: &str, name: &str, persist: bool) -> Result<String> {
    use anyhow::{bail, Context};
    if !softroce_available() {
        bail!(
            "rdma_rxe (software RoCE) is not available on this kernel — \
             install the `rdma-core`/`rdma_rxe` support or use --transport tcp"
        );
    }
    let _ = Command::new("modprobe").arg("rdma_rxe").status();

    // Already bound to this iface? (rxe exposes its netdev under the ib device)
    for dev in std::fs::read_dir("/sys/class/infiniband").into_iter().flatten().flatten() {
        let parent = read_str(&dev.path().join("parent"));
        if parent == iface {
            return Ok(format!(
                "software RoCE already up: {} on {iface}",
                dev.file_name().to_string_lossy()
            ));
        }
    }

    let out = Command::new("rdma")
        .args(["link", "add", name, "type", "rxe", "netdev", iface])
        .output()
        .context("run `rdma link add` (needs root and iproute2 rdma)")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        if err.contains("File exists") {
            // name taken — fine if it's ours
            return Ok(format!("software RoCE link {name} already exists"));
        }
        bail!("rdma link add failed: {}", err.trim());
    }

    let mut persisted = String::new();
    if persist {
        std::fs::write("/etc/modules-load.d/linkbench-rxe.conf", "rdma_rxe\n")
            .context("write modules-load.d (needs root)")?;
        // A tiny oneshot unit re-adds the rxe link at boot (rdma links are
        // not persistent across reboots on their own).
        let unit = format!(
            "[Unit]\nDescription=linkbench software RoCE ({name} on {iface})\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=oneshot\nExecStart=/bin/sh -c 'modprobe rdma_rxe; rdma link add {name} type rxe netdev {iface} 2>/dev/null || true'\nRemainAfterExit=yes\n\n[Install]\nWantedBy=multi-user.target\n"
        );
        std::fs::write("/etc/systemd/system/linkbench-rxe.service", unit)
            .context("write systemd unit (needs root)")?;
        let _ = Command::new("systemctl").args(["daemon-reload"]).status();
        let _ = Command::new("systemctl").args(["enable", "linkbench-rxe.service"]).status();
        persisted = " (persisted via modules-load.d + linkbench-rxe.service)".into();
    }
    Ok(format!("software RoCE up: {name} on {iface}{persisted}"))
}

/// Which local interface the kernel would use to reach `ip`.
pub fn route_dev(ip: &str) -> Option<String> {
    let out = Command::new("ip").args(["-j", "route", "get", ip]).output().ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    v.as_array()?.first()?["dev"].as_str().map(|s| s.to_string())
}

pub fn sudo_passwordless() -> bool {
    Command::new("sudo")
        .arg("-n")
        .arg("true")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn gather(hostname: String) -> Result<NodeProbe> {
    Ok(NodeProbe {
        hostname,
        version: crate::proto::VERSION.into(),
        sudo_passwordless: sudo_passwordless(),
        tuning: crate::tune::read_state().map(|s| s.summary()).unwrap_or_default(),
        interfaces: interfaces(),
        rdma: crate::rdma::list_devices().unwrap_or_default(),
        softroce_available: softroce_available(),
        thunderbolt: thunderbolt_devices(),
    })
}

// ------------------------------------------------------- thunderbolt bring-up

/// Bring the thunderbolt-net link up on this node: load the module, find
/// the tbnet interface, assign `cidr`, raise MTU as high as the driver
/// allows, optionally persist (modules-load.d + netplan file, not applied).
/// Needs root. Returns a human summary.
pub fn tb_up(cidr: &str, mtu: Option<u32>, persist: bool) -> Result<String> {
    use anyhow::{bail, Context};
    let _ = Command::new("modprobe").arg("thunderbolt_net").status();
    // The netdev can take a moment to appear after modprobe.
    let mut ifname = None;
    for _ in 0..20 {
        if let Some(i) = interfaces().into_iter().find(|i| i.kind == IfKind::Thunderbolt) {
            ifname = Some(i.name);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    let Some(ifname) = ifname else {
        bail!(
            "no thunderbolt-net interface appeared — is the cable connected \
             and the thunderbolt_net module available?"
        );
    };

    let run = |args: &[&str]| -> Result<bool> {
        Ok(Command::new("ip")
            .args(args)
            .status()
            .with_context(|| format!("run ip {args:?} (needs root)"))?
            .success())
    };
    if !run(&["addr", "replace", cidr, "dev", &ifname])? {
        bail!("ip addr replace {cidr} dev {ifname} failed (needs root)");
    }
    run(&["link", "set", &ifname, "up"])?;
    // Thunderbolt-net supports very large frames; take the biggest the
    // driver accepts.
    let candidates: Vec<u32> = match mtu {
        Some(m) => vec![m],
        None => vec![65520, 9000, 4000],
    };
    for m in candidates {
        if run(&["link", "set", &ifname, "mtu", &m.to_string()])? {
            break;
        }
    }
    let actual_mtu = read_str(Path::new(&format!("/sys/class/net/{ifname}/mtu")));

    let mut persisted = String::new();
    if persist {
        std::fs::write("/etc/modules-load.d/linkbench-thunderbolt.conf", "thunderbolt_net\n")
            .context("write modules-load.d (needs root)")?;
        let netplan = format!(
            "# written by linkbench tb-up\nnetwork:\n  version: 2\n  ethernets:\n    {ifname}:\n      addresses: [{cidr}]\n      mtu: {actual_mtu}\n"
        );
        let np_path = "/etc/netplan/70-linkbench-thunderbolt.yaml";
        std::fs::write(np_path, netplan).context("write netplan file (needs root)")?;
        let _ = std::fs::set_permissions(np_path, std::os::unix::fs::PermissionsExt::from_mode(0o600));
        persisted = format!(", persisted ({np_path} + modules-load.d)");
    }
    Ok(format!("{ifname} up with {cidr}, mtu {actual_mtu}{persisted}"))
}

/// The self-elevating script the GUI stages on nodes that lack
/// passwordless sudo; the user runs it themselves, then re-probes.
pub const SUDO_TOGGLE_SCRIPT: &str = r#"#!/usr/bin/env bash
# Staged by linkbench: toggle passwordless sudo for the invoking user.
set -e
TARGET_USER="${SUDO_USER:-$USER}"
if [ "$(id -u)" -ne 0 ]; then
  echo "elevating with sudo (you may be asked for your password)…"
  exec sudo bash "$0"
fi
F="/etc/sudoers.d/99-linkbench-nopasswd"
if [ -f "$F" ]; then
  rm -f "$F"
  echo "passwordless sudo: DISABLED for $TARGET_USER (removed $F)"
else
  printf '%s ALL=(ALL) NOPASSWD:ALL\n' "$TARGET_USER" > "$F"
  chmod 0440 "$F"
  echo "passwordless sudo: ENABLED for $TARGET_USER ($F)"
fi
"#;
