//! Opt-in local media diagnostics. Set GOLIVE_TRACE_DIR before launch, or
//! place `.golive-media-trace` beside an unbundled executable.
//! Only fixed stage names and numeric measurements can enter the trace.
//! No background polling: active stages flush roughly once per second.

use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Capture,
    Source,
    Encode,
    Send,
    Rtp,
    Decode,
    Present,
}

#[derive(Default, serde::Serialize)]
pub struct Sample {
    pub frames: u64,
    pub bytes: u64,
    pub dropped: u64,
    pub timeouts: u64,
    pub errors: u64,
    pub keyframes: u64,
    pub repeats: u64,
    pub gpu_frames: u64,
    /// Viewer loss recovery: PLIs actually sent (debounced ~1/s per SSRC).
    pub pli_sent: u64,
    /// Viewer loss recovery: AU gaps that asked for no PLI (debounce held).
    pub pli_suppressed: u64,
    /// Publisher loss recovery: inbound FIR/PLI requests applied as force-intra.
    pub intra_applied: u64,
    /// Presenter pacing: max gap between consecutive successful acks within
    /// the record (microseconds, 0 when fewer than 2 acks). Max-merged, not summed.
    pub max_gap_us: u64,
    pub width: u32,
    pub height: u32,
    pub target_fps: u32,
}

#[derive(serde::Serialize)]
struct Record {
    version: u32,
    pid: u32,
    instance: u64,
    stage: Stage,
    timestamp_ms: u64,
    elapsed_us: u64,
    work_us: u64,
    max_work_us: u64,
    observations: u64,
    #[serde(flatten)]
    sample: Sample,
}

pub struct Trace(Option<Active>);
struct Active {
    file: File,
    since: Instant,
    record: Record,
}

fn trace_directory(env: Option<&OsStr>, executable: Option<&Path>) -> Option<PathBuf> {
    match env {
        Some(value) if value.is_empty() => None,
        Some(value) => Some(PathBuf::from(value)),
        None => {
            let parent = executable?.parent()?;
            parent
                .join(".golive-media-trace")
                .is_file()
                .then(|| parent.join("media-trace"))
        }
    }
}

impl Trace {
    pub fn new(stage: Stage) -> Self {
        let env = std::env::var_os("GOLIVE_TRACE_DIR");
        let executable = std::env::current_exe().ok();
        match trace_directory(env.as_deref(), executable.as_deref()) {
            Some(dir) => Self::open(stage, &dir),
            None => Self(None),
        }
    }

    fn open(stage: Stage, dir: &Path) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let file = std::fs::create_dir_all(dir).and_then(|_| {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join(format!("golive-trace-{}.jsonl", std::process::id())))
        });
        match file {
            Ok(file) => Self(Some(Active {
                file,
                since: Instant::now(),
                record: Record {
                    version: 1,
                    pid: std::process::id(),
                    instance: NEXT.fetch_add(1, Ordering::Relaxed),
                    stage,
                    timestamp_ms: 0,
                    elapsed_us: 0,
                    work_us: 0,
                    max_work_us: 0,
                    observations: 0,
                    sample: Sample::default(),
                },
            })),
            Err(_) => {
                eprintln!("golive: debug trace unavailable");
                Self(None)
            }
        }
    }

    pub fn start(&self) -> Option<Instant> {
        self.0.as_ref().map(|_| Instant::now())
    }

    pub fn record(&mut self, sample: Sample, started: Option<Instant>) {
        let Some(active) = self.0.as_mut() else {
            return;
        };
        let r = &mut active.record;
        let us = started.map(|t| t.elapsed().as_micros() as u64).unwrap_or(0);
        r.observations += 1;
        r.work_us += us;
        r.max_work_us = r.max_work_us.max(us);
        macro_rules! sum { ($($f:ident),*) => { $(r.sample.$f += sample.$f;)* }; }
        sum!(frames, bytes, dropped, timeouts, errors, keyframes, repeats, gpu_frames,
             pli_sent, pli_suppressed, intra_applied);
        // Pacing extremes never average away: keep the worst ack gap seen.
        r.sample.max_gap_us = r.sample.max_gap_us.max(sample.max_gap_us);
        if sample.width != 0 {
            r.sample.width = sample.width;
        }
        if sample.height != 0 {
            r.sample.height = sample.height;
        }
        if sample.target_fps != 0 {
            r.sample.target_fps = sample.target_fps;
        }
        if active.since.elapsed() >= Duration::from_secs(1) {
            self.flush();
        }
    }

    fn flush(&mut self) {
        let Some(active) = self.0.as_mut() else {
            return;
        };
        if active.record.observations == 0 {
            return;
        }
        active.record.elapsed_us = active.since.elapsed().as_micros() as u64;
        active.record.timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let result = serde_json::to_vec(&active.record)
            .ok()
            .and_then(|mut bytes| {
                bytes.push(b'\n');
                active.file.write_all(&bytes).ok()
            });
        if result.is_none() {
            self.0 = None;
            return;
        }
        active.since = Instant::now();
        active.record.sample = Sample::default();
        active.record.work_us = 0;
        active.record.max_work_us = 0;
        active.record.observations = 0;
    }
}

