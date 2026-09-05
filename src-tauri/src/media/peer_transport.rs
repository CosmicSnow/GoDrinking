//! Native, local-only WebRTC sender for encoded Annex-B H.264 samples.

use super::access_unit::AccessUnitReceiver;
use super::pipeline::EncoderControl;
use super::process_tap::EncodedAudioPacket;
use super::types::{JoinMode, PeerTransportState, VideoCodec};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{
    MediaEngine, MIME_TYPE_AV1, MIME_TYPE_H264, MIME_TYPE_HEVC, MIME_TYPE_OPUS,
};
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::ice::mdns::MulticastDnsMode;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::media::Sample;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtcp::payload_feedbacks::receiver_estimated_maximum_bitrate::ReceiverEstimatedMaximumBitrate;
use webrtc::rtcp::receiver_report::ReceiverReport;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};
use webrtc::rtp_transceiver::RTCPFeedback;
use webrtc::stats::StatsReportType;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;

const TRANSPORT_COMMAND_CAPACITY: usize = 8;
const TRANSPORT_SAMPLE_CAPACITY: usize = 16;
const ICE_GATHER_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const PEER_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
const WORKER_COMPLETION_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PeerSignal {
    #[serde(rename = "type")]
    pub kind: PeerSignalKind,
    pub sdp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PeerSignalKind {
    Offer,
    Answer,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PeerTransportStatus {
    pub(crate) state: PeerTransportState,
    pub(crate) detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShutdownStatus {
    pub(crate) quiesced: bool,
    pub(crate) pending: Vec<&'static str>,
    pub(crate) errors: Vec<String>,
}

pub(crate) enum PeerTransportInitError {
    Failed(String),
    Pending(PendingPeerTransport),
}

pub(crate) struct PendingPeerTransport {
    command_tx: SyncSender<PeerCommand>,
    shutdown: Arc<AtomicBool>,
    completion: Arc<WorkerCompletion>,
    worker: Option<JoinHandle<()>>,
}

impl PendingPeerTransport {
    pub(crate) fn shutdown_and_join(&mut self, timeout: Duration) -> ShutdownStatus {
        self.shutdown.store(true, Ordering::Release);
        let _ = self.command_tx.try_send(PeerCommand::Close);
        let mut status = ShutdownStatus {
            quiesced: true,
            pending: Vec::new(),
            errors: Vec::new(),
        };
        let Some(worker) = self.worker.as_ref() else {
            return status;
        };
        let deadline = std::time::Instant::now() + timeout;
        let Ok(mut done) = self.completion.done.lock() else {
            status.quiesced = false;
            status
                .errors
                .push("peer completion state is poisoned".into());
            return status;
        };
        if !*done {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                status.quiesced = false;
                status.pending.push("peer initialization worker");
                return status;
            }
            let Ok((next, _)) = self.completion.wake.wait_timeout(done, remaining) else {
                status.quiesced = false;
                status.errors.push("peer completion wait failed".into());
                return status;
            };
            done = next;
        }
        if !*done || !worker.is_finished() {
            status.quiesced = false;
            status.pending.push("peer initialization worker");
            return status;
        }
        let worker = self.worker.take().expect("pending peer worker handle");
        if worker.join().is_err() {
            status.quiesced = false;
            status
                .errors
                .push("peer initialization worker panicked".into());
        }
        status
    }
}

struct SharedStatus {
    state: PeerTransportState,
    detail: String,
}

struct WorkerCompletion {
    done: Mutex<bool>,
    wake: std::sync::Condvar,
}

enum PeerCommand {
    CreateOffer(SyncSender<Result<PeerSignal, String>>),
    AcceptOffer(PeerSignal, SyncSender<Result<PeerSignal, String>>),
    SetAnswer(PeerSignal, SyncSender<Result<(), String>>),
    GetRtt(SyncSender<Option<f64>>),
    Close,
}

pub(crate) struct PeerTransport {
    command_tx: SyncSender<PeerCommand>,
    status: Arc<Mutex<SharedStatus>>,
    shutdown: Arc<AtomicBool>,
    completion: Arc<WorkerCompletion>,
    worker: Option<JoinHandle<()>>,
}

#[derive(Clone)]
pub(crate) struct PeerTransportClient {
    command_tx: SyncSender<PeerCommand>,
    shutdown: Arc<AtomicBool>,
}

impl PeerTransport {
    pub(crate) fn new(
        access_units: AccessUnitReceiver,
        audio_packets: Option<Receiver<EncodedAudioPacket>>,
        encoder_control: Arc<EncoderControl>,
        frame_duration: Duration,
        join_mode: JoinMode,
        video_codec: VideoCodec,
    ) -> Result<Self, String> {
        match Self::new_with_initialization(
            access_units,
            audio_packets,
            encoder_control,
            frame_duration,
            join_mode,
            video_codec,
        ) {
            Ok(peer) => Ok(peer),
            Err(PeerTransportInitError::Failed(error)) => Err(error),
            Err(PeerTransportInitError::Pending(mut pending)) => {
                let status = pending.shutdown_and_join(WORKER_COMPLETION_TIMEOUT);
                Err(format!("peer initialization cleanup pending: {status:?}"))
            }
        }
    }

    pub(crate) fn new_with_initialization(
        access_units: AccessUnitReceiver,
        audio_packets: Option<Receiver<EncodedAudioPacket>>,
        encoder_control: Arc<EncoderControl>,
        frame_duration: Duration,
        join_mode: JoinMode,
        video_codec: VideoCodec,
    ) -> Result<Self, PeerTransportInitError> {
        let (command_tx, command_rx) = sync_channel(TRANSPORT_COMMAND_CAPACITY);
        let (ready_tx, ready_rx) = sync_channel(1);
        let status = Arc::new(Mutex::new(SharedStatus {
            state: PeerTransportState::Starting,
            detail: "Creating local-only WebRTC peer.".into(),
        }));
        let shutdown = Arc::new(AtomicBool::new(false));
        let completion = Arc::new(WorkerCompletion {
            done: Mutex::new(false),
            wake: std::sync::Condvar::new(),
        });
        let worker_status = Arc::clone(&status);
        let worker_shutdown = Arc::clone(&shutdown);
        let worker_completion = Arc::clone(&completion);
        let worker = thread::Builder::new()
            .name("godrinking-webrtc-peer".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        set_status(
                            &worker_status,
                            PeerTransportState::Failed,
                            format!("WebRTC runtime initialization failed: {error}"),
                        );
                        let _ = ready_tx.send(Err(error.to_string()));
                        mark_worker_complete(&worker_completion);
                        return;
                    }
                };
                runtime.block_on(run_peer(
                    command_rx,
                    access_units,
                    audio_packets,
                    encoder_control,
                    frame_duration,
                    join_mode,
                    video_codec,
                    ready_tx,
                    worker_status,
                    worker_shutdown,
                ));
                mark_worker_complete(&worker_completion);
            })
            .map_err(|error| {
                PeerTransportInitError::Failed(format!("failed to start WebRTC worker: {error}"))
            })?;

        let ready = match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(ready) => ready,
            Err(_) => {
                shutdown.store(true, Ordering::Release);
                let _ = command_tx.try_send(PeerCommand::Close);
                return Err(PeerTransportInitError::Pending(PendingPeerTransport {
                    command_tx,
                    shutdown,
                    completion,
                    worker: Some(worker),
                }));
            }
        };
        match ready {
            Ok(()) => Ok(Self {
                command_tx,
                status,
                shutdown,
                completion,
                worker: Some(worker),
            }),
            Err(error) => {
                shutdown.store(true, Ordering::Release);
                finish_worker(worker, &completion);
                Err(PeerTransportInitError::Failed(error))
            }
        }
    }

    pub(crate) fn status(&self) -> PeerTransportStatus {
        self.status
            .lock()
            .map(|status| PeerTransportStatus {
                state: status.state.clone(),
                detail: status.detail.clone(),
            })
            .unwrap_or(PeerTransportStatus {
                state: PeerTransportState::Failed,
                detail: "WebRTC peer state is unavailable.".into(),
            })
    }

    pub(crate) fn client(&self) -> PeerTransportClient {
        PeerTransportClient {
            command_tx: self.command_tx.clone(),
            shutdown: Arc::clone(&self.shutdown),
        }
    }

    pub(crate) fn close(&mut self) {
        let status = self.shutdown_and_join(PEER_CLOSE_TIMEOUT + WORKER_COMPLETION_TIMEOUT);
        if !status.quiesced {
            eprintln!("[goDrinking] peer cleanup incomplete: {status:?}");
        }
    }

    /// Requests peer shutdown and joins the native WebRTC worker. If the
    /// deadline expires, the worker handle remains owned here for a later
    /// retry and the pending component is returned to the caller.
    pub(crate) fn shutdown_and_join(&mut self, timeout: Duration) -> ShutdownStatus {
        self.shutdown.store(true, Ordering::Release);
        let _ = self.command_tx.try_send(PeerCommand::Close);
        let mut status = ShutdownStatus {
            quiesced: true,
            pending: Vec::new(),
            errors: Vec::new(),
        };
        let Some(worker) = self.worker.as_ref() else {
            return status;
        };
        let deadline = std::time::Instant::now() + timeout;
        let Ok(mut done) = self.completion.done.lock() else {
            status.quiesced = false;
            status
                .errors
                .push("peer completion state is poisoned".into());
            return status;
        };
        if !*done {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                status.quiesced = false;
                status.pending.push("peer worker");
                return status;
            }
            let Ok((next, _)) = self.completion.wake.wait_timeout(done, remaining) else {
                status.quiesced = false;
                status.errors.push("peer completion wait failed".into());
                return status;
            };
            done = next;
        }
        if !*done {
            status.quiesced = false;
            status.pending.push("peer worker");
            return status;
        }
        drop(done);
        if !worker.is_finished() {
            status.quiesced = false;
            status.pending.push("peer worker");
            return status;
        }
        let worker = self
            .worker
            .take()
            .expect("peer worker handle still present");
        if worker.join().is_err() {
            status.quiesced = false;
            status.errors.push("peer worker panicked".into());
        }
        status
    }
}

