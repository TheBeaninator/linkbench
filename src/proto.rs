//! Control-channel protocol: newline-delimited JSON over TCP.
//! The control channel coordinates tests; bulk data flows over the
//! selected data plane (TCP streams, RDMA QP, ...).

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const DEFAULT_PORT: u16 = 7842;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RdmaDeviceInfo {
    pub name: String,
    pub port_state: String,
    pub link_layer: String,
    pub active_mtu: u32,
    pub rate_note: String,
    /// Network interface this RDMA device rides on (route safety checks).
    #[serde(default)]
    pub netdev: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RdmaEndpoint {
    pub qpns: Vec<u32>,
    pub psn: u32,
    pub gid: [u8; 16],
    pub lid: u16,
    pub mtu: u32,
    pub device: String,
    pub gid_index: i32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum TestSpec {
    /// Client sends pings of `size`, server echoes, `iters` times.
    PingEcho { size: u32, iters: u32 },
    /// Client sends `count` messages of `size`; server receives.
    C2S { size: u32, count: u64 },
    /// Server sends, client receives.
    S2C { size: u32, count: u64 },
    /// Both directions simultaneously, `count` messages each way.
    Bidir { size: u32, count: u64 },
    /// All-reduce simulation: `rounds` iterations of (bidirectional
    /// exchange of `chunks` x `chunk` bytes + local f32 reduction pass).
    Reduce { chunk: u32, chunks: u32, rounds: u32 },
    /// Like C2S, but the server buckets received bytes into `bucket_ms`
    /// windows and reports the timeline (sustained-throughput graph).
    C2STimeline { size: u32, count: u64, bucket_ms: u64 },
    /// Pings on stream/QP 0 while all other streams/QPs carry bulk:
    /// latency under load. Ping count is open-ended; the last ping is
    /// flagged in its first payload byte, followed by one unechoed drain
    /// message to keep posted-receive accounting balanced.
    LoadedPing { ping_size: u32, bulk_size: u32, bulk_count: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Msg {
    Hello {
        version: String,
        host: String,
        rdma_devices: Vec<RdmaDeviceInfo>,
        /// Compact CPU-tuning summary (governor/EPP + disabled idle states),
        /// recorded into results so runs are comparable.
        #[serde(default)]
        tuning: String,
    },
    /// Client asks server to open a TCP data plane with N parallel streams.
    SetupTcp { streams: u16 },
    TcpListening { port: u16 },
    /// Client asks server to bring up RDMA; carries the client's endpoint.
    SetupRdma {
        device: Option<String>,
        gid_index: Option<i32>,
        ep: RdmaEndpoint,
    },
    RdmaAccept { ep: RdmaEndpoint },
    SetupErr { error: String },
    Run { spec: TestSpec },
    Ready,
    /// Passive-side measurement for the test that just ran. For receive
    /// roles, elapsed_ns spans first..last message completion and bytes
    /// counts messages 2..=N (clock-skew-free rate measurement).
    Done { elapsed_ns: u64, bytes: u64 },
    /// Done plus a receive-side byte-count timeline (one entry per bucket)
    /// and this node's temperature series sampled at the same cadence
    /// (hottest non-NIC sensor, and NIC sensor if present; NaN-free, may
    /// be empty if no hwmon).
    DoneTimeline {
        elapsed_ns: u64,
        bytes: u64,
        buckets: Vec<u64>,
        /// Leading buckets during which every lane was still active.
        #[serde(default)]
        steady: u64,
        #[serde(default)]
        temps_sys: Vec<f32>,
        #[serde(default)]
        temps_nic: Vec<f32>,
    },
    Bye,
}

pub struct Ctl {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    line: String,
}

impl Ctl {
    pub fn new(stream: TcpStream) -> Result<Self> {
        stream.set_nodelay(true).ok();
        let reader = BufReader::new(stream.try_clone().context("clone control stream")?);
        Ok(Self { reader, writer: stream, line: String::new() })
    }

    pub fn send(&mut self, msg: &Msg) -> Result<()> {
        let mut s = serde_json::to_string(msg)?;
        s.push('\n');
        self.writer.write_all(s.as_bytes()).context("control channel write")?;
        Ok(())
    }

    pub fn recv(&mut self) -> Result<Msg> {
        self.line.clear();
        let n = self.reader.read_line(&mut self.line).context("control channel read")?;
        if n == 0 {
            bail!("control channel closed by peer");
        }
        serde_json::from_str(self.line.trim())
            .with_context(|| format!("bad control message: {}", self.line.trim()))
    }

    pub fn expect_ready(&mut self) -> Result<()> {
        match self.recv()? {
            Msg::Ready => Ok(()),
            other => bail!("expected Ready, got {other:?}"),
        }
    }

    pub fn expect_done(&mut self) -> Result<(u64, u64)> {
        match self.recv()? {
            Msg::Done { elapsed_ns, bytes } => Ok((elapsed_ns, bytes)),
            other => bail!("expected Done, got {other:?}"),
        }
    }

    #[allow(clippy::type_complexity)]
    pub fn expect_done_timeline(
        &mut self,
    ) -> Result<(u64, u64, Vec<u64>, u64, Vec<f32>, Vec<f32>)> {
        match self.recv()? {
            Msg::DoneTimeline { elapsed_ns, bytes, buckets, steady, temps_sys, temps_nic } => {
                Ok((elapsed_ns, bytes, buckets, steady, temps_sys, temps_nic))
            }
            other => bail!("expected DoneTimeline, got {other:?}"),
        }
    }
}
