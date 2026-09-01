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
