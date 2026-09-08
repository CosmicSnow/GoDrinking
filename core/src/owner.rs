//! Serialized Sala owner: one `Mutex` around every Session/Share/Link.
//!
//! Every command locks, validates, mutates, and drops the lock — no two
//! commands interleave. Snapshots are immutable clones taken under the lock;
//! reading one never advances lifecycle, touches signaling, or runs media.
//!
//! Fencing: every async completion carries a [`Fence`]
//! `(session, share, link, attempt)`. A completion whose fence no longer
//! matches current state is discarded as stale ([`OwnerError::Stale`]) with
//! a redacted milestone. Re-watch advances the attempt on the SAME link id
//! (never re-rolls: the rendezvous rejects numerically-lower re-offers), so
//! in-flight work from the previous generation turns stale without touching
//! current resources.

use super::ids::{AttemptId, LinkId, SessionId, ShareId};
use super::state::{
    LinkEvent, LinkState, SalaEvent, SalaState, ShareEvent, ShareState, TransitionError,
};
use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

// ---------------------------------------------------------------------------
// Fence
// ---------------------------------------------------------------------------

/// Fences an asynchronous completion. `session` always identifies the Sala;
/// `share`/`link` scope further; `attempt` distinguishes generations so late
/// completions go stale instead of clobbering current state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fence {
    pub session: SessionId,
    pub share: Option<ShareId>,
    pub link: Option<LinkId>,
    pub attempt: AttemptId,
}

impl Fence {
    /// A fence that matches nothing. Useful for idempotent completions on
    /// empty state (e.g. stopping a share that was never started): the
    /// empty-state early-returns accept it without touching lifecycle.
    pub fn idle() -> Self {
        Self {
            session: SessionId::generate(),
            share: None,
            link: None,
            attempt: AttemptId::generate(),
        }
    }
}

// ---------------------------------------------------------------------------
// Snapshots (immutable, observation-only)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionSnapshot {
    pub id: Option<SessionId>,
    pub state: SalaState,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ShareSnapshot {
    pub id: Option<ShareId>,
    pub state: ShareState,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LinkSnapshot {
    pub id: LinkId,
    pub watcher: String,
    pub state: LinkState,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RosterEntry {
    pub member: String,
    pub watcher: bool,
}

/// Immutable observation of the whole owner. `watchers` derives from LIVE
/// links only — never from roster flags.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OwnerSnapshot {
    pub session: SessionSnapshot,
    pub share: ShareSnapshot,
    pub links: Vec<LinkSnapshot>,
    pub watchers: Vec<String>,
    pub roster: Vec<RosterEntry>,
}

// ---------------------------------------------------------------------------
// Errors (typed; Display carries redacted ids only)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OwnerError {
    NoSession,
    SessionNotOpen { state: SalaState },
    SessionBusy { state: SalaState },
    NoShare,
    ShareBusy { state: ShareState },
    NoLink,
    WatcherUnknown { watcher: String },
    Stale { reason: String },
    Transition(TransitionError),
    InvalidInput { reason: String },
}

impl fmt::Display for OwnerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OwnerError::NoSession => write!(f, "no session"),
            OwnerError::SessionNotOpen { state } => {
                write!(f, "session not open (state {:?})", state)
            }
            OwnerError::SessionBusy { state } => write!(f, "session busy (state {:?})", state),
            OwnerError::NoShare => write!(f, "no share"),
            OwnerError::ShareBusy { state } => write!(f, "share busy (state {:?})", state),
            OwnerError::NoLink => write!(f, "no such link"),
            OwnerError::WatcherUnknown { watcher } => write!(f, "unknown watcher {watcher}"),
            OwnerError::Stale { reason } => write!(f, "stale completion discarded: {reason}"),
            OwnerError::Transition(e) => write!(f, "{e}"),
            OwnerError::InvalidInput { reason } => write!(f, "invalid input: {reason}"),
        }
    }
}

impl std::error::Error for OwnerError {}

impl From<TransitionError> for OwnerError {
    fn from(e: TransitionError) -> Self {
        OwnerError::Transition(e)
    }
}

// ---------------------------------------------------------------------------
// Owner
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct LinkEntry {
    id: LinkId,
    watcher: String,
    state: LinkState,
    attempt: AttemptId,
}