impl Drop for Trace {
    fn drop(&mut self) {
        self.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_launch_marker_enables_trace_without_environment() {
        let dir = std::env::temp_dir().join(format!("golive-trace-marker-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let executable = dir.join("goDrinking");
        assert_eq!(trace_directory(None, Some(&executable)), None);
        std::fs::write(dir.join(".golive-media-trace"), b"").unwrap();
        let output = trace_directory(None, Some(&executable)).expect("normal launch enabled");
        assert_eq!(output, dir.join("media-trace"));
        let mut trace = Trace::open(Stage::Capture, &output);
        trace.record(
            Sample {
                frames: 1,
                ..Default::default()
            },
            None,
        );
        drop(trace);
        let file = output.join(format!("golive-trace-{}.jsonl", std::process::id()));
        let record: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
        assert_eq!(record["frames"], 1);
        // Explicit configuration wins, including an empty value to disable.
        assert_eq!(
            trace_directory(Some(OsStr::new("")), Some(&executable)),
            None
        );
        assert_eq!(
            trace_directory(Some(dir.as_os_str()), Some(&executable)),
            Some(dir.clone())
        );
        std::fs::remove_file(dir.join(".golive-media-trace")).unwrap();
        assert_eq!(trace_directory(None, Some(&executable)), None);
        assert_eq!(trace_directory(None, None), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn disabled_trace_does_no_work() {
        let mut trace = Trace(None);
        assert!(trace.start().is_none());
        trace.record(
            Sample {
                frames: 1,
                ..Default::default()
            },
            None,
        );
        assert!(trace.0.is_none());
    }

    #[test]
    fn active_trace_flushes_without_waiting_for_shutdown() {
        let dir =
            std::env::temp_dir().join(format!("golive-trace-periodic-{}", std::process::id()));
        let mut trace = Trace::open(Stage::Capture, &dir);
        let file = dir.join(format!("golive-trace-{}.jsonl", std::process::id()));
        trace.record(
            Sample {
                frames: 1,
                ..Default::default()
            },
            None,
        );
        assert!(std::fs::read_to_string(&file).unwrap().is_empty());
        trace.0.as_mut().unwrap().since = Instant::now() - Duration::from_secs(2);
        trace.record(
            Sample {
                timeouts: 1,
                ..Default::default()
            },
            None,
        );
        let content = std::fs::read_to_string(&file).unwrap();
        let r: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(r["frames"], 1);
        assert_eq!(r["timeouts"], 1);
        assert!(r["elapsed_us"].as_u64().unwrap() >= 2_000_000);
        trace.record(
            Sample {
                frames: 3,
                ..Default::default()
            },
            None,
        );
        drop(trace);
        let content = std::fs::read_to_string(&file).unwrap();
        assert_eq!(content.lines().count(), 2);
        let r: serde_json::Value = serde_json::from_str(content.lines().last().unwrap()).unwrap();
        assert_eq!(r["frames"], 3);
        assert_eq!(r["timeouts"], 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn trace_aggregates_and_only_emits_fixed_names_and_numbers() {
        let dir = std::env::temp_dir().join(format!("golive-trace-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut trace = Trace::open(Stage::Present, &dir);
        for _ in 0..2 {
            trace.record(
                Sample {
                    frames: 1,
                    bytes: 16,
                    repeats: 1,
                    pli_sent: 1,
                    pli_suppressed: 4,
                    intra_applied: 1,
                    max_gap_us: 40_000,
                    ..Default::default()
                },
                None,
            );
        }
        drop(trace);
        let file = dir.join(format!("golive-trace-{}.jsonl", std::process::id()));
        let content = std::fs::read_to_string(&file).unwrap();
        let r: serde_json::Value = serde_json::from_str(content.lines().last().unwrap()).unwrap();
        assert_eq!(r["frames"], 2);
        assert_eq!(r["bytes"], 32);
        assert_eq!(r["repeats"], 2);
        assert_eq!(r["pli_sent"], 2, "loss counters sum like the rest");
        assert_eq!(r["pli_suppressed"], 8);
        assert_eq!(r["intra_applied"], 2);
        assert_eq!(r["max_gap_us"], 40_000, "pacing keeps the worst gap, never a sum");
        for (key, value) in r.as_object().unwrap() {
            if key == "stage" {
                assert_eq!(value, "present");
            } else {
                assert!(value.is_number(), "only numeric diagnostics: {key}");
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
