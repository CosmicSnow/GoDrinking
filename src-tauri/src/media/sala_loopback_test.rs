//! Sala same-PC loopback E2E: a REAL local Rendezvous (spawned `node
//! server.mjs`) plus TWO real `MediaEngine`s plus a REAL WebRTC loopback.
//!
//! No real capture: synthetic BGRA frames are fed to the REAL encoder
//! (VideoToolbox on macOS, OpenH264 on Windows). The viewer side is a raw
//! `webrtc` peer connection driven by the test, exactly like the browser
//! lane: it answers the host-minted offer and reads back RTP.
//!
//! Each test spawns its own server on its own port and kills it on exit
//! (also on panic, via `Drop`), so rate limits and rooms never leak across
//! tests. Every phase fails LOUDLY (no skips): a missing milestone panics
//! with the phase name and what was observed. This is the no-black-screen
//! proof at engine level.
//!
//! Directions covered:
//! - `sala_forward_decode_ok`: A hosts Sala, B joins, B watches A.
//!   watch → offer → answer → peer_connected → first IDR (SPS/PPS) → decode_ok.
//! - `sala_reverse_decode_ok`: B hosts Sala, A joins as viewer (dual-role),
//!   A watches B over the VIEWER socket. Same chain reversed, plus the
//!   session-branch `self_id` exclusivity and `viewer_member_id` exposure.
//! - `broadcast_decode_ok`: D hosts Broadcast, E joins; the handshake offer
//!   is answered; decode_ok without any watch.
//! - `dualrole_self_watch_skips_mint`: C hosts and joins its own room;
//!   watching self mints nothing (self-skip in either role) while another
//!   member's watch still mints (positive control that delivery works).

use super::super::peer_transport::{
    reassemble_h264_fu_a, rewrite_mdns_candidate_addresses, PeerSignal, PeerSignalKind,
};
use super::super::pipeline::EncoderCommand;
use super::super::types::{
    FrameRate, JoinMode, MediaSessionSnapshot, PeerTransportState, VideoResolution,
};
use super::super::SessionMode;
use super::tests::{request as base_request, worker_engine};
use super::MediaEngine;
use std::collections::HashMap;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine as WebrtcMediaEngine, MIME_TYPE_H264};
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::ice::mdns::MulticastDnsMode;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};
use webrtc::rtp_transceiver::RTCPFeedback;
use webrtc::track::track_remote::TrackRemote;

const PASSWORD: &str = "loopback-seat-password";
/// Baseline profile-level-id the product contract requires on the wire.
const BASELINE_PROFILE_IDC: u8 = 0x42;
const OFFER_WAIT: Duration = Duration::from_secs(30);
const ROSTER_WAIT: Duration = Duration::from_secs(30);
const CONNECT_WAIT: Duration = Duration::from_secs(30);
const IDR_WAIT: Duration = Duration::from_secs(60);
const NO_LINK_WAIT: Duration = Duration::from_secs(3);

/// A real Rendezvous killed on drop (including test-panic unwind), so no
/// node process ever leaks even when a phase fails loudly.
struct TestServer {
    child: Child,
    base: String,
    stderr_path: std::path::PathBuf,
}

fn stderr_tail(path: &std::path::Path, max_bytes: u64) -> String {
    let text = std::fs::read(path).unwrap_or_default();
    let tail = if text.len() as u64 > max_bytes {
        &text[text.len() - max_bytes as usize..]
    } else {
        &text[..]
    };
    String::from_utf8_lossy(tail).into_owned()
}

impl TestServer {
    fn spawn(port: u16) -> Self {
        let rendezvous_dir = format!("{}/../rendezvous", env!("CARGO_MANIFEST_DIR"));
        // Child stderr goes to a temp file (never null): when the server
        // dies early — e.g. `import ws` failing because `npm ci` never ran
        // — the failure below quotes the real cause instead of a bare port
        // timeout.
        let stderr_path = std::env::temp_dir().join(format!(
            "godrinking-rendezvous-{port}-p{}-{}.log",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
        ));
        let stderr_file = std::fs::File::create(&stderr_path).unwrap_or_else(|error| {
            panic!("loopback E2E: cannot create server stderr file: {error}")
        });
        let mut child = Command::new("node")
            .arg("server.mjs")
            .env("PORT", port.to_string())
            .env("BIND", "127.0.0.1")
            .current_dir(&rendezvous_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr_file))
            .spawn()
            .unwrap_or_else(|error| {
                panic!(
                    "loopback E2E needs node server.mjs: spawn failed: {error} \
                     (is node on PATH?)"
                )
            });
        let base = format!("http://127.0.0.1:{port}");
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
                break;
            }
            // Fail fast when the server process is already gone: the port
            // will never open, so quote stderr immediately instead of
            // burning the whole deadline on a misleading timeout.
            match child.try_wait() {
                Ok(Some(status)) => {
                    let _ = child.wait();
                    panic!(
                        "loopback E2E: rendezvous on {base} exited early ({status}) — server stderr tail:\n{} \
                         (did rendezvous `npm ci` run? is node on PATH?)",
                        stderr_tail(&stderr_path, 2048),
                    );
                }
                Ok(None) => {}
                Err(_) => {}
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "loopback E2E: rendezvous on {base} never opened its port — server stderr tail:\n{} \
                     (did rendezvous `npm ci` run? is node on PATH?)",
                    stderr_tail(&stderr_path, 2048),
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Self {
            child,
            base,
            stderr_path,
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.stderr_path);
    }
}