#[derive(Default)]
struct Inner {
    session_id: Option<SessionId>,
    session_state: SalaState,
    session_attempt: Option<AttemptId>,
    share_id: Option<ShareId>,
    share_state: ShareState,
    share_attempt: Option<AttemptId>,
    links: HashMap<LinkId, LinkEntry>,
    watcher_to_link: HashMap<String, LinkId>,
    roster: HashMap<String, RosterEntry>,
}

impl Default for SalaState {
    fn default() -> Self {
        SalaState::Closed
    }
}

impl Default for ShareState {
    fn default() -> Self {
        ShareState::Stopped
    }
}

/// The single serialized owner of Sala/Share/Media-link state.
#[derive(Default)]
pub struct Owner {
    inner: Mutex<Inner>,
}

impl Owner {
    pub fn new() -> Self {
        Self::default()
    }

    // -- session ---------------------------------------------------------

    /// Validates `Closed`, allocates IDs, moves to `Joining`.
    pub fn begin_join(&self) -> Result<Fence, OwnerError> {
        let mut inner = self.inner.lock().expect("owner lock poisoned");
        if inner.session_state != SalaState::Closed {
            return Err(OwnerError::SessionBusy {
                state: inner.session_state,
            });
        }
        let session = SessionId::generate();
        let attempt = AttemptId::generate();
        inner.session_state = inner.session_state.apply(SalaEvent::BeginJoin)?;
        inner.session_id = Some(session);
        inner.session_attempt = Some(attempt);
        Ok(Fence {
            session,
            share: None,
            link: None,
            attempt,
        })
    }

    /// Join handshake completed. Fenced: stale attempts discard.
    pub fn complete_opened(&self, fence: &Fence) -> Result<(), OwnerError> {
        let mut inner = self.inner.lock().expect("owner lock poisoned");
        check_session_fence(&inner, fence)?;
        inner.session_state = inner.session_state.apply(SalaEvent::Opened)?;
        Ok(())
    }

    /// Validates `Open`, moves to `Closing`.
    pub fn begin_close(&self) -> Result<Fence, OwnerError> {
        let mut inner = self.inner.lock().expect("owner lock poisoned");
        let session = inner.session_id.ok_or(OwnerError::NoSession)?;
        if inner.session_state != SalaState::Open {
            return Err(OwnerError::SessionBusy {
                state: inner.session_state,
            });
        }
        let attempt = AttemptId::generate();
        inner.session_state = inner.session_state.apply(SalaEvent::BeginClose)?;
        inner.session_attempt = Some(attempt);
        Ok(Fence {
            session,
            share: None,
            link: None,
            attempt,
        })
    }

    /// Completes a close: deterministic teardown of share + links + IDs.
    pub fn complete_closed(&self, fence: &Fence) -> Result<(), OwnerError> {
        let mut inner = self.inner.lock().expect("owner lock poisoned");
        check_session_fence(&inner, fence)?;
        inner.session_state = inner.session_state.apply(SalaEvent::Closed)?;
        inner.session_id = None;
        inner.session_attempt = None;
        inner.share_id = None;
        inner.share_state = ShareState::Stopped;
        inner.share_attempt = None;
        inner.links.clear();
        inner.watcher_to_link.clear();
        Ok(())
    }

    // -- share -----------------------------------------------------------

    /// Validates session `Open` + share `Stopped`, moves to `Starting`.
    pub fn begin_share_start(&self) -> Result<Fence, OwnerError> {
        let mut inner = self.inner.lock().expect("owner lock poisoned");
        require_session_open(&inner)?;
        if inner.share_state != ShareState::Stopped {
            return Err(OwnerError::ShareBusy {
                state: inner.share_state,
            });
        }
        let session = inner.session_id.expect("session checked open");
        let share = ShareId::generate();
        let attempt = AttemptId::generate();
        inner.share_state = inner.share_state.apply(ShareEvent::BeginStart)?;
        inner.share_id = Some(share);
        inner.share_attempt = Some(attempt);
        Ok(Fence {
            session,
            share: Some(share),
            link: None,
            attempt,
        })
    }

    pub fn complete_share_live(&self, fence: &Fence) -> Result<(), OwnerError> {
        let mut inner = self.inner.lock().expect("owner lock poisoned");
        check_share_fence(&inner, fence)?;
        inner.share_state = inner.share_state.apply(ShareEvent::BecameLive)?;
        Ok(())
    }

