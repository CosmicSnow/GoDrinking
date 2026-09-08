//! GoLive Sala core: serialized lifecycle, native WebRTC media, signaling.
//!
//! No UI, no Tauri, no GStreamer. The [`owner`] serializes every command
//! under one lock (immutable snapshots, fenced completions, idempotent
//! stop); [`media`] owns OpenH264 encode/decode and `webrtc`-crate
//! PeerConnections (encode once, fanout per watcher); [`signal`] speaks
//! REST + WebSocket to `server/` and never sends media.
//!
//! The core emits snapshots and events for future UI layers to poll or
//! subscribe to — never the reverse.

pub mod ids;
pub mod media;
pub mod owner;
pub mod signal;
pub mod state;
// Private: hardware backend surface is `media::{EngineKind, VideoEncoder}`;
// objc2 types never leak past `vt.rs`.
mod vt;
