//! Native media control plane.
//!
//! The WebView currently owns display capture and WebRTC. This module is the
//! native performance foundation for the eventual platform adapters: it owns
//! session lifecycle, capability discovery, and bounded control/frame queues.
//! macOS capture is provided by the ScreenCaptureKit adapter; VideoToolbox
//! produces native H.264 access units consumed by the local-only WebRTC peer.

mod access_unit;
mod benchmark;
mod capabilities;
mod engine;
mod fanout;
pub(crate) mod logger;
mod peer_transport;
mod pipeline;
mod process_tap;
mod rendezvous;
#[cfg(test)]
mod stunar_integration_test;
mod room;
mod room_mode;
mod session_gate;
mod screen_capture_kit;
mod timestamp;
mod transport;
mod types;
#[cfg(target_os = "macos")]
mod video_toolbox;
#[cfg(target_os = "windows")]
mod windows_capture;
#[cfg(target_os = "windows")]
mod windows_encoder;
#[cfg(target_os = "windows")]
mod mf_encoder;
#[cfg(target_os = "windows")]
pub(crate) mod firewall;
#[cfg(not(target_os = "windows"))]
pub(crate) mod firewall;

pub use benchmark::{run_local_probe, ProbeReport, RecommendedPreset};
pub use capabilities::MediaCapabilities;
pub use engine::MediaEngine;
pub use room_mode::SessionMode;
pub use logger::{clear, read_sessions, LogSession};
#[allow(unused_imports)]
pub use peer_transport::{PeerSignal, PeerSignalKind};
pub use room::{discover_direct, discover_room, submit_answer as submit_room_answer};
#[allow(unused_imports)]
pub use types::{
    CreateMediaSessionRequest, DirectAddress, JoinMode, MediaSessionSnapshot, NativeCaptureSource,
    NativeRunningApp, PreviewFrameEvent, StunarState, UpdateCredentialsRequest,
    UpdateMediaSessionRequest, MediaSessionStats, VideoCodec, VideoEncoder, ViewerLinkStats,
};