impl PeerTransportClient {
    pub(crate) fn create_offer(&self) -> Result<PeerSignal, String> {
        let (response_tx, response_rx) = sync_channel(1);
        self.command_tx
            .try_send(PeerCommand::CreateOffer(response_tx))
            .map_err(|error| format!("WebRTC offer request rejected: {error}"))?;
        receive_response(response_rx, "WebRTC offer request")
    }

    pub(crate) fn accept_offer(&self, offer: PeerSignal) -> Result<PeerSignal, String> {
        let (response_tx, response_rx) = sync_channel(1);
        self.command_tx
            .try_send(PeerCommand::AcceptOffer(offer, response_tx))
            .map_err(|error| format!("WebRTC answer request rejected: {error}"))?;
        receive_response(response_rx, "WebRTC answer request")
    }

    pub(crate) fn set_answer(&self, answer: PeerSignal) -> Result<(), String> {
        let (response_tx, response_rx) = sync_channel(1);
        self.command_tx
            .try_send(PeerCommand::SetAnswer(answer, response_tx))
            .map_err(|error| format!("WebRTC set-answer request rejected: {error}"))?;
        receive_response(response_rx, "WebRTC set-answer request")
    }

    /// Current round-trip time in milliseconds, measured via get_stats().
    /// None while unmeasured (pre-first STUN response / no RTCP yet).
    pub(crate) fn rtt_ms(&self) -> Option<f64> {
        let (response_tx, response_rx) = sync_channel(1);
        self.command_tx
            .try_send(PeerCommand::GetRtt(response_tx))
            .ok()?;
        response_rx
            .recv_timeout(Duration::from_secs(2))
            .ok()
            .flatten()
    }

    pub(crate) fn request_close(&self) -> Result<(), String> {
        self.shutdown.store(true, Ordering::Release);
        match self.command_tx.try_send(PeerCommand::Close) {
            Ok(()) | Err(std::sync::mpsc::TrySendError::Full(_)) => Ok(()),
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                Err("WebRTC peer worker is closed".into())
            }
        }
    }
}

fn receive_response<T>(
    receiver: Receiver<Result<T, String>>,
    operation: &str,
) -> Result<T, String> {
    receive_response_with_timeout(receiver, REQUEST_TIMEOUT, operation)
}

fn receive_response_with_timeout<T>(
    receiver: Receiver<Result<T, String>>,
    timeout: Duration,
    operation: &str,
) -> Result<T, String> {
    receiver
        .recv_timeout(timeout)
        .map_err(|_| format!("{operation} timed out"))?
}

fn mark_worker_complete(completion: &WorkerCompletion) {
    if let Ok(mut done) = completion.done.lock() {
        *done = true;
        completion.wake.notify_all();
    }
}

fn finish_worker(worker: JoinHandle<()>, completion: &Arc<WorkerCompletion>) {
    let _ = finish_worker_with_timeout(worker, completion, WORKER_COMPLETION_TIMEOUT);
}

fn finish_worker_with_timeout(
    worker: JoinHandle<()>,
    completion: &Arc<WorkerCompletion>,
    timeout: Duration,
) -> Result<(), String> {
    let Ok(mut done) = completion.done.lock() else {
        drop(worker);
        return Err("peer completion state is poisoned".into());
    };
    if !*done {
        let Ok((next, _)) = completion.wake.wait_timeout(done, timeout) else {
            drop(worker);
            return Err("peer completion wait failed".into());
        };
        done = next;
    }
    let completed = *done;
    drop(done);
    if completed {
        worker.join().map_err(|_| "peer worker panicked".to_owned())
    } else {
        // Initialization has no PeerTransport owner to retain this handle;
        // make the cleanup-pending result visible to the caller instead of
        // treating a detached worker as quiescent.
        drop(worker);
        Err("peer worker cleanup pending after deadline".into())
    }
}

impl Drop for PeerTransport {
    fn drop(&mut self) {
        self.close();
    }
}

