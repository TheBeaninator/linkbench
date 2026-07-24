//! GUI frontend for linkbench: pick two nodes, press Run, get the pane.
//! Local or SSH nodes; handles binary deploy, remote `serve` lifecycle,
//! live progress, and renders the results dashboard.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui::{self, Color32, RichText};
use linkbench::bench::Results;
use linkbench::probe::{IfKind, NodeProbe, SUDO_TOGGLE_SCRIPT};
use linkbench::report::{
    fmt_bits, fmt_bytes, fmt_rate, fmt_size, timeline_stats, verdicts, Grade,
};
use serde::{Deserialize, Serialize};
use std::io::BufRead;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

// ------------------------------------------------------------------- config

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    a_ssh: bool,
    a_host: String,
    b_ssh: bool,
    b_host: String,
    data_addr: String,
    transport: String,
    quick: bool,
    deploy: bool,
    local_bin: String,
    remote_bin: String,
    port: u16,
    duration: f64,
    streams: u16,
    qps: u16,
    rdma_device: String,
    gid_index: String,
    region_mb: u32,
    tune_profile: String,
    notes: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            a_ssh: false,
            a_host: String::new(),
            b_ssh: true,
            b_host: String::new(),
            data_addr: String::new(),
            transport: "auto".into(),
            quick: false,
            deploy: true,
            local_bin: default_local_bin(),
            remote_bin: "~/linkbench".into(),
            port: 7842,
            duration: 1.0,
            streams: 4,
            qps: 4,
            rdma_device: String::new(),
            gid_index: String::new(),
            region_mb: 128,
            tune_profile: "leave".into(),
            notes: String::new(),
        }
    }
}

fn default_local_bin() -> String {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sib = dir.join("linkbench");
            if sib.exists() {
                return sib.to_string_lossy().into_owned();
            }
        }
    }
    "linkbench".into()
}

fn ssh_host_part(target: &str) -> &str {
    target.rsplit_once('@').map(|(_, h)| h).unwrap_or(target)
}

// ------------------------------------------------------------------- worker

enum WorkerMsg {
    Log(String),
    Status(String),
    Done(Box<Results>),
    Failed(String),
    Probed(Box<ProbeOutcome>),
    BatchItem { label: String, results: Box<Results> },
    BatchDone,
}

// ---------------------------------------------------------- node probing

#[derive(Clone)]
struct LinkPair {
    kind: String,
    kind_label: String,
    a_desc: String,
    b_desc: String,
    detail: String,
    /// B-side bare address to benchmark against, when the link is usable.
    b_addr: Option<String>,
    /// This link supports RDMA on both ends (a second testable mode).
    rdma: bool,
    /// Bare interface names on each side (for roce-up / diagnostics).
    a_iface: String,
    b_iface: String,
    /// No hardware RDMA here, but both ends can run Soft-RoCE — offer to
    /// enable it so the rdma mode becomes testable.
    softroce_offerable: bool,
    /// MAC B reported for its interface — verified against A's neighbor
    /// table after the ping (true L2 adjacency, no router/proxy between).
    b_mac: String,
    rtt_ms: Option<f64>,
}

impl LinkPair {
    fn label(&self) -> String {
        format!("{}  {} <-> {}", self.kind_label, self.a_desc, self.b_desc)
    }
    /// Short unique name for a (link, mode) result row.
    fn short(&self, mode: &str) -> String {
        format!("{} · {mode}", self.kind_label)
    }
    /// Testable rows: RDMA-capable links expand into both modes.
    fn modes(&self) -> Vec<&'static str> {
        if self.b_addr.is_none() {
            vec![]
        } else if self.rdma {
            vec!["tcp", "rdma"]
        } else {
            vec!["tcp"]
        }
    }
}

struct BatchEntry {
    label: String,
    results: Results,
}

struct ProbeOutcome {
    a: Result<NodeProbe, String>,
    b: Result<NodeProbe, String>,
    links: Vec<LinkPair>,
}

