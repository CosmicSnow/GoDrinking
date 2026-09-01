//! Native, local-only WebRTC sender for encoded Annex-B H.264 samples.

use super::access_unit::AccessUnitReceiver;
use super::pipeline::EncoderControl;
use super::process_tap::EncodedAudioPacket;
use super::types::PeerTransportState;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264, MIME_TYPE_OPUS};
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::ice::mdns::MulticastDnsMode;
use webrtc::interceptor::registry::Registry;
use webrtc::media::Sample;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtcp::payload_feedbacks::receiver_estimated_maximum_bitrate::ReceiverEstimatedMaximumBitrate;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};
use webrtc::rtp_transceiver::RTCPFeedback;
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
    ) -> Result<Self, String> {
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
                    ready_tx,
                    worker_status,
                    worker_shutdown,
                ));
                mark_worker_complete(&worker_completion);
            })
            .map_err(|error| format!("failed to start WebRTC worker: {error}"))?;

        let ready = match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(ready) => ready,
            Err(_) => {
                shutdown.store(true, Ordering::Release);
                let _ = command_tx.try_send(PeerCommand::Close);
                finish_worker(worker, &completion);
                return Err("WebRTC worker failed during initialization".to_owned());
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
                Err(error)
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
        self.shutdown.store(true, Ordering::Release);
        let _ = self.command_tx.try_send(PeerCommand::Close);
        if let Some(worker) = self.worker.take() {
            finish_worker(worker, &self.completion);
        }
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
    finish_worker_with_timeout(worker, completion, WORKER_COMPLETION_TIMEOUT);
}

fn finish_worker_with_timeout(
    worker: JoinHandle<()>,
    completion: &Arc<WorkerCompletion>,
    timeout: Duration,
) {
    let Ok(mut done) = completion.done.lock() else {
        drop(worker);
        return;
    };
    if !*done {
        let Ok((next, _)) = completion.wake.wait_timeout(done, timeout) else {
            drop(worker);
            return;
        };
        done = next;
    }
    let completed = *done;
    drop(done);
    if completed {
        let _ = worker.join();
    } else {
        // The worker owns all native peer resources and has its own shutdown
        // deadline. Dropping the JoinHandle detaches it without blocking the
        // caller or invalidating its captured Arcs.
        drop(worker);
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
    ready_tx: SyncSender<Result<(), String>>,
    status: Arc<Mutex<SharedStatus>>,
    shutdown: Arc<AtomicBool>,
) {
    let mut media_engine = MediaEngine::default();
    if let Err(error) = media_engine.register_codec(h264_codec(), RTPCodecType::Video) {
        let message = format!("H.264 codec registration failed: {error}");
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
    // Empty ICE servers intentionally restrict this transport to host/local
    // candidates. No public STUN or TURN service is configured.
    let configuration = RTCConfiguration {
        ice_servers: Vec::new(),
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
        h264_codec().capability,
        "godrinking-video".into(),
        "godrinking".into(),
    ));
    let track_for_peer: Arc<dyn TrackLocal + Send + Sync> = track.clone();
    let sender = match run_signaling_operation(
        async {
            peer.add_track(track_for_peer)
                .await
                .map_err(|error| format!("WebRTC H.264 track registration failed: {error}"))
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
        while let Some(unit) = sample_rx.recv().await {
            if !is_connected(&sample_status) {
                awaiting_keyframe = true;
                previous_timestamp = None;
                continue;
            }
            if awaiting_keyframe {
                if !unit.keyframe {
                    continue;
                }
                awaiting_keyframe = false;
            }
            if unit
                .profile_level_id
                .as_deref()
                .is_some_and(|profile| !super::access_unit::is_baseline_profile(profile))
            {
                eprintln!(
                    "[goDrinking] skipping H.264 sample with profile {:?}",
                    unit.profile_level_id
                );
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
        }
    });

    let rtcp_encoder_control = Arc::clone(&encoder_control);
    let rtcp_worker = tokio::spawn(async move {
        loop {
            let result = sender.read_rtcp().await;
            let Ok((packets, _)) = result else { break };
            for packet in packets {
                if packet.as_any().is::<PictureLossIndication>()
                    || packet.as_any().is::<FullIntraRequest>()
                {
                    rtcp_encoder_control.request_keyframe();
                }
                if let Some(remb) = packet
                    .as_any()
                    .downcast_ref::<ReceiverEstimatedMaximumBitrate>()
                {
                    if remb.bitrate.is_finite() && remb.bitrate > 0.0 {
                        let bitrate = (remb.bitrate as u32).clamp(250_000, 50_000_000);
                        rtcp_encoder_control.set_bitrate(bitrate);
                    }
                }
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

fn h264_codec() -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        capability: RTCRtpCodecCapability {
            mime_type: MIME_TYPE_H264.into(),
            clock_rate: 90_000,
            channels: 0,
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e02a"
                .into(),
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
        payload_type: 102,
        ..Default::default()
    }
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
    wait_for_ice(gather).await?;
    let description = peer
        .local_description()
        .await
        .ok_or_else(|| "local offer is unavailable".to_owned())?;
    Ok(PeerSignal {
        kind: PeerSignalKind::Offer,
        sdp: description.sdp,
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
    wait_for_ice(gather).await?;
    let description = peer
        .local_description()
        .await
        .ok_or_else(|| "local answer is unavailable".to_owned())?;
    Ok(PeerSignal {
        kind: PeerSignalKind::Answer,
        sdp: description.sdp,
    })
}

async fn set_answer(peer: &Arc<RTCPeerConnection>, answer: PeerSignal) -> Result<(), String> {
    if answer.kind != PeerSignalKind::Answer {
        return Err("set_answer requires an answer signal".into());
    }
    let sdp = rewrite_mdns_candidate_addresses(&answer.sdp, "127.0.0.1");
    peer.set_remote_description(
        RTCSessionDescription::answer(sdp)
            .map_err(|error| format!("invalid remote answer: {error}"))?,
    )
    .await
    .map_err(|error| format!("set remote answer failed: {error}"))
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
        close_async_operation_with_deadline, finish_worker_with_timeout, h264_codec,
        map_peer_state, rewrite_mdns_candidate_addresses, run_signaling_operation_with_timeout,
        PeerSignal, PeerSignalKind, WorkerCompletion,
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
        };
        let encoded = serde_json::to_string(&signal).expect("signal serializes");
        assert_eq!(encoded, r#"{"type":"offer","sdp":"v=0\r\n"}"#);
        assert_eq!(
            serde_json::from_str::<PeerSignal>(&encoded).expect("signal deserializes"),
            signal
        );
    }

    #[test]
    fn h264_codec_advertises_low_latency_feedback() {
        let codec = h264_codec();
        assert_eq!(codec.capability.mime_type, "video/H264");
        assert_eq!(codec.capability.clock_rate, 90_000);
        assert!(codec
            .capability
            .sdp_fmtp_line
            .contains("profile-level-id=42e02a"));
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
    fn worker_completion_can_detach_after_a_deadline() {
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
        finish_worker_with_timeout(worker, &completion, Duration::from_millis(1));
        stop.store(true, std::sync::atomic::Ordering::Release);
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