async fn run_peer(
    command_rx: Receiver<PeerCommand>,
    access_units: AccessUnitReceiver,
    audio_packets: Option<Receiver<EncodedAudioPacket>>,
    encoder_control: Arc<EncoderControl>,
    frame_duration: Duration,
    join_mode: JoinMode,
    video_codec: VideoCodec,
    ready_tx: SyncSender<Result<(), String>>,
    status: Arc<Mutex<SharedStatus>>,
    shutdown: Arc<AtomicBool>,
) {
    // Only the session codec is registered: the offer carries exactly what
    // the encoder produces, so a viewer can never negotiate a codec the
    // host is not sending.
    let mut media_engine = MediaEngine::default();
    let session_codec = match video_codec {
        VideoCodec::H264 | VideoCodec::H264High => h264_codec(
            video_codec
                .h264_profile_level_id()
                .unwrap_or(super::types::H264_BASELINE_PROFILE_LEVEL_ID),
        ),
        VideoCodec::Hevc => hevc_codec(),
        VideoCodec::Av1 => av1_codec(),
    };
    if let Err(error) = media_engine.register_codec(session_codec.clone(), RTPCodecType::Video) {
        let message = format!(
            "{} codec registration failed: {error}",
            video_codec.mime_type()
        );
        set_status(&status, PeerTransportState::Failed, message.clone());
        let _ = ready_tx.send(Err(message));
        return;
    }
    if audio_packets.is_some() {
        if let Err(error) = media_engine.register_codec(opus_codec(), RTPCodecType::Audio) {
            let message = format!("Opus codec registration failed: {error}");
            set_status(&status, PeerTransportState::Failed, message.clone());
            let _ = ready_tx.send(Err(message));
            return;
        }
    }
    let registry = match register_default_interceptors(Registry::new(), &mut media_engine) {
        Ok(registry) => registry,
        Err(error) => {
            let message = format!("WebRTC interceptor registration failed: {error}");
            set_status(&status, PeerTransportState::Failed, message.clone());
            let _ = ready_tx.send(Err(message));
            return;
        }
    };
    let mut setting_engine = SettingEngine::default();
    // Same-machine Watch/Share needs 127.0.0.1. WKWebView host candidates are
    // mDNS (`*.local`); webrtc-rs mDNS on macOS often fails to bind :5353, so
    // disable it and rewrite those addresses before setRemoteDescription.
    setting_engine.set_include_loopback_candidate(true);
    setting_engine.set_ice_multicast_dns_mode(MulticastDnsMode::Disabled);
    let api = APIBuilder::new()
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .with_setting_engine(setting_engine)
        .build();
    // LAN stays host/local-only (no internet needed). Direct and Stunar use
    // the public STUN mirror so host candidates behind NAT can be discovered.
    // No TURN is ever configured.
    let configuration = RTCConfiguration {
        ice_servers: if join_mode == JoinMode::Lan {
            Vec::new()
        } else {
            vec![RTCIceServer {
                urls: vec!["stun:stun.l.google.com:19302".into()],
                ..Default::default()
            }]
        },
        ..Default::default()
    };
    let peer = match run_signaling_operation(
        async {
            api.new_peer_connection(configuration)
                .await
                .map(Arc::new)
                .map_err(|error| format!("WebRTC peer creation failed: {error}"))
        },
        Arc::clone(&shutdown),
        "WebRTC peer initialization",
    )
    .await
    {
        Ok(peer) => peer,
        Err(message) => {
            set_status(&status, PeerTransportState::Failed, message.clone());
            let _ = ready_tx.send(Err(message));
            return;
        }
    };
    let track = Arc::new(TrackLocalStaticSample::new(
        session_codec.capability,
        "godrinking-video".into(),
        "godrinking".into(),
    ));
    let track_for_peer: Arc<dyn TrackLocal + Send + Sync> = track.clone();
    let sender = match run_signaling_operation(
        async {
            peer.add_track(track_for_peer)
                .await
                .map_err(|error| format!("WebRTC video track registration failed: {error}"))
        },
        Arc::clone(&shutdown),
        "WebRTC track initialization",
    )
    .await
    {
        Ok(sender) => sender,
        Err(message) => {
            set_status(&status, PeerTransportState::Failed, message.clone());
            let _ = ready_tx.send(Err(message));
            let _ = close_peer_with_deadline(&peer).await;
            return;
        }
    };
    let peer_status = Arc::clone(&status);
    let state_keyframe_control = Arc::clone(&encoder_control);
    peer.on_peer_connection_state_change(Box::new(move |state| {
        let peer_status = Arc::clone(&peer_status);
        let state_keyframe_control = Arc::clone(&state_keyframe_control);
        Box::pin(async move {
            let mapped_state = map_peer_state(state);
            set_status(
                &peer_status,
                mapped_state.clone(),
                format!("WebRTC peer connection state: {state}"),
            );
            if mapped_state == PeerTransportState::Connected {
                state_keyframe_control.request_keyframe();
            }
        })
    }));
    set_status(
        &status,
        PeerTransportState::New,
        "Local-only WebRTC peer is ready.".into(),
    );
    let audio_track = if audio_packets.is_some() {
        let track = Arc::new(TrackLocalStaticSample::new(
            opus_codec().capability,
            "godrinking-audio".into(),
            "godrinking".into(),
        ));
        let track_for_peer: Arc<dyn TrackLocal + Send + Sync> = track.clone();
        if let Err(error) = peer.add_track(track_for_peer).await {
            let message = format!("WebRTC Opus track registration failed: {error}");
            set_status(&status, PeerTransportState::Failed, message.clone());
            let _ = ready_tx.send(Err(message));
            let _ = close_peer_with_deadline(&peer).await;
            return;
        }
        Some(track)
    } else {
        None
    };
    if ready_tx.send(Ok(())).is_err() {
        let _ = close_peer_with_deadline(&peer).await;
        return;
    }

    let (sample_tx, mut sample_rx) = mpsc::channel(TRANSPORT_SAMPLE_CAPACITY);
    let frame_bridge_shutdown = Arc::clone(&shutdown);
    let frame_bridge = tokio::task::spawn_blocking(move || loop {
        if frame_bridge_shutdown.load(Ordering::Acquire) {
            break;
        }
        let Some(unit) = access_units.recv_timeout(Duration::from_millis(10)) else {
            if access_units.is_closed() {
                break;
            }
            continue;
        };
        let mut pending = unit;
        loop {
            if frame_bridge_shutdown.load(Ordering::Acquire) {
                break;
            }
            match sample_tx.try_send(pending) {
                Ok(()) => break,
                Err(mpsc::error::TrySendError::Full(unit)) => {
                    pending = unit;
                    thread::sleep(Duration::from_millis(10));
                }
                Err(mpsc::error::TrySendError::Closed(_)) => break,
            }
        }
    });

    let (command_tx, mut command_async_rx) = mpsc::channel(TRANSPORT_COMMAND_CAPACITY);
    let command_bridge_shutdown = Arc::clone(&shutdown);
    let command_bridge = tokio::task::spawn_blocking(move || loop {
        if command_bridge_shutdown.load(Ordering::Acquire) {
            break;
        }
        match command_rx.recv_timeout(Duration::from_millis(10)) {
            Ok(command) => {
                let mut pending = command;
                loop {
                    if command_bridge_shutdown.load(Ordering::Acquire) {
                        return;
                    }
                    match command_tx.try_send(pending) {
                        Ok(()) => break,
                        Err(mpsc::error::TrySendError::Full(command)) => {
                            pending = command;
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => return,
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    });

    let sample_status = Arc::clone(&status);
    let sample_encoder_control = Arc::clone(&encoder_control);
    let sample_track = Arc::clone(&track);
    // The sample gate must match the session codec: H.264 sessions carry
    // Baseline (42e02a) only, H.264 High sessions (64002a) also accept
    // Baseline (it decodes everywhere) but never anything else.
    let allow_high_profile = video_codec == VideoCodec::H264High;
    let audio_worker = {
        let audio_status = Arc::clone(&status);
        let shutdown = Arc::clone(&shutdown);
        tokio::task::spawn_blocking(move || {
            let Some(audio_rx) = audio_packets else {
                return;
            };
            let Some(track) = audio_track else {
                return;
            };
            let runtime = tokio::runtime::Handle::current();
            while !shutdown.load(Ordering::Acquire) {
                let Ok(packet) = audio_rx.recv_timeout(Duration::from_millis(20)) else {
                    continue;
                };
                if !is_connected(&audio_status) {
                    continue;
                }
                let sample = Sample {
                    data: packet.data.into(),
                    duration: packet.duration,
                    ..Default::default()
                };
                let track = Arc::clone(&track);
                let _ = runtime.block_on(async move { track.write_sample(&sample).await });
            }
        })
    };

    let sample_worker = tokio::spawn(async move {
        let mut awaiting_keyframe = true;
        let mut previous_timestamp = None;
        let mut units_seen = 0u64;
        let mut dropped_awaiting = 0u64;
        let mut dropped_profile_mismatch = 0u64;
        let mut wrote_first = false;
        while let Some(unit) = sample_rx.recv().await {
            units_seen += 1;
            if units_seen == 1 {
                super::logger::log(
                    "INFO",
                    "pump",
                    &format!(
                        "first access unit received (keyframe={}, {} bytes, profile={:?})",
                        unit.keyframe,
                        unit.data.len(),
                        unit.profile_level_id
                    ),
                );
            }
            if !is_connected(&sample_status) {
                awaiting_keyframe = true;
                previous_timestamp = None;
                continue;
            }
            if awaiting_keyframe {
                if !unit.keyframe {
                    dropped_awaiting += 1;
                    // Self-heal: re-assert the IDR request while waiting so
                    // a keyframe request lost to encoder-queue pacing (or a
                    // quiet coalescing window) cannot black-screen the viewer
                    // until the next natural intra period.
                    if dropped_awaiting % 30 == 0 {
                        sample_encoder_control.request_keyframe();
                    }
                    if dropped_awaiting % 300 == 0 {
                        super::logger::log(
                            "WARN",
                            "pump",
                            &format!(
                                "still waiting for first keyframe (dropped {dropped_awaiting} non-keyframe units)"
                            ),
                        );
                    }
                    continue;
                }
                awaiting_keyframe = false;
                super::logger::log("INFO", "pump", "first keyframe seen, starting stream");
            }
            let profile_ok =
                sample_profile_accepted(unit.profile_level_id.as_deref(), allow_high_profile);
            if !profile_ok {
                // Was a silent `eprintln!` (outside the session file): a
                // profile mismatch black-screened viewers while the host log
                // looked healthy (incident: 640c2a in a Baseline session).
                // Now it lands in the session file, throttled: first drop
                // plus every 600th, so the cause is one grep away.
                dropped_profile_mismatch += 1;
                if dropped_profile_mismatch == 1 || dropped_profile_mismatch % 600 == 0 {
                    let message = format!(
                        "dropping H.264 sample with SPS profile {:?} (session {}; dropped {dropped_profile_mismatch} total)",
                        unit.profile_level_id,
                        if allow_high_profile { "H.264 High" } else { "H.264" },
                    );
                    eprintln!("[goDrinking] {message}");
                    super::logger::log("WARN", "pump", &message);
                }
                continue;
            }
            let Some(timestamp) = timestamp_from_90khz(unit.timestamp_90khz) else {
                set_status(
                    &sample_status,
                    PeerTransportState::Failed,
                    "Encoded sample timestamp overflowed.".into(),
                );
                sample_encoder_control.request_stop();
                break;
            };
            let duration =
                duration_from_90khz(previous_timestamp, unit.timestamp_90khz, frame_duration);
            previous_timestamp = Some(unit.timestamp_90khz);
            let unit_len = unit.data.len();
            let sample = Sample {
                data: unit.data.into(),
                timestamp,
                duration,
                packet_timestamp: unit.timestamp_90khz.min(u32::MAX as u64) as u32,
                prev_dropped_packets: 0,
                ..Default::default()
            };
            if let Err(error) = sample_track.write_sample(&sample).await {
                set_status(
                    &sample_status,
                    PeerTransportState::Failed,
                    format!("WebRTC H.264 sample write failed: {error}"),
                );
                sample_encoder_control.request_stop();
                break;
            }
            if !wrote_first {
                wrote_first = true;
                super::logger::log(
                    "INFO",
                    "pump",
                    &format!("first sample written ({unit_len} bytes)"),
                );
            }
        }
    });

    let rtcp_encoder_control = Arc::clone(&encoder_control);
    let rtcp_worker = tokio::spawn(async move {
        // Throttled REMB trace for the session logs: first signal, then only
        // on significant moves (>25%) or every 30s, so the story is visible
        // without spamming (browsers send REMB ~1/s).
        let mut last_logged: Option<(std::time::Instant, u32)> = None;
        loop {
            let result = sender.read_rtcp().await;
            let Ok((packets, _)) = result else { break };
            for packet in packets {
                if packet.as_any().is::<PictureLossIndication>()
                    || packet.as_any().is::<FullIntraRequest>()
                {
                    rtcp_encoder_control.request_keyframe();
                }
                if let Some(rr) = packet.as_any().downcast_ref::<ReceiverReport>() {
                    let worst = rr.reports.iter().map(|report| report.fraction_lost).max();
                    if let Some(fraction) = worst {
                        rtcp_encoder_control.note_loss(fraction);
                    }
                }
                if let Some(remb) = packet
                    .as_any()
                    .downcast_ref::<ReceiverEstimatedMaximumBitrate>()
                {
                    if remb.bitrate.is_finite() && remb.bitrate > 0.0 {
                        let bitrate = (remb.bitrate as u32).clamp(250_000, 50_000_000);
                        rtcp_encoder_control.set_congestion_bitrate(bitrate);
                        let now = std::time::Instant::now();
                        let significant = last_logged.map(|(at, prev)| {
                            now.duration_since(at).as_secs() >= 30
                                || (bitrate as i64 - prev as i64).abs() * 4 > prev as i64
                        });
                        if significant != Some(false) {
                            last_logged = Some((now, bitrate));
                            super::logger::log(
                                "INFO",
                                "remb",
                                &format!(
                                    "receiver estimate {} kbps (target {} kbps)",
                                    bitrate / 1000,
                                    rtcp_encoder_control.target() / 1000
                                ),
                            );
                        }
                    }
                }
            }
        }
    });

    // Sender-side probing: once per second, step back toward the target
    // while the path looks clean (no recent decrease, no recent loss).
    // REMB decreases still win immediately inside set_congestion_bitrate.
    let probe_control = Arc::clone(&encoder_control);
    let probe_shutdown = Arc::clone(&shutdown);
    let _probe_worker = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        loop {
            ticker.tick().await;
            if probe_shutdown.load(Ordering::Acquire) {
                break;
            }
            if let Some(bitrate) = probe_control.probe_candidate(std::time::Instant::now()) {
                probe_control.apply_probe(bitrate);
            }
        }
    });

    while let Some(command) = command_async_rx.recv().await {
        match command {
            PeerCommand::CreateOffer(response) => {
                let result = run_signaling_operation(
                    create_offer(&peer),
                    Arc::clone(&shutdown),
                    "WebRTC offer",
                )
                .await;
                let _ = response.send(result);
            }
            PeerCommand::AcceptOffer(offer, response) => {
                let result = run_signaling_operation(
                    accept_offer(&peer, offer),
                    Arc::clone(&shutdown),
                    "WebRTC answer",
                )
                .await;
                let _ = response.send(result);
            }
            PeerCommand::SetAnswer(answer, response) => {
                let result = run_signaling_operation(
                    set_answer(&peer, answer),
                    Arc::clone(&shutdown),
                    "WebRTC set-answer",
                )
                .await;
                let _ = response.send(result);
            }
            PeerCommand::GetRtt(response) => {
                let rtt = peer_rtt_ms(&peer).await;
                let _ = response.send(rtt);
            }
            PeerCommand::Close => break,
        }
    }

    shutdown.store(true, Ordering::Release);
    sample_worker.abort();
    audio_worker.abort();
    rtcp_worker.abort();
    let _ = frame_bridge.await;
    command_bridge.abort();
    let _ = command_bridge.await;
    let close_result = close_peer_with_deadline(&peer).await;
    if let Err(error) = close_result {
        set_status(
            &status,
            PeerTransportState::Failed,
            format!("WebRTC peer teardown failed: {error}"),
        );
    } else {
        set_status(
            &status,
            PeerTransportState::Closed,
            "WebRTC peer closed.".into(),
        );
    }
}

async fn run_signaling_operation<T, F>(
    operation: F,
    shutdown: Arc<AtomicBool>,
    name: &str,
) -> Result<T, String>
where
    F: Future<Output = Result<T, String>>,
{
    run_signaling_operation_with_timeout(operation, shutdown, name, REQUEST_TIMEOUT).await
}

async fn run_signaling_operation_with_timeout<T, F>(
    operation: F,
    shutdown: Arc<AtomicBool>,
    name: &str,
    timeout_duration: Duration,
) -> Result<T, String>
where
    F: Future<Output = Result<T, String>>,
{
    tokio::pin!(operation);
    let cancellation = async move {
        loop {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if shutdown.load(Ordering::Acquire) {
                return ();
            }
        }
    };
    tokio::pin!(cancellation);
    let timeout = tokio::time::sleep(timeout_duration);
    tokio::pin!(timeout);
    tokio::select! {
        result = &mut operation => result,
        _ = &mut cancellation => Err(format!("{name} operation cancelled during shutdown")),
        _ = &mut timeout => Err(format!("{name} operation timed out")),
    }
}

async fn close_peer_with_deadline(peer: &Arc<RTCPeerConnection>) -> Result<(), String> {
    close_async_operation_with_deadline(peer.close(), PEER_CLOSE_TIMEOUT, "WebRTC peer close").await
}

async fn close_async_operation_with_deadline<F, E>(
    operation: F,
    timeout: Duration,
    name: &str,
) -> Result<(), String>
where
    F: Future<Output = Result<(), E>>,
    E: std::fmt::Display,
{
    match tokio::time::timeout(timeout, operation).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(format!("{name} failed: {error}")),
        Err(_) => Err(format!("{name} timed out")),
    }
}

fn opus_codec() -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        capability: RTCRtpCodecCapability {
            mime_type: MIME_TYPE_OPUS.into(),
            clock_rate: 48_000,
            channels: 2,
            sdp_fmtp_line: "minptime=10;useinbandfec=1".into(),
            rtcp_feedback: Vec::new(),
        },
        payload_type: 111,
        ..Default::default()
    }
}

