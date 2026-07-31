use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use linkbench::bench::{self, BenchOpts};
use linkbench::proto::{Ctl, Msg, DEFAULT_PORT, VERSION};
use linkbench::{rdma, report};
use linkbench::transport::{DataPlane, TcpPlane};
use std::net::{TcpListener, TcpStream};

#[derive(Parser)]
#[command(
    name = "linkbench",
    version,
    about = "How fast and useful is the link between these two computers, for ML work?"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Wait for a client and serve benchmark traffic (run this on node B).
    Serve {
        #[arg(long, default_value_t = DEFAULT_PORT)]
        port: u16,
        /// RDMA device to use if the client requests RDMA (default: first).
        #[arg(long)]
        device: Option<String>,
        /// RoCE GID index override (default: auto-pick RoCE v2 / IPv4).
        #[arg(long)]
        gid_index: Option<i32>,
        /// Buffer region size in MiB (also the max message size).
        #[arg(long, default_value_t = 128)]
        region_mb: usize,
    },
    /// Connect to a serving node and run the benchmark suite (node A).
    Run {
        /// Server address: HOST or HOST:PORT.
        to: String,
        /// Data plane: auto, tcp, or rdma.
        #[arg(long, default_value = "auto")]
        transport: String,
        /// Parallel TCP data streams (tcp transport).
        #[arg(long, default_value_t = 4)]
        streams: u16,
        /// Parallel RDMA queue pairs (rdma transport). More than 1 mainly
        /// helps CPU-bound paths like Soft-RoCE; the server follows.
        #[arg(long, default_value_t = 4)]
        qps: u16,
        /// RDMA device (default: first).
        #[arg(long)]
        device: Option<String>,
        /// RoCE GID index override.
        #[arg(long)]
        gid_index: Option<i32>,
        /// Target seconds per bandwidth test.
        #[arg(long, default_value_t = 1.0)]
        duration: f64,
        /// Abbreviated run (~3x faster).
        #[arg(long)]
        quick: bool,
        /// Emit results as JSON on stdout instead of the report pane.
        #[arg(long)]
        json: bool,
        #[arg(long, default_value_t = 128)]
        region_mb: usize,
        /// Free-form experiment notes stored with the run in the history
        /// archive ("riser B", "new DAC cable", "C2 disabled", …).
        #[arg(long, default_value = "")]
        notes: String,
    },
    /// Show what this node offers: sudo state, tuning, every network
    /// interface classified by kind, RDMA devices, thunderbolt peers.
    Probe {
        /// Full machine-readable node report (what the GUI consumes).
        #[arg(long)]
        json: bool,
    },
    /// Bring the thunderbolt network link up on this node (module, address,
    /// max MTU). Needs root. Run on both ends with different addresses.
    TbUp {
        /// CIDR address for this end, e.g. 10.111.11.1/24.
        #[arg(long)]
        ip: String,
        /// MTU override (default: largest the driver accepts).
        #[arg(long)]
        mtu: Option<u32>,
        /// Also persist via /etc/modules-load.d + a netplan file.
        #[arg(long)]
        persist: bool,
    },
    /// Bring up software RoCE (rdma_rxe) on an interface, so the RDMA
    /// transport works on nodes with no hardware RDMA NIC. Needs root.
    RoceUp {
        /// Interface to carry RoCE, e.g. eth0 or enp2s0.
        #[arg(long)]
        dev: String,
        /// Name for the rxe link (default rxe0).
        #[arg(long, default_value = "rxe0")]
        name: String,
        /// Persist across reboots (modules-load.d + a oneshot unit).
        #[arg(long)]
        persist: bool,
    },
    /// List past benchmark runs from this machine's history archive
    /// (~/.local/share/linkbench/history/, one JSON per run).
    History {
        /// Only the most recent N entries.
        #[arg(long, default_value_t = 30)]
        limit: usize,
        /// Emit the full records as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Read or apply CPU tuning that affects link behaviour (governor,
    /// EPP, idle-state gating). Applying needs root; state is runtime-only.
    Tune {
        /// default | balanced | latency. Omit to just show current state.
        #[arg(long)]
        profile: Option<String>,
        /// Also apply the profile's NIC interrupt-coalescing to this
        /// interface (e.g. fastlink0).
        #[arg(long)]
        nic: Option<String>,
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
    },
}