    pub fn begin_share_stop(&self) -> Result<Fence, OwnerError> {
        let mut inner = self.inner.lock().expect("owner lock poisoned");
        require_session_open(&inner)?;
        if inner.share_state != ShareState::Live {
            return Err(OwnerError::ShareBusy {
                state: inner.share_state,
            });
        }
        let session = inner.session_id.expect("session checked open");
        let share = inner.share_id.ok_or(OwnerError::NoShare)?;
        let attempt = AttemptId::generate();
        inner.share_state = inner.share_state.apply(ShareEvent::BeginStop)?;
        inner.share_attempt = Some(attempt);
        Ok(Fence {
            session,
            share: Some(share),
            link: None,
            attempt,
        })
    }

    /// Completes a stop: `Stopped`, share IDs cleared, all links removed
    /// deterministically (watcher list becomes empty). IDEMPOTENT: completing
    /// an already-stopped share is a no-op (never a wedged state).
    pub fn complete_share_stopped(&self, fence: &Fence) -> Result<(), OwnerError> {
        let mut inner = self.inner.lock().expect("owner lock poisoned");
        if inner.share_state == ShareState::Stopped && inner.share_id.is_none() {
            return Ok(());
        }
        check_share_fence(&inner, fence)?;
        inner.share_state = inner.share_state.apply(ShareEvent::Stopped)?;
        inner.share_id = None;
        inner.share_attempt = None;
        inner.links.clear();
        inner.watcher_to_link.clear();
        Ok(())
    }

    // -- media links -----------------------------------------------------

    /// Creates or refreshes exactly one link for `watcher`. Re-watch keeps
    /// the LinkId but advances the attempt, so in-flight work from the
    /// previous generation turns stale. Requires an open session; a live
    /// share is NOT required to register intent.
    pub fn watch(&self, watcher: &str) -> Result<Fence, OwnerError> {
        if watcher.trim().is_empty() {
            return Err(OwnerError::InvalidInput {
                reason: "watcher must not be empty".into(),
            });
        }
        let mut inner = self.inner.lock().expect("owner lock poisoned");
        require_session_open(&inner)?;
        let session = inner.session_id.expect("session checked open");
        let share = inner.share_id;
        if let Some(link_id) = inner.watcher_to_link.get(watcher).copied() {
            // Same link: ADVANCE the attempt (never re-roll). The rendezvous
            // rejects offers numerically lower than the current attempt for
            // the same session/share/link key, so a fresh random id would go
            // stale ~50% of the time.
            let entry = inner.links.get_mut(&link_id).expect("index consistent");
            let attempt = entry.attempt.next();
            entry.attempt = attempt;
            return Ok(Fence {
                session,
                share,
                link: Some(link_id),
                attempt,
            });
        }
        let attempt = AttemptId::generate();
        let link_id = LinkId::generate();
        let state = LinkState::Absent.apply(LinkEvent::BeginNegotiate)?;
        inner.links.insert(
            link_id,
            LinkEntry {
                id: link_id,
                watcher: watcher.to_owned(),
                state,
                attempt,
            },
        );
        inner
            .watcher_to_link
            .insert(watcher.to_owned(), link_id);
        Ok(Fence {
            session,
            share,
            link: Some(link_id),
            attempt,
        })
    }

    /// Transport reports the link connected. Fenced.
    pub fn link_connected(&self, fence: &Fence) -> Result<(), OwnerError> {
        let mut inner = self.inner.lock().expect("owner lock poisoned");
        let entry = check_link_fence(&mut inner, fence)?;
        entry.state = entry.state.apply(LinkEvent::Connected)?;
        Ok(())
    }

    /// Closes exactly one watcher's link (`Closing`, fresh attempt).
    /// Idempotent while already closing.
    pub fn unwatch(&self, watcher: &str) -> Result<Fence, OwnerError> {
        let mut inner = self.inner.lock().expect("owner lock poisoned");
        let session = inner.session_id.ok_or(OwnerError::NoSession)?;
        let share = inner.share_id;
        let link_id = inner
            .watcher_to_link
            .get(watcher)
            .copied()
            .ok_or_else(|| OwnerError::WatcherUnknown {
                watcher: watcher.to_owned(),
            })?;
        let entry = inner.links.get_mut(&link_id).expect("index consistent");
        if entry.state == LinkState::Closing {
            return Ok(Fence {
                session,
                share,
                link: Some(link_id),
                attempt: entry.attempt,
            });
        }
        if !entry.state.is_live() {
            return Err(OwnerError::Stale {
                reason: "link not live".into(),
            });
        }
        let attempt = AttemptId::generate();
        entry.attempt = attempt;
        entry.state = entry.state.apply(LinkEvent::BeginClose)?;
        Ok(Fence {
            session,
            share,
            link: Some(link_id),
            attempt,
        })
    }