/// Session HEVC codec: empty fmtp mirrors the webrtc-rs default
/// registration; the encoder emits annex-B IRAPs the H265 payloader
/// packetizes per RFC 7798 SSCH.
fn av1_codec() -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        capability: RTCRtpCodecCapability {
            mime_type: MIME_TYPE_AV1.into(),
            clock_rate: 90_000,
            channels: 0,
            sdp_fmtp_line: "".into(),
            rtcp_feedback: vec![
                RTCPFeedback {
                    typ: "goog-remb".into(),
                    parameter: "".into(),
                },
                RTCPFeedback {
                    typ: "nack".into(),
                    parameter: "pli".into(),
                },
                RTCPFeedback {
                    typ: "ccm".into(),
                    parameter: "fir".into(),
                },
            ],
        },
        payload_type: 45,
        ..Default::default()
    }
}

fn hevc_codec() -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        capability: RTCRtpCodecCapability {
            mime_type: MIME_TYPE_HEVC.into(),
            clock_rate: 90_000,
            channels: 0,
            sdp_fmtp_line: "".into(),
            rtcp_feedback: vec![
                RTCPFeedback {
                    typ: "goog-remb".into(),
                    parameter: "".into(),
                },
                RTCPFeedback {
                    typ: "nack".into(),
                    parameter: "pli".into(),
                },
                RTCPFeedback {
                    typ: "ccm".into(),
                    parameter: "fir".into(),
                },
            ],
        },
        payload_type: 103,
        ..Default::default()
    }
}

