//! Broadcast vs Sala, Master succession.

use serde::{Deserialize, Serialize};

/// How a Session treats members. Broadcast is 1 Host, N Viewers (default).
/// Sala lets every member capture (Share slot) and watch the others.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    #[default]
    Broadcast,
    Room,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoomMember {
    pub id: String,
    pub nickname: String,
    pub joined_at: u64,
}

/// Broadcast still mints an offer to every accepted Viewer.
/// Sala waits for an explicit `watch` from the member who wants the stream.
pub fn auto_mint_on_accept(mode: SessionMode) -> bool {
    matches!(mode, SessionMode::Broadcast)
}

/// Viewer handshake events from the Rendezvous WS.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HandshakeEvent {
    Accepted,
    Offer,
    Rejected,
    Gone,
}

/// What the Viewer handshake should do with a WS event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HandshakeOutcome {
    KeepWaiting,
    ReadyWithoutOffer,
    ReadyWithOffer,
    Rejected,
    Gone,
}

/// Sala join must succeed even if nobody is sharing yet. Broadcast still
/// waits for the Host offer.
pub fn handshake_outcome(mode: &str, event: HandshakeEvent) -> HandshakeOutcome {
    match event {
        HandshakeEvent::Rejected => HandshakeOutcome::Rejected,
        HandshakeEvent::Gone => HandshakeOutcome::Gone,
        HandshakeEvent::Offer if mode == "room" => HandshakeOutcome::KeepWaiting,
        HandshakeEvent::Offer => HandshakeOutcome::ReadyWithOffer,
        HandshakeEvent::Accepted if mode == "room" => HandshakeOutcome::ReadyWithoutOffer,
        HandshakeEvent::Accepted => HandshakeOutcome::KeepWaiting,
    }
}

/// Who gets the crown after `leaving_id` walks. Oldest remaining `joined_at`,
/// then id lexicographic. `None` means the Sala is empty and must close.
pub fn next_master(members: &[RoomMember], leaving_id: &str) -> Option<String> {
    let mut rest: Vec<&RoomMember> = members
        .iter()
        .filter(|member| member.id != leaving_id)
        .collect();
    rest.sort_by(|left, right| {
        left.joined_at
            .cmp(&right.joined_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    rest.first().map(|member| member.id.clone())
}

#[cfg(test)]
mod tests {
    use super::{next_master, RoomMember};

    fn member(id: &str, joined_at: u64) -> RoomMember {
        RoomMember {
            id: id.into(),
            nickname: id.into(),
            joined_at,
        }
    }

    #[test]
    fn crown_goes_to_the_next_person_who_joined() {
        let members = vec![member("a", 1), member("b", 2), member("c", 3)];
        assert_eq!(next_master(&members, "a").as_deref(), Some("b"));
        assert_eq!(next_master(&members, "b").as_deref(), Some("a"));
    }

    #[test]
    fn last_member_leaving_closes_the_room() {
        let members = vec![member("solo", 1)];
        assert_eq!(next_master(&members, "solo"), None);
        assert_eq!(next_master(&[], "ghost"), None);
    }

    #[test]
    fn tied_join_time_breaks_on_id() {
        let members = vec![member("zeta", 5), member("alpha", 5), member("mu", 5)];
        assert_eq!(next_master(&members, "zeta").as_deref(), Some("alpha"));
    }

    #[test]
    fn sala_does_not_auto_mint_to_everyone() {
        assert!(!super::auto_mint_on_accept(super::SessionMode::Room));
        assert!(super::auto_mint_on_accept(super::SessionMode::Broadcast));
    }

    #[test]
    fn sala_join_is_ready_after_accepted_without_an_offer() {
        use super::{handshake_outcome, HandshakeEvent, HandshakeOutcome};
        assert_eq!(
            handshake_outcome("room", HandshakeEvent::Accepted),
            HandshakeOutcome::ReadyWithoutOffer
        );
        assert_eq!(
            handshake_outcome("broadcast", HandshakeEvent::Accepted),
            HandshakeOutcome::KeepWaiting
        );
        assert_eq!(
            handshake_outcome("broadcast", HandshakeEvent::Offer),
            HandshakeOutcome::ReadyWithOffer
        );
        assert_eq!(
            handshake_outcome("room", HandshakeEvent::Offer),
            HandshakeOutcome::KeepWaiting
        );
    }
}
