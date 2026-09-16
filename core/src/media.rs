//! Native media: synthetic/movie sources, OpenH264 encode/decode, WebRTC
//! transport via the `webrtc` crate (0.14). No GStreamer, no manual
//! packetization beyond what the crate already does.
//!
//! Contract: H.264 Constrained Baseline (packetization-mode=1, 42e01f) +
//! Opus 48 kHz modeled (audio track lands later; this step is video-only —
//! the requested E2E asserts video), 720p30/1080p60. No TURN anywhere.
//!
//! Design notes (hard lessons, enforced):
//! - Trickle ICE from day one: candidates + ice-complete flow as envelopes.
//! - mDNS is DISABLED symmetrically on both ends (both are our code), so
//!   host candidates carry literal IPs. A redacted census (type/proto/
//!   family, never addresses) is recorded per side.
//! - RTP clock anchored: every sample carries the same `duration` (1/30 s);
//!   the webrtc track advances timestamps from those DELTAS from its own
//!   random base. No wall-clock-absolute arithmetic anywhere near RTP.
//! - Telemetry is redacted permanently: ICE/PC states, census, first-missing
//!   milestones. Never SDP, candidates, IPs or pixels in logs.
//! - Sessions are single-use with bounded idempotent stop (flag + aborts +
//!   timed `close()`; never a wedged state).
//! - Capture GPU buffers cross as opaque platform handles (types-only
//!   dependency — still zero OS bindings in this crate's own code).

use golive_platform::{EncodedAudioPacket, GpuPixelBuffer};
use crate::trace::{Trace, Stage, Sample as TraceSample};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use interceptor::registry::Registry;
use openh264::decoder::Decoder;
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, Level, Profile,
};
use openh264::formats::{YUVSlices, YUVSource};
use std::borrow::Cow;
use rtcp::packet::Packet as RtcpPacket;
use rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
use rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use tokio::sync::mpsc;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::ice::mdns::MulticastDnsMode;
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::api::media_engine::MIME_TYPE_OPUS;
use webrtc::rtp_transceiver::rtp_codec::{RTCRtpCodecCapability, RTPCodecType};
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::rtp_transceiver::RTCPFeedback;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp::codecs::h264::H264Packet;
use webrtc::rtp::packetizer::Depacketizer;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_remote::TrackRemote;
use webrtc_media::Sample;

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

/// Share quality. 30 fps always; the viewer validates full-res frames.
///
/// Legacy selector kept working: `P720` maps to the MEDIUM profile and
/// `P1080` to HIGH (see [`QualityProfile`]). New code prefers profiles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quality {
    P720,
    P1080,
}

impl Quality {
    pub fn dims(self) -> (usize, usize) {
        match self {
            Quality::P720 => (1280, 720),
            Quality::P1080 => (1920, 1080),
        }
    }

    fn level(self) -> Level {
        match self {
            Quality::P720 => Level::Level_3_1,
            Quality::P1080 => Level::Level_4_0,
        }
    }

    /// Legacy mapping. P720 is byte-identical to the historical defaults
    /// (2 Mbps, 30 fps); P1080 follows HIGH.
    pub fn profile(self) -> QualityProfile {
        match self {
            Quality::P720 => QualityProfile::medium(),
            Quality::P1080 => QualityProfile::high(),
        }
    }
}

/// Validated quality profile: resolution + bitrate + frame rate.
///
/// Preset ladder (documented choices):
/// - LOW `854x480 @ 800 kbps @ 15 fps`: floor for weak machines and
///   congested networks; software 480p15 costs single-digit ms/frame.
/// - MEDIUM `1280x720 @ 2000 kbps @ 30 fps`: the historical defaults,
///   byte-identical to the previous hardcoded contract.
/// - HIGH `1920x1080 @ 10000 kbps @ 60 fps`: screen-share ladder (OBS-like
///   1080p60). Software may not hold 60; Auto picks VideoToolbox on macOS.
///
/// Inclusive max per axis for a quality/encode profile. Ultrawide
/// 5120×1440 fits; backends clamp capture to the same ceiling.
pub const MAX_DIM: u32 = 8192;

/// Validation ranges: dims `2..=MAX_DIM` (any parity in, even out — see
/// [`normalize_dims`]); bitrate `100..=20000` kbps (below 100 the control
/// loop starves at HD sizes); fps `1..=60` (decoder/player sanity plus
/// encoder throughput; above 60 is rejected, never silently clamped).
/// Invalid profiles are `Err` before any encoder exists — never black.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QualityProfile {
    pub w: u32,
    pub h: u32,
    pub bitrate_kbps: u32,
    pub fps: u32,
}

impl QualityProfile {
    pub fn low() -> Self {
        Self { w: 854, h: 480, bitrate_kbps: 800, fps: 15 }
    }

    pub fn medium() -> Self {
        Self { w: 1280, h: 720, bitrate_kbps: 2000, fps: 30 }
    }

    pub fn high() -> Self {
        Self { w: 1920, h: 1080, bitrate_kbps: 10_000, fps: 60 }
    }

    pub fn custom(w: u32, h: u32, bitrate_kbps: u32, fps: u32) -> Result<Self, QualityError> {
        let profile = Self { w, h, bitrate_kbps, fps };
        profile.validate()?;
        Ok(profile)
    }

    pub fn validate(&self) -> Result<(), QualityError> {
        if self.w < 2 || self.w > MAX_DIM || self.h < 2 || self.h > MAX_DIM {
            return Err(QualityError::InvalidDimensions { w: self.w, h: self.h });
        }
        if !(100..=20_000).contains(&self.bitrate_kbps) {
            return Err(QualityError::BitrateOutOfRange(self.bitrate_kbps));
        }
        if !(1..=60).contains(&self.fps) {
            return Err(QualityError::FpsOutOfRange(self.fps));
        }
        Ok(())
    }

    /// Effective encode dims for a source: fit inside the request preserving
    /// aspect (never upscale beyond the source), then floor to even (at most
    /// a 1px crop — what makes odd dims backend-valid). Idempotent.
    pub fn normalized_dims(&self, src_w: u32, src_h: u32) -> (u32, u32) {
        normalize_dims(src_w, src_h, self.w, self.h)
    }

    /// Per-frame pacing interval for this profile.
    pub fn frame_duration(&self) -> Duration {
        Duration::from_micros(1_000_000 / self.fps.max(1) as u64)
    }

    /// Sane keyframe cadence: one IDR every ~2 s of frames.
    pub fn keyframe_interval_frames(&self) -> u32 {
        (2 * self.fps).max(2)
    }
}

/// Fit `(src)` into `(req)` preserving aspect, never upscaling, then floor
/// each axis to even. Pure; see [`QualityProfile::normalized_dims`].
pub fn normalize_dims(src_w: u32, src_h: u32, req_w: u32, req_h: u32) -> (u32, u32) {
    if src_w == 0 || src_h == 0 || req_w == 0 || req_h == 0 {
        return (2, 2);
    }
    let scale = ((req_w as f64 / src_w as f64).min(req_h as f64 / src_h as f64)).min(1.0);
    let w = (((src_w as f64 * scale) as u32).max(2)) & !1;
    let h = (((src_h as f64 * scale) as u32).max(2)) & !1;
    (w.max(2), h.max(2))
}

/// Profile validation failures. Numbers only, no secrets possible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QualityError {
    InvalidDimensions { w: u32, h: u32 },
    BitrateOutOfRange(u32),
    FpsOutOfRange(u32),
}

impl std::fmt::Display for QualityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QualityError::InvalidDimensions { w, h } => {
                write!(f, "dimensions out of range 2..={MAX_DIM}: {w}x{h}")
            }
            QualityError::BitrateOutOfRange(bps) => {
                write!(f, "bitrate out of range 100..=20000 kbps: {bps}")
            }
            QualityError::FpsOutOfRange(fps) => write!(f, "fps out of range 1..=60: {fps}"),
        }
    }
}

impl std::error::Error for QualityError {}

/// Requested encoder backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineKind {
    /// Probe hardware, use it if present, else software (logged).
    Auto,
    /// OpenH264 software, always. Deterministic everywhere.
    Software,
    /// Platform hardware (VideoToolbox / NVENC), hard fail if absent.
    Hardware,
}

/// Rough uplink estimate: bitrate × watchers, saturating. Pure planning
/// helper for UI claim-checks (the transport sends one full copy per
/// watcher — encode once, fanout free does not apply to the wire).
pub fn upload_estimate_bps(bitrate_bps: u32, watchers: usize) -> u64 {
    (bitrate_bps as u64).saturating_mul(watchers as u64)
}

/// Frame sources. Synthetic and movie are self-contained; screen capture
/// arrives injected (see `ExternalSource`): the core imports platform
/// TYPES only (opaque GPU handles) — still zero OS bindings of its own.
/// Frames cross as plain channel data, and tests inject without any OS
/// involved.
pub enum VideoSource {
    /// Gradient + bouncing square + counter bar at 30 fps.
    SyntheticBall,
    /// H.264 Annex-B file: decoded with OpenH264, scaled, re-encoded.
    MovieFile(PathBuf),
    /// Externally produced frames (screen capture via the app bridge).
    External(ExternalSource),
}

/// One injected frame: decoded CPU pixels or a retained GPU buffer for
/// zero-copy submit. The app bridge produces both; the encoder routes.
pub enum ExternalFrame {
    Cpu(I420Frame),
    Gpu(GpuPixelBuffer),
}

/// Injected frame feed. `rx` yields CPU frames (the core scales to the
/// share size) or retained GPU buffers (zero-copy submit when the encoder
/// is hardware at matching dims, one conversion otherwise); `label` is an
/// opaque human tag for logs (never pixels, never tokens).
pub struct ExternalSource {
    pub rx: std::sync::mpsc::Receiver<ExternalFrame>,
    pub label: String,
}

impl VideoSource {
    /// Duplicates restartable sources. External feeds are single-shot by
    /// design (the OS stream belongs to whoever opened it): re-share
    /// requires the owner to re-enumerate and re-open, so this is `None`.
    pub fn try_clone(&self) -> Option<VideoSource> {
        match self {
            Self::SyntheticBall => Some(Self::SyntheticBall),
            Self::MovieFile(path) => Some(Self::MovieFile(path.clone())),
            Self::External(_) => None,
        }
    }
}

/// How long one pacing tick waits for an external frame before repeating
/// the last one (keeps the stream alive through capture hiccups).
const EXT_TICK: Duration = Duration::from_millis(100);

/// Frames per second of the contract.
pub const FPS: u32 = 30;
/// One frame's duration. Constant => RTP timestamps advance by deltas.
pub const FRAME_DURATION: Duration = Duration::from_micros(1_000_000 / FPS as u64);
/// IDR interval (frames). Guarantees a keyframe at least every 2 s.
pub const INTRA_PERIOD: u32 = 60;
/// Encoder target bitrate.
pub const BITRATE_BPS: u32 = 2_000_000;
/// Annex-B start code we emit between NALs.
const START_CODE: [u8; 4] = [0, 0, 0, 1];

#[derive(Clone, Debug)]
pub enum MediaEvent {
    /// Local SDP text ready to become an offer/answer envelope.
    LocalSdp(String),
    /// Trickled local candidate (opaque string for the envelope).
    IceCandidate { candidate: String },
    /// Local gathering finished (send ice-complete).
    IceGatheringComplete,
    /// ICE reached Connected/Completed.
    IceConnected,
    /// ICE reached Failed (terminal). Disconnected/Closed stay Error.
    IceFailed,
    /// A decoded frame arrived with validation results.
    VideoFrame { non_black: bool, motion: bool },
    /// A keyframe (IDR) was decoded.
    Keyframe,
    /// Redacted telemetry snapshot.
    Stats(MediaStats),
    Error(String),
}

/// Redacted per-session telemetry. Counts and enums only.
#[derive(Clone, Debug, Default)]
pub struct MediaStats {
    pub census: CandidateCensus,
    pub ice_connected: bool,
    pub frames_decoded: u64,
    pub keyframes_decoded: u64,
    /// Encode generation (fence token): starts at 0, +1 per applied
    /// reconfiguration. Upper layers discard work from older generations.
    /// Adding a field is reader-compatible (no new event variant needed,
    /// which keeps downstream matches compiling).
    pub generation: u64,
    /// Actual encoder dims behind this Stats (post-fit: hardware step-down
    /// or software fit, never the raw request). Numbers only, redaction-safe.
    /// `0` means unknown (older sender path that never built an encoder).
    pub encode_w: usize,
    pub encode_h: usize,
}

/// Redacted ICE census: kinds and families, never addresses.
#[derive(Clone, Debug, Default)]
pub struct CandidateCensus {
    pub host: u32,
    pub srflx: u32,
    pub other_typ: u32,
    pub udp: u32,
    pub tcp: u32,
    pub ip4: u32,
    pub ip6: u32,
}

impl CandidateCensus {
    fn add(&mut self, candidate: &str) {
        let lower = candidate.to_ascii_lowercase();
        if lower.contains("typ host") {
            self.host += 1;
        } else if lower.contains("typ srflx") {
            self.srflx += 1;
        } else {
            self.other_typ += 1;
        }
        if lower.contains(" udp ") {
            self.udp += 1;
        } else if lower.contains(" tcp ") {
            self.tcp += 1;
        }
        // Family without addresses: the candidate holds exactly one IP token
        // after the port; count dots vs colons on the address-looking token.
        for token in candidate.split_whitespace().skip(4).take(1) {
            if token.contains(':') {
                self.ip6 += 1;
            } else if token.contains('.') {
                self.ip4 += 1;
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MediaError {
    Codec(String),
    Transport(String),
    Source(String),
    /// Explicit hardware was demanded and is absent. Fail-high by design:
    /// callers must fall back (or abort) explicitly — never silent software.
    HwUnavailable(String),
    Closed,
}

impl std::fmt::Display for MediaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MediaError::Codec(detail) => write!(f, "codec: {detail}"),
            MediaError::Transport(detail) => write!(f, "transport: {detail}"),
            MediaError::Source(detail) => write!(f, "source: {detail}"),
            MediaError::HwUnavailable(detail) => write!(f, "hw unavailable: {detail}"),
            MediaError::Closed => write!(f, "session closed"),
        }
    }
}

impl std::error::Error for MediaError {}

// ---------------------------------------------------------------------------
// I420 frames + sources
// ---------------------------------------------------------------------------

/// One planar I420 frame, contiguous Y + U + V.
#[derive(Clone, Debug)]
pub struct I420Frame {
    pub w: usize,
    pub h: usize,
    pub data: Vec<u8>,
}

impl I420Frame {
    fn black(w: usize, h: usize) -> Self {
        let mut data = vec![0u8; w * h * 3 / 2];
        data[..w * h].fill(16);
        data[w * h..].fill(128);
        Self { w, h, data }
    }

    fn yuv_buffer(&self) -> YUVSlices<'_> {
        let pixels = self.w * self.h;
        YUVSlices::new(
            (&self.data[..pixels], &self.data[pixels..pixels + pixels / 4], &self.data[pixels + pixels / 4..]),
            (self.w, self.h), (self.w, self.w / 2, self.w / 2),
        )
    }
}

/// Deterministic synthetic frame: luma gradient + bouncing white square +
/// a bottom counter bar (guarantees motion every frame).
fn synthetic_frame(w: usize, h: usize, n: u64) -> I420Frame {
    let mut frame = I420Frame::black(w, h);
    let y = &mut frame.data[..w * h];
    // Gradient 32..208 across the width.
    for row in 0..h {
        for col in 0..w {
            y[row * w + col] = (32 + (176 * col / w.max(1)) as u8).min(208);
        }
    }
    // Bouncing 96px square.
    let size = 96.min(w / 4).min(h / 4).max(16);
    let x = ((n * 8) % (w - size).max(1) as u64) as usize;
    let yy = ((n * 5) % (h - size).max(1) as u64) as usize;
    for row in yy..(yy + size).min(h) {
        for col in x..(x + size).min(w) {
            y[row * w + col] = 235;
        }
    }
    // Counter bar: 24px tall, brightness encodes the frame number.
    let bar_h = 24.min(h / 8).max(8);
    let blocks = 32;
    for row in (h - bar_h)..h {
        for b in 0..blocks {
            let on = (n >> (b % 16)) & 1 == 1;
            let v = if on { 200 } else { 60 };
            let x0 = b * w / blocks;
            let x1 = (b + 1) * w / blocks;
            for col in x0..x1 {
                y[row * w + col] = v;
            }
        }
    }
    frame
}

/// Nearest-neighbor scale of an I420 frame (for movie normalization).
fn scale_frame(src: &I420Frame, w: usize, h: usize) -> Cow<'_, I420Frame> {
    if src.w == w && src.h == h {
        return Cow::Borrowed(src);
    }
    let mut scaled = I420Frame { w, h, data: Vec::new() };
    scale_frame_reusing(src, w, h, &mut scaled);
    Cow::Owned(scaled)
}

/// The encode thread owns the scratch frame until synchronous encode returns.
fn scale_frame_reusing<'a>(src: &'a I420Frame, w: usize, h: usize, scratch: &'a mut I420Frame) -> &'a I420Frame {
    if (src.w, src.h) == (w, h) {
        return src;
    }
    scratch.w = w;
    scratch.h = h;
    scratch.data.resize(w * h * 3 / 2, 0);
    let data = &mut scratch.data;
    let (sy, su, sv) = (
        &src.data[..src.w * src.h],
        &src.data[src.w * src.h..src.w * src.h + src.w * src.h / 4],
        &src.data[src.w * src.h + src.w * src.h / 4..],
    );
    for row in 0..h {
        for col in 0..w {
            data[row * w + col] = sy[(row * src.h / h) * src.w + (col * src.w / w)];
        }
    }
    let (du, dv) = (w * h, w * h + w * h / 4);
    for row in 0..h / 2 {
        for col in 0..w / 2 {
            let s = (row * (src.h / 2) / (h / 2)) * (src.w / 2) + (col * (src.w / 2) / (w / 2));
            data[du + row * (w / 2) + col] = su[s];
            data[dv + row * (w / 2) + col] = sv[s];
        }
    }
    scratch
}

/// Capture silence is not a new picture. Repeat the last CPU frame when we
/// have one; otherwise skip the tick. GPU buffers are one-shot, so a timeout
/// after retained frames has no CPU holdover — inventing black would replace
/// the still on the wire.
fn cpu_holdover_on_timeout(last: Option<&I420Frame>) -> Option<&I420Frame> {
    last
}

/// Split Annex-B into NAL byte-ranges (start codes included).
pub(crate) fn annexb_nals(data: &[u8]) -> Vec<std::ops::Range<usize>> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && (data[i + 2] == 1 || (data[i + 2] == 0 && data[i + 3] == 1)) {
            let len = if data[i + 2] == 1 { 3 } else { 4 };
            starts.push((i, len));
            i += len;
        } else {
            i += 1;
        }
    }
    if starts.is_empty() {
        return Vec::new();
    }
    starts
        .iter()
        .enumerate()
        .map(|(idx, &(start, _))| {
            let end = starts
                .get(idx + 1)
                .map(|&(next, _)| next)
                .unwrap_or(data.len());
            start..end
        })
        .collect()
}

/// NAL unit type of an Annex-B NAL (byte after the start code).
pub(crate) fn nal_type(nal: &[u8]) -> Option<u8> {
    let mut i = 0;
    while i + 2 < nal.len() && nal[i] == 0 {
        i += 1;
    }
    if i + 1 < nal.len() && nal[i] == 0 && nal[i + 1] == 1 {
        nal.get(i + 2).map(|b| b & 0x1F)
    } else if i < nal.len() && nal[i] == 1 {
        nal.get(i + 1).map(|b| b & 0x1F)
    } else {
        None
    }
}

/// True when an Annex-B access unit carries an IDR slice (NAL type 5).
fn contains_idr(unit: &[u8]) -> bool {
    annexb_nals(unit)
        .iter()
        .any(|range| nal_type(&unit[range.clone()]) == Some(5))
}

// ---------------------------------------------------------------------------
// OpenH264 encode / decode
// ---------------------------------------------------------------------------

/// H.264 Constrained-Baseline encoder (software; hardware is a later step
/// with explicit fallback — never a black screen).
pub struct H264Encoder {
    enc: Encoder,
    w: usize,
    h: usize,
}

impl H264Encoder {
    pub fn new(quality: Quality) -> Result<Self, MediaError> {
        let (w, _h) = quality.dims();
        let _ = w;
        let config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(BITRATE_BPS))
            .max_frame_rate(FrameRate::from_hz(FPS as f32))
            .intra_frame_period(IntraFramePeriod::from_num_frames(INTRA_PERIOD))
            .profile(Profile::Baseline)
            .level(quality.level())
            .skip_frames(false);
        let enc = Encoder::with_api_config(openh264::OpenH264API::from_source(), config)
            .map_err(|e| MediaError::Codec(format!("encoder init: {e}")))?;
        let (w, h) = quality.dims();
        Ok(Self { enc, w, h })
    }

    /// Profile-driven constructor. `w`/`h` are the EFFECTIVE encode dims
    /// (already normalized even); frames must match exactly.
    pub fn new_with_profile(profile: &QualityProfile, w: usize, h: usize) -> Result<Self, MediaError> {
        profile.validate().map_err(|e| MediaError::Codec(format!("profile: {e}")))?;
        if w < 2 || h < 2 || w > MAX_DIM as usize || h > MAX_DIM as usize || w % 2 != 0 || h % 2 != 0 {
            return Err(MediaError::Codec(format!("backend dims must be even 2..={MAX_DIM}: {w}x{h}")));
        }
        let (w, h) = fit_openh264_dims(w, h);
        let config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(profile.bitrate_kbps * 1000))
            .max_frame_rate(FrameRate::from_hz(profile.fps as f32))
            .intra_frame_period(IntraFramePeriod::from_num_frames(
                profile.keyframe_interval_frames(),
            ))
            .profile(Profile::Baseline)
            .level(level_for(w, h))
            .skip_frames(false);
        let enc = Encoder::with_api_config(openh264::OpenH264API::from_source(), config)
            .map_err(|e| MediaError::Codec(format!("encoder init: {e}")))?;
        Ok(Self { enc, w, h })
    }

    /// Force the next unit to start with an IDR (reconfig, error recovery).
    pub fn force_intra(&mut self) {
        self.enc.force_intra_frame();
    }

    /// Encode one frame; returns an Annex-B access unit (start codes added
    /// explicitly per NAL — never depends on encoder defaults).
    pub fn encode(&mut self, frame: &I420Frame) -> Result<Vec<u8>, MediaError> {
        if frame.w != self.w || frame.h != self.h {
            return Err(MediaError::Codec("frame size mismatch".into()));
        }
        let bitstream = self
            .enc
            .encode(&frame.yuv_buffer())
            .map_err(|e| MediaError::Codec(format!("encode: {e}")))?;
        let mut out = Vec::new();
        for layer_idx in 0..bitstream.num_layers() {
            let Some(layer) = bitstream.layer(layer_idx) else {
                continue;
            };
            for nal_idx in 0..layer.nal_count() {
                if let Some(nal) = layer.nal_unit(nal_idx) {
                    out.extend_from_slice(&START_CODE);
                    // Strip any start code the encoder may have included so
                    // units never carry doubled prefixes.
                    out.extend_from_slice(strip_start_code(nal));
                }
            }
        }
        if out.is_empty() {
            return Err(MediaError::Codec("encoder produced no NALs".into()));
        }
        Ok(out)
    }
}

/// Encoder backend: software everywhere, hardware where probed.
pub enum VideoEncoder {
    Software(H264Encoder),
    #[cfg(target_os = "macos")]
    Hardware(crate::vt::VtEncoder),
    #[cfg(target_os = "windows")]
    Hardware(crate::nvenc::NvencEncoder),
}

/// One real probe per process (a VT session + frame costs single-digit ms,
/// but every reconfig rebuild would repay it). The `GOLIVE_DISABLE_HW`
/// hook bypasses the cache so forced-fallback tests stay deterministic.
static PROBE_OUTCOME: std::sync::OnceLock<(bool, String)> = std::sync::OnceLock::new();

fn probe_cached() -> (bool, String) {
    if std::env::var_os("GOLIVE_DISABLE_HW").is_some() {
        return match probe_hardware() {
            Ok(()) => (true, String::new()),
            Err(e) => (false, e.to_string()),
        };
    }
    PROBE_OUTCOME
        .get_or_init(|| match probe_hardware() {
            Ok(()) => (true, String::new()),
            Err(e) => (false, e.to_string()),
        })
        .clone()
}

/// Pure decision (tested): a working probe selects hardware, anything else
/// is explicit software. `Hardware` requested directly never passes here
/// (fail-high at the call site).
fn decide_engine(hw_available: bool) -> EngineKind {
    if hw_available {
        EngineKind::Hardware
    } else {
        EngineKind::Software
    }
}