fn h264_codec(profile_level_id: &str) -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        capability: RTCRtpCodecCapability {
            mime_type: MIME_TYPE_H264.into(),
            clock_rate: 90_000,
            channels: 0,
            sdp_fmtp_line: super::types::h264_fmtp_line(profile_level_id),
            rtcp_feedback: vec![
                RTCPFeedback {
                    typ: "goog-remb".into(),
                    parameter: "".into(),
                },
                RTCPFeedback {
                    typ: "nack".into(),
                    parameter: "pli".into(),
                },
                RTCPFeedback {
                    typ: "ccm".into(),
                    parameter: "fir".into(),
                },
            ],
        },
        payload_type: super::types::H264_PAYLOAD_TYPE,
        ..Default::default()
    }
}

/// RFC 6184 FU-A fragmentation for one H.264 NAL unit (pure, tested here so
/// the sender packetization round-trips without a live peer). Small NALs
/// pass through as a single RTP payload; larger ones split into
/// indicator/header/chunk packets with S/E bits set on the ends.
pub(crate) fn fragment_h264_fu_a(nal: &[u8], max_fragment_payload: usize) -> Vec<Vec<u8>> {
    if nal.is_empty() || max_fragment_payload == 0 {
        return Vec::new();
    }
    if nal.len() <= max_fragment_payload + 1 {
        return vec![nal.to_vec()];
    }
    let header = nal[0];
    let forbidden = header & 0x80;
    let nri = header & 0x60;
    let nal_type = header & 0x1f;
    let indicator = forbidden | nri | 28;
    let body = &nal[1..];
    let mut packets = Vec::new();
    let mut offset = 0;
    while offset < body.len() {
        let end = (offset + max_fragment_payload).min(body.len());
        let start = u8::from(offset == 0) << 7;
        let finish = u8::from(end == body.len()) << 6;
        let fu_header = start | finish | nal_type;
        let mut packet = Vec::with_capacity(2 + (end - offset));
        packet.push(indicator);
        packet.push(fu_header);
        packet.extend_from_slice(&body[offset..end]);
        packets.push(packet);
        offset = end;
    }
    packets
}

/// Inverse of [`fragment_h264_fu_a`]: reassembles FU-A fragments (or a
/// single NAL payload) back into the original NAL unit. Returns None on
/// truncated headers, mixed types, or missing S/E markers.
pub(crate) fn reassemble_h264_fu_a(packets: &[Vec<u8>]) -> Option<Vec<u8>> {
    if packets.is_empty() {
        return None;
    }
    if packets.len() == 1 && packets[0].first().is_some_and(|b| b & 0x1f != 28) {
        return Some(packets[0].clone());
    }
    let mut nal_type = None;
    let mut nri = 0;
    let mut forbidden = 0;
    let mut body = Vec::new();
    for (index, packet) in packets.iter().enumerate() {
        if packet.len() < 3 {
            return None;
        }
        let indicator = packet[0];
        let fu_header = packet[1];
        if indicator & 0x1f != 28 {
            return None;
        }
        let start = fu_header & 0x80 != 0;
        let end = fu_header & 0x40 != 0;
        if (index == 0) != start || (index + 1 == packets.len()) != end {
            return None;
        }
        let current_type = fu_header & 0x1f;
        if nal_type
            .replace(current_type)
            .is_some_and(|first| first != current_type)
        {
            return None;
        }
        forbidden = forbidden | (indicator & 0x80);
        nri = nri | (indicator & 0x60);
        body.extend_from_slice(&packet[2..]);
    }
    let Some(nal_type) = nal_type else {
        return None;
    };
    let mut nal = Vec::with_capacity(body.len() + 1);
    nal.push(forbidden | nri | nal_type);
    nal.extend_from_slice(&body);
    Some(nal)
}