#[track_caller]
fn poll_until<T>(what: &str, timeout: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = f() {
            return value;
        }
        if Instant::now() > deadline {
            panic!("loopback E2E milestone missing: {what}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn sala_host_request(nickname: &str, base: &str) -> super::super::types::CreateMediaSessionRequest {
    let mut request = base_request();
    request.join_mode = JoinMode::Stunar;
    request.rendezvous_url = Some(base.to_owned());
    request.password = PASSWORD.to_owned();
    request.nickname = nickname.to_owned();
    request.session_mode = SessionMode::Room;
    request.share_on_start = false;
    request.admission = false;
    request.resolution = VideoResolution::P720;
    request.frame_rate = FrameRate::Fps30;
    request.system_audio = false;
    request
}

fn broadcast_host_request(
    nickname: &str,
    base: &str,
) -> super::super::types::CreateMediaSessionRequest {
    let mut request = sala_host_request(nickname, base);
    request.session_mode = SessionMode::Broadcast;
    request
}

fn roster_entry(snapshot: &MediaSessionSnapshot, id: &str) -> Option<(PeerTransportState, bool)> {
    snapshot
        .roster
        .iter()
        .find(|entry| entry.id == id)
        .map(|entry| (entry.state.clone(), entry.share))
}

/// Engine-level equivalent of the UI Watch gate: the target shares and is
/// not self.
fn watch_enabled(snapshot: &MediaSessionSnapshot, self_id: Option<&str>, target: &str) -> bool {
    snapshot
        .roster
        .iter()
        .any(|entry| entry.id == target && entry.share)
        && self_id != Some(target)
}

fn wait_share_visible(
    joiner: &MediaEngine,
    joiner_self: &str,
    host_id: &str,
) -> MediaSessionSnapshot {
    poll_until(
        &format!("roster shows {host_id} share:true (joiner {joiner_self})"),
        ROSTER_WAIT,
        || {
            let snapshot = joiner.snapshot();
            let watchable = watch_enabled(&snapshot, Some(joiner_self), host_id);
            watchable.then_some(snapshot)
        },
    )
}

fn wait_link_connected(host: &MediaEngine, viewer_id: &str, viewer: &LoopbackViewer, what: &str) {
    let deadline = Instant::now() + CONNECT_WAIT;
    loop {
        let snapshot = host.snapshot();
        if matches!(
            roster_entry(&snapshot, viewer_id),
            Some((PeerTransportState::Connected, _))
        ) {
            return;
        }
        if Instant::now() > deadline {
            let host_state = snapshot
                .roster
                .iter()
                .find(|entry| entry.id == viewer_id)
                .map(|entry| format!("{:?}", entry.state));
            panic!(
                "loopback E2E milestone missing: {what} peer_connected \
                 (viewer_pc={:?} viewer_ice={:?} viewer_signaling={:?} host_link={host_state:?})",
                viewer.pc.connection_state(),
                viewer.pc.ice_connection_state(),
                viewer.pc.signaling_state(),
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Force the Share slot open without real capture: the drain's `capturing`
/// flag reads this, and the encoder/pipeline workers run regardless.
fn force_capture_active(engine: &MediaEngine) {
    let mut guard = engine.state.lock().expect("engine state is available");
    let session = guard.session.as_mut().expect("session must exist");
    session.native_capture_active = true;
}

fn announce_and_force(engine: &MediaEngine) {
    force_capture_active(engine);
    engine
        .announce_share(true)
        .expect("share announce must send");
}

/// Raw webrtc viewer (the browser lane, in-process): answers an SDP offer
/// and streams received RTP payloads to the caller. No STUN: same-process
/// loopback only needs host candidates.
struct LoopbackViewer {
    pc: Arc<RTCPeerConnection>,
    runtime: tokio::runtime::Runtime,
}

impl LoopbackViewer {
    fn answer(
        offer_sdp: &str,
        rtp_tx: tokio::sync::mpsc::UnboundedSender<(u32, Vec<u8>)>,
    ) -> (Self, String) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("viewer runtime must build");
        let (pc, answer_sdp) = runtime.block_on(async {
            let mut media = WebrtcMediaEngine::default();
            media
                .register_codec(
                    RTCRtpCodecParameters {
                        capability: RTCRtpCodecCapability {
                            mime_type: MIME_TYPE_H264.into(),
                            clock_rate: 90_000,
                            channels: 0,
                            sdp_fmtp_line:
                                "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e02a"
                                    .into(),
                            rtcp_feedback: vec![
                                RTCPFeedback { typ: "nack".into(), parameter: "pli".into() },
                                RTCPFeedback { typ: "ccm".into(), parameter: "fir".into() },
                            ],
                        },
                        payload_type: 102,
                        ..Default::default()
                    },
                    RTPCodecType::Video,
                )
                .expect("viewer H264 registration must succeed");
            let registry = register_default_interceptors(Registry::new(), &mut media)
                .expect("viewer interceptors must register");
            let mut settings = SettingEngine::default();
            settings.set_include_loopback_candidate(true);
            settings.set_ice_multicast_dns_mode(MulticastDnsMode::Disabled);
            let api = APIBuilder::new()
                .with_media_engine(media)
                .with_interceptor_registry(registry)
                .with_setting_engine(settings)
                .build();
            let pc = Arc::new(
                api.new_peer_connection(RTCConfiguration::default())
                    .await
                    .expect("viewer peer connection must build"),
            );
            pc.on_track(Box::new(move |track: Arc<TrackRemote>, _, _| {
                let tx = rtp_tx.clone();
                Box::pin(async move {
                    loop {
                        match track.read_rtp().await {
                            Ok((packet, _)) => {
                                if tx
                                    .send((packet.header.timestamp, packet.payload.to_vec()))
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                })
            }));
            // The host never rewrites offer candidates (answers are rewritten
            // on receipt instead): do what `set_answer` does before remote.
            let offer_sdp = rewrite_mdns_candidate_addresses(offer_sdp, "127.0.0.1");
            pc.set_remote_description(
                RTCSessionDescription::offer(offer_sdp)
                    .expect("host offer must parse"),
            )
            .await
            .expect("viewer must accept the host offer");
            let answer = pc
                .create_answer(None)
                .await
                .expect("viewer must create an answer");
            let mut gather = pc.gathering_complete_promise().await;
            pc.set_local_description(answer)
                .await
                .expect("viewer must set its local answer");
            tokio::time::timeout(Duration::from_secs(10), gather.recv())
                .await
                .expect("viewer ICE gathering must finish");
            let sdp = pc
                .local_description()
                .await
                .expect("viewer local answer must exist")
                .sdp;
            assert!(
                sdp.contains("m=video") && !sdp.contains("m=video 0 "),
                "viewer answer must carry a live video section"
            );
            (pc, sdp)
        });
        (Self { pc, runtime }, answer_sdp)
    }

    fn wait_connected(&self, what: &str, host: &MediaEngine, viewer_id: &str) {
        let pc = Arc::clone(&self.pc);
        self.runtime.block_on(async {
            let deadline = Instant::now() + CONNECT_WAIT;
            loop {
                if pc.connection_state() == RTCPeerConnectionState::Connected {
                    return;
                }
                if Instant::now() > deadline {
                    let host_state = host
                        .snapshot()
                        .roster
                        .iter()
                        .find(|entry| entry.id == viewer_id)
                        .map(|entry| format!("{:?}", entry.state));
                    panic!(
                        "loopback E2E milestone missing: viewer {what} connected \
                         (viewer_ice={:?} viewer_signaling={:?} host_link={host_state:?})",
                        pc.ice_connection_state(),
                        pc.signaling_state(),
                    );
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
    }
}

/// NAL units reassembled from the RTP path (single, STAP-A, FU-A).
#[derive(Default, Debug)]
struct NalSeen {
    sps: bool,
    pps: bool,
    idr: bool,
    sps_profile: Option<u8>,
}

impl NalSeen {
    fn decode_ok(&self) -> bool {
        self.sps && self.pps && self.idr && self.sps_profile == Some(BASELINE_PROFILE_IDC)
    }
}

fn note_nal(seen: &mut NalSeen, nal: &[u8]) {
    if nal.is_empty() {
        return;
    }
    match nal[0] & 0x1F {
        7 => {
            seen.sps = true;
            if nal.len() > 1 {
                seen.sps_profile = Some(nal[1]);
            }
        }
        8 => seen.pps = true,
        5 => seen.idr = true,
        _ => {}
    }
}

fn feed_rtp_payload(
    seen: &mut NalSeen,
    fu_buffers: &mut HashMap<u32, Vec<Vec<u8>>>,
    timestamp: u32,
    payload: &[u8],
) {
    if payload.len() < 2 {
        return;
    }
    match payload[0] & 0x1F {
        28 => {
            let entry = fu_buffers.entry(timestamp).or_default();
            entry.push(payload.to_vec());
            let is_end = payload[1] & 0x40 != 0;
            if is_end {
                let fragments = fu_buffers.remove(&timestamp).unwrap_or_default();
                if let Some(nal) = reassemble_h264_fu_a(&fragments) {
                    note_nal(seen, &nal);
                }
            }
        }
        24 => {
            let mut rest = &payload[1..];
            while rest.len() >= 2 {
                let len = ((rest[0] as usize) << 8) | (rest[1] as usize);
                rest = &rest[2..];
                if rest.len() < len || len == 0 {
                    break;
                }
                note_nal(seen, &rest[..len]);
                rest = &rest[len..];
            }
        }
        1..=23 => note_nal(seen, payload),
        _ => {}
    }
}

fn assert_has_candidates(sdp: &str, what: &str) {
    assert!(
        sdp.lines()
            .any(|line| line.trim_start().starts_with("a=candidate:")),
        "loopback E2E milestone missing: {what} SDP carries ICE candidates:\n{sdp}"
    );
}

#[cfg(target_os = "macos")]
fn push_synthetic_frames(
    engine: &MediaEngine,
    width: u32,
    height: u32,
    count: usize,
    first_sequence: u64,
) {
    use super::super::pipeline::NativeEncoderFrame;
    use objc2::rc::Retained;
    use objc2_core_video::{
        kCVPixelFormatType_32BGRA, kCVReturnSuccess, CVPixelBuffer, CVPixelBufferCreate,
        CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferLockBaseAddress,
        CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    };
    use std::ptr::NonNull;

    let (encoder_tx, generation) = {
        let guard = engine.state.lock().expect("engine state is available");
        let session = guard.session.as_ref().expect("session must exist");
        (session._pipeline.encoder_tx.clone(), session.generation)
    };
    for index in 0..count {
        let mut raw: *mut CVPixelBuffer = std::ptr::null_mut();
        let status = unsafe {
            CVPixelBufferCreate(
                None,
                width as usize,
                height as usize,
                kCVPixelFormatType_32BGRA,
                None,
                NonNull::new(&mut raw).expect("out pointer is non-null"),
            )
        };
        assert_eq!(
            status, kCVReturnSuccess,
            "loopback E2E: synthetic pixel buffer creation failed loudly"
        );
        let buffer: Retained<CVPixelBuffer> =
            unsafe { Retained::from_raw(raw).expect("created buffer is non-null") };
        assert_eq!(
            unsafe { CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags::empty()) },
            kCVReturnSuccess,
            "loopback E2E: pixel buffer lock failed loudly"
        );
        let bytes_per_row = unsafe { CVPixelBufferGetBytesPerRow(&buffer) };
        let base = unsafe { CVPixelBufferGetBaseAddress(&buffer) } as *mut u8;
        assert!(
            !base.is_null(),
            "loopback E2E: pixel buffer has no base address"
        );
        // Moving-bar gradient so successive frames actually differ.
        for y in 0..height as usize {
            for x in 0..width as usize {
                let v = (x.wrapping_add(y).wrapping_add(index.wrapping_mul(7)) % 256) as u8;
                let offset = y * bytes_per_row + x * 4;
                unsafe {
                    base.add(offset).write_volatile(v);
                    base.add(offset + 1).write_volatile(255 - v);
                    base.add(offset + 2).write_volatile(v / 2);
                    base.add(offset + 3).write_volatile(255);
                }
            }
        }
        unsafe { CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags::empty()) };
        if let Err(error) = encoder_tx.send(EncoderCommand::Video(NativeEncoderFrame {
            pixel_buffer: buffer,
            timestamp_micros: (first_sequence + index as u64) * 33_333,
            generation,
        })) {
            let failure = engine.state.lock().ok().and_then(|guard| {
                guard
                    .session
                    .as_ref()
                    .and_then(|session| session._pipeline.state.failure())
            });
            panic!(
                "loopback E2E milestone missing: encoder accepted synthetic frame {index} \
                 ({error:?}; pipeline failure: {failure:?})"
            );
        }
    }
}

#[cfg(target_os = "windows")]
fn push_synthetic_frames(
    engine: &MediaEngine,
    width: u32,
    height: u32,
    count: usize,
    first_sequence: u64,
) {
    use super::super::pipeline::NativeFrame;

    let (encoder_tx, generation) = {
        let guard = engine.state.lock().expect("engine state is available");
        let session = guard.session.as_ref().expect("session must exist");
        (session._pipeline.encoder_tx.clone(), session.generation)
    };
    let base_micros = 0u64;
    for index in 0..count {
        let mut bgra = vec![0u8; width as usize * height as usize * 4];
        for (i, px) in bgra.chunks_exact_mut(4).enumerate() {
            let v = (i.wrapping_add(index.wrapping_mul(7)) % 256) as u8;
            px[0] = v;
            px[1] = 255 - v;
            px[2] = v / 2;
            px[3] = 255;
        }
        encoder_tx
            .send(EncoderCommand::Video(NativeFrame {
                storage: Arc::from(bgra.into_boxed_slice()),
                timestamp_micros: base_micros + (first_sequence + index as u64) * 33_333,
                sequence: first_sequence + index as u64,
                width,
                height,
                generation,
            }))
            .expect("loopback E2E: encoder queue must accept synthetic frames");
    }
}

fn request_keyframe(engine: &MediaEngine) {
    let guard = engine.state.lock().expect("engine state is available");
    let session = guard.session.as_ref().expect("session must exist");
    session._pipeline.encoder_control.request_keyframe();
}

fn assert_pipeline_healthy(engine: &MediaEngine, what: &str) {
    let guard = engine.state.lock().expect("engine state is available");
    let session = guard.session.as_ref().expect("session must exist");
    assert!(
        session._pipeline.state.failure().is_none(),
        "loopback E2E milestone missing: {what} pipeline healthy, got {:?}",
        session._pipeline.state.failure(),
    );
}

/// Drive the full media leg: feed synthetic frames until an SPS+PPS+IDR
/// arrives intact through the real packetization path, or fail loudly.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn wait_first_idr(
    host: &MediaEngine,
    rtp_rx: &mut tokio::sync::mpsc::UnboundedReceiver<(u32, Vec<u8>)>,
    runtime: &tokio::runtime::Runtime,
    what: &str,
) -> NalSeen {
    let (width, height) = VideoResolution::P720.max_dimensions();
    push_synthetic_frames(host, width, height, 30, 0);
    request_keyframe(host);
    let mut seen = NalSeen::default();
    let mut fu_buffers: HashMap<u32, Vec<Vec<u8>>> = HashMap::new();
    let mut fed_more = false;
    runtime.block_on(async {
        let deadline = Instant::now() + IDR_WAIT;
        // Keep the encoder fed while waiting: one paced frame per tick.
        let mut sequence = 30u64;
        loop {
            if seen.decode_ok() {
                break;
            }
            if Instant::now() > deadline {
                panic!(
                    "loopback E2E milestone missing: {what} first-IDR-with-SPS/PPS (seen {seen:?})"
                );
            }
            match tokio::time::timeout(Duration::from_millis(500), rtp_rx.recv()).await {
                Ok(Some((timestamp, payload))) => {
                    feed_rtp_payload(&mut seen, &mut fu_buffers, timestamp, &payload);
                    // Drain FU-A fragments that never saw an end bit: they
                    // cannot decode, drop them so a later IDR still counts.
                    fu_buffers.retain(|_, fragments| fragments.len() < 256);
                }
                _ => {
                    if !fed_more {
                        push_synthetic_frames(host, width, height, 60, sequence);
                        sequence += 60;
                        request_keyframe(host);
                        fed_more = true;
                    }
                }
            }
        }
    });
    assert_pipeline_healthy(host, what);
    seen
}

/// Watch → host-minted offer → test-viewer answer → connected, then media.
/// Returns the viewer member id that now holds a Connected link on `host`.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn watch_offer_answer_media(
    host: &MediaEngine,
    joiner: &MediaEngine,
    joiner_self: &str,
    host_id: &str,
    what: &str,
) -> NalSeen {
    // The joiner must already see the host sharing (Watch gate).
    let snapshot = wait_share_visible(joiner, joiner_self, host_id);
    assert_eq!(
        snapshot.self_id.as_deref(),
        Some(joiner_self),
        "loopback E2E: joiner self must be exclusively its viewer member id"
    );
    // Drop any stale offers, then watch.
    let _ = joiner.poll_incoming_offers();
    joiner
        .request_watch(host_id, true)
        .expect("loopback E2E: watch must send");
    // Host mints + sends exactly once; the offer arrives on the joiner.
    let incoming = poll_until(
        &format!("{what} offer arrives on the joiner"),
        OFFER_WAIT,
        || {
            joiner
                .poll_incoming_offers()
                .into_iter()
                .find(|offer| !offer.sdp.is_empty())
        },
    );
    assert_eq!(
        incoming.from, host_id,
        "loopback E2E: {what} offer must come from the sharing host"
    );
    assert_has_candidates(&incoming.sdp, &format!("{what} host offer"));
    let fence = incoming
        .fence
        .as_ref()
        .unwrap_or_else(|| panic!("loopback E2E milestone missing: {what} fenced offer"));
    let fence_json = serde_json::to_string(fence).expect("loopback E2E: fence must serialize");
    // Answer with the raw webrtc viewer (the browser lane, in-process).
    // OLD frontend envelope on purpose: the answer carries the SHARER id,
    // not the watcher id. Fence-first matching applies it anyway, so
    // decode_ok below proves either-layer sufficiency (the frontend may
    // send either shape and the host still connects).
    let (rtp_tx, mut rtp_rx) = tokio::sync::mpsc::unbounded_channel();
    let (viewer, answer_sdp) = LoopbackViewer::answer(&incoming.sdp, rtp_tx);
    assert_has_candidates(&answer_sdp, &format!("{what} viewer answer"));
    assert_eq!(
        incoming.from, host_id,
        "loopback E2E: {what} answer must target the sharing host"
    );
    joiner
        .submit_stunar_answer(
            PeerSignal {
                kind: PeerSignalKind::Answer,
                sdp: answer_sdp,
                id: Some(host_id.to_owned()),
            },
            fence_json,
        )
        .expect("loopback E2E: answer must submit");
    // Both ends connect: host link Connected, viewer PC Connected. The
    // viewer wait runs on a scoped thread because each side only makes ICE
    // progress while its own runtime is driven.
    std::thread::scope(|scope| {
        scope.spawn(|| viewer.wait_connected(what, host, joiner_self));
        wait_link_connected(host, joiner_self, &viewer, what);
    });
    // Media: first IDR with SPS/PPS through the real packetization path.
    wait_first_idr(host, &mut rtp_rx, &viewer.runtime, what)
}

/// End-of-test teardown. NOTE: this deliberately does NOT call
/// `stop_session` on Stunar hosts: the stop worker builds a
/// `tokio::time::timeout` outside any runtime context and panics
/// (`rendezvous.rs` `shutdown_and_join`, callee: session-stop thread), so a
/// Stunar stop can never complete off-runtime. Dropping the engines instead
/// closes every WS via the `Drop` impls (panic-free) and the per-test server
/// dies with this function scope right after. Filed as a latent production
/// issue alongside this E2E; the media/signaling assertions above are
/// unaffected.
fn teardown(host: MediaEngine, joiner: MediaEngine) {
    let _ = host.announce_share(false);
    joiner.close_stunar_viewer();
    drop(host);
    drop(joiner);
}

#[test]
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn sala_forward_decode_ok() {
    let server = TestServer::spawn(18781);
    let host = worker_engine(None, None);
    let joiner = worker_engine(None, None);

    host.create_session(sala_host_request("LoopA", &server.base))
        .expect("loopback E2E: host session must start");
    announce_and_force(&host);
    let host_id = host
        .snapshot()
        .self_id
        .clone()
        .expect("loopback E2E: host self must be its host member id");
    let code = host
        .snapshot()
        .session_code
        .clone()
        .expect("loopback E2E: host must publish a room code");

    joiner
        .discover_stunar(&server.base, &code, PASSWORD, "LoopB")
        .expect("loopback E2E: joiner must discover the Sala room");
    let joiner_self = joiner
        .snapshot()
        .self_id
        .clone()
        .expect("loopback E2E: joiner self must be its viewer member id");
    assert_ne!(
        joiner_self, host_id,
        "loopback E2E: viewer member id must differ from host id"
    );

    let seen = watch_offer_answer_media(&host, &joiner, &joiner_self, &host_id, "sala-forward");
    assert!(
        seen.decode_ok(),
        "loopback E2E: sala-forward decode_ok, seen {seen:?}"
    );

    teardown(host, joiner);
}

#[test]
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn sala_reverse_decode_ok() {
    let server = TestServer::spawn(18782);
    // B hosts this room; A joins as a viewer (dual-role on A only if A also
    // hosted — here A is viewer-only, B is host-only).
    let host_b = worker_engine(None, None);
    let viewer_a = worker_engine(None, None);

    host_b
        .create_session(sala_host_request("LoopB", &server.base))
        .expect("loopback E2E: B host session must start");
    announce_and_force(&host_b);
    let host_b_id = host_b
        .snapshot()
        .self_id
        .clone()
        .expect("loopback E2E: B self must be its host member id");
    let code = host_b
        .snapshot()
        .session_code
        .clone()
        .expect("loopback E2E: B must publish a room code");

    viewer_a
        .discover_stunar(&server.base, &code, PASSWORD, "LoopA")
        .expect("loopback E2E: A must discover B's Sala room");
    let viewer_a_self = viewer_a
        .snapshot()
        .self_id
        .clone()
        .expect("loopback E2E: A self must be its viewer member id");

    // A watches B: the target is in A's viewer roster, so the watch MUST
    // leave on the viewer socket. Had it gone via a host socket it would
    // target A's own (nonexistent here) room and B would never mint.
    let seen = watch_offer_answer_media(
        &host_b,
        &viewer_a,
        &viewer_a_self,
        &host_b_id,
        "sala-reverse",
    );
    assert!(
        seen.decode_ok(),
        "loopback E2E: sala-reverse decode_ok, seen {seen:?}"
    );

    viewer_a.close_stunar_viewer();
    drop(host_b);
}

#[test]
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn broadcast_decode_ok() {
    let server = TestServer::spawn(18783);
    let host = worker_engine(None, None);
    let joiner = worker_engine(None, None);

    host.create_session(broadcast_host_request("LoopD", &server.base))
        .expect("loopback E2E: broadcast host session must start");
    announce_and_force(&host);
    // Broadcast hosts carry no member id by protocol design (only Sala room
    // members do): session-branch self stays None, exclusively.
    assert!(
        host.snapshot().self_id.is_none(),
        "loopback E2E: broadcast host self must be None (no member id by design)"
    );
    let code = host
        .snapshot()
        .session_code
        .clone()
        .expect("loopback E2E: broadcast host must publish a room code");

    // Broadcast mints on accept: the handshake itself returns the offer,
    // stamped from="host" (there is no host member id to address).
    let (_token, offer) = joiner
        .discover_stunar(&server.base, &code, PASSWORD, "LoopE")
        .expect("loopback E2E: joiner must discover the Broadcast room");
    assert!(
        !offer.signal.sdp.is_empty(),
        "loopback E2E milestone missing: broadcast handshake offer"
    );
    assert_eq!(
        offer.signal.id.as_deref(),
        Some("host"),
        "loopback E2E: broadcast offer must be stamped from the host"
    );
    let joiner_self = joiner
        .snapshot()
        .self_id
        .clone()
        .expect("loopback E2E: joiner self must be its viewer member id");
    // Broadcast rooms carry no share flags: the join is proven by the
    // handshake offer above, not by a roster share bit.
    assert_has_candidates(&offer.signal.sdp, "broadcast handshake offer");

    let (rtp_tx, mut rtp_rx) = tokio::sync::mpsc::unbounded_channel();
    let (viewer, answer_sdp) = LoopbackViewer::answer(&offer.signal.sdp, rtp_tx);
    assert_has_candidates(&answer_sdp, "broadcast viewer answer");
    joiner
        .submit_stunar_answer(
            PeerSignal {
                kind: PeerSignalKind::Answer,
                sdp: answer_sdp,
                id: Some(joiner_self.clone()),
            },
            offer.offer_attempt.clone(),
        )
        .expect("loopback E2E: broadcast answer must submit");
    std::thread::scope(|scope| {
        scope.spawn(|| viewer.wait_connected("broadcast", &host, &joiner_self));
        wait_link_connected(&host, &joiner_self, &viewer, "broadcast");
    });
    let seen = wait_first_idr(&host, &mut rtp_rx, &viewer.runtime, "broadcast");
    assert!(
        seen.decode_ok(),
        "loopback E2E: broadcast decode_ok, seen {seen:?}"
    );

    teardown(host, joiner);
}

#[test]
fn dualrole_self_watch_skips_mint() {
    let server = TestServer::spawn(18784);
    let engine = worker_engine(None, None);
    let other = worker_engine(None, None);

    engine
        .create_session(sala_host_request("LoopC", &server.base))
        .expect("loopback E2E: C host session must start");
    announce_and_force(&engine);
    let host_id = engine
        .snapshot()
        .self_id
        .clone()
        .expect("loopback E2E: C self must be its host member id");
    let code = engine
        .snapshot()
        .session_code
        .clone()
        .expect("loopback E2E: C must publish a room code");

    // C joins its own room: dual-role in one engine.
    engine
        .discover_stunar(&server.base, &code, PASSWORD, "LoopC-viewer")
        .expect("loopback E2E: C must join its own room as viewer");
    let viewer_id = {
        let snapshot = engine.snapshot();
        // Session branch: self is EXCLUSIVELY the host id, and the viewer
        // id is exposed separately so the UI cannot conflate them.
        assert_eq!(
            snapshot.self_id.as_deref(),
            Some(host_id.as_str()),
            "loopback E2E: dual-role self_id must be the host id, never the viewer id"
        );
        assert!(
            snapshot
                .viewer_member_id
                .as_deref()
                .is_some_and(|id| id != host_id),
            "loopback E2E: dual-role must expose a distinct viewer_member_id"
        );
        snapshot.viewer_member_id.clone().expect("viewer id")
    };
    // Wait until the viewer roster knows the host (routing precondition).
    poll_until(
        "dual-role viewer roster knows the host",
        ROSTER_WAIT,
        || {
            let snapshot = engine.snapshot();
            watch_enabled(&snapshot, Some(viewer_id.as_str()), &host_id).then_some(())
        },
    );

    // Positive control: another member's watch DOES mint (delivery works).
    other
        .discover_stunar(&server.base, &code, PASSWORD, "LoopD2")
        .expect("loopback E2E: second viewer must join");
    let other_self = other
        .snapshot()
        .self_id
        .clone()
        .expect("loopback E2E: second viewer needs its member id");
    poll_until(
        "dual-role viewer roster knows the second member",
        ROSTER_WAIT,
        || {
            let snapshot = engine.snapshot();
            snapshot
                .roster
                .iter()
                .any(|entry| entry.id == other_self)
                .then_some(())
        },
    );
    other
        .request_watch(&host_id, true)
        .expect("loopback E2E: control watch must send");
    poll_until("control watch mints a link", OFFER_WAIT, || {
        let guard = engine.state.lock().expect("engine state is available");
        guard
            .session
            .as_ref()
            .and_then(|session| session.viewers.get(&other_self))
            .map(|_| ())
    });

    // The actual assertion: watching SELF mints nothing (self-skip fires in
    // the viewer role; mint-skip-self milestone). The control above proves
    // watches are being delivered and drained.
    engine
        .request_watch(&host_id, true)
        .expect("loopback E2E: self watch must send");
    std::thread::sleep(NO_LINK_WAIT);
    // Pump more commands through so any pending drain definitely ran.
    let _ = engine.snapshot();
    std::thread::sleep(NO_LINK_WAIT);
    {
        let guard = engine.state.lock().expect("engine state is available");
        let session = guard.session.as_ref().expect("session must exist");
        assert!(
            !session.viewers.contains_key(&viewer_id),
            "loopback E2E milestone missing: self-skip must leave no link for the own viewer member"
        );
        assert!(
            session.viewers.contains_key(&other_self),
            "loopback E2E: control link must still exist"
        );
    }

    other.close_stunar_viewer();
    drop(engine);
    drop(other);
}

#[test]
fn rtp_nal_parser_reassembles_fu_a_and_stap_a() {
    let mut seen = NalSeen::default();
    let mut fu_buffers: HashMap<u32, Vec<Vec<u8>>> = HashMap::new();
    // Single NAL IDR.
    feed_rtp_payload(&mut seen, &mut fu_buffers, 1, &[0x65, 0xAA, 0xBB]);
    assert!(seen.idr && !seen.sps && !seen.pps);
    // STAP-A carrying SPS (profile Baseline) + PPS.
    feed_rtp_payload(
        &mut seen,
        &mut fu_buffers,
        2,
        &[24, 0, 3, 0x67, 0x42, 0x00, 0, 2, 0x68, 0xCE],
    );
    assert!(seen.sps && seen.pps);
    assert_eq!(seen.sps_profile, Some(BASELINE_PROFILE_IDC));
    // FU-A split IDR across two fragments (start + end).
    feed_rtp_payload(&mut seen, &mut fu_buffers, 3, &[0x7C, 0x85, 0x01, 0x02]);
    assert!(seen.decode_ok());
    feed_rtp_payload(&mut seen, &mut fu_buffers, 3, &[0x7C, 0x45, 0x03, 0x04]);
    assert!(seen.decode_ok());
    // Incomplete FU-A (no end bit) never counts.
    let mut partial = NalSeen::default();
    let mut buffers: HashMap<u32, Vec<Vec<u8>>> = HashMap::new();
    feed_rtp_payload(&mut partial, &mut buffers, 9, &[0x7C, 0x85, 0x01]);
    assert!(!partial.idr);
}

#[test]
fn watch_gate_needs_share_and_non_self_target() {
    use super::super::types::RosterEntry;
    let snapshot = MediaSessionSnapshot {
        roster: vec![RosterEntry {
            id: "a".into(),
            nickname: "A".into(),
            state: PeerTransportState::Connected,
            master: false,
            share: true,
        }],
        self_id: Some("b".into()),
        viewer_member_id: Some("b".into()),
        ..MediaSessionSnapshot::idle("test")
    };
    assert!(watch_enabled(&snapshot, Some("b"), "a"));
    assert!(!watch_enabled(&snapshot, Some("a"), "a"));
    assert!(!watch_enabled(&snapshot, Some("b"), "missing"));
}
