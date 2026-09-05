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
/// evidence that a native capture stream has been created. The source is
/// fit inside preserving aspect ratio and never upscaled, so picking above
/// the source size just captures at native size.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum VideoResolution {
    #[serde(rename = "2160p")]
    P2160,
    #[serde(rename = "1440p")]
    P1440,
    #[serde(rename = "1080p")]
    P1080,
    #[serde(rename = "720p")]
    P720,
    #[serde(rename = "480p")]
    P480,
}

impl VideoResolution {
    /// Encode ceiling (width, height) for the variant.
    pub(crate) fn max_dimensions(self) -> (u32, u32) {
        match self {
            Self::P2160 => (3840, 2160),
            Self::P1440 => (2560, 1440),
            Self::P1080 => (1920, 1080),
            Self::P720 => (1280, 720),
            Self::P480 => (854, 480),
        }
    }
}

pub(crate) fn fitted_even_size(
    source_width: u32,
    source_height: u32,
    max_width: u32,
    max_height: u32,
) -> (u32, u32) {
    let source_width = source_width.max(1);
    let source_height = source_height.max(1);
    let max_width = max_width.max(2);
    let max_height = max_height.max(2);
    // Contained sources stay native (no upscale, even dimensions).
    if source_width <= max_width && source_height <= max_height {
        return ((source_width & !1).max(2), (source_height & !1).max(2));
    }
    // Pixel-budget fit: use the area of the cap, not a 16:9 letterbox.
    // A 3440x1440 ultrawide at a 1080p cap stays ~21:9 and wider than 1920.
    let cap_pixels = (max_width as u64).saturating_mul(max_height as u64).max(1);
    let src_pixels = (source_width as u64)
        .saturating_mul(source_height as u64)
        .max(1);
    let scale = ((cap_pixels as f64) / (src_pixels as f64)).sqrt().min(1.0);
    let mut width = ((source_width as f64 * scale).round() as u32).max(2) & !1;
    let mut height = ((source_height as f64 * scale).round() as u32).max(2) & !1;
    while (width as u64).saturating_mul(height as u64) > cap_pixels && (width > 2 || height > 2) {
        if width >= height && width > 2 {
            width -= 2;
        } else if height > 2 {
            height -= 2;
        } else {
            break;
        }
    }
    (width.max(2), height.max(2))
}