fn probe_node(cfg: &Config, is_ssh: bool, host: &str) -> Result<NodeProbe, String> {
    let out = if is_ssh {
        if host.is_empty() {
            return Err("no SSH target set".into());
        }
        if cfg.deploy {
            let dest = format!("{host}:{}", cfg.remote_bin.trim_start_matches("~/"));
            let _ = Command::new("scp")
                .arg("-oBatchMode=yes")
                .arg(&cfg.local_bin)
                .arg(&dest)
                .output();
        }
        ssh_cmd(host, &format!("{} probe --json", cfg.remote_bin))
            .output()
            .map_err(|e| e.to_string())?
    } else {
        Command::new(&cfg.local_bin)
            .args(["probe", "--json"])
            .output()
            .map_err(|e| e.to_string())?
    };
    if !out.status.success() {
        return Err(format!(
            "probe failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("bad probe JSON: {e}"))
}

fn parse_cidr(s: &str) -> Option<(u32, u32)> {
    let (ip, prefix) = s.split_once('/')?;
    let mut v: u32 = 0;
    for oct in ip.split('.') {
        v = (v << 8) | oct.parse::<u32>().ok()?;
    }
    let p: u32 = prefix.parse().ok()?;
    let mask = if p == 0 { 0 } else { u32::MAX << (32 - p.min(32)) };
    Some((v, mask))
}

fn subnet_match(a: &[String], b: &[String]) -> Option<(String, String)> {
    for aa in a {
        let (av, am) = parse_cidr(aa)?;
        for bb in b {
            if let Some((bv, bm)) = parse_cidr(bb) {
                let m = am.min(bm);
                if av & m == bv & m {
                    let bare = |s: &str| s.split('/').next().unwrap_or(s).to_string();
                    return Some((bare(aa), bare(bb)));
                }
            }
        }
    }
    None
}

fn kind_speed(i: &linkbench::probe::NetIf) -> String {
    match i.speed_mbps {
        Some(s) if s >= 1000 => format!("{} {}G", i.kind, s / 1000),
        Some(s) => format!("{} {s}M", i.kind),
        None => format!("{}", i.kind),
    }
}

fn correlate(a: &NodeProbe, b: &NodeProbe) -> Vec<LinkPair> {
    let mut links = Vec::new();
    let mut tb_netdev_seen = false;
    for ai in &a.interfaces {
        if matches!(ai.kind, IfKind::Loopback | IfKind::Virtual) {
            continue;
        }
        for bi in &b.interfaces {
            if bi.kind != ai.kind {
                continue;
            }
            if ai.kind == IfKind::Thunderbolt {
                tb_netdev_seen = true;
            }
            if let Some((aip, bip)) = subnet_match(&ai.ipv4, &bi.ipv4) {
                let rdma = a.rdma.iter().any(|d| d.netdev == ai.name && d.port_state == "ACTIVE")
                    && b.rdma.iter().any(|d| d.netdev == bi.name && d.port_state == "ACTIVE");
                let softroce_offerable =
                    !rdma && a.softroce_available && b.softroce_available;
                links.push(LinkPair {
                    kind: format!("{}", ai.kind),
                    kind_label: kind_speed(ai),
                    a_desc: format!("{} ({aip})", ai.name),
                    b_desc: format!("{} ({bip})", bi.name),
                    detail: if rdma {
                        "RDMA available on both ends".into()
                    } else if softroce_offerable {
                        "TCP only — Soft-RoCE can be enabled for an RDMA run".into()
                    } else {
                        String::new()
                    },
                    b_addr: Some(bip),
                    rdma,
                    a_iface: ai.name.clone(),
                    b_iface: bi.name.clone(),
                    softroce_offerable,
                    b_mac: bi.mac.clone(),
                    rtt_ms: None,
                });
            } else if ai.kind == IfKind::Thunderbolt {
                links.push(LinkPair {
                    kind: "thunderbolt".into(),
                    kind_label: "thunderbolt".into(),
                    a_desc: ai.name.clone(),
                    b_desc: bi.name.clone(),
                    detail: "no shared IPv4 — needs address bring-up".into(),
                    b_addr: None,
                    rdma: false,
                    a_iface: ai.name.clone(),
                    b_iface: bi.name.clone(),
                    softroce_offerable: false,
                    b_mac: String::new(),
                    rtt_ms: None,
                });
            }
        }
    }
    // Bus-level thunderbolt peer with no netdev yet.
    if !tb_netdev_seen {
        let a_peer = a.thunderbolt.iter().find(|t| t.is_host_peer);
        let b_peer = b.thunderbolt.iter().find(|t| t.is_host_peer);
        if let (Some(ap), Some(_)) = (a_peer, b_peer) {
            links.push(LinkPair {
                kind: "thunderbolt".into(),
                kind_label: "thunderbolt bus".into(),
                a_desc: format!("peer \"{} {}\"", ap.vendor, ap.device),
                b_desc: "detected both ends".into(),
                detail: "no network interface yet — needs thunderbolt-net (modprobe thunderbolt_net + IPs)".into(),
                b_addr: None,
                rdma: false,
                a_iface: String::new(),
                b_iface: String::new(),
                softroce_offerable: false,
                b_mac: String::new(),
                rtt_ms: None,
            });
        }
    }
    links
}

fn ping_from_a(cfg: &Config, addr: &str) -> Option<f64> {
    let out = if cfg.a_ssh {
        ssh_cmd(&cfg.a_host, &format!("ping -c 2 -i 0.3 -W 1 {addr}")).output().ok()?
    } else {
        Command::new("ping").args(["-c", "2", "-i", "0.3", "-W", "1", addr]).output().ok()?
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let stats = text.lines().find(|l| l.contains("min/avg/max"))?;
    stats.split('=').nth(1)?.trim().split('/').nth(1)?.parse().ok()
}

/// The MAC A's neighbor table holds for `addr` (populated by the ping).
fn neigh_mac_from_a(cfg: &Config, addr: &str) -> Option<String> {
    let out = if cfg.a_ssh {
        ssh_cmd(&cfg.a_host, &format!("ip neigh show {addr}")).output().ok()?
    } else {
        Command::new("ip").args(["neigh", "show", addr]).output().ok()?
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut it = text.split_whitespace();
    while let Some(tok) = it.next() {
        if tok == "lladdr" {
            return it.next().map(|m| m.to_lowercase());
        }
    }
    None
}

fn probe_worker(cfg: Config, tx: Sender<WorkerMsg>, ctx: egui::Context) {
    let log = |s: String| {
        let _ = tx.send(WorkerMsg::Log(s));
        ctx.request_repaint();
    };
    log("probing node A…".into());
    let a = probe_node(&cfg, cfg.a_ssh, &cfg.a_host);
    log("probing node B…".into());
    let b = probe_node(&cfg, cfg.b_ssh, &cfg.b_host);
    let mut links = match (&a, &b) {
        (Ok(a), Ok(b)) => correlate(a, b),
        _ => Vec::new(),
    };
    for l in &mut links {
        if let Some(addr) = &l.b_addr.clone() {
            log(format!("ping test A → {addr}…"));
            l.rtt_ms = ping_from_a(&cfg, addr);
            // Same-subnet implies L2 adjacency — verify it. Frames should
            // land on exactly the MAC B reported (direct cable or switch);
            // anything else means a router/proxy is in the middle.
            if !l.b_mac.is_empty() {
                match neigh_mac_from_a(&cfg, addr) {
                    Some(seen) if seen == l.b_mac.to_lowercase() => {
                        let tag = "L2 verified";
                        if l.detail.is_empty() {
                            l.detail = tag.into();
                        } else {
                            l.detail = format!("{} · {tag}", l.detail);
                        }
                    }
                    Some(seen) => {
                        l.detail = format!(
                            "WARNING: not L2-adjacent — expected MAC {}, saw {seen} (router/proxy in path?)",
                            l.b_mac
                        );
                    }
                    None => {}
                }
            }
        }
    }
    let _ = tx.send(WorkerMsg::Probed(Box::new(ProbeOutcome { a, b, links })));
    ctx.request_repaint();
}

/// Bring the thunderbolt link up on both nodes (persisted), then re-probe.
fn tb_bringup_worker(cfg: Config, tx: Sender<WorkerMsg>, ctx: egui::Context) {
    let log = |s: String| {
        let _ = tx.send(WorkerMsg::Log(s));
        ctx.request_repaint();
    };
    let _ = tx.send(WorkerMsg::Status("bringing up thunderbolt link…".into()));
    for (is_ssh, host, ip, label) in [
        (cfg.a_ssh, cfg.a_host.clone(), "10.111.11.1/24", 'A'),
        (cfg.b_ssh, cfg.b_host.clone(), "10.111.11.2/24", 'B'),
    ] {
        let mut cmd = if is_ssh {
            ssh_cmd(
                &host,
                &format!("sudo -n {} tb-up --ip {ip} --persist", cfg.remote_bin),
            )
        } else {
            let mut c = Command::new("sudo");
            c.arg("-n").arg(&cfg.local_bin).args(["tb-up", "--ip", ip, "--persist"]);
            c
        };
        match cmd.stdin(Stdio::null()).output() {
            Ok(out) if out.status.success() => {
                log(format!(
                    "node {label}: {}",
                    String::from_utf8_lossy(&out.stdout).trim()
                ));
            }
            Ok(out) => log(format!(
                "node {label}: tb-up FAILED: {}{}",
                String::from_utf8_lossy(&out.stderr).trim(),
                String::from_utf8_lossy(&out.stdout).trim()
            )),
            Err(e) => log(format!("node {label}: tb-up failed to start: {e}")),
        }
    }
    log("re-probing…".into());
    probe_worker(cfg, tx, ctx);
}

/// Enable Soft-RoCE on both ends of a link (persisted), then re-probe so
/// the rdma mode becomes available for that link.
fn roce_bringup_worker(cfg: Config, a_iface: String, b_iface: String, tx: Sender<WorkerMsg>, ctx: egui::Context) {
    let log = |s: String| {
        let _ = tx.send(WorkerMsg::Log(s));
        ctx.request_repaint();
    };
    let _ = tx.send(WorkerMsg::Status("enabling Soft-RoCE on both ends…".into()));
    for (is_ssh, host, iface, label) in [
        (cfg.a_ssh, cfg.a_host.clone(), a_iface, 'A'),
        (cfg.b_ssh, cfg.b_host.clone(), b_iface, 'B'),
    ] {
        if iface.is_empty() {
            continue;
        }
        let mut cmd = if is_ssh {
            ssh_cmd(&host, &format!("sudo -n {} roce-up --dev {iface} --persist", cfg.remote_bin))
        } else {
            let mut c = Command::new("sudo");
            c.arg("-n").arg(&cfg.local_bin).args(["roce-up", "--dev", &iface, "--persist"]);
            c
        };
        match cmd.stdin(Stdio::null()).output() {
            Ok(out) if out.status.success() => {
                log(format!("node {label}: {}", String::from_utf8_lossy(&out.stdout).trim()));
            }
            Ok(out) => log(format!(
                "node {label}: roce-up FAILED (needs passwordless sudo): {}{}",
                String::from_utf8_lossy(&out.stderr).trim(),
                String::from_utf8_lossy(&out.stdout).trim()
            )),
            Err(e) => log(format!("node {label}: roce-up failed to start: {e}")),
        }
    }
    log("re-probing…".into());
    probe_worker(cfg, tx, ctx);
}

/// Test each selected link in sequence; deploy and CPU tuning happen only
/// on the first run.
fn batch_worker(
    mut cfg: Config,
    rows: Vec<(LinkPair, String)>,
    tx: Sender<WorkerMsg>,
    ctx: egui::Context,
    procs: ProcHandles,
) {
    let n = rows.len();
    for (i, (l, mode)) in rows.into_iter().enumerate() {
        if procs.aborted() {
            let _ = tx.send(WorkerMsg::Log("batch stopped".into()));
            break;
        }
        let _ = tx.send(WorkerMsg::Status(format!(
            "testing {} ({} of {n})…",
            l.short(&mode),
            i + 1
        )));
        ctx.request_repaint();
        let mut c = cfg.clone();
        c.data_addr = l.b_addr.clone().unwrap_or_default();
        c.transport = mode.clone();
        let res = worker_inner(&c, &tx, &ctx, &procs, &l.short(&mode));
        if c.b_ssh && !c.b_host.is_empty() {
            let _ = ssh_cmd(&c.b_host, "pkill -x linkbench").stdin(Stdio::null()).output();
        }
        procs.kill_local();
        match res {
            Ok(r) => {
                let _ = tx.send(WorkerMsg::BatchItem {
                    label: l.short(&mode),
                    results: Box::new(r),
                });
            }
            Err(e) => {
                let _ = tx.send(WorkerMsg::Log(format!(
                    "link {} FAILED: {e:#}",
                    l.short(&mode)
                )));
            }
        }
        ctx.request_repaint();
        cfg.deploy = false;
        cfg.tune_profile = "leave".into();
    }
    let _ = tx.send(WorkerMsg::BatchDone);
    ctx.request_repaint();
}

fn stage_sudo_script(_cfg: &Config, is_ssh: bool, host: &str) -> anyhow::Result<String> {
    if is_ssh {
        let tmp = std::env::temp_dir().join("linkbench-sudo-toggle.sh");
        std::fs::write(&tmp, SUDO_TOGGLE_SCRIPT)?;
        let out = Command::new("scp")
            .arg("-oBatchMode=yes")
            .arg(&tmp)
            .arg(format!("{host}:toggle-passwordless-sudo.sh"))
            .output()?;
        if !out.status.success() {
            anyhow::bail!("scp failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        let _ = ssh_cmd(host, "chmod +x ~/toggle-passwordless-sudo.sh").output();
        Ok(format!("staged — now run on {host}:  bash ~/toggle-passwordless-sudo.sh   then re-probe"))
    } else {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        let p = std::path::Path::new(&home).join("toggle-passwordless-sudo.sh");
        std::fs::write(&p, SUDO_TOGGLE_SCRIPT)?;
        let _ = Command::new("chmod").arg("+x").arg(&p).output();
        Ok(format!("staged — now run:  bash {}   then re-probe", p.display()))
    }
}

#[derive(Default, Clone)]
struct ProcHandles {
    client: Arc<Mutex<Option<Child>>>,
    local_server: Arc<Mutex<Option<Child>>>,
    abort: Arc<std::sync::atomic::AtomicBool>,
}

impl ProcHandles {
    fn kill_local(&self) {
        for slot in [&self.client, &self.local_server] {
            if let Some(mut c) = slot.lock().unwrap().take() {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    }
    /// Stop button: kill current processes and abort any batch loop.
    fn kill_all(&self) {
        self.abort.store(true, std::sync::atomic::Ordering::SeqCst);
        self.kill_local();
    }
    fn aborted(&self) -> bool {
        self.abort.load(std::sync::atomic::Ordering::SeqCst)
    }
}

fn ssh_cmd(target: &str, remote: &str) -> Command {
    let mut c = Command::new("ssh");
    c.arg("-oBatchMode=yes")
        .arg("-oConnectTimeout=8")
        .arg(target)
        .arg(remote);
    c
}

fn run_logged(mut cmd: Command, tx: &Sender<WorkerMsg>, ctx: &egui::Context) -> anyhow::Result<String> {
    let out = cmd.stdin(Stdio::null()).output()?;
    for line in String::from_utf8_lossy(&out.stderr).lines() {
        let _ = tx.send(WorkerMsg::Log(format!("  {line}")));
        ctx.request_repaint();
    }
    if !out.status.success() {
        anyhow::bail!("command failed ({})", out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn worker(cfg: Config, tx: Sender<WorkerMsg>, ctx: egui::Context, procs: ProcHandles) {
    let result = worker_inner(&cfg, &tx, &ctx, &procs, "manual");
    // Always tear the server down, success or not.
    if cfg.b_ssh && !cfg.b_host.is_empty() {
        let _ = ssh_cmd(&cfg.b_host, "pkill -x linkbench")
            .stdin(Stdio::null())
            .output();
    }
    procs.kill_all();
    match result {
        Ok(r) => {
            let _ = tx.send(WorkerMsg::Done(Box::new(r)));
        }
        Err(e) => {
            let _ = tx.send(WorkerMsg::Failed(format!("{e:#}")));
        }
    }
    ctx.request_repaint();
}

fn worker_inner(
    cfg: &Config,
    tx: &Sender<WorkerMsg>,
    ctx: &egui::Context,
    procs: &ProcHandles,
    label: &str,
) -> anyhow::Result<Results> {
    let log = |s: String| {
        let _ = tx.send(WorkerMsg::Log(s));
        ctx.request_repaint();
    };
    let status = |s: &str| {
        let _ = tx.send(WorkerMsg::Status(s.into()));
        ctx.request_repaint();
    };

    // --- deploy
    if cfg.deploy {
        for (is_ssh, host) in [(cfg.a_ssh, &cfg.a_host), (cfg.b_ssh, &cfg.b_host)] {
            if !is_ssh || host.is_empty() {
                continue;
            }
            status(&format!("deploying binary to {host}…"));
            log(format!("scp {} {host}:{}", cfg.local_bin, cfg.remote_bin));
            let dest = format!("{host}:{}", cfg.remote_bin.trim_start_matches("~/"));
            let mut scp = Command::new("scp");
            scp.arg("-oBatchMode=yes").arg(&cfg.local_bin).arg(&dest);
            run_logged(scp, tx, ctx).map_err(|e| anyhow::anyhow!("deploy to {host} failed: {e}"))?;
        }
    }

    // --- apply CPU tuning profile on both nodes (needs passwordless sudo)
    if cfg.tune_profile != "leave" && !cfg.tune_profile.is_empty() {
        status(&format!("applying cpu tuning profile {}…", cfg.tune_profile));
        for (is_ssh, host, label) in [
            (cfg.a_ssh, &cfg.a_host, "A"),
            (cfg.b_ssh, &cfg.b_host, "B"),
        ] {
            let result = if is_ssh {
                if host.is_empty() {
                    continue;
                }
                run_logged(
                    ssh_cmd(host, &format!("sudo -n {} tune --profile {}", cfg.remote_bin, cfg.tune_profile)),
                    tx,
                    ctx,
                )
            } else {
                let mut c = Command::new("sudo");
                c.arg("-n").arg(&cfg.local_bin).arg("tune").arg("--profile").arg(&cfg.tune_profile);
                run_logged(c, tx, ctx)
            };
            match result {
                Ok(out) => {
                    if let Some(line) = out.lines().last() {
                        log(format!("node {label} tuning: {line}"));
                    }
                }
                Err(e) => log(format!(
                    "node {label} tuning FAILED (needs passwordless sudo): {e}"
                )),
            }
        }
    }

    // --- start server on B
    status("starting server on node B…");
    if cfg.b_ssh {
        if cfg.b_host.is_empty() {
            anyhow::bail!("node B SSH target is empty");
        }
        let start = format!(
            "pkill -x linkbench 2>/dev/null; sleep 0.2; \
             nohup {} serve --port {} --region-mb {} >/tmp/linkbench-serve.log 2>&1 </dev/null & \
             sleep 0.5; pgrep -x linkbench >/dev/null && echo SERVER_UP || \
             (echo SERVER_FAILED; cat /tmp/linkbench-serve.log)",
            cfg.remote_bin, cfg.port, cfg.region_mb
        );
        let out = run_logged(ssh_cmd(&cfg.b_host, &start), tx, ctx)?;
        log(format!("  {}", out.trim()));
        if !out.contains("SERVER_UP") {
            anyhow::bail!("server failed to start on {}: {}", cfg.b_host, out.trim());
        }
    } else {
        let child = Command::new(&cfg.local_bin)
            .args([
                "serve",
                "--port",
                &cfg.port.to_string(),
                "--region-mb",
                &cfg.region_mb.to_string(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| anyhow::anyhow!("start local server ({}): {e}", cfg.local_bin))?;
        *procs.local_server.lock().unwrap() = Some(child);
        std::thread::sleep(std::time::Duration::from_millis(300));
    }

    // --- run client on A
    status("running benchmark…");
    let target = if cfg.data_addr.is_empty() {
        if cfg.b_ssh {
            ssh_host_part(&cfg.b_host).to_string()
        } else {
            "127.0.0.1".into()
        }
    } else {
        cfg.data_addr.clone()
    };
    let mut args = format!(
        "run {target}:{} --transport {} --duration {} --streams {} --qps {} --region-mb {}",
        cfg.port, cfg.transport, cfg.duration, cfg.streams, cfg.qps, cfg.region_mb
    );
    if !cfg.rdma_device.is_empty() {
        args.push_str(&format!(" --device {}", cfg.rdma_device));
    }
    if let Ok(g) = cfg.gid_index.trim().parse::<i32>() {
        args.push_str(&format!(" --gid-index {g}"));
    }
    if cfg.quick {
        args.push_str(" --quick");
    }
    args.push_str(" --json");

    let mut cmd = if cfg.a_ssh {
        if cfg.a_host.is_empty() {
            anyhow::bail!("node A SSH target is empty");
        }
        ssh_cmd(
            &cfg.a_host,
            &format!("LINKBENCH_NO_HISTORY=1 {} {args}", cfg.remote_bin),
        )
    } else {
        let mut c = Command::new(&cfg.local_bin);
        c.env("LINKBENCH_NO_HISTORY", "1");
        c.args(args.split_whitespace());
        c
    };
    log(format!("client: {args}"));

    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("start client: {e}"))?;
    let stderr = child.stderr.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    *procs.client.lock().unwrap() = Some(child);

    let tx2 = tx.clone();
    let ctx2 = ctx.clone();
    let err_thread = std::thread::spawn(move || {
        for line in std::io::BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = tx2.send(WorkerMsg::Log(line));
            ctx2.request_repaint();
        }
    });
    let out_thread = std::thread::spawn(move || {
        let mut s = String::new();
        use std::io::Read;
        let _ = std::io::BufReader::new(stdout).read_to_string(&mut s);
        s
    });

    // Poll rather than block on wait() so the Stop button (which takes and
    // kills the child under the same lock) stays responsive.
    let status_code = loop {
        let mut guard = procs.client.lock().unwrap();
        match guard.as_mut() {
            Some(child) => {
                if let Some(st) = child.try_wait()? {
                    break st;
                }
            }
            None => anyhow::bail!("stopped by user"),
        }
        drop(guard);
        std::thread::sleep(std::time::Duration::from_millis(80));
    };
    let _ = err_thread.join();
    let json = out_thread.join().unwrap_or_default();
    procs.client.lock().unwrap().take();

    if !status_code.success() {
        anyhow::bail!("benchmark client exited with {status_code}");
    }
    let results: Results = serde_json::from_str(json.trim())
        .map_err(|e| anyhow::anyhow!("could not parse results JSON: {e}"))?;

    // Auto-save alongside the GUI's data, and archive to the run history.
    if let Some(dir) = dirs_data_dir() {
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("last-run.json");
        if std::fs::write(&path, json.trim()).is_ok() {
            log(format!("results saved to {}", path.display()));
        }
    }
    match linkbench::history::append(&results, label, &cfg.notes) {
        Ok(p) => log(format!("archived to {}", p.display())),
        Err(e) => log(format!("history append failed: {e:#}")),
    }
    Ok(results)
}

fn dirs_data_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share/linkbench"))
}

// ---------------------------------------------------------------------- app

#[derive(PartialEq)]
enum RunState {
    Idle,
    Running,
}

struct App {
    cfg: Config,
    state: RunState,
    status: String,
    log: Vec<String>,
    results: Option<Results>,
    run_seq: u64,
    probe: Option<ProbeOutcome>,
    probing: bool,
    checked: Vec<bool>,
    batch: Vec<BatchEntry>,
    batch_sel: usize,
    rx: Option<Receiver<WorkerMsg>>,
    procs: ProcHandles,
    error: Option<String>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let cfg = cc
            .storage
            .and_then(|s| s.get_string("config"))
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let results = dirs_data_dir()
            .map(|d| d.join("last-run.json"))
            .filter(|p| p.exists())
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok());
        Self {
            cfg,
            state: RunState::Idle,
            status: String::new(),
            log: Vec::new(),
            results,
            run_seq: 0,
            probe: None,
            probing: false,
            checked: Vec::new(),
            batch: Vec::new(),
            batch_sel: 0,
            rx: None,
            procs: ProcHandles::default(),
            error: None,
        }
    }

    fn start(&mut self, ctx: &egui::Context) {
        // No link row selected: benchmark B's SSH host, never a stale
        // address left over from an earlier probe selection.
        self.cfg.data_addr.clear();
        self.cfg.transport = "auto".into();
        self.log.clear();
        self.error = None;
        self.status = "starting…".into();
        self.state = RunState::Running;
        let (tx, rx) = std::sync::mpsc::channel();
        self.rx = Some(rx);
        self.procs = ProcHandles::default();
        let cfg = self.cfg.clone();
        let ctx = ctx.clone();
        let procs = self.procs.clone();
        std::thread::spawn(move || worker(cfg, tx, ctx, procs));
    }

    fn start_batch(&mut self, ctx: &egui::Context, rows: Vec<(LinkPair, String)>) {
        self.log.clear();
        self.error = None;
        self.batch.clear();
        self.batch_sel = 0;
        self.status = format!("starting batch of {}…", rows.len());
        self.state = RunState::Running;
        let (tx, rx) = std::sync::mpsc::channel();
        self.rx = Some(rx);
        self.procs = ProcHandles::default();
        let cfg = self.cfg.clone();
        let ctx = ctx.clone();
        let procs = self.procs.clone();
        std::thread::spawn(move || batch_worker(cfg, rows, tx, ctx, procs));
    }

    fn poll(&mut self) {
        if let Some(rx) = &self.rx {
            for msg in rx.try_iter() {
                match msg {
                    WorkerMsg::Log(l) => self.log.push(l),
                    WorkerMsg::Status(s) => self.status = s,
                    WorkerMsg::Done(r) => {
                        self.results = Some(*r);
                        self.run_seq += 1;
                        self.status = "done".into();
                        self.state = RunState::Idle;
                    }
                    WorkerMsg::Failed(e) => {
                        self.error = Some(e);
                        self.status = "failed".into();
                        self.state = RunState::Idle;
                    }
                    WorkerMsg::Probed(o) => {
                        self.checked.clear();
                        self.probe = Some(*o);
                        self.probing = false;
                        self.status = "probe complete".into();
                    }
                    WorkerMsg::BatchItem { label, results } => {
                        self.batch.push(BatchEntry { label, results: *results });
                        self.batch_sel = self.batch.len() - 1;
                        self.run_seq += 1;
                    }
                    WorkerMsg::BatchDone => {
                        self.status = format!("batch complete ({} tested)", self.batch.len());
                        self.state = RunState::Idle;
                    }
                }
            }
        }
    }
}

// Status palette (mode-invariant) and per-mode series hues, validated with
// the dataviz palette checker against the light/dark surfaces.
const GOOD: Color32 = Color32::from_rgb(0x0c, 0xa3, 0x0c);
const WARN: Color32 = Color32::from_rgb(0xfa, 0xb2, 0x19);
const BAD: Color32 = Color32::from_rgb(0xd0, 0x3b, 0x3b);
const ACCENT: Color32 = Color32::from_rgb(0x39, 0x87, 0xe5); // dark-mode series blue

fn series_color(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::from_rgb(0x39, 0x87, 0xe5)
    } else {
        Color32::from_rgb(0x2a, 0x78, 0xd6)
    }
}

fn temp_color(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::from_rgb(0xd9, 0x59, 0x26)
    } else {
        Color32::from_rgb(0xeb, 0x68, 0x34)
    }
}

fn muted(ui: &egui::Ui) -> Color32 {
    ui.visuals().weak_text_color()
}

fn tile_fill(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::from_gray(0x24)
    } else {
        Color32::from_gray(0xf2)
    }
}

fn section_title(text: &str) -> RichText {
    RichText::new(text).size(11.5).strong()
}

fn section<R>(
    ui: &mut egui::Ui,
    title: &str,
    default_open: bool,
    add: impl FnOnce(&mut egui::Ui) -> R,
) {
    egui::CollapsingHeader::new(section_title(title))
        .id_salt(title)
        .default_open(default_open)
        .show(ui, add);
    ui.add_space(2.0);
}

fn kpi(ui: &mut egui::Ui, label: &str, value: String, sub: Option<String>) {
    let accent = series_color(ui);
    egui::Frame::group(ui.style())
        .fill(tile_fill(ui))
        .stroke(egui::Stroke::NONE)
        .inner_margin(egui::Margin::symmetric(14, 9))
        .show(ui, |ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new(value).size(19.0).strong().color(accent));
                if let Some(s) = sub {
                    ui.label(RichText::new(s).size(12.0).color(muted(ui)));
                }
                ui.label(RichText::new(label).size(10.5).weak());
            });
        });
}

fn kpi_rate(ui: &mut egui::Ui, label: &str, bps: f64) {
    kpi(ui, label, fmt_bits(bps), Some(format!("{}/s", fmt_bytes(bps))));
}

/// Sustained-throughput chart (one bar per bucket) with an aligned but
/// separate temperature strip below — two aligned charts, never dual-axis.
fn timeline_chart(
    ui: &mut egui::Ui,
    label: &str,
    bps: &[f64],
    steady: usize,
    temps: &[f32],
    bucket_ms: u64,
) {
    if bps.is_empty() {
        return;
    }
    let accent = series_color(ui);
    let stats = timeline_stats(bps, steady);
    let median = stats.as_ref().map(|s| s.median).unwrap_or(0.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).strong().size(12.0));
        ui.label(RichText::new(fmt_rate(median)).size(12.0).color(muted(ui)));
        if let Some(s) = &stats {
            let (txt, color) = if s.stalls > 0 {
                (format!("{} stalls, dip −{:.0}%", s.stalls, s.dip_pct), BAD)
            } else if s.dip_pct > 20.0 {
                (format!("dip −{:.0}%", s.dip_pct), WARN)
            } else {
                (format!("steady · dip −{:.0}%", s.dip_pct), GOOD)
            };
            ui.label(RichText::new(txt).size(11.0).color(color));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                RichText::new(format!(
                    "{:.0} s · {bucket_ms} ms buckets",
                    bps.len() as f64 * bucket_ms as f64 / 1e3
                ))
                .size(10.5)
                .weak(),
            );
        });
    });

    let width = ui.available_width();
    let (resp, painter) = ui.allocate_painter(egui::vec2(width, 64.0), egui::Sense::hover());
    let rect = resp.rect.shrink(1.0);
    painter.rect_filled(rect, 4.0, tile_fill(ui));
    let max = bps.iter().cloned().fold(1e-9, f64::max);
    let n = bps.len();
    let bw = rect.width() / n as f32;
    let gap = if bw > 3.0 { 1.0 } else { 0.5 };
    for (i, v) in bps.iter().enumerate() {
        let h = ((v / max) as f32 * (rect.height() - 6.0)).max(1.5);
        let x0 = rect.left() + i as f32 * bw;
        let color = if steady > 0 && i >= steady {
            muted(ui).gamma_multiply(0.45) // lane wind-down, not link behaviour
        } else {
            accent
        };
        painter.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(x0 + gap / 2.0, rect.bottom() - h - 2.0),
                egui::pos2((x0 + bw - gap / 2.0).max(x0 + 1.0), rect.bottom() - 2.0),
            ),
            2.0,
            color,
        );
    }
    if resp.hovered() {
        resp.on_hover_text(format!("peak bucket {}", fmt_rate(max)));
    }

    // Aligned temperature strip (own scale, own chart).
    if temps.len() > 1 {
        let tcol = temp_color(ui);
        let tmin = temps.iter().cloned().fold(f32::MAX, f32::min);
        let tmax = temps.iter().cloned().fold(f32::MIN, f32::max);
        let span = (tmax - tmin).max(1.0);
        let (resp, painter) = ui.allocate_painter(egui::vec2(width, 24.0), egui::Sense::hover());
        let rect = resp.rect.shrink(1.0);
        painter.rect_filled(rect, 4.0, tile_fill(ui).gamma_multiply(0.6));
        let pts: Vec<egui::Pos2> = temps
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let x = rect.left() + rect.width() * (i as f32 + 0.5) / temps.len() as f32;
                let y = rect.bottom() - 5.0 - ((t - tmin) / span) * (rect.height() - 12.0);
                egui::pos2(x, y)
            })
            .collect();
        painter.add(egui::Shape::line(pts, egui::Stroke::new(1.5, tcol)));
        painter.text(
            egui::pos2(rect.left() + 6.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            "temp",
            egui::FontId::proportional(9.5),
            muted(ui),
        );
        painter.text(
            egui::pos2(rect.right() - 6.0, rect.center().y),
            egui::Align2::RIGHT_CENTER,
            format!("{tmin:.0}–{tmax:.0} °C"),
            egui::FontId::proportional(10.0),
            muted(ui),
        );
        resp.on_hover_text("hottest sensor on the receiving node");
    }
}