fn hostname() -> String {
    let mut buf = [0u8; 256];
    unsafe {
        if libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) == 0 {
            let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
            return String::from_utf8_lossy(&buf[..end]).into_owned();
        }
    }
    "unknown".into()
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Probe { json } => probe(json),
        Cmd::TbUp { ip, mtu, persist } => {
            println!("{}", linkbench::probe::tb_up(&ip, mtu, persist)?);
            Ok(())
        }
        Cmd::RoceUp { dev, name, persist } => {
            println!("{}", linkbench::probe::roce_up(&dev, &name, persist)?);
            Ok(())
        }
        Cmd::Tune { profile, nic, json } => tune_cmd(profile.as_deref(), nic.as_deref(), json),
        Cmd::History { limit, json } => history_cmd(limit, json),
        Cmd::Serve { port, device, gid_index, region_mb } => {
            serve(port, device, gid_index, region_mb << 20)
        }
        Cmd::Run {
            to,
            transport,
            streams,
            qps,
            device,
            gid_index,
            duration,
            quick,
            json,
            region_mb,
            notes,
        } => run(
            &to,
            &transport,
            streams,
            qps,
            device,
            gid_index,
            BenchOpts { duration_s: duration, quick },
            json,
            region_mb << 20,
            &notes,
        ),
    }
}

fn history_cmd(limit: usize, json: bool) -> Result<()> {
    let all = linkbench::history::list();
    let entries = &all[all.len().saturating_sub(limit)..];
    if json {
        println!("{}", serde_json::to_string_pretty(entries)?);
        return Ok(());
    }
    if entries.is_empty() {
        println!("no history yet — runs append to ~/.local/share/linkbench/history/");
        return Ok(());
    }
    println!(
        "{:<16} {:<26} {:<13} {:<9} {:>5} {:>10} {:>9}  tuning (A)",
        "when (UTC)", "label / peer", "path", "transport", "score", "up", "16K ar"
    );
    for e in entries {
        let r = &e.results;
        let label = if e.label.is_empty() {
            format!("{} - {}", r.client_host, r.server_host)
        } else {
            e.label.clone()
        };
        let path_short: String = r.path.chars().take(13).collect();
        println!(
            "{:<16} {:<26} {:<13} {:<9} {:>5.1} {:>10} {:>7.0}µs  {}{}",
            linkbench::history::fmt_ts(e.ts),
            label,
            path_short,
            r.transport,
            r.score.as_ref().map(|s| s.score).unwrap_or(0.0),
            linkbench::report::fmt_bits(r.uni_c2s_bps),
            r.allreduce_16k_us,
            r.tuning_a,
            if e.notes.is_empty() {
                String::new()
            } else {
                format!("  — {}", e.notes)
            },
        );
    }
    println!("({} shown of {} total)", entries.len(), all.len());
    Ok(())
}

fn tune_cmd(profile: Option<&str>, nic: Option<&str>, json: bool) -> Result<()> {
    let state = match profile {
        Some(p) => {
            let profile = linkbench::tune::Profile::parse(p)
                .ok_or_else(|| anyhow::anyhow!("unknown profile {p:?} (default|balanced|latency)"))?;
            let state = linkbench::tune::apply(profile)?;
            if let Some(iface) = nic {
                let msg = linkbench::tune::apply_nic(profile, iface)?;
                if !json {
                    eprintln!("{msg}");
                }
            }
            if !json {
                eprintln!("applied profile {profile} (runtime-only; resets on reboot)");
            }
            state
        }
        None => linkbench::tune::read_state()?,
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&state)?);
    } else {
        println!("cpus: {}", state.cpus);
        println!("governor: {}   epp: {}", state.governor, state.epp);
        for (i, st) in state.idle_states.iter().enumerate() {
            println!(
                "  idle state{i} {:<10} exit {:>4} µs   {}",
                st.name,
                st.latency_us,
                if st.disabled { "DISABLED" } else { "enabled" }
            );
        }
        println!("summary: {}", state.summary());
    }
    Ok(())
}