    /// Completes async link teardown. Fenced; removes the entry so the
    /// watcher leaves the derived list deterministically.
    pub fn complete_link_removed(&self, fence: &Fence) -> Result<(), OwnerError> {
        let mut inner = self.inner.lock().expect("owner lock poisoned");
        let link_id = fence.link.ok_or(OwnerError::NoLink)?;
        let current = inner.links.get(&link_id).ok_or(OwnerError::NoLink)?;
        if current.attempt != fence.attempt || inner.session_id != Some(fence.session) {
            emit_stale("link_remove", fence, "attempt superseded");
            return Err(OwnerError::Stale {
                reason: "link remove: attempt superseded".into(),
            });
        }
        let entry = inner.links.get_mut(&link_id).expect("checked above");
        entry.state = entry.state.apply(LinkEvent::Removed)?;
        debug_assert_eq!(entry.state, LinkState::Absent);
        let entry = inner.links.remove(&link_id).expect("checked above");
        inner.watcher_to_link.remove(&entry.watcher);
        Ok(())
    }

    // -- roster (observed signaling metadata) ------------------------------

    pub fn member_upsert(&self, member: &str, watcher: bool) -> Result<(), OwnerError> {
        if member.trim().is_empty() {
            return Err(OwnerError::InvalidInput {
                reason: "member must not be empty".into(),
            });
        }
        let mut inner = self.inner.lock().expect("owner lock poisoned");
        inner.roster.insert(
            member.to_owned(),
            RosterEntry {
                member: member.to_owned(),
                watcher,
            },
        );
        Ok(())
    }

    pub fn member_remove(&self, member: &str) {
        let mut inner = self.inner.lock().expect("owner lock poisoned");
        inner.roster.remove(member);
    }

    // -- observation --------------------------------------------------------

    /// Active watcher IDs derived from **live links only**, sorted.
    pub fn watchers(&self) -> Vec<String> {
        let inner = self.inner.lock().expect("owner lock poisoned");
        live_watchers(&inner)
    }

    /// Immutable snapshot. Locks only to clone; never runs lifecycle work.
    pub fn snapshot(&self) -> OwnerSnapshot {
        let inner = self.inner.lock().expect("owner lock poisoned");
        let mut links: Vec<LinkSnapshot> = inner
            .links
            .values()
            .map(|entry| LinkSnapshot {
                id: entry.id,
                watcher: entry.watcher.clone(),
                state: entry.state,
            })
            .collect();
        links.sort_by(|a, b| a.watcher.cmp(&b.watcher));
        let mut roster: Vec<RosterEntry> =
            inner.roster.values().cloned().collect();
        roster.sort_by(|a, b| a.member.cmp(&b.member));
        OwnerSnapshot {
            session: SessionSnapshot {
                id: inner.session_id,
                state: inner.session_state,
            },
            share: ShareSnapshot {
                id: inner.share_id,
                state: inner.share_state,
            },
            links,
            watchers: live_watchers(&inner),
            roster,
        }
    }
}

// ---------------------------------------------------------------------------
// Fencing helpers (lock held)
// ---------------------------------------------------------------------------

fn require_session_open(inner: &Inner) -> Result<(), OwnerError> {
    if inner.session_id.is_none() {
        return Err(OwnerError::NoSession);
    }
    if inner.session_state != SalaState::Open {
        return Err(OwnerError::SessionNotOpen {
            state: inner.session_state,
        });
    }
    Ok(())
}

fn check_session_fence(inner: &Inner, fence: &Fence) -> Result<(), OwnerError> {
    if inner.session_id != Some(fence.session) || inner.session_attempt != Some(fence.attempt) {
        emit_stale("session", fence, "session/attempt mismatch");
        return Err(OwnerError::Stale {
            reason: "session completion: attempt superseded".into(),
        });
    }
    Ok(())
}

fn share_fence_current(inner: &Inner, fence: &Fence) -> bool {
    inner.session_id == Some(fence.session)
        && inner.share_id == fence.share
        && inner.share_attempt == Some(fence.attempt)
}

