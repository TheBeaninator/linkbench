//! The LinkBench2026 score: a 1.0–10.0 rating of a two-node link for
//! paired-LLM work, anchored to the hobbyist space of its era.
//!
//! - 1.0 — 2.5GbE Ethernet: the bare minimum that technically works.
//! - 10.0 — an NVLink-bridged flagship pair (H200 NVL / GH200 class):
//!   the pinnacle a (moderately wealthy) hobbyist can buy outright.
//!   Hyperscaler-only fabric is out of scope.
//!
//! Two log-scaled axes — sustained bandwidth and measured 16 KiB
//! all-reduce latency — combined by geometric mean (a link must be good
//! at both; imbalance is punished the way real workloads punish it),
//! then nudged down by real-workload penalties (stalls, bufferbloat,
//! wake-up lag). Sub-floor links clamp to 1.0.

use crate::bench::Results;
use crate::report::timeline_stats;
use serde::{Deserialize, Serialize};

pub const SCORE_NAME: &str = "LinkBench2026";

const BW_FLOOR: f64 = 0.29e9; // 2.5GbE effective bytes/s
const BW_CEIL: f64 = 900e9; // NVLink-bridged flagship pair
const AR_FLOOR_US: f64 = 300.0; // 16 KiB all-reduce over 2.5GbE
const AR_CEIL_US: f64 = 5.0; // over NVLink

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoreCard {
    pub name: String,
    /// 1.0–10.0, one decimal is the intended display precision.
    pub score: f64,
    /// Raw 0–1 axis positions (log-scaled between the era anchors).
    pub bandwidth_axis: f64,
    pub latency_axis: f64,
    pub penalties: Vec<String>,
    /// Checks that could not run on this link (too-short timeline, single QP,
    /// missing latency sample). A score with skipped checks is an optimistic
    /// upper bound and says so rather than quietly omitting the penalty.
    #[serde(default)]
    pub skipped: Vec<String>,
    pub hint: String,
}

impl ScoreCard {
    /// An axis shown on the same 1–10 scale as the headline score.
    pub fn axis_as_score(axis: f64) -> f64 {
        1.0 + 9.0 * axis.clamp(0.0, 1.0)
    }
}

/// Sub-floor performance clamps to a small epsilon rather than zero: under
/// the geometric mean a zero axis would erase the other axis entirely, but
/// a link with floor-grade latency and 20x-floor bandwidth is still more
/// useful than 2.5GbE (bucketed gradient sync, weight shuffling). The
/// epsilon keeps the weak axis dominant without making it absolute.
fn axis(x: f64, floor: f64, ceil: f64) -> f64 {
    ((x / floor).ln() / (ceil / floor).ln()).clamp(0.02, 1.0)
}

pub fn compute(r: &Results) -> ScoreCard {
    let bw = r.uni_c2s_bps.max(r.uni_s2c_bps).max(1.0);
    let bandwidth_axis = axis(bw, BW_FLOOR, BW_CEIL);

    // Lower is better: position of the measured all-reduce between the
    // floor (300 µs) and ceiling (5 µs) anchors.
    let ar = r.allreduce_16k_us.max(0.1);
    let latency_axis = axis(AR_FLOOR_US / ar, 1.0, AR_FLOOR_US / AR_CEIL_US);

    let mut combined = (bandwidth_axis * latency_axis).sqrt();
    let mut penalties = Vec::new();
    let mut skipped = Vec::new();

    let stats: Vec<_> = [
        (&r.timeline_up_bps, r.timeline_up_steady),
        (&r.timeline_down_bps, r.timeline_down_steady),
    ]
    .iter()
    .filter_map(|(b, s)| timeline_stats(b, *s))
    .collect();
    if stats.is_empty() {
        skipped.push("sustained-throughput stability (timeline too short)".into());
    }
    let stalled = stats.iter().any(|s| s.stalls > 0);
    if stalled {
        combined *= 0.85;
        penalties.push("sustained-throughput stalls (−15%)".into());
    }

    match (&r.loaded_rtt, crate::report::latency_near(r, 16 * 1024)) {
        (Some(l), Some(idle)) => {
            if l.p50_us / (idle.oneway_p50_us * 2.0).max(0.1) > crate::report::BLOAT_POOR_X {
                combined *= 0.9;
                penalties.push("bufferbloat under load (−10%)".into());
            }
        }
        // Single-QP devices skip the loaded-latency test entirely, so this
        // penalty can never fire for them — that must be visible.
        _ => skipped.push("latency under load (needs ≥2 QPs/streams)".into()),
    }

    if let (Some(hot), Some(cold)) = (
        r.idle_gaps.first(),
        r.idle_gaps.iter().find(|g| g.gap_ms >= 100),
    ) {
        if cold.rtt_p50_us - hot.rtt_p50_us > crate::report::WAKEUP_POOR_US {
            combined *= 0.95;
            penalties.push("after-idle wake-up lag (−5%)".into());
        }
    }

    let hint = if latency_axis + 0.15 < bandwidth_axis {
        "latency-bound: a lower-latency transport (e.g. hardware RDMA) raises this most"
    } else if bandwidth_axis + 0.15 < latency_axis {
        "bandwidth-bound: a faster link or more lanes raises this most"
    } else {
        "balanced: bandwidth and latency limit this link about equally"
    };

    ScoreCard {
        name: SCORE_NAME.into(),
        score: 1.0 + 9.0 * combined.clamp(0.0, 1.0),
        bandwidth_axis,
        latency_axis,
        penalties,
        skipped,
        hint: hint.into(),
    }
}