/// Single home for capture and encoder sizing: pixel-budget fit inside the
/// session ceiling, Baseline sessions additionally capped to 1920 wide
/// (wider Baseline black-screens some decoders), then macroblock-aligned
/// down (Media Foundation MFTs reject non-mod16 input such as 2714x764).
/// Idempotent: feeding an already-final size back returns it unchanged.
pub(crate) fn final_encode_size(
    src_width: u32,
    src_height: u32,
    max_width: u32,
    max_height: u32,
    baseline: bool,
) -> (u32, u32) {
    const MAX_BASELINE_WIDTH: u32 = 1920;
    const MACROBLOCK: u32 = 16;
    let (mut width, mut height) = fitted_even_size(src_width, src_height, max_width, max_height);
    if baseline && width > MAX_BASELINE_WIDTH {
        height = ((height as u64 * MAX_BASELINE_WIDTH as u64 / width as u64) as u32 & !1).max(2);
        width = MAX_BASELINE_WIDTH;
    }
    (
        (width & !(MACROBLOCK - 1)).max(MACROBLOCK),
        (height & !(MACROBLOCK - 1)).max(MACROBLOCK),
    )
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum FrameRate {
    #[serde(rename = "120_fps")]
    Fps120,
    #[serde(rename = "60_fps")]
    Fps60,
    #[serde(rename = "30_fps")]
    Fps30,
}

impl FrameRate {
    pub(crate) fn hertz(self) -> u32 {
        match self {
            Self::Fps120 => 120,
            Self::Fps60 => 60,
            Self::Fps30 => 30,
        }
    }
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

/// Bounds for a user-supplied encoder bitrate override (bps).
/// The floor matches the congestion-control floor so a custom value can
/// never starve the encoder below what REMB may already apply.
pub(crate) const MIN_BITRATE_BPS: u32 = 250_000;
pub(crate) const MAX_BITRATE_BPS: u32 = 100_000_000;

pub(crate) fn clamp_bitrate_bps(bps: u32) -> u32 {
    bps.clamp(MIN_BITRATE_BPS, MAX_BITRATE_BPS)
}

/// Effective encoder target: an explicit override wins over the preset.
pub(crate) fn resolve_bitrate(quality: TransmissionQuality, override_bps: Option<u32>) -> u32 {
    override_bps
        .filter(|bps| *bps > 0)
        .map(clamp_bitrate_bps)
        .unwrap_or_else(|| quality.bitrate())
}

/// Default congestion floor as a fraction of the target, not a flat 1 Mbps.
/// High (8 Mbps) → 2 Mbps; Low (1.5 Mbps) → 375 kbps. A hunting REMB
/// estimator can no longer pin every preset to the same megabit.
pub(crate) fn default_floor_bps(target_bps: u32) -> u32 {
    let target = target_bps.max(MIN_BITRATE_BPS);
    (target / 4).max(MIN_BITRATE_BPS).min(target)
}

/// Effective congestion floor, always within [250 kbps, target].
pub(crate) fn resolve_floor(target_bps: u32, floor_override_bps: Option<u32>) -> u32 {
    let target = target_bps.max(MIN_BITRATE_BPS);
    let floor = floor_override_bps
        .filter(|bps| *bps > 0)
        .map(clamp_bitrate_bps)
        .unwrap_or_else(|| default_floor_bps(target));
    floor.min(target).max(MIN_BITRATE_BPS)
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

fn default_true() -> bool {
    true
}

/// Session video codec. Presets always send H.264 Baseline so mixed
/// Mac/Windows (GTX 1050 through 40-series, M1+) can decode. HEVC and AV1
/// are Customize-only: they need a capable Host encoder and a Viewer that
/// can decode them. Fixed at session start.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoCodec {
    #[default]
    H264,
    H264High,
    Hevc,
    Av1,
}

impl VideoCodec {
    pub(crate) fn mime_type(self) -> &'static str {
        match self {
            Self::H264 | Self::H264High => "video/H264",
            Self::Hevc => "video/H265",
            Self::Av1 => "video/AV1",
        }
    }

    /// SDP profile-level-id for the H.264 variants (None = not H.264).
    pub(crate) fn h264_profile_level_id(self) -> Option<&'static str> {
        match self {
            Self::H264 => Some("42e02a"),
            Self::H264High => Some("640033"),
            Self::Hevc | Self::Av1 => None,
        }
    }
}

/// Windows encoder backend. Auto tries the Media Foundation hardware
/// encoder (NVENC silicon on NVIDIA) and falls back to OpenH264 software;
/// the choice is fixed at Start like the codec.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoEncoder {
    #[default]
    Auto,
    Software,
    Hardware,
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
    /// Optional encoder bitrate override in bps. When set (> 0), it wins
    /// over the quality preset for the encoder target (resolution/fps still
    /// follow the preset). Clamped to 250 kbps – 100 Mbps.
    #[serde(default)]
    pub bitrate_bps: Option<u32>,
    /// Optional congestion floor in bps: REMB is never followed below this
    /// (None = 1 Mbps auto, always capped by the target). Raises the worst
    /// case on hunting estimators without disabling adaptation.
    #[serde(default)]
    pub min_bitrate_bps: Option<u32>,
    /// Session video codec (default H.264). HEVC requires macOS.
    #[serde(default)]
    pub codec: VideoCodec,
    /// Windows encoder backend (default Auto = hardware when available).
    /// Ignored on macOS, which always uses VideoToolbox.
    #[serde(default)]
    pub encoder: VideoEncoder,
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
    /// Broadcast (1 Host) or Sala (everyone may share). Default Broadcast.
    #[serde(default)]
    pub session_mode: super::room_mode::SessionMode,
    /// Capture + encode without opening a new Rendezvous/LAN room. Used
    /// when a Sala member starts their own Share slot.
    #[serde(default)]
    pub attach_only: bool,
    /// When false, open the Session (code, roster, signaling) without
    /// capturing. Sala Hosts start this way; they share later if they want.
    /// Default true so Broadcast keeps capturing on Start.
    #[serde(default = "default_true")]
    pub share_on_start: bool,
    /// Rendezvous base URL, only used by Stunar (not in this fatia).
    #[serde(default)]
    pub rendezvous_url: Option<String>,
}