/// Fixed-column bar row: [label | bar track | value]. The value column is
/// reserved space, so bars can never collide with the text.
fn hbar(ui: &mut egui::Ui, label: &str, label_w: f32, frac: f32, text: String, color: Color32) {
    const VALUE_W: f32 = 86.0;
    const H: f32 = 15.0;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.allocate_ui_with_layout(
            egui::vec2(label_w, H),
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| {
                ui.add(
                    egui::Label::new(RichText::new(label).size(11.0).color(muted(ui)))
                        .truncate(),
                );
            },
        );
        let bar_w = (ui.available_width() - VALUE_W - 8.0).max(40.0);
        let (rect, _) = ui.allocate_exact_size(egui::vec2(bar_w, H), egui::Sense::hover());
        let track = rect.shrink2(egui::vec2(0.0, 2.5));
        ui.painter().rect_filled(track, 4.0, tile_fill(ui));
        let mut fill = track;
        fill.set_width((track.width() * frac.clamp(0.015, 1.0)).max(3.0));
        ui.painter().rect_filled(fill, 4.0, color);
        ui.allocate_ui_with_layout(
            egui::vec2(VALUE_W, H),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.add(egui::Label::new(RichText::new(text).size(11.0)).truncate());
            },
        );
    });
}

impl eframe::App for App {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        if let Ok(s) = serde_json::to_string(&self.cfg) {
            storage.set_string("config", s);
        }
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll();
        if self.state == RunState::Running {
            ctx.request_repaint_after(std::time::Duration::from_millis(150));
        }

