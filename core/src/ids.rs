//! Opaque 128-bit identifiers for sessions, shares, links and attempts.
//!
//! Every ID is a random 128-bit value. On the wire they travel as decimal
//! strings (never JSON numbers: anything above 2^53 would lose precision in
//! a JavaScript parser and corrupt fencing). In logs and snapshots only the
//! redacted [`Display`] form (`sess:1a2b3c4d`) may appear — never the full
//! value, never SDP, tokens or passwords (those never enter this module).

use rand::Rng;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

macro_rules! opaque_id {
    ($name:ident, $prefix:literal) => {
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
        pub struct $name(pub u128);

        impl $name {
            /// Fresh cryptographic-quality random 128-bit ID.
            pub fn generate() -> Self {
                Self(rand::thread_rng().gen::<u128>())
            }

            /// Short redacted fragment for milestones and snapshots.
            pub fn short(&self) -> String {
                format!("{:08x}", (self.0 >> 96) as u32 ^ (self.0 as u32))
            }

            /// Full value for in-memory fencing comparisons only.
            pub fn raw(&self) -> u128 {
                self.0
            }

            /// Monotonic successor within one scope (wrapping).
            ///
            /// The rendezvous rejects an offer whose attempt is numerically
            /// lower than the current one for the same session/share/link key,
            /// so re-offers on the SAME scope must advance, never re-roll:
            /// double rounding is monotonic, hence `+1` can never go stale.
            /// New scopes (new session/share/link) use [`Self::generate`].
            pub fn next(&self) -> Self {
                Self(self.0.wrapping_add(1))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!($prefix, "{}"), self.short())
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.0.to_string())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let raw = String::deserialize(d)?;
                raw.parse::<u128>()
                    .map(Self)
                    .map_err(serde::de::Error::custom)
            }
        }
    };
}

opaque_id!(SessionId, "sess:");
opaque_id!(ShareId, "shr:");
opaque_id!(LinkId, "lnk:");
opaque_id!(AttemptId, "att:");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique() {
        assert_ne!(SessionId::generate(), SessionId::generate());
        assert_ne!(AttemptId::generate(), AttemptId::generate());
    }

    #[test]
    fn display_is_redacted() {
        let id = SessionId(u128::MAX);
        let shown = id.to_string();
        assert!(shown.starts_with("sess:"));
        assert_eq!(shown.len(), "sess:".len() + 8);
        assert!(!shown.contains(&u128::MAX.to_string()));
    }

    #[test]
    fn wire_is_decimal_string_roundtrip() {
        for _ in 0..8 {
            let id = ShareId::generate();
            let json = serde_json::to_string(&id).unwrap();
            // A JSON number would lose precision in JS parsers; must be string.
            assert!(json.starts_with('"'), "wire id must be a string: {json}");
            let back: ShareId = serde_json::from_str(&json).unwrap();
            assert_eq!(id, back);
        }
        assert!(serde_json::from_str::<LinkId>("\"not-a-number\"").is_err());
    }
}
