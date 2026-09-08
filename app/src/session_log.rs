//! Session file log: redacted milestones for packaged verification.
//!
//! The packaged app writes no reachable stdout (`log show` stays empty),
//! so this is how the orchestrator verifies a bundle without guessing:
//! one `golive-<unixmillis>.log` per run under the platform log dir
//! (`~/Library/Logs/dev.golive.sala/` on macOS), rotation keeps the
//! newest 5, older files are deleted on init.
//!
//! Redaction is structural: every line is `[millis] kind k=v…` where values
//! are numbers, fixed kinds, backend names, or short member-id prefixes.
//! No SDP, candidates, tokens, passwords, pixels, window titles, file
//! paths, or free-form strings ever reach this file. Member ids are
//! truncated to 8 chars (same rule as window titles).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Log file prefix + rotation bound.
const FILE_PREFIX: &str = "golive-";
const FILE_SUFFIX: &str = ".log";
const KEEP_FILES: usize = 5;

/// Unix millis for filenames and line stamps (monotonic enough here;
// wall-clock jumps only reorder lines, never corrupt).
pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Short non-secret member identity (ids are opaque, not secrets, but
/// short is all any diagnostic needs).
pub fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// Session log handle. `Disabled` (tests, handle-less contexts) is a
/// silent no-op; `Live` appends timestamped lines under its own lock.
pub enum SessionLog {
    Disabled,
    Live { file: Mutex<File> },
}

impl SessionLog {
    /// Disabled handle for tests and handle-less contexts.
    pub fn disabled() -> Self {
        Self::Disabled
    }

    /// Open `dir` (created), start this run's file, then rotate old runs.
    /// Any failure degrades to `Disabled` (logged to stderr, never fatal).
    pub fn init_in(dir: &Path) -> Self {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("golive: session log dir failed ({dir:?}): {e}");
            return Self::Disabled;
        }
        let path = dir.join(format!("{FILE_PREFIX}{}{FILE_SUFFIX}", now_millis()));
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(file) => {
                eprintln!("golive: session log {}", path.display());
                rotate(dir);
                Self::Live { file: Mutex::new(file) }
            }
            Err(e) => {
                eprintln!("golive: session log open failed ({path:?}): {e}");
                Self::Disabled
            }
        }
    }

    /// Append one redacted line (already shaped by the caller).
    pub fn log(&self, line: &str) {
        if let Self::Live { file } = self {
            if let Ok(mut guard) = file.lock() {
                let _ = writeln!(guard, "[{}] {line}", now_millis());
            }
        }
    }
}

/// Delete oldest `golive-*.log` runs beyond [`KEEP_FILES`]. Errors are
/// best-effort (a crowded dir never fails startup).
fn rotate(dir: &Path) {
    let mut runs: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with(FILE_PREFIX) && name.ends_with(FILE_SUFFIX) {
                runs.push(path);
            }
        }
    }
    runs.sort();
    while runs.len() > KEEP_FILES {
        let oldest = runs.remove(0);
        let _ = std::fs::remove_file(&oldest);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_run_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "golive-session-test-{}-{}-{tag}",
            std::process::id(),
            now_millis()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn run_files(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("read dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(FILE_PREFIX))
            .collect();
        names.sort();
        names
    }

    #[test]
    fn creates_run_file_and_appends_lines() {
        let dir = temp_run_dir("create");
        let log = SessionLog::init_in(&dir);
        assert!(matches!(log, SessionLog::Live { .. }));
        log.log("share start kind=synthetic profile=1280x720@30");
        log.log("backend videotoolbox");
        drop(log);
        let files = run_files(&dir);
        assert_eq!(files.len(), 1);
        let text = std::fs::read_to_string(dir.join(&files[0])).expect("read log");
        assert!(text.contains("share start kind=synthetic"));
        assert!(text.contains("backend videotoolbox"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotation_keeps_newest_five() {
        let dir = temp_run_dir("rotate");
        for n in 0..7 {
            std::fs::write(dir.join(format!("{FILE_PREFIX}100{n}{FILE_SUFFIX}")), "x")
                .expect("plant");
        }
        let _log = SessionLog::init_in(&dir);
        drop(_log);
        let files = run_files(&dir);
        // 7 planted + 1 new run = 8 → oldest 3 deleted, 5 kept.
        assert_eq!(files.len(), 5);
        assert!(!files.iter().any(|n| n.contains("1000") || n.contains("1001") || n.contains("1002")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hostile_member_id_cannot_leak_a_secret() {
        // Member ids flow into watch/unwatch milestone lines truncated to
        // 8 chars: a hostile id carrying a sentinel value must not land
        // the VALUE in the file.
        let dir = temp_run_dir("redact");
        let log = SessionLog::init_in(&dir);
        let hostile = "ab token=hunter2-password=s3cret";
        log.log(&format!("watch member={}", short_id(hostile)));
        drop(log);
        let files = run_files(&dir);
        let text = std::fs::read_to_string(dir.join(&files[0])).expect("read log");
        assert!(!text.contains("hunter2"), "secret value leaked");
        assert!(!text.contains("s3cret"), "secret value leaked");
        assert!(!text.contains("password"), "secret key leaked");
        assert!(text.contains("watch member=ab token"), "truncated id present");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disabled_never_touches_disk() {
        let log = SessionLog::disabled();
        log.log("anything at all");
        // No panic, no file: nothing to assert on disk by construction.
    }
}