fn probe(json: bool) -> Result<()> {
    let report = linkbench::probe::gather(hostname())?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    println!("linkbench {VERSION} on {}", report.hostname);
    println!(
        "  passwordless sudo: {}",
        if report.sudo_passwordless { "yes" } else { "no" }
    );
    if !report.tuning.is_empty() {
        println!("  cpu tuning: {}", report.tuning);
    }
    for i in &report.interfaces {
        if i.kind == linkbench::probe::IfKind::Loopback {
            continue;
        }
        println!(
            "  {:<12} {:<12} {:<5} {}{}  {}",
            i.name,
            format!("{}", i.kind),
            if i.oper_up { "UP" } else { "down" },
            i.speed_mbps.map(|s| format!("{s} Mb/s ")).unwrap_or_default(),
            if i.ipv4.is_empty() { "no-ip".into() } else { i.ipv4.join(",") },
            i.driver,
        );
    }
    for d in &report.rdma {
        println!(
            "  rdma: {} on {} [{}] {} mtu {} ({})",
            d.name, d.netdev, d.port_state, d.link_layer, d.active_mtu, d.rate_note
        );
    }
    for t in &report.thunderbolt {
        println!(
            "  thunderbolt: {} {} {}{}",
            t.id,
            t.vendor,
            t.device,
            if t.is_host_peer { "  [HOST PEER]" } else { "" }
        );
    }
    if report.rdma.is_empty() {
        println!(
            "  rdma: none — {}",
            if report.softroce_available {
                "software RoCE available (`sudo linkbench roce-up --dev <iface>`)"
            } else {
                "no hardware RDMA and rdma_rxe unavailable; use tcp"
            }
        );
    }
    Ok(())
}

// -------------------------------------------------------------------- serve

fn serve(port: u16, device: Option<String>, gid_index: Option<i32>, region: usize) -> Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", port))
        .with_context(|| format!("bind control port {port}"))?;
    eprintln!("linkbench {VERSION} serving on port {port} ({})", hostname());
    loop {
        let (stream, peer) = listener.accept()?;
        eprintln!("client connected: {peer}");
        match serve_client(stream, device.as_deref(), gid_index, region) {
            Ok(()) => eprintln!("session complete"),
            Err(e) => eprintln!("session error: {e:#}"),
        }
    }
}

