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
use std::collections::HashMap;
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
fn annexb_nals(data: &[u8]) -> Vec<std::ops::Range<usize>> {
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
fn nal_type(nal: &[u8]) -> Option<u8> {
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
            #[cfg(any(target_os = "macos", target_os = "windows"))]
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

/// Latest-only unit slot between the encode thread and the RTP pump: at
/// most ONE access unit is ever queued. Publishing replaces stale; taking
/// consumes. Under congestion the viewer gets the freshest decodable unit
/// (gaps are packet-loss-shaped, which the decoder already survives) and
/// the encoder never blocks on a slow peer — the anti-jank root fix.
/// An empty unit is the poison pill (encoders never emit empty units).
#[derive(Debug, Default)]
struct FrameSlot {
    slot: std::sync::Mutex<Option<(Vec<u8>, Duration)>>,
    notify: tokio::sync::Notify,
}

impl FrameSlot {
    fn publish(&self, unit: Vec<u8>, duration: Duration) {
        *self.slot.lock().expect("frame slot poisoned") = Some((unit, duration));
        self.notify.notify_one();
    }

    fn poison(&self) {
        *self.slot.lock().expect("frame slot poisoned") = Some((Vec::new(), Duration::ZERO));
        self.notify.notify_one();
    }

    async fn take(&self) -> (Vec<u8>, Duration) {
        loop {
            if let Some(item) = self.slot.lock().expect("frame slot poisoned").take() {
                return item;
            }
            self.notify.notified().await;
        }
    }
}

/// Decoder feeding whole access units; returns luma stats per decoded frame.
pub struct H264Decoder {
    dec: Decoder,
}

#[derive(Clone, Copy, Debug)]
pub struct DecodedStats {
    pub w: usize,
    pub h: usize,
    pub luma_mean: f64,
    pub is_keyframe: bool,
}

/// One decoded picture ready to present. RGBA, row-major, `w*h*4` bytes.
/// Carried by value through the present callback (owned per frame; the shell
/// drops stale ones instead of queuing).
#[derive(Clone, Debug)]
pub struct PresentedFrame {
    pub w: usize,
    pub h: usize,
    pub rgba: Vec<u8>,
}

/// Stats plus pixels from a single decode. One decode feeds validation,
/// events and presentation — never decoded twice.
#[derive(Clone, Debug)]
pub struct DecodedPicture {
    pub stats: DecodedStats,
    pub frame: PresentedFrame,
}

impl H264Decoder {
    pub fn new() -> Result<Self, MediaError> {
        let dec =
            Decoder::new().map_err(|e| MediaError::Codec(format!("decoder init: {e}")))?;
        Ok(Self { dec })
    }

    pub fn decode(&mut self, annexb: &[u8]) -> Result<Option<DecodedPicture>, MediaError> {
        let is_keyframe = annexb_nals(annexb)
            .iter()
            .any(|range| nal_type(&annexb[range.clone()]) == Some(5));
        match self
            .dec
            .decode(annexb)
            .map_err(|e| MediaError::Codec(format!("decode: {e}")))?
        {
            Some(yuv) => {
                let [slice] = yuv.split::<1>();
                let y = slice.y();
                let (w, h) = slice.dimensions();
                if y.is_empty() {
                    return Ok(None);
                }
                let sum: u64 = y.iter().map(|b| *b as u64).sum();
                let mut rgba = vec![0u8; w * h * 4];
                yuv.write_rgba8(&mut rgba);
                Ok(Some(DecodedPicture {
                    stats: DecodedStats {
                        w,
                        h,
                        luma_mean: sum as f64 / y.len() as f64,
                        is_keyframe,
                    },
                    frame: PresentedFrame { w, h, rgba },
                }))
            }
            None => Ok(None),
        }
    }
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

pub struct Publisher {
    pc: Arc<RTCPeerConnection>,
    event_tx: mpsc::UnboundedSender<MediaEvent>,
    encode_stop: Arc<AtomicBool>,
    encode_thread: Option<std::thread::JoinHandle<()>>,
    stopped: Arc<AtomicBool>,
    reconfig_tx: Option<std::sync::mpsc::Sender<QualityProfile>>,
    slot: Arc<FrameSlot>,
    /// Live encoder backend (`None` until the encode thread builds it).
    /// Written on every build/rebuild; read for counters/diagnostics.
    backend: Arc<std::sync::Mutex<Option<&'static str>>>,
    /// PLI/FIR recovery task handle (aborted in `stop()`). The flag it sets
    /// is owned by the encode thread + the task itself — this handle is the
    /// only publisher-side state the recovery needs.
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

        // Encode on a blocking thread; latest-only slot into tokio (see
        // FrameSlot: at most one unit queued, stale replaced, never block).
        // `intra_requested` bridges the async RTCP task (PLI/FIR in) to the
        // encode thread (`force_intra()` out).
        let slot = Arc::new(FrameSlot::default());
        let backend = Arc::new(std::sync::Mutex::new(None));
        let intra_requested = Arc::new(AtomicBool::new(false));
        let (reconfig_tx, reconfig_rx) = std::sync::mpsc::channel::<QualityProfile>();
        let encode_stop = Arc::new(AtomicBool::new(false));
        let encode_thread = {
            let stop = Arc::clone(&encode_stop);
            let event_tx = event_tx.clone();
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
            let stop = Arc::clone(&encode_stop);
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
                    if result.is_err() {
                        break;
                    }
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
            pc,
            event_tx,
            encode_stop,
            encode_thread: Some(encode_thread),
            stopped: Arc::new(AtomicBool::new(false)),
            reconfig_tx: Some(reconfig_tx),
            slot,
            backend,
            rtcp_task: Some(rtcp_task),
        })
    }

    /// Live encoder backend for counters/diagnostics (`None` until the
    /// encode thread finishes its first build). Non-blocking read.
    pub fn backend(&self) -> Option<&'static str> {
        self.backend.lock().ok().and_then(|guard| *guard)
    }

    /// Transactional reconfig without re-signaling: validates first (typed
    /// failure, current profile untouched), then hands to the encode thread,
    /// which rebuilds + forces IDR + bumps the generation fence. Same
    /// m-line, SPS in-band — WebRTC resolves it.
    pub fn reconfigure(&self, profile: QualityProfile) -> Result<(), MediaError> {
        profile
            .validate()
            .map_err(|e| MediaError::Codec(format!("profile: {e}")))?;
        self.reconfig_tx
            .as_ref()
            .ok_or(MediaError::Closed)?
            .send(profile)
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
        self.encode_stop.store(true, Ordering::Release);
        let _ = tokio::time::timeout(Duration::from_secs(5), self.pc.close()).await;
        if let Some(handle) = self.rtcp_task.take() {
            handle.abort();
        }
        self.slot.poison();
        if let Some(thread) = self.encode_thread.take() {
            let _ = thread.join();
        }
        let _ = self.event_tx.send(MediaEvent::Stats(MediaStats::default()));
    }
}

