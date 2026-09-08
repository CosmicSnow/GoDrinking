//! Explicit lifecycle state machines: one Sala session, one Share inside it,
//! one media Link per watcher. Every transition goes through a typed event;
//! anything not listed is rejected. No booleans, no implied states.
//!
//! ```text
//! Sala: Closed -> Joining -> Open -> Closing -> Closed
//! Share: Stopped -> Starting -> Live -> Stopping -> Stopped
//! Link: Absent -> Negotiating -> Connected -> Closing -> Absent
//! ```

use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransitionError {
    pub from: &'static str,
    pub event: &'static str,
}

impl fmt::Display for TransitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid transition: {} + {}", self.from, self.event)
    }
}

impl std::error::Error for TransitionError {}

fn reject(from: &'static str, event: &'static str) -> TransitionError {
    TransitionError { from, event }
}

// ---------------------------------------------------------------------------
// Sala session
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SalaState {
    Closed,
    Joining,
    Open,
    Closing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SalaEvent {
    BeginJoin,
    Opened,
    BeginClose,
    Closed,
}

impl SalaState {
    pub fn name(&self) -> &'static str {
        match self {
            SalaState::Closed => "Closed",
            SalaState::Joining => "Joining",
            SalaState::Open => "Open",
            SalaState::Closing => "Closing",
        }
    }

    pub fn apply(&self, event: SalaEvent) -> Result<SalaState, TransitionError> {
        let ev = match event {
            SalaEvent::BeginJoin => "BeginJoin",
            SalaEvent::Opened => "Opened",
            SalaEvent::BeginClose => "BeginClose",
            SalaEvent::Closed => "Closed",
        };
        match (*self, event) {
            (SalaState::Closed, SalaEvent::BeginJoin) => Ok(SalaState::Joining),
            (SalaState::Joining, SalaEvent::Opened) => Ok(SalaState::Open),
            (SalaState::Open, SalaEvent::BeginClose) => Ok(SalaState::Closing),
            (SalaState::Closing, SalaEvent::Closed) => Ok(SalaState::Closed),
            _ => Err(reject(self.name(), ev)),
        }
    }
}

// ---------------------------------------------------------------------------
// Share
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShareState {
    Stopped,
    Starting,
    Live,
    Stopping,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShareEvent {
    BeginStart,
    BecameLive,
    BeginStop,
    Stopped,
}

impl ShareState {
    pub fn name(&self) -> &'static str {
        match self {
            ShareState::Stopped => "Stopped",
            ShareState::Starting => "Starting",
            ShareState::Live => "Live",
            ShareState::Stopping => "Stopping",
        }
    }

    pub fn apply(&self, event: ShareEvent) -> Result<ShareState, TransitionError> {
        let ev = match event {
            ShareEvent::BeginStart => "BeginStart",
            ShareEvent::BecameLive => "BecameLive",
            ShareEvent::BeginStop => "BeginStop",
            ShareEvent::Stopped => "Stopped",
        };
        match (*self, event) {
            (ShareState::Stopped, ShareEvent::BeginStart) => Ok(ShareState::Starting),
            (ShareState::Starting, ShareEvent::BecameLive) => Ok(ShareState::Live),
            (ShareState::Live, ShareEvent::BeginStop) => Ok(ShareState::Stopping),
            (ShareState::Stopping, ShareEvent::Stopped) => Ok(ShareState::Stopped),
            _ => Err(reject(self.name(), ev)),
        }
    }
}

// ---------------------------------------------------------------------------
// Media link (per watcher)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LinkState {
    Absent,
    Negotiating,
    Connected,
    Closing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkEvent {
    BeginNegotiate,
    Connected,
    BeginClose,
    Removed,
}

impl LinkState {
    pub fn name(&self) -> &'static str {
        match self {
            LinkState::Absent => "Absent",
            LinkState::Negotiating => "Negotiating",
            LinkState::Connected => "Connected",
            LinkState::Closing => "Closing",
        }
    }

    /// Live legs drive the derived watcher list; Closing/Absent never do.
    pub fn is_live(&self) -> bool {
        matches!(self, LinkState::Negotiating | LinkState::Connected)
    }

    pub fn apply(&self, event: LinkEvent) -> Result<LinkState, TransitionError> {
        let ev = match event {
            LinkEvent::BeginNegotiate => "BeginNegotiate",
            LinkEvent::Connected => "Connected",
            LinkEvent::BeginClose => "BeginClose",
            LinkEvent::Removed => "Removed",
        };
        match (*self, event) {
            (LinkState::Absent, LinkEvent::BeginNegotiate) => Ok(LinkState::Negotiating),
            (LinkState::Negotiating, LinkEvent::Connected) => Ok(LinkState::Connected),
            (
                LinkState::Negotiating | LinkState::Connected,
                LinkEvent::BeginClose,
            ) => Ok(LinkState::Closing),
            (LinkState::Closing, LinkEvent::Removed) => Ok(LinkState::Absent),
            _ => Err(reject(self.name(), ev)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sala_full_cycle() {
        let s = SalaState::Closed.apply(SalaEvent::BeginJoin).unwrap();
        let s = s.apply(SalaEvent::Opened).unwrap();
        assert_eq!(s, SalaState::Open);
        let s = s.apply(SalaEvent::BeginClose).unwrap();
        let s = s.apply(SalaEvent::Closed).unwrap();
        assert_eq!(s, SalaState::Closed);
    }

    #[test]
    fn sala_rejects_skips() {
        assert!(SalaState::Closed.apply(SalaEvent::Opened).is_err());
        assert!(SalaState::Open.apply(SalaEvent::BeginJoin).is_err());
        assert!(SalaState::Joining.apply(SalaEvent::Closed).is_err());
    }

    #[test]
    fn share_start_stop_cycle() {
        let s = ShareState::Stopped.apply(ShareEvent::BeginStart).unwrap();
        let s = s.apply(ShareEvent::BecameLive).unwrap();
        assert_eq!(s, ShareState::Live);
        let s = s.apply(ShareEvent::BeginStop).unwrap();
        let s = s.apply(ShareEvent::Stopped).unwrap();
        assert_eq!(s, ShareState::Stopped);
        assert!(ShareState::Live.apply(ShareEvent::BecameLive).is_err());
        assert!(ShareState::Stopped.apply(ShareEvent::BeginStop).is_err());
    }

    #[test]
    fn link_cycle_and_liveness() {
        let s = LinkState::Absent.apply(LinkEvent::BeginNegotiate).unwrap();
        assert!(s.is_live());
        let s = s.apply(LinkEvent::Connected).unwrap();
        assert!(s.is_live());
        let s = s.apply(LinkEvent::BeginClose).unwrap();
        assert!(!s.is_live());
        let s = s.apply(LinkEvent::Removed).unwrap();
        assert_eq!(s, LinkState::Absent);
        assert!(LinkState::Absent.apply(LinkEvent::Connected).is_err());
        assert!(LinkState::Connected.apply(LinkEvent::BeginNegotiate).is_err());
    }
}