fn serve_client(
    stream: TcpStream,
    device: Option<&str>,
    gid_index: Option<i32>,
    region: usize,
) -> Result<()> {
    let peer_ip = stream.peer_addr().ok().map(|a| a.ip().to_string());
    let mut ctl = Ctl::new(stream)?;
    ctl.send(&Msg::Hello {
        version: VERSION.into(),
        host: hostname(),
        rdma_devices: rdma::list_devices().unwrap_or_default(),
        tuning: linkbench::tune::read_state().map(|s| s.summary()).unwrap_or_default(),
    })?;
    let Msg::Hello { version, .. } = ctl.recv()? else {
        bail!("expected client Hello");
    };
    if version != VERSION {
        eprintln!("warning: client is linkbench {version}, we are {VERSION}");
    }

    let mut plane: Box<dyn DataPlane> = match ctl.recv()? {
        Msg::SetupTcp { streams } => {
            let data = TcpListener::bind("0.0.0.0:0")?;
            let port = data.local_addr()?.port();
            ctl.send(&Msg::TcpListening { port })?;
            Box::new(TcpPlane::accept(&data, streams, region)?)
        }
        Msg::SetupRdma { device: want, gid_index: gidx, ep } => {
            // Prefer the RDMA device that rides the interface the client
            // reached us on; an explicit --device on either side wins but
            // a mismatch is refused with an explanation rather than run.
            let route_if = peer_ip.as_deref().and_then(linkbench::probe::route_dev);
            let devs = rdma::list_devices().unwrap_or_default();
            let route_match = route_if.as_deref().and_then(|ri| {
                devs.iter()
                    .find(|d| d.port_state == "ACTIVE" && d.netdev == ri)
                    .map(|d| d.name.clone())
            });
            let explicit = want.as_deref().or(device);
            let dev = match (explicit, &route_match) {
                (Some(e), Some(m)) if e != m => {
                    let err = format!(
                        "server RDMA device {e} does not ride {} (the link the client used); \
                         {m} does — drop the explicit device or use it",
                        route_if.as_deref().unwrap_or("?")
                    );
                    ctl.send(&Msg::SetupErr { error: err.clone() })?;
                    bail!("{err}");
                }
                (Some(e), _) => Some(e.to_string()),
                (None, Some(m)) => Some(m.clone()),
                (None, None) => {
                    let err = format!(
                        "no ACTIVE server RDMA device rides {} (the link the client used); devices: {}",
                        route_if.as_deref().unwrap_or("unknown"),
                        devs.iter()
                            .map(|d| format!("{} (on {})", d.name, d.netdev))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    ctl.send(&Msg::SetupErr { error: err.clone() })?;
                    bail!("{err}");
                }
            };
            let nqps = ep.qpns.len().max(1);
            match rdma::RdmaPlane::new(dev.as_deref(), gidx.or(gid_index), region, nqps) {
                Ok(mut p) => match p.connect(ep) {
                    Ok(()) => {
                        ctl.send(&Msg::RdmaAccept { ep: p.local_endpoint() })?;
                        Box::new(p)
                    }
                    Err(e) => {
                        ctl.send(&Msg::SetupErr { error: format!("{e:#}") })?;
                        return Err(e);
                    }
                },
                Err(e) => {
                    ctl.send(&Msg::SetupErr { error: format!("{e:#}") })?;
                    return Err(e);
                }
            }
        }
        other => bail!("expected a Setup message, got {other:?}"),
    };
    eprintln!("data plane up: {} ({})", plane.kind(), plane.describe());
    bench::serve_specs(&mut ctl, plane.as_mut())
}

// ---------------------------------------------------------------------- run

#[allow(clippy::too_many_arguments)]
fn run(
    to: &str,
    transport: &str,
    streams: u16,
    qps: u16,
    device: Option<String>,
    gid_index: Option<i32>,
    opts: BenchOpts,
    json: bool,
    region: usize,
    notes: &str,
) -> Result<()> {
    let addr = if to.contains(':') {
        to.to_string()
    } else {
        format!("{to}:{DEFAULT_PORT}")
    };
    let host_part = addr.rsplit_once(':').map(|(h, _)| h.to_string()).unwrap();

    eprintln!("linkbench {VERSION} → {addr}");
    let stream = TcpStream::connect(&addr).with_context(|| format!("connect to {addr}"))?;
    let mut ctl = Ctl::new(stream)?;
    let Msg::Hello { version, host: server_host, rdma_devices, tuning: server_tuning } =
        ctl.recv()?
    else {
        bail!("expected server Hello — is that a linkbench server?");
    };
    if version != VERSION {
        eprintln!("warning: server is linkbench {version}, we are {VERSION}");
    }
    ctl.send(&Msg::Hello {
        version: VERSION.into(),
        host: hostname(),
        rdma_devices: vec![],
        tuning: String::new(),
    })?;

    // Route safety: RDMA data flows over the device's backing netdev, not
    // the target address — never let those silently diverge.
    let target_ip = std::net::ToSocketAddrs::to_socket_addrs(&addr)
        .ok()
        .and_then(|mut a| a.next())
        .map(|a| a.ip().to_string());
    let route_if = target_ip.as_deref().and_then(linkbench::probe::route_dev);
    let local_devs = rdma::list_devices().unwrap_or_default();
    let route_dev_for = |wanted: Option<&str>| -> Option<String> {
        let route_if = route_if.as_deref()?;
        local_devs
            .iter()
            .find(|d| {
                d.port_state == "ACTIVE"
                    && d.netdev == route_if
                    && wanted.map_or(true, |w| w == d.name)
            })
            .map(|d| d.name.clone())
    };
    let dev_map = || {
        local_devs
            .iter()
            .map(|d| format!("{} (on {})", d.name, d.netdev))
            .collect::<Vec<_>>()
            .join(", ")
    };

    let mut chosen_device = device.clone();
    let use_rdma = match transport {
        "tcp" => false,
        "rdma" => {
            match (&device, route_dev_for(device.as_deref())) {
                // Explicit device that matches the route, or auto-matched one.
                (_, Some(dev)) => {
                    chosen_device = Some(dev);
                    true
                }
                // Explicit device that did not match the route. Two very
                // different cases, and conflating them once told a tester
                // their perfectly good USB4 RDMA results were invalid:
                //   * we could not work out which interface backs the device
                //     — say so, and say nothing about where data will flow;
                //   * we know its interface and it differs from the route —
                //     that is the real warning.
                (Some(dev), None) => {
                    let backing = local_devs
                        .iter()
                        .find(|d| d.name == *dev)
                        .map(|d| d.netdev.as_str())
                        .filter(|n| !n.is_empty());
                    match backing {
                        Some(nd) => eprintln!(
                            "WARNING: RDMA device {dev} rides {nd}, but {} routes over {}; \
                             data will NOT flow over the link you addressed",
                            addr,
                            route_if.as_deref().unwrap_or("unknown")
                        ),
                        None => eprintln!(
                            "NOTE: could not determine which interface backs RDMA device {dev} \
                             (control connection to {} routes over {}). RDMA data egresses via \
                             the device itself, so this is usually fine — but the path is \
                             unverified; confirm with the counters on the interface you expect.",
                            addr,
                            route_if.as_deref().unwrap_or("unknown")
                        ),
                    }
                    true
                }
                (None, None) => bail!(
                    "no ACTIVE RDMA device rides {} (the interface that routes to {}). \
                     Local devices: {}. Use --transport tcp, or --device to override explicitly.",
                    route_if.as_deref().unwrap_or("unknown"),
                    addr,
                    dev_map()
                ),
            }
        }
        "auto" => {
            if rdma_devices.is_empty() {
                eprintln!("auto: using tcp (server reports no RDMA)");
                false
            } else if let Some(dev) = route_dev_for(device.as_deref()) {
                eprintln!(
                    "auto: RDMA on both ends and {dev} rides the route to {} — using rdma (force with --transport tcp)",
                    route_if.as_deref().unwrap_or("?")
                );
                chosen_device = Some(dev);
                true
            } else {
                eprintln!(
                    "auto: using tcp — no RDMA device on {} (the route to the target); local devices: {}",
                    route_if.as_deref().unwrap_or("unknown"),
                    if local_devs.is_empty() { "none".into() } else { dev_map() }
                );
                false
            }
        }
        other => bail!("unknown transport {other:?} (auto|tcp|rdma)"),
    };

    let plane: Box<dyn DataPlane> = if use_rdma {
        let mut p =
            rdma::RdmaPlane::new(chosen_device.as_deref(), gid_index, region, qps as usize)?;
        ctl.send(&Msg::SetupRdma {
            device: None,
            gid_index: None,
            ep: p.local_endpoint(),
        })?;
        match ctl.recv()? {
            Msg::RdmaAccept { ep } => p.connect(ep)?,
            Msg::SetupErr { error } => bail!("server failed to set up RDMA: {error}"),
            other => bail!("unexpected reply to SetupRdma: {other:?}"),
        }
        Box::new(p)
    } else {
        ctl.send(&Msg::SetupTcp { streams })?;
        let Msg::TcpListening { port } = ctl.recv()? else {
            bail!("expected TcpListening");
        };
        Box::new(TcpPlane::connect(&format!("{host_part}:{port}"), streams, region)?)
    };
    eprintln!("data plane up: {} ({})", plane.kind(), plane.describe());

    let results = bench::run_client(&mut ctl, plane, hostname(), server_host, server_tuning, &opts)?;

    // The GUI orchestrator archives runs itself (with link labels and
    // notes); it sets this to avoid double-logging.
    if std::env::var_os("LINKBENCH_NO_HISTORY").is_none() {
        match linkbench::history::append(&results, "", notes) {
            Ok(p) => eprintln!("history: {}", p.display()),
            Err(e) => eprintln!("history append failed: {e:#}"),
        }
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&results)?);
    } else {
        report::render(&results);
    }
    Ok(())
}
