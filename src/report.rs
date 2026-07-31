//! Renders the single-pane answer to: "how fast and useful is the link
//! between these two computers, for ML work?"

use crate::bench::{Results, KIB, MIB};
use std::io::IsTerminal;

pub fn fmt_size(b: u64) -> String {
    if b >= MIB {
        format!("{} MiB", b / MIB)
    } else if b >= KIB {
        format!("{} KiB", b / KIB)
    } else {
        format!("{b} B")
    }
}

pub fn fmt_bytes(bps: f64) -> String {
    if bps >= 1e9 {
        format!("{:.2} GB", bps / 1e9)
    } else if bps >= 1e6 {
        format!("{:.1} MB", bps / 1e6)
    } else {
        format!("{:.0} kB", bps / 1e3)
    }
}

/// A transfer rate in both units: bytes/s (tool-native) and bits/s
/// (how links are marketed): "6.93 GB/s (55.4 Gb/s)".
pub fn fmt_rate(bps: f64) -> String {
    let bits = bps * 8.0;
    if bps >= 1e9 {
        format!("{:.2} GB/s ({:.1} Gb/s)", bps / 1e9, bits / 1e9)
    } else if bps >= 125e6 {
        // 1..8 Gb/s: still worth showing in Gb
        format!("{:.0} MB/s ({:.2} Gb/s)", bps / 1e6, bits / 1e9)
    } else if bps >= 1e6 {
        format!("{:.1} MB/s ({:.0} Mb/s)", bps / 1e6, bits / 1e6)
    } else {
        format!("{:.0} kB/s", bps / 1e3)
    }
}

/// Just the bits/s view: "55.4 Gb/s".
pub fn fmt_bits(bps: f64) -> String {
    let bits = bps * 8.0;
    if bits >= 1e9 {
        format!("{:.1} Gb/s", bits / 1e9)
    } else {
        format!("{:.0} Mb/s", bits / 1e6)
    }
}

struct Style {
    on: bool,
}

