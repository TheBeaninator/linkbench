//! Test choreography. The client drives; the server reacts to `Run` specs.
//! All wire-rate numbers come from receive-side first..last completion
//! timing, so nothing depends on clock sync between the nodes.

use crate::proto::{Ctl, Msg, TestSpec};
use crate::sensors::TempSampler;
use crate::transport::{DataPlane, RecvTiming};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

pub const KIB: u64 = 1024;
pub const MIB: u64 = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatencyPoint {
    pub size: u64,
    pub oneway_p50_us: f64,
    pub oneway_p99_us: f64,
    pub rtt_mean_us: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BwPoint {
    pub size: u64,
    pub bps: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Results {
    pub transport: String,
    pub path: String,
    pub client_host: String,
    pub server_host: String,
    pub duration_target_s: f64,
    pub latency: Vec<LatencyPoint>,
    pub sweep: Vec<BwPoint>,
    pub uni_c2s_bps: f64,
    pub uni_s2c_bps: f64,
    pub bidir_agg_bps: f64,
    pub msg_rate_per_s: f64,
    pub allreduce_16k_us: f64,
    pub allreduce_1g_bps: f64,
    pub kv32m_ms: f64,
    #[serde(default)]
    pub timeline_bucket_ms: u64,
    /// Receive-side throughput per bucket during the sustained runs.
    #[serde(default)]
    pub timeline_up_bps: Vec<f64>,
    #[serde(default)]
    pub timeline_down_bps: Vec<f64>,
    /// Steady-window length (buckets with every lane active); the rest of
    /// each timeline is lane wind-down, excluded from stability stats.
    #[serde(default)]
    pub timeline_up_steady: usize,
    #[serde(default)]
    pub timeline_down_steady: usize,
    /// Temperatures (°C) sampled during the sustained runs: node B during
    /// the up run, node A during the down run. "sys" = hottest non-NIC
    /// hwmon sensor, "nic" = mlx-style adapter sensor (may be empty).
    #[serde(default)]
    pub temps_b_sys: Vec<f32>,
    #[serde(default)]
    pub temps_b_nic: Vec<f32>,
    #[serde(default)]
    pub temps_a_sys: Vec<f32>,
    #[serde(default)]
    pub temps_a_nic: Vec<f32>,
    #[serde(default)]
    pub loaded_rtt: Option<LoadedRtt>,
    #[serde(default)]
    pub idle_gaps: Vec<IdleGap>,
    /// LinkBench2026 score, computed from the fields above once the run
    /// completes (recomputed at render time for older result files).
    #[serde(default)]
    pub score: Option<crate::score::ScoreCard>,
    /// CPU-tuning summaries the run executed under (A = client, B = server).
    #[serde(default)]
    pub tuning_a: String,
    #[serde(default)]
    pub tuning_b: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadedRtt {
    pub ping_size: u64,
    pub p50_us: f64,
    pub p99_us: f64,
    pub samples: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdleGap {
    pub gap_ms: u64,
    pub rtt_p50_us: f64,
    pub rtt_p99_us: f64,
}

pub struct BenchOpts {
    pub duration_s: f64,
    pub quick: bool,
}

fn timing_bps(t: &RecvTiming, size: u64) -> f64 {
    let secs = t.last.duration_since(t.first).as_secs_f64();
    if t.count < 2 || secs <= 0.0 {
        return 0.0;
    }
    ((t.count - 1) * size) as f64 / secs
}

fn done_bps(elapsed_ns: u64, bytes: u64) -> f64 {
    if elapsed_ns == 0 {
        return 0.0;
    }
    bytes as f64 / (elapsed_ns as f64 / 1e9)
}

/// One local reduction pass, modelling the CPU cost of summing a received
/// gradient chunk into an accumulator.
fn reduce_pass(acc: &mut [f32], grad: &[f32]) {
    for (a, g) in acc.iter_mut().zip(grad) {
        *a += *g;
    }
    std::hint::black_box(&acc[0]);
}

// ------------------------------------------------------------------ client

pub struct Client<'a> {
    pub ctl: &'a mut Ctl,
    pub plane: Box<dyn DataPlane>,
}

impl<'a> Client<'a> {
    fn run(&mut self, spec: TestSpec) -> Result<()> {
        self.ctl.send(&Msg::Run { spec })?;
        self.ctl.expect_ready()
    }

    fn c2s(&mut self, size: u64, count: u64) -> Result<f64> {
        self.run(TestSpec::C2S { size: size as u32, count })?;
        self.plane.send_burst(size as usize, count)?;
        let (elapsed, bytes) = self.ctl.expect_done()?;
        Ok(done_bps(elapsed, bytes))
    }

    fn bidir(&mut self, size: u64, count: u64) -> Result<f64> {
        self.run(TestSpec::Bidir { size: size as u32, count })?;
        let t = self.plane.bidir_burst(size as usize, count)?;
        let (elapsed, bytes) = self.ctl.expect_done()?;
        Ok(timing_bps(&t, size) + done_bps(elapsed, bytes))
    }

    fn pingpong(&mut self, size: u64, warmup: u32, iters: u32) -> Result<Vec<u64>> {
        self.run(TestSpec::PingEcho { size: size as u32, iters: warmup + iters })?;
        let mut rtts_ns = Vec::with_capacity(iters as usize);
        for i in 0..warmup + iters {
            let t0 = Instant::now();
            self.plane.ping(size as usize)?;
            if i >= warmup {
                rtts_ns.push(t0.elapsed().as_nanos() as u64);
            }
        }
        self.ctl.expect_done()?;
        Ok(rtts_ns)
    }

    /// All-reduce simulation; returns elapsed seconds for all rounds.
    fn reduce(&mut self, chunk: u64, chunks: u32, rounds: u32) -> Result<f64> {
        self.run(TestSpec::Reduce { chunk: chunk as u32, chunks, rounds })?;
        let secs = reduce_loop(self.plane.as_mut(), chunk, chunks, rounds)?;
        self.ctl.expect_done()?;
        Ok(secs)
    }
}

/// Pipelined small-all-reduce rounds (chunks == 1 path), run identically on
/// both sides: receives are pre-posted across rounds via `reduce_begin`, so
/// no round can hit receiver-not-ready backoff — matching how real
/// collectives behave. Returns per-round durations (ns).
fn allreduce_rounds(plane: &mut dyn DataPlane, chunk: u64, rounds: u32) -> Result<Vec<u64>> {
    let floats = (chunk / 4) as usize;
    let mut acc = vec![0.0f32; floats];
    let grad = vec![1.0f32; floats];
    plane.reduce_begin(chunk as usize, rounds)?;
    let mut times = Vec::with_capacity(rounds as usize);
    for _ in 0..rounds {
        let t0 = Instant::now();
        plane.reduce_round(chunk as usize)?;
        reduce_pass(&mut acc, &grad);
        times.push(t0.elapsed().as_nanos() as u64);
    }
    Ok(times)
}

/// Shared by both sides: `rounds` x (exchange `chunks * chunk` bytes each
/// way + local f32 add). chunks == 1 takes the pipelined round path;
/// larger exchanges use the true bidirectional burst path.
fn reduce_loop(plane: &mut dyn DataPlane, chunk: u64, chunks: u32, rounds: u32) -> Result<f64> {
    let t0 = Instant::now();
    if chunks == 1 {
        allreduce_rounds(plane, chunk, rounds)?;
        return Ok(t0.elapsed().as_secs_f64());
    }
    let floats = (chunk / 4) as usize;
    let mut acc = vec![0.0f32; floats];
    let grad = vec![1.0f32; floats];
    for _ in 0..rounds {
        plane.bidir_burst(chunk as usize, chunks as u64)?;
        for _ in 0..chunks {
            reduce_pass(&mut acc, &grad);
        }
    }
    Ok(t0.elapsed().as_secs_f64())
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx]
}

pub fn run_client(
    ctl: &mut Ctl,
    plane: Box<dyn DataPlane>,
    client_host: String,
    server_host: String,
    server_tuning: String,
    opts: &BenchOpts,
) -> Result<Results> {
    let transport = plane.kind().to_string();
    let path = plane.describe();
    let max_msg = plane.max_msg() as u64;
    let mut c = Client { ctl, plane };
    let d = opts.duration_s;
    let scale = if opts.quick { 0.35 } else { 1.0 };

    let count_for = |size: u64, bps: f64, cap: u64| -> u64 {
        (((bps * d * scale) / size as f64) as u64).clamp(16, cap)
    };

    eprint!("  calibrating … ");
    c.c2s(MIB, 64)?; // warm the path (page faults, cwnd, caches)
    let est = c.c2s(MIB, 256)?.max(1e6);
    eprintln!("~{}", crate::report::fmt_rate(est));

    // --- latency: control-plane + hidden-state sized messages
    let lat_sizes = [64, 8 * KIB, 16 * KIB, 128 * KIB];
    let iters = if opts.quick { 300 } else { 1000 };
    let mut latency = Vec::new();
    for size in lat_sizes {
        eprint!("  latency {:>7} … ", crate::report::fmt_size(size));
        let mut rtts = c.pingpong(size, iters / 10, iters)?;
        rtts.sort_unstable();
        let p50 = percentile(&rtts, 0.5) as f64 / 1000.0;
        let p99 = percentile(&rtts, 0.99) as f64 / 1000.0;
        let mean = rtts.iter().sum::<u64>() as f64 / rtts.len() as f64 / 1000.0;
        eprintln!("one-way p50 {:.1} µs", p50 / 2.0);
        latency.push(LatencyPoint {
            size,
            oneway_p50_us: p50 / 2.0,
            oneway_p99_us: p99 / 2.0,
            rtt_mean_us: mean,
        });
    }

    // --- bandwidth sweep (client -> server)
    let sweep_sizes: Vec<u64> = [4 * KIB, 64 * KIB, MIB, 8 * MIB, 32 * MIB, 128 * MIB]
        .into_iter()
        .filter(|s| *s <= max_msg)
        .collect();
    let mut sweep = Vec::new();
    for size in sweep_sizes {
        eprint!("  bandwidth {:>7} … ", crate::report::fmt_size(size));
        let bps = c.c2s(size, count_for(size, est, 250_000))?;
        eprintln!("{}", crate::report::fmt_rate(bps));
        sweep.push(BwPoint { size, bps });
    }

    // --- sustained runs: longer, with a throughput timeline (receive-side
    // buckets) and temperature sampling on the busy node
    let big = (8 * MIB).min(max_msg);
    let sustain_s = if opts.quick { 3.0 } else { 8.0 };
    let bucket_ms: u64 = 200;
    let bucket = Duration::from_millis(bucket_ms);
    let sus_count = ((est * sustain_s / big as f64) as u64).max(32);

    eprint!("  sustained up (c→s, {sustain_s:.0} s) … ");
    c.run(TestSpec::C2STimeline { size: big as u32, count: sus_count, bucket_ms })?;
    // Sustained rates come from the steady window (all lanes active), not
    // the full first..last span — the wind-down tail is our static-share
    // skew, not the link's behaviour.
    let bucket_s = bucket_ms as f64 / 1e3;
    let steady_rate = |buckets: &[u64], steady: usize, fallback: f64| -> f64 {
        if steady >= 3 {
            let b = &buckets[1..steady];
            b.iter().sum::<u64>() as f64 / (b.len() as f64 * bucket_s)
        } else {
            fallback
        }
    };

    c.plane.send_burst(big as usize, sus_count)?;
    let (el, by, up_buckets, up_steady, temps_b_sys, temps_b_nic) =
        c.ctl.expect_done_timeline()?;
    let uni_c2s_bps = steady_rate(&up_buckets, up_steady as usize, done_bps(el, by));
    eprintln!("{}", crate::report::fmt_rate(uni_c2s_bps));

    eprint!("  sustained down (s→c, {sustain_s:.0} s) … ");
    c.run(TestSpec::S2C { size: big as u32, count: sus_count })?;
    let sampler = TempSampler::start(bucket);
    let (t, down_buckets, down_steady) =
        c.plane.recv_timeline(big as usize, sus_count, bucket)?;
    let (temps_a_sys, temps_a_nic) = sampler.finish();
    c.ctl.expect_done()?;
    let uni_s2c_bps = steady_rate(&down_buckets, down_steady, timing_bps(&t, big));
    eprintln!("{}", crate::report::fmt_rate(uni_s2c_bps));

    eprint!("  bidirectional … ");
    let big_count = count_for(big, est * 1.2, 100_000).max(32);
    let bidir_agg_bps = c.bidir(big, big_count)?;
    eprintln!("{} aggregate", crate::report::fmt_rate(bidir_agg_bps));

    let to_bps = |b: Vec<u64>| -> Vec<f64> {
        b.into_iter()
            .map(|bytes| bytes as f64 / (bucket_ms as f64 / 1e3))
            .collect()
    };
    let timeline_up_bps = to_bps(up_buckets);
    let timeline_down_bps = to_bps(down_buckets);

    // --- message rate (token/RPC-sized)
    eprint!("  message rate … ");
    let probe_n = 20_000u64;
    c.run(TestSpec::C2S { size: 256, count: probe_n })?;
    let t0 = Instant::now();
    c.plane.send_burst(256, probe_n)?;
    let (e, b) = c.ctl.expect_done()?;
    let probe_rate = if e > 0 { (b / 256) as f64 / (e as f64 / 1e9) } else { probe_n as f64 / t0.elapsed().as_secs_f64() };
    let n = ((probe_rate * d * scale) as u64).clamp(probe_n, 3_000_000);
    let bps = c.c2s(256, n)?;
    let msg_rate_per_s = bps / 256.0;
    eprintln!("{:.2} M msg/s", msg_rate_per_s / 1e6);

    // --- small all-reduce (per-token tensor-parallel sync, 16 KiB).
    // Median round, not mean: the mean is contaminated by rare
    // scheduler/backoff stragglers that real collectives don't pay.
    eprint!("  all-reduce 16 KiB … ");
    let rounds = if opts.quick { 300 } else { 1000 };
    c.run(TestSpec::Reduce { chunk: (8 * KIB) as u32, chunks: 1, rounds })?;
    let mut times = allreduce_rounds(c.plane.as_mut(), 8 * KIB, rounds)?;
    c.ctl.expect_done()?;
    times.sort_unstable();
    // one 16 KiB all-reduce = reduce-scatter half + all-gather half = 2 rounds
    let allreduce_16k_us = 2.0 * percentile(&times, 0.5) as f64 / 1000.0;
    let ar_p99 = 2.0 * percentile(&times, 0.99) as f64 / 1000.0;
    eprintln!("{allreduce_16k_us:.1} µs p50 ({ar_p99:.0} µs p99)");

    // --- large all-reduce (gradient sync, 1 GiB)
    eprint!("  all-reduce 1 GiB … ");
    let g = 1024 * MIB;
    let chunk = (16 * MIB).min(max_msg);
    let chunks = (g / 2 / chunk) as u32;
    let secs = c.reduce(chunk, chunks, 2)?;
    let allreduce_1g_bps = g as f64 / secs;
    eprintln!("{} effective", crate::report::fmt_rate(allreduce_1g_bps));

    // --- KV-cache block shuttle (32 MiB blocks)
    eprint!("  KV block 32 MiB … ");
    let kv = (32 * MIB).min(max_msg);
    let kv_bps = c.c2s(kv, count_for(kv, est, 4096).max(16))?;
    let kv32m_ms = (32 * MIB) as f64 / kv_bps * 1e3;
    eprintln!("{kv32m_ms:.2} ms/block");

    // --- latency under load: pings on lane 0 while the rest carry bulk
    // (serving tokens while a transfer saturates the link)
    let loaded_rtt = if c.plane.lanes() >= 2 {
        eprint!("  latency under load … ");
        let bulk_secs = if opts.quick { 1.5 } else { 3.0 };
        let bulk_count = ((est * bulk_secs / big as f64) as u64).max(16);
        c.run(TestSpec::LoadedPing {
            ping_size: (16 * KIB) as u32,
            bulk_size: big as u32,
            bulk_count,
        })?;
        let mut rtts = c
            .plane
            .loaded_ping_initiator((16 * KIB) as usize, big as usize, bulk_count)?;
        c.ctl.expect_done()?;
        rtts.sort_unstable();
        let lr = LoadedRtt {
            ping_size: 16 * KIB,
            p50_us: percentile(&rtts, 0.5) as f64 / 1000.0,
            p99_us: percentile(&rtts, 0.99) as f64 / 1000.0,
            samples: rtts.len() as u64,
        };
        eprintln!("RTT p50 {:.0} µs under full load", lr.p50_us);
        Some(lr)
    } else {
        eprintln!("  latency under load … skipped (needs ≥2 streams/QPs)");
        None
    };

    // --- after-idle wakeup: ping after a compute-shaped pause (coalescing,
    // cwnd decay, power states)
    let mut idle_gaps = Vec::new();
    for gap_ms in [1u64, 10, 100] {
        eprint!("  after {gap_ms:>3} ms idle … ");
        // Full mode: >=30 samples even at the 100 ms gap, so p50 is solid.
        // (The stored p99 is only meaningful for the short gaps; treat it
        // as "max observed" at 100 ms.)
        let budget_ms = if opts.quick { 600 } else { 3000 };
        let iters = ((budget_ms / gap_ms) as u32).clamp(8, 120) + 2;
        c.run(TestSpec::PingEcho { size: (16 * KIB) as u32, iters })?;
        let mut rtts = Vec::new();
        for i in 0..iters {
            std::thread::sleep(Duration::from_millis(gap_ms));
            let t0 = Instant::now();
            c.plane.ping((16 * KIB) as usize)?;
            if i >= 2 {
                rtts.push(t0.elapsed().as_nanos() as u64);
            }
        }
        c.ctl.expect_done()?;
        rtts.sort_unstable();
        let p50 = percentile(&rtts, 0.5) as f64 / 1000.0;
        idle_gaps.push(IdleGap {
            gap_ms,
            rtt_p50_us: p50,
            rtt_p99_us: percentile(&rtts, 0.99) as f64 / 1000.0,
        });
        eprintln!("RTT p50 {p50:.0} µs");
    }

    c.ctl.send(&Msg::Bye)?;

    let mut results = Results {
        transport,
        path,
        client_host,
        server_host,
        duration_target_s: d * scale,
        latency,
        sweep,
        uni_c2s_bps,
        uni_s2c_bps,
        bidir_agg_bps,
        msg_rate_per_s,
        allreduce_16k_us,
        allreduce_1g_bps,
        kv32m_ms,
        timeline_bucket_ms: bucket_ms,
        timeline_up_bps,
        timeline_down_bps,
        timeline_up_steady: up_steady as usize,
        timeline_down_steady: down_steady,
        temps_b_sys,
        temps_b_nic,
        temps_a_sys,
        temps_a_nic,
        loaded_rtt,
        idle_gaps,
        score: None,
        tuning_a: crate::tune::read_state().map(|s| s.summary()).unwrap_or_default(),
        tuning_b: server_tuning,
    };
    results.score = Some(crate::score::compute(&results));
    let s = results.score.as_ref().unwrap();
    eprintln!(
        "  {} score: {:.1} / 10",
        crate::score::SCORE_NAME,
        s.score
    );
    Ok(results)
}

// ------------------------------------------------------------------ server

/// Handle `Run` specs until `Bye`. Returns when the client is done.
pub fn serve_specs(ctl: &mut Ctl, plane: &mut dyn DataPlane) -> Result<()> {
    loop {
        match ctl.recv()? {
            Msg::Run { spec } => {
                ctl.send(&Msg::Ready)?;
                match spec {
                    TestSpec::PingEcho { size, iters } => {
                        plane.echo(size as usize, iters)?;
                        ctl.send(&Msg::Done { elapsed_ns: 0, bytes: 0 })?;
                    }
                    TestSpec::C2S { size, count } => {
                        let t = plane.recv_burst(size as usize, count)?;
                        ctl.send(&Msg::Done {
                            elapsed_ns: t.last.duration_since(t.first).as_nanos() as u64,
                            bytes: count.saturating_sub(1) * size as u64,
                        })?;
                    }
                    TestSpec::S2C { size, count } => {
                        plane.send_burst(size as usize, count)?;
                        ctl.send(&Msg::Done { elapsed_ns: 0, bytes: 0 })?;
                    }
                    TestSpec::Bidir { size, count } => {
                        let t = plane.bidir_burst(size as usize, count)?;
                        ctl.send(&Msg::Done {
                            elapsed_ns: t.last.duration_since(t.first).as_nanos() as u64,
                            bytes: count.saturating_sub(1) * size as u64,
                        })?;
                    }
                    TestSpec::Reduce { chunk, chunks, rounds } => {
                        let secs = reduce_loop(plane, chunk as u64, chunks, rounds)?;
                        ctl.send(&Msg::Done {
                            elapsed_ns: (secs * 1e9) as u64,
                            bytes: 0,
                        })?;
                    }
                    TestSpec::C2STimeline { size, count, bucket_ms } => {
                        let bucket = Duration::from_millis(bucket_ms.max(1));
                        let sampler = TempSampler::start(bucket);
                        let r = plane.recv_timeline(size as usize, count, bucket);
                        let (temps_sys, temps_nic) = sampler.finish();
                        let (t, buckets, steady) = r?;
                        ctl.send(&Msg::DoneTimeline {
                            elapsed_ns: t.last.duration_since(t.first).as_nanos() as u64,
                            bytes: count.saturating_sub(1) * size as u64,
                            buckets,
                            steady: steady as u64,
                            temps_sys,
                            temps_nic,
                        })?;
                    }
                    TestSpec::LoadedPing { ping_size, bulk_size, bulk_count } => {
                        plane.loaded_ping_echoer(
                            ping_size as usize,
                            bulk_size as usize,
                            bulk_count,
                        )?;
                        ctl.send(&Msg::Done { elapsed_ns: 0, bytes: 0 })?;
                    }
                }
            }
            Msg::Bye => return Ok(()),
            other => bail!("unexpected control message during bench: {other:?}"),
        }
    }
}