async fn create_offer(peer: &Arc<RTCPeerConnection>) -> Result<PeerSignal, String> {
    let offer = peer
        .create_offer(None)
        .await
        .map_err(|error| format!("create offer failed: {error}"))?;
    let gather = peer.gathering_complete_promise().await;
    peer.set_local_description(offer)
        .await
        .map_err(|error| format!("set local offer failed: {error}"))?;
    gather_candidates_best_effort(gather, "offer").await;
    let description = peer
        .local_description()
        .await
        .ok_or_else(|| "local offer is unavailable".to_owned())?;
    require_candidates(&description.sdp, "offer")?;
    Ok(PeerSignal {
        kind: PeerSignalKind::Offer,
        sdp: description.sdp,
        id: None,
    })
}

async fn accept_offer(
    peer: &Arc<RTCPeerConnection>,
    offer: PeerSignal,
) -> Result<PeerSignal, String> {
    if offer.kind != PeerSignalKind::Offer {
        return Err("accept_offer requires an offer signal".into());
    }
    peer.set_remote_description(
        RTCSessionDescription::offer(offer.sdp)
            .map_err(|error| format!("invalid remote offer: {error}"))?,
    )
    .await
    .map_err(|error| format!("set remote offer failed: {error}"))?;
    let answer = peer
        .create_answer(None)
        .await
        .map_err(|error| format!("create answer failed: {error}"))?;
    let gather = peer.gathering_complete_promise().await;
    peer.set_local_description(answer)
        .await
        .map_err(|error| format!("set local answer failed: {error}"))?;
    gather_candidates_best_effort(gather, "answer").await;
    let description = peer
        .local_description()
        .await
        .ok_or_else(|| "local answer is unavailable".to_owned())?;
    require_candidates(&description.sdp, "answer")?;
    Ok(PeerSignal {
        kind: PeerSignalKind::Answer,
        sdp: description.sdp,
        id: None,
    })
}

async fn set_answer(peer: &Arc<RTCPeerConnection>, answer: PeerSignal) -> Result<(), String> {
    if answer.kind != PeerSignalKind::Answer {
        return Err("set_answer requires an answer signal".into());
    }
    let sdp = rewrite_mdns_candidate_addresses(&answer.sdp, "127.0.0.1");
    // Fail fast when the viewer rejects the video m-section (port 0):
    // that means its browser has no decoder for the session codec (e.g.
    // HEVC on Firefox). Without this the peer sits in "connecting" forever
    // with no media flowing and no error anywhere.
    if let Some(rejected) = rejected_video_section(&sdp) {
        super::logger::log(
            "WARN",
            "set-answer",
            &format!("viewer rejected the video stream ({rejected}) — browser likely lacks a decoder for the session codec"),
        );
        return Err(
            "viewer rejected the video stream: browser has no decoder for this codec".into(),
        );
    }
    peer.set_remote_description(
        RTCSessionDescription::answer(sdp)
            .map_err(|error| format!("invalid remote answer: {error}"))?,
    )
    .await
    .map_err(|error| format!("set remote answer failed: {error}"))
}

/// Sample gate predicate, extracted pure so it is unit-tested directly
/// instead of only via mirrors: Baseline sessions accept `42*` SPS only;
/// H.264 High sessions also accept `64*` (Baseline decodes everywhere).
/// `None` (no SPS seen yet) is accepted — same as the old inline `map_or`.
pub(crate) fn sample_profile_accepted(
    profile_level_id: Option<&str>,
    allow_high_profile: bool,
) -> bool {
    profile_level_id.map_or(true, |profile| {
        super::access_unit::is_baseline_profile(profile)
            || (allow_high_profile && super::access_unit::is_high_profile(profile))
    })
}

/// Inspects an SDP answer: returns the video m-line when its port is 0
/// (stream rejected — no common codec), None when video was accepted.
fn rejected_video_section(sdp: &str) -> Option<String> {
    sdp.lines().find_map(|line| {
        let line = line.trim().trim_end_matches('\r');
        if !line.starts_with("m=video ") {
            return None;
        }
        let rejected = line.split_whitespace().nth(1) == Some("0");
        rejected.then(|| line.to_owned())
    })
}

/// WKWebView/Safari obfuscate host ICE addresses as `<uuid>.local`.
/// Replace those with a reachable IP so webrtc-rs does not need mDNS.
pub(crate) fn rewrite_mdns_candidate_addresses(sdp: &str, ip: &str) -> String {
    let newline = if sdp.contains("\r\n") { "\r\n" } else { "\n" };
    let ends_with_newline = sdp.ends_with('\n');
    let mut lines: Vec<String> = sdp
        .lines()
        .map(|line| rewrite_mdns_candidate_line(line.trim_end_matches('\r'), ip))
        .collect();
    if !ends_with_newline && lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    let mut rewritten = lines.join(newline);
    if ends_with_newline {
        rewritten.push_str(newline);
    }
    rewritten
}

fn rewrite_mdns_candidate_line(line: &str, ip: &str) -> String {
    let Some(rest) = line.strip_prefix("a=candidate:") else {
        return line.to_string();
    };
    let mut parts: Vec<&str> = rest.split_whitespace().collect();
    // foundation component protocol priority address port typ ...
    if parts.len() >= 5 && parts[4].ends_with(".local") {
        parts[4] = ip;
        format!("a=candidate:{}", parts.join(" "))
    } else {
        line.to_string()
    }
}

async fn wait_for_ice(mut gather: tokio::sync::mpsc::Receiver<()>) -> Result<(), String> {
    tokio::time::timeout(ICE_GATHER_TIMEOUT, gather.recv())
        .await
        .map(|_| ())
        .map_err(|_| "ICE gathering timed out".to_owned())
}

/// Best-effort ICE wait: a slow/blocked STUN mirror must not fail the whole
/// join when host candidates are already usable (same NAT, LAN, loopback).
/// Only a candidate-less SDP still fails (via require_candidates).
async fn gather_candidates_best_effort(gather: tokio::sync::mpsc::Receiver<()>, what: &str) {
    if let Err(error) = wait_for_ice(gather).await {
        super::logger::log(
            "WARN",
            "ice",
            &format!("{error} while gathering {what} candidates; proceeding with partial SDP"),
        );
    }
}

/// Fail fast only when the SDP carries zero candidates: without any
/// candidate no ICE pair can ever form, so minting/sending it would leave a
/// viewer stuck in Connecting forever.
fn require_candidates(sdp: &str, what: &str) -> Result<(), String> {
    let has_candidate = sdp
        .lines()
        .any(|line| line.trim().starts_with("a=candidate:"));
    if has_candidate {
        Ok(())
    } else {
        super::logger::log(
            "WARN",
            "ice",
            &format!("{what} SDP has no ICE candidates; not sending"),
        );
        Err(format!("{what} gathered no ICE candidates"))
    }
}