impl CreateMediaSessionRequest {
    /// Capture cap (max dimensions) for the session. The explicit
    /// `resolution` wins; the UI resolves "auto" to the quality preset
    /// before sending, so callers that only set quality must also send
    /// the preset-mapped resolution.
    pub(crate) fn capture_cap(&self) -> (u32, u32) {
        self.resolution.max_dimensions()
    }

    /// Effective frame rate for the session. The explicit `frame_rate`
    /// wins (same auto-resolution rule as `capture_cap`).
    pub(crate) fn effective_frame_rate(&self) -> FrameRate {
        self.frame_rate
    }
}

/// Live settings update for an active session. Applied without tearing down
/// the room or the WebRTC peer. Resolution/frame rate restart capture and
/// recreate the encoder (brief freeze, no rejoin); None keeps the Start value.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UpdateMediaSessionRequest {
    pub quality: TransmissionQuality,
    /// Live encoder bitrate override in bps (None = follow the preset).
    #[serde(default)]
    pub bitrate_bps: Option<u32>,
    /// Live congestion floor in bps (None = 1 Mbps auto).
    #[serde(default)]
    pub min_bitrate_bps: Option<u32>,
    /// Live capture cap (None = keep). Restarts capture + encoder.
    #[serde(default)]
    pub resolution: Option<VideoResolution>,
    /// Live frame rate (None = keep). Restarts capture + encoder.
    #[serde(default)]
    pub frame_rate: Option<FrameRate>,
    /// Carried for shape compatibility; the session codec is fixed at Start
    /// and changes here are ignored (selector is disabled while active).
    #[serde(default)]
    pub codec: VideoCodec,
    /// Same deal as codec: swapping the encoder live would drop the stream,
    /// so updates carry it but the running session keeps the Start choice.
    #[serde(default)]
    pub encoder: VideoEncoder,
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

/// Session-wide encoder + per-viewer diagnostics for the Host popup.
#[derive(Clone, Debug, Serialize)]
pub struct MediaSessionStats {
    pub links: Vec<ViewerLinkStats>,
    /// Encoder target (preset or user override) in bps.
    pub target_bps: u32,
    /// Last congestion-imposed bitrate in bps (None = no REMB signal yet).
    pub congestion_bps: Option<u32>,
    /// Current congestion floor in bps.
    pub floor_bps: u32,
}

/// Per-viewer link diagnostics for the Host status popup (RTT in ms).
#[derive(Clone, Debug, Serialize)]
pub struct ViewerLinkStats {
    pub id: String,
    pub nickname: String,
    pub state: PeerTransportState,
    /// Last measured round-trip time in ms. None = not measured yet.
    pub rtt_ms: Option<f64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RosterEntry {
    pub id: String,
    pub nickname: String,
    pub state: PeerTransportState,
    #[serde(default)]
    pub master: bool,
    #[serde(default)]
    pub share: bool,
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
    /// Effective encoder bitrate target in bps (preset or user override).
    /// 0 when no session is active.
    pub bitrate_bps: u32,
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
    /// This member's id on the Rendezvous (Sala) or None for Broadcast Host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_id: Option<String>,
    /// True when the Session has a Password. The Password itself never leaves
    /// the native side.
    pub password_set: bool,
    /// Admission rule of the Session.
    pub admission: bool,
    /// Join mode of the Session.
    pub join_mode: JoinMode,
    pub session_mode: super::room_mode::SessionMode,
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
            bitrate_bps: 0,
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
            self_id: None,
            password_set: false,
            admission: false,
            join_mode: JoinMode::Lan,
            session_mode: super::room_mode::SessionMode::Broadcast,
            direct_listen_port: None,
            direct_addresses: Vec::new(),
            direct_mapping: false,
            stunar_state: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        fitted_even_size, resolve_floor, TransmissionQuality, VideoCodec, MIN_BITRATE_BPS,
    };

    fn aspect(width: u32, height: u32) -> f64 {
        width as f64 / height as f64
    }

