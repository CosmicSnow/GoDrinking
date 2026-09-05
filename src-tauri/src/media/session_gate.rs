//! Password, Admission, Ignore list, and pending Viewer decisions for LAN/Direct.

use super::logger;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const FAIL_WINDOW: Duration = Duration::from_secs(10 * 60);
const FAIL_LIMIT: usize = 5;
const IGNORE_FOR: Duration = Duration::from_secs(15 * 60);
const PENDING_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) struct SessionGate {
    password: Mutex<String>,
    admission: AtomicBool,
    ignore: Mutex<IgnoreList>,
    pending: Mutex<HashMap<String, PendingSlot>>,
}

struct IgnoreList {
    fails: HashMap<IpAddr, Vec<Instant>>,
    until: HashMap<IpAddr, Instant>,
}

struct PendingSlot {
    nickname: String,
    decision: SyncSender<bool>,
}

impl SessionGate {
    pub(crate) fn new(password: String, admission: bool) -> Self {
        Self {
            password: Mutex::new(password),
            admission: AtomicBool::new(admission),
            ignore: Mutex::new(IgnoreList {
                fails: HashMap::new(),
                until: HashMap::new(),
            }),
            pending: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn admission(&self) -> bool {
        self.admission.load(Ordering::Acquire)
    }

    pub(crate) fn set_admission(&self, value: bool) {
        self.admission.store(value, Ordering::Release);
    }

    pub(crate) fn set_password(&self, password: String) {
        if let Ok(mut current) = self.password.lock() {
            *current = password;
        }
    }

    pub(crate) fn password_set(&self) -> bool {
        self.password
            .lock()
            .map(|password| !password.is_empty())
            .unwrap_or(false)
    }

    pub(crate) fn is_ignored(&self, ip: IpAddr) -> bool {
        let Ok(mut ignore) = self.ignore.lock() else {
            return false;
        };
        if let Some(until) = ignore.until.get(&ip).copied() {
            if Instant::now() < until {
                logger::log(
                    "WARN",
                    "ignore list",
                    &format!("{ip} refused (on ignore list)"),
                );
                return true;
            }
            ignore.until.remove(&ip);
        }
        false
    }

    pub(crate) fn note_auth_failure(&self, ip: IpAddr) {
        let Ok(mut ignore) = self.ignore.lock() else {
            return;
        };
        let now = Instant::now();
        let fails = ignore.fails.entry(ip).or_default();
        fails.retain(|at| now.duration_since(*at) < FAIL_WINDOW);
        fails.push(now);
        if fails.len() >= FAIL_LIMIT {
            fails.clear();
            ignore.until.insert(ip, now + IGNORE_FOR);
            logger::log(
                "WARN",
                "ignore list",
                &format!("{ip} ignored for 15 minutes after 5 auth failures"),
            );
        }
    }

    pub(crate) fn auth_ok(&self, offered: &str) -> bool {
        let Ok(expected) = self.password.lock() else {
            return false;
        };
        passwords_match(expected.as_str(), offered)
    }

    pub(crate) fn register_pending(&self, id: String, nickname: String) -> Receiver<bool> {
        let (tx, rx) = sync_channel(1);
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(
                id,
                PendingSlot {
                    nickname,
                    decision: tx,
                },
            );
        }
        rx
    }

    pub(crate) fn wait_pending(rx: Receiver<bool>) -> bool {
        match rx.recv_timeout(PENDING_TIMEOUT) {
            Ok(accepted) => accepted,
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => false,
        }
    }

    pub(crate) fn decide(&self, id: &str, accept: bool) -> bool {
        let tx = {
            let Ok(mut pending) = self.pending.lock() else {
                return false;
            };
            pending.remove(id).map(|slot| slot.decision)
        };
        tx.and_then(|tx| tx.send(accept).ok()).is_some()
    }

    pub(crate) fn pending_roster(&self) -> Vec<(String, String)> {
        self.pending
            .lock()
            .map(|pending| {
                pending
                    .iter()
                    .map(|(id, slot)| (id.clone(), slot.nickname.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }
}

pub(crate) fn passwords_match(expected: &str, got: &str) -> bool {
    let a = expected.as_bytes();
    let b = got.as_bytes();
    let len = a.len().max(b.len());
    let mut diff = a.len() ^ b.len();
    for index in 0..len {
        let left = *a.get(index).unwrap_or(&0);
        let right = *b.get(index).unwrap_or(&0);
        diff |= (left ^ right) as usize;
    }
    diff == 0
}

pub(crate) fn valid_nickname(value: &str) -> bool {
    let trimmed = value.trim();
    let len = trimmed.chars().count();
    (2..=24).contains(&len)
        && trimmed
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == ' ' || matches!(ch, '_' | '-' | '.'))
}

#[cfg(test)]
mod tests {
    use super::{passwords_match, valid_nickname, SessionGate};
    use std::net::IpAddr;

    #[test]
    fn empty_passwords_match() {
        assert!(passwords_match("", ""));
        assert!(!passwords_match("a", ""));
        assert!(!passwords_match("", "a"));
        assert!(passwords_match("secret", "secret"));
    }

    #[test]
    fn nickname_rules() {
        assert!(valid_nickname("Ana"));
        assert!(!valid_nickname("A"));
        assert!(!valid_nickname("bad!"));
    }

    #[test]
    fn ignore_after_five_failures() {
        let gate = SessionGate::new(String::new(), false);
        let ip: IpAddr = "10.0.0.9".parse().unwrap();
        for _ in 0..4 {
            gate.note_auth_failure(ip);
            assert!(!gate.is_ignored(ip));
        }
        gate.note_auth_failure(ip);
        assert!(gate.is_ignored(ip));
    }
}