/// Round-trip time in ms for the selected ICE pair (STUN-based, same as
/// the browser `candidate-pair/currentRoundTripTime`), falling back to the
/// RTCP-level measurement when the pair has no sample yet.
async fn peer_rtt_ms(peer: &Arc<RTCPeerConnection>) -> Option<f64> {
    let report = peer.get_stats().await;
    let mut any_pair: Option<f64> = None;
    for stats in report.reports.values() {
        if let StatsReportType::CandidatePair(pair) = stats {
            if pair.current_round_trip_time > 0.0 {
                let ms = pair.current_round_trip_time * 1000.0;
                if pair.nominated {
                    return Some(ms);
                }
                any_pair = Some(ms);
            }
        }
    }
    if any_pair.is_some() {
        return any_pair;
    }
    for stats in report.reports.values() {
        if let StatsReportType::RemoteInboundRTP(inbound) = stats {
            // NOTE: the interceptor records this from `rtt_ms`, so it is
            // already in milliseconds (unlike the ICE pair stats above).
            if let Some(rtt) = inbound.round_trip_time {
                if rtt > 0.0 {
                    return Some(rtt);
                }
            }
        }
    }
    None
}

fn timestamp_from_90khz(timestamp: u64) -> Option<SystemTime> {
    let nanos = timestamp.checked_mul(1_000_000_000)?.checked_div(90_000)?;
    Some(UNIX_EPOCH + Duration::from_nanos(nanos))
}

fn is_connected(status: &Arc<Mutex<SharedStatus>>) -> bool {
    status
        .lock()
        .map(|status| status.state == PeerTransportState::Connected)
        .unwrap_or(false)
}

fn duration_from_90khz(previous: Option<u64>, current: u64, fallback: Duration) -> Duration {
    let Some(previous) = previous else {
        return fallback;
    };
    let delta = (current as u32).wrapping_sub(previous as u32);
    if delta == 0 || delta > 900_000 {
        return fallback;
    }
    Duration::from_nanos((delta as u64).saturating_mul(1_000_000_000) / 90_000)
}

fn set_status(status: &Arc<Mutex<SharedStatus>>, state: PeerTransportState, detail: String) {
    if let Ok(mut status) = status.lock() {
        status.state = state;
        status.detail = detail;
    }
}

