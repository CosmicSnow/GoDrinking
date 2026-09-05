//! Serialized control-plane state used by the media actor.
//!
//! This module deliberately contains metadata only.  Frames, encoded access
//! units, and transport queues stay outside the actor mailbox.

use super::peer_transport::PeerSignal;
use super::types::MediaLifecycleState;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub(crate) struct SessionEpoch(pub(crate) u64);

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub(crate) struct ShareEpoch(pub(crate) u64);

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub(crate) struct LinkId(pub(crate) u64);

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub(crate) struct OperationId(pub(crate) u64);

/// Opaque identity for one exact offer attempt. It is deliberately separate
/// from Viewer ID and LinkId: retries for one link must be able to carry a
/// fresh identity without relying on drain-time state.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub(crate) struct OfferAttemptId(u64);

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct EpochFence {
    pub(crate) session: SessionEpoch,
    pub(crate) share: ShareEpoch,
    pub(crate) link: Option<LinkId>,
}

/// The fence carried by offer/answer work. `attempt` is allocated at offer
/// ingress, so a completion can be rejected without consulting current
/// Viewer or drain-time state.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct OfferEpochFence {
    pub(crate) epoch: EpochFence,
    pub(crate) attempt: OfferAttemptId,
}

impl EpochFence {
    pub(crate) fn with_offer_attempt(self, attempt: OfferAttemptId) -> OfferEpochFence {
        OfferEpochFence {
            epoch: self,
            attempt,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OperationKind {
    Generic,
    StartSession,
    StartShare,
    StopShare,
    UpdateSession,
    StopSession,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct OperationFence {
    pub(crate) operation: OperationId,
    pub(crate) epoch: EpochFence,
    pub(crate) kind: OperationKind,
}

#[derive(Clone, Debug)]
pub(crate) struct FencedPeerSignal {
    pub(crate) fence: EpochFence,
    pub(crate) signal: PeerSignal,
}

/// Events produced by join/signaling workers.  The worker loop is the sole
/// consumer; high-rate media never becomes an event.
#[allow(dead_code)]
pub(crate) enum SessionEvent {
    JoinServiceReady { session: SessionEpoch },
    Admission { session: SessionEpoch, id: String },
    OfferAnswer { fence: EpochFence },
    IceState { fence: EpochFence },
    PeerConnected { fence: EpochFence },
    PeerFailed { fence: EpochFence, detail: String },
    ResourceReady { fence: EpochFence },
    ResourceFailed { fence: EpochFence, detail: String },
    StopComplete { session: SessionEpoch },
}

/// Mutable lifecycle ownership for the serialized media worker.
///
/// `MediaSessionSnapshot` copies observations from this value; it never calls
/// `advance` or consumes an event.
pub(crate) struct SessionActor {
    pub(crate) lifecycle: MediaLifecycleState,
    pub(crate) session_epoch: SessionEpoch,
    pub(crate) share_epoch: ShareEpoch,
    next_session: u64,
    next_share: u64,
    next_link: u64,
    next_offer_attempt: u64,
    next_operation: u64,
    active_links: HashSet<LinkId>,
    active_offer_attempts: HashSet<OfferAttemptId>,
    operations: HashMap<OperationId, OperationFence>,
    discarded_events: u64,
}

impl SessionActor {
    pub(crate) fn new() -> Self {
        Self {
            lifecycle: MediaLifecycleState::Idle,
            session_epoch: SessionEpoch(0),
            share_epoch: ShareEpoch(0),
            next_session: 0,
            next_share: 0,
            next_link: 0,
            next_offer_attempt: 0,
            next_operation: 0,
            active_links: HashSet::new(),
            active_offer_attempts: HashSet::new(),
            operations: HashMap::new(),
            discarded_events: 0,
        }
    }

    pub(crate) fn begin_session(&mut self) -> SessionEpoch {
        self.next_session = self.next_session.saturating_add(1);
        self.session_epoch = SessionEpoch(self.next_session);
        self.share_epoch = ShareEpoch(0);
        self.active_links.clear();
        self.active_offer_attempts.clear();
        self.operations.clear();
        self.lifecycle = MediaLifecycleState::Starting;
        self.session_epoch
    }

    pub(crate) fn invalidate_session(&mut self) -> SessionEpoch {
        self.next_session = self.next_session.saturating_add(1);
        self.session_epoch = SessionEpoch(self.next_session);
        self.active_links.clear();
        self.active_offer_attempts.clear();
        self.operations.clear();
        self.lifecycle = MediaLifecycleState::Stopping;
        self.session_epoch
    }

    pub(crate) fn begin_share(&mut self) -> ShareEpoch {
        self.next_share = self.next_share.saturating_add(1);
        self.share_epoch = ShareEpoch(self.next_share);
        self.active_offer_attempts.clear();
        self.share_epoch
    }

    pub(crate) fn end_share(&mut self) {
        // Stopping a Share invalidates links and offers, but does not create a
        // new Share epoch.  The next real start/replacement reserves it.
        self.active_links.clear();
        self.active_offer_attempts.clear();
    }

    pub(crate) fn begin_link(&mut self) -> LinkId {
        self.next_link = self.next_link.saturating_add(1);
        let id = LinkId(self.next_link);
        self.active_links.insert(id);
        id
    }

    /// Allocates a unique identity for one exact offer attempt on `link`.
    /// The returned fence is bound to the current Session/Share epochs.
    pub(crate) fn begin_offer_attempt(&mut self, link: LinkId) -> Option<OfferEpochFence> {
        if !self.active_links.contains(&link) {
            return None;
        }
        self.next_offer_attempt = self.next_offer_attempt.checked_add(1)?;
        let attempt = OfferAttemptId(self.next_offer_attempt);
        self.active_offer_attempts.insert(attempt);
        Some(self.fence(Some(link)).with_offer_attempt(attempt))
    }

    pub(crate) fn fence(&self, link: Option<LinkId>) -> EpochFence {
        EpochFence {
            session: self.session_epoch,
            share: self.share_epoch,
            link,
        }
    }

    pub(crate) fn reserve_operation(&mut self, epoch: EpochFence) -> OperationFence {
        self.reserve_operation_kind(epoch, OperationKind::Generic)
    }

    pub(crate) fn reserve_operation_kind(
        &mut self,
        epoch: EpochFence,
        kind: OperationKind,
    ) -> OperationFence {
        self.next_operation = self.next_operation.saturating_add(1);
        let fence = OperationFence {
            operation: OperationId(self.next_operation),
            epoch,
            kind,
        };
        self.operations.insert(fence.operation, fence);
        fence
    }

    /// Share activation changes the ShareEpoch before reserving its
    /// operation. This prevents a Start operation from being fenced to the
    /// pre-activation ShareEpoch.
    pub(crate) fn reserve_start_share(&mut self) -> (ShareEpoch, OperationFence) {
        let share = self.begin_share();
        let operation = self.reserve_operation_kind(self.fence(None), OperationKind::StartShare);
        (share, operation)
    }

    pub(crate) fn accepts_operation(&self, fence: OperationFence) -> bool {
        self.operations.get(&fence.operation).copied() == Some(fence) && self.accepts(fence.epoch)
    }

    pub(crate) fn accepts_operation_kind(
        &self,
        fence: OperationFence,
        kind: OperationKind,
    ) -> bool {
        fence.kind == kind && self.accepts_operation(fence)
    }

    pub(crate) fn retire_operation(&mut self, operation: OperationId) {
        self.operations.remove(&operation);
    }

    pub(crate) fn accepts(&self, fence: EpochFence) -> bool {
        self.session_epoch == fence.session
            && self.share_epoch == fence.share
            && fence
                .link
                .map(|link| self.active_links.contains(&link))
                .unwrap_or(true)
    }

    pub(crate) fn accepts_offer(&self, fence: OfferEpochFence) -> bool {
        self.accepts(fence.epoch) && self.active_offer_attempts.contains(&fence.attempt)
    }

    pub(crate) fn retire_offer_attempt(&mut self, attempt: OfferAttemptId) {
        self.active_offer_attempts.remove(&attempt);
    }

    pub(crate) fn retire_link(&mut self, link: LinkId) {
        self.active_links.remove(&link);
    }

    pub(crate) fn discard(&mut self, event: &str, fence: EpochFence) {
        self.discarded_events = self.discarded_events.saturating_add(1);
        let link = fence
            .link
            .map(|link| link.0.to_string())
            .unwrap_or_else(|| "none".into());
        crate::media::logger::log(
            "WARN",
            "epoch discard",
            &format!(
                "event={event} session={} share={} link={link}",
                fence.session.0, fence.share.0
            ),
        );
    }

    #[cfg(test)]
    pub(crate) fn discarded_events(&self) -> u64 {
        self.discarded_events
    }
}

impl Default for SessionActor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_session_and_share_fences_are_discarded() {
        let mut actor = SessionActor::new();
        let first = actor.begin_session();
        let first_share = actor.begin_share();
        let first_fence = EpochFence {
            session: first,
            share: first_share,
            link: None,
        };
        actor.begin_session();
        assert!(!actor.accepts(first_fence));
        actor.discard("resource-ready", first_fence);
        assert_eq!(actor.discarded_events(), 1);
    }

    #[test]
    fn current_fence_is_observationally_accepted() {
        let mut actor = SessionActor::new();
        let session = actor.begin_session();
        let share = actor.begin_share();
        let link = actor.begin_link();
        assert!(actor.accepts(EpochFence {
            session,
            share,
            link: Some(link),
        }));
    }

    #[test]
    fn offer_attempts_are_unique_and_bound_to_their_exact_fence() {
        let mut actor = SessionActor::new();
        actor.begin_session();
        actor.begin_share();
        let link = actor.begin_link();
        let first = actor.begin_offer_attempt(link).expect("active link");
        let second = actor.begin_offer_attempt(link).expect("active link");

        assert_ne!(first.attempt, second.attempt);
        assert_ne!(first.attempt, OfferAttemptId(0));
        assert!(actor.accepts_offer(first));
        assert!(actor.accepts_offer(second));

        actor.retire_offer_attempt(first.attempt);
        assert!(!actor.accepts_offer(first));
        assert!(actor.accepts_offer(second));
    }

    #[test]
    fn offer_completion_is_rejected_after_share_epoch_changes() {
        let mut actor = SessionActor::new();
        actor.begin_session();
        actor.begin_share();
        let link = actor.begin_link();
        let offer = actor.begin_offer_attempt(link).expect("active link");

        actor.begin_share();
        assert!(!actor.accepts_offer(offer));
    }

    #[test]
    fn share_operation_is_reserved_after_share_activation() {
        let mut actor = SessionActor::new();
        actor.begin_session();
        let (share, operation) = actor.reserve_start_share();

        assert_eq!(operation.kind, OperationKind::StartShare);
        assert_eq!(operation.epoch.share, share);
        assert!(actor.accepts_operation(operation));
    }

    #[test]
    fn operation_acceptance_requires_the_declared_kind() {
        let mut actor = SessionActor::new();
        actor.begin_session();
        let epoch = actor.fence(None);
        let operation = actor.reserve_operation_kind(epoch, OperationKind::UpdateSession);

        assert!(actor.accepts_operation_kind(operation, OperationKind::UpdateSession));
        assert!(!actor.accepts_operation_kind(operation, OperationKind::StartShare));
    }

    #[test]
    fn stopping_share_does_not_advance_share_epoch() {
        let mut actor = SessionActor::new();
        actor.begin_session();
        let first = actor.begin_share();
        actor.end_share();
        assert_eq!(actor.share_epoch, first);
        let (replacement, _) = actor.reserve_start_share();
        assert!(replacement.0 > first.0);
    }

    #[test]
    fn durable_fences_round_trip_through_ipc_serialization() {
        let mut actor = SessionActor::new();
        actor.begin_session();
        actor.begin_share();
        let link = actor.begin_link();
        let offer = actor.begin_offer_attempt(link).expect("active link");
        let operation = actor.reserve_operation_kind(offer.epoch, OperationKind::StartShare);

        assert_eq!(
            serde_json::from_value::<OfferEpochFence>(
                serde_json::to_value(offer).expect("serialize offer fence")
            )
            .expect("deserialize offer fence"),
            offer
        );
        assert_eq!(
            serde_json::from_value::<OperationFence>(
                serde_json::to_value(operation).expect("serialize operation fence")
            )
            .expect("deserialize operation fence"),
            operation
        );
    }
}