        egui::TopBottomPanel::top("cfg").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading(RichText::new("linkbench").color(ACCENT).strong());
                ui.label(RichText::new("how fast and useful is the link between these two computers?").weak());
            });
            ui.add_space(6.0);

            let cfg = &mut self.cfg;
            egui::Grid::new("nodes").num_columns(4).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Node A (runs the test)");
                ui.selectable_value(&mut cfg.a_ssh, false, "this machine");
                ui.selectable_value(&mut cfg.a_ssh, true, "over SSH");
                ui.add_enabled(
                    cfg.a_ssh,
                    egui::TextEdit::singleline(&mut cfg.a_host)
                        .hint_text("user@host  e.g. dad@m5")
                        .desired_width(200.0),
                );
                ui.end_row();

                ui.label("Node B (serves)");
                ui.selectable_value(&mut cfg.b_ssh, false, "this machine");
                ui.selectable_value(&mut cfg.b_ssh, true, "over SSH");
                ui.add_enabled(
                    cfg.b_ssh,
                    egui::TextEdit::singleline(&mut cfg.b_host)
                        .hint_text("user@host  e.g. dad@m6")
                        .desired_width(200.0),
                );
                ui.end_row();

            });

            // ---- probe & link discovery
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let can = !self.probing && self.state == RunState::Idle;
                if ui.add_enabled(can, egui::Button::new("🔍 Probe nodes")).clicked() {
                    self.probing = true;
                    self.probe = None;
                    self.log.clear();
                    self.status = "probing…".into();
                    let (tx, rx) = std::sync::mpsc::channel();
                    self.rx = Some(rx);
                    let cfg = self.cfg.clone();
                    let ctx2 = ctx.clone();
                    std::thread::spawn(move || probe_worker(cfg, tx, ctx2));
                }
                if self.probing {
                    ui.spinner();
                }
                ui.label(
                    RichText::new("discovers links between A and B, sudo state, and what each node sees")
                        .weak()
                        .size(10.5),
                );
            });

            let mut stage_on: Option<(bool, String, char)> = None;
            let mut tb_up_clicked = false;
            let mut roce_up_clicked: Option<(String, String)> = None;
            let probe_links: Option<Vec<LinkPair>> =
                self.probe.as_ref().map(|o| o.links.clone());
            if let Some(o) = &self.probe {
                ui.add_space(4.0);
                for (node, label, is_ssh, host) in [
                    (&o.a, 'A', self.cfg.a_ssh, self.cfg.a_host.clone()),
                    (&o.b, 'B', self.cfg.b_ssh, self.cfg.b_host.clone()),
                ] {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(format!("{label}:")).strong().size(11.5));
                        match node {
                            Ok(p) => {
                                ui.label(RichText::new(&p.hostname).strong().size(11.5));
                                if p.sudo_passwordless {
                                    ui.label(RichText::new("sudo ok").color(GOOD).size(11.0));
                                } else {
                                    ui.label(
                                        RichText::new("no passwordless sudo").color(WARN).size(11.0),
                                    );
                                    if ui.small_button("stage toggle script").clicked() {
                                        stage_on = Some((is_ssh, host.clone(), label));
                                    }
                                }
                                if !p.tuning.is_empty() {
                                    ui.label(RichText::new(&p.tuning).weak().size(10.5));
                                }
                            }
                            Err(e) => {
                                ui.label(RichText::new(e).color(BAD).size(11.0));
                            }
                        }
                    });
                }
            }
            if let Some(links) = &probe_links {
                ui.add_space(4.0);
                if links.is_empty() {
                    ui.label(RichText::new("no shared links discovered").weak().size(11.0));
                } else {
                    // (link, mode) test rows; RDMA-capable links contribute
                    // one row per mode.
                    let rows: Vec<(usize, &'static str)> = links
                        .iter()
                        .enumerate()
                        .flat_map(|(i, l)| l.modes().into_iter().map(move |m| (i, m)))
                        .collect();
                    if self.checked.len() != rows.len() {
                        self.checked = vec![true; rows.len()];
                    }
                    ui.label(
                        RichText::new("DISCOVERED LINKS — check the ones to test")
                            .weak()
                            .size(10.5),
                    );
                    egui::Grid::new("linktable")
                        .striped(true)
                        .num_columns(7)
                        .spacing([14.0, 4.0])
                        .show(ui, |ui| {
                            for h in ["", "link", "mode", "node A", "node B", "ping", "notes"] {
                                ui.label(RichText::new(h).weak().size(10.5));
                            }
                            ui.end_row();
                            for (row_idx, (li, mode)) in rows.iter().enumerate() {
                                let l = &links[*li];
                                ui.checkbox(&mut self.checked[row_idx], "");
                                ui.label(RichText::new(&l.kind_label).strong().size(11.5));
                                ui.label(RichText::new(*mode).monospace().size(11.0));
                                ui.label(RichText::new(&l.a_desc).size(11.0));
                                ui.label(RichText::new(&l.b_desc).size(11.0));
                                ui.label(
                                    RichText::new(
                                        l.rtt_ms
                                            .map(|r| format!("{r:.2} ms"))
                                            .unwrap_or_else(|| "–".into()),
                                    )
                                    .monospace()
                                    .size(11.0),
                                );
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(&l.detail).weak().size(10.0));
                                    if l.softroce_offerable
                                        && ui
                                            .small_button("+ Soft-RoCE")
                                            .on_hover_text(
                                                "enable software RoCE on both ends (persisted), \
                                                 then re-probe so an rdma row appears",
                                            )
                                            .clicked()
                                    {
                                        roce_up_clicked =
                                            Some((l.a_iface.clone(), l.b_iface.clone()));
                                    }
                                });
                                ui.end_row();
                            }
                            // Unusable links: informational rows with actions.
                            for l in links.iter().filter(|l| l.b_addr.is_none()) {
                                ui.label(RichText::new("–").weak());
                                ui.label(RichText::new(&l.kind_label).weak().size(11.5));
                                ui.label(RichText::new("–").weak());
                                ui.label(RichText::new(&l.a_desc).weak().size(11.0));
                                ui.label(RichText::new(&l.b_desc).weak().size(11.0));
                                ui.label(RichText::new("–").weak());
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(&l.detail).weak().size(10.0));
                                    if l.kind == "thunderbolt"
                                        && ui
                                            .small_button("⚡ bring up")
                                            .on_hover_text(
                                                "10.111.11.1 ⇔ .2, persisted; then re-probes",
                                            )
                                            .clicked()
                                    {
                                        tb_up_clicked = true;
                                    }
                                });
                                ui.end_row();
                            }
                        });
                }
            }
            if tb_up_clicked && !self.probing {
                self.probing = true;
                self.status = "bringing up thunderbolt…".into();
                let (tx, rx) = std::sync::mpsc::channel();
                self.rx = Some(rx);
                let cfg = self.cfg.clone();
                let ctx2 = ctx.clone();
                std::thread::spawn(move || tb_bringup_worker(cfg, tx, ctx2));
            }
            if let Some((a_if, b_if)) = roce_up_clicked {
                if !self.probing {
                    self.probing = true;
                    self.status = "enabling Soft-RoCE…".into();
                    let (tx, rx) = std::sync::mpsc::channel();
                    self.rx = Some(rx);
                    let cfg = self.cfg.clone();
                    let ctx2 = ctx.clone();
                    std::thread::spawn(move || roce_bringup_worker(cfg, a_if, b_if, tx, ctx2));
                }
            }
            if let Some((is_ssh, host, label)) = stage_on {
                match stage_sudo_script(&self.cfg, is_ssh, &host) {
                    Ok(msg) => self.log.push(format!("node {label}: {msg}")),
                    Err(e) => self.log.push(format!("node {label}: staging failed: {e:#}")),
                }
            }

            egui::CollapsingHeader::new("advanced").show(ui, |ui| {
                egui::Grid::new("adv").num_columns(2).spacing([10.0, 4.0]).show(ui, |ui| {
                    ui.label("quick run").on_hover_text(
                        "abbreviated benchmark (~3x faster): shorter sustained runs, fewer \
                         latency/all-reduce samples — indicative, not the official score",
                    );
                    ui.checkbox(&mut self.cfg.quick, "")
                        .on_hover_text("abbreviated benchmark (~3x faster), noisier numbers");
                    ui.end_row();
                    ui.label("deploy binary first").on_hover_text(
                        "scp the local linkbench CLI to each SSH node before probing/running, \
                         so the nodes always run the same version as this GUI",
                    );
                    ui.checkbox(&mut self.cfg.deploy, "")
                        .on_hover_text("copy the CLI binary to the nodes before each probe/run");
                    ui.end_row();
                    ui.label("TCP streams").on_hover_text(
                        "parallel TCP connections for the data plane; one stream rarely fills \
                         a fast link — 4 saturates 100GbE-class paths",
                    );
                    ui.add(egui::DragValue::new(&mut self.cfg.streams).range(1..=32))
                        .on_hover_text("parallel TCP data connections (default 4)");
                    ui.end_row();
                    ui.label("RDMA queue pairs").on_hover_text(
                        "parallel RDMA QPs, each driven by its own thread; big win on \
                         CPU-bound Soft-RoCE (rxe), mostly neutral on hardware RDMA",
                    );
                    ui.add(egui::DragValue::new(&mut self.cfg.qps).range(1..=32))
                        .on_hover_text("parallel RDMA queue pairs (default 4)");
                    ui.end_row();
                    ui.label("local linkbench binary").on_hover_text(
                        "path to the linkbench CLI on this machine; used for local runs and \
                         as the source for deploys",
                    );
                    ui.add(egui::TextEdit::singleline(&mut self.cfg.local_bin).desired_width(380.0))
                        .on_hover_text("the CLI binary this GUI drives and deploys");
                    ui.end_row();
                    ui.label("remote binary path").on_hover_text(
                        "where the CLI lives on the SSH nodes (deploy copies it here; \
                         serve/probe/tune run it from here)",
                    );
                    ui.add(egui::TextEdit::singleline(&mut self.cfg.remote_bin).desired_width(380.0))
                        .on_hover_text("CLI path on the remote nodes");
                    ui.end_row();
                    ui.label("control port").on_hover_text(
                        "TCP port for the coordination channel between the two nodes \
                         (benchmark data uses separate connections/QPs)",
                    );
                    ui.add(egui::DragValue::new(&mut self.cfg.port).range(1024..=65535))
                        .on_hover_text("coordination channel port (default 7842)");
                    ui.end_row();
                    ui.label("seconds per test").on_hover_text(
                        "target duration for each bandwidth test; longer = steadier numbers, \
                         slower runs (sustained runs are 8 s regardless)",
                    );
                    ui.add(egui::Slider::new(&mut self.cfg.duration, 0.3..=5.0))
                        .on_hover_text("per-test time budget");
                    ui.end_row();
                    ui.label("RDMA device").on_hover_text(
                        "override which RDMA device to use; blank = pick the one riding the \
                         interface that routes to the target (recommended)",
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut self.cfg.rdma_device)
                            .hint_text("blank = auto (route-matched)")
                            .desired_width(180.0),
                    )
                    .on_hover_text("explicit RDMA device name, e.g. rxe0 or mlx5_0");
                    ui.end_row();
                    ui.label("RoCE GID index").on_hover_text(
                        "override the RoCE address entry; blank = auto-pick a RoCE v2 \
                         IPv4-mapped GID (recommended)",
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut self.cfg.gid_index)
                            .hint_text("blank = auto (RoCE v2 / IPv4)")
                            .desired_width(180.0),
                    )
                    .on_hover_text("numeric GID table index");
                    ui.end_row();
                    ui.label("buffer region (MiB)").on_hover_text(
                        "size of the pinned transfer buffers per direction; also caps the \
                         largest single message (region / QPs on the RDMA path)",
                    );
                    ui.add(egui::DragValue::new(&mut self.cfg.region_mb).range(16..=2048))
                        .on_hover_text("buffer region size (default 128 MiB)");
                    ui.end_row();
                    ui.label("CPU tuning profile").on_hover_text(
                        "applied to both nodes via sudo before the run. balanced = performance \
                         governor + deep C-states off (kills multi-hundred-µs wake-up lag); \
                         latency = also gate mid C-states (bursty RPC); default = powersave. \
                         Runtime-only — resets on reboot.",
                    );
                    ui.horizontal(|ui| {
                        egui::ComboBox::from_id_salt("tune_profile")
                            .selected_text(&self.cfg.tune_profile)
                            .width(110.0)
                            .show_ui(ui, |ui| {
                                for (t, tip) in [
                                    ("leave", "don't touch CPU settings"),
                                    ("balanced", "performance governor, deep C-states off — best all-round"),
                                    ("latency", "also gate mid C-states — fastest wake-up, costs some boost"),
                                    ("default", "restore powersave defaults"),
                                ] {
                                    ui.selectable_value(&mut self.cfg.tune_profile, t.into(), t)
                                        .on_hover_text(tip);
                                }
                            });
                        ui.label(
                            RichText::new("applied to both nodes before the run (sudo -n); runtime-only")
                                .weak()
                                .size(10.5),
                        );
                    });
                    ui.end_row();
                });
            });

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("notes").weak().size(11.0)).on_hover_text(
                    "free-form experiment notes stored with every run in the history \
                     archive — e.g. \"riser B\", \"new DAC cable\", \"C2 disabled\"",
                );
                ui.add(
                    egui::TextEdit::singleline(&mut self.cfg.notes)
                        .hint_text("what are you testing? (saved with each run)")
                        .desired_width(340.0),
                )
                .on_hover_text("stored in ~/.local/share/linkbench/history/ with each result");
            });
            ui.horizontal(|ui| {
                match self.state {
                    RunState::Idle => {
                        let selected_rows: Vec<(LinkPair, String)> = self
                            .probe
                            .as_ref()
                            .map(|o| {
                                o.links
                                    .iter()
                                    .flat_map(|l| {
                                        l.modes().into_iter().map(move |m| (l.clone(), m.to_string()))
                                    })
                                    .zip(self.checked.iter())
                                    .filter(|(_, c)| **c)
                                    .map(|(r, _)| r)
                                    .collect()
                            })
                            .unwrap_or_default();
                        let label = if selected_rows.is_empty() {
                            "▶  Run benchmark".to_string()
                        } else {
                            format!("▶  Run Test ({} link modes)", selected_rows.len())
                        };
                        let run = egui::Button::new(RichText::new(label).size(15.0).strong())
                            .fill(ACCENT.gamma_multiply(0.25));
                        if ui.add(run).clicked() {
                            if selected_rows.is_empty() {
                                self.start(ctx);
                            } else {
                                self.start_batch(ctx, selected_rows);
                            }
                        }
                    }
                    RunState::Running => {
                        if ui.button(RichText::new("■  Stop").size(15.0)).clicked() {
                            self.procs.kill_all();
                        }
                        ui.spinner();
                    }
                }
                let color = match (self.error.is_some(), self.state == RunState::Running) {
                    (true, _) => BAD,
                    (_, true) => WARN,
                    _ => GOOD,
                };
                ui.label(RichText::new(&self.status).color(color));
            });
            if let Some(e) = &self.error {
                ui.label(RichText::new(e).color(BAD).small());
            }
            ui.add_space(6.0);
        });

        egui::TopBottomPanel::bottom("log")
            .resizable(true)
            .default_height(90.0)
            .show(ctx, |ui| {
                egui::CollapsingHeader::new("log").default_open(true).show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .stick_to_bottom(true)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            for line in &self.log {
                                ui.label(RichText::new(line).monospace().size(11.0));
                            }
                        });
                });
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            if !self.batch.is_empty() {
                egui::ScrollArea::vertical()
                    .id_salt(("batch", self.run_seq))
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        comparison_pane(ui, &self.batch, &mut self.batch_sel);
                        ui.separator();
                        results_pane(ui, &self.batch[self.batch_sel.min(self.batch.len() - 1)].results);
                    });
                return;
            }
            let Some(r) = self.results.clone() else {
                ui.centered_and_justified(|ui| {
                    ui.label(RichText::new("no results yet — configure the nodes above and press Run").weak());
                });
                return;
            };
            egui::ScrollArea::vertical()
                .id_salt(("results", self.run_seq))
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    results_pane(ui, &r);
                });
        });
    }
}