fn check_share_fence(inner: &Inner, fence: &Fence) -> Result<(), OwnerError> {
    if !share_fence_current(inner, fence) {
        emit_stale("share", fence, "share/attempt mismatch");
        return Err(OwnerError::Stale {
            reason: "share completion: attempt superseded".into(),
        });
    }
    Ok(())
}

fn check_link_fence<'a>(
    inner: &'a mut Inner,
    fence: &Fence,
) -> Result<&'a mut LinkEntry, OwnerError> {
    let link_id = fence.link.ok_or(OwnerError::NoLink)?;
    if inner.session_id != Some(fence.session) {
        emit_stale("link", fence, "session mismatch");
        return Err(OwnerError::Stale {
            reason: "link completion: session superseded".into(),
        });
    }
    let entry = inner.links.get_mut(&link_id).ok_or(OwnerError::NoLink)?;
    if entry.attempt != fence.attempt {
        emit_stale("link", fence, "attempt superseded");
        return Err(OwnerError::Stale {
            reason: "link completion: attempt superseded".into(),
        });
    }
    Ok(entry)
}

fn live_watchers(inner: &Inner) -> Vec<String> {
    let mut out: Vec<String> = inner
        .links
        .values()
        .filter(|entry| entry.state.is_live())
        .map(|entry| entry.watcher.clone())
        .collect();
    out.sort();
    out
}