impl VideoEncoder {
    pub fn new(
        profile: &QualityProfile,
        w: usize,
        h: usize,
        engine: EngineKind,
    ) -> Result<Self, MediaError> {
        match engine {
            EngineKind::Software => Ok(Self::Software(H264Encoder::new_with_profile(
                profile, w, h,
            )?)),
            EngineKind::Hardware => {
                #[cfg(target_os = "macos")]
                {
                    Ok(Self::Hardware(crate::vt::VtEncoder::new(
                        w,
                        h,
                        profile.bitrate_kbps * 1000,
                        profile.fps,
                        true,
                    )?))
                }
                #[cfg(target_os = "windows")]
                {
                    Ok(Self::Hardware(crate::nvenc::NvencEncoder::new(
                        w,
                        h,
                        profile.bitrate_kbps * 1000,
                        profile.fps,
                        true,
                    )?))
                }
                #[cfg(not(any(target_os = "macos", target_os = "windows")))]
                {
                    let _ = (profile, w, h);
                    Err(MediaError::HwUnavailable(
                        "no hardware encoder on this platform".into(),
                    ))
                }
            }
            EngineKind::Auto => {
                let (hw, reason) = probe_cached();
                let selected = decide_engine(hw);
                match Self::new(profile, w, h, selected) {
                    Ok(encoder) => {
                        let motive = if hw {
                            "probe ok".to_owned()
                        } else {
                            format!("probe failed ({reason}); software fallback")
                        };
                        eprintln!(
                            "golive: encode backend={} target={}x{} ({motive})",
                            encoder.backend_name(),
                            encoder.dims().0,
                            encoder.dims().1
                        );
                        Ok(encoder)
                    }
                    Err(e) if selected == EngineKind::Hardware => {
                        let encoder = Self::new(profile, w, h, EngineKind::Software)?;
                        eprintln!(
                            "golive: encode backend={} target={}x{} (hw failed ({e}); software fallback)",
                            encoder.backend_name(),
                            encoder.dims().0,
                            encoder.dims().1
                        );
                        Ok(encoder)
                    }
                    Err(e) => Err(e),
                }
            }
        }
    }

    /// Encode one frame; `Ok(None)` is a transient skip (capped upstream),
    /// `Err` is fatal. Units are Annex-B either way.
    pub fn encode_frame(&mut self, frame: &I420Frame) -> Result<Option<Vec<u8>>, MediaError> {
        match self {
            Self::Software(enc) => enc.encode(frame).map(Some),
            #[cfg(target_os = "macos")]
            Self::Hardware(enc) => {
                let pixels = frame.w * frame.h;
                enc.encode_i420(
                    frame.w,
                    frame.h,
                    &frame.data[..pixels],
                    &frame.data[pixels..pixels + pixels / 4],
                    &frame.data[pixels + pixels / 4..],
                )
            }
            #[cfg(target_os = "windows")]
            Self::Hardware(enc) => {
                let nv12 = crate::vt::i420_to_nv12(
                    frame.w,
                    frame.h,
                    &frame.data[..frame.w * frame.h],
                    &frame.data[frame.w * frame.h..frame.w * frame.h + frame.w * frame.h / 4],
                    &frame.data[frame.w * frame.h + frame.w * frame.h / 4..],
                );
                enc.encode_nv12(&nv12)
            }
        }
    }

    pub fn force_intra(&mut self) {
        match self {
            Self::Software(enc) => enc.force_intra(),
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            Self::Hardware(enc) => enc.force_intra(),
        }
    }

    pub fn dims(&self) -> (usize, usize) {
        match self {
            Self::Software(enc) => (enc.w, enc.h),
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            Self::Hardware(enc) => enc.dims(),
        }
    }

    pub fn backend_name(&self) -> &'static str {
        match self {
            Self::Software(_) => "openh264",
            #[cfg(target_os = "macos")]
            Self::Hardware(_) => "videotoolbox",
            #[cfg(target_os = "windows")]
            Self::Hardware(enc) => enc.backend_name(),
        }
    }
}

/// Hardware probe: `Ok` iff a real HW H.264 encode completes. Env hook
/// `GOLIVE_DISABLE_HW=1` forces failure (deterministic fallback tests).
pub fn probe_hardware() -> Result<(), MediaError> {
    #[cfg(target_os = "macos")]
    {
        crate::vt::probe_hardware()
    }
    #[cfg(target_os = "windows")]
    {
        crate::nvenc::probe_hardware()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Err(MediaError::HwUnavailable(
            "no hardware encoder on this platform".into(),
        ))
    }
}

fn strip_start_code(nal: &[u8]) -> &[u8] {
    if nal.starts_with(&[0, 0, 0, 1]) {
        &nal[4..]
    } else if nal.starts_with(&[0, 0, 1]) {
        &nal[3..]
    } else {
        nal
    }
}

/// OpenH264 software cannot hold 3840×1080 in realtime (Windows has no
/// VideoToolbox; the crate also hard-fails above 3840×2160). Cap to 1080p
/// so a 5120×1440 window still emits video instead of audio-only.
pub fn fit_openh264_dims(w: usize, h: usize) -> (usize, usize) {
    const LONG: usize = 1920;
    const SHORT: usize = 1080;
    if w < 2 || h < 2 {
        return (2, 2);
    }
    let (long, short, portrait) = if w >= h { (w, h, false) } else { (h, w, true) };
    let scale = (LONG as f64 / long as f64).min(SHORT as f64 / short as f64).min(1.0);
    let long = ((long as f64 * scale) as usize).max(2) & !1;
    let short = ((short as f64 * scale) as usize).max(2) & !1;
    if portrait { (short, long) } else { (long, short) }
}

/// Hardware encode box: Media Foundation inbox H.264 MFTs refuse widths past
/// 4096 (`SetOutputType 0x80041000` at 5120 wide on NVENC/RTX 3070, while
/// 4096×1152 and 3840×2160 encode fine). Aspect-fit into 4096×4096,
/// even-floored, never upscaling. Pure; idempotent. This is the step-down
/// the Auto cascade tries before giving up to software, so a 5120×1440
/// monitor still emits 4096×1152 NVENC instead of 1920×540 software.
pub fn fit_hardware_dims(w: usize, h: usize) -> (usize, usize) {
    const LONG: usize = 4096;
    const SHORT: usize = 4096;
    if w < 2 || h < 2 {
        return (2, 2);
    }
    let (long, short, portrait) = if w >= h { (w, h, false) } else { (h, w, true) };
    let scale = (LONG as f64 / long as f64).min(SHORT as f64 / short as f64).min(1.0);
    let long = ((long as f64 * scale) as usize).max(2) & !1;
    let short = ((short as f64 * scale) as usize).max(2) & !1;
    if portrait { (short, long) } else { (long, short) }
}

/// H.264 level tier by pixel count: HD and below is 3.1, above is 4.0.
/// Matches the historical mapping (720p→3.1, 1080p→4.0).
fn level_for(w: usize, h: usize) -> Level {
    let pixels = (w as u64).saturating_mul(h as u64);
    if pixels <= 1280 * 720 {
        Level::Level_3_1
    } else if pixels <= 1920 * 1080 {
        Level::Level_4_1
    } else if pixels <= 2560 * 1440 {
        Level::Level_5_0
    } else {
        Level::Level_5_1
    }
}

/// Bounded unit slot between the encode thread and the RTP pump: at most ONE
/// access unit is queued. The producer waits for capacity before encoding, so
/// an already encoded unit is never replaced and shutdown remains bounded.
/// An empty unit is the poison pill (encoders never emit empty units).
#[derive(Debug, Default)]
struct FrameSlot {
    state: std::sync::Mutex<FrameSlotState>,
    capacity: std::sync::Condvar,
    notify: tokio::sync::Notify,
}

#[derive(Debug, Default)]
struct FrameSlotState {
    item: Option<(Vec<u8>, Duration)>,
    closed: bool,
}

impl FrameSlot {
    fn wait_for_capacity(&self, stop: &AtomicBool) -> bool {
        let mut state = self.state.lock().expect("frame slot poisoned");
        while state.item.is_some() && !state.closed && !stop.load(Ordering::Acquire) {
            state = self.capacity.wait_timeout(state, Duration::from_millis(50)).expect("frame slot poisoned").0;
        }
        !state.closed && !stop.load(Ordering::Acquire)
    }

    fn publish(&self, unit: Vec<u8>, duration: Duration) -> bool {
        let mut state = self.state.lock().expect("frame slot poisoned");
        if state.closed || state.item.is_some() {
            return false;
        }
        state.item = Some((unit, duration));
        self.notify.notify_one();
        true
    }

    fn poison(&self) {
        let mut state = self.state.lock().expect("frame slot poisoned");
        state.closed = true;
        self.capacity.notify_all();
        self.notify.notify_one();
    }

    async fn take(&self) -> (Vec<u8>, Duration) {
        loop {
            let result = {
                let mut state = self.state.lock().expect("frame slot poisoned");
                if let Some(item) = state.item.take() {
                    self.capacity.notify_one();
                    Some(item)
                } else if state.closed {
                    Some((Vec::new(), Duration::ZERO))
                } else {
                    None
                }
            };
            if let Some(item) = result { return item; }
            self.notify.notified().await;
        }
    }
}

/// Decoder feeding whole access units; returns luma stats per decoded frame.
///
/// Backend is an enum like the encoder side: software everywhere, hardware
/// where probed (Windows MF DXVA — see `mfdec`). `new()` pins software
/// (deterministic: tests, movie preload); `new_auto()` probes for hardware
/// with transparent software fallback, and is what the live viewer uses.
pub enum H264Decoder {
    Software { dec: Decoder },
    Native(Box<dyn golive_platform::decode::VideoDecoder>),
    RecoveringSoftware { parameter_sets: Vec<u8> },
    #[cfg(target_os = "windows")]
    Hardware(crate::mfdec::MfDecoder),
}

#[derive(Clone, Copy, Debug)]
pub struct DecodedStats {
    pub w: usize,
    pub h: usize,
    pub luma_mean: f64,
    pub is_keyframe: bool,
}

/// Pixel layout carried to a presenter. YUV uses BT.601 limited range,
/// matching the existing software conversion. Planes are tightly packed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum PixelFormat { Rgba = 0, I420 = 1, Nv12 = 2 }

/// One decoded picture ready to present.
/// Carried by value through the present callback (owned per frame; the shell
/// drops stale ones instead of queuing).
#[derive(Clone, Debug)]
pub struct PresentedFrame {
    pub w: usize,
    pub h: usize,
    pub data: Vec<u8>,
    pub format: PixelFormat,
}

/// Stats plus pixels from a single decode. One decode feeds validation,
/// events and presentation — never decoded twice.
#[derive(Clone, Debug)]
pub struct DecodedPicture {
    pub stats: DecodedStats,
    pub frame: PresentedFrame,
}

static THREAD_CPU_CLOCK: std::sync::OnceLock<fn() -> Option<u64>> = std::sync::OnceLock::new();

/// Installs a clock measuring only the calling media worker, for opt-in traces.
pub fn install_media_cpu_clock(clock: fn() -> Option<u64>) { let _ = THREAD_CPU_CLOCK.set(clock); }
pub(crate) fn media_cpu_us() -> Option<u64> { THREAD_CPU_CLOCK.get().and_then(|clock| clock()) }

static NATIVE_DECODER_FACTORY: std::sync::OnceLock<golive_platform::decode::DecoderFactory> = std::sync::OnceLock::new();

/// The app installs an OS backend before starting media. Core imports only
/// the platform's pure contract. Creation happens on the serial codec worker.
pub fn install_decoder_factory(factory: golive_platform::decode::DecoderFactory) {
    let _ = NATIVE_DECODER_FACTORY.set(factory);
}

impl H264Decoder {
    /// Software decoder, always. Deterministic everywhere (tests, movie
    /// preload, forced-fallback paths).
    pub fn new() -> Result<Self, MediaError> {
        let dec =
            Decoder::new().map_err(|e| MediaError::Codec(format!("decoder init: {e}")))?;
        Ok(Self::Software { dec })
    }

    /// Automatic engine selection: hardware DXVA where the probe passes
    /// (Windows MF decoder MFT), explicit software fallback otherwise.
    /// Never fails while software constructs; a mid-stream hardware fault
    /// transparently degrades to software on the failing unit.
    pub fn new_auto() -> Result<Self, MediaError> {
        if std::env::var_os("GOLIVE_DISABLE_HW").is_none() {
            if let Some(factory) = NATIVE_DECODER_FACTORY.get() {
                if let Ok(decoder) = factory() { return Ok(Self::Native(decoder)); }
            }
        }
        #[cfg(target_os = "windows")]
        {
            if std::env::var_os("GOLIVE_DISABLE_HW").is_none() {
                if crate::mfdec::probe_hardware().is_ok() {
                    match crate::mfdec::MfDecoder::new() {
                        Ok(hw) => {
                            eprintln!("golive: decode backend={}", hw.backend_name());
                            return Ok(Self::Hardware(hw));
                        }
                        Err(_) => {}
                    }
                }
            }
        }
        Self::new()
    }

    /// Live decode backend (`openh264` or the hardware name). Diagnostics
    /// only — same redaction rules as the encode side (names, never pixels).
    pub fn backend_name(&self) -> &'static str {
        match self {
            Self::Software { .. } | Self::RecoveringSoftware { .. } => "openh264",
            Self::Native(_) => "native",
            #[cfg(target_os = "windows")]
            Self::Hardware(hw) => hw.backend_name(),
        }
    }

    fn waiting_for_keyframe(&self) -> bool { matches!(self, Self::RecoveringSoftware { .. }) }

    pub fn decode(&mut self, annexb: &[u8]) -> Result<Option<DecodedPicture>, MediaError> {
        self.decode_for_present(annexb, false)
    }

    pub fn decode_for_present(&mut self, annexb: &[u8], compact: bool) -> Result<Option<DecodedPicture>, MediaError> {
        self.decode_measured(annexb, compact, None)
    }

    fn decode_measured(&mut self, annexb: &[u8], compact: bool, mut measurements: Option<&mut DecodeMeasurements>) -> Result<Option<DecodedPicture>, MediaError> {
        let is_keyframe = contains_idr(annexb);
        let started = measurements.as_ref().map(|_| Instant::now());
        match self {
            Self::RecoveringSoftware { parameter_sets } => {
                // A fresh decoder has no reference pictures. Never feed it
                // deltas from the failed hardware session or conceal garbage.
                if !is_keyframe { return Ok(None); }
                let mut recovered = std::mem::take(parameter_sets);
                recovered.extend_from_slice(annexb);
                *self = Self::new()?;
                self.decode_measured(&recovered, compact, measurements)
            }
            Self::Native(hw) => {
                let decoded = hw.decode(annexb);
                if let Some(m) = measurements.as_deref_mut() { m.codec_us = elapsed_us(started); }
                match decoded {
                    Ok(None) => Ok(None),
                    Ok(Some(pic)) => {
                        let started = measurements.as_ref().map(|_| Instant::now());
                        let (w, h) = (pic.width, pic.height);
                        if w == 0 || h == 0 || w > 8192 || h > 8192 || w % 2 != 0 || h % 2 != 0 || pic.data.len() != w*h*3/2 {
                            let parameter_sets = hw.parameter_sets();
                            *self = Self::RecoveringSoftware { parameter_sets };
                            eprintln!("golive: decode backend=openh264 recovery=invalid-native-output");
                            return self.decode_measured(annexb, compact, measurements);
                        }
                        let sum: u64 = pic.data[..w*h].iter().map(|b| *b as u64).sum();
                        let (data, format) = if compact { (pic.data, PixelFormat::Nv12) }
                            else { (crate::mfdec::nv12_to_rgba(w,h,&pic.data).ok_or_else(|| MediaError::Codec("native pixel conversion".into()))?, PixelFormat::Rgba) };
                        if let Some(m) = measurements { m.convert_us = elapsed_us(started); }
                        Ok(Some(DecodedPicture {
                            stats: DecodedStats { w, h, luma_mean: sum as f64 / (w*h) as f64, is_keyframe },
                            frame: PresentedFrame { w, h, data, format },
                        }))
                    }
                    Err(_) => {
                        let parameter_sets = hw.parameter_sets();
                        *self = Self::RecoveringSoftware { parameter_sets };
                        eprintln!("golive: decode backend=openh264 recovery=await-idr");
                        self.decode_measured(annexb, compact, measurements)
                    }
                }
            }
            Self::Software { dec } => {
                let decoded = dec.decode(annexb).map_err(|e| MediaError::Codec(format!("decode: {e}")))?;
                if let Some(m) = measurements.as_deref_mut() { m.codec_us = elapsed_us(started); }
                let Some(yuv) = decoded else { return Ok(None) };
                let started = measurements.as_ref().map(|_| Instant::now());
                let (w, h) = yuv.dimensions();
                let strides = yuv.strides();
                if w == 0 || h == 0 { return Ok(None); }
                // Ignore decoder row padding both in the pixels and the luma statistic.
                let sum: u64 = yuv.y().chunks(strides.0).take(h)
                    .flat_map(|row| row[..w].iter()).map(|b| *b as u64).sum();
                let (data, format) = if compact && w % 2 == 0 && h % 2 == 0 {
                    let mut data = Vec::with_capacity(w * h * 3 / 2);
                    for (plane, stride, width, height) in [
                        (yuv.y(), strides.0, w, h),
                        (yuv.u(), strides.1, w / 2, h / 2),
                        (yuv.v(), strides.2, w / 2, h / 2),
                    ] {
                        for row in plane.chunks(stride).take(height) { data.extend_from_slice(&row[..width]); }
                    }
                    (data, PixelFormat::I420)
                } else {
                    let mut data = vec![0; w * h * 4];
                    yuv.write_rgba8(&mut data);
                    (data, PixelFormat::Rgba)
                };
                if let Some(m) = measurements { m.convert_us = elapsed_us(started); }
                Ok(Some(DecodedPicture {
                    stats: DecodedStats { w, h, luma_mean: sum as f64 / (w * h) as f64, is_keyframe },
                    frame: PresentedFrame { w, h, data, format },
                }))
            }
            #[cfg(target_os = "windows")]
            Self::Hardware(hw) => {
                let decoded = hw.decode_annexb(annexb);
                if let Some(m) = measurements.as_deref_mut() { m.codec_us = elapsed_us(started); }
                match decoded {
                    Ok(None) => Ok(None),
                    Ok(Some(pic)) => {
                        let started = measurements.as_ref().map(|_| Instant::now());
                        let (w, h) = (pic.w, pic.h);
                        let sum: u64 = pic.nv12[..w * h].iter().map(|b| *b as u64).sum();
                        let (data, format) = if compact {
                            (pic.nv12, PixelFormat::Nv12)
                        } else {
                            (crate::mfdec::nv12_to_rgba(w, h, &pic.nv12)
                                .ok_or_else(|| MediaError::Codec("hw decode dims invalid".into()))?, PixelFormat::Rgba)
                        };
                        if let Some(m) = measurements { m.convert_us = elapsed_us(started); }
                        Ok(Some(DecodedPicture {
                            stats: DecodedStats { w, h, luma_mean: sum as f64 / (w * h) as f64, is_keyframe },
                            frame: PresentedFrame { w, h, data, format },
                        }))
                    }
                    Err(_) => {
                        *self = Self::RecoveringSoftware { parameter_sets: Vec::new() };
                        self.decode_measured(annexb, compact, measurements)
                    }
                }
            }
        }
    }

}

#[derive(Default)]
struct DecodeMeasurements { codec_us: u64, convert_us: u64, cpu_work_us: u64, cpu_samples: u64, request_keyframe: bool }
fn elapsed_us(started: Option<Instant>) -> u64 {
    started.map(|s| s.elapsed().as_micros() as u64).unwrap_or(0)
}

/// Luma validator: non-black + motion against the previous frame.
pub struct FrameValidator {
    prev_mean: Option<f64>,
}

impl FrameValidator {
    pub fn new() -> Self {
        Self { prev_mean: None }
    }

    /// Non-black: mean luma far above black, far below any real picture risk.
    pub fn non_black(mean: f64) -> bool {
        mean > 8.0
    }

    /// Motion: mean shifted since the previous decoded frame.
    pub fn motion(&mut self, mean: f64) -> bool {
        let moved = self.prev_mean.map(|p| (p - mean).abs() > 0.05).unwrap_or(false);
        self.prev_mean = Some(mean);
        moved
    }
}

// ---------------------------------------------------------------------------
// PeerConnection factory (symmetric both ends)
// ---------------------------------------------------------------------------

/// Builds the shared API: default codecs (H264 CB mode-1 included, already
/// carrying goog-remb + ccm fir + nack + nack pli), mDNS DISABLED on both
/// ends (lesson 2: symmetric, plus redacted census), and the default
/// interceptors (NACK generator+responder, sender/receiver reports, TWCC
/// receiver-only) — the loss-recovery half of PLI. The IDR-request half is
/// the viewer `read_loop` PLI below plus the publisher RTCP task.
fn build_api() -> Result<webrtc::api::API, MediaError> {
    let mut media_engine = MediaEngine::default();
    media_engine
        .register_default_codecs()
        .map_err(|e| MediaError::Transport(format!("codecs: {e}")))?;
    let registry = register_default_interceptors(Registry::new(), &mut media_engine)
        .map_err(|e| MediaError::Transport(format!("interceptors: {e}")))?;
    let mut setting_engine = SettingEngine::default();
    setting_engine.set_ice_multicast_dns_mode(MulticastDnsMode::Disabled);
    Ok(APIBuilder::default()
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .with_setting_engine(setting_engine)
        .build())
}

/// Apply one trickle candidate to every m-line (audio+video). Hardcoding
/// index 0 attached ICE to only the first media section, so a two-m-line
/// offer delivered one medium per attempt.
async fn add_ice_candidate_all_mlines(
    pc: &RTCPeerConnection,
    candidate: &str,
) -> Result<(), MediaError> {
    let n = pc.get_transceivers().await.len().max(1);
    let mut last = None;
    let mut any = false;
    for index in 0..=n {
        let init = RTCIceCandidateInit {
            candidate: candidate.to_owned(),
            sdp_mid: None,
            sdp_mline_index: if index == n {
                None
            } else {
                Some(index as u16)
            },
            username_fragment: None,
        };
        match pc.add_ice_candidate(init).await {
            Ok(()) => any = true,
            Err(e) => last = Some(e),
        }
    }
    if any {
        Ok(())
    } else {
        Err(MediaError::Transport(format!(
            "add candidate: {}",
            last.map(|e| e.to_string()).unwrap_or_else(|| "failed".into())
        )))
    }
}

