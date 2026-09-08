//! Native media: synthetic/movie sources, OpenH264 encode/decode, WebRTC
//! transport via the `webrtc` crate (0.14). No GStreamer, no manual
//! packetization beyond what the crate already does.
//!
//! Contract: H.264 Constrained Baseline (packetization-mode=1, 42e01f) +
//! Opus 48 kHz modeled (audio track lands later; this step is video-only —
//! the requested E2E asserts video), 720p30/1080p30. No TURN anywhere.
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

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use openh264::decoder::Decoder;
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, Level, Profile,
};
use openh264::formats::{YUVBuffer, YUVSource};
use tokio::sync::mpsc;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::ice::mdns::MulticastDnsMode;
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
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
}

/// Frame sources. Synthetic and movie are self-contained; screen capture
/// arrives injected (see `ExternalSource`): the core never imports platform
/// or OS bindings — frames cross as plain channel data, and tests inject
/// without any OS involved.
pub enum VideoSource {
    /// Gradient + bouncing square + counter bar at 30 fps.
    SyntheticBall,
    /// H.264 Annex-B file: decoded with OpenH264, scaled, re-encoded.
    MovieFile(PathBuf),
    /// Externally produced frames (screen capture via the app bridge).
    External(ExternalSource),
}

/// Injected frame feed. `rx` yields contract-agnostic I420 frames (the core
/// scales to the share size); `label` is an opaque human tag for logs
/// (never pixels, never tokens).
pub struct ExternalSource {
    pub rx: std::sync::mpsc::Receiver<I420Frame>,
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
    Closed,
}

