//! Lightweight session file logger for debugging join failures.
//!
//! One file per session attempt (Host start or Viewer join) is written to
//! `{data_dir}/godrinking/logs/session-{timestamp}-{role}-{mode}-p{pid}.log`
//! and the last 30 files are kept. The pid suffix keeps two local instances
//! (e.g. a dev Host plus a portable Viewer on the same machine) from
//! sharing one file when they start in the same second. Passwords are never
//! written; raw Rendezvous error codes are, so a generic "Could not join."
//! can be traced back to denied/full/busy/invalid/unreachable in seconds.
//!
//! Thread-safe via a global `Mutex<Option<LoggerState>>`; every write is
//! flushed so a crash never loses the last line.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_SESSIONS: usize = 30;

struct LoggerState {
    file: Option<File>,
}

static LOGGER: Mutex<Option<LoggerState>> = Mutex::new(None);

fn now_parts() -> (i64, u32) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (now.as_secs() as i64, now.subsec_millis())
}

/// Formats a unix timestamp as `YYYYMMDD-HHMMSS` (UTC, civil-from-days).
fn stamp(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}")
}

fn logs_dir() -> Option<PathBuf> {
    let base = dirs::data_dir()?;
    Some(base.join("godrinking").join("logs"))
}

fn state() -> std::sync::MutexGuard<'static, Option<LoggerState>> {
    LOGGER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Keeps only the newest `MAX_SESSIONS` log files. File names sort
/// chronologically (`session-YYYYMMDD-HHMMSS-...`), so name sort == age sort.
fn prune(dir: &std::path::Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().map(|ext| ext == "log").unwrap_or(false))
        .collect();
    if files.len() <= MAX_SESSIONS {
        return;
    }
    files.sort();
    for old in files.iter().take(files.len() - MAX_SESSIONS) {
        let _ = fs::remove_file(old);
    }
}

/// Opens a fresh session file (Host start or Viewer join attempt) and prunes
/// old files beyond the last 30. The previous file, if any, is closed. The
/// pid suffix keeps concurrent local instances on separate files.
pub fn begin_session(role: &str, mode: &str) {
    let Some(dir) = logs_dir() else {
        return;
    };
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    prune(&dir);
    let (secs, _) = now_parts();
    let name = format!(
        "session-{}-{role}-{mode}-p{}.log",
        stamp(secs),
        std::process::id()
    );
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(&name))
        .ok();
    {
        let mut guard = state();
        *guard = Some(LoggerState { file });
    }
    log(
        "INFO",
        "session",
        &format!("log file opened ({role}/{mode})"),
    );
}

/// Writes `[HH:MM:SS.mmm] LEVEL event: details` to the current session file.
/// If no session file is open yet (or it was closed by `clear`), one is
/// created lazily so early logs are captured too.
pub fn log(level: &str, event: &str, details: &str) {
    let mut guard = state();
    let needs_file = match guard.as_ref() {
        None => true,
        Some(state) => state.file.is_none(),
    };
    if needs_file {
        let Some(dir) = logs_dir() else {
            return;
        };
        if fs::create_dir_all(&dir).is_err() {
            return;
        };
        // Same pid suffix as sessions: early logs from two local instances
        // must not share a file either.
        let (secs, _) = now_parts();
        let name = format!(
            "session-{}-app-app-p{}.log",
            stamp(secs),
            std::process::id()
        );
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(&name))
            .ok();
        *guard = Some(LoggerState { file });
    }
    let Some(state) = guard.as_mut() else {
        return;
    };
    let Some(file) = state.file.as_mut() else {
        return;
    };
    let (secs, millis) = now_parts();
    let rem = secs.rem_euclid(86_400);
    let line = format!(
        "[{:02}:{:02}:{:02}.{:03}] {level:<5} {event}: {details}\n",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60,
        millis
    );
    let _ = file.write_all(line.as_bytes());
    let _ = file.flush();
}

/// One session log file, ready to serialize to the UI.
#[derive(serde::Serialize)]
pub struct LogSession {
    /// File name without the `.log` extension, e.g.
    /// `session-20260902-153045-host-stunar`.
    pub session: String,
    /// Last-write time of the file, `YYYYMMDD-HHMMSS` (UTC).
    pub timestamp: String,
    pub lines: Vec<String>,
}

/// Returns the last `MAX_SESSIONS` session logs, newest first.
pub fn read_sessions() -> Vec<LogSession> {
    let Some(dir) = logs_dir() else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut files: Vec<(PathBuf, SystemTime)> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().map(|ext| ext == "log").unwrap_or(false))
        .filter_map(|path| {
            fs::metadata(&path)
                .ok()
                .and_then(|meta| meta.modified().ok())
                .map(|modified| (path, modified))
        })
        .collect();
    files.sort_by(|a, b| b.1.cmp(&a.1)); // newest first
    files.truncate(MAX_SESSIONS);
    files
        .into_iter()
        .filter_map(|(path, modified)| {
            let name = path.file_stem()?.to_string_lossy().into_owned();
            let content = fs::read_to_string(&path).unwrap_or_default();
            let lines = content.lines().map(|line| line.to_owned()).collect();
            let secs = modified.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
            Some(LogSession {
                session: name,
                timestamp: stamp(secs),
                lines,
            })
        })
        .collect()
}

/// Deletes every session log file and closes the open handle.
pub fn clear() {
    let Some(dir) = logs_dir() else {
        return;
    };
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.filter_map(|entry| entry.ok()) {
            let path = entry.path();
            if path.extension().map(|ext| ext == "log").unwrap_or(false) {
                let _ = fs::remove_file(path);
            }
        }
    }
    let mut guard = state();
    if let Some(state) = guard.as_mut() {
        state.file = None;
    }
}

/// Absolute path of the logs directory, for the "open folder" UI action.
pub fn logs_dir_string() -> Option<String> {
    logs_dir().map(|dir| dir.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::{begin_session, clear, log, read_sessions, stamp};

    #[test]
    fn stamp_formats_utc() {
        // 2026-09-02 15:30:45 UTC.
        assert_eq!(stamp(1_788_363_045), "20260902-153045");
        assert_eq!(stamp(0), "19700101-000000");
    }

    #[test]
    fn session_roundtrip_and_clear() {
        // Uses the real data dir; acceptable for a debug logger.
        // NOTE: other tests (and real app runs) share this dir and the
        // global file handle in parallel, so the marker is searched in
        // EVERY session file instead of assuming sessions[0] is ours.
        begin_session("host", "stunar");
        log("INFO", "test", "hello from the logger");
        let sessions = read_sessions();
        assert!(!sessions.is_empty(), "a session file should exist");
        assert!(
            sessions
                .iter()
                .flat_map(|session| session.lines.iter())
                .any(|line| line.contains("hello from the logger")),
            "the written line should be readable back"
        );
        clear();
        assert!(read_sessions().is_empty(), "clear should remove every file");
    }
}
