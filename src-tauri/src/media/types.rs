use serde::{Deserialize, Serialize};

/// The source selected by the future native capture adapter.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureSource {
    Screen,
    Window,
    Game,
}

/// Native capture target dimensions. These are configuration values, not
/// evidence that a native capture stream has been created.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum VideoResolution {
    #[serde(rename = "1080p")]
    P1080,
    #[serde(rename = "720p")]
    P720,
}

pub(crate) fn fitted_even_size(
    source_width: u32,
    source_height: u32,
    max_width: u32,
    max_height: u32,
) -> (u32, u32) {
    let source_width = source_width.max(1);
    let source_height = source_height.max(1);
    let scale = (max_width as f64 / source_width as f64)
        .min(max_height as f64 / source_height as f64)
        .min(1.0);
    let width = ((source_width as f64 * scale).round() as u32)
        .max(2)
        .min(max_width)
        & !1;
    let height = ((source_height as f64 * scale).round() as u32)
        .max(2)
        .min(max_height)
        & !1;
    (width.max(2), height.max(2))
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum FrameRate {
    #[serde(rename = "60_fps")]
    Fps60,
    #[serde(rename = "30_fps")]
    Fps30,
}

/// Transmission quality presets. When present, the preset wins over
/// `resolution`/`frame_rate` for the capture cap, fps, and encoder bitrate.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransmissionQuality {
    Low,
    Medium,
    High,
}

impl Default for TransmissionQuality {
    fn default() -> Self {
        Self::High
    }
}

impl TransmissionQuality {
    /// Maximum capture dimensions for the preset. The encoder still follows
    /// the actual CVPixelBuffer size; this is only the cap used to fit the
    /// source before capture.
    pub(crate) fn max_dimensions(self) -> (u32, u32) {
        match self {
            Self::Low => (1280, 720),
            Self::Medium => (1920, 1080),
            Self::High => (1920, 1080),
        }
    }

    pub(crate) fn frame_rate(self) -> FrameRate {
        match self {
            Self::Low => FrameRate::Fps30,
            Self::Medium => FrameRate::Fps30,
            Self::High => FrameRate::Fps60,
        }
    }

    /// Target average bitrate for the preset.
    pub(crate) fn bitrate(self) -> u32 {
        match self {
            Self::Low => 1_500_000,
            Self::Medium => 4_000_000,
            Self::High => 8_000_000,
        }
    }
}

fn default_host_nickname() -> String {
    "Host".into()
}

/// How a Viewer finds the Host. LAN and Direct are implemented; Stunar needs
/// the Rendezvous (Fatia 4/5).
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JoinMode {
    #[default]
    Lan,
    Direct,
    Stunar,
}

/// Health of the Host's connection to the Rendezvous (Stunar mode only).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StunarState {
    /// Opening the room / connecting the WS inbox.
    Calling,
    /// WS inbox connected and heartbeats succeeding.
    Live,
    /// Network failure; the room will expire if it lasts.
    Unreachable,
}

/// Control input for a native capture session.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CreateMediaSessionRequest {
    pub source: CaptureSource,
    #[serde(default)]
    pub source_id: Option<u64>,
    pub resolution: VideoResolution,
    pub frame_rate: FrameRate,
    pub system_audio: bool,
    #[serde(default)]
    pub excluded_apps: Vec<String>,
    #[serde(default)]
    pub quality: TransmissionQuality,
    /// Password for the Session. Empty means no Password on LAN/Direct;
    /// Stunar rooms require one (4-64 chars) and the server rejects open
    /// without it.
    #[serde(default)]
    pub password: String,
    /// Host Nickname shown in the Roster context. Not an account.
    #[serde(default = "default_host_nickname")]
    pub nickname: String,
    /// Admission rule: when true, the Host must accept each Viewer before
    /// signaling starts.
    #[serde(default)]
    pub admission: bool,
    /// How Viewers find this Session.
    #[serde(default)]
    pub join_mode: JoinMode,
    /// Rendezvous base URL, only used by Stunar (not in this fatia).
    #[serde(default)]
    pub rendezvous_url: Option<String>,
}

impl CreateMediaSessionRequest {
    /// Capture cap (max dimensions) for the session. The quality preset wins
    /// over `resolution` when present.
    pub(crate) fn capture_cap(&self) -> (u32, u32) {
        self.quality.max_dimensions()
    }

    /// Effective frame rate for the session. The quality preset wins over
    /// `frame_rate` when present.
    pub(crate) fn effective_frame_rate(&self) -> FrameRate {
        self.quality.frame_rate()
    }
}

/// Live settings update for an active session. Applied without tearing down
/// capture, the room, or the WebRTC peer.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UpdateMediaSessionRequest {
    pub quality: TransmissionQuality,
    pub system_audio: bool,
    pub excluded_apps: Vec<String>,
}

/// Live credential rotation for an active Session (PRD-18). Connected
/// Viewers are never dropped; only new requests use the new values.
/// `None` keeps the current value; `Some("")` removes the Password
/// (LAN/Direct only — Stunar rooms always have one).
/// The Room code is server-owned (Stunar) or fixed for the Session
/// (LAN/Direct): it never rotates.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct UpdateCredentialsRequest {
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub admission: Option<bool>,
}