/// The shared comparison: one row per tested link mode, best value per
/// column highlighted, plus "best for" summary lines.
fn comparison_pane(ui: &mut egui::Ui, batch: &[BatchEntry], sel: &mut usize) {
    ui.add_space(4.0);
    ui.label(section_title("TESTED LINKS — click a row for full details"));
    ui.add_space(2.0);

    struct Col {
        head: &'static str,
        val: fn(&Results) -> Option<f64>,
        fmt: fn(f64) -> String,
        higher_better: bool,
    }
    let lat16 = |r: &Results| {
        r.latency
            .iter()
            .find(|l| l.size == 16 * 1024)
            .map(|l| l.oneway_p50_us)
    };
    let wake = |r: &Results| {
        match (r.idle_gaps.first(), r.idle_gaps.iter().find(|g| g.gap_ms >= 100)) {
            (Some(h), Some(c)) => Some((c.rtt_p50_us - h.rtt_p50_us).max(0.0)),
            _ => None,
        }
    };
    let cols: [Col; 7] = [
        Col {
            head: "score",
            val: |r| r.score.as_ref().map(|s| s.score),
            fmt: |v| format!("{v:.1}"),
            higher_better: true,
        },
        Col {
            head: "up",
            val: |r| Some(r.uni_c2s_bps),
            fmt: |v| fmt_bits(v),
            higher_better: true,
        },
        Col {
            head: "down",
            val: |r| Some(r.uni_s2c_bps),
            fmt: |v| fmt_bits(v),
            higher_better: true,
        },
        Col {
            head: "16K 1-way",
            val: lat16,
            fmt: |v| format!("{v:.0} µs"),
            higher_better: false,
        },
        Col {
            head: "16K allreduce",
            val: |r| Some(r.allreduce_16k_us),
            fmt: |v| format!("{v:.0} µs"),
            higher_better: false,
        },
        Col {
            head: "msg rate",
            val: |r| Some(r.msg_rate_per_s),
            fmt: |v| format!("{:.1} M/s", v / 1e6),
            higher_better: true,
        },
        Col {
            head: "wake-up",
            val: wake,
            fmt: |v| format!("+{v:.0} µs"),
            higher_better: false,
        },
    ];

    // Best row per column.
    let best: Vec<Option<usize>> = cols
        .iter()
        .map(|c| {
            let mut best: Option<(usize, f64)> = None;
            for (i, e) in batch.iter().enumerate() {
                if let Some(v) = (c.val)(&e.results) {
                    let better = best.map_or(true, |(_, bv)| {
                        if c.higher_better { v > bv } else { v < bv }
                    });
                    if better {
                        best = Some((i, v));
                    }
                }
            }
            best.map(|(i, _)| i)
        })
        .collect();

    let accent = series_color(ui);
    egui::Grid::new("batchtable")
        .striped(true)
        .num_columns(cols.len() + 1)
        .spacing([16.0, 5.0])
        .show(ui, |ui| {
            ui.label(RichText::new("link · mode").weak().size(10.5));
            for c in &cols {
                ui.label(RichText::new(c.head).weak().size(10.5));
            }
            ui.end_row();
            for (i, e) in batch.iter().enumerate() {
                if ui
                    .selectable_label(*sel == i, RichText::new(&e.label).strong().size(11.5))
                    .clicked()
                {
                    *sel = i;
                }
                for (ci, c) in cols.iter().enumerate() {
                    let text = (c.val)(&e.results)
                        .map(c.fmt)
                        .unwrap_or_else(|| "–".into());
                    let rich = RichText::new(text).monospace().size(11.5);
                    if best[ci] == Some(i) && batch.len() > 1 {
                        ui.label(rich.color(accent).strong());
                    } else {
                        ui.label(rich);
                    }
                }
                ui.end_row();
            }
        });

    if batch.len() > 1 {
        ui.add_space(4.0);
        let by = |f: fn(&Results) -> Option<f64>, hi: bool| -> Option<&BatchEntry> {
            batch
                .iter()
                .filter(|e| f(&e.results).is_some())
                .max_by(|a, b| {
                    let (av, bv) = (f(&a.results).unwrap(), f(&b.results).unwrap());
                    let ord = av.partial_cmp(&bv).unwrap_or(std::cmp::Ordering::Equal);
                    if hi { ord } else { ord.reverse() }
                })
        };
        let mut lines: Vec<String> = Vec::new();
        if let Some(e) = by(|r| r.score.as_ref().map(|s| s.score), true) {
            lines.push(format!("overall: {}", e.label));
        }
        if let Some(e) = by(|r| Some(r.uni_c2s_bps.max(r.uni_s2c_bps)), true) {
            lines.push(format!("bulk transfer: {}", e.label));
        }
        if let Some(e) = by(|r| Some(r.allreduce_16k_us), false) {
            lines.push(format!("per-token sync: {}", e.label));
        }
        if let Some(e) = by(wake, false) {
            lines.push(format!("bursty RPC: {}", e.label));
        }
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("best for").weak().size(10.5));
            for l in lines {
                ui.label(RichText::new(format!("· {l}")).size(11.0).color(series_color(ui)));
            }
        });
    }
    ui.add_space(4.0);
}