/// Encode loop: profile-paced frames, transactional reconfig, latest-only
/// output. Deterministic pacing from the profile fps (never wall-clock
/// absolute near RTP): each tick targets start+n*interval; overruns skip
/// sleep (catch-up, no burst). Reconfigs drain to latest and apply atomically
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
    let start = Instant::now();
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
                let received = ext.rx.recv_timeout(EXT_TICK);
                source_trace.record(TraceSample {
                    frames: received.is_ok() as u64,
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
                    None => continue,
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
        let encoded = match frame {
            PendingFrame::Cpu(frame) => encoder.encode_frame(&frame),
            PendingFrame::Gpu(gpu) => encode_gpu_frame(&mut encoder, gpu, target),
        };
        if started.is_some() { encode_trace.record(TraceSample {
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
        // 4. Pace on the CURRENT profile (reconfig may have changed fps).
        let next = start + profile.frame_duration() * (n as u32);
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        }
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
    let mut decoder = H264Decoder::new()?;
    let mut frames = Vec::new();
    let mut unit = Vec::<u8>::new();
    let mut unit_has_vcl = false;
    let flush = |unit: &mut Vec<u8>, unit_has_vcl: &mut bool, decoder: &mut H264Decoder, frames: &mut Vec<I420Frame>| -> Result<(), MediaError> {
        if unit.is_empty() {
            return Ok(());
        }
        if let Some(yuv) = decoder.dec.decode(unit).map_err(|e| MediaError::Codec(format!("movie decode: {e}")))? {
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
                        read_loop(track, &pc_pli, &event_tx, &on_frame, &census).await;
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
) {
    let mut depacketizer = H264Packet::default();
    let mut decoder = match H264Decoder::new() {
        Ok(dec) => dec,
        Err(e) => {
            let _ = event_tx.send(MediaEvent::Error(e.to_string()));
            return;
        }
    };
    let mut validator = FrameValidator::new();
    let mut stats = MediaStats::default();
    let mut unit = Vec::<u8>::new();
    let mut unit_ts: Option<u32> = None;
    let mut last_decoded_ts: Option<u32> = None;
    let mut last_pli: HashMap<u32, Instant> = HashMap::new();
    let mut rtp_trace = Trace::new(Stage::Rtp);
    let mut decode_trace = Trace::new(Stage::Decode);
    loop {
        let (packet, _) = match track.read_rtp().await {
            Ok(pair) => pair,
            Err(_) => break,
        };
        let ts = packet.header.timestamp;
        rtp_trace.record(TraceSample { frames: 1, bytes: packet.payload.len() as u64, ..Default::default() }, None);
        // Access-unit boundary: timestamp rollover flushes the previous unit.
        if unit_ts.map(|t| t != ts).unwrap_or(false) && !unit.is_empty() {
            let completed_ts = unit_ts.expect("guarded by the map above");
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
                unit.clear();
            } else {
                let started = decode_trace.start();
                let decoded = decode_unit(&mut decoder, &unit, event_tx);
                decode_trace.record(TraceSample {
                    frames: decoded.is_some() as u64, dropped: decoded.is_none() as u64,
                    bytes: unit.len() as u64,
                    width: decoded.as_ref().map(|p| p.frame.w as u32).unwrap_or(0),
                    height: decoded.as_ref().map(|p| p.frame.h as u32).unwrap_or(0),
                    keyframes: decoded.as_ref().map(|p| p.stats.is_keyframe as u64).unwrap_or(0),
                    ..Default::default()
                }, started);
                if let Some(picture) = decoded {
                    last_decoded_ts = Some(completed_ts);
                    let frame = picture.stats;
                    stats.frames_decoded += 1;
                    if frame.is_keyframe {
                        stats.keyframes_decoded += 1;
                        let _ = event_tx.send(MediaEvent::Keyframe);
                    }
                    let non_black = FrameValidator::non_black(frame.luma_mean);
                    let motion = validator.motion(frame.luma_mean);
                    let _ = event_tx.send(MediaEvent::VideoFrame { non_black, motion });
                    on_frame(picture.frame);
                    if stats.frames_decoded % 30 == 0 {
                        let mut snapshot = stats.clone();
                        snapshot.census = census_snapshot(census);
                        let _ = event_tx.send(MediaEvent::Stats(snapshot));
                    }
                }
                unit.clear();
            } // end non-stale branch
        }
        unit_ts = Some(ts);
        match depacketizer.depacketize(&packet.payload) {
            Ok(bytes) if !bytes.is_empty() => unit.extend_from_slice(&bytes),
            Ok(_) => {}
            Err(_) => {
                // Irrecoverable AU gap: the unit being assembled can never
                // decode cleanly — ask for an IDR (debounced inside). Trace
                // sent vs suppressed so gap storms stay visible as numbers.
                let sent = maybe_send_pli(pc, &mut last_pli, packet.header.ssrc).await;
                decode_trace.record(TraceSample {
                    pli_sent: sent as u64,
                    pli_suppressed: (!sent) as u64,
                    ..Default::default()
                }, None);
                continue;
            }
        }
    }
}

fn decode_unit(
    decoder: &mut H264Decoder,
    unit: &[u8],
    event_tx: &mpsc::UnboundedSender<MediaEvent>,
) -> Option<DecodedPicture> {
    match decoder.decode(unit) {
        Ok(frame) => frame,
        Err(e) => {
            let _ = event_tx.send(MediaEvent::Error(e.to_string()));
            None
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
        for n in 0..5 {
            ext_tx.send(ExternalFrame::Cpu(gray(128, 96, 16 + n * 20))).unwrap();
        }
        drop(ext_tx); // source gone: loop must end by itself, bounded
        // Drain latest-only units on a throwaway runtime (take is async).
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let mut units = 0;
        for _ in 0..3 {
            let got = rt.block_on(async {
                tokio::time::timeout(Duration::from_secs(10), slot.take()).await
            });
            let (unit, duration) = got.expect("unit arrives within 10s");
            assert!(!unit.is_empty(), "poison pill never counted");
            assert_eq!(duration, QualityProfile::medium().frame_duration());
            units += 1;
        }
        assert_eq!(units, 3);
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
        let feeder = std::thread::spawn(move || {
            for n in 0..24u8 {
                if ext_tx.send(ExternalFrame::Cpu(gray(2000, 1000, 16 + n))).is_err() {
                    break;
                }
            }
        });
        // Drain units on a throwaway runtime; decode the first to prove the
        // software fit still applies (output stays 1920x960, never native).
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let mut decoded_dims = None;
        let mut decoder = H264Decoder::new().expect("decoder");
        for take in 0..10 {
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
        feeder.join().expect("feeder drains into the loop");
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
        let feeder = std::thread::spawn(move || {
            for n in 0..16u8 {
                if ext_tx.send(ExternalFrame::Cpu(gray(5120, 1440, 16 + n))).is_err() {
                    break;
                }
            }
        });
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        for take in 0..8 {
            let got = rt.block_on(async {
                tokio::time::timeout(Duration::from_secs(20), slot.take()).await
            });
            let (unit, _) = got.unwrap_or_else(|_| panic!("unit {take} arrives"));
            assert!(!unit.is_empty(), "poison pill never counted");
        }
        feeder.join().expect("feeder drains into the loop");
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
                assert_eq!(picture.frame.rgba.len(), 1280 * 720 * 4);
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
        assert_eq!(picture.frame.rgba.len(), 480 * 270 * 4);
        // Stream continues at the new size.
        for n in 1..5 {
            let unit = enc_b.encode(&synthetic_frame(480, 270, n)).expect("encode B");
            let picture = dec.decode(&unit).expect("decode B").expect("picture B");
            assert_eq!((picture.frame.w, picture.frame.h), (480, 270));
        }
    }

    static HW_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
                Err(_) => panic!("event channel closed early"),
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
                    assert_eq!(picture.frame.rgba.len(), 640 * 360 * 4);
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