impl Style {
    fn new() -> Self {
        Self { on: std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none() }
    }
    fn paint(&self, code: &str, s: &str) -> String {
        if self.on {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    fn bold(&self, s: &str) -> String {
        self.paint("1", s)
    }
    fn dim(&self, s: &str) -> String {
        self.paint("2", s)
    }
    fn cyan(&self, s: &str) -> String {
        self.paint("36", s)
    }
    fn good(&self, s: &str) -> String {
        self.paint("32", s)
    }
    fn warn(&self, s: &str) -> String {
        self.paint("33", s)
    }
    fn bad(&self, s: &str) -> String {
        self.paint("31", s)
    }
}

/// Downsample to `width` columns and render as a block-height sparkline.
pub fn spark(vals: &[f64], width: usize) -> String {
    if vals.is_empty() {
        return String::new();
    }
    const LEVELS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let max = vals.iter().cloned().fold(f64::MIN, f64::max).max(1e-9);
    let cols = width.min(vals.len()).max(1);
    let per = vals.len() as f64 / cols as f64;
    (0..cols)
        .map(|i| {
            let a = (i as f64 * per) as usize;
            let b = (((i + 1) as f64 * per) as usize).clamp(a + 1, vals.len());
            let avg = vals[a..b].iter().sum::<f64>() / (b - a) as f64;
            LEVELS[((avg / max) * 7.0).round().clamp(0.0, 7.0) as usize]
        })
        .collect()
}

pub struct TimelineStats {
    pub median: f64,
    pub p5: f64,
    pub min: f64,
    pub dip_pct: f64,
    pub stalls: usize,
}

/// Stats over the steady portion of a timeline: the first bucket (partial)
/// and everything from `steady` onward (lane wind-down) are excluded.
pub fn timeline_stats(buckets: &[f64], steady: usize) -> Option<TimelineStats> {
    // steady == 0 means "unknown" (older results): fall back to trimming
    // just the trailing partial bucket.
    let end = if steady == 0 {
        buckets.len().saturating_sub(1)
    } else {
        steady.min(buckets.len())
    };
    if end < 5 {
        return None;
    }
    let mid = &buckets[1..end];
    let mut sorted: Vec<f64> = mid.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = sorted[sorted.len() / 2];
    let p5 = sorted[(sorted.len() as f64 * 0.05) as usize];
    let min = sorted[0];
    if median <= 0.0 {
        return None;
    }
    Some(TimelineStats {
        median,
        p5,
        min,
        dip_pct: (1.0 - min / median) * 100.0,
        stalls: mid.iter().filter(|b| **b < 0.5 * median).count(),
    })
}

fn temp_span(series: &[f32]) -> Option<(f32, f32)> {
    let first = *series.first()?;
    let max = series.iter().cloned().fold(f32::MIN, f32::max);
    Some((first, max))
}

pub fn max_temp(r: &Results) -> Option<f32> {
    [&r.temps_a_sys, &r.temps_a_nic, &r.temps_b_sys, &r.temps_b_nic]
        .iter()
        .flat_map(|s| s.iter().cloned())
        .fold(None, |acc: Option<f32>, t| Some(acc.map_or(t, |a| a.max(t))))
}

fn bar(value: f64, max: f64, width: usize) -> String {
    // log-ish scale so 10 GbE and NVLink can share one chart
    let norm = (value.max(1e6).log10() - 6.0) / (max.log10() - 6.0);
    let n = ((norm.clamp(0.02, 1.0)) * width as f64).round() as usize;
    "█".repeat(n.max(1))
}

/// Thresholds shared by the verdicts and the score, so the narrative and the
/// number can never disagree. The rule: the score penalizes exactly when a
/// verdict reads `Poor`. (A USB4 run showed +250 µs of wake-up lag — graded
/// Poor, penalized not at all, because these lived in two places.)
pub const BLOAT_POOR_X: f64 = 10.0;
pub const WAKEUP_POOR_US: f64 = 200.0;

/// The latency sample nearest `target` bytes. Callers used to match the sweep
/// size exactly, so a sweep that skipped 16 KiB silently produced NaN verdicts
/// and disabled the bufferbloat penalty.
pub fn latency_near(r: &Results, target: u64) -> Option<&crate::bench::LatencyPoint> {
    r.latency
        .iter()
        .min_by_key(|p| (p.size as i64 - target as i64).unsigned_abs())
}

pub enum Grade {
    Good(String),
    Ok(String),
    Poor(String),
}

pub fn verdicts(r: &Results) -> Vec<(String, Grade)> {
    let mut v = Vec::new();

    let bulk = r.uni_c2s_bps.max(r.uni_s2c_bps);
    let t7b = 14e9 / bulk;
    let t70b = 140e9 / bulk;
    v.push((
        "model movement (shards, checkpoints, weights)".into(),
        if bulk > 5e9 {
            Grade::Good(format!("7B fp16 in {t7b:.1} s, 70B in {t70b:.0} s"))
        } else if bulk > 1e9 {
            Grade::Ok(format!("7B fp16 in {t7b:.0} s, 70B in {t70b:.0} s — workable"))
        } else {
            Grade::Poor(format!("7B fp16 takes {t7b:.0} s — plan around it"))
        },
    ));

    let hop = latency_near(r, 16 * KIB)
        .map(|l| l.oneway_p50_us)
        .unwrap_or(f64::NAN);
    v.push((
        "pipeline-parallel inference (activation hop/token)".into(),
        if hop < 30.0 {
            Grade::Good(format!("+{hop:.0} µs/token/hop — negligible vs typical 10–50 ms/token"))
        } else if hop < 150.0 {
            Grade::Ok(format!("+{hop:.0} µs/token/hop — fine unless you chase <5 ms tokens"))
        } else {
            Grade::Poor(format!("+{hop:.0} µs/token/hop — will show up in token latency"))
        },
    ));

    let ar = r.allreduce_16k_us;
    let per_sec = 1e6 / ar;
    v.push((
        "tensor-parallel decode (per-token all-reduce)".into(),
        if ar < 40.0 {
            Grade::Good(format!("{ar:.0} µs/sync → ~{:.0}k syncs/s; TP2 across nodes is viable", per_sec / 1000.0))
        } else if ar < 150.0 {
            Grade::Ok(format!("{ar:.0} µs/sync — TP2 works but sync cost is visible per layer"))
        } else {
            Grade::Poor(format!("{ar:.0} µs/sync — cross-node TP will crawl; prefer pipeline splits"))
        },
    ));

    let gsync = r.allreduce_1g_bps;
    let step2g = 2e9 / gsync;
    v.push((
        "data-parallel training (gradient sync)".into(),
        if gsync > 4e9 {
            Grade::Good(format!("1B-param fp16 grads sync in {step2g:.2} s/step"))
        } else if gsync > 1e9 {
            Grade::Ok(format!("1B-param fp16 grads in {step2g:.1} s/step — overlap comm/compute"))
        } else {
            Grade::Poor(format!("1B-param fp16 grads in {step2g:.1} s/step — gradient compression territory"))
        },
    ));

    let stats: Vec<TimelineStats> = [
        (&r.timeline_up_bps, r.timeline_up_steady),
        (&r.timeline_down_bps, r.timeline_down_steady),
    ]
    .iter()
    .filter_map(|(b, s)| timeline_stats(b, *s))
    .collect();
    if !stats.is_empty() {
        let dip = stats.iter().map(|s| s.dip_pct).fold(0.0, f64::max);
        let stalls: usize = stats.iter().map(|s| s.stalls).sum();
        v.push((
            "sustained stability (throughput over time)".into(),
            if stalls == 0 && dip < 20.0 {
                Grade::Good(format!("steady — worst dip −{dip:.0}% from median"))
            } else if stalls == 0 && dip < 50.0 {
                Grade::Ok(format!("some jitter — worst dip −{dip:.0}%"))
            } else {
                Grade::Poor(format!(
                    "{stalls} stall bucket(s), worst dip −{dip:.0}% — check iommu/thermals/cabling"
                ))
            },
        ));
    }

    if let Some(l) = &r.loaded_rtt {
        let idle = latency_near(r, 16 * KIB)
            .map(|p| p.oneway_p50_us * 2.0)
            .unwrap_or(f64::NAN);
        let x = l.p50_us / idle.max(0.1);
        v.push((
            "latency while link is saturated (serving + bulk)".into(),
            if x < 3.0 {
                Grade::Good(format!("×{x:.1} inflation — fine to serve during transfers"))
            } else if x < BLOAT_POOR_X {
                Grade::Ok(format!("×{x:.1} inflation — schedule bulk moves off-peak"))
            } else {
                Grade::Poor(format!("×{x:.1} inflation (bufferbloat) — don't serve during bulk transfers"))
            },
        ));
    }

    if let (Some(hot), Some(cold)) = (
        r.idle_gaps.first(),
        r.idle_gaps.iter().find(|g| g.gap_ms >= 100),
    ) {
        let extra = cold.rtt_p50_us - hot.rtt_p50_us;
        v.push((
            "wake-up after idle (bursty pipeline traffic)".into(),
            if extra < 40.0 {
                Grade::Good(format!("+{extra:.0} µs after 100 ms idle — no warm-up penalty"))
            } else if extra < WAKEUP_POOR_US {
                Grade::Ok(format!("+{extra:.0} µs after 100 ms idle (coalescing/power states)"))
            } else {
                Grade::Poor(format!("+{extra:.0} µs after 100 ms idle — bursty workloads will feel this"))
            },
        ));
    }

    if let Some(t) = max_temp(r) {
        v.push((
            "thermals during sustained load".into(),
            if t < 80.0 {
                Grade::Good(format!("max {t:.0}°C"))
            } else if t < 92.0 {
                Grade::Ok(format!("max {t:.0}°C — warm, keep an eye on it"))
            } else {
                Grade::Poor(format!("max {t:.0}°C — throttling territory"))
            },
        ));
    }

    v.push((
        "KV-cache / prefix migration between nodes".into(),
        if r.kv32m_ms < 10.0 {
            Grade::Good(format!("{:.1} ms per 32 MiB block — live migration is practical", r.kv32m_ms))
        } else if r.kv32m_ms < 50.0 {
            Grade::Ok(format!("{:.0} ms per 32 MiB block", r.kv32m_ms))
        } else {
            Grade::Poor(format!("{:.0} ms per 32 MiB block — recompute may beat transfer", r.kv32m_ms))
        },
    ));

    v
}

pub fn render(r: &Results) {
    let st = Style::new();
    let w = 78;
    let rule = st.dim(&"─".repeat(w));

    println!();
    println!(
        "{}  {}",
        st.bold(&st.cyan("linkbench")),
        st.dim(&format!("{} ⇄ {}", r.client_host, r.server_host))
    );
    println!("{rule}");
    println!(
        "  transport {}   {}",
        st.bold(&r.transport),
        st.dim(&r.path)
    );
    if !r.tuning_a.is_empty() || !r.tuning_b.is_empty() {
        println!(
            "  {}",
            st.dim(&format!("cpu tuning   A: {}   B: {}", r.tuning_a, r.tuning_b))
        );
    }

    // --- the headline score
    let card = r
        .score
        .clone()
        .unwrap_or_else(|| crate::score::compute(r));
    println!();
    println!(
        "  {}   {}   {}",
        st.bold(&format!("{} SCORE", card.name.to_uppercase())),
        st.bold(&st.cyan(&format!("{:.1} / 10", card.score))),
        st.dim(&format!(
            "bandwidth {:.1} · latency {:.1}",
            crate::score::ScoreCard::axis_as_score(card.bandwidth_axis),
            crate::score::ScoreCard::axis_as_score(card.latency_axis),
        ))
    );
    println!("  {}", st.dim(&card.hint));
    for p in &card.penalties {
        println!("  {}", st.warn(&format!("penalty: {p}")));
    }
    for s in &card.skipped {
        println!(
            "  {}",
            st.dim(&format!("not checked: {s} — score is an upper bound"))
        );
    }

    // --- microbenchmarks
    println!();
    println!("  {}", st.bold("MICROBENCHMARKS"));
    println!(
        "    {:<22}{:>14}{:>14}{:>14}",
        st.dim("one-way latency"),
        st.dim("p50"),
        st.dim("p99"),
        st.dim("RTT mean")
    );
    for l in &r.latency {
        println!(
            "    {:<22}{:>11.1} µs{:>11.1} µs{:>11.1} µs",
            fmt_size(l.size),
            l.oneway_p50_us,
            l.oneway_p99_us,
            l.rtt_mean_us
        );
    }
    println!();
    println!(
        "    throughput   up {}   down {}",
        st.bold(&fmt_rate(r.uni_c2s_bps)),
        st.bold(&fmt_rate(r.uni_s2c_bps)),
    );
    println!(
        "                 bidir {}   {:.2} M msg/s",
        st.bold(&fmt_rate(r.bidir_agg_bps)),
        r.msg_rate_per_s / 1e6
    );
    println!();
    let peak = r.sweep.iter().map(|p| p.bps).fold(1.0, f64::max);
    for p in &r.sweep {
        let n = ((p.bps / peak) * 30.0).round().max(1.0) as usize;
        println!(
            "    {:>8}  {} {}",
            fmt_size(p.size),
            st.cyan(&"▮".repeat(n)),
            fmt_rate(p.bps)
        );
    }

    // --- sustained timelines
    if !r.timeline_up_bps.is_empty() || !r.timeline_down_bps.is_empty() {
        println!();
        let secs = r.timeline_up_bps.len() as f64 * r.timeline_bucket_ms as f64 / 1e3;
        println!(
            "  {}  {}",
            st.bold("SUSTAINED"),
            st.dim(&format!("({secs:.0} s, {} ms buckets)", r.timeline_bucket_ms))
        );
        for (label, buckets, steady) in [
            ("up", &r.timeline_up_bps, r.timeline_up_steady),
            ("down", &r.timeline_down_bps, r.timeline_down_steady),
        ] {
            if buckets.is_empty() {
                continue;
            }
            let stats = timeline_stats(buckets, steady);
            let tail = buckets.len().saturating_sub(steady);
            let extra = match &stats {
                Some(s) => format!(
                    "dip −{:.0}%{}{}",
                    s.dip_pct,
                    if s.stalls > 0 {
                        format!("  {} stalls", s.stalls)
                    } else {
                        String::new()
                    },
                    if tail > buckets.len() / 5 {
                        format!(
                            "  (lane skew: {:.1} s wind-down)",
                            tail as f64 * r.timeline_bucket_ms as f64 / 1e3
                        )
                    } else {
                        String::new()
                    }
                ),
                None => String::new(),
            };
            let median = stats.as_ref().map(|s| s.median).unwrap_or(0.0);
            println!(
                "    {:<5} {} {}  {}",
                label,
                st.cyan(&spark(buckets, 44)),
                fmt_rate(median),
                st.dim(&extra)
            );
        }
        let mut temp_bits = Vec::new();
        for (node, sys, nic) in [
            ("A", &r.temps_a_sys, &r.temps_a_nic),
            ("B", &r.temps_b_sys, &r.temps_b_nic),
        ] {
            if let Some((from, max)) = temp_span(sys) {
                let nic_s = temp_span(nic)
                    .map(|(_, m)| format!(" nic {m:.0}°C"))
                    .unwrap_or_default();
                temp_bits.push(format!("{node} sys {from:.0}→{max:.0}°C{nic_s}"));
            }
        }
        if !temp_bits.is_empty() {
            println!("    {} {}", st.dim("temps"), st.dim(&temp_bits.join("   ")));
        }
    }

    // --- behaviour under realistic pressure
    if r.loaded_rtt.is_some() || !r.idle_gaps.is_empty() {
        println!();
        println!("  {}", st.bold("UNDER PRESSURE"));
        if let Some(l) = &r.loaded_rtt {
            let idle_rtt = r
                .latency
                .iter()
                .find(|p| p.size == 16 * KIB)
                .map(|p| p.oneway_p50_us * 2.0);
            let vs = idle_rtt
                .map(|i| format!("  (idle {i:.0} µs → ×{:.1})", l.p50_us / i.max(0.1)))
                .unwrap_or_default();
            println!(
                "    16 KiB RTT during full-rate transfer   {} p50 / {} p99{}",
                st.bold(&format!("{:.0} µs", l.p50_us)),
                st.bold(&format!("{:.0} µs", l.p99_us)),
                st.dim(&vs)
            );
        }
        if !r.idle_gaps.is_empty() {
            let gaps: Vec<String> = r.idle_gaps.iter().map(|g| format!("{}", g.gap_ms)).collect();
            let p50s: Vec<String> =
                r.idle_gaps.iter().map(|g| format!("{:.0}", g.rtt_p50_us)).collect();
            println!(
                "    16 KiB RTT after {} ms idle{}{} µs p50",
                gaps.join("/"),
                " ".repeat(14usize.saturating_sub(gaps.join("/").len())),
                st.bold(&p50s.join(" / "))
            );
        }
    }

    // --- ML scenarios
    println!();
    println!("  {}", st.bold("ML SCENARIOS"));
    let rows: Vec<(String, String)> = vec![
        (
            "activation hop, 16 KiB (pipeline parallel)".into(),
            r.latency
                .iter()
                .find(|l| l.size == 16 * KIB)
                .map(|l| format!("{:.1} µs one-way", l.oneway_p50_us))
                .unwrap_or_default(),
        ),
        (
            "per-token all-reduce, 16 KiB (tensor parallel)".into(),
            format!("{:.1} µs", r.allreduce_16k_us),
        ),
        (
            "gradient all-reduce, 1 GiB (data parallel)".into(),
            format!("{} effective", fmt_rate(r.allreduce_1g_bps)),
        ),
        ("KV-cache block, 32 MiB".into(), format!("{:.2} ms", r.kv32m_ms)),
        (
            "weight streaming (sustained one-way)".into(),
            fmt_rate(r.uni_c2s_bps.max(r.uni_s2c_bps)),
        ),
    ];
    for (name, val) in rows {
        println!("    {name:<48} {}", st.bold(&val));
    }

    // --- context
    println!();
    println!(
        "  {}  {}",
        st.bold("CONTEXT"),
        st.dim("(big-message bandwidth, log scale)")
    );
    let bulk = r.uni_c2s_bps.max(r.uni_s2c_bps);
    let refs: Vec<(String, f64, bool)> = vec![
        ("this link".to_string(), bulk, true),
        ("10 GbE".into(), 1.25e9, false),
        ("TB4/USB4".into(), 3.8e9, false),
        ("100 GbE line rate".into(), 12.5e9, false),
        ("PCIe 4.0 x16".into(), 32e9, false),
        ("NVLink 4".into(), 450e9, false),
    ];
    let max = refs.iter().map(|r| r.1).fold(1.0, f64::max);
    for (name, bps, me) in refs {
        let b = bar(bps, max, 34);
        let line = format!("    {:<18} {} {}", name, b, fmt_rate(bps));
        println!("{}", if me { st.cyan(&line) } else { st.dim(&line) });
    }

    // --- verdicts
    println!();
    println!("  {}", st.bold("VERDICT"));
    for (name, grade) in verdicts(r) {
        let (mark, text) = match &grade {
            Grade::Good(t) => (st.good("✓"), t.clone()),
            Grade::Ok(t) => (st.warn("~"), t.clone()),
            Grade::Poor(t) => (st.bad("✗"), t.clone()),
        };
        println!("    {mark} {:<46} {}", name, st.dim(&text));
    }
    println!("{rule}");
    println!();
}