fn results_pane(ui: &mut egui::Ui, r: &Results) {
    let accent = series_color(ui);
    ui.add_space(4.0);

    // ---- header line
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!("{} – {}", r.client_host, r.server_host))
                .strong()
                .size(15.0),
        );
        egui::Frame::group(ui.style())
            .fill(accent.gamma_multiply(0.18))
            .stroke(egui::Stroke::NONE)
            .inner_margin(egui::Margin::symmetric(8, 2))
            .show(ui, |ui| {
                ui.label(RichText::new(&r.transport).size(11.0).strong().color(accent));
            });
        ui.add(egui::Label::new(RichText::new(&r.path).weak().size(10.5)).truncate());
    });
    if !r.tuning_a.is_empty() || !r.tuning_b.is_empty() {
        ui.label(
            RichText::new(format!("cpu tuning   A: {}   B: {}", r.tuning_a, r.tuning_b))
                .weak()
                .size(10.5),
        );
    }
    ui.add_space(6.0);

    // ---- KPI tiles, led by the headline score
    let card = r
        .score
        .clone()
        .unwrap_or_else(|| linkbench::score::compute(r));
    ui.horizontal_wrapped(|ui| {
        egui::Frame::group(ui.style())
            .fill(accent.gamma_multiply(0.16))
            .stroke(egui::Stroke::new(1.0, accent.gamma_multiply(0.5)))
            .inner_margin(egui::Margin::symmetric(16, 9))
            .show(ui, |ui| {
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new(format!("{:.1}", card.score))
                            .size(26.0)
                            .strong()
                            .color(accent),
                    );
                    ui.label(
                        RichText::new(format!(
                            "bw {:.1} · lat {:.1}",
                            linkbench::score::ScoreCard::axis_as_score(card.bandwidth_axis),
                            linkbench::score::ScoreCard::axis_as_score(card.latency_axis),
                        ))
                        .size(11.0)
                        .color(muted(ui)),
                    );
                    ui.label(RichText::new(format!("{} score", card.name)).size(10.5).weak());
                });
            })
            .response
            .on_hover_text(format!(
                "1.0 = two nodes on 2.5GbE · 10.0 = NVLink-bridged flagship pair\n{}{}",
                card.hint,
                if card.penalties.is_empty() {
                    String::new()
                } else {
                    format!("\npenalties: {}", card.penalties.join(", "))
                }
            ));
        kpi_rate(ui, "up (A to B)", r.uni_c2s_bps);
        kpi_rate(ui, "down (B to A)", r.uni_s2c_bps);
        kpi_rate(ui, "bidirectional", r.bidir_agg_bps);
        if let Some(l) = r.latency.first() {
            kpi(ui, "one-way latency (64 B)", format!("{:.1} µs", l.oneway_p50_us), None);
        }
        kpi(ui, "message rate", format!("{:.2} M/s", r.msg_rate_per_s / 1e6), None);
    });
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(&card.hint).size(10.5).color(muted(ui)));
        for p in &card.penalties {
            ui.label(RichText::new(format!("· {p}")).size(10.5).color(WARN));
        }
    });
    ui.add_space(8.0);

    // ---- verdicts: the answer, always visible
    ui.label(section_title("VERDICT"));
    ui.add_space(2.0);
    for (name, grade) in verdicts(r) {
        let (color, word, text) = match grade {
            Grade::Good(t) => (GOOD, "good", t),
            Grade::Ok(t) => (WARN, "fair", t),
            Grade::Poor(t) => (BAD, "poor", t),
        };
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 14.0), egui::Sense::hover());
            ui.painter().circle_filled(rect.center(), 4.0, color);
            ui.label(RichText::new(word).color(color).size(10.5).strong());
            ui.label(RichText::new(name).strong().size(12.0));
            ui.label(RichText::new(text).size(11.0).color(muted(ui)));
        });
    }
    ui.add_space(8.0);
    ui.separator();
    ui.add_space(2.0);

    // ---- expandable detail sections
    let has_timeline = !r.timeline_up_bps.is_empty() || !r.timeline_down_bps.is_empty();
    if has_timeline {
        section(ui, "SUSTAINED THROUGHPUT & TEMPERATURE", true, |ui| {
            timeline_chart(
                ui,
                &format!("up · {} to {}", r.client_host, r.server_host),
                &r.timeline_up_bps,
                r.timeline_up_steady,
                &r.temps_b_sys,
                r.timeline_bucket_ms,
            );
            ui.add_space(6.0);
            timeline_chart(
                ui,
                &format!("down · {} to {}", r.server_host, r.client_host),
                &r.timeline_down_bps,
                r.timeline_down_steady,
                &r.temps_a_sys,
                r.timeline_bucket_ms,
            );
            ui.label(
                RichText::new(
                    "receive-side throughput per bucket · dimmed tail = lane wind-down (share skew, not the link)",
                )
                .weak()
                .size(10.0),
            );
        });
    }

    section(ui, "LATENCY & MESSAGE-SIZE SWEEP", false, |ui| {
        ui.columns(2, |cols| {
            cols[0].label(RichText::new("one-way latency").size(11.0).color(muted(&cols[0])));
            egui::Grid::new("lat")
                .striped(true)
                .num_columns(4)
                .spacing([14.0, 4.0])
                .show(&mut cols[0], |ui| {
                    for h in ["size", "p50", "p99", "RTT mean"] {
                        ui.label(RichText::new(h).weak().size(10.5));
                    }
                    ui.end_row();
                    for l in &r.latency {
                        ui.label(RichText::new(fmt_size(l.size)).monospace().size(11.5));
                        ui.label(
                            RichText::new(format!("{:.1} µs", l.oneway_p50_us))
                                .monospace()
                                .size(11.5),
                        );
                        ui.label(
                            RichText::new(format!("{:.1} µs", l.oneway_p99_us))
                                .monospace()
                                .size(11.5),
                        );
                        ui.label(
                            RichText::new(format!("{:.1} µs", l.rtt_mean_us))
                                .monospace()
                                .size(11.5),
                        );
                        ui.end_row();
                    }
                });
            let accent = series_color(&cols[1]);
            cols[1].label(
                RichText::new("bandwidth by message size").size(11.0).color(muted(&cols[1])),
            );
            let peak = r.sweep.iter().map(|p| p.bps).fold(1.0, f64::max);
            for p in &r.sweep {
                hbar(
                    &mut cols[1],
                    &fmt_size(p.size),
                    58.0,
                    (p.bps / peak) as f32,
                    fmt_bits(p.bps),
                    accent,
                );
            }
        });
    });

    section(ui, "REAL-WORKLOAD BEHAVIOUR", false, |ui| {
        egui::Grid::new("scen").striped(true).num_columns(2).spacing([16.0, 5.0]).show(ui, |ui| {
            let mut row = |name: &str, val: String| {
                ui.label(RichText::new(name).size(11.5));
                ui.label(RichText::new(val).monospace().strong().size(11.5));
                ui.end_row();
            };
            if let Some(l) = r.latency.iter().find(|l| l.size == 16 * 1024) {
                row(
                    "activation hop, 16 KiB (pipeline parallel)",
                    format!("{:.1} µs one-way", l.oneway_p50_us),
                );
            }
            row("per-token all-reduce, 16 KiB (tensor parallel)", format!("{:.1} µs", r.allreduce_16k_us));
            row("gradient all-reduce, 1 GiB (data parallel)", fmt_rate(r.allreduce_1g_bps));
            row("KV-cache block, 32 MiB", format!("{:.2} ms", r.kv32m_ms));
            row("weight streaming", fmt_rate(r.uni_c2s_bps.max(r.uni_s2c_bps)));
            if let Some(l) = &r.loaded_rtt {
                row(
                    "16 KiB RTT while link is saturated",
                    format!("{:.0} µs p50 · {:.0} µs p99", l.p50_us, l.p99_us),
                );
            }
            if !r.idle_gaps.is_empty() {
                let gaps: Vec<String> = r.idle_gaps.iter().map(|g| g.gap_ms.to_string()).collect();
                let vals: Vec<String> =
                    r.idle_gaps.iter().map(|g| format!("{:.0}", g.rtt_p50_us)).collect();
                row(
                    &format!("RTT after {} ms idle", gaps.join("/")),
                    format!("{} µs p50", vals.join(" / ")),
                );
            }
        });
    });

    section(ui, "CONTEXT VS OTHER INTERCONNECTS", false, |ui| {
        let bulk = r.uni_c2s_bps.max(r.uni_s2c_bps);
        let refs: [(&str, f64, bool); 6] = [
            ("this link", bulk, true),
            ("10 GbE", 1.25e9, false),
            ("TB4/USB4", 3.8e9, false),
            ("100 GbE", 12.5e9, false),
            ("PCIe4 x16", 32e9, false),
            ("NVLink 4", 450e9, false),
        ];
        let accent = series_color(ui);
        let maxv = refs.iter().map(|x| x.1).fold(1.0, f64::max);
        for (name, bps, me) in refs {
            let frac = ((bps.max(1e6).log10() - 6.0) / (maxv.log10() - 6.0)) as f32;
            hbar(
                ui,
                name,
                74.0,
                frac,
                fmt_bits(bps),
                if me { accent } else { muted(ui).gamma_multiply(0.6) },
            );
        }
        ui.label(RichText::new("log scale · big-message bandwidth").weak().size(10.0));
    });

    ui.add_space(6.0);
}