/// Redacted one-line JSON milestone. Short id prefixes only; no secrets,
/// SDP, tokens, or candidates ever flow through here.
fn emit_stale(kind: &str, fence: &Fence, detail: &str) {
    let share = fence
        .share
        .map(|s| s.to_string())
        .unwrap_or_else(|| "-".to_owned());
    let link = fence
        .link
        .map(|l| l.to_string())
        .unwrap_or_else(|| "-".to_owned());
    eprintln!(
        concat!(
            r#"{{"level":"INFO","subsystem":"domain","event":"stale_completion_discarded","#,
            r#""kind":"{kind}","session":"{session}","share":"{share}","link":"{link}","#,
            r#""attempt":"{attempt}","detail":"{detail}"}}"#
        ),
        kind = kind,
        session = fence.session,
        share = share,
        link = link,
        attempt = fence.attempt,
        detail = detail,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_owner() -> Owner {
        let owner = Owner::new();
        let fence = owner.begin_join().expect("begin join");
        owner.complete_opened(&fence).expect("opened");
        owner
    }

    #[test]
    fn join_open_close_cycle() {
        let owner = Owner::new();
        assert_eq!(owner.snapshot().session.state, SalaState::Closed);
        let fence = owner.begin_join().unwrap();
        assert_eq!(owner.snapshot().session.state, SalaState::Joining);
        owner.complete_opened(&fence).unwrap();
        assert_eq!(owner.snapshot().session.state, SalaState::Open);
        let fence = owner.begin_close().unwrap();
        assert_eq!(owner.snapshot().session.state, SalaState::Closing);
        owner.complete_closed(&fence).unwrap();
        let snap = owner.snapshot();
        assert_eq!(snap.session.state, SalaState::Closed);
        assert_eq!(snap.session.id, None);
    }

    #[test]
    fn stale_session_completion_rejected() {
        let owner = Owner::new();
        let first = owner.begin_join().unwrap();
        owner.complete_opened(&first).unwrap();
        // begin_close re-fences: completing the old open is now stale.
        let close = owner.begin_close().unwrap();
        assert!(matches!(
            owner.complete_opened(&first),
            Err(OwnerError::Stale { .. })
        ));
        owner.complete_closed(&close).unwrap();
        assert_eq!(owner.snapshot().session.state, SalaState::Closed);
    }

    #[test]
    fn share_start_stop_cycle() {
        let owner = open_owner();
        let fence = owner.begin_share_start().unwrap();
        owner.complete_share_live(&fence).unwrap();
        assert_eq!(owner.snapshot().share.state, ShareState::Live);
        let fence = owner.begin_share_stop().unwrap();
        owner.complete_share_stopped(&fence).unwrap();
        let snap = owner.snapshot();
        assert_eq!(snap.share.state, ShareState::Stopped);
        assert_eq!(snap.share.id, None);
    }

    #[test]
    fn stop_is_idempotent_never_wedged() {
        let owner = open_owner();
        // Completing a stop with no share started is a no-op, not an error.
        let idle = Fence {
            session: SessionId::generate(),
            share: None,
            link: None,
            attempt: AttemptId::generate(),
        };
        owner.complete_share_stopped(&idle).unwrap();
        assert_eq!(owner.snapshot().share.state, ShareState::Stopped);
        // Full cycle then double-complete with the same fence.
        let start = owner.begin_share_start().unwrap();
        owner.complete_share_live(&start).unwrap();
        let stop = owner.begin_share_stop().unwrap();
        owner.complete_share_stopped(&stop).unwrap();
        owner.complete_share_stopped(&stop).unwrap();
        owner.complete_share_stopped(&idle).unwrap();
        assert_eq!(owner.snapshot().share.state, ShareState::Stopped);
        // And a fresh start still works afterwards (no wedged state).
        let restart = owner.begin_share_start().unwrap();
        owner.complete_share_live(&restart).unwrap();
        assert_eq!(owner.snapshot().share.state, ShareState::Live);
    }

    #[test]
    fn failed_begin_changes_nothing_rollback_by_validation() {
        let owner = open_owner();
        // Validation runs before any mutation: failed begins are pure no-ops.
        let idle = owner.snapshot();
        assert!(owner.begin_join().is_err());
        assert_eq!(owner.snapshot(), idle);
        // One share starts; every conflicting begin fails without effect.
        let start = owner.begin_share_start().unwrap();
        let starting = owner.snapshot();
        assert_eq!(starting.share.state, ShareState::Starting);
        assert!(owner.begin_share_start().is_err());
        assert!(owner.begin_share_stop().is_err());
        assert_eq!(owner.snapshot(), starting);
        // The successful begin still completes normally afterwards.
        owner.complete_share_live(&start).unwrap();
        assert_eq!(owner.snapshot().share.state, ShareState::Live);
    }

    #[test]
    fn link_failure_is_isolated() {
        let owner = open_owner();
        let ana = owner.watch("ana").unwrap();
        let bob = owner.watch("bob").unwrap();
        // Ana's stale completion fails but Bob's link is untouched.
        let stale = Fence {
            attempt: AttemptId::generate(),
            ..ana
        };
        assert!(matches!(
            owner.link_connected(&stale),
            Err(OwnerError::Stale { .. })
        ));
        owner.link_connected(&bob).unwrap();
        let snap = owner.snapshot();
        assert_eq!(snap.links.len(), 2);
        assert_eq!(snap.links[1].state, LinkState::Connected);
        assert_eq!(snap.links[0].state, LinkState::Negotiating);
    }

    #[test]
    fn watch_unwatch_remove_is_deterministic() {
        let owner = open_owner();
        let fence = owner.watch("ana").unwrap();
        assert_eq!(owner.watchers(), vec!["ana".to_owned()]);
        let close = owner.unwatch("ana").unwrap();
        assert_eq!(close.link, fence.link);
        assert!(owner.watchers().is_empty());
        owner.complete_link_removed(&close).unwrap();
        assert!(owner.snapshot().links.is_empty());
        assert!(matches!(
            owner.unwatch("ana"),
            Err(OwnerError::WatcherUnknown { .. })
        ));
    }

    #[test]
    fn rewatch_refences_same_link() {
        let owner = open_owner();
        let first = owner.watch("ana").unwrap();
        let second = owner.watch("ana").unwrap();
        assert_eq!(first.link, second.link);
        assert_ne!(first.attempt, second.attempt);
        // Monotonic per link: the rendezvous rejects numerically-lower
        // re-offers on the same session/share/link key.
        assert_eq!(
            second.attempt.raw().wrapping_sub(first.attempt.raw()),
            1,
            "re-watch must advance the attempt"
        );
        assert!(matches!(
            owner.link_connected(&first),
            Err(OwnerError::Stale { .. })
        ));
        owner.link_connected(&second).unwrap();
        assert_eq!(owner.snapshot().links[0].state, LinkState::Connected);
    }

    #[test]
    fn watcher_list_derives_from_live_links_only() {
        let owner = open_owner();
        owner.member_upsert("", false).unwrap_err();
        // member_upsert with watcher=false creates no watcher.
        owner.member_upsert("lurker", false).unwrap();
        assert!(owner.watchers().is_empty());
        owner.watch("ana").unwrap();
        assert_eq!(owner.watchers(), vec!["ana".to_owned()]);
    }

    #[test]
    fn snapshot_is_observational() {
        let owner = open_owner();
        let before = owner.snapshot();
        assert_eq!(owner.snapshot(), before);
    }
}
