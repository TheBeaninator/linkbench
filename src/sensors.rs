//! Temperature sampling via /sys/class/hwmon — no dependencies, works on
//! anything Linux. Two aggregate series: "sys" (hottest non-NIC sensor,
//! typically the CPU package) and "nic" (mlx5/ConnectX-style adapters).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Chip {
    is_nic: bool,
    temp_files: Vec<PathBuf>,
}

pub struct Sensors {
    chips: Vec<Chip>,
}

impl Sensors {
    pub fn discover() -> Self {
        let mut chips = Vec::new();
        let Ok(rd) = std::fs::read_dir("/sys/class/hwmon") else {
            return Self { chips };
        };
        for entry in rd.flatten() {
            let dir = entry.path();
            let name = std::fs::read_to_string(dir.join("name"))
                .unwrap_or_default()
                .trim()
                .to_lowercase();
            let is_nic = name.contains("mlx") || name.contains("ibv");
            let mut temp_files = Vec::new();
            if let Ok(files) = std::fs::read_dir(&dir) {
                for f in files.flatten() {
                    let fname = f.file_name().to_string_lossy().into_owned();
                    if fname.starts_with("temp") && fname.ends_with("_input") {
                        temp_files.push(f.path());
                    }
                }
            }
            if !temp_files.is_empty() {
                chips.push(Chip { is_nic, temp_files });
            }
        }
        Self { chips }
    }

    /// (hottest non-NIC °C, hottest NIC °C) — None where nothing readable.
    pub fn sample(&self) -> (Option<f32>, Option<f32>) {
        let mut sys: Option<f32> = None;
        let mut nic: Option<f32> = None;
        for chip in &self.chips {
            for f in &chip.temp_files {
                let Ok(s) = std::fs::read_to_string(f) else { continue };
                let Ok(milli) = s.trim().parse::<i64>() else { continue };
                let c = milli as f32 / 1000.0;
                if !(-50.0..=150.0).contains(&c) {
                    continue;
                }
                let slot = if chip.is_nic { &mut nic } else { &mut sys };
                *slot = Some(slot.map_or(c, |m: f32| m.max(c)));
            }
        }
        (sys, nic)
    }
}

/// Background sampler: one (sys, nic) reading per interval until stopped.
pub struct TempSampler {
    stop: Arc<AtomicBool>,
    out: Arc<Mutex<(Vec<f32>, Vec<f32>)>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl TempSampler {
    pub fn start(interval: Duration) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let out = Arc::new(Mutex::new((Vec::new(), Vec::new())));
        let (stop2, out2) = (stop.clone(), out.clone());
        let handle = std::thread::spawn(move || {
            let sensors = Sensors::discover();
            while !stop2.load(Ordering::Relaxed) {
                let (sys, nic) = sensors.sample();
                {
                    let mut o = out2.lock().unwrap();
                    if let Some(t) = sys {
                        o.0.push(t);
                    }
                    if let Some(t) = nic {
                        o.1.push(t);
                    }
                }
                std::thread::sleep(interval);
            }
        });
        Self { stop, out, handle: Some(handle) }
    }

    pub fn finish(mut self) -> (Vec<f32>, Vec<f32>) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        let o = self.out.lock().unwrap();
        o.clone()
    }
}