fn rtc_config(ice_servers: Option<Vec<String>>) -> RTCConfiguration {
    let urls = ice_servers.unwrap_or_else(|| vec!["stun:stun.l.google.com:19302".to_owned()]);
    RTCConfiguration {
        ice_servers: vec![RTCIceServer {
            urls,
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn opus_codec() -> RTCRtpCodecCapability {
    RTCRtpCodecCapability {
        mime_type: MIME_TYPE_OPUS.to_owned(),
        clock_rate: 48000,
        channels: 2,
        sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
        rtcp_feedback: vec![],
    }
}

fn video_codec() -> RTCRtpCodecCapability {
    use webrtc::api::media_engine::MIME_TYPE_H264;
    RTCRtpCodecCapability {
        mime_type: MIME_TYPE_H264.to_owned(),
        clock_rate: 90000,
        channels: 0,
        sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
            .to_owned(),
        // Loss recovery, mirrored from the media-engine defaults: generic
        // NACK (retransmit via the responder interceptor) + PLI (viewer asks
        // for an IDR on irrecoverable AU gaps) + FIR (same, alternate form).
        // The offer carries `a=rtcp-fb:<pt> nack pli`; behavior under no
        // loss is byte-identical to before.
        rtcp_feedback: vec![
            RTCPFeedback {
                typ: "nack".to_owned(),
                parameter: "".to_owned(),
            },
            RTCPFeedback {
                typ: "nack".to_owned(),
                parameter: "pli".to_owned(),
            },
            RTCPFeedback {
                typ: "ccm".to_owned(),
                parameter: "fir".to_owned(),
            },
        ],
    }
}

// ---------------------------------------------------------------------------
// PLI loss recovery (viewer asks, publisher obeys; signaling untouched)
// ---------------------------------------------------------------------------

/// Minimum gap between PLIs for one SSRC (~1/s): loss recovery without RTCP
/// floods. Never per packet — only the irrecoverable-AU-gap path arms it.
const PLI_DEBOUNCE: Duration = Duration::from_secs(1);

/// Pure debounce decision (the clock is the caller's, so tests own time):
/// first sighting per SSRC is due, repeats within [`PLI_DEBOUNCE`] are not.
fn pli_due(last: &mut HashMap<u32, Instant>, media_ssrc: u32, now: Instant) -> bool {
    match last.get(&media_ssrc) {
        Some(&t) if now.duration_since(t) < PLI_DEBOUNCE => false,
        _ => {
            last.insert(media_ssrc, now);
            true
        }
    }
}

/// Sends one PLI for `media_ssrc` unless one went out within the debounce
/// window. Fire-and-forget: RTCP send failures just mean the next gap asks
/// again (still debounced). Returns whether a PLI went out, so the caller
/// can trace sent vs suppressed gap storms (numeric counters only).
async fn maybe_send_pli(
    pc: &Arc<RTCPeerConnection>,
    last_pli: &mut HashMap<u32, Instant>,
    media_ssrc: u32,
) -> bool {
    if !pli_due(last_pli, media_ssrc, Instant::now()) {
        return false;
    }
    let pli = PictureLossIndication {
        sender_ssrc: 0,
        media_ssrc,
    };
    let _ = pc
        .write_rtcp(&[Box::new(pli) as Box<dyn RtcpPacket + Send + Sync>])
        .await;
    true
}

// ---------------------------------------------------------------------------
// Publisher (encode + send; one TrackLocalStaticSample fanned out by the
// crate to every watcher — encode once, fanout free)
// ---------------------------------------------------------------------------

struct SharedEncoder {
    track: Arc<TrackLocalStaticSample>,
    encode_stop: Arc<AtomicBool>,
    encode_thread: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
    reconfig_tx: std::sync::mpsc::Sender<QualityProfile>,
    slot: Arc<FrameSlot>,
    backend: Arc<std::sync::Mutex<Option<&'static str>>>,
    intra_requested: Arc<AtomicBool>,
    listeners: Arc<std::sync::Mutex<HashMap<u64, mpsc::UnboundedSender<MediaEvent>>>>,
    next_listener: std::sync::atomic::AtomicU64,
}

pub struct Publisher {
    pc: Arc<RTCPeerConnection>,
    event_tx: mpsc::UnboundedSender<MediaEvent>,
    shared: Arc<SharedEncoder>,
    listener: u64,
    stopped: Arc<AtomicBool>,
    rtcp_task: Option<tokio::task::JoinHandle<()>>,
}

impl Publisher {
    /// Starts publishing `source` at `quality` with automatic engine
    /// selection (hardware where the probe passes, explicit software
    /// fallback otherwise). Needs a running tokio runtime.
    pub async fn start(
        source: VideoSource,
        quality: Quality,
        ice_servers: Option<Vec<String>>,
        event_tx: mpsc::UnboundedSender<MediaEvent>,
    ) -> Result<Self, MediaError> {
        Self::start_with_profile(
            source,
            quality.profile(),
            EngineKind::Auto,
            ice_servers,
            event_tx,
        )
        .await
    }

    /// Starts publishing at an explicit profile + engine. Needs a running
    /// tokio runtime. Profile validation fails fast and typed, before any
    /// peer connection exists.
    pub async fn start_with_profile(
        source: VideoSource,
        profile: QualityProfile,
        engine: EngineKind,
        ice_servers: Option<Vec<String>>,
        event_tx: mpsc::UnboundedSender<MediaEvent>,
    ) -> Result<Self, MediaError> {
        Self::start_with_profile_and_audio(
            source,
            profile,
            engine,
            ice_servers,
            event_tx,
            None,
        )
        .await
    }

    pub async fn start_with_profile_and_audio(
        source: VideoSource,
        profile: QualityProfile,
        engine: EngineKind,
        ice_servers: Option<Vec<String>>,
        event_tx: mpsc::UnboundedSender<MediaEvent>,
        audio_rx: Option<std::sync::mpsc::Receiver<EncodedAudioPacket>>,
    ) -> Result<Self, MediaError> {
        profile
            .validate()
            .map_err(|e| MediaError::Codec(format!("profile: {e}")))?;
        // Shared redacted ICE census: trickle handler bumps it, loops
        // snapshot it into Stats. Kinds/families only, never addresses.
        let census = Arc::new(std::sync::Mutex::new(CandidateCensus::default()));
        let api = build_api()?;
        let pc = Arc::new(
            api.new_peer_connection(rtc_config(ice_servers))
                .await
                .map_err(|e| MediaError::Transport(format!("pc: {e}")))?,
        );
        let track = Arc::new(TrackLocalStaticSample::new(
            video_codec(),
            "video".to_owned(),
            "golive".to_owned(),
        ));
        let sender = pc
            .add_track(track.clone())
            .await
            .map_err(|e| MediaError::Transport(format!("add_track: {e}")))?;
        let audio_pair = if let Some(audio_rx) = audio_rx {
            let audio_track = Arc::new(TrackLocalStaticSample::new(
                opus_codec(),
                "audio".to_owned(),
                "golive".to_owned(),
            ));
            pc.add_track(audio_track.clone())
                .await
                .map_err(|e| MediaError::Transport(format!("add_audio_track: {e}")))?;
            Some((audio_track, audio_rx))
        } else {
            None
        };
        // Explicit sendonly: this side never receives. (Default would be
        // sendrecv; the contract pins directions, asserted in E2E.)
        for transceiver in pc.get_transceivers().await {
            transceiver
                .set_direction(
                    webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection::Sendonly,
                )
                .await;
        }
        wire_ice_events(&pc, &event_tx, &census);

        // Encode on a blocking thread; bounded slot into tokio (see FrameSlot).
        // `intra_requested` bridges the async RTCP task (PLI/FIR in) to the
        // encode thread (`force_intra()` out).
        let slot = Arc::new(FrameSlot::default());
        let backend = Arc::new(std::sync::Mutex::new(None));
        let intra_requested = Arc::new(AtomicBool::new(false));
        let (reconfig_tx, reconfig_rx) = std::sync::mpsc::channel::<QualityProfile>();
        let encode_stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let listeners = Arc::new(std::sync::Mutex::new(HashMap::from([(0, event_tx.clone())])));
        let (encode_events, mut encode_event_rx) = mpsc::unbounded_channel::<MediaEvent>();
        {
            let listeners = Arc::clone(&listeners);
            tokio::spawn(async move {
                while let Some(event) = encode_event_rx.recv().await {
                    if let Ok(listeners) = listeners.lock() {
                        for tx in listeners.values() { let _ = tx.send(event.clone()); }
                    }
                }
            });
        }
        let encode_thread = {
            let stop = Arc::clone(&encode_stop);
            let event_tx = encode_events;
            let slot = Arc::clone(&slot);
            let backend = Arc::clone(&backend);
            let census = Arc::clone(&census);
            let intra = Arc::clone(&intra_requested);
            std::thread::Builder::new()
                .name("golive-encode".into())
                .spawn(move || {
                    encode_loop(source, profile, engine, &stop, &slot, &backend, &census, &event_tx, &reconfig_rx, &intra);
                })
                .map_err(|e| MediaError::Codec(format!("encode thread: {e}")))?
        };
        if let Some((audio_track, audio_rx)) = audio_pair {
            let stop = Arc::clone(&stopped);
            let runtime = tokio::runtime::Handle::current();
            tokio::task::spawn_blocking(move || {
                while !stop.load(Ordering::Acquire) {
                    let Ok(packet) = audio_rx.recv_timeout(Duration::from_millis(20)) else {
                        continue;
                    };
                    let sample = Sample {
                        data: Bytes::from(packet.data),
                        timestamp: SystemTime::now(),
                        duration: packet.duration,
                        packet_timestamp: 0,
                        prev_dropped_packets: 0,
                        prev_padding_packets: 0,
                    };
                    let audio_track = Arc::clone(&audio_track);
                    let _ = runtime.block_on(async move { audio_track.write_sample(&sample).await });
                }
            });
        }
        // Pump freshest units into the track with per-sample durations from
        // the live profile (reconfig may change fps mid-share).
        {
            let slot = Arc::clone(&slot);
            let track = Arc::clone(&track);
            tokio::spawn(async move {
                let mut trace = Trace::new(Stage::Send);
                loop {
                    let (unit, duration) = slot.take().await;
                    if unit.is_empty() {
                        break; // poison pill from stop()
                    }
                    let sample = Sample {
                        data: Bytes::from(unit),
                        timestamp: SystemTime::now(),
                        duration,
                        packet_timestamp: 0,
                        prev_dropped_packets: 0,
                        prev_padding_packets: 0,
                    };
                    let started = trace.start();
                    let result = track.write_sample(&sample).await;
                    trace.record(TraceSample {
                        frames: result.is_ok() as u64,
                        bytes: sample.data.len() as u64,
                        errors: result.is_err() as u64,
                        ..Default::default()
                    }, started);
                    // A closing binding must not stop the other peers. WebRTC
                    // writes all bindings before returning aggregate errors.
                    // The producer's poison pill ends the source explicitly.
                }
            });
        }
        // RTCP recovery task: the viewer sends PLI (irrecoverable AU gap) or
        // FIR; either arms `intra_requested` and the encode loop forces the
        // next unit to start with an IDR. Generic NACKs never reach us (the
        // responder interceptor retransmits below us). Ends on RTCP errors
        // (close) and is aborted in `stop()`.
        let rtcp_task = {
            let intra = Arc::clone(&intra_requested);
            tokio::spawn(async move {
                loop {
                    match sender.read_rtcp().await {
                        Ok((pkts, _)) => {
                            for pkt in &pkts {
                                if pkt.as_any().downcast_ref::<PictureLossIndication>().is_some()
                                    || pkt.as_any().downcast_ref::<FullIntraRequest>().is_some()
                                {
                                    intra.store(true, Ordering::Release);
                                    break;
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
            })
        };

        Ok(Self {
            pc, event_tx, stopped, listener: 0, rtcp_task: Some(rtcp_task),
            shared: Arc::new(SharedEncoder {
                track, encode_stop, encode_thread: std::sync::Mutex::new(Some(encode_thread)),
                reconfig_tx, slot, backend, intra_requested, listeners,
                next_listener: std::sync::atomic::AtomicU64::new(1),
            }),
        })
    }

    /// Independent WebRTC/ICE/audio connection using the same captured and
    /// encoded video. No additional capture receiver or encoder is created.
    pub async fn fork(
        &self,
        ice_servers: Option<Vec<String>>,
        event_tx: mpsc::UnboundedSender<MediaEvent>,
        audio_rx: Option<std::sync::mpsc::Receiver<EncodedAudioPacket>>,
    ) -> Result<Self, MediaError> {
        if self.stopped.load(Ordering::Acquire) || self.shared.encode_stop.load(Ordering::Acquire) {
            return Err(MediaError::Closed);
        }
        let pc = Arc::new(build_api()?.new_peer_connection(rtc_config(ice_servers)).await
            .map_err(|e| MediaError::Transport(format!("pc: {e}")))?);
        let sender = pc.add_track(self.shared.track.clone()).await
            .map_err(|e| MediaError::Transport(format!("add_track: {e}")))?;
        let stopped = Arc::new(AtomicBool::new(false));
        if let Some(audio_rx) = audio_rx {
            let track = Arc::new(TrackLocalStaticSample::new(opus_codec(), "audio".into(), "golive".into()));
            pc.add_track(track.clone()).await.map_err(|e| MediaError::Transport(format!("add_audio_track: {e}")))?;
            let stop = stopped.clone();
            let runtime = tokio::runtime::Handle::current();
            tokio::task::spawn_blocking(move || {
                while !stop.load(Ordering::Acquire) {
                    let Ok(packet) = audio_rx.recv_timeout(Duration::from_millis(20)) else { continue };
                    let sample = Sample { data: Bytes::from(packet.data), duration: packet.duration, ..Default::default() };
                    let _ = runtime.block_on(track.write_sample(&sample));
                }
            });
        }
        for transceiver in pc.get_transceivers().await {
            transceiver.set_direction(RTCRtpTransceiverDirection::Sendonly).await;
        }
        wire_ice_events(&pc, &event_tx, &Arc::new(std::sync::Mutex::new(CandidateCensus::default())));
        let intra = self.shared.intra_requested.clone();
        let rtcp_task = tokio::spawn(async move {
            while let Ok((packets, _)) = sender.read_rtcp().await {
                if packets.iter().any(|packet| packet.as_any().is::<PictureLossIndication>() || packet.as_any().is::<FullIntraRequest>()) {
                    intra.store(true, Ordering::Release);
                }
            }
        });
        let listener = self.shared.next_listener.fetch_add(1, Ordering::Relaxed);
        self.shared.listeners.lock().unwrap().insert(listener, event_tx.clone());
        Ok(Self { pc, event_tx, stopped, listener, shared: self.shared.clone(), rtcp_task: Some(rtcp_task) })
    }

    /// Next encoded unit starts with an IDR. Used when a watcher joins after
    /// the startup keyframe has already left the output slot.
    pub fn request_keyframe(&self) {
        self.shared.intra_requested.store(true, Ordering::Release);
    }

    /// Live encoder backend for counters/diagnostics (`None` until the
    /// encode thread finishes its first build). Non-blocking read.
    pub fn backend(&self) -> Option<&'static str> {
        self.shared.backend.lock().ok().and_then(|guard| *guard)
    }

    /// Transactional reconfig without re-signaling: validates first (typed
    /// failure, current profile untouched), then hands to the encode thread,
    /// which rebuilds + forces IDR + bumps the generation fence. Same
    /// m-line, SPS in-band — WebRTC resolves it.
    pub fn reconfigure(&self, profile: QualityProfile) -> Result<(), MediaError> {
        profile
            .validate()
            .map_err(|e| MediaError::Codec(format!("profile: {e}")))?;
        if self.stopped.load(Ordering::Acquire) { return Err(MediaError::Closed); }
        self.shared.reconfig_tx.send(profile)
            .map_err(|_| MediaError::Closed)?;
        Ok(())
    }

    pub async fn create_offer(&self) -> Result<String, MediaError> {
        let offer = self
            .pc
            .create_offer(None)
            .await
            .map_err(|e| MediaError::Transport(format!("offer: {e}")))?;
        let sdp = offer.sdp.clone();
        self.pc
            .set_local_description(offer)
            .await
            .map_err(|e| MediaError::Transport(format!("set local: {e}")))?;
        Ok(sdp)
    }

    pub async fn set_remote_answer(&self, sdp: &str) -> Result<(), MediaError> {
        let answer = RTCSessionDescription::answer(sdp.to_owned())
            .map_err(|e| MediaError::Transport(format!("parse answer: {e}")))?;
        self.pc
            .set_remote_description(answer)
            .await
            .map_err(|e| MediaError::Transport(format!("set remote: {e}")))?;
        Ok(())
    }

    pub async fn add_remote_candidate(&self, candidate: &str) -> Result<(), MediaError> {
        add_ice_candidate_all_mlines(&self.pc, candidate).await
    }

    /// Bounded idempotent stop: flag, timed PC close, poison the pump so it
    /// cannot park in `take()` forever, abort the RTCP task, thread join.
    /// Never wedges.
    pub async fn stop(&mut self) {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = tokio::time::timeout(Duration::from_secs(5), self.pc.close()).await;
        if let Some(handle) = self.rtcp_task.take() { handle.abort(); }
        let last = {
            let mut listeners = self.shared.listeners.lock().unwrap();
            listeners.remove(&self.listener);
            listeners.is_empty()
        };
        if last {
            self.shared.encode_stop.store(true, Ordering::Release);
            self.shared.slot.poison();
            let thread = self.shared.encode_thread.lock().unwrap().take();
            if let Some(thread) = thread { let _ = tokio::task::spawn_blocking(move || thread.join()).await; }
        }
        let _ = self.event_tx.send(MediaEvent::Stats(MediaStats::default()));
    }
}

/// Encode loop: profile-paced frames, transactional reconfig, bounded
/// output. Wait for capacity before encode and drain stale raw capture frames.
/// Monotonic deadlines are reanchored after overruns, without catch-up bursts.
/// Reconfigs drain to latest and apply atomically
/// (rebuild + forced IDR + new generation + fresh Stats) without touching
/// signaling — WebRTC resolves it: same m-line, SPS in-band.
fn encode_loop(
    source: VideoSource,
    mut profile: QualityProfile,
    engine: EngineKind,
    stop: &AtomicBool,
    slot: &Arc<FrameSlot>,
    backend: &Arc<std::sync::Mutex<Option<&'static str>>>,
    census: &Arc<std::sync::Mutex<CandidateCensus>>,
    event_tx: &mpsc::UnboundedSender<MediaEvent>,
    reconfig_rx: &std::sync::mpsc::Receiver<QualityProfile>,
    intra_requested: &AtomicBool,
) {
    // 0. Movie preload (native size, capped — see preload_movie). The
    // bridge/setup failures below are fatal + loud (typed errors).
    let movie_frames: Option<Vec<I420Frame>> = match &source {
        VideoSource::MovieFile(path) => match preload_movie(path) {
            Ok(frames) => Some(frames),
            Err(e) => {
                let _ = event_tx.send(MediaEvent::Error(e.to_string()));
                return;
            }
        },
        VideoSource::SyntheticBall | VideoSource::External(_) => None,
    };
    // 1. Initial target + encoder (target computed inside from native dims).
    let (mut encoder, mut target) =
        match build_encoder(&profile, &source, engine, movie_frames.as_ref()) {
            Ok(built) => built,
            Err(e) => {
                let _ = event_tx.send(MediaEvent::Error(e.to_string()));
                return;
            }
        };
    note_backend(backend, encoder.backend_name());
    let mut generation: u64 = 0;
    // Retarget stability: maps the last frame-derived `needed` dims to the
    // dims they actually built, so converged sizes (software fit / hardware
    // step-down) never rebuild. Cleared on every applied reconfig.
    let mut retarget_cache: Option<((usize, usize), (usize, usize))> = None;
    let mut ext_last: Option<I420Frame> = None;
    let mut scaled = I420Frame { w: 0, h: 0, data: Vec::new() };
    let mut consecutive_skips: u32 = 0;
    let mut encode_trace = Trace::new(Stage::Encode);
    let mut source_trace = Trace::new(Stage::Source);
    let mut next_deadline = Instant::now();
    let mut n: u64 = 0;
    while !stop.load(Ordering::Acquire) {
        // 1. Drain reconfigs to latest; apply atomically.
        if let Some(next) = drain_reconfigs(reconfig_rx) {
            if next != profile {
                match apply_reconfig(&next, &source, movie_frames.as_ref(), engine) {
                    Ok((new_encoder, new_target)) => {
                        encoder = new_encoder;
                        target = new_target;
                        profile = next;
                        retarget_cache = None;
                        generation = generation.wrapping_add(1);
                        encoder.force_intra();
                        note_backend(backend, encoder.backend_name());
                        let _ = event_tx.send(MediaEvent::Stats(MediaStats {
                            census: census_snapshot(census),
                            generation,
                            encode_w: new_target.0,
                            encode_h: new_target.1,
                            ..Default::default()
                        }));
                    }
                    Err(e) => {
                        let _ = event_tx.send(MediaEvent::Error(e.to_string()));
                    }
                }
            }
        }
        // 1b. PLI/FIR recovery: the viewer's loss signal (set by the RTCP
        // task) forces the next unit to start with an IDR. Consumed once;
        // the normal path never sets it, so behavior there is unchanged.
        // Inbound FIR/PLI batches coalesce here, so one applied IDR may
        // answer several inbound requests — the trace counts applications.
        let intra_applied = intra_requested.swap(false, Ordering::Acquire) as u64;
        if intra_applied == 1 {
            encoder.force_intra();
        }
        // Wait before conversion/encode, preserving every compressed reference.
        // Once capacity returns, take the newest raw capture frame.
        if !slot.wait_for_capacity(stop) { break; }
        let now = Instant::now();
        if next_deadline > now { std::thread::sleep(next_deadline - now); }
        let frame_time = Instant::now();
        next_deadline += profile.frame_duration();
        if next_deadline <= frame_time { next_deadline = frame_time + profile.frame_duration(); }
        // 2. Fetch one frame at the current target dims. CPU frames scale
        // here; GPU buffers stay retained until the encode step routes
        // them (zero-copy submit or one conversion — see below).
        enum PendingFrame<'a> {
            Cpu(Cow<'a, I420Frame>),
            Gpu(GpuPixelBuffer),
        }
        let frame: PendingFrame = match &source {
            VideoSource::MovieFile(_) => {
                let frames = movie_frames.as_ref().expect("preloaded above");
                PendingFrame::Cpu(Cow::Borrowed(scale_frame_reusing(&frames[(n as usize) % frames.len()], target.0, target.1, &mut scaled)))
            }
            VideoSource::SyntheticBall => PendingFrame::Cpu(Cow::Owned(synthetic_frame(target.0, target.1, n))),
            VideoSource::External(ext) => match {
                let started = source_trace.start();
                let mut received = ext.rx.recv_timeout(EXT_TICK);
                let mut raw_dropped = 0;
                if received.is_ok() {
                    while let Ok(newest) = ext.rx.try_recv() {
                        received = Ok(newest);
                        raw_dropped += 1;
                    }
                }
                source_trace.record(TraceSample {
                    frames: received.is_ok() as u64,
                    dropped: raw_dropped,
                    gpu_frames: matches!(&received, Ok(ExternalFrame::Gpu(_))) as u64,
                    timeouts: matches!(&received, Err(std::sync::mpsc::RecvTimeoutError::Timeout)) as u64,
                    errors: matches!(&received, Err(std::sync::mpsc::RecvTimeoutError::Disconnected)) as u64,
                    target_fps: profile.fps, ..Default::default()
                }, started);
                received
            } {
                Ok(ExternalFrame::Cpu(frame)) => {
                    ext_last = Some(frame);
                    let needed = encode_target_for_frame(
                        ext_last.as_ref().unwrap().w,
                        ext_last.as_ref().unwrap().h,
                        &profile,
                    );
                    try_retarget_encoder(
                        &mut encoder, &mut target, &profile, engine,
                        &mut generation, &backend, &event_tx, census, needed,
                        &mut retarget_cache,
                    );
                    PendingFrame::Cpu(Cow::Borrowed(scale_frame_reusing(ext_last.as_ref().unwrap(), target.0, target.1, &mut scaled)))
                }
                // Retained GPU buffer: ownership moves into the encode
                // step (submit or convert); a drop there releases it.
                Ok(ExternalFrame::Gpu(gpu)) => {
                    let needed = encode_target_for_frame(gpu.w as usize, gpu.h as usize, &profile);
                    try_retarget_encoder(
                        &mut encoder, &mut target, &profile, engine,
                        &mut generation, &backend, &event_tx, census, needed,
                        &mut retarget_cache,
                    );
                    PendingFrame::Gpu(gpu)
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => match cpu_holdover_on_timeout(ext_last.as_ref()) {
                    Some(last) => PendingFrame::Cpu(Cow::Borrowed(scale_frame_reusing(last, target.0, target.1, &mut scaled))),
                    None => {
                        continue;
                    }
                },
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = event_tx.send(MediaEvent::Error(format!(
                        "screen source ended ({})",
                        ext.label
                    )));
                    break;
                }
            },
        };
        // 3. Encode; transient skips are capped, anything else is fatal+loud.
        let started = encode_trace.start();
        let cpu_start = started.and_then(|_| THREAD_CPU_CLOCK.get().and_then(|clock| clock()));
        let encoded = match frame {
            PendingFrame::Cpu(frame) => encoder.encode_frame(&frame),
            PendingFrame::Gpu(gpu) => encode_gpu_frame(&mut encoder, gpu, target),
        };
        let cpu_elapsed = cpu_start.zip(cpu_start.and_then(|_| THREAD_CPU_CLOCK.get().and_then(|clock| clock())))
            .map(|(start, end)| end.saturating_sub(start));
        if started.is_some() { encode_trace.record(TraceSample {
            cpu_work_us: cpu_elapsed.unwrap_or(0), cpu_samples: cpu_elapsed.is_some() as u64,
            max_cpu_work_us: cpu_elapsed.unwrap_or(0),
            frames: matches!(&encoded, Ok(Some(_))) as u64,
            bytes: encoded.as_ref().ok().and_then(|u| u.as_ref()).map(|u| u.len() as u64).unwrap_or(0),
            keyframes: encoded.as_ref().ok().and_then(|u| u.as_ref()).map(|u| contains_idr(u) as u64).unwrap_or(0),
            dropped: matches!(&encoded, Ok(None)) as u64, errors: encoded.is_err() as u64,
            intra_applied,
            width: target.0 as u32, height: target.1 as u32, target_fps: profile.fps,
            ..Default::default()
        }, started); }
        match encoded {
            Ok(Some(unit)) => {
                consecutive_skips = 0;
                // The host observes its own keyframes here (the sender side
                // never decodes): NAL type 5 in the produced access unit.
                if contains_idr(&unit) {
                    let _ = event_tx.send(MediaEvent::Keyframe);
                }
                slot.publish(unit, profile.frame_duration());
            }
            Ok(None) => {
                consecutive_skips += 1;
                if consecutive_skips > 30 {
                    let _ = event_tx.send(MediaEvent::Error("encoder dropping every frame".into()));
                    break;
                }
            }
            Err(e) => {
                let _ = event_tx.send(MediaEvent::Error(e.to_string()));
                break;
            }
        }
        n += 1;

    }
}

/// Publishes the live encoder backend for counters/diagnostics.
// Best-effort (a poisoned cell keeps its last value; readers use `None`).
fn note_backend(
    backend: &Arc<std::sync::Mutex<Option<&'static str>>>,
    name: &'static str,
) {
    if let Ok(mut guard) = backend.lock() {
        *guard = Some(name);
    }
}

/// Routes a retained GPU buffer: zero-copy submit when the encoder is
/// hardware at matching dims, else exactly one conversion at capture size
/// plus scale (same correct output — the documented fallback, still a
/// fraction of the old always-copy path).
#[cfg(target_os = "macos")]
fn encode_gpu_frame(
    encoder: &mut VideoEncoder,
    mut gpu: GpuPixelBuffer,
    target: (usize, usize),
) -> Result<Option<Vec<u8>>, MediaError> {
    if (gpu.w as usize, gpu.h as usize) != target {
        // Dims drift (reconfig in flight, or a source behind the profile):
        // convert + scale keeps the output correct instead of failing.
        let frame = gpu_to_i420(&gpu)?;
        drop(gpu); // release the surface as soon as pixels are out
        let frame = scale_frame(&frame, target.0, target.1);
        return encoder.encode_frame(&frame);
    }
    match encoder {
        VideoEncoder::Software(_) => {
            let frame = gpu_to_i420(&gpu)?;
            drop(gpu);
            let frame = scale_frame(&frame, target.0, target.1);
            encoder.encode_frame(&frame)
        }
        VideoEncoder::Hardware(enc) => {
            let raw = gpu.take(); // adopt the +1; Drop goes inert
            // SAFETY: adopted from a live +1 (handler retain); from_raw
            // reconstitutes exactly one owner, dropped at fn end.
            let pixel = std::ptr::NonNull::new(raw as *mut objc2_core_video::CVPixelBuffer)
                .map(|nn| unsafe {
                    objc2_core_foundation::CFRetained::from_raw(nn)
                });
            match pixel {
                Some(pixel) => enc.encode_cv_pixel_buffer(pixel),
                // Null adopt (cannot happen from the handler): transient
                // skip, capped upstream like any encoder drop.
                None => Ok(None),
            }
        }
    }
}

/// Off-macOS the GPU arm is unreachable (never constructed there); a loud
/// typed error surfaces a platform bug instead of wedging the share.
#[cfg(not(target_os = "macos"))]
fn encode_gpu_frame(
    _encoder: &mut VideoEncoder,
    gpu: GpuPixelBuffer,
    _target: (usize, usize),
) -> Result<Option<Vec<u8>>, MediaError> {
    drop(gpu);
    Err(MediaError::Codec("GPU frame off macOS".into()))
}

/// One conversion of a retained GPU buffer to I420 (fallback path only:
/// software backend or dims drift). Borrows the buffer (the handle keeps
/// the +1); stride-aware copy + platform conversion, all redacted errors.
#[cfg(target_os = "macos")]
fn gpu_to_i420(gpu: &GpuPixelBuffer) -> Result<I420Frame, MediaError> {
    use objc2_core_video::{
        kCVPixelFormatType_32BGRA, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
        CVPixelBufferUnlockBaseAddress,
    };
    let w = gpu.w as usize;
    let h = gpu.h as usize;
    if w < 2 || h < 2 || w > 8192 || h > 8192 || gpu.stride < w * 4 {
        return Err(MediaError::Codec("GPU frame dims out of range".into()));
    }
    // SAFETY: borrow only — the handle outlives this call, and the format
    // check below runs before any byte is touched.
    let pixel = unsafe { &*(gpu.as_ptr() as *const objc2_core_video::CVPixelBuffer) };
    if objc2_core_video::CVPixelBufferGetPixelFormatType(pixel) != kCVPixelFormatType_32BGRA {
        return Err(MediaError::Codec("GPU frame not BGRA".into()));
    }
    if unsafe { CVPixelBufferLockBaseAddress(pixel, CVPixelBufferLockFlags::ReadOnly) } != 0 {
        return Err(MediaError::Codec("GPU frame lock failed".into()));
    }
    let frame = (|| {
        let base = objc2_core_video::CVPixelBufferGetBaseAddress(pixel) as *const u8;
        if base.is_null() {
            return None;
        }
        let bytes = gpu
            .stride
            .checked_mul(h.saturating_sub(1))?
            .checked_add(w.checked_mul(4)?)?;
        // SAFETY: locked above, bounds checked, unlocked below.
        let src = unsafe { std::slice::from_raw_parts(base, bytes) };
        let mut data = vec![0u8; bytes];
        for (dst_row, src_row) in data
            .chunks_exact_mut(gpu.stride)
            .zip(src.chunks(gpu.stride))
            .take(h)
        {
            dst_row[..w * 4].copy_from_slice(&src_row[..w * 4]);
        }
        let bgra = golive_platform::BgraFrame {
            w: gpu.w,
            h: gpu.h,
            stride: gpu.stride,
            format: golive_platform::PixelFormat::Bgra8888,
            data,
        };
        let planar = golive_platform::bgra_to_i420(&bgra).ok()?;
        let mut out = Vec::with_capacity(w * h * 3 / 2);
        out.extend_from_slice(&planar.y);
        out.extend_from_slice(&planar.u);
        out.extend_from_slice(&planar.v);
        Some(I420Frame { w, h, data: out })
    })();
    unsafe { CVPixelBufferUnlockBaseAddress(pixel, CVPixelBufferLockFlags::ReadOnly) };
    frame.ok_or_else(|| MediaError::Codec("GPU frame convert failed".into()))
}

/// Native source dims for target resolution (synthetic generates at will;
/// movie reports its file; external is unknown until frames arrive).
fn native_dims(source: &VideoSource, profile: &QualityProfile, movie: Option<&Vec<I420Frame>>) -> (u32, u32) {
    match source {
        VideoSource::SyntheticBall => (profile.w, profile.h),
        VideoSource::MovieFile(_) => movie
            .and_then(|frames| frames.first())
            .map(|frame| (frame.w as u32, frame.h as u32))
            .unwrap_or((profile.w, profile.h)),
        VideoSource::External(_) => (profile.w, profile.h),
    }
}

fn encode_target_for_frame(src_w: usize, src_h: usize, profile: &QualityProfile) -> (usize, usize) {
    let (w, h) = profile.normalized_dims(src_w.max(2) as u32, src_h.max(2) as u32);
    (w.max(2) as usize, h.max(2) as usize)
}

fn try_retarget_encoder(
    encoder: &mut VideoEncoder,
    target: &mut (usize, usize),
    profile: &QualityProfile,
    engine: EngineKind,
    generation: &mut u64,
    backend: &Arc<std::sync::Mutex<Option<&'static str>>>,
    event_tx: &mpsc::UnboundedSender<MediaEvent>,
    census: &Arc<std::sync::Mutex<CandidateCensus>>,
    needed: (usize, usize),
    retarget_cache: &mut Option<((usize, usize), (usize, usize))>,
) {
    if needed == *target || needed.0 < 2 || needed.1 < 2 {
        return;
    }
    // Converged earlier: this `needed` already builds the live `target`
    // (software fit or hardware step-down swallow the difference). Rebuilding
    // here would swap an identical encoder while bumping the generation fence
    // and forcing an IDR on EVERY frame — the 5120x1440 session-log storm.
    if *retarget_cache == Some((needed, *target)) {
        return;
    }
    match build_best(profile, needed, engine) {
        Ok((new_encoder, new_dims)) => {
            *retarget_cache = Some((needed, new_dims));
            if new_dims == *target {
                return; // converged: keep the live encoder, no fence churn.
            }
            *target = new_dims;
            *encoder = new_encoder;
            *generation = generation.wrapping_add(1);
            encoder.force_intra();
            note_backend(backend, encoder.backend_name());
            let _ = event_tx.send(MediaEvent::Stats(MediaStats {
                census: census_snapshot(census),
                generation: *generation,
                encode_w: new_dims.0,
                encode_h: new_dims.1,
                ..Default::default()
            }));
        }
        Err(_) => {}
    }
}

fn initial_target(
    source: &VideoSource,
    profile: &QualityProfile,
    movie: Option<&Vec<I420Frame>>,
) -> (usize, usize) {
    let (sw, sh) = native_dims(source, profile, movie);
    encode_target_for_frame(sw as usize, sh as usize, profile)
}

/// Best-effort builder for the encode loop. Explicit engines keep exact
/// [`VideoEncoder::new`] semantics (Software fits internally, Hardware fails
/// hard when absent). Auto keeps probe-then-hardware-first but steps down
/// instead of collapsing straight to software: full size, then one
/// aspect-preserving step into the hardware box ([`fit_hardware_dims`]),
/// then software. Every step is a fixed point for its backend, and the
/// retarget cache pins the choice frame to frame, so oversize requests
/// converge (4096x1152 NVENC on 5120-wide-capable boxes, software fit
/// elsewhere) instead of rebuilding per frame.
fn build_best(
    profile: &QualityProfile,
    needed: (usize, usize),
    engine: EngineKind,
) -> Result<(VideoEncoder, (usize, usize)), MediaError> {
    if !matches!(engine, EngineKind::Auto) {
        let encoder = VideoEncoder::new(profile, needed.0, needed.1, engine)?;
        let dims = encoder.dims();
        return Ok((encoder, dims));
    }
    let (hw, _) = probe_cached();
    if !hw {
        let encoder = VideoEncoder::new(profile, needed.0, needed.1, EngineKind::Software)?;
        let dims = encoder.dims();
        return Ok((encoder, dims));
    }
    // Hardware first at full size (unchanged: VideoToolbox 5K, NVENC up to
    // the MFT width cap).
    match VideoEncoder::new(profile, needed.0, needed.1, EngineKind::Hardware) {
        Ok(encoder) => {
            let dims = encoder.dims();
            Ok((encoder, dims))
        }
        Err(_) => {
            let stepped = fit_hardware_dims(needed.0, needed.1);
            if stepped != needed {
                if let Ok(encoder) =
                    VideoEncoder::new(profile, stepped.0, stepped.1, EngineKind::Hardware)
                {
                    let dims = encoder.dims();
                    return Ok((encoder, dims));
                }
            }
            let encoder = VideoEncoder::new(profile, needed.0, needed.1, EngineKind::Software)?;
            let dims = encoder.dims();
            Ok((encoder, dims))
        }
    }
}

/// Build the encoder backend for a target. Software always works;
/// hardware is probed (Auto) or demanded (Hardware → hard fail).
/// Routed through [`build_best`] so oversize requests converge to the
/// strongest encodable size (hardware step-down, else software fit).
fn build_encoder(
    profile: &QualityProfile,
    source: &VideoSource,
    engine: EngineKind,
    movie: Option<&Vec<I420Frame>>,
) -> Result<(VideoEncoder, (usize, usize)), MediaError> {
    let needed = initial_target(source, profile, movie);
    build_best(profile, needed, engine)
}

/// Drain pending reconfigs, keeping only the latest (a burst of UI drags
/// applies once, not N times).
fn drain_reconfigs(
    reconfig_rx: &std::sync::mpsc::Receiver<QualityProfile>,
) -> Option<QualityProfile> {
    let mut last = None;
    while let Ok(profile) = reconfig_rx.try_recv() {
        last = Some(profile);
    }
    last
}

/// Rebuild for a new profile: fresh encoder at the new target + forced IDR
/// (applied by the caller right after) + SPS in-band on it. No SDP change.
fn apply_reconfig(
    next: &QualityProfile,
    source: &VideoSource,
    movie: Option<&Vec<I420Frame>>,
    engine: EngineKind,
) -> Result<(VideoEncoder, (usize, usize)), MediaError> {
    next.validate().map_err(|e| MediaError::Codec(format!("profile: {e}")))?;
    build_encoder(next, source, engine, movie)
}

/// Reads a movie file, decodes up to 300 frames at NATIVE size capped to
/// 1080p (memory-bounded; the loop scales per frame to the live target).
/// Native (not target) size is kept so reconfigs never upscale beyond the
/// file (see `normalize_dims`).
fn preload_movie(path: &PathBuf) -> Result<Vec<I420Frame>, MediaError> {
    let bytes =
        std::fs::read(path).map_err(|e| MediaError::Source(format!("read movie: {e}")))?;
    if bytes.len() > 256 * 1024 * 1024 {
        return Err(MediaError::Source("movie over 256 MiB".into()));
    }
    let mut decoder =
        Decoder::new().map_err(|e| MediaError::Codec(format!("movie decoder init: {e}")))?;
    let mut frames = Vec::new();
    let mut unit = Vec::<u8>::new();
    let mut unit_has_vcl = false;
    let flush = |unit: &mut Vec<u8>, unit_has_vcl: &mut bool, decoder: &mut Decoder, frames: &mut Vec<I420Frame>| -> Result<(), MediaError> {
        if unit.is_empty() {
            return Ok(());
        }
        if let Some(yuv) = decoder.decode(unit).map_err(|e| MediaError::Codec(format!("movie decode: {e}")))? {
            let [slice] = yuv.split::<1>();
            frames.push(contiguous_i420(&slice));
        }
        unit.clear();
        *unit_has_vcl = false;
        Ok(())
    };
    for range in annexb_nals(&bytes) {
        let nal = &bytes[range];
        let typ = nal_type(nal);
        let is_vcl = matches!(typ, Some(1) | Some(5));
        if is_vcl && unit_has_vcl {
            flush(&mut unit, &mut unit_has_vcl, &mut decoder, &mut frames)?;
            if frames.len() >= 300 {
                break;
            }
        }
        unit.extend_from_slice(nal);
        if is_vcl {
            unit_has_vcl = true;
        }
    }
    flush(&mut unit, &mut unit_has_vcl, &mut decoder, &mut frames)?;
    if frames.is_empty() {
        return Err(MediaError::Source("movie produced no frames".into()));
    }
    // Cap native size at 1080p (memory-bounded preload); the loop scales
    // per frame to the live target, never upscaling beyond this.
    Ok(frames
        .into_iter()
        .map(|f| {
            let (w, h) = normalize_dims(f.w as u32, f.h as u32, 1920, 1080);
            scale_frame(&f, w as usize, h as usize).into_owned()
        })
        .collect())
}

/// Copies a (possibly strided) decoded slice into a contiguous I420 frame.
fn contiguous_i420(slice: &openh264::formats::YUVSlices<'_>) -> I420Frame {
    let (w, h) = slice.dimensions();
    let (sy, su, sv) = slice.strides();
    let mut data = vec![0u8; w * h * 3 / 2];
    for row in 0..h {
        data[row * w..(row + 1) * w].copy_from_slice(&slice.y()[row * sy..row * sy + w]);
    }
    for row in 0..h / 2 {
        let (du, dv) = (w * h, w * h + w * h / 4);
        data[du + row * (w / 2)..du + (row + 1) * (w / 2)]
            .copy_from_slice(&slice.u()[row * su..row * su + w / 2]);
        data[dv + row * (w / 2)..dv + (row + 1) * (w / 2)]
            .copy_from_slice(&slice.v()[row * sv..row * sv + w / 2]);
    }
    I420Frame { w, h, data }
}

// ---------------------------------------------------------------------------
// Native viewer (receive + decode + validate + present callback)
// ---------------------------------------------------------------------------

pub struct NativeViewer {
    pc: Arc<RTCPeerConnection>,
    stopped: Arc<AtomicBool>,
}

impl NativeViewer {
    /// Starts the viewer side. `on_frame` is the present callback: it
    /// receives every decoded picture (owned pixels). The callback must
    /// never block — drop stale frames instead of queuing. Never carries
    /// secrets (pixels only).
    pub async fn start(
        ice_servers: Option<Vec<String>>,
        event_tx: mpsc::UnboundedSender<MediaEvent>,
        on_frame: Arc<dyn Fn(PresentedFrame) + Send + Sync>,
    ) -> Result<Self, MediaError> {
        Self::start_with_audio(ice_servers, event_tx, on_frame, None).await
    }

    pub async fn start_with_audio(
        ice_servers: Option<Vec<String>>,
        event_tx: mpsc::UnboundedSender<MediaEvent>,
        on_frame: Arc<dyn Fn(PresentedFrame) + Send + Sync>,
        on_audio: Option<Arc<dyn Fn(&[f32]) + Send + Sync>>,
    ) -> Result<Self, MediaError> {
        Self::start_with_audio_format(ice_servers, event_tx, on_frame, on_audio, false).await
    }

    pub async fn start_with_audio_format(
        ice_servers: Option<Vec<String>>,
        event_tx: mpsc::UnboundedSender<MediaEvent>,
        on_frame: Arc<dyn Fn(PresentedFrame) + Send + Sync>,
        on_audio: Option<Arc<dyn Fn(&[f32]) + Send + Sync>>,
        compact: bool,
    ) -> Result<Self, MediaError> {
        let api = build_api()?;
        let pc = Arc::new(
            api.new_peer_connection(rtc_config(ice_servers))
                .await
                .map_err(|e| MediaError::Transport(format!("pc: {e}")))?,
        );
        // Shared redacted ICE census (see publisher side).
        let census = Arc::new(std::sync::Mutex::new(CandidateCensus::default()));
        wire_ice_events(&pc, &event_tx, &census);
        {
            let event_tx = event_tx.clone();
            let census = Arc::clone(&census);
            let pc_pli = Arc::clone(&pc);
            pc.on_track(Box::new(move |track, _, _| {
                let event_tx = event_tx.clone();
                let on_frame = Arc::clone(&on_frame);
                let on_audio = on_audio.clone();
                let census = Arc::clone(&census);
                let pc_pli = Arc::clone(&pc_pli);
                Box::pin(async move {
                    // webrtc holds its on_track handler mutex until this future
                    // returns. A lifetime-long read here prevents the other
                    // media track from starting, depending on which arrives first.
                    // Closing the peer connection ends both RTP readers.
                    tokio::spawn(async move {
                        if track.kind() == RTPCodecType::Audio {
                            if let Some(on_audio) = on_audio {
                                audio_read_loop(track, on_audio).await;
                            }
                            return;
                        }
                        read_loop(track, &pc_pli, &event_tx, &on_frame, &census, compact).await;
                    });
                })
            }));
        }
        Ok(Self {
            pc,
            stopped: Arc::new(AtomicBool::new(false)),
        })
    }

    pub async fn set_remote_offer(&self, sdp: &str) -> Result<String, MediaError> {
        let offer = RTCSessionDescription::offer(sdp.to_owned())
            .map_err(|e| MediaError::Transport(format!("parse offer: {e}")))?;
        self.pc
            .set_remote_description(offer)
            .await
            .map_err(|e| MediaError::Transport(format!("set remote: {e}")))?;
        for transceiver in self.pc.get_transceivers().await {
            transceiver
                .set_direction(RTCRtpTransceiverDirection::Recvonly)
                .await;
        }
        let answer = self
            .pc
            .create_answer(None)
            .await
            .map_err(|e| MediaError::Transport(format!("answer: {e}")))?;
        let sdp = answer.sdp.clone();
        self.pc
            .set_local_description(answer)
            .await
            .map_err(|e| MediaError::Transport(format!("set local: {e}")))?;
        Ok(sdp)
    }

    pub async fn add_remote_candidate(&self, candidate: &str) -> Result<(), MediaError> {
        add_ice_candidate_all_mlines(&self.pc, candidate).await
    }

    /// Bounded idempotent stop. Never wedges.
    pub async fn stop(&mut self) {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = tokio::time::timeout(Duration::from_secs(5), self.pc.close()).await;
    }
}

/// Stale-AU decision for the viewer read loop: a completed access unit
/// older than the last DECODED RTP timestamp moves presentation backwards
/// (late/duplicate pre-switch unit decoded after newer frames, tripping
/// dims respawns) — drop it instead of decoding/presenting. Equal/newer
/// pass untouched; keyframes need no bypass (recovery IDRs always carry
/// newer timestamps) and the PLI path is separate (depacketize errors never
/// reach here). Wrapping-aware: RTP timestamps wrap every ~13h at 90kHz, so
/// "older" means more than half the u32 space behind, never a plain `<`
/// (which would discard the whole stream after one wrap).
fn au_is_stale(last_decoded_ts: Option<u32>, completed_ts: u32) -> bool {
    match last_decoded_ts {
        None => false,
        Some(prev) => completed_ts != prev && completed_ts.wrapping_sub(prev) > (u32::MAX >> 1),
    }
}

// ---------------------------------------------------------------------------
// Viewer micro jitter buffer (paced presentation, bounded extra latency)
// ---------------------------------------------------------------------------

/// Decoded pictures held between decode and `on_frame`. Arrival bursts and
/// single network gaps no longer hit the screen directly.
const PRESENT_BUFFER_MAX: usize = 2;
/// Nominal rhythm before the first RTP timestamp delta is observed. Only the
/// second picture of a stream can ever use it (the first presents at once,
/// the second already carries a delta); 30 fps is the safe middle.
const DEFAULT_PRESENT_INTERVAL: Duration = Duration::from_micros(1_000_000 / 30);
/// EWMA weight per observed arrival delta: ~8 frames to adapt, slow enough
/// to iron out source wobble, fast enough to follow a real rate change.
const INTERVAL_SMOOTHING_ALPHA: f64 = 0.125;
/// Phase correction per frame: chase drift gradually instead of jumping the
/// schedule at once. Well under one 60 fps interval, over timer slop.
const MAX_SLEW_PER_FRAME: Duration = Duration::from_micros(2_000);

/// Nominal frame interval from consecutive RTP timestamps (90 kHz clock).
/// `None` on same-timestamp repeats or gaps over a second — the caller keeps
/// its previous rhythm instead of scheduling nonsense.
fn rtp_frame_interval(prev_ts: u32, cur_ts: u32) -> Option<Duration> {
    let delta = cur_ts.wrapping_sub(prev_ts);
    if delta == 0 || delta > 90_000 {
        return None;
    }
    Some(Duration::from_micros(delta as u64 * 1_000_000 / 90_000))
}

/// Micro jitter buffer: at most [`PRESENT_BUFFER_MAX`] decoded pictures held
/// between decode and `on_frame`, presented on a smoothed clock (EWMA of the
/// RTP timestamp deltas), not the raw arrival rhythm.
///
/// The first picture presents immediately, without pre-roll. At a steady
/// rate the two slots schedule at most two intervals ahead (~33ms @60fps).
/// OS/IPC stalls are not bounded by this scheduling budget.
/// Arrivals past 2 held slots are refused (caller drops + counts, decoder
/// untouched — the drop is post-decode so the chain stays intact, no IDR
/// needed) — order never changes. Small phase errors are slewed out at
/// [`MAX_SLEW_PER_FRAME`] per frame; only holes bigger than the buffer can
/// absorb reanchor at once. Late wakeups keep only the newest due picture.
/// Scheduling is pure: the caller supplies `now` for deterministic tests.
struct PresentPacer {
    interval: Duration,
    observed: bool,
    slow_candidate: Option<(Duration, u8)>,
    queue: VecDeque<(DecodedPicture, Instant, Instant)>,
    last_due: Option<Instant>,
    dropped: u64,
}

// Test + diagnostics accessors below (len/is_empty/dropped) have no
// production caller yet: the loop drives off next_due/pop_due/push.
#[allow(dead_code)]
impl PresentPacer {
    fn new(interval: Duration) -> Self {
        Self {
            interval: interval.max(Duration::from_micros(1)),
            observed: false,
            slow_candidate: None,
            queue: VecDeque::new(),
            last_due: None,
            dropped: 0,
        }
    }

    fn set_interval(&mut self, interval: Duration) {
        // Sane presenter range (1 fps..1000 fps): garbage ts deltas must
        // never schedule a picture minutes out.
        self.interval = interval.max(Duration::from_millis(1)).min(Duration::from_secs(1));
    }

    /// Fold one arrival-delta sample into the smoothed clock. The first
    /// plausible sample replaces the seed; isolated long gaps are ignored.
    /// Later samples move the EWMA by [`INTERVAL_SMOOTHING_ALPHA`]. Callers pre-filter deltas
    /// (see `rtp_frame_interval`: 0 and >1s never reach here).
    fn observe_interval(&mut self, sample: Duration) {
        let sample = sample.max(Duration::from_millis(1)).min(Duration::from_secs(1));
        // RTP's 90kHz clock and microsecond rounding must not classify a
        // normal 60->30fps switch (33333 vs 2*16666us) as a gap.
        let gap_threshold = 2 * self.interval + Duration::from_millis(1);
        if !self.observed && sample <= gap_threshold {
            self.interval = sample;
            self.observed = true;
            self.slow_candidate = None;
            return;
        }
        // A single missing stretch is not a new frame rate. Accept a much
        // slower clock only after three similar deltas; normal 30<->60 fps
        // changes still use the EWMA. This adds no picture buffering.
        if sample > gap_threshold {
            let count = match self.slow_candidate {
                Some((previous, count)) if sample.abs_diff(previous) <= previous / 4 => count + 1,
                _ => 1,
            };
            self.slow_candidate = Some((sample, count));
            if count < 3 { return; }
            self.interval = sample;
            self.observed = true;
            self.slow_candidate = None;
            return;
        }
        self.slow_candidate = None;
        let current = self.interval.as_micros() as f64;
        let next = current + INTERVAL_SMOOTHING_ALPHA * (sample.as_micros() as f64 - current);
        self.interval = Duration::from_micros(next.round() as u64)
            .max(Duration::from_millis(1))
            .min(Duration::from_secs(1));
    }

    fn len(&self) -> usize {
        self.queue.len()
    }

    fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Hold one decoded picture for paced presentation. `false` = buffer was
    /// full: the arrival is refused (caller counts `dropped`; decoder and
    /// publisher need nothing — post-decode drop, chain intact).
    fn push(&mut self, picture: DecodedPicture, now: Instant) -> bool {
        if self.queue.len() >= PRESENT_BUFFER_MAX {
            self.dropped += 1;
            return false;
        }
        let due = match self.last_due {
            // First picture ever: present at once (no pre-roll).
            None => now,
            Some(last) => {
                let nominal = last.checked_add(self.interval).unwrap_or(now);
                if nominal >= now {
                    // Early/on-time arrival: hold the slot, wobble stops here.
                    nominal
                } else if now.duration_since(nominal) > 2 * self.interval {
                    // Hole bigger than the buffer can absorb: skip the debt,
                    // reanchor at once (same as the old full reanchor).
                    now
                } else {
                    // Small phase lag: chase at most one slew step per frame
                    // instead of jumping the whole schedule at once.
                    nominal + now.duration_since(nominal).min(MAX_SLEW_PER_FRAME)
                }
            }
        };
        self.last_due = Some(due);
        self.queue.push_back((picture, due, now));
        true
    }

    /// Next scheduled present (for the RTP read timeout). `None` when nothing
    /// is held — the reader blocks on the network instead.
    fn next_due(&self) -> Option<Instant> {
        self.queue.front().map(|(_, due, _)| *due)
    }

    /// Pop the front picture when its time has come, with the time it spent
    /// retained (`due - ready`, saturating, always >= 0). Call in a loop.
    fn pop_due(&mut self, now: Instant) -> Option<(DecodedPicture, u64)> {
        // After a scheduler stall, replaying every expired slot just replaces
        // pictures at the shell. Keep the freshest due picture instead.
        while self.queue.get(1).is_some_and(|(_, due, _)| *due <= now) {
            self.queue.pop_front();
            self.dropped += 1;
        }
        match self.queue.front() {
            Some((_, due, _)) if *due <= now => self.queue.pop_front().map(|(picture, due, ready)| {
                if self.queue.is_empty() && now.saturating_duration_since(due) >= self.interval {
                    self.last_due = Some(now);
                }
                (picture, due.saturating_duration_since(ready).as_micros() as u64)
            }),
            _ => None,
        }
    }

    fn drain_due(&mut self, emit: &mut impl FnMut(DecodedPicture, u64)) {
        while let Some((picture, hold_us)) = self.pop_due(Instant::now()) {
            emit(picture, hold_us);
        }
    }

    /// Keep the same future alive across deadlines: no cancelled RTP reads
    /// and no duplicate codec submissions. The codec remains serial.
    async fn wait<F: std::future::Future>(
        &mut self,
        future: F,
        emit: &mut impl FnMut(DecodedPicture, u64),
    ) -> F::Output {
        tokio::pin!(future);
        loop {
            self.drain_due(emit);
            let Some(due) = self.next_due() else { return future.await; };
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(due.into()) => {},
                result = &mut future => return result,
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum DecodeDelivery { Queued, PresentationDrop, Pending, RequestKeyframe }

/// Only a failure before presentation can request an IDR. Keeping the whole
/// decision here lets regression tests exercise the same policy as read_loop.
fn deliver_decoded(
    pacer: &mut PresentPacer,
    decoded: Result<Option<DecodedPicture>, MediaError>,
    waiting_for_keyframe: bool,
    has_decoded: bool,
    now: Instant,
) -> DecodeDelivery {
    match decoded {
        Ok(Some(picture)) => {
            if pacer.push(picture, now) { DecodeDelivery::Queued }
            else { DecodeDelivery::PresentationDrop }
        }
        Err(_) => DecodeDelivery::RequestKeyframe,
        Ok(None) if waiting_for_keyframe || !has_decoded => DecodeDelivery::RequestKeyframe,
        Ok(None) => DecodeDelivery::Pending,
    }
}

async fn audio_read_loop(
    track: Arc<TrackRemote>,
    on_audio: Arc<dyn Fn(&[f32]) + Send + Sync>,
) {
    let Ok(mut decoder) = opus::Decoder::new(48_000, opus::Channels::Stereo) else {
        return;
    };
    let mut pcm = vec![0f32; 960 * 2 * 6];
    loop {
        let (packet, _) = match track.read_rtp().await {
            Ok(pair) => pair,
            Err(_) => break,
        };
        match decoder.decode_float(&packet.payload, &mut pcm, false) {
            Ok(samples) if samples > 0 => {
                let end = (samples * 2).min(pcm.len());
                on_audio(&pcm[..end]);
            }
            _ => {}
        }
    }
}

/// Timestamp fallback for peers without markers; marked units can be released
/// without waiting for a future picture (RFC 6184 section 5.1).
#[derive(Default)]
struct VideoAssembly {
    unit: Vec<u8>,
    timestamp: Option<u32>,
    released: bool,
}

impl VideoAssembly {
    fn take(&mut self) -> Option<(u32, Vec<u8>)> {
        if self.unit.is_empty() {
            return None;
        }
        self.released = true;
        Some((self.timestamp?, std::mem::take(&mut self.unit)))
    }

    fn begin(&mut self, timestamp: u32) -> Option<(u32, Vec<u8>)> {
        let previous = if self.timestamp.is_some_and(|old| old != timestamp) {
            self.take()
        } else {
            None
        };
        if self.timestamp != Some(timestamp) {
            self.released = false;
        }
        self.timestamp = Some(timestamp);
        previous
    }

    fn append(&mut self, bytes: &[u8], marker: bool) -> Option<(u32, Vec<u8>)> {
        if self.released {
            return None;
        }
        self.unit.extend_from_slice(bytes);
        if marker { self.take() } else { None }
    }

    fn recycle(&mut self, mut bytes: Vec<u8>) {
        if self.unit.is_empty() && bytes.capacity() > self.unit.capacity() {
            bytes.clear();
            self.unit = bytes;
        }
    }
}

/// Track read loop: depacketize, assemble access units per RTP timestamp,
/// decode once, then validate + emit + present the same picture. On an
/// irrecoverable AU gap (the depacketize error path) asks the publisher for
/// an IDR via PLI — debounced ~1/s per SSRC, never per packet.
async fn read_loop(
    track: Arc<TrackRemote>,
    pc: &Arc<RTCPeerConnection>,
    event_tx: &mpsc::UnboundedSender<MediaEvent>,
    on_frame: &Arc<dyn Fn(PresentedFrame) + Send + Sync>,
    census: &Arc<std::sync::Mutex<CandidateCensus>>,
    compact: bool,
) {
    let mut depacketizer = H264Packet::default();
    // Live viewer: hardware DXVA where probed, transparent software
    // fallback otherwise (never black on probe failure).
    let mut decoder = match crate::worker::SerialWorker::start("golive-decode", move || {
        let mut decoder = H264Decoder::new_auto();
        move |(unit, measure): (Vec<u8>, bool)| {
            let mut measurements = DecodeMeasurements::default();
            let cpu_start = measure.then(|| THREAD_CPU_CLOCK.get().and_then(|clock| clock())).flatten();
            let picture = match decoder.as_mut() {
                Ok(decoder) => decoder.decode_measured(&unit, compact, measure.then_some(&mut measurements)),
                Err(error) => Err(MediaError::Codec(error.to_string())),
            };
            if let (Some(start), Some(end)) = (cpu_start, cpu_start.and_then(|_| THREAD_CPU_CLOCK.get().and_then(|clock| clock()))) {
                measurements.cpu_work_us = end.saturating_sub(start);
                measurements.cpu_samples = 1;
            }
            measurements.request_keyframe = decoder.as_ref().map(|decoder| decoder.waiting_for_keyframe()).unwrap_or(false);
            (unit, picture, measurements)
        }
    }) {
        Ok(worker) => worker,
        Err(e) => {
            let _ = event_tx.send(MediaEvent::Error(e.to_string()));
            return;
        }
    };
    let mut validator = FrameValidator::new();
    let mut stats = MediaStats::default();
    let mut assembly = VideoAssembly::default();
    let mut last_decoded_ts: Option<u32> = None;
    let mut last_au_ts: Option<u32> = None;
    let mut last_pli: HashMap<u32, Instant> = HashMap::new();
    let mut pacer = PresentPacer::new(DEFAULT_PRESENT_INTERVAL);
    let mut rtp_trace = Trace::new(Stage::Rtp);
    let mut decode_trace = Trace::new(Stage::Decode);
    let mut codec_trace = Trace::new(Stage::Codec);
    let mut convert_trace = Trace::new(Stage::Convert);
    let mut pacer_hold_trace = Trace::new(Stage::PacerHold);
    let mut dispatch_trace = Trace::new(Stage::Dispatch);
    let mut emit = |picture: DecodedPicture, hold_us: u64| {
        // Scheduled retention only (due - ready): excludes decode wait,
        // IPC, draw and ack. Disjoint from dispatch/present work below.
        pacer_hold_trace.record_cost(TraceSample { frames: 1, ..Default::default() }, hold_us);
        let frame = picture.stats;
        stats.frames_decoded += 1;
        if frame.is_keyframe {
            stats.keyframes_decoded += 1;
            let _ = event_tx.send(MediaEvent::Keyframe);
        }
        let non_black = FrameValidator::non_black(frame.luma_mean);
        let motion = validator.motion(frame.luma_mean);
        let _ = event_tx.send(MediaEvent::VideoFrame { non_black, motion });
        let started = dispatch_trace.start();
        on_frame(picture.frame);
        dispatch_trace.record(TraceSample { frames: 1, ..Default::default() }, started);
        if stats.frames_decoded % 30 == 0 {
            let mut snapshot = stats.clone();
            snapshot.census = census_snapshot(census);
            let _ = event_tx.send(MediaEvent::Stats(snapshot));
        }
    };
    let mut reported_pacer_drops = 0;
    loop {
        let (packet, _) = match pacer.wait(track.read_rtp(), &mut emit).await {
            Ok(pair) => pair,
            Err(_) => break,
        };
        {
            let ts = packet.header.timestamp;
            rtp_trace.record(TraceSample { frames: 1, bytes: packet.payload.len() as u64, ..Default::default() }, None);
            let previous = assembly.begin(ts);
            let current = match depacketizer.depacketize(&packet.payload) {
                Ok(bytes) => assembly.append(&bytes, packet.header.marker),
                Err(_) => {
                    let sent = maybe_send_pli(pc, &mut last_pli, packet.header.ssrc).await;
                    decode_trace.record(TraceSample {
                        pli_sent: sent as u64,
                        pli_suppressed: (!sent) as u64,
                        ..Default::default()
                    }, None);
                    None
                }
            };
            for (completed_ts, mut unit) in [previous, current].into_iter().flatten() {
                if au_is_stale(last_decoded_ts, completed_ts) {
                    // Late pre-switch duplicate: decoding it would move
                    // presentation backwards in time. Count it in the existing
                    // decode `dropped` sample; the PLI path already asked for
                    // (or will ask for) the IDR that replaces it.
                    decode_trace.record(TraceSample {
                        dropped: 1,
                        bytes: unit.len() as u64,
                        ..Default::default()
                    }, None);
                } else {
                    // Presenter rhythm follows a smoothed sender clock, not the
                    // raw arrival jitter: consecutive AU timestamps feed the
                    // pacer EWMA (0 and >1s deltas never reach it).
                    if let Some(prev) = last_au_ts {
                        if let Some(rhythm) = rtp_frame_interval(prev, completed_ts) {
                            pacer.observe_interval(rhythm);
                        }
                    }
                    last_au_ts = Some(completed_ts);
                    let started = decode_trace.start();
                    let (returned_unit, decoded, measurements) = match pacer.wait(decoder.run((unit, started.is_some())), &mut emit).await {
                        Ok(result) => result,
                        Err(error) => { let _ = event_tx.send(MediaEvent::Error(error.into())); return; }
                    };
                    unit = returned_unit;
                    if let Err(error) = &decoded {
                        let _ = event_tx.send(MediaEvent::Error(error.to_string()));
                    }
                    let picture = decoded.as_ref().ok().and_then(|p| p.as_ref());
                    if started.is_some() {
                        codec_trace.record_cost(TraceSample { frames: picture.is_some() as u64, ..Default::default() }, measurements.codec_us);
                        convert_trace.record_cost(TraceSample { frames: picture.is_some() as u64,
                            bytes: picture.map(|p| p.frame.data.len() as u64).unwrap_or(0), ..Default::default() }, measurements.convert_us);
                    }
                    decode_trace.record(TraceSample {
                        frames: picture.is_some() as u64, dropped: picture.is_none() as u64,
                        cpu_work_us: measurements.cpu_work_us, cpu_samples: measurements.cpu_samples,
                        max_cpu_work_us: measurements.cpu_work_us,
                        bytes: unit.len() as u64,
                        width: picture.map(|p| p.frame.w as u32).unwrap_or(0),
                        height: picture.map(|p| p.frame.h as u32).unwrap_or(0),
                        keyframes: picture.map(|p| p.stats.is_keyframe as u64).unwrap_or(0),
                        ..Default::default()
                    }, started);
                    let produced_picture = picture.is_some();
                    let delivery = deliver_decoded(&mut pacer, decoded, measurements.request_keyframe,
                        last_decoded_ts.is_some(), Instant::now());
                    if produced_picture { last_decoded_ts = Some(completed_ts); }
                    if delivery == DecodeDelivery::RequestKeyframe {
                        let sent = maybe_send_pli(pc, &mut last_pli, packet.header.ssrc).await;
                        decode_trace.record(TraceSample {
                            pli_sent: sent as u64,
                            pli_suppressed: (!sent) as u64,
                            ..Default::default()
                        }, None);
                    }
                } // end non-stale branch
                assembly.recycle(unit);
            }
        }
        pacer.drain_due(&mut emit);
        let dropped = pacer.dropped() - reported_pacer_drops;
        if dropped != 0 {
            decode_trace.record(TraceSample { dropped, ..Default::default() }, None);
            reported_pacer_drops = pacer.dropped();
        }
    }
}

/// Shared ICE wiring: trickle + redacted census + connected milestone.
/// Snapshot of the shared ICE census (lock briefly, clone small counts).
fn census_snapshot(cell: &Arc<std::sync::Mutex<CandidateCensus>>) -> CandidateCensus {
    cell.lock().map(|guard| guard.clone()).unwrap_or_default()
}

fn wire_ice_events(
    pc: &Arc<RTCPeerConnection>,
    event_tx: &mpsc::UnboundedSender<MediaEvent>,
    census: &Arc<std::sync::Mutex<CandidateCensus>>,
) {
    {
        let event_tx = event_tx.clone();
        let census = Arc::clone(census);
        let mut done = false;
        pc.on_ice_candidate(Box::new(move |candidate: Option<RTCIceCandidate>| {
            // Census updates under a short lock here (sync callback
            // context); only owned event data moves into async below.
            let outgoing = match candidate {
                Some(candidate) => {
                    // W3C toJSON form: full "candidate:..." line. (Display is
                    // a short human form and does NOT unmarshal remotely.)
                    match candidate.to_json() {
                        Ok(init) => {
                            if let Ok(mut guard) = census.lock() {
                                guard.add(&init.candidate);
                            }
                            Some(MediaEvent::IceCandidate {
                                candidate: init.candidate,
                            })
                        }
                        Err(e) => Some(MediaEvent::Error(format!("candidate: {e}"))),
                    }
                }
                None => {
                    if done {
                        None
                    } else {
                        done = true;
                        Some(MediaEvent::IceGatheringComplete)
                    }
                }
            };
            let event_tx = event_tx.clone();
            Box::pin(async move {
                if let Some(event) = outgoing {
                    let _ = event_tx.send(event);
                }
            })
        }));
    }
    {
        let event_tx = event_tx.clone();
        let connected_sent = Arc::new(AtomicBool::new(false));
        pc.on_ice_connection_state_change(Box::new(move |state: RTCIceConnectionState| {
            let event_tx = event_tx.clone();
            let connected_sent = Arc::clone(&connected_sent);
            Box::pin(async move {
                use RTCIceConnectionState::*;
                match state {
                    Connected | Completed => {
                        if !connected_sent.swap(true, Ordering::SeqCst) {
                            let _ = event_tx.send(MediaEvent::IceConnected);
                        }
                    }
                    Failed => {
                        let _ = event_tx.send(MediaEvent::IceFailed);
                    }
                    Disconnected | Closed => {
                        let _ = event_tx.send(MediaEvent::Error(format!("ice {state:?}")));
                    }
                    _ => {}
                }
            })
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frame_slot_reservation_never_replaces_queued_unit() {
        let slot = FrameSlot::default();
        let stop = AtomicBool::new(false);
        assert!(slot.wait_for_capacity(&stop));
        assert!(slot.publish(vec![1, 2, 3], Duration::from_millis(16)));
        assert!(!slot.publish(vec![9], Duration::from_millis(16)));
        let (unit, _) = slot.take().await;
        assert_eq!(unit, vec![1, 2, 3]);
        assert!(slot.wait_for_capacity(&stop));
    }

    #[tokio::test]
    async fn congestion_preserves_the_encoded_prediction_chain() {
        let profile = QualityProfile::custom(320, 180, 1000, 60).unwrap();
        let slot = Arc::new(FrameSlot::default());
        let stop = Arc::new(AtomicBool::new(false));
        let (events, _rx) = mpsc::unbounded_channel();
        let (_reconfig, configs) = std::sync::mpsc::channel();
        let output = slot.clone();
        let stopped = stop.clone();
        let handle = std::thread::spawn(move || encode_loop(
            VideoSource::SyntheticBall, profile, EngineKind::Software, &stopped, &output,
            &Arc::new(std::sync::Mutex::new(None)), &Arc::new(std::sync::Mutex::new(CandidateCensus::default())),
            &events, &configs, &AtomicBool::new(false),
        ));
        let mut reference = VideoEncoder::new(&profile, 320, 180, EngineKind::Software).unwrap();
        let mut decoder = H264Decoder::new().unwrap();
        let result = async {
            for n in 0..12 {
                if n == 3 { tokio::time::sleep(Duration::from_millis(150)).await; }
                let (unit, _) = tokio::time::timeout(Duration::from_secs(2), slot.take()).await.map_err(|_| "sender stalled")?;
                let expected = reference.encode_frame(&synthetic_frame(320, 180, n)).unwrap().unwrap();
                if unit != expected { return Err("encoded reference was skipped while output stalled"); }
                if decoder.decode(&unit).unwrap().is_none() { return Err("picture failed to decode"); }
            }
            Ok(())
        }.await;
        stop.store(true, Ordering::Release);
        slot.poison();
        handle.join().unwrap();
        assert_eq!(result, Ok(()));
        assert!(!slot.wait_for_capacity(&AtomicBool::new(false)), "closed source cannot restart");
        assert!(!slot.publish(vec![1], Duration::ZERO), "closed source rejects late publish");
    }

    #[test]
    fn matching_scale_and_encoder_view_borrow_pixels() {
        let frame = I420Frame { w: 4, h: 2, data: (0..12).collect() };
        let scaled = scale_frame(&frame, 4, 2);
        assert_eq!(scaled.data.as_ptr(), frame.data.as_ptr(), "identity scaling copies pixels");
        let yuv = frame.yuv_buffer();
        assert_eq!(yuv.y().as_ptr(), frame.data.as_ptr(), "encoder input copies pixels");
        assert_eq!(yuv.u(), &[8, 9]);
        assert_eq!(yuv.v(), &[10, 11]);
        let small = scale_frame(&frame, 2, 2);
        assert_eq!(small.data, [0, 2, 4, 6, 8, 10]);
    }

    #[test]
    fn resized_frames_reuse_scratch_without_stale_pixels() {
        let mut scratch = I420Frame { w: 0, h: 0, data: Vec::new() };
        let frame = I420Frame { w: 4, h: 2, data: (0..12).collect() };
        let first = scale_frame_reusing(&frame, 2, 2, &mut scratch);
        assert_eq!(first.data, [0, 2, 4, 6, 8, 10]);
        let allocation = first.data.as_ptr();
        let next = I420Frame { w: 4, h: 2, data: vec![42; 12] };
        let second = scale_frame_reusing(&next, 2, 2, &mut scratch);
        assert_eq!(second.data, [42; 6]);
        assert_eq!(second.data.as_ptr(), allocation);
        assert_eq!(scale_frame_reusing(&next, 4, 2, &mut scratch).data.as_ptr(), next.data.as_ptr());
    }

    #[test]
    fn idle_source_timeout_does_not_invent_black() {
        assert!(cpu_holdover_on_timeout(None).is_none());
        let last = I420Frame { w: 2, h: 2, data: vec![40; 6] };
        let held = cpu_holdover_on_timeout(Some(&last)).unwrap();
        assert_eq!(held.data.as_ptr(), last.data.as_ptr());
        assert_ne!(held.data[..4], I420Frame::black(2, 2).data[..4]);
    }

    #[test]
    fn contract_dims() {
        assert_eq!(Quality::P720.dims(), (1280, 720));
        assert_eq!(Quality::P1080.dims(), (1920, 1080));
    }

    #[test]
    fn external_source_injects_without_os() {
        // Tiny gray-ramp frames; the core scales to contract and encodes.
        // No OS, no platform crate: pure channel injection.
        fn gray(w: usize, h: usize, v: u8) -> I420Frame {
            let mut data = vec![0u8; w * h * 3 / 2];
            data[..w * h].fill(v);
            data[w * h..].fill(128);
            I420Frame { w, h, data }
        }
        let (ext_tx, ext_rx) = std::sync::mpsc::sync_channel::<ExternalFrame>(4);
        let source = VideoSource::External(ExternalSource {
            rx: ext_rx,
            label: "test-inject".into(),
        });
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<MediaEvent>();
        let slot = Arc::new(FrameSlot::default());
        let (_reconfig_tx, reconfig_rx) = std::sync::mpsc::channel::<QualityProfile>();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let stop_ = std::sync::Arc::clone(&stop);
        let slot_ = Arc::clone(&slot);
        let backend_ = Arc::new(std::sync::Mutex::new(None));
        let census_ = Arc::new(std::sync::Mutex::new(CandidateCensus::default()));
        let intra_ = Arc::new(AtomicBool::new(false));
        let handle = std::thread::spawn(move || {
            encode_loop(
                source,
                QualityProfile::medium(),
                EngineKind::Software,
                &stop_,
                &slot_,
                &backend_,
                &census_,
                &event_tx,
                &reconfig_rx,
                &intra_,
            );
        });
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let mut units = 0;
        for n in 0..3 {
            ext_tx.send(ExternalFrame::Cpu(gray(128, 96, 16 + n * 20))).unwrap();
            let got = rt.block_on(async {
                tokio::time::timeout(Duration::from_secs(10), slot.take()).await
            });
            let (unit, duration) = got.expect("unit arrives within 10s");
            assert!(!unit.is_empty(), "poison pill never counted");
            assert_eq!(duration, QualityProfile::medium().frame_duration());
            units += 1;
        }
        assert_eq!(units, 3);
        drop(ext_tx); // source gone: loop must end by itself, bounded
        // Loop exits on disconnect (bounded by the pacing tick + timeout).
        handle.join().expect("encode loop thread");
        let mut saw_gone = false;
        while let Ok(event) = event_rx.try_recv() {
            if let MediaEvent::Error(detail) = event {
                if detail.contains("test-inject") {
                    saw_gone = true;
                }
            }
        }
        assert!(saw_gone, "disconnect reported with source label");
    }

    #[test]
    fn oversize_external_frames_never_rebuild_per_frame() {
        // Regression: with an oversize profile on the software path the
        // encode target (fitted, e.g. 2000x1000 -> 1920x960) could never
        // equal the frame-derived `needed` (unfitted), so the retarget check
        // rebuilt the encoder, bumped the generation fence and forced an IDR
        // on EVERY frame (session-log storm ~5/s, fence churn drowning out
        // set_quality, 5120x1440 collapsing to a 1920x540 slideshow).
        // Steady frames at a fixed size must not move the generation at all.
        fn gray(w: usize, h: usize, v: u8) -> I420Frame {
            let mut data = vec![0u8; w * h * 3 / 2];
            data[..w * h].fill(v);
            data[w * h..].fill(128);
            I420Frame { w, h, data }
        }
        let profile = QualityProfile::custom(2000, 1000, 2000, 30).expect("profile");
        let (ext_tx, ext_rx) = std::sync::mpsc::sync_channel::<ExternalFrame>(8);
        let source = VideoSource::External(ExternalSource {
            rx: ext_rx,
            label: "storm-repro".into(),
        });
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<MediaEvent>();
        let slot = Arc::new(FrameSlot::default());
        let (_reconfig_tx, reconfig_rx) = std::sync::mpsc::channel::<QualityProfile>();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_ = Arc::clone(&stop);
        let slot_ = Arc::clone(&slot);
        let backend_ = Arc::new(std::sync::Mutex::new(None));
        let census_ = Arc::new(std::sync::Mutex::new(CandidateCensus::default()));
        let intra_ = Arc::new(AtomicBool::new(false));
        let handle = std::thread::spawn(move || {
            encode_loop(
                source,
                profile,
                EngineKind::Software,
                &stop_,
                &slot_,
                &backend_,
                &census_,
                &event_tx,
                &reconfig_rx,
                &intra_,
            );
        });
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let mut decoded_dims = None;
        let mut decoder = H264Decoder::new().expect("decoder");
        for take in 0..10 {
            ext_tx.send(ExternalFrame::Cpu(gray(2000, 1000, 16 + take as u8))).unwrap();
            let got = rt.block_on(async {
                tokio::time::timeout(Duration::from_secs(15), slot.take()).await
            });
            let (unit, _) = got.unwrap_or_else(|_| panic!("unit {take} arrives"));
            assert!(!unit.is_empty(), "poison pill never counted");
            if decoded_dims.is_none() {
                if let Some(picture) = decoder.decode(&unit).expect("decode") {
                    decoded_dims = Some((picture.frame.w, picture.frame.h));
                }
            }
        }
        drop(ext_tx);
        stop.store(true, Ordering::Release);
        handle.join().expect("encode loop thread");
        assert_eq!(decoded_dims, Some((1920, 960)), "software fit still caps output");
        let mut bumps = 0u32;
        let mut keyframes = 0u32;
        while let Ok(event) = event_rx.try_recv() {
            match event {
                MediaEvent::Stats(stats) if stats.generation > 0 => bumps += 1,
                MediaEvent::Keyframe => keyframes += 1,
                _ => {}
            }
        }
        assert_eq!(bumps, 0, "steady oversize frames must not bump the generation");
        assert!(keyframes <= 3, "no per-frame IDR storm (saw {keyframes})");
    }

    #[test]
    fn auto_external_oversize_converges_to_strongest_encodable() {
        // End-to-end loop with EngineKind::Auto, a 5120x1440 profile and
        // 5120x1440 injected frames (the 32:9 monitor case): the loop must
        // converge with zero generation churn and land the strongest
        // encodable size — hardware step-down (4096x1152 NVENC) where a GPU
        // encoder exists, software fit otherwise. Serialized with the HW
        // env-hook tests (process-global env mutation).
        let _guard = HW_ENV_LOCK.lock().expect("hw lock");
        fn gray(w: usize, h: usize, v: u8) -> I420Frame {
            let mut data = vec![0u8; w * h * 3 / 2];
            data[..w * h].fill(v);
            data[w * h..].fill(128);
            I420Frame { w, h, data }
        }
        let profile = QualityProfile::custom(5120, 1440, 10_000, 30).expect("profile");
        let (ext_tx, ext_rx) = std::sync::mpsc::sync_channel::<ExternalFrame>(8);
        let source = VideoSource::External(ExternalSource {
            rx: ext_rx,
            label: "auto-converge".into(),
        });
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<MediaEvent>();
        let slot = Arc::new(FrameSlot::default());
        let (_reconfig_tx, reconfig_rx) = std::sync::mpsc::channel::<QualityProfile>();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_ = Arc::clone(&stop);
        let slot_ = Arc::clone(&slot);
        let backend_ = Arc::new(std::sync::Mutex::new(None));
        let backend_probe = Arc::clone(&backend_);
        let census_ = Arc::new(std::sync::Mutex::new(CandidateCensus::default()));
        let intra_ = Arc::new(AtomicBool::new(false));
        let handle = std::thread::spawn(move || {
            encode_loop(
                source,
                profile,
                EngineKind::Auto,
                &stop_,
                &slot_,
                &backend_,
                &census_,
                &event_tx,
                &reconfig_rx,
                &intra_,
            );
        });
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        for take in 0..8 {
            ext_tx.send(ExternalFrame::Cpu(gray(5120, 1440, 16 + take as u8))).unwrap();
            let got = rt.block_on(async {
                tokio::time::timeout(Duration::from_secs(20), slot.take()).await
            });
            let (unit, _) = got.unwrap_or_else(|_| panic!("unit {take} arrives"));
            assert!(!unit.is_empty(), "poison pill never counted");
        }
        drop(ext_tx);
        stop.store(true, Ordering::Release);
        handle.join().expect("encode loop thread");
        let mut bumps = 0u32;
        while let Ok(event) = event_rx.try_recv() {
            if let MediaEvent::Stats(stats) = event {
                if stats.generation > 0 {
                    bumps += 1;
                }
            }
        }
        assert_eq!(bumps, 0, "auto loop must converge without per-frame rebuilds");
        let backend = backend_probe.lock().expect("backend cell").unwrap_or("none");
        if probe_hardware().is_ok() {
            assert_ne!(backend, "openh264", "hardware must carry the 5120-wide share");
        }
    }

    #[test]
    fn encoder_emits_annexb_idr_then_deltas() {
        let mut enc = H264Encoder::new(Quality::P720).expect("encoder");
        let mut saw_idr = false;
        for n in 0..5 {
            let frame = synthetic_frame(1280, 720, n);
            let unit = enc.encode(&frame).expect("encode");
            // Annex-B start code present; NALs individually parseable.
            assert!(unit.starts_with(&[0, 0, 0, 1]), "annex-b start");
            assert!(!annexb_nals(&unit).is_empty(), "units split");
            for range in annexb_nals(&unit) {
                if nal_type(&unit[range]) == Some(5) {
                    saw_idr = true;
                }
            }
        }
        assert!(saw_idr, "first access unit carries an IDR");
    }

    #[test]
    fn viewer_releases_marked_picture_without_next_timestamp() {
        use webrtc::rtp::codecs::h264::H264Payloader;
        use webrtc::rtp::packetizer::Payloader;
        let mut encoder = H264Encoder::new(Quality::P720).unwrap();
        let encoded = encoder.encode(&synthetic_frame(1280, 720, 0)).unwrap();
        let packets = H264Payloader::default().payload(500, &Bytes::from(encoded)).unwrap();
        assert!(packets.len() > 2, "exercise fragmented IDR and parameter sets");
        let mut depacketizer = H264Packet::default();
        let mut assembly = VideoAssembly::default();
        let mut picture = None;
        for (i, payload) in packets.iter().enumerate() {
            assert!(assembly.begin(9000).is_none());
            let bytes = depacketizer.depacketize(payload).unwrap();
            let completed = assembly.append(&bytes, i + 1 == packets.len());
            if i + 1 < packets.len() { assert!(completed.is_none()); }
            if completed.is_some() { picture = completed; }
        }
        let (timestamp, bytes) = picture.expect("last RTP packet must release the picture while the source is idle");
        assert_eq!(timestamp, 9000);
        assert!(assembly.begin(9000).is_none());
        assert!(assembly.append(&bytes, true).is_none(), "duplicate marker cannot replay a picture");
        let picture = H264Decoder::new().unwrap().decode(&bytes).unwrap().unwrap();
        assert_eq!((picture.frame.w, picture.frame.h), (1280, 720));
        assert!(assembly.begin(12000).is_none(), "marked picture cannot replay on rollover");
    }

    #[test]
    fn viewer_keeps_timestamp_fallback_without_marker() {
        let mut assembly = VideoAssembly::default();
        assert!(assembly.begin(u32::MAX - 100).is_none());
        assert!(assembly.append(&[1, 2, 3], false).is_none());
        assert!(assembly.begin(u32::MAX - 100).is_none());
        assert!(assembly.append(&[4, 5], false).is_none());
        assert_eq!(assembly.begin(200), Some((u32::MAX - 100, vec![1, 2, 3, 4, 5])));
        assert!(assembly.begin(300).is_none());
    }

    #[test]
    fn compact_viewer_does_not_expand_decoded_pixels_to_rgba() {
        let mut encoder = H264Encoder::new(Quality::P720).unwrap();
        let encoded = encoder.encode(&synthetic_frame(1280, 720, 0)).unwrap();
        let picture = H264Decoder::new().unwrap().decode_for_present(&encoded, true).unwrap().unwrap();
        assert_eq!(picture.frame.data.len(), 1280 * 720 * 3 / 2,
            "compact presentation must transport YUV, not an RGBA expansion");
    }

    #[test]
    fn host_keyframe_detector_matches_decoder() {
        // What the host's encode loop reports must agree with IDR presence.
        let mut enc = H264Encoder::new(Quality::P720).expect("encoder");
        let first = enc.encode(&synthetic_frame(1280, 720, 0)).expect("encode");
        assert!(contains_idr(&first), "first unit is an IDR");
        assert!(!contains_idr(&[]), "empty unit has no IDR");
    }

    // -- PLI loss recovery ---------------------------------------------------

    #[test]
    fn video_codec_advertises_pli_recovery() {
        // Negotiation half of recovery: the track codec mirrors the
        // media-engine defaults (nack + nack pli + ccm fir) so the offer
        // carries `a=rtcp-fb:<pt> nack pli`.
        let codec = video_codec();
        let feedback: Vec<(&str, &str)> = codec
            .rtcp_feedback
            .iter()
            .map(|fb| (fb.typ.as_str(), fb.parameter.as_str()))
            .collect();
        assert!(feedback.contains(&("nack", "")), "generic NACK present");
        assert!(feedback.contains(&("nack", "pli")), "PLI present");
        assert!(feedback.contains(&("ccm", "fir")), "FIR present");
    }

    #[test]
    fn pli_debounce_allows_one_per_second_per_ssrc() {
        // Pure decision with caller-owned time: first sighting per SSRC is
        // due, repeats within the window are not, other SSRCs are unaffected.
        let mut last: HashMap<u32, Instant> = HashMap::new();
        let t0 = Instant::now();
        assert!(pli_due(&mut last, 11, t0), "first gap asks");
        assert!(!pli_due(&mut last, 11, t0), "never per packet");
        assert!(
            !pli_due(&mut last, 11, t0 + PLI_DEBOUNCE - Duration::from_millis(1)),
            "still debounced just before the window"
        );
        assert!(pli_due(&mut last, 11, t0 + PLI_DEBOUNCE), "window re-arms");
        assert!(pli_due(&mut last, 22, t0), "other SSRC independent");
    }

    #[test]
    fn pli_gap_storm_sends_once_per_second() {
        // Simulated gap storm: dozens of irrecoverable AU gaps inside one
        // debounce window must emit exactly 1 PLI; the rest is suppressed
        // (numeric counters only — the trace records both sides).
        let mut last: HashMap<u32, Instant> = HashMap::new();
        let t0 = Instant::now();
        let mut sent = 0u64;
        let mut suppressed = 0u64;
        for _ in 0..50 {
            if pli_due(&mut last, 7, t0) {
                sent += 1;
            } else {
                suppressed += 1;
            }
        }
        assert_eq!(sent, 1, "one PLI per storm");
        assert_eq!(suppressed, 49, "rest suppressed by debounce");
        let sample = TraceSample {
            pli_sent: sent,
            pli_suppressed: suppressed,
            ..Default::default()
        };
        assert_eq!((sample.pli_sent, sample.pli_suppressed), (1, 49));
        // Next window re-arms: recovery is delayed, never lost.
        assert!(pli_due(&mut last, 7, t0 + PLI_DEBOUNCE), "window re-arms");
    }

    #[test]
    fn stale_au_drops_time_travel_passes_normal_flow() {
        // Pure decision behind the read_loop stale guard: a completed AU
        // older than the last decoded timestamp is dropped (late pre-switch
        // duplicate); everything else decodes untouched. No decoder needed —
        // the read_loop wiring just calls this and counts `dropped`.
        assert!(!au_is_stale(None, 0), "first AU always passes");
        assert!(!au_is_stale(None, u32::MAX), "first AU always passes");
        assert!(!au_is_stale(Some(3000), 3000), "equal timestamps pass");
        assert!(!au_is_stale(Some(3000), 3001), "newer timestamps pass");
        assert!(!au_is_stale(Some(3000), 6000), "post-switch jump passes");
        assert!(au_is_stale(Some(6000), 3000), "older AU drops");
        assert!(au_is_stale(Some(6000), 5999), "one tick back still drops");
        // RTP wrap (~13h at 90kHz) must not nuke the stream: just-wrapped
        // timestamps are newer, just-about-to-wrap are older.
        assert!(!au_is_stale(Some(u32::MAX), 5), "post-wrap passes");
        assert!(au_is_stale(Some(5), u32::MAX - 5), "pre-wrap straggler drops");
    }

    fn pacer_picture(id: u8) -> DecodedPicture {
        DecodedPicture {
            stats: DecodedStats { w: 2, h: 2, luma_mean: f64::from(id), is_keyframe: false },
            frame: PresentedFrame { w: 2, h: 2, data: vec![id], format: PixelFormat::I420 },
        }
    }

    #[test]
    fn rtp_frame_interval_maps_clock_ticks() {
        // 90 kHz timestamps → profile rhythm: 1500 ticks is 60 fps.
        assert_eq!(rtp_frame_interval(0, 1500), Some(Duration::from_micros(16_666)));
        assert_eq!(rtp_frame_interval(1500, 4500), Some(Duration::from_micros(33_333)));
        assert_eq!(rtp_frame_interval(7, 7), None, "same timestamp keeps rhythm");
        assert_eq!(rtp_frame_interval(0, 90_001), None, ">1s gap keeps rhythm");
        assert_eq!(
            rtp_frame_interval(u32::MAX - 749, 750),
            Some(Duration::from_micros(16_666)),
            "wrap-around delta still maps"
        );
    }

    #[test]
    fn present_pacer_passes_healthy_stream_without_delay() {
        // Low-watermark: a lone picture is due at once — a good network
        // gains ~0 added latency and the buffer sits empty.
        let mut pacer = PresentPacer::new(Duration::from_micros(16_666));
        let t0 = Instant::now();
        assert!(pacer.is_empty());
        assert!(pacer.push(pacer_picture(1), t0));
        assert_eq!(pacer.len(), 1);
        assert_eq!(pacer.next_due(), Some(t0), "lone picture due immediately");
        let (out, hold_us) = pacer.pop_due(t0).expect("due now");
        assert_eq!(out.frame.data[0], 1);
        assert_eq!(hold_us, 0, "lone picture is never retained");
        assert!(pacer.pop_due(t0).is_none());
        assert!(pacer.is_empty());
        assert_eq!(pacer.dropped(), 0);
        assert_eq!(pacer.next_due(), None, "empty buffer blocks on network");
        for n in 1..=300 {
            let now = t0 + Duration::from_micros(n * 16_666);
            pacer.push(pacer_picture(n as u8), now);
            let (_, hold_us) = pacer.pop_due(now).expect("steady 60fps must never wait");
            assert_eq!(hold_us, 0);
        }
    }

    #[test]
    fn present_pacer_smooths_burst_keeps_order_bounds_latency() {
        // 60 fps rhythm, arrivals outpacing presentation across iterations:
        // A presents at once, B and C space one interval apart, D past 2
        // held slots is refused (counted, never reordered, never early).
        let interval = Duration::from_micros(16_666);
        let mut pacer = PresentPacer::new(interval);
        let t0 = Instant::now();
        assert!(pacer.push(pacer_picture(1), t0));
        assert_eq!(pacer.next_due(), Some(t0), "first due at once");
        let (first, hold_us) = pacer.pop_due(t0).expect("first due at once");
        assert_eq!(first.frame.data[0], 1);
        assert_eq!(hold_us, 0);
        assert!(pacer.push(pacer_picture(2), t0));
        assert_eq!(pacer.next_due(), Some(t0 + interval), "second one interval out");
        assert!(pacer.push(pacer_picture(3), t0));
        assert_eq!(pacer.next_due(), Some(t0 + interval), "front still second");
        assert!(!pacer.push(pacer_picture(4), t0), "past 2 held slots drops");
        assert_eq!(pacer.dropped(), 1);
        assert_eq!(pacer.len(), 2, "refused arrival is not queued");
        assert!(pacer.pop_due(t0).is_none(), "held pictures wait their turn");
        let (second, hold_us) = pacer.pop_due(t0 + interval).expect("second due");
        assert_eq!(second.frame.data[0], 2);
        assert_eq!(hold_us, 16_666, "one scheduled interval of retention");
        // The third was scheduled two intervals out: the full buffer adds at
        // most 2 intervals (~33.3ms @60fps) of extra latency.
        assert_eq!(pacer.next_due(), Some(t0 + 2 * interval), "third two intervals out");
        assert!(pacer.pop_due(t0 + 2 * interval - Duration::from_micros(1)).is_none());
        let (third, hold_us) = pacer.pop_due(t0 + 2 * interval).expect("third due");
        assert_eq!(third.frame.data[0], 3);
        assert_eq!(hold_us, 33_332, "two scheduled intervals of retention");
        assert!(pacer.is_empty());
    }

    #[test]
    fn present_pacer_reanchors_after_gap_without_burst_debt() {
        // After a 10 s gap the next picture is due at once — the missed
        // slots are skipped, never caught up as a burst.
        let interval = Duration::from_micros(16_666);
        let mut pacer = PresentPacer::new(interval);
        let t0 = Instant::now();
        pacer.push(pacer_picture(1), t0);
        assert!(pacer.pop_due(t0).is_some());
        let late = t0 + Duration::from_secs(10);
        assert!(pacer.push(pacer_picture(2), late));
        assert_eq!(pacer.next_due(), Some(late), "overdue reanchors to now");
        assert!(pacer.pop_due(late).is_some());
        // Interval updates stay in the sane presenter range (1ms..1s): a
        // same-instant arrival after clamping to 1s waits one second.
        pacer.set_interval(Duration::from_secs(10));
        assert!(pacer.push(pacer_picture(3), late));
        assert_eq!(pacer.next_due(), Some(late + Duration::from_secs(1)), "clamped to 1s");
        assert!(pacer.pop_due(late).is_none());
        assert!(pacer.pop_due(late + Duration::from_secs(1)).is_some());
    }

    #[test]
    fn present_pacer_pause_does_not_slow_resumed_stream() {
        let interval = Duration::from_micros(16_667);
        let mut pacer = PresentPacer::new(interval);
        let t = Instant::now();
        pacer.observe_interval(interval);
        pacer.push(pacer_picture(0), t);
        pacer.pop_due(t);
        let resumed = t + Duration::from_millis(500);
        pacer.observe_interval(Duration::from_millis(500));
        pacer.push(pacer_picture(1), resumed);
        assert!(pacer.pop_due(resumed).is_some());
        pacer.observe_interval(interval);
        pacer.push(pacer_picture(2), resumed + interval);
        assert!(pacer.pop_due(resumed + interval).is_some(), "pause must not add delay after resuming");
        let mut fresh = PresentPacer::new(DEFAULT_PRESENT_INTERVAL);
        fresh.observe_interval(Duration::from_millis(500));
        assert_eq!(fresh.interval, DEFAULT_PRESENT_INTERVAL, "even the first delta can be a join pause");
        fresh.observe_interval(interval);
        assert_eq!(fresh.interval, interval);
    }

    #[test]
    fn present_pacer_late_wakeup_keeps_only_latest_due_picture() {
        let interval = Duration::from_micros(16_667);
        let mut pacer = PresentPacer::new(interval);
        let t = Instant::now();
        pacer.push(pacer_picture(0), t);
        pacer.pop_due(t);
        pacer.push(pacer_picture(1), t);
        pacer.push(pacer_picture(2), t);
        let resumed = t + Duration::from_millis(100);
        let (picture, _) = pacer.pop_due(resumed).unwrap();
        assert_eq!(picture.frame.data[0], 2, "do not replay stale pictures after a stall");
        assert!(pacer.pop_due(resumed).is_none());
        assert_eq!(pacer.dropped(), 1);
        pacer.push(pacer_picture(3), resumed);
        assert_eq!(pacer.next_due(), Some(resumed + interval));
    }

    #[test]
    fn pacer_overflow_drop_leaves_decoder_chain_intact() {
        // Post-decode presentation drop is not a loss: the decoder already
        // advanced its references, so refusing a pacer arrival must neither
        // arm recovery nor ask for a keyframe — the next delta still pictures.
        let mut enc = H264Encoder::new(Quality::P720).expect("encoder");
        let mut dec = H264Decoder::new().expect("decoder");
        let mut pacer = PresentPacer::new(Duration::from_micros(33_333));
        let t0 = Instant::now();
        let unit = enc.encode(&synthetic_frame(1280, 720, 0)).expect("priming IDR");
        assert!(contains_idr(&unit), "stream starts on an IDR");
        let picture = dec.decode(&unit).expect("decode").expect("IDR pictures");
        assert!(pacer.push(picture, t0));
        // Fill both slots, then overflow: the refused picture is counted by
        // the caller (decode.dropped) while the decoder is never touched.
        let unit = enc.encode(&synthetic_frame(1280, 720, 1)).expect("delta");
        assert!(!contains_idr(&unit), "test drives deltas, not IDRs");
        let picture = dec.decode(&unit).expect("decode").expect("delta pictures");
        assert!(pacer.push(picture, t0));
        let unit = enc.encode(&synthetic_frame(1280, 720, 2)).expect("delta");
        let picture = dec.decode(&unit).expect("decode").expect("delta pictures");
        assert_eq!(deliver_decoded(&mut pacer, Ok(Some(picture)), false, true, t0),
            DecodeDelivery::PresentationDrop, "production overflow policy never requests PLI");
        assert_eq!(pacer.dropped(), 1);
        assert!(!dec.waiting_for_keyframe(), "post-decode drop arms no recovery");
        assert_eq!(dec.backend_name(), "openh264", "no backend switch");
        // The chain continues on deltas alone: no IDR, no PLI needed.
        for n in 3..6 {
            let unit = enc.encode(&synthetic_frame(1280, 720, n)).expect("encode");
            assert!(!contains_idr(&unit), "still deltas, no forced IDR");
            let picture = dec.decode(&unit).expect("decode").expect("chain intact");
            assert!(!dec.waiting_for_keyframe());
            let _ = picture;
        }
    }

    #[test]
    fn decoder_error_after_first_picture_requests_keyframe() {
        let mut pacer = PresentPacer::new(DEFAULT_PRESENT_INTERVAL);
        let now = Instant::now();
        assert_eq!(deliver_decoded(&mut pacer, Err(MediaError::Codec("injected".into())), false, true, now),
            DecodeDelivery::RequestKeyframe);
        assert_eq!(deliver_decoded(&mut pacer, Ok(None), false, true, now), DecodeDelivery::Pending);
        assert_eq!(deliver_decoded(&mut pacer, Ok(None), true, true, now), DecodeDelivery::RequestKeyframe);
        assert_eq!(deliver_decoded(&mut pacer, Ok(None), false, false, now), DecodeDelivery::RequestKeyframe);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn present_pacer_releases_while_serial_decoder_is_blocked() {
        let (release, wait) = std::sync::mpsc::channel();
        let mut worker = crate::worker::SerialWorker::start("pacer-test-decode", move || move |()| {
            wait.recv_timeout(Duration::from_secs(2)).expect("presentation must run before decode finishes")
        }).unwrap();
        let mut pacer = PresentPacer::new(Duration::from_millis(16));
        let now = Instant::now();
        pacer.push(pacer_picture(0), now);
        pacer.pop_due(now);
        pacer.push(pacer_picture(1), now);
        let mut presented = 0;
        let result = tokio::time::timeout(Duration::from_secs(1), pacer.wait(worker.run(()), &mut |picture, _| {
            assert_eq!(picture.frame.data[0], 1);
            presented += 1;
            release.send(42).unwrap();
        })).await.expect("no dependency on decoder completion").unwrap();
        assert_eq!(result, 42);
        assert_eq!(presented, 1);
    }

    #[test]
    fn present_pacer_tracks_sustained_rate_changes() {
        let mut pacer = PresentPacer::new(Duration::from_micros(16_667));
        pacer.observe_interval(Duration::from_micros(16_667));
        for sample in [33_333, 16_667, 100_000, 16_667] {
            for _ in 0..80 { pacer.observe_interval(Duration::from_micros(sample)); }
            assert!(pacer.interval.abs_diff(Duration::from_micros(sample)) < Duration::from_micros(10));
        }
    }

    #[test]
    fn pacer_ewma_first_sample_wins_then_tracks() {
        // Seed is only a guess: the first real delta sets the clock outright,
        // later ones move it by alpha (0.125), always clamped 1ms..1s.
        let mut pacer = PresentPacer::new(Duration::from_micros(16_666));
        pacer.observe_interval(Duration::from_micros(33_333));
        assert_eq!(pacer.interval, Duration::from_micros(33_333), "first sample wins");
        pacer.observe_interval(Duration::from_micros(33_333));
        assert_eq!(pacer.interval, Duration::from_micros(33_333), "steady rate holds");
        pacer.observe_interval(Duration::from_micros(16_666));
        assert_eq!(pacer.interval, Duration::from_micros(31_250), "one alpha step down");
        pacer.observe_interval(Duration::from_secs(10));
        assert!(pacer.interval <= Duration::from_secs(1), "clamped");
        pacer.observe_interval(Duration::from_micros(0));
        assert!(pacer.interval >= Duration::from_millis(1), "clamped");
    }

    #[test]
    fn present_pacer_irons_arrival_wobble_without_phase_jumps() {
        // ±8ms arrival wobble around 16.7ms with an exact clock: releases must
        // leave on the regular grid (output jitter ≪ input jitter), in order,
        // with nothing dropped and no schedule jump bigger than one slew step.
        let interval = Duration::from_micros(16_666);
        let mut pacer = PresentPacer::new(interval);
        let t0 = Instant::now();
        assert!(pacer.push(pacer_picture(0), t0));
        let mut arrivals = vec![t0];
        for (k, wobble) in [8_000i64, -8_000, 8_000, -8_000, 8_000, -8_000, 8_000, -8_000]
            .iter()
            .enumerate()
        {
            let grid = t0 + Duration::from_micros((k as u64 + 1) * 16_666);
            arrivals.push(if *wobble >= 0 {
                grid + Duration::from_micros(*wobble as u64)
            } else {
                grid - Duration::from_micros((-*wobble) as u64)
            });
        }
        let mut releases: Vec<(u8, Instant)> = Vec::new();
        for (id, &at) in arrivals.iter().enumerate().skip(1) {
            while let Some(due) = pacer.next_due() {
                if due > at {
                    break;
                }
                let (pic, _) = pacer.pop_due(due).expect("due reached");
                releases.push((pic.frame.data[0], due));
            }
            assert!(pacer.push(pacer_picture(id as u8), at), "wobble must not overflow");
        }
        while let Some(due) = pacer.next_due() {
            let (pic, _) = pacer.pop_due(due).expect("flush");
            releases.push((pic.frame.data[0], due));
        }
        // Anchor released at once, then everything in arrival order.
        assert_eq!(releases[0].0, 0);
        for w in releases.windows(2) {
            assert_eq!(w[1].0, w[0].0 + 1, "order never changes");
        }
        // Input swings wildly, output stays on grid within one slew step.
        let in_gaps: Vec<i64> = arrivals.windows(2)
            .map(|w| w[1].duration_since(w[0]).as_micros() as i64)
            .collect();
        let in_spread = in_gaps.iter().max().unwrap() - in_gaps.iter().min().unwrap();
        assert!(in_spread > 20_000, "test input actually wobbles (spread {in_spread}us)");
        for w in releases.windows(2) {
            let gap = w[1].1.duration_since(w[0].1).as_micros() as i64;
            assert!(
                (gap - 16_666).abs() <= 2_000,
                "release grid regular (gap {gap}us)"
            );
        }
        assert_eq!(pacer.dropped(), 0);
    }

    #[test]
    fn present_pacer_slews_small_lag_reanchors_big_hole() {
        // A lag within what the buffer can absorb only nudges the schedule
        // (≤2ms); a 100ms hole reanchors at once with no leftover debt.
        let interval = Duration::from_micros(16_666);
        let mut pacer = PresentPacer::new(interval);
        let t0 = Instant::now();
        assert!(pacer.push(pacer_picture(1), t0));
        assert!(pacer.pop_due(t0).is_some());
        // Small lag (5ms): nudged by one slew step, not jumped to now.
        assert!(pacer.push(pacer_picture(2), t0 + interval + Duration::from_micros(5_000)));
        assert_eq!(pacer.next_due(), Some(t0 + interval + Duration::from_micros(2_000)));
        assert!(pacer.pop_due(t0 + interval + Duration::from_micros(5_000)).is_some());
        // Big hole on a fresh grid: arrival 100ms late reanchors at once.
        let mut pacer = PresentPacer::new(interval);
        assert!(pacer.push(pacer_picture(1), t0));
        assert!(pacer.pop_due(t0).is_some());
        assert!(pacer.push(pacer_picture(2), t0 + interval));
        assert!(pacer.pop_due(t0 + interval).is_some());
        let late = t0 + interval + Duration::from_micros(100_000);
        assert!(pacer.push(pacer_picture(3), late));
        assert_eq!(pacer.next_due(), Some(late), "big hole reanchors, no debt");
        assert!(pacer.pop_due(late).is_some());
        // The grid restarts from the reanchor, not from the stale schedule.
        assert!(pacer.push(pacer_picture(4), late + interval));
        assert_eq!(pacer.next_due(), Some(late + interval));
        assert_eq!(pacer.dropped(), 0);
    }

    #[test]
    fn present_pacer_follows_true_rate_below_seed_without_debt() {
        // 30 fps arrivals with a 60 fps seed, wired like read_loop (observe
        // each ts delta, then push): the clock converges and releases settle
        // on 33.3ms with no accumulated lag and no drops.
        let mut pacer = PresentPacer::new(Duration::from_micros(16_666));
        let t0 = Instant::now();
        assert!(pacer.push(pacer_picture(0), t0));
        let mut prev_ts = 0u32;
        let mut releases: Vec<Instant> = Vec::new();
        for k in 1..=8u64 {
            let at = t0 + Duration::from_micros(k * 33_333);
            let cur_ts = prev_ts + 3000;
            if let Some(rhythm) = rtp_frame_interval(prev_ts, cur_ts) {
                pacer.observe_interval(rhythm);
            }
            prev_ts = cur_ts;
            while let Some(due) = pacer.next_due() {
                if due > at {
                    break;
                }
                let (_, _) = pacer.pop_due(due).expect("due reached");
                releases.push(due);
            }
            assert!(pacer.push(pacer_picture(k as u8), at), "true rate must not overflow");
        }
        while let Some(due) = pacer.next_due() {
            let (_, _) = pacer.pop_due(due).expect("flush");
            releases.push(due);
        }
        assert_eq!(releases[0], t0, "anchor immediate");
        for w in releases.windows(2) {
            let gap = w[1].duration_since(w[0]).as_micros() as i64;
            assert!(
                (gap - 33_333).abs() <= 3_000,
                "settled on the true 30fps grid (gap {gap}us)"
            );
        }
        let last_arrival = t0 + Duration::from_micros(8 * 33_333);
        let debt = releases.last().unwrap().saturating_duration_since(last_arrival);
        assert!(debt <= Duration::from_micros(2 * 33_333), "no accumulated debt ({debt:?})");
        assert_eq!(pacer.dropped(), 0);
    }

    #[test]
    fn loss_gap_arms_pli_then_idr() {
        // perda → PLI → IDR, minus the wire: a corrupt payload fails the
        // same depacketize the read loop runs (arming exactly one PLI), and
        // the publisher-side `force_intra()` the RTCP task would trigger
        // lands an IDR on the very next unit.
        let mut depacketizer = H264Packet::default();
        assert!(
            depacketizer
                .depacketize(&Bytes::from_static(&[]))
                .is_err(),
            "empty payload is an irrecoverable AU gap"
        );
        let mut last: HashMap<u32, Instant> = HashMap::new();
        let now = Instant::now();
        assert!(pli_due(&mut last, 7, now), "gap arms one PLI");
        assert!(!pli_due(&mut last, 7, now), "burst of gaps still one PLI");
        // Publisher side: stream running, then the PLI-equivalent signal.
        let mut enc = H264Encoder::new(Quality::P720).expect("encoder");
        let _ = enc.encode(&synthetic_frame(1280, 720, 0)).expect("priming IDR");
        enc.force_intra();
        let unit = enc.encode(&synthetic_frame(1280, 720, 1)).expect("encode");
        assert!(contains_idr(&unit), "PLI-equivalent forces an observed IDR");
    }

    #[test]
    fn keyframe_wait_without_first_picture_arms_pli() {
        // Pre-IDR deltas decode to None; that wait must arm the same
        // debounce the AU-gap path uses, or a late join stays black until
        // the host reconfigures.
        let mut last: HashMap<u32, Instant> = HashMap::new();
        let now = Instant::now();
        assert!(pli_due(&mut last, 7, now), "first wait arms one PLI");
        assert!(!pli_due(&mut last, 7, now), "wait storm still one PLI");
        assert!(
            pli_due(&mut last, 7, now + PLI_DEBOUNCE),
            "window re-arms if the IDR still has not landed"
        );
    }

    #[tokio::test]
    async fn offer_sdp_advertises_nack_pli() {
        // Negotiation over the real path: the publisher offer must carry
        // `nack pli` (and `ccm fir`) — no signaling-protocol change, just
        // the codec feedback lines.
        let (event_tx, _event_rx) = mpsc::unbounded_channel::<MediaEvent>();
        let mut publisher = Publisher::start_with_profile(
            VideoSource::SyntheticBall,
            QualityProfile::low(),
            EngineKind::Software,
            None,
            event_tx,
        )
        .await
        .expect("publisher starts");
        let sdp = publisher.create_offer().await.expect("offer");
        publisher.stop().await;
        assert!(sdp.contains("nack pli"), "offer negotiates PLI recovery");
        assert!(sdp.contains("ccm fir"), "offer negotiates FIR recovery");
        assert!(!sdp.contains("m=audio"), "synthetic share stays video-only");
    }

    #[tokio::test]
    async fn offer_sdp_includes_opus_when_audio_is_attached() {
        let (event_tx, _event_rx) = mpsc::unbounded_channel::<MediaEvent>();
        let (_tx, rx) = std::sync::mpsc::sync_channel::<EncodedAudioPacket>(1);
        let mut publisher = Publisher::start_with_profile_and_audio(
            VideoSource::SyntheticBall,
            QualityProfile::low(),
            EngineKind::Software,
            None,
            event_tx,
            Some(rx),
        )
        .await
        .expect("publisher starts");
        let sdp = publisher.create_offer().await.expect("offer");
        publisher.stop().await;
        assert!(sdp.contains("m=audio"), "audio track advertised");
        assert!(sdp.contains("opus") || sdp.contains("OPUS") || sdp.contains("Opus"));
        let (event_tx, _event_rx) = mpsc::unbounded_channel::<MediaEvent>();
        let on_frame = Arc::new(|_frame: PresentedFrame| {});
        let mut viewer = NativeViewer::start_with_audio(None, event_tx, on_frame, None)
            .await
            .expect("viewer starts");
        let answer = viewer.set_remote_offer(&sdp).await.expect("answer");
        viewer.stop().await;
        assert!(answer.contains("m=video"), "answer keeps video");
        assert!(answer.contains("m=audio"), "answer keeps audio");
        assert!(
            !answer.contains("m=video 0"),
            "video m-line must not be rejected"
        );
    }

    #[tokio::test]
    async fn shared_encoder_survives_first_viewer_leaving_and_reconfigures() {
        let (p1_tx, p1_rx) = mpsc::unbounded_channel();
        let (p2_tx, p2_rx) = mpsc::unbounded_channel();
        let p1 = Publisher::start_with_profile(VideoSource::SyntheticBall,
            QualityProfile::custom(320, 180, 1000, 30).unwrap(), EngineKind::Software, Some(vec![]), p1_tx).await.unwrap();
        let p2 = p1.fork(Some(vec![]), p2_tx, None).await.unwrap();
        assert!(Arc::ptr_eq(&p1.shared, &p2.shared), "fork starts no second encoder");
        let counts: Vec<_> = (0..2).map(|_| Arc::new(std::sync::atomic::AtomicU64::new(0))).collect();
        let dims: Vec<_> = (0..2).map(|_| Arc::new(std::sync::atomic::AtomicU64::new(0))).collect();
        let mut viewers = Vec::new();
        let mut viewer_rx = Vec::new();
        for i in 0..2 {
            let (tx, rx) = mpsc::unbounded_channel();
            let count = counts[i].clone(); let dim = dims[i].clone();
            viewers.push(NativeViewer::start(Some(vec![]), tx, Arc::new(move |frame| {
                count.fetch_add(1, Ordering::Relaxed);
                dim.store(((frame.w as u64) << 32) | frame.h as u64, Ordering::Relaxed);
            })).await.unwrap());
            viewer_rx.push(rx);
        }
        let mut publishers = [p1, p2];
        let mut publisher_rx = [p1_rx, p2_rx];
        for i in 0..2 {
            let offer = publishers[i].create_offer().await.unwrap();
            let answer = viewers[i].set_remote_offer(&offer).await.unwrap();
            publishers[i].set_remote_answer(&answer).await.unwrap();
        }
        let mut stage = 0;
        let mut after_close = 0;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
        while tokio::time::Instant::now() < deadline {
            for i in 0..2 {
                while let Ok(event) = publisher_rx[i].try_recv() {
                    if let MediaEvent::IceCandidate { candidate } = event { viewers[i].add_remote_candidate(&candidate).await.unwrap(); }
                }
                while let Ok(event) = viewer_rx[i].try_recv() {
                    if let MediaEvent::IceCandidate { candidate } = event { publishers[i].add_remote_candidate(&candidate).await.unwrap(); }
                }
            }
            if stage == 0 && counts.iter().all(|c| c.load(Ordering::Relaxed) >= 5) {
                publishers[0].stop().await;
                viewers[0].stop().await;
                after_close = counts[1].load(Ordering::Relaxed);
                publishers[1].reconfigure(QualityProfile::custom(640, 360, 1500, 30).unwrap()).unwrap();
                stage = 1;
            }
            if stage == 1 && counts[1].load(Ordering::Relaxed) >= after_close + 5 && dims[1].load(Ordering::Relaxed) == (640u64 << 32) | 360 {
                stage = 2; break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        for i in 0..2 { publishers[i].stop().await; viewers[i].stop().await; }
        assert_eq!(stage, 2, "both viewers must decode, then survivor must follow reconfiguration");
        assert!(publishers[1].shared.encode_stop.load(Ordering::Acquire), "last viewer stops the shared encoder");
    }

    async fn rtp_pair(
        audio: bool,
        audio_after_video: bool,
    ) -> (u64, bool, bool, u64) {
        let (pub_tx, mut pub_rx) = mpsc::unbounded_channel::<MediaEvent>();
        let (view_tx, mut view_rx) = mpsc::unbounded_channel::<MediaEvent>();
        let (audio_tx, audio_rx) = std::sync::mpsc::sync_channel::<EncodedAudioPacket>(4);
        let audio_rx = audio.then_some(audio_rx);
        let samples: Vec<f32> = (0..960)
            .flat_map(|n| {
                let v = (n as f32 * std::f32::consts::TAU * 440.0 / 48_000.0).sin() * 0.25;
                [v, v]
            })
            .collect();
        let mut encoder =
            opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Audio)
                .expect("audio encoder");
        let mut encoded = vec![0u8; 4000];
        let audio_frames = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let counted_audio = Arc::clone(&audio_frames);
        let on_audio: Arc<dyn Fn(&[f32]) + Send + Sync> = Arc::new(move |pcm| {
            if pcm.iter().any(|sample| sample.abs() > 0.001) {
                counted_audio.fetch_add(1, Ordering::Relaxed);
            }
        });
        let frames = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let counted = Arc::clone(&frames);
        let on_frame = Arc::new(move |_frame: PresentedFrame| {
            counted.fetch_add(1, Ordering::Relaxed);
        });
        let mut publisher = Publisher::start_with_profile_and_audio(
            VideoSource::SyntheticBall,
            QualityProfile::low(),
            EngineKind::Software,
            Some(vec![]),
            pub_tx,
            audio_rx,
        )
        .await
        .expect("publisher");
        let mut viewer = NativeViewer::start_with_audio(Some(vec![]), view_tx, on_frame, Some(on_audio))
            .await
            .expect("viewer");
        let offer = publisher.create_offer().await.expect("offer");
        let answer = viewer.set_remote_offer(&offer).await.expect("answer");
        publisher.set_remote_answer(&answer).await.expect("set answer");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        let mut pub_ice = false;
        let mut view_ice = false;
        let mut errors = 0u64;
        while (frames.load(Ordering::Relaxed) < 3
            || (audio && audio_frames.load(Ordering::Relaxed) < 3))
            && tokio::time::Instant::now() < deadline
        {
            if audio && (!audio_after_video || frames.load(Ordering::Relaxed) > 0) {
                let len = encoder.encode_float(&samples, &mut encoded).unwrap();
                let _ = audio_tx.try_send(EncodedAudioPacket {
                    data: encoded[..len].to_vec(),
                    duration: Duration::from_millis(20),
                });
            }
            while let Ok(event) = pub_rx.try_recv() {
                match event {
                    MediaEvent::IceCandidate { candidate } => {
                        let _ = viewer.add_remote_candidate(&candidate).await;
                    }
                    MediaEvent::IceConnected => pub_ice = true,
                    MediaEvent::Error(_) => errors += 1,
                    _ => {}
                }
            }
            while let Ok(event) = view_rx.try_recv() {
                match event {
                    MediaEvent::IceCandidate { candidate } => {
                        let _ = publisher.add_remote_candidate(&candidate).await;
                    }
                    MediaEvent::IceConnected => view_ice = true,
                    MediaEvent::Error(_) => errors += 1,
                    _ => {}
                }
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        let got = frames.load(Ordering::Relaxed);
        publisher.stop().await;
        viewer.stop().await;
        if audio {
            assert!(
                got >= 3 && audio_frames.load(Ordering::Relaxed) >= 3,
                "both media must arrive: video={got} audio={}",
                audio_frames.load(Ordering::Relaxed)
            );
        }
        (got, pub_ice, view_ice, errors)
    }

    #[tokio::test]
    async fn video_frames_flow_video_only() {
        let (got, pub_ice, view_ice, errors) = rtp_pair(false, false).await;
        assert!(
            got > 0,
            "video-only must present (frames={got} pub_ice={pub_ice} view_ice={view_ice} errors={errors})"
        );
    }

    #[tokio::test]
    async fn video_frames_flow_when_audio_track_is_attached() {
        let (got, pub_ice, view_ice, errors) = rtp_pair(true, false).await;
        assert!(
            got > 0,
            "video must present with audio attached (frames={got} pub_ice={pub_ice} view_ice={view_ice} errors={errors})"
        );
    }

    #[tokio::test]
    async fn audio_and_video_flow_after_repeated_peer_teardown() {
        // Each watch creates a new native viewer. Also force video to arrive
        // before audio, so success cannot depend on the track arrival order.
        for _ in 0..3 {
            rtp_pair(true, true).await;
        }
    }

    #[test]
    fn decode_roundtrip_is_nonblack_with_motion() {
        let mut enc = H264Encoder::new(Quality::P720).expect("encoder");
        let mut dec = H264Decoder::new().expect("decoder");
        let mut validator = FrameValidator::new();
        let mut frames = 0;
        let mut motions = 0;
        for n in 0..70 {
            let frame = synthetic_frame(1280, 720, n);
            let unit = enc.encode(&frame).expect("encode");
            if let Some(picture) = dec.decode(&unit).expect("decode") {
                let stats = picture.stats;
                frames += 1;
                assert!(FrameValidator::non_black(stats.luma_mean), "non-black");
                // Pixels ride along exactly once: dims match, RGBA sized.
                assert_eq!((picture.frame.w, picture.frame.h), (1280, 720));
                assert_eq!(picture.frame.data.len(), 1280 * 720 * 4);
                if validator.motion(stats.luma_mean) {
                    motions += 1;
                }
                if n == 0 {
                    assert!(stats.is_keyframe, "stream starts with IDR");
                }
            }
        }
        assert!(frames > 60, "nearly every unit decodes, got {frames}");
        assert!(motions > 0, "motion detected across frames");
    }

    #[test]
    fn census_counts_without_addresses() {
        let mut census = CandidateCensus::default();
        census.add("candidate:1 1 udp 2113937151 192.168.1.5 53555 typ host");
        census.add("candidate:2 1 udp 1685987071 203.0.113.9 53555 typ srflx");
        assert_eq!(census.host, 1);
        assert_eq!(census.srflx, 1);
        assert_eq!(census.udp, 2);
        assert_eq!(census.ip4, 2);
        let debug = format!("{census:?}");
        assert!(!debug.contains("192.168"), "no addresses in census");
    }

    #[test]
    fn validator_rejects_black() {
        assert!(!FrameValidator::non_black(0.0));
        assert!(!FrameValidator::non_black(4.0));
        assert!(FrameValidator::non_black(50.0));
    }

    // -- quality engine ----------------------------------------------------

    #[test]
    fn presets_match_spec_ladder() {
        let low = QualityProfile::low();
        assert_eq!((low.w, low.h, low.bitrate_kbps, low.fps), (854, 480, 800, 15));
        let medium = QualityProfile::medium();
        assert_eq!((medium.w, medium.h, medium.bitrate_kbps, medium.fps), (1280, 720, 2000, 30));
        let high = QualityProfile::high();
        assert_eq!((high.w, high.h, high.bitrate_kbps, high.fps), (1920, 1080, 10_000, 60));
        for preset in [low, medium, high] {
            preset.validate().expect("presets are valid");
        }
        // Legacy mapping: P720 is the historical default, P1080 follows HIGH.
        assert_eq!(Quality::P720.profile(), QualityProfile::medium());
        assert_eq!(Quality::P1080.profile(), QualityProfile::high());
    }

    #[test]
    fn fit_openh264_keeps_5120x1440_inside_3840x2160() {
        assert_eq!(fit_openh264_dims(5120, 1440), (1920, 540));
        assert_eq!(fit_openh264_dims(1920, 1080), (1920, 1080));
        assert_eq!(fit_openh264_dims(3840, 2160), (1920, 1080));
    }

    #[test]
    fn fit_hardware_caps_each_axis_at_4096_preserving_aspect() {
        // Media Foundation inbox H.264 MFTs refuse widths past 4096
        // (SetOutputType 0x80041000 at 5120 wide); inside the box is identity.
        assert_eq!(fit_hardware_dims(5120, 1440), (4096, 1152));
        assert_eq!(fit_hardware_dims(1440, 5120), (1152, 4096));
        assert_eq!(fit_hardware_dims(4096, 1152), (4096, 1152));
        assert_eq!(fit_hardware_dims(3840, 2160), (3840, 2160));
        assert_eq!(fit_hardware_dims(1920, 1080), (1920, 1080));
        assert_eq!(fit_hardware_dims(8192, 8192), (4096, 4096));
        assert_eq!(fit_hardware_dims(0, 1080), (2, 2));
        // Idempotent: fitting twice is a fixed point (retarget stability).
        for (w, h) in [(5120, 1440), (4096, 1152), (3440, 1440), (1920, 1080)] {
            let once = fit_hardware_dims(w, h);
            assert_eq!(fit_hardware_dims(once.0, once.1), once);
            assert_eq!(once.0 % 2, 0);
            assert_eq!(once.1 % 2, 0);
        }
    }

    #[test]
    fn build_best_software_keeps_exact_fit_semantics() {
        // Explicit Software engine: identical to VideoEncoder::new, portable
        // (no hardware probe involved).
        let profile = QualityProfile::custom(5120, 1440, 10_000, 30).expect("profile");
        let (enc, dims) =
            build_best(&profile, (5120, 1440), EngineKind::Software).expect("software builds");
        assert_eq!(dims, (1920, 540));
        assert_eq!(enc.dims(), (1920, 540));
        assert_eq!(enc.backend_name(), "openh264");
        let small = QualityProfile::custom(1280, 720, 2000, 30).expect("profile");
        let (_, dims) =
            build_best(&small, (1280, 720), EngineKind::Software).expect("software builds");
        assert_eq!(dims, (1280, 720));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn build_best_auto_steps_down_to_hardware_before_software() {
        // 5120-wide exceeds the MF width cap; Auto must land the strongest
        // encodable size: hardware at the 4096 box when a GPU encoder exists,
        // software fit otherwise. Branched on the live probe so the test is
        // deterministic per machine (never assumes hardware).
        let profile = QualityProfile::custom(5120, 1440, 10_000, 30).expect("profile");
        let (enc, dims) =
            build_best(&profile, (5120, 1440), EngineKind::Auto).expect("auto always lands");
        if probe_hardware().is_ok() {
            assert_eq!(dims, (4096, 1152));
            assert_eq!(enc.dims(), (4096, 1152));
            assert_ne!(enc.backend_name(), "openh264", "hardware must win at 4096x1152");
        } else {
            assert_eq!(dims, (1920, 540));
            assert_eq!(enc.backend_name(), "openh264");
        }
    }

    #[test]
    fn encode_decode_ultrawide_5120x1440() {
        let profile = QualityProfile::custom(5120, 1440, 20_000, 30).expect("profile");
        let mut enc = H264Encoder::new_with_profile(&profile, 5120, 1440).expect("encoder");
        assert_eq!((enc.w, enc.h), (1920, 540));
        let mut dec = H264Decoder::new().expect("decoder");
        let mut pictures = 0;
        for n in 0..4 {
            let unit = enc.encode(&synthetic_frame(1920, 540, n)).expect("encode");
            if let Some(picture) = dec.decode(&unit).expect("decode") {
                assert_eq!((picture.frame.w, picture.frame.h), (1920, 540));
                pictures += 1;
            }
        }
        assert!(pictures >= 1, "software still emits after fitting 5120x1440");
    }

    #[test]
    fn custom_validates_ranges_with_typed_errors() {
        let ok = QualityProfile::custom(640, 360, 1000, 24).expect("valid custom");
        assert_eq!((ok.w, ok.h), (640, 360));
        // Boundaries hold.
        assert!(QualityProfile::custom(2, 2, 100, 1).is_ok());
        assert!(QualityProfile::custom(4096, 4096, 20_000, 60).is_ok());
        assert!(QualityProfile::custom(5120, 1440, 20_000, 60).is_ok());
        assert!(QualityProfile::custom(MAX_DIM, MAX_DIM, 20_000, 60).is_ok());
        // Each axis fails typed (numbers only, no secrets possible).
        assert_eq!(
            QualityProfile::custom(1, 360, 1000, 30).unwrap_err(),
            QualityError::InvalidDimensions { w: 1, h: 360 }
        );
        assert_eq!(
            QualityProfile::custom(640, MAX_DIM + 1, 1000, 30).unwrap_err(),
            QualityError::InvalidDimensions { w: 640, h: MAX_DIM + 1 }
        );
        assert_eq!(
            QualityProfile::custom(640, 360, 99, 30).unwrap_err(),
            QualityError::BitrateOutOfRange(99)
        );
        assert_eq!(
            QualityProfile::custom(640, 360, 20_001, 30).unwrap_err(),
            QualityError::BitrateOutOfRange(20_001)
        );
        assert_eq!(
            QualityProfile::custom(640, 360, 1000, 0).unwrap_err(),
            QualityError::FpsOutOfRange(0)
        );
        assert_eq!(
            QualityProfile::custom(640, 360, 1000, 61).unwrap_err(),
            QualityError::FpsOutOfRange(61)
        );
    }

    #[test]
    fn normalize_evens_aspect_and_never_upscales() {
        // Odd request floors to even (at most 1px per axis).
        assert_eq!(normalize_dims(1280, 720, 855, 481), (854, 480));
        // Aspect preserved: wide source in a square request fits by width.
        assert_eq!(normalize_dims(1280, 720, 480, 480), (480, 270));
        // Never upscale beyond the source.
        assert_eq!(normalize_dims(640, 360, 1920, 1080), (640, 360));
        // Exact fit passes through.
        assert_eq!(normalize_dims(1280, 720, 1280, 720), (1280, 720));
        // Degenerate input never panics, never sub-minimum.
        assert_eq!(normalize_dims(0, 720, 1280, 720), (2, 2));
        assert_eq!(normalize_dims(1280, 720, 0, 0), (2, 2));
        // Idempotent: normalizing twice is a fixed point.
        let once = normalize_dims(721, 405, 1280, 720);
        assert_eq!(once, (720, 404));
        assert_eq!(normalize_dims(once.0, once.1, 1280, 720), once);
    }

    #[test]
    fn encode_target_follows_source_within_profile() {
        let cap = QualityProfile::high();
        assert_eq!(encode_target_for_frame(800, 600, &cap), (800, 600));
        let fitted = cap.normalized_dims(3440, 1440);
        assert_eq!(encode_target_for_frame(3440, 1440, &cap), (fitted.0 as usize, fitted.1 as usize));
        assert!(fitted.0 <= 1920 && fitted.1 <= 1080);
        let native = QualityProfile::custom(MAX_DIM, MAX_DIM, 10_000, 60).unwrap();
        assert_eq!(encode_target_for_frame(3440, 1440, &native), (3440, 1440));
        assert_eq!(encode_target_for_frame(5120, 1440, &native), (5120, 1440));
        assert_eq!(encode_target_for_frame(1200, 900, &native), (1200, 900));
    }

    #[test]
    fn pacing_and_keyframe_cadence_follow_fps() {
        assert_eq!(QualityProfile::low().frame_duration(), Duration::from_micros(1_000_000 / 15));
        assert_eq!(QualityProfile::medium().frame_duration(), Duration::from_micros(1_000_000 / 30));
        assert_eq!(QualityProfile::high().frame_duration(), Duration::from_micros(1_000_000 / 60));
        assert_eq!(QualityProfile::low().keyframe_interval_frames(), 30);
        assert_eq!(QualityProfile::medium().keyframe_interval_frames(), 60);
        // floor of 2 frames keeps tiny fps sane.
        let one = QualityProfile::custom(320, 240, 500, 1).unwrap();
        assert_eq!(one.keyframe_interval_frames(), 2);
    }

    #[test]
    fn upload_estimate_is_bitrate_times_watchers_saturating() {
        assert_eq!(upload_estimate_bps(2_000_000, 0), 0);
        assert_eq!(upload_estimate_bps(2_000_000, 3), 6_000_000);
        assert_eq!(upload_estimate_bps(10_000_000, 5), 50_000_000);
        assert_eq!(upload_estimate_bps(u32::MAX, usize::MAX), u64::MAX);
    }

    #[test]
    fn decoder_survives_sps_dims_change_mid_stream() {
        // Reconfig without re-signaling: the viewer keeps ONE decoder and
        // picks the new SPS up mid-stream (new dims out, no reset).
        let mut dec = H264Decoder::new().expect("decoder");
        let mut enc_a =
            H264Encoder::new_with_profile(&QualityProfile::custom(640, 360, 2000, 30).unwrap(), 640, 360)
                .expect("encoder A");
        for n in 0..5 {
            let unit = enc_a.encode(&synthetic_frame(640, 360, n)).expect("encode A");
            let picture = dec.decode(&unit).expect("decode A").expect("picture A");
            assert_eq!((picture.frame.w, picture.frame.h), (640, 360));
        }
        // New generation, new dims: first unit carries fresh SPS/PPS + IDR.
        let mut enc_b =
            H264Encoder::new_with_profile(&QualityProfile::custom(480, 270, 1000, 15).unwrap(), 480, 270)
                .expect("encoder B");
        let first_b = enc_b.encode(&synthetic_frame(480, 270, 0)).expect("encode B");
        assert!(contains_idr(&first_b), "reconfig lands on an IDR");
        let picture = dec.decode(&first_b).expect("decode B").expect("picture B");
        assert_eq!((picture.frame.w, picture.frame.h), (480, 270));
        assert_eq!(picture.frame.data.len(), 480 * 270 * 4);
        // Stream continues at the new size.
        for n in 1..5 {
            let unit = enc_b.encode(&synthetic_frame(480, 270, n)).expect("encode B");
            let picture = dec.decode(&unit).expect("decode B").expect("picture B");
            assert_eq!((picture.frame.w, picture.frame.h), (480, 270));
        }
    }

    static HW_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn hw_decode_auto_selects_hardware_where_probed() {
        // Env mutation is process-global: serialize all HW-touching tests.
        let _guard = HW_ENV_LOCK.lock().expect("hw lock");
        let auto = H264Decoder::new_auto().expect("auto never fails while software builds");
        #[cfg(target_os = "windows")]
        if crate::mfdec::probe_hardware().is_ok() {
            assert_ne!(auto.backend_name(), "openh264", "hardware must win where probed");
            return;
        }
        assert_eq!(auto.backend_name(), "openh264");
    }

    #[test]
    fn hw_decode_falls_back_to_software_on_hook() {
        // Deterministic everywhere (non-Windows never probes): the hook
        // forces software, removal restores auto.
        let _guard = HW_ENV_LOCK.lock().expect("hw lock");
        std::env::set_var("GOLIVE_DISABLE_HW", "1");
        let hooked = H264Decoder::new_auto().expect("hooked auto still builds");
        assert_eq!(hooked.backend_name(), "openh264");
        std::env::remove_var("GOLIVE_DISABLE_HW");
    }

    #[test]
    fn hw_decode_roundtrip_matches_encode_dims() {
        // REAL path both ends: NVENC (or software where absent) encodes,
        // new_auto decodes. Pictures arrive at encode dims, non-black.
        let _guard = HW_ENV_LOCK.lock().expect("hw lock");
        let profile = QualityProfile::custom(640, 360, 2000, 30).expect("profile");
        let engine = EngineKind::Auto;
        // Demanded hardware may still refuse odd sizes: fall back to
        // software for the ENCODE side (the decode side under test is what
        // matters here).
        let mut enc = VideoEncoder::new(&profile, 640, 360, engine)
            .or_else(|_| VideoEncoder::new(&profile, 640, 360, EngineKind::Software))
            .expect("encoder");
        let mut dec = H264Decoder::new_auto().expect("decoder");
        let mut pictures = 0u32;
        for n in 0..6u8 {
            let frame = I420Frame {
                w: 640,
                h: 360,
                data: {
                    let mut data = vec![0u8; 640 * 360 * 3 / 2];
                    data[..640 * 360].fill(16 + n * 10);
                    data[640 * 360..].fill(128);
                    data
                },
            };
            let unit = match enc.encode_frame(&frame).expect("encode") {
                Some(unit) => unit,
                None => continue,
            };
            if let Some(picture) = dec.decode(&unit).expect("decode") {
                assert_eq!((picture.frame.w, picture.frame.h), (640, 360));
                pictures += 1;
            }
        }
        assert!(pictures >= 2, "roundtrip yields pictures (saw {pictures})");
        #[cfg(target_os = "windows")]
        if crate::mfdec::probe_hardware().is_ok() {
            assert_ne!(
                dec.backend_name(),
                "openh264",
                "roundtrip must stay on hardware end to end"
            );
        }
    }

    #[test]
    fn hw_decode_recovers_to_software_mid_stream() {
        // A fatally failing unit degrades the decoder to software
        // transparently: the NEXT valid unit still decodes, now in software.
        let _guard = HW_ENV_LOCK.lock().expect("hw lock");
        let mut dec = H264Decoder::new_auto().expect("decoder");
        let garbage = [0u8, 0, 0, 1, 0x65, 0xFF, 0x00];
        let _ = dec.decode(&garbage);
        let profile = QualityProfile::custom(320, 240, 500, 15).expect("profile");
        let mut enc = H264Encoder::new_with_profile(&profile, 320, 240).expect("encoder");
        let frame = I420Frame { w: 320, h: 240, data: vec![128u8; 320 * 240 * 3 / 2] };
        let unit = enc.encode(&frame).expect("encode");
        let picture = dec.decode(&unit).expect("decode").expect("picture after fault");
        assert_eq!((picture.frame.w, picture.frame.h), (320, 240));
        assert_eq!(dec.backend_name(), "openh264", "faulted hardware stays software");
    }

    #[test]
    fn hw_probe_falls_back_explicitly_and_demanded_hw_fails_high() {
        // Env mutation is process-global: serialize all HW-touching tests.
        let _guard = HW_ENV_LOCK.lock().expect("hw lock");
        std::env::set_var("GOLIVE_DISABLE_HW", "1");
        assert!(
            matches!(probe_hardware(), Err(MediaError::HwUnavailable(_))),
            "hook forces probe failure"
        );
        let auto = VideoEncoder::new(&QualityProfile::low(), 320, 240, EngineKind::Auto)
            .expect("auto falls back to software");
        assert_eq!(auto.backend_name(), "openh264");
        assert!(
            matches!(
                VideoEncoder::new(&QualityProfile::low(), 320, 240, EngineKind::Hardware),
                Err(MediaError::HwUnavailable(_))
            ),
            "demanded hardware fails high, never silent software"
        );
        std::env::remove_var("GOLIVE_DISABLE_HW");
        // Unhooked, Auto always yields a working encoder (either backend).
        let auto = VideoEncoder::new(&QualityProfile::low(), 320, 240, EngineKind::Auto)
            .expect("auto resolves");
        assert!(matches!(
            auto.backend_name(),
            "openh264" | "videotoolbox" | "nvenc" | "qsv" | "amf" | "mfhw"
        ));
        // Hook bypasses the process cache: a primed cache never leaks
        // hardware into a forced-fallback decision (called twice on purpose).
        std::env::set_var("GOLIVE_DISABLE_HW", "1");
        for _ in 0..2 {
            let auto = VideoEncoder::new(&QualityProfile::low(), 320, 240, EngineKind::Auto)
                .expect("hooked auto still falls back");
            assert_eq!(auto.backend_name(), "openh264");
        }
        std::env::remove_var("GOLIVE_DISABLE_HW");
    }

    #[test]
    fn decide_engine_selects_hw_only_on_probe_ok() {
        assert_eq!(decide_engine(true), EngineKind::Hardware);
        assert_eq!(decide_engine(false), EngineKind::Software);
    }

    #[tokio::test]
    async fn publisher_reports_live_backend() {
        // Backend cell starts None and flips to the built backend without
        // any product-code polling: the encode thread publishes on build.
        let (event_tx, _event_rx) = mpsc::unbounded_channel::<MediaEvent>();
        let mut publisher = Publisher::start_with_profile(
            VideoSource::SyntheticBall,
            QualityProfile::low(),
            EngineKind::Software,
            None,
            event_tx,
        )
        .await
        .expect("publisher starts");
        let mut backend = None;
        for _ in 0..100 {
            backend = publisher.backend();
            if backend.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        publisher.stop().await;
        assert_eq!(backend, Some("openh264"));
    }

    #[tokio::test]
    async fn request_keyframe_forces_idr_without_reconfig() {
        // Watcher join after the startup IDR: request_keyframe must force a
        // new IDR on the live encoder without bumping generation.
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<MediaEvent>();
        let mut publisher = Publisher::start_with_profile(
            VideoSource::SyntheticBall,
            QualityProfile::low(),
            EngineKind::Software,
            None,
            event_tx,
        )
        .await
        .expect("publisher starts");
        let mut saw_live_idr = false;
        let live_deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while !saw_live_idr {
            let remaining = live_deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, event_rx.recv()).await {
                Ok(Some(MediaEvent::Keyframe)) => saw_live_idr = true,
                Ok(Some(MediaEvent::Error(detail))) => panic!("encode failed: {detail}"),
                Ok(_) => {}
                Err(_) => break,
            }
        }
        assert!(saw_live_idr, "startup IDR observed");
        publisher.request_keyframe();
        let mut saw_join_idr = false;
        let join_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !saw_join_idr {
            let remaining = join_deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, event_rx.recv()).await {
                Ok(Some(MediaEvent::Keyframe)) => saw_join_idr = true,
                Ok(Some(MediaEvent::Stats(stats))) => {
                    assert_eq!(stats.generation, 0, "join IDR must not bump quality generation");
                }
                Ok(Some(MediaEvent::Error(detail))) => panic!("encode failed: {detail}"),
                Ok(_) => {}
                Err(_) => break,
            }
        }
        publisher.stop().await;
        assert!(saw_join_idr, "request_keyframe lands an IDR without reconfig");
    }

    #[tokio::test]
    async fn reconfigure_bumps_generation_with_immediate_idr() {
        // Full transactional path, no network: synthetic → encode thread →
        // events. Reconfig applies atomically (rebuild + IDR + generation).
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<MediaEvent>();
        let mut publisher = Publisher::start_with_profile(
            VideoSource::SyntheticBall,
            QualityProfile::low(),
            EngineKind::Software,
            None,
            event_tx,
        )
        .await
        .expect("publisher starts");
        // Liveness first: the initial encoder opens with an IDR.
        let mut saw_live_idr = false;
        let live_deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while !saw_live_idr {
            let remaining = live_deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, event_rx.recv()).await {
                Ok(Some(MediaEvent::Keyframe)) => saw_live_idr = true,
                Ok(Some(MediaEvent::Error(detail))) => panic!("encode failed: {detail}"),
                Ok(_) => {}
                Err(_) => break,
            }
        }
        assert!(saw_live_idr, "stream alive before reconfig");
        // Invalid profiles fail typed BEFORE touching the live encoder.
        let bad = QualityProfile { w: 640, h: 360, bitrate_kbps: 50, fps: 15 };
        assert!(
            matches!(publisher.reconfigure(bad), Err(MediaError::Codec(_))),
            "validate-first rejects bad profiles"
        );
        let next = QualityProfile::custom(640, 360, 1000, 15).expect("valid next");
        publisher.reconfigure(next).expect("reconfig accepted");
        // Await the generation fence (bounded), then the forced IDR after it.
        let mut saw_post_idr = false;
        let mut gen_seen = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while !gen_seen || !saw_post_idr {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, event_rx.recv()).await {
                Ok(Some(MediaEvent::Keyframe)) => {
                    if gen_seen {
                        saw_post_idr = true;
                    }
                }
                Ok(Some(MediaEvent::Stats(stats))) => {
                    if stats.generation == 1 {
                        gen_seen = true;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        publisher.stop().await;
        assert!(gen_seen, "generation fence bumps to 1");
        assert!(saw_post_idr, "forced IDR lands right after reconfig");
    }

    #[tokio::test]
    async fn reconfig_mid_stream_viewer_follows_dims_change() {
        // End-to-end through the real path minus RTP: encode thread →
        // latest-only slot → ONE decoder standing in for the viewer. A
        // 720p→360p reconfig must land via in-band SPS: new dims out of the
        // same decoder, no reset, stream alive throughout.
        let profile_b = QualityProfile::custom(640, 360, 1000, 15).expect("profile B");
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<MediaEvent>();
        let slot = Arc::new(FrameSlot::default());
        let (reconfig_tx, reconfig_rx) = std::sync::mpsc::channel::<QualityProfile>();
        let stop = Arc::new(AtomicBool::new(false));
        let (stop_, slot_) = (Arc::clone(&stop), Arc::clone(&slot));
        let backend_ = Arc::new(std::sync::Mutex::new(None));
        let census_ = Arc::new(std::sync::Mutex::new(CandidateCensus::default()));
        let intra_ = Arc::new(AtomicBool::new(false));
        let handle = std::thread::spawn(move || {
            encode_loop(
                VideoSource::SyntheticBall,
                QualityProfile::medium(),
                EngineKind::Software,
                &stop_,
                &slot_,
                &backend_,
                &census_,
                &event_tx,
                &reconfig_rx,
                &intra_,
            );
        });
        let mut dec = H264Decoder::new().expect("decoder");
        async fn take_unit(slot: &FrameSlot, secs: u64) -> Vec<u8> {
            let (unit, _) = tokio::time::timeout(Duration::from_secs(secs), slot.take())
                .await
                .expect("unit arrives");
            assert!(!unit.is_empty(), "poison pill never counted");
            unit
        }
        // Phase 1: 720p pictures out of the shared decoder.
        let mut saw_a = false;
        let phase1 = tokio::time::Instant::now() + Duration::from_secs(15);
        while !saw_a {
            assert!(
                tokio::time::Instant::now() < phase1,
                "720p pictures never arrived"
            );
            let unit = take_unit(&slot, 10).await;
            if let Some(picture) = dec.decode(&unit).expect("decode") {
                if (picture.frame.w, picture.frame.h) == (1280, 720) {
                    saw_a = true;
                }
            }
        }
        // Reconfig mid-stream; fence + forced IDR observed on events.
        reconfig_tx.send(profile_b).expect("reconfig sent");
        let mut gen_seen = false;
        let mut saw_post_idr = false;
        let fence_line = tokio::time::Instant::now() + Duration::from_secs(15);
        while !gen_seen || !saw_post_idr {
            let remaining = fence_line.saturating_duration_since(tokio::time::Instant::now());
            assert!(!remaining.is_zero(), "fence/IDR never observed");
            tokio::select! {
                event = event_rx.recv() => match event {
                    Some(MediaEvent::Keyframe) if gen_seen => saw_post_idr = true,
                    Some(MediaEvent::Stats(stats)) if stats.generation == 1 => gen_seen = true,
                    None => panic!("event channel closed early"),
                    _ => {}
                },
                (unit, _) = slot.take() => { let _ = dec.decode(&unit).expect("decode during reconfig"); },
                _ = tokio::time::sleep(remaining) => panic!("fence/IDR never observed"),
            }
        }
        // Phase 2: the SAME decoder now yields 360p (SPS-driven, no reset).
        let mut saw_b = false;
        let phase2 = tokio::time::Instant::now() + Duration::from_secs(15);
        while !saw_b {
            assert!(
                tokio::time::Instant::now() < phase2,
                "360p pictures never arrived after reconfig"
            );
            let unit = take_unit(&slot, 10).await;
            if let Some(picture) = dec.decode(&unit).expect("decode") {
                assert!(
                    (picture.frame.w, picture.frame.h) == (1280, 720)
                        || (picture.frame.w, picture.frame.h) == (640, 360),
                    "no other dims mid-flight: {:?}",
                    (picture.frame.w, picture.frame.h)
                );
                if (picture.frame.w, picture.frame.h) == (640, 360) {
                    assert_eq!(picture.frame.data.len(), 640 * 360 * 4);
                    saw_b = true;
                }
            }
        }
        stop.store(true, Ordering::Release);
        handle.join().expect("encode loop thread");
        assert!(saw_a && gen_seen && saw_post_idr && saw_b);
    }

    /// Real IOSurface-backed pixel buffer with no capture involved:
    /// allocating buffers needs no TCC (only SCK capture does).
    #[cfg(target_os = "macos")]
    fn mock_hw_buffer(w: usize, h: usize) -> objc2_core_foundation::CFRetained<objc2_core_video::CVPixelBuffer> {
        use objc2_core_video::{kCVPixelFormatType_32BGRA, CVPixelBuffer, CVPixelBufferCreate};
        let mut raw: *mut CVPixelBuffer = std::ptr::null_mut();
        // SAFETY: out-pointer valid; None allocator/attributes are allowed.
        let status = unsafe {
            CVPixelBufferCreate(
                None,
                w,
                h,
                kCVPixelFormatType_32BGRA,
                None,
                std::ptr::NonNull::new(&mut raw).expect("out-pointer"),
            )
        };
        assert_eq!(status, 0, "pool buffer created");
        unsafe {
            objc2_core_foundation::CFRetained::from_raw(
                std::ptr::NonNull::new(raw).expect("non-null buffer"),
            )
        }
    }

    /// Test-only release balancing a `GpuPixelBuffer` wrap below.
    /// Reconstitutes the +1 and drops it (safe: only ever handed real
    /// buffers from `mock_hw_buffer`).
    #[cfg(target_os = "macos")]
    unsafe extern "C-unwind" fn test_gpu_release(ptr: *mut std::ffi::c_void) {
        if ptr.is_null() {
            return;
        }
        unsafe {
            drop(
                objc2_core_foundation::CFRetained::<objc2_core_video::CVPixelBuffer>::from_raw(
                    std::ptr::NonNull::new(ptr as *mut objc2_core_video::CVPixelBuffer)
                        .expect("non-null"),
                ),
            );
        }
    }

    /// Counting no-op release for dangling/error-injection handles: never
    /// dereferences, only proves Drop runs exactly once.
    #[cfg(target_os = "macos")]
    unsafe extern "C-unwind" fn counting_noop_release(ptr: *mut std::ffi::c_void) {
        assert!(!ptr.is_null());
        TEST_RELEASES.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    #[cfg(target_os = "macos")]
    static TEST_RELEASES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    #[cfg(target_os = "macos")]
    #[test]
    fn hw_zero_copy_submit_mock_iosurface_buffer() {
        // True zero-copy integration without TCC: a real (IOSurface-backed)
        // pool buffer goes straight into VTCompressionSessionEncodeFrame —
        // no pool copy, no NV12 staging. VT works on this machine (see the
        // probe test); the hook stays off here under the shared lock.
        let _guard = HW_ENV_LOCK.lock().expect("hw lock");
        std::env::remove_var("GOLIVE_DISABLE_HW");
        let pixel = mock_hw_buffer(640, 360);
        let mut enc = crate::vt::VtEncoder::new(640, 360, 2_000_000, 30, true)
            .expect("hw session");
        let unit = enc
            .encode_cv_pixel_buffer(pixel)
            .expect("submit")
            .expect("unit out");
        assert!(!unit.is_empty());
        assert!(contains_idr(&unit), "first zero-copy submit is an IDR");
        assert!(!annexb_nals(&unit).is_empty(), "units split");
        enc.close();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn hw_routing_falls_back_on_injected_dims_mismatch() {
        // Injected error: retained 640x360 buffer against a 320x240 target.
        // No HW needed (software route): convert + scale still yields
        // correct-dims output instead of failing the session.
        let pixel = mock_hw_buffer(640, 360);
        let raw = objc2_core_foundation::CFRetained::into_raw(pixel);
        // SAFETY: +1 moves into the handle with the balancing release.
        let gpu = unsafe {
            GpuPixelBuffer::from_raw(
                raw.as_ptr() as *mut std::ffi::c_void,
                640,
                360,
                640 * 4,
                test_gpu_release,
            )
        };
        let profile = QualityProfile::custom(320, 240, 500, 15).expect("profile");
        let mut encoder =
            VideoEncoder::new(&profile, 320, 240, EngineKind::Software).expect("sw encoder");
        let unit = encode_gpu_frame(&mut encoder, gpu, (320, 240))
            .expect("fallback converts")
            .expect("unit out");
        let mut dec = H264Decoder::new().expect("decoder");
        let picture = dec.decode(&unit).expect("decode").expect("picture");
        assert_eq!((picture.frame.w, picture.frame.h), (320, 240));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn gpu_convert_rejects_degenerate_dims_without_touching_pixels() {
        // Injected error with a dangling handle: w=0 must fail on the
        // bounds check before any pointer is borrowed.
        // SAFETY: never dereferenced — the dims check fires first, and the
        // counting release never touches the pointer either.
        TEST_RELEASES.store(0, std::sync::atomic::Ordering::SeqCst);
        let gpu = unsafe {
            GpuPixelBuffer::from_raw(
                0x1000 as *mut std::ffi::c_void,
                0,
                48,
                256,
                counting_noop_release,
            )
        };
        assert!(gpu_to_i420(&gpu).is_err());
        // Drop releases exactly once even on the error path above.
        drop(gpu);
        assert_eq!(TEST_RELEASES.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}

#[cfg(test)]
mod native_decoder_recovery_tests {
    use super::*;
    use golive_platform::decode::{Nv12Picture, VideoDecoder};
    struct FailingDecoder;
    impl VideoDecoder for FailingDecoder {
        fn decode(&mut self, _: &[u8]) -> Result<Option<Nv12Picture>, String> { Err("injected driver failure".into()) }
        fn parameter_sets(&self) -> Vec<u8> { Vec::new() }
    }
    #[test]
    fn hardware_failure_waits_for_idr_before_software_deltas() {
        let profile = QualityProfile { w: 320, h: 180, fps: 60, bitrate_kbps: 600 };
        let mut encoder = H264Encoder::new_with_profile(&profile, 320, 180).unwrap();
        let frame = synthetic_frame(320,180,0);
        let idr = encoder.encode(&frame).unwrap();
        assert!(contains_idr(&idr));
        let delta = encoder.encode(&synthetic_frame(320,180,1)).unwrap();
        assert!(!contains_idr(&delta));
        let mut decoder = H264Decoder::Native(Box::new(FailingDecoder));
        assert!(decoder.decode_for_present(&delta,true).unwrap().is_none());
        assert!(matches!(decoder, H264Decoder::RecoveringSoftware { .. }));
        assert!(decoder.waiting_for_keyframe(), "mid-stream recovery must request a fresh IDR");
        assert!(decoder.decode_for_present(&delta,true).unwrap().is_none());
        let recovered = decoder.decode_for_present(&idr,true).unwrap().unwrap();
        assert_eq!((recovered.frame.w,recovered.frame.h), (320,180));
        assert_eq!(decoder.backend_name(), "openh264");
        assert!(decoder.decode_for_present(&delta,true).unwrap().is_some());
    }
    #[test]
    fn recovery_reuses_valid_parameter_sets_for_an_idr_without_headers() {
        struct CachedFailure(Vec<u8>);
        impl VideoDecoder for CachedFailure {
            fn decode(&mut self, _: &[u8]) -> Result<Option<Nv12Picture>, String> { Err("injected".into()) }
            fn parameter_sets(&self) -> Vec<u8> { self.0.clone() }
        }
        let mut encoder = H264Encoder::new(Quality::P720).unwrap();
        let unit = encoder.encode(&synthetic_frame(1280,720,0)).unwrap();
        let mut headers = Vec::new(); let mut idr = Vec::new();
        for range in annexb_nals(&unit) {
            let nal = &unit[range];
            if matches!(nal_type(nal), Some(7 | 8)) { headers.extend_from_slice(nal); }
            else { idr.extend_from_slice(nal); }
        }
        assert!(!headers.is_empty()); assert!(contains_idr(&idr));
        let mut decoder = H264Decoder::Native(Box::new(CachedFailure(headers)));
        assert!(decoder.decode_for_present(&idr,true).unwrap().is_some());
        assert_eq!(decoder.backend_name(), "openh264");
    }
    #[test]
    fn invalid_native_output_recovers_instead_of_retrying_broken_backend() {
        struct InvalidOutput;
        impl VideoDecoder for InvalidOutput {
            fn decode(&mut self, _: &[u8]) -> Result<Option<Nv12Picture>, String> {
                Ok(Some(Nv12Picture { width: 320, height: 180, data: vec![0] }))
            }
            fn parameter_sets(&self) -> Vec<u8> { Vec::new() }
        }
        let mut encoder = H264Encoder::new(Quality::P720).unwrap();
        let idr = encoder.encode(&synthetic_frame(1280,720,0)).unwrap();
        let mut decoder = H264Decoder::Native(Box::new(InvalidOutput));
        assert!(decoder.decode_for_present(&idr,true).unwrap().is_some());
        assert_eq!(decoder.backend_name(), "openh264");
    }
    #[test]
    fn hardware_failure_on_idr_recovers_in_the_same_call() {
        let mut encoder = H264Encoder::new(Quality::P720).unwrap();
        let idr = encoder.encode(&synthetic_frame(1280,720,0)).unwrap();
        let mut decoder = H264Decoder::Native(Box::new(FailingDecoder));
        assert!(decoder.decode_for_present(&idr,true).unwrap().is_some());
        assert_eq!(decoder.backend_name(), "openh264");
    }
}