fn map_peer_state(state: RTCPeerConnectionState) -> PeerTransportState {
    match state {
        RTCPeerConnectionState::New => PeerTransportState::New,
        RTCPeerConnectionState::Connecting => PeerTransportState::Connecting,
        RTCPeerConnectionState::Connected => PeerTransportState::Connected,
        RTCPeerConnectionState::Disconnected => PeerTransportState::Disconnected,
        RTCPeerConnectionState::Failed => PeerTransportState::Failed,
        RTCPeerConnectionState::Closed => PeerTransportState::Closed,
        RTCPeerConnectionState::Unspecified => PeerTransportState::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        close_async_operation_with_deadline, finish_worker_with_timeout, fragment_h264_fu_a,
        h264_codec, map_peer_state, reassemble_h264_fu_a, rewrite_mdns_candidate_addresses,
        run_signaling_operation_with_timeout, PeerSignal, PeerSignalKind, WorkerCompletion,
    };
    use crate::media::types::PeerTransportState;
    use std::time::Duration;
    use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;

    #[test]
    fn rewrites_webkit_mdns_host_candidates_to_loopback() {
        let sdp = concat!(
            "v=0\r\n",
            "a=candidate:1 1 UDP 2122260223 8f7a9c2e-1b2c-4d5e-8f90-abcdef123456.local 54321 typ host\r\n",
            "a=candidate:2 1 UDP 2122260222 192.168.1.20 4000 typ host\r\n",
        );
        let rewritten = rewrite_mdns_candidate_addresses(sdp, "127.0.0.1");
        assert!(rewritten.contains("127.0.0.1 54321 typ host"));
        assert!(rewritten.contains("192.168.1.20 4000 typ host"));
        assert!(!rewritten.contains(".local"));
    }

    #[test]
    fn signaling_types_round_trip_with_wire_type_field() {
        let signal = PeerSignal {
            kind: PeerSignalKind::Offer,
            sdp: "v=0\r\n".into(),
            id: None,
        };
        let encoded = serde_json::to_string(&signal).expect("signal serializes");
        assert_eq!(encoded, r#"{"type":"offer","sdp":"v=0\r\n"}"#);
        assert_eq!(
            serde_json::from_str::<PeerSignal>(&encoded).expect("signal deserializes"),
            signal
        );
    }

    #[test]
    fn rejected_video_section_detects_port_zero() {
        let accepted = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\nm=video 9 UDP/TLS/RTP/SAVPF 103\r\na=rtpmap:103 H265/90000\r\n";
        assert_eq!(super::rejected_video_section(accepted), None);
        let rejected = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\nm=video 0 UDP/TLS/RTP/SAVPF 103\r\n";
        assert!(super::rejected_video_section(rejected).is_some());
    }

    #[test]
    fn sample_gate_accepts_only_the_session_profile() {
        // Baseline sessions (the default): every 42* SPS passes, anything
        // else (incident: 640c2a) is dropped so the browser never gets an
        // undecodable stream.
        for profile in ["42e02a", "42c02a", "42c01e", "42c028", "42C02A"] {
            assert!(
                super::sample_profile_accepted(Some(profile), false),
                "Baseline session must accept {profile}"
            );
        }
        for profile in ["640c2a", "64002a", "640c29", "4d002a", "", "42e02"] {
            assert!(
                !super::sample_profile_accepted(Some(profile), false),
                "Baseline session must drop {profile:?}"
            );
        }
        // H.264 High sessions additionally accept High SPS (Baseline still
        // decodes everywhere, so it stays accepted).
        assert!(super::sample_profile_accepted(Some("640c2a"), true));
        assert!(super::sample_profile_accepted(Some("42e02a"), true));
        assert!(!super::sample_profile_accepted(Some("4d002a"), true));
        // No SPS parsed yet: let it through (keyframe wait still applies).
        assert!(super::sample_profile_accepted(None, false));
        assert!(super::sample_profile_accepted(None, true));
    }

    #[test]
    fn h264_codec_advertises_low_latency_feedback() {
        let codec = h264_codec("42e02a");
        assert_eq!(codec.capability.mime_type, "video/H264");
        assert_eq!(codec.capability.clock_rate, 90_000);
        assert_eq!(codec.payload_type, 102);
        assert_eq!(
            codec.capability.sdp_fmtp_line,
            "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e02a"
        );
        assert!(codec
            .capability
            .rtcp_feedback
            .iter()
            .any(|feedback| feedback.typ == "nack" && feedback.parameter == "pli"));
        assert!(codec
            .capability
            .rtcp_feedback
            .iter()
            .any(|feedback| feedback.typ == "ccm" && feedback.parameter == "fir"));
    }

    #[test]
    fn h264_sdp_is_the_single_baseline_contract() {
        use crate::media::types::{
            h264_fmtp_requires_packetization_mode_1, H264_BASELINE_FMTP,
            H264_BASELINE_PROFILE_LEVEL_ID, H264_PAYLOAD_TYPE,
        };
        // ONE payload type for every H.264 session; the fmtp line is the
        // centralized baseline constant (4.2 covers 1080p60; 3.1 would not).
        let codec = h264_codec(H264_BASELINE_PROFILE_LEVEL_ID);
        assert_eq!(codec.payload_type, H264_PAYLOAD_TYPE);
        assert_eq!(codec.payload_type, 102);
        assert_eq!(codec.capability.sdp_fmtp_line, H264_BASELINE_FMTP);
        assert_eq!(
            codec.capability.sdp_fmtp_line,
            "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e02a"
        );
        assert!(h264_fmtp_requires_packetization_mode_1(
            &codec.capability.sdp_fmtp_line
        ));
    }

    #[test]
    fn peer_states_map_to_serializable_media_states() {
        assert_eq!(
            map_peer_state(RTCPeerConnectionState::Connecting),
            PeerTransportState::Connecting
        );
        assert_eq!(
            map_peer_state(RTCPeerConnectionState::Connected),
            PeerTransportState::Connected
        );
        assert_eq!(
            map_peer_state(RTCPeerConnectionState::Failed),
            PeerTransportState::Failed
        );
        assert_eq!(
            map_peer_state(RTCPeerConnectionState::Closed),
            PeerTransportState::Closed
        );
    }

    #[test]
    fn sample_duration_uses_timestamp_delta_and_wraps_rtp_clock() {
        let fallback = Duration::from_millis(33);
        assert_eq!(
            super::duration_from_90khz(Some(90_000), 93_000, fallback),
            Duration::from_nanos(3_000 * 1_000_000_000 / 90_000)
        );
        assert_eq!(
            super::duration_from_90khz(Some(u32::MAX as u64 - 900), 600, fallback),
            Duration::from_nanos(1501 * 1_000_000_000 / 90_000)
        );
        assert_eq!(super::duration_from_90khz(None, 90_000, fallback), fallback);
    }

    #[test]
    fn frame_durations_follow_the_60fps_tick_grid_and_wrap() {
        // 1/60s == 1500 ticks @90kHz; three monotonic frames stay on the grid.
        let frame = Duration::from_nanos(1_500 * 1_000_000_000 / 90_000);
        let fallback = Duration::from_millis(33);
        let t0 = 1_000_000u64;
        assert_eq!(
            super::duration_from_90khz(Some(t0), t0 + 1_500, fallback),
            frame
        );
        assert_eq!(
            super::duration_from_90khz(Some(t0 + 1_500), t0 + 3_000, fallback),
            frame
        );
        // Sequence/timestamp stay monotonic across the u32 wrap.
        assert_eq!(
            super::duration_from_90khz(Some(u32::MAX as u64 - 750), 750, fallback),
            Duration::from_nanos(1_501 * 1_000_000_000 / 90_000)
        );
        // Zero deltas and gaps over 10s (>900_000 ticks) fall back to the
        // configured frame duration instead of emitting 0 / huge samples.
        assert_eq!(super::duration_from_90khz(Some(t0), t0, fallback), fallback);
        assert_eq!(
            super::duration_from_90khz(Some(t0), t0 + 900_001, fallback),
            fallback
        );
        // Sender timestamps are overflow-checked and monotonic in 90kHz.
        let a = super::timestamp_from_90khz(t0).expect("a converts");
        let b = super::timestamp_from_90khz(t0 + 1_500).expect("b converts");
        assert!(b > a);
        // SystemTime precision is platform-dependent (100ns FILETIME ticks
        // on Windows), so compare the round-trip numerically within 1µs
        // instead of exact Duration equality.
        let round_trip = b.duration_since(a).expect("monotonic");
        assert!(
            round_trip.as_nanos().abs_diff(frame.as_nanos()) <= 1_000,
            "60fps tick-grid round-trip out of tolerance: got {round_trip:?}, want {frame:?}"
        );
        assert_eq!(super::timestamp_from_90khz(u64::MAX), None);
    }

    #[test]
    fn fu_a_fragmentation_round_trips_the_original_nal() {
        // A realistic IDR NAL fragments and reassembles byte-identical, so a
        // viewer behind a small MTU decodes the same access unit the
        // encoder emitted.
        let mut nal = vec![0x65u8];
        nal.extend((0..5_000u32).map(|value| (value % 251) as u8));
        let fragments = fragment_h264_fu_a(&nal, 1_200);
        assert!(fragments.len() > 1);
        assert!(fragments.iter().all(|packet| packet.len() <= 1_202));
        // S set only on the first fragment, E only on the last.
        assert_eq!(fragments[0][1] & 0x80, 0x80);
        assert_eq!(fragments[0][1] & 0x40, 0x00);
        let last = fragments.len() - 1;
        assert_eq!(fragments[last][1] & 0x80, 0x00);
        assert_eq!(fragments[last][1] & 0x40, 0x40);
        assert_eq!(reassemble_h264_fu_a(&fragments), Some(nal));
        // Small NALs pass through unfragmented and still round-trip.
        let small = vec![0x41, 0x9a, 0x22, 0x11];
        let single = fragment_h264_fu_a(&small, 1_200);
        assert_eq!(single, vec![small.clone()]);
        assert_eq!(reassemble_h264_fu_a(&single), Some(small));
        // Corrupt framing (missing E marker) is rejected, not half-decoded.
        let mut truncated = fragments.clone();
        truncated.pop();
        assert_ne!(
            reassemble_h264_fu_a(&truncated),
            reassemble_h264_fu_a(&fragments)
        );
    }

    #[test]
    fn response_wait_is_bounded() {
        let (_tx, rx) = std::sync::mpsc::sync_channel::<Result<(), String>>(1);
        let started = std::time::Instant::now();
        let result =
            super::receive_response_with_timeout(rx, Duration::from_millis(1), "test request");
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn shutdown_cannot_be_dropped_when_signaling_queue_is_full() {
        let (command_tx, _command_rx) = std::sync::mpsc::sync_channel(1);
        let (response_tx, _response_rx) = std::sync::mpsc::sync_channel(1);
        command_tx
            .try_send(super::PeerCommand::CreateOffer(response_tx))
            .expect("fill signaling queue");
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let client = super::PeerTransportClient {
            command_tx,
            shutdown: std::sync::Arc::clone(&shutdown),
        };
        client.request_close().expect("shutdown is coalesced");
        assert!(shutdown.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn shutdown_cancels_an_in_flight_signaling_operation() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("test runtime");
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let result = runtime.block_on(super::run_signaling_operation(
            std::future::pending::<Result<(), String>>(),
            shutdown,
            "test signaling",
        ));
        assert_eq!(
            result,
            Err("test signaling operation cancelled during shutdown".into())
        );
    }

    #[test]
    fn initialization_timeout_is_bounded() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("test runtime");
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let started = std::time::Instant::now();
        let result = runtime.block_on(run_signaling_operation_with_timeout(
            std::future::pending::<Result<(), String>>(),
            shutdown,
            "test initialization",
            Duration::from_millis(1),
        ));
        assert_eq!(
            result,
            Err("test initialization operation timed out".into())
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn close_timeout_is_bounded() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("test runtime");
        let started = std::time::Instant::now();
        let result = runtime.block_on(close_async_operation_with_deadline(
            std::future::pending::<Result<(), &'static str>>(),
            Duration::from_millis(1),
            "test close",
        ));
        assert_eq!(result, Err("test close timed out".into()));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn worker_completion_reports_pending_after_a_deadline() {
        let completion = std::sync::Arc::new(WorkerCompletion {
            done: std::sync::Mutex::new(false),
            wake: std::sync::Condvar::new(),
        });
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_stop = std::sync::Arc::clone(&stop);
        let worker = std::thread::spawn(move || {
            while !worker_stop.load(std::sync::atomic::Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        let started = std::time::Instant::now();
        let result = finish_worker_with_timeout(worker, &completion, Duration::from_millis(1));
        assert_eq!(
            result,
            Err("peer worker cleanup pending after deadline".into())
        );
        stop.store(true, std::sync::atomic::Ordering::Release);
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