    #[test]
    fn fitted_size_preserves_aspect_instead_of_forcing_1080p() {
        let (width, height) = fitted_even_size(3024, 1964, 1920, 1080);
        assert_eq!(width % 2, 0);
        assert_eq!(height % 2, 0);
        assert!(width * height <= 1920 * 1080);
        assert!((aspect(width, height) - aspect(3024, 1964)).abs() < 0.02);
        assert_eq!(fitted_even_size(1280, 720, 1920, 1080), (1280, 720));
        assert_eq!(fitted_even_size(1920, 1080, 1920, 1080), (1920, 1080));
    }

    #[test]
    fn fitted_size_uses_pixel_budget_on_ultrawide_not_a_16_by_9_box() {
        // Letterbox into 1920x1080 would be 1920x804 and waste ~25% of the
        // 1080p budget. Pixel-budget fit keeps the 21:9 picture larger.
        let (width, height) = fitted_even_size(3440, 1440, 1920, 1080);
        assert_eq!(width % 2, 0);
        assert_eq!(height % 2, 0);
        assert!(width * height <= 1920 * 1080);
        assert!(
            width > 1920,
            "ultrawide may be wider than 1920, got {width}x{height}"
        );
        assert!(
            height > 804,
            "must beat the old letterbox height 804, got {height}"
        );
        assert!((aspect(width, height) - aspect(3440, 1440)).abs() < 0.02);

        let (uwqhd_w, uwqhd_h) = fitted_even_size(5120, 1440, 1920, 1080);
        assert!(uwqhd_w * uwqhd_h <= 1920 * 1080);
        assert!(uwqhd_w > 1920);
        assert!((aspect(uwqhd_w, uwqhd_h) - aspect(5120, 1440)).abs() < 0.03);

        let (qhd_w, qhd_h) = fitted_even_size(2560, 1440, 1920, 1080);
        assert_eq!((qhd_w, qhd_h), (1920, 1080));
    }

    #[test]
    fn fitted_size_never_upscales() {
        assert_eq!(fitted_even_size(800, 600, 1920, 1080), (800, 600));
    }

    #[test]
    fn final_size_caps_baseline_and_aligns_macroblock() {
        use super::final_encode_size;
        // O caso do incidente: ultrawide Baseline vira 1920 de largura e
        // altura multipla de 16 (MFTs rejeitam 2714x764).
        assert_eq!(final_encode_size(5120, 1440, 1920, 1080, true), (1920, 528));
        assert_eq!(final_encode_size(3620, 1018, 1920, 1080, true), (1920, 528));
        // High mantem o budget fit, so alinhado.
        let (w, h) = final_encode_size(5120, 1440, 1920, 1080, false);
        assert!(w * h <= 1920 * 1080);
        assert_eq!((w % 16, h % 16), (0, 0));
        // Tamanhos comuns passam quase intactos (so alinhamento).
        assert_eq!(
            final_encode_size(1920, 1080, 1920, 1080, true),
            (1920, 1072)
        );
        assert_eq!(final_encode_size(1280, 720, 1920, 1080, true), (1280, 720));
        // Idempotente: reaplicar nao muda nada.
        let once = final_encode_size(5120, 1440, 1920, 1080, true);
        assert_eq!(final_encode_size(once.0, once.1, 1920, 1080, true), once);
        assert_eq!(fitted_even_size(854, 480, 1280, 720), (854, 480));
    }

    #[test]
    fn auto_floor_is_a_quarter_of_the_target_not_a_flat_megabit() {
        assert_eq!(resolve_floor(8_000_000, None), 2_000_000);
        assert_eq!(resolve_floor(4_000_000, None), 1_000_000);
        assert_eq!(resolve_floor(1_500_000, None), 375_000);
        assert_eq!(resolve_floor(800_000, None), MIN_BITRATE_BPS);
        assert_eq!(resolve_floor(8_000_000, Some(3_000_000)), 3_000_000);
        assert_eq!(resolve_floor(8_000_000, Some(20_000_000)), 8_000_000);
    }

    #[test]
    fn quality_presets_stay_on_universal_h264() {
        assert_eq!(VideoCodec::default(), VideoCodec::H264);
        assert_eq!(TransmissionQuality::High.bitrate(), 8_000_000);
        assert_eq!(TransmissionQuality::High.max_dimensions(), (1920, 1080));
        assert_eq!(TransmissionQuality::Low.max_dimensions(), (1280, 720));
    }
}