impl std::fmt::Display for MediaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MediaError::Codec(detail) => write!(f, "codec: {detail}"),
            MediaError::Transport(detail) => write!(f, "transport: {detail}"),
            MediaError::Source(detail) => write!(f, "source: {detail}"),
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

    fn yuv_buffer(&self) -> YUVBuffer {
        YUVBuffer::from_vec(self.data.clone(), self.w, self.h)
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
fn scale_frame(src: &I420Frame, w: usize, h: usize) -> I420Frame {
    if src.w == w && src.h == h {
        return I420Frame {
            w,
            h,
            data: src.data.clone(),
        };
    }
    let mut data = vec![0u8; w * h * 3 / 2];
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
    I420Frame { w, h, data }
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

fn strip_start_code(nal: &[u8]) -> &[u8] {
    if nal.starts_with(&[0, 0, 0, 1]) {
        &nal[4..]
    } else if nal.starts_with(&[0, 0, 1]) {
        &nal[3..]
    } else {
        nal
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

/// Builds the shared API: default codecs (H264 CB mode-1 included), mDNS
/// DISABLED on both ends (lesson 2: symmetric, plus redacted census).
fn build_api() -> Result<webrtc::api::API, MediaError> {
    let mut media_engine = MediaEngine::default();
    media_engine
        .register_default_codecs()
        .map_err(|e| MediaError::Transport(format!("codecs: {e}")))?;
    let mut setting_engine = SettingEngine::default();
    setting_engine.set_ice_multicast_dns_mode(MulticastDnsMode::Disabled);
    Ok(APIBuilder::default()
        .with_media_engine(media_engine)
        .with_setting_engine(setting_engine)
        .build())
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

fn video_codec() -> RTCRtpCodecCapability {
    use webrtc::api::media_engine::MIME_TYPE_H264;
    RTCRtpCodecCapability {
        mime_type: MIME_TYPE_H264.to_owned(),
        clock_rate: 90000,
        channels: 0,
        sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
            .to_owned(),
        rtcp_feedback: vec![],
    }
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
}

impl Publisher {
    /// Starts publishing `source` at `quality`. Needs a running tokio runtime.
    pub async fn start(
        source: VideoSource,
        quality: Quality,
        ice_servers: Option<Vec<String>>,
        event_tx: mpsc::UnboundedSender<MediaEvent>,
    ) -> Result<Self, MediaError> {
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
        pc.add_track(track.clone())
            .await
            .map_err(|e| MediaError::Transport(format!("add_track: {e}")))?;
        // Explicit sendonly: this side never receives. (Default would be
        // sendrecv; the contract pins directions, asserted in E2E.)
        for transceiver in pc.get_transceivers().await {
            transceiver
                .set_direction(
                    webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection::Sendonly,
                )
                .await;
        }
        wire_ice_events(&pc, &event_tx);

        // Encode on a blocking thread; bridge into tokio via blocking_send.
        // Backpressure (cap 8) instead of unbounded growth.
        let (sample_tx, mut sample_rx) = mpsc::channel::<Vec<u8>>(8);
        let encode_stop = Arc::new(AtomicBool::new(false));
        let (w, h) = quality.dims();
        let encode_thread = {
            let stop = Arc::clone(&encode_stop);
            let event_tx = event_tx.clone();
            std::thread::Builder::new()
                .name("golive-encode".into())
                .spawn(move || {
                    encode_loop(source, quality, w, h, &stop, &sample_tx, &event_tx);
                })
                .map_err(|e| MediaError::Codec(format!("encode thread: {e}")))?
        };
        // Pump encoded units into the track with anchored durations.
        tokio::spawn(async move {
            while let Some(unit) = sample_rx.recv().await {
                let sample = Sample {
                    data: Bytes::from(unit),
                    timestamp: SystemTime::now(),
                    duration: FRAME_DURATION,
                    packet_timestamp: 0,
                    prev_dropped_packets: 0,
                    prev_padding_packets: 0,
                };
                if track.write_sample(&sample).await.is_err() {
                    break;
                }
            }
        });

        Ok(Self {
            pc,
            event_tx,
            encode_stop,
            encode_thread: Some(encode_thread),
            stopped: Arc::new(AtomicBool::new(false)),
        })
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
        self.pc
            .add_ice_candidate(RTCIceCandidateInit {
                candidate: candidate.to_owned(),
                sdp_mid: None,
                sdp_mline_index: Some(0),
                username_fragment: None,
            })
            .await
            .map_err(|e| MediaError::Transport(format!("add candidate: {e}")))?;
        Ok(())
    }

    /// Bounded idempotent stop: flag, timed PC close, thread join. Never wedges.
    pub async fn stop(&mut self) {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        self.encode_stop.store(true, Ordering::Release);
        let _ = tokio::time::timeout(Duration::from_secs(5), self.pc.close()).await;
        if let Some(thread) = self.encode_thread.take() {
            let _ = thread.join();
        }
        let _ = self.event_tx.send(MediaEvent::Stats(MediaStats::default()));
    }
}

/// Encode loop: deterministic pacing from a frame counter (never wall-clock
/// absolute near RTP). Skips sleep on overrun (catch-up, no burst).
fn encode_loop(
    source: VideoSource,
    quality: Quality,
    w: usize,
    h: usize,
    stop: &AtomicBool,
    sample_tx: &mpsc::Sender<Vec<u8>>,
    event_tx: &mpsc::UnboundedSender<MediaEvent>,
) {
    let mut encoder = match H264Encoder::new(quality) {
        Ok(enc) => enc,
        Err(e) => {
            let _ = event_tx.send(MediaEvent::Error(e.to_string()));
            return;
        }
    };
    // Movie frames are pre-decoded once (bounded: 300), then cycled.
    let movie_frames: Option<Vec<I420Frame>> = match &source {
        VideoSource::MovieFile(path) => match preload_movie(path, w, h) {
            Ok(frames) => Some(frames),
            Err(e) => {
                let _ = event_tx.send(MediaEvent::Error(e.to_string()));
                return;
            }
        },
        VideoSource::SyntheticBall | VideoSource::External(_) => None,
    };
    // External feeds repeat their last frame across pacing gaps (capture
    // hiccups must not stall the stream); disconnect ends the share.
    let mut ext_last: Option<I420Frame> = None;
    let start = Instant::now();
    let mut n: u64 = 0;
    while !stop.load(Ordering::Acquire) {
        let base: I420Frame = match &source {
            VideoSource::MovieFile(_) => {
                let frames = movie_frames.as_ref().expect("preloaded above");
                scale_frame(&frames[(n as usize) % frames.len()], w, h)
            }
            VideoSource::SyntheticBall => synthetic_frame(w, h, n),
            VideoSource::External(ext) => match ext.rx.recv_timeout(EXT_TICK) {
                Ok(frame) => {
                    ext_last = Some(frame.clone());
                    scale_frame(&frame, w, h)
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => match &ext_last {
                    Some(last) => scale_frame(last, w, h),
                    // No frame yet: black until the first arrives (never a
                    // stale picture from another source — there is none).
                    None => I420Frame::black(w, h),
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
        let frame = base;
        match encoder.encode(&frame) {
            Ok(unit) => {
                // The host observes its own keyframes here (the sender side
                // never decodes): NAL type 5 in the produced access unit.
                if contains_idr(&unit) {
                    let _ = event_tx.send(MediaEvent::Keyframe);
                }
                if sample_tx.blocking_send(unit).is_err() {
                    break;
                }
            }
            Err(e) => {
                let _ = event_tx.send(MediaEvent::Error(e.to_string()));
                break;
            }
        }
        n += 1;
        let next = start + FRAME_DURATION * (n as u32);
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        }
    }
}

/// Reads a movie file, decodes up to 300 frames into access units
/// (new unit at each VCL NAL following a unit that already has video),
/// normalizes to contract size.
fn preload_movie(path: &PathBuf, w: usize, h: usize) -> Result<Vec<I420Frame>, MediaError> {
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
    Ok(frames.into_iter().map(|f| scale_frame(&f, w, h)).collect())
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
        let api = build_api()?;
        let pc = Arc::new(
            api.new_peer_connection(rtc_config(ice_servers))
                .await
                .map_err(|e| MediaError::Transport(format!("pc: {e}")))?,
        );
        wire_ice_events(&pc, &event_tx);
        {
            let event_tx = event_tx.clone();
            pc.on_track(Box::new(move |track, _, _| {
                let event_tx = event_tx.clone();
                let on_frame = Arc::clone(&on_frame);
                Box::pin(async move {
                    read_loop(track, &event_tx, &on_frame).await;
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
        self.pc
            .add_ice_candidate(RTCIceCandidateInit {
                candidate: candidate.to_owned(),
                sdp_mid: None,
                sdp_mline_index: Some(0),
                username_fragment: None,
            })
            .await
            .map_err(|e| MediaError::Transport(format!("add candidate: {e}")))?;
        Ok(())
    }

    /// Bounded idempotent stop. Never wedges.
    pub async fn stop(&mut self) {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = tokio::time::timeout(Duration::from_secs(5), self.pc.close()).await;
    }
}

/// Track read loop: depacketize, assemble access units per RTP timestamp,
/// decode once, then validate + emit + present the same picture.
async fn read_loop(
    track: Arc<TrackRemote>,
    event_tx: &mpsc::UnboundedSender<MediaEvent>,
    on_frame: &Arc<dyn Fn(PresentedFrame) + Send + Sync>,
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
    loop {
        let (packet, _) = match track.read_rtp().await {
            Ok(pair) => pair,
            Err(_) => break,
        };
        let ts = packet.header.timestamp;
        // Access-unit boundary: timestamp rollover flushes the previous unit.
        if unit_ts.map(|t| t != ts).unwrap_or(false) && !unit.is_empty() {
            if let Some(picture) = decode_unit(&mut decoder, &unit, event_tx) {
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
                    snapshot.ice_connected = true;
                    let _ = event_tx.send(MediaEvent::Stats(snapshot));
                }
            }
            unit.clear();
        }
        unit_ts = Some(ts);
        match depacketizer.depacketize(&packet.payload) {
            Ok(bytes) if !bytes.is_empty() => unit.extend_from_slice(&bytes),
            Ok(_) => {}
            Err(_) => continue,
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
fn wire_ice_events(pc: &Arc<RTCPeerConnection>, event_tx: &mpsc::UnboundedSender<MediaEvent>) {
    {
        let event_tx = event_tx.clone();
        let mut census = CandidateCensus::default();
        let mut done = false;
        pc.on_ice_candidate(Box::new(move |candidate: Option<RTCIceCandidate>| {
            // Census and completion update here (outer FnMut context); only
            // owned event data moves into the async block below.
            let outgoing = match candidate {
                Some(candidate) => {
                    // W3C toJSON form: full "candidate:..." line. (Display is
                    // a short human form and does NOT unmarshal remotely.)
                    match candidate.to_json() {
                        Ok(init) => {
                            census.add(&init.candidate);
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
        // State transitions are rare; consumers dedupe. No flag needed.
        pc.on_ice_connection_state_change(Box::new(move |state: RTCIceConnectionState| {
            let event_tx = event_tx.clone();
            Box::pin(async move {
                use RTCIceConnectionState::*;
                match state {
                    Connected | Completed => {
                        let _ = event_tx.send(MediaEvent::IceConnected);
                    }
                    Failed | Disconnected | Closed => {
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
        let (ext_tx, ext_rx) = std::sync::mpsc::sync_channel::<I420Frame>(4);
        let source = VideoSource::External(ExternalSource {
            rx: ext_rx,
            label: "test-inject".into(),
        });
        let (sample_tx, mut sample_rx) = mpsc::channel::<Vec<u8>>(16);
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<MediaEvent>();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let stop_ = std::sync::Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            encode_loop(source, Quality::P720, 1280, 720, &stop_, &sample_tx, &event_tx);
        });
        for n in 0..5 {
            ext_tx.send(gray(128, 96, 16 + n * 20)).unwrap();
        }
        drop(ext_tx); // source gone: loop must end by itself, bounded
        let mut units = 0;
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while units < 3 && std::time::Instant::now() < deadline {
            if sample_rx.try_recv().is_ok() {
                units += 1;
            } else {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        assert!(units >= 3, "encoded {units} injected units");
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
}