/// Headless check of the worker pipeline: local server + local client,
/// quick run, results printed as one summary line.
fn selftest() {
    let cfg = Config {
        quick: true,
        b_ssh: false,
        deploy: false,
        port: 7913,
        ..Config::default()
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let ctx = egui::Context::default();
    let procs = ProcHandles::default();
    std::thread::spawn(move || worker(cfg, tx, ctx, procs));
    for msg in rx {
        match msg {
            WorkerMsg::Log(l) => eprintln!("log: {l}"),
            WorkerMsg::Status(s) => eprintln!("status: {s}"),
            WorkerMsg::Done(r) => {
                println!(
                    "SELFTEST OK: {} up {}/s, 16k allreduce {:.1}us, {} sweep pts",
                    r.transport,
                    fmt_bytes(r.uni_c2s_bps),
                    r.allreduce_16k_us,
                    r.sweep.len()
                );
                return;
            }
            WorkerMsg::Failed(e) => {
                println!("SELFTEST FAILED: {e}");
                std::process::exit(1);
            }
            WorkerMsg::Probed(_) | WorkerMsg::BatchItem { .. } | WorkerMsg::BatchDone => {}
        }
    }
}

/// Headless check of the probe pipeline: `--selftest-probe <A> <B>` probes
/// two SSH targets, correlates links, ping-tests them, prints the outcome.
fn selftest_probe(a: String, b: String) {
    let cfg = Config { a_ssh: true, a_host: a, b_ssh: true, b_host: b, ..Config::default() };
    let (tx, rx) = std::sync::mpsc::channel();
    let ctx = egui::Context::default();
    std::thread::spawn(move || probe_worker(cfg, tx, ctx));
    for msg in rx {
        match msg {
            WorkerMsg::Log(l) => eprintln!("log: {l}"),
            WorkerMsg::Probed(o) => {
                for (label, n) in [("A", &o.a), ("B", &o.b)] {
                    match n {
                        Ok(p) => println!(
                            "{label}: {} sudo={} tuning='{}' ifs={} tb_peers={}",
                            p.hostname,
                            p.sudo_passwordless,
                            p.tuning,
                            p.interfaces.len(),
                            p.thunderbolt.iter().filter(|t| t.is_host_peer).count()
                        ),
                        Err(e) => println!("{label}: ERROR {e}"),
                    }
                }
                for l in &o.links {
                    println!(
                        "LINK [{}] {} | {} | addr={:?} rtt={:?} modes={:?}",
                        l.kind,
                        l.label(),
                        l.detail,
                        l.b_addr,
                        l.rtt_ms,
                        l.modes()
                    );
                }
                return;
            }
            _ => {}
        }
    }
}

/// Headless batch test: probe, expand all usable link modes, benchmark
/// each (quick), print the comparison rows.
fn selftest_batch(a: String, b: String) {
    let cfg = Config {
        a_ssh: true,
        a_host: a,
        b_ssh: true,
        b_host: b,
        quick: true,
        ..Config::default()
    };
    let pa = probe_node(&cfg, cfg.a_ssh, &cfg.a_host).expect("probe A");
    let pb = probe_node(&cfg, cfg.b_ssh, &cfg.b_host).expect("probe B");
    let links = correlate(&pa, &pb);
    let rows: Vec<(LinkPair, String)> = links
        .iter()
        .flat_map(|l| l.modes().into_iter().map(move |m| (l.clone(), m.to_string())))
        .collect();
    eprintln!("testing {} link modes…", rows.len());
    let (tx, rx) = std::sync::mpsc::channel();
    let ctx = egui::Context::default();
    let procs = ProcHandles::default();
    std::thread::spawn(move || batch_worker(cfg, rows, tx, ctx, procs));
    let mut n = 0;
    for msg in rx {
        match msg {
            WorkerMsg::Log(l) => eprintln!("log: {l}"),
            WorkerMsg::Status(st) => eprintln!("status: {st}"),
            WorkerMsg::BatchItem { label, results } => {
                n += 1;
                println!(
                    "ROW {label}: score {:.1} · up {} · 16K-ar {:.0}us",
                    results.score.as_ref().map(|s| s.score).unwrap_or(0.0),
                    fmt_bits(results.uni_c2s_bps),
                    results.allreduce_16k_us
                );
            }
            WorkerMsg::BatchDone => {
                println!("BATCH OK: {n} link modes tested");
                return;
            }
            _ => {}
        }
    }
}

/// Visual demo: fake probe + batch built from last-run.json so the table
/// and comparison layouts can be screenshotted without hardware.
fn demo_state(app: &mut App) {
    let base: Option<Results> = dirs_data_dir()
        .map(|d| d.join("last-run.json"))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok());
    let Some(base) = base else { return };
    let mk = |kind: &str, kl: &str, a: &str, b: &str, addr: &str, rdma: bool| LinkPair {
        kind: kind.into(),
        kind_label: kl.into(),
        a_desc: a.into(),
        b_desc: b.into(),
        a_iface: String::new(),
        b_iface: String::new(),
        softroce_offerable: false,
        detail: if rdma { "RDMA available on both ends".into() } else { String::new() },
        b_addr: Some(addr.into()),
        rdma,
        b_mac: String::new(),
        rtt_ms: Some(0.11),
    };
    let links = vec![
        mk("ethernet", "ethernet 1G", "eno1 (192.168.155.112)", "eno1 (192.168.155.234)", "192.168.155.234", false),
        mk("connectx", "connectx 100G", "fastlink0 (10.10.10.1)", "fastlink0 (10.10.10.2)", "10.10.10.2", true),
        mk("thunderbolt", "thunderbolt 20G", "thunderbolt0 (10.111.11.1)", "thunderbolt0 (10.111.11.2)", "10.111.11.2", false),
    ];
    let mut probe_a = linkbench::probe::NodeProbe {
        hostname: "m5".into(),
        version: "demo".into(),
        sudo_passwordless: true,
        tuning: "performance/performance C3:off".into(),
        interfaces: vec![],
        rdma: vec![],
        thunderbolt: vec![],
        softroce_available: true,
    };
    let probe_b = linkbench::probe::NodeProbe { hostname: "m6".into(), ..probe_a.clone() };
    probe_a.hostname = "m5".into();
    app.probe = Some(ProbeOutcome { a: Ok(probe_a), b: Ok(probe_b), links });
    for (label, f) in [
        ("connectx 100G · tcp", 1.0),
        ("connectx 100G · rdma", 0.91),
        ("thunderbolt 20G · tcp", 0.17),
        ("ethernet 1G · tcp", 0.02),
    ] {
        let mut r = base.clone();
        r.uni_c2s_bps *= f;
        r.uni_s2c_bps *= f;
        r.allreduce_16k_us /= f.max(0.05);
        r.score = Some(linkbench::score::compute(&r));
        app.batch.push(BatchEntry { label: label.into(), results: r });
    }
}

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--selftest-batch") {
        if let (Some(a), Some(b)) = (args.get(i + 1), args.get(i + 2)) {
            selftest_batch(a.clone(), b.clone());
        } else {
            eprintln!("usage: --selftest-batch <sshA> <sshB>");
        }
        return Ok(());
    }
    if let Some(i) = args.iter().position(|a| a == "--selftest-probe") {
        if let (Some(a), Some(b)) = (args.get(i + 1), args.get(i + 2)) {
            selftest_probe(a.clone(), b.clone());
        } else {
            eprintln!("usage: --selftest-probe <sshA> <sshB>");
        }
        return Ok(());
    }
    if std::env::args().any(|a| a == "--selftest") {
        selftest();
        return Ok(());
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1000.0, 780.0])
            .with_min_inner_size([760.0, 560.0]),
        ..Default::default()
    };
    let demo = std::env::args().any(|a| a == "--demo");
    eframe::run_native(
        "linkbench",
        options,
        Box::new(move |cc| {
            let mut app = App::new(cc);
            if demo {
                demo_state(&mut app);
            }
            Ok(Box::new(app))
        }),
    )
}
