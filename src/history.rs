//! Run history: every completed benchmark appends a full record to
//! ~/.local/share/linkbench/history/, one JSON file per run. That archive
//! is the raw material for tuning work — each record carries the score,
//! all metrics, both nodes' CPU-tuning summaries, transport and path.

use crate::bench::Results;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// Unix seconds, UTC.
    pub ts: u64,
    /// Optional context label, e.g. "connectx 100G · rdma" from a batch.
    #[serde(default)]
    pub label: String,
    /// Free-form experiment notes ("riser B", "new DAC cable", …).
    #[serde(default)]
    pub notes: String,
    pub results: Results,
}

pub fn dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".local/share/linkbench/history"))
}

pub fn append(results: &Results, label: &str, notes: &str) -> Result<PathBuf> {
    let dir = dir().context("no HOME")?;
    std::fs::create_dir_all(&dir)?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let slug: String = if label.is_empty() {
        format!("{}-{}", results.transport, results.server_host)
    } else {
        label.to_string()
    }
    .chars()
    .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '_' })
    .collect();
    let entry = HistoryEntry {
        ts,
        label: label.to_string(),
        notes: notes.to_string(),
        results: results.clone(),
    };
    // ts + slug keeps names unique enough; collisions within one second
    // for the same label don't happen in practice (runs take seconds).
    let path = dir.join(format!("{ts}-{slug}.json"));
    std::fs::write(&path, serde_json::to_string_pretty(&entry)?)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

pub fn list() -> Vec<HistoryEntry> {
    let Some(dir) = dir() else { return Vec::new() };
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<HistoryEntry> = rd
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| {
            serde_json::from_str(&std::fs::read_to_string(e.path()).ok()?).ok()
        })
        .collect();
    out.sort_by_key(|e| e.ts);
    out
}

/// Unix seconds → "YYYY-MM-DD HH:MM" (UTC), no dependencies.
pub fn fmt_ts(secs: u64) -> String {
    let days = secs / 86400;
    let (h, m) = ((secs % 86400) / 3600, (secs % 3600) / 60);
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe as i64 + era * 400 + i64::from(mth <= 2);
    format!("{y:04}-{mth:02}-{d:02} {h:02}:{m:02}")
}