/// Metadata only; no native source handles cross the command boundary.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeSourceKind {
    Display,
    Window,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct NativeRunningApp {
    pub name: String,
    pub bundle_id: Option<String>,
    pub pid: i32,
    /// True when Core Audio reports the app's process object running output
    /// (`kAudioProcessPropertyIsRunningOutput`). Best-effort; false when the
    /// property cannot be read.
    #[serde(default)]
    pub emitting_audio: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct NativeCaptureSource {
    pub id: u64,
    pub kind: NativeSourceKind,
    pub title: Option<String>,
    pub application_name: Option<String>,
    pub width: Option<u64>,
    pub height: Option<u64>,
}

/// A bounded, derived thumbnail. This is never the source pixel buffer.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PreviewFrameEvent {
    pub sequence: u64,
    pub timestamp_micros: u64,
    pub width: u32,
    pub height: u32,
    pub encoding: String,
    pub payload: Vec<u8>,
}

/// Explicit lifecycle states for the native media control plane.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaLifecycleState {
    Idle,
    Starting,
    Running,
    Stopping,
    CleanupPending,
    #[allow(dead_code)]
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerTransportState {
    Disabled,
    Starting,
    New,
    Connecting,
    Connected,
    Disconnected,
    Failed,
    Closed,
    /// Waiting for the Host's Admission decision. Only used for Roster
    /// entries; a real PeerTransport never reports this state.
    Pending,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RosterEntry {
    pub id: String,
    pub nickname: String,
    pub state: PeerTransportState,
}

/// One copyable address the Host can share for Direct joins.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DirectAddress {
    pub ip: String,
    pub port: u16,
    pub version: u8,
    /// `"lan"` | `"public"` | `"ipv6"`.
    pub kind: String,
    /// The exact string to copy: `ip:port` or `[ipv6]:port`.
    pub copy: String,
}

/// Safe, small state returned to the WebView. Frame buffers and pipeline
/// handles never implement Serialize and therefore cannot cross Tauri IPC.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MediaSessionSnapshot {
    pub state: MediaLifecycleState,
    pub session_id: Option<String>,
    pub source: Option<CaptureSource>,
    pub source_id: Option<u64>,
    pub resolution: Option<VideoResolution>,
    pub frame_rate: Option<FrameRate>,
    pub system_audio: bool,
    pub excluded_apps: Vec<String>,
    pub native_capture_active: bool,
    pub preview_callback_count: u64,
    pub preview_frame_count: u64,
    pub preview_dropped_count: u64,
    pub preview_error: Option<String>,
    pub detail: String,
    pub peer_state: PeerTransportState,
    pub peer_detail: String,
    pub session_code: Option<String>,
    pub lan_addresses: Vec<String>,
    pub lan_port: Option<u16>,
    pub roster: Vec<RosterEntry>,
    /// True when the Session has a Password. The Password itself never leaves
    /// the native side.
    pub password_set: bool,
    /// Admission rule of the Session.
    pub admission: bool,
    /// Join mode of the Session.
    pub join_mode: JoinMode,
    /// Direct mode: the TCP Signaling port the Host listens on.
    pub direct_listen_port: Option<u16>,
    /// Direct mode: copyable addresses (lan/public/ipv6) for the Host to share.
    pub direct_addresses: Vec<DirectAddress>,
    /// Direct mode: true when a NAT port mapping was created. Always false in
    /// this fatia (UPnP/NAT-PMP/PCP are stubbed).
    pub direct_mapping: bool,
    /// Stunar mode: health of the Host's Rendezvous connection. None for
    /// LAN/Direct sessions.
    pub stunar_state: Option<StunarState>,
}

impl MediaSessionSnapshot {
    pub fn idle(detail: impl Into<String>) -> Self {
        Self {
            state: MediaLifecycleState::Idle,
            session_id: None,
            source: None,
            source_id: None,
            resolution: None,
            frame_rate: None,
            system_audio: false,
            excluded_apps: Vec::new(),
            native_capture_active: false,
            preview_callback_count: 0,
            preview_frame_count: 0,
            preview_dropped_count: 0,
            preview_error: None,
            detail: detail.into(),
            peer_state: PeerTransportState::Disabled,
            peer_detail: "Peer transport is unavailable.".into(),
            session_code: None,
            lan_addresses: Vec::new(),
            lan_port: None,
            roster: Vec::new(),
            password_set: false,
            admission: false,
            join_mode: JoinMode::Lan,
            direct_listen_port: None,
            direct_addresses: Vec::new(),
            direct_mapping: false,
            stunar_state: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fitted_even_size;

    #[test]
    fn fitted_size_preserves_aspect_instead_of_forcing_1080p() {
        assert_eq!(fitted_even_size(3024, 1964, 1920, 1080), (1662, 1080));
        assert_eq!(fitted_even_size(1280, 720, 1920, 1080), (1280, 720));
        assert_eq!(fitted_even_size(1920, 1080, 1920, 1080), (1920, 1080));
    }
}
