use super::capabilities;
use super::peer_transport::{PeerSignal, PeerTransport};
use super::pipeline::{NativePipeline, PreviewState};
use super::process_tap::{EncodedAudioPacket, ProcessTap};
use super::room::LanRoom;
use super::types::{
    CreateMediaSessionRequest, FrameRate, MediaLifecycleState, MediaSessionSnapshot,
    NativeCaptureSource, NativeRunningApp, PeerTransportState, PreviewFrameEvent,
    TransmissionQuality, UpdateMediaSessionRequest, VideoResolution,
};
use super::MediaCapabilities;
use std::fmt::{Display, Formatter};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const CONTROL_QUEUE_CAPACITY: usize = 32;

struct SessionRecord {
    id: String,
    generation: u64,
    request: CreateMediaSessionRequest,
    peer: Option<PeerTransport>,
    // Kept alive with the session so capture and preview workers share the
    // bounded pipeline ownership boundary.
    _pipeline: NativePipeline,
    #[cfg(target_os = "macos")]
    adapter: super::screen_capture_kit::ScreenCaptureKitAdapter,
    #[cfg(target_os = "windows")]
    adapter: super::windows_capture::WindowsCaptureAdapter,
    native_capture_active: bool,
    room: Option<LanRoom>,
    // The active process tap, if any. Dropped to silence system audio without
    // touching the peer; recreated against `audio_tx` when exclusions change.
    audio_tap: Option<ProcessTap>,
    // The engine-owned Opus channel. Created once when the session first
    // enables audio; the peer keeps the matching receiver for the session
    // lifetime so a recreated tap can keep feeding the same audio track.
    audio_tx: Option<SyncSender<EncodedAudioPacket>>,
}

struct EngineState {
    capabilities: MediaCapabilities,
    lifecycle: MediaLifecycleState,
    session: Option<SessionRecord>,
    next_session_id: u64,
    detail: String,
    preview: Arc<PreviewState>,
}

enum MediaCommand {
    Create {
        request: CreateMediaSessionRequest,
        response: SyncSender<Result<MediaSessionSnapshot, MediaEngineError>>,
    },
    Update {
        request: UpdateMediaSessionRequest,
        response: SyncSender<Result<MediaSessionSnapshot, MediaEngineError>>,
    },
    Stop {
        response: SyncSender<Result<MediaSessionSnapshot, MediaEngineError>>,
    },
}

/// Thread-safe native media control plane.
///
/// Commands are copied into a bounded control queue and applied by one worker,
/// making create/stop operations serialized and safe to call concurrently.
/// Only metadata snapshots and derived preview thumbnails leave this type.
/// Native source frames remain inside the bounded pipeline boundary.
#[derive(Clone)]
pub struct MediaEngine {
    control_tx: SyncSender<MediaCommand>,
    state: Arc<Mutex<EngineState>>,
}

#[derive(Debug, Eq, PartialEq)]
pub enum MediaEngineError {
    QueueClosed,
    StatePoisoned,
    UnsupportedPlatform,
    SessionAlreadyActive,
    NoActiveSession,
    NativeCapture(String),
    NativePeer(String),
}

impl Display for MediaEngineError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::QueueClosed => "media worker is not available",
            Self::StatePoisoned => "media state is unavailable",
            Self::UnsupportedPlatform => "native media sessions are unsupported on this platform",
            Self::SessionAlreadyActive => "a media session is already active",
            Self::NoActiveSession => "no media session is active",
            Self::NativeCapture(message) => return formatter.write_str(message),
            Self::NativePeer(message) => return formatter.write_str(message),
        };
        formatter.write_str(message)
    }
}

impl MediaEngine {
    pub fn new() -> Self {
        let capabilities = capabilities::detect();
        let state = Arc::new(Mutex::new(EngineState {
            detail: if capabilities.supported {
                "Ready; native capture, VideoToolbox H.264 encoding, and local-only WebRTC peer transport are available.".into()
            } else {
                "Native media is unsupported; WebView capture remains active.".into()
            },
            capabilities,
            lifecycle: MediaLifecycleState::Idle,
            session: None,
            next_session_id: 1,
            preview: Arc::new(PreviewState::new()),
        }));
        let (control_tx, control_rx) = sync_channel(CONTROL_QUEUE_CAPACITY);
        let worker_state = Arc::clone(&state);
        thread::Builder::new()
            .name("godrinking-media-control".into())
            .spawn(move || worker_loop(control_rx, worker_state))
            .expect("failed to start media control worker");
        Self { control_tx, state }
    }

    pub fn capabilities(&self) -> MediaCapabilities {
        self.state
            .lock()
            .map(|state| state.capabilities.clone())
            .unwrap_or_else(|_| MediaCapabilities {
                platform: "unknown".into(),
                supported: false,
                screen_capture_kit: false,
                screen_recording_authorization:
                    super::screen_capture_kit::ScreenRecordingAuthorization::Unsupported,
                source_enumeration_available: false,
                windows_graphics_capture: false,
                wasapi: false,
                process_loopback: false,
                app_audio_exclusion: capabilities::AppAudioExclusionSupport::Unsupported,
                native_capture_implemented: false,
                native_encoder_implemented: false,
                native_peer_transport_implemented: false,
                detail: "Native media state is unavailable.".into(),
            })
    }

    pub fn refresh_screen_recording_capabilities(
        &self,
        app: &tauri::AppHandle,
    ) -> MediaCapabilities {
        #[cfg(target_os = "windows")]
        {
            // Windows has no pre-flight permission probe: WGC fails at capture
            // start if display capture is blocked. Report the static
            // capabilities as granted.
            let _ = app;
            self.capabilities()
        }
        #[cfg(not(target_os = "windows"))]
        {
            let mut adapter = super::screen_capture_kit::ScreenCaptureKitAdapter::new();
            let availability = adapter.probe_permission(app);
            self.update_capabilities(availability)
        }
    }

    pub fn snapshot(&self) -> MediaSessionSnapshot {
        self.apply_room_answer();
        self.state
            .lock()
            .map(|mut state| {
                refresh_native_state(&mut state);
                snapshot_from_state(&state)
            })
            .unwrap_or_else(|_| MediaSessionSnapshot::idle("Native media state is unavailable."))
    }

    fn apply_room_answer(&self) {
        let pending = self.state.lock().ok().and_then(|state| {
            let session = state.session.as_ref()?;
            let answer = session.room.as_ref()?.take_answer()?;
            Some((session.peer.as_ref()?.client(), answer))
        });
        if let Some((peer, answer)) = pending {
            let _ = peer.set_answer(answer);
        }
    }

    pub fn running_applications(&self) -> Result<Vec<NativeRunningApp>, MediaEngineError> {
        Ok(super::process_tap::running_applications())
    }

    pub fn publish_peer_offer(&self, signal: PeerSignal) -> Result<(), MediaEngineError> {
        let state = self
            .state
            .lock()
            .map_err(|_| MediaEngineError::StatePoisoned)?;
        if let Some(room) = state.session.as_ref().and_then(|session| session.room.as_ref()) {
            room.publish_offer(signal);
        }
        Ok(())
    }

    pub fn latest_preview(&self) -> Option<PreviewFrameEvent> {
        self.state.lock().ok().and_then(|state| {
            state
                .preview
                .latest
                .lock()
                .ok()
                .and_then(|preview| preview.clone())
        })
    }

    pub fn enumerate_sources(&self) -> Result<Vec<NativeCaptureSource>, MediaEngineError> {
        #[cfg(target_os = "windows")]
        {
            super::windows_capture::WindowsCaptureAdapter::enumerate_sources()
                .map_err(MediaEngineError::NativeCapture)
        }
        #[cfg(not(target_os = "windows"))]
        {
            let adapter = super::screen_capture_kit::ScreenCaptureKitAdapter::new();
            adapter
                .enumerate_sources()
                .map_err(|error| MediaEngineError::NativeCapture(error.to_string()))
        }
    }

    pub fn request_screen_recording_permission(&self, app: &tauri::AppHandle) -> MediaCapabilities {
        #[cfg(target_os = "windows")]
        {
            // No-op granted: WGC will fail at capture start if display capture
            // is blocked in Windows privacy settings.
            let _ = app;
            self.capabilities()
        }
        #[cfg(not(target_os = "windows"))]
        {
            let mut adapter = super::screen_capture_kit::ScreenCaptureKitAdapter::new();
            let availability = adapter.request_permission(app);
            self.update_capabilities(availability)
        }
    }

    #[cfg(not(target_os = "windows"))]
    fn update_capabilities(
        &self,
        availability: super::screen_capture_kit::ScreenCaptureKitAvailability,
    ) -> MediaCapabilities {
        let mut capabilities = self.capabilities();
        capabilities.screen_capture_kit = availability.framework_available;
        capabilities.screen_recording_authorization = availability.authorization;
        capabilities.source_enumeration_available = availability.source_enumeration_available;
        capabilities.detail = availability.detail;
        if let Ok(mut state) = self.state.lock() {
            state.capabilities = capabilities.clone();
        }
        capabilities
    }

    pub fn create_session(
        &self,
        request: CreateMediaSessionRequest,
    ) -> Result<MediaSessionSnapshot, MediaEngineError> {
        let (response_tx, response_rx) = sync_channel(1);
        self.control_tx
            .send(MediaCommand::Create {
                request,
                response: response_tx,
            })
            .map_err(|_| MediaEngineError::QueueClosed)?;
        response_rx
            .recv()
            .map_err(|_| MediaEngineError::QueueClosed)?
    }

    /// Applies live settings (quality, system audio, exclusions) to the
    /// active session without tearing down capture, the room, or the peer.
    pub fn update_session(
        &self,
        request: UpdateMediaSessionRequest,
    ) -> Result<MediaSessionSnapshot, MediaEngineError> {
        let (response_tx, response_rx) = sync_channel(1);
        self.control_tx
            .send(MediaCommand::Update {
                request,
                response: response_tx,
            })
            .map_err(|_| MediaEngineError::QueueClosed)?;
        response_rx
            .recv()
            .map_err(|_| MediaEngineError::QueueClosed)?
    }

    pub fn stop_session(&self) -> Result<MediaSessionSnapshot, MediaEngineError> {
        let (response_tx, response_rx) = sync_channel(1);
        self.control_tx
            .send(MediaCommand::Stop {
                response: response_tx,
            })
            .map_err(|_| MediaEngineError::QueueClosed)?;
        response_rx
            .recv()
            .map_err(|_| MediaEngineError::QueueClosed)?
    }

    pub fn create_peer_offer(&self) -> Result<PeerSignal, MediaEngineError> {
        let state = self
            .state
            .lock()
            .map_err(|_| MediaEngineError::StatePoisoned)?;
        let peer = state
            .session
            .as_ref()
            .and_then(|session| session.peer.as_ref())
            .map(PeerTransport::client)
            .ok_or_else(|| MediaEngineError::NativePeer("native peer is unavailable".into()))?;
        drop(state);
        let signal = peer.create_offer().map_err(MediaEngineError::NativePeer)?;
        let _ = self.publish_peer_offer(signal.clone());
        Ok(signal)
    }

    pub fn accept_peer_offer(&self, offer: PeerSignal) -> Result<PeerSignal, MediaEngineError> {
        let state = self
            .state
            .lock()
            .map_err(|_| MediaEngineError::StatePoisoned)?;
        let peer = state
            .session
            .as_ref()
            .and_then(|session| session.peer.as_ref())
            .map(PeerTransport::client)
            .ok_or_else(|| MediaEngineError::NativePeer("native peer is unavailable".into()))?;
        drop(state);
        peer.accept_offer(offer)
            .map_err(MediaEngineError::NativePeer)
    }

    pub fn set_peer_answer(&self, answer: PeerSignal) -> Result<(), MediaEngineError> {
        let state = self
            .state
            .lock()
            .map_err(|_| MediaEngineError::StatePoisoned)?;
        let peer = state
            .session
            .as_ref()
            .and_then(|session| session.peer.as_ref())
            .map(PeerTransport::client)
            .ok_or_else(|| MediaEngineError::NativePeer("native peer is unavailable".into()))?;
        drop(state);
        peer.set_answer(answer)
            .map_err(MediaEngineError::NativePeer)
    }

    pub fn close_peer_transport(&self) -> Result<(), MediaEngineError> {
        let state = self
            .state
            .lock()
            .map_err(|_| MediaEngineError::StatePoisoned)?;
        let session = state
            .session
            .as_ref()
            .ok_or(MediaEngineError::NoActiveSession)?;
        let peer = session
            .peer
            .as_ref()
            .map(PeerTransport::client)
            .ok_or_else(|| MediaEngineError::NativePeer("native peer is unavailable".into()))?;
        drop(state);
        peer.request_close().map_err(MediaEngineError::NativePeer)
    }
}

impl Default for MediaEngine {
    fn default() -> Self {
        Self::new()
    }
}

fn worker_loop(receiver: Receiver<MediaCommand>, state: Arc<Mutex<EngineState>>) {
    while let Ok(command) = receiver.recv() {
        match command {
            MediaCommand::Create { request, response } => {
                let _ = response.send(create_in_state(&state, request));
            }
            MediaCommand::Update { request, response } => {
                let _ = response.send(update_in_state(&state, request));
            }
            MediaCommand::Stop { response } => {
                let _ = response.send(stop_in_state(&state));
            }
        }
    }
}

fn create_in_state(
    state: &Arc<Mutex<EngineState>>,
    request: CreateMediaSessionRequest,
) -> Result<MediaSessionSnapshot, MediaEngineError> {
    let has_active_session = state
        .lock()
        .map_err(|_| MediaEngineError::StatePoisoned)?
        .session
        .is_some();
    if has_active_session {
        // A repeated Start is a recoverable user action: release the
        // discoverable session before creating the requested replacement.
        // If native cleanup fails, return that real error and leave the
        // existing session available for an explicit Stop/retry.
        stop_in_state(state)?;
    }
    let (capabilities, id, preview) = {
        let mut state = state.lock().map_err(|_| MediaEngineError::StatePoisoned)?;
        if !state.capabilities.supported {
            return Err(MediaEngineError::UnsupportedPlatform);
        }
        if state.session.is_some() || state.lifecycle != MediaLifecycleState::Idle {
            return Err(MediaEngineError::SessionAlreadyActive);
        }
        state.lifecycle = MediaLifecycleState::Starting;
        let id = format!("native-{}", state.next_session_id);
        state.next_session_id += 1;
        (state.capabilities.clone(), id, Arc::clone(&state.preview))
    };

    preview.begin_session();
    let mut pipeline = NativePipeline::new(
        preview,
        request.resolution,
        request.frame_rate,
        request.quality,
    );
    // The engine owns the Opus channel for the session lifetime. The peer
    // keeps the receiver even if the first tap fails, so a later update can
    // restart the tap against the same channel and feed the same audio track.
    let (audio_tx, audio_rx, audio_tap) = if request.system_audio {
        let (tx, rx) = sync_channel::<EncodedAudioPacket>(16);
        match ProcessTap::start(&request.excluded_apps, tx.clone()) {
            Ok(tap) => (Some(tx), Some(rx), Some(tap)),
            Err(error) => {
                eprintln!("[goDrinking] system audio tap unavailable: {error}");
                (Some(tx), Some(rx), None)
            }
        }
    } else {
        (None, None, None)
    };
    let peer = if capabilities.native_peer_transport_implemented {
        let frame_rate = match request.effective_frame_rate() {
            super::types::FrameRate::Fps60 => 60,
            super::types::FrameRate::Fps30 => 30,
        };
        let frame_duration = Duration::from_nanos(1_000_000_000 / frame_rate);
        match PeerTransport::new(
            pipeline.take_access_unit_receiver(),
            audio_rx,
            Arc::clone(&pipeline.encoder_control),
            frame_duration,
        ) {
            Ok(peer) => Some(peer),
            Err(error) => {
                if let Ok(mut state) = state.lock() {
                    state.lifecycle = MediaLifecycleState::Idle;
                    state.detail = format!("Native WebRTC peer start failed: {error}");
                }
                return Err(MediaEngineError::NativePeer(error));
            }
        }
    } else {
        None
    };
    #[cfg(target_os = "macos")]
    let mut adapter = super::screen_capture_kit::ScreenCaptureKitAdapter::new();
    #[cfg(target_os = "macos")]
    if capabilities.screen_capture_kit {
        if let Err(error) = adapter.start_capture(
            &request,
            pipeline.capture_tx.clone(),
            pipeline.encoder_tx.clone(),
            pipeline.preview_diagnostics(),
            pipeline.generation,
        ) {
            if let Ok(mut state) = state.lock() {
                state.lifecycle = MediaLifecycleState::Idle;
                state.detail = format!("Native capture start failed and is retryable: {error}");
            }
            return Err(MediaEngineError::NativeCapture(error.to_string()));
        }
    }
    #[cfg(target_os = "windows")]
    let mut adapter = super::windows_capture::WindowsCaptureAdapter::new();
    #[cfg(target_os = "windows")]
    if capabilities.windows_graphics_capture {
        if let Err(error) = adapter.start_capture(
            &request,
            pipeline.capture_tx.clone(),
            pipeline.encoder_tx.clone(),
            pipeline.preview_diagnostics(),
            pipeline.generation,
        ) {
            if let Ok(mut state) = state.lock() {
                state.lifecycle = MediaLifecycleState::Idle;
                state.detail = format!("Native capture start failed and is retryable: {error}");
            }
            return Err(MediaEngineError::NativeCapture(error));
        }
    }
    let native_capture_active = (cfg!(target_os = "macos") && capabilities.screen_capture_kit)
        || (cfg!(target_os = "windows") && capabilities.windows_graphics_capture);
    let room = LanRoom::start().ok();
    let mut state = state.lock().map_err(|_| MediaEngineError::StatePoisoned)?;
    let audio_note = if request.system_audio && audio_tap.is_none() {
        " System audio could not start; video is still sharing."
    } else if request.system_audio {
        " System audio is captured with selected apps excluded."
    } else {
        ""
    };
    state.session = Some(SessionRecord {
        id,
        generation: pipeline.generation,
        request,
        peer,
        _pipeline: pipeline,
        #[cfg(target_os = "macos")]
        adapter,
        #[cfg(target_os = "windows")]
        adapter,
        native_capture_active,
        room,
        audio_tap,
        audio_tx,
    });
    state.lifecycle = MediaLifecycleState::Running;
    state.detail = if capabilities.native_capture_implemented {
        format!(
            "Native capture is running. Share the session code on your LAN.{audio_note}"
        )
    } else {
        "Control session is running; native capture is not implemented on this platform.".into()
    };
    Ok(snapshot_from_state(&state))
}

/// Applies live settings to the active session. Capture, the room, and the
/// WebRTC peer are never torn down: quality is a live bitrate/keyframe update
/// and audio changes recreate only the process tap against the engine-owned
/// Opus channel.
fn update_in_state(
    state: &Arc<Mutex<EngineState>>,
    request: UpdateMediaSessionRequest,
) -> Result<MediaSessionSnapshot, MediaEngineError> {
    let mut state = state.lock().map_err(|_| MediaEngineError::StatePoisoned)?;
    let Some(session) = state.session.as_mut() else {
        return Err(MediaEngineError::NoActiveSession);
    };

    // 1. Quality: live bitrate + keyframe. The stored request (and its
    // derived resolution/frame_rate) follow the preset so snapshots reflect
    // the effective settings.
    if session.request.quality != request.quality {
        let _ = session._pipeline.set_bitrate(request.quality.bitrate());
        let _ = session._pipeline.force_keyframe();
        session.request.quality = request.quality;
        session.request.resolution = match request.quality {
            TransmissionQuality::Low => VideoResolution::P720,
            TransmissionQuality::Medium | TransmissionQuality::High => VideoResolution::P1080,
        };
        session.request.frame_rate = match request.quality {
            TransmissionQuality::High => FrameRate::Fps60,
            TransmissionQuality::Low | TransmissionQuality::Medium => FrameRate::Fps30,
        };
    }

    // 2. Audio: recreate only the process tap. The peer keeps its original
    // receiver, so a restarted tap feeds the same audio track.
    let mut audio_note = String::new();
    if request.system_audio {
        if let Some(tx) = session.audio_tx.clone() {
            session.audio_tap = None;
            match ProcessTap::start(&request.excluded_apps, tx) {
                Ok(tap) => {
                    session.audio_tap = Some(tap);
                    audio_note = " System audio restarted with updated exclusions.".into();
                }
                Err(error) => {
                    eprintln!("[goDrinking] system audio tap restart failed: {error}");
                    audio_note = format!(" System audio tap restart failed: {error}");
                }
            }
        } else {
            audio_note =
                " System audio cannot be added mid-session; restart the session to enable it."
                    .into();
        }
    } else {
        // Silence: drop the tap. The peer keeps its (now silent) audio track.
        session.audio_tap = None;
    }
    session.request.system_audio = request.system_audio;
    session.request.excluded_apps = request.excluded_apps;

    let mut detail =
        "Session settings updated; capture and peer transport kept running.".to_string();
    detail.push_str(&audio_note);
    state.detail = detail;
    Ok(snapshot_from_state(&state))
}

fn stop_in_state(
    state: &Arc<Mutex<EngineState>>,
) -> Result<MediaSessionSnapshot, MediaEngineError> {
    let mut session = {
        let mut state = state.lock().map_err(|_| MediaEngineError::StatePoisoned)?;
        if state.session.is_none() {
            return Err(MediaEngineError::NoActiveSession);
        }
        state.lifecycle = MediaLifecycleState::Stopping;
        state.session.take().expect("active session checked above")
    };
    #[cfg(target_os = "macos")]
    if let Err(error) = session.adapter.stop_capture(session.generation) {
        if let Ok(mut state) = state.lock() {
            state.session = Some(session);
            let native_lifecycle = state
                .session
                .as_ref()
                .expect("session was restored")
                .adapter
                .lifecycle();
            state.lifecycle = match native_lifecycle {
                super::screen_capture_kit::CaptureLifecycle::CleanupPending => {
                    MediaLifecycleState::CleanupPending
                }
                super::screen_capture_kit::CaptureLifecycle::Failed => MediaLifecycleState::Failed,
                super::screen_capture_kit::CaptureLifecycle::Stopping => {
                    MediaLifecycleState::Stopping
                }
                _ => MediaLifecycleState::Running,
            };
            state.detail = state
                .session
                .as_ref()
                .and_then(|session| session.adapter.failure_detail())
                .unwrap_or_else(|| format!("Native capture stop failed and is retryable: {error}"));
        }
        return Err(MediaEngineError::NativeCapture(error.to_string()));
    }

    #[cfg(target_os = "windows")]
    if let Err(error) = session.adapter.stop_capture() {
        if let Ok(mut state) = state.lock() {
            state.session = Some(session);
            state.lifecycle = MediaLifecycleState::Running;
            state.detail = format!("Native capture stop failed and is retryable: {error}");
        }
        return Err(MediaEngineError::NativeCapture(error));
    }

    let mut state = state.lock().map_err(|_| MediaEngineError::StatePoisoned)?;
    state.lifecycle = MediaLifecycleState::Idle;
    state.detail = "Session stopped; native pipeline handles released.".into();
    state.preview.begin_session();
    Ok(snapshot_from_state(&state))
}

fn snapshot_from_state(state: &EngineState) -> MediaSessionSnapshot {
    let Some(session) = state.session.as_ref() else {
        return MediaSessionSnapshot {
            state: state.lifecycle,
            detail: state.detail.clone(),
            ..MediaSessionSnapshot::idle(state.detail.clone())
        };
    };
    #[cfg(target_os = "macos")]
    let native_capture_active = session.native_capture_active
        && session.adapter.lifecycle() == super::screen_capture_kit::CaptureLifecycle::Running;
    #[cfg(not(target_os = "macos"))]
    let native_capture_active = session.native_capture_active;
    #[cfg(target_os = "macos")]
    let native_failed = session.adapter.lifecycle()
        == super::screen_capture_kit::CaptureLifecycle::Failed
        || session._pipeline.state.is_failed();
    #[cfg(target_os = "macos")]
    let native_cleanup_pending =
        session.adapter.lifecycle() == super::screen_capture_kit::CaptureLifecycle::CleanupPending;
    #[cfg(not(target_os = "macos"))]
    let native_failed = session._pipeline.state.is_failed();
    #[cfg(not(target_os = "macos"))]
    let native_cleanup_pending = false;
    #[cfg(target_os = "macos")]
    let native_failure_detail = session
        ._pipeline
        .state
        .failure()
        .or_else(|| session.adapter.failure_detail());
    #[cfg(not(target_os = "macos"))]
    let native_failure_detail = session._pipeline.state.failure();
    let peer_status = session.peer.as_ref().map(|peer| peer.status());
    let peer_failed = peer_status
        .as_ref()
        .is_some_and(|status| status.state == PeerTransportState::Failed);
    let preview_diagnostics = session._pipeline.preview_diagnostics();
    MediaSessionSnapshot {
        state: if native_cleanup_pending {
            MediaLifecycleState::CleanupPending
        } else if native_failed || peer_failed {
            MediaLifecycleState::Failed
        } else {
            state.lifecycle
        },
        session_id: Some(session.id.clone()),
        source: Some(session.request.source),
        source_id: session.request.source_id,
        resolution: Some(session.request.resolution),
        frame_rate: Some(session.request.frame_rate),
        system_audio: session.request.system_audio,
        excluded_apps: session.request.excluded_apps.clone(),
        native_capture_active,
        preview_callback_count: preview_diagnostics
            .callback_count
            .load(std::sync::atomic::Ordering::Acquire),
        preview_frame_count: preview_diagnostics
            .frame_count
            .load(std::sync::atomic::Ordering::Acquire),
        preview_dropped_count: preview_diagnostics
            .dropped_count
            .load(std::sync::atomic::Ordering::Acquire),
        preview_error: preview_diagnostics.error(),
        detail: native_failure_detail
            .or_else(|| peer_status.as_ref().map(|status| status.detail.clone()))
            .unwrap_or_else(|| state.detail.clone()),
        peer_state: peer_status
            .as_ref()
            .map(|status| status.state.clone())
            .unwrap_or(PeerTransportState::Disabled),
        peer_detail: peer_status
            .map(|status| status.detail)
            .unwrap_or_else(|| "Native peer transport is unavailable.".into()),
        session_code: session.room.as_ref().map(|room| room.code.clone()),
        lan_addresses: session
            .room
            .as_ref()
            .map(|_| LanRoom::addresses())
            .unwrap_or_default(),
        lan_port: session.room.as_ref().map(|room| room.port),
    }
}

fn refresh_native_state(state: &mut EngineState) {
    if let Some(session) = state.session.as_ref() {
        if let Some(detail) = session._pipeline.state.failure() {
            state.lifecycle = MediaLifecycleState::Failed;
            state.detail = detail;
        }
        if let Some(peer) = session.peer.as_ref() {
            if peer.status().state == PeerTransportState::Failed {
                state.lifecycle = MediaLifecycleState::Failed;
                state.detail = peer.status().detail;
            }
        }
    }
    #[cfg(target_os = "macos")]
    if let Some(session) = state.session.as_ref() {
        if session.adapter.lifecycle() == super::screen_capture_kit::CaptureLifecycle::Failed {
            state.lifecycle = MediaLifecycleState::Failed;
            if let Some(detail) = session.adapter.failure_detail() {
                state.detail = detail;
            }
        } else if session.adapter.lifecycle()
            == super::screen_capture_kit::CaptureLifecycle::CleanupPending
        {
            state.lifecycle = MediaLifecycleState::CleanupPending;
            if let Some(detail) = session.adapter.failure_detail() {
                state.detail = detail;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::capabilities::AppAudioExclusionSupport;
    use super::super::capabilities::MediaCapabilities;
    use super::super::pipeline::PreviewState;
    use super::super::types::MediaLifecycleState;
    use super::super::types::{
        CaptureSource, FrameRate, PreviewFrameEvent, TransmissionQuality, UpdateMediaSessionRequest,
        VideoResolution,
    };
    use super::{
        create_in_state, refresh_native_state, snapshot_from_state, stop_in_state, update_in_state,
        EngineState, MediaEngineError,
    };
    use std::sync::{Arc, Mutex};

    fn test_state() -> Arc<Mutex<EngineState>> {
        Arc::new(Mutex::new(EngineState {
            capabilities: MediaCapabilities {
                platform: "test".into(),
                supported: true,
                screen_capture_kit: false,
                screen_recording_authorization:
                    super::super::screen_capture_kit::ScreenRecordingAuthorization::Unsupported,
                source_enumeration_available: false,
                windows_graphics_capture: false,
                wasapi: false,
                process_loopback: false,
                app_audio_exclusion: AppAudioExclusionSupport::Unsupported,
                native_capture_implemented: false,
                native_encoder_implemented: false,
                native_peer_transport_implemented: false,
                detail: "test".into(),
            },
            lifecycle: MediaLifecycleState::Idle,
            session: None,
            next_session_id: 1,
            detail: "idle".into(),
            preview: Arc::new(PreviewState::new()),
        }))
    }

    fn request() -> super::super::types::CreateMediaSessionRequest {
        super::super::types::CreateMediaSessionRequest {
            source: CaptureSource::Screen,
            source_id: None,
            resolution: VideoResolution::P1080,
            frame_rate: FrameRate::Fps60,
            system_audio: false,
            excluded_apps: vec!["Discord".into()],
            quality: super::super::types::TransmissionQuality::High,
        }
    }

    #[test]
    fn state_transitions_create_and_stop_safely() {
        let state = test_state();
        let created = create_in_state(&state, request()).expect("create should succeed");
        assert_eq!(created.state, MediaLifecycleState::Running);
        assert!(created.session_id.is_some());
        assert!(!created.native_capture_active);
        let recreated =
            create_in_state(&state, request()).expect("duplicate create should recover");
        assert_ne!(recreated.session_id, created.session_id);
        let stopped = stop_in_state(&state).expect("stop should succeed");
        assert_eq!(stopped.state, MediaLifecycleState::Idle);
        assert!(stopped.session_id.is_none());
        assert_eq!(
            stop_in_state(&state),
            Err(MediaEngineError::NoActiveSession)
        );
    }

    #[test]
    fn unsupported_platform_does_not_enter_starting_state() {
        let state = test_state();
        state.lock().expect("test state").capabilities.supported = false;
        assert_eq!(
            create_in_state(&state, request()),
            Err(MediaEngineError::UnsupportedPlatform)
        );
        assert_eq!(
            state.lock().expect("test state").lifecycle,
            MediaLifecycleState::Idle
        );
    }

    #[test]
    fn session_boundaries_clear_stale_preview_state() {
        let state = test_state();
        state
            .lock()
            .expect("test state")
            .preview
            .latest
            .lock()
            .expect("preview state")
            .replace(PreviewFrameEvent {
                sequence: 7,
                timestamp_micros: 8,
                width: 2,
                height: 1,
                encoding: "rgb8_thumbnail".into(),
                payload: vec![1, 2, 3],
            });

        create_in_state(&state, request()).expect("control session should start");
        assert!(state
            .lock()
            .expect("test state")
            .preview
            .latest
            .lock()
            .expect("preview state")
            .is_none());
        stop_in_state(&state).expect("control session should stop");
        assert!(state
            .lock()
            .expect("test state")
            .preview
            .latest
            .lock()
            .expect("preview state")
            .is_none());
    }

    #[test]
    fn pipeline_failure_is_exposed_as_a_failed_snapshot_detail() {
        let state = test_state();
        create_in_state(&state, request()).expect("control session should start");
        state
            .lock()
            .expect("test state")
            .session
            .as_ref()
            .expect("session should exist")
            ._pipeline
            .state
            .fail("VideoToolbox encode failed: test failure");

        let mut state_guard = state.lock().expect("test state");
        refresh_native_state(&mut state_guard);
        let snapshot = snapshot_from_state(&state_guard);
        assert_eq!(snapshot.state, MediaLifecycleState::Failed);
        assert_eq!(snapshot.detail, "VideoToolbox encode failed: test failure");
    }

    #[test]
    fn system_audio_requests_still_start_video_when_the_tap_is_unavailable() {
        let state = test_state();
        let mut request = request();
        request.system_audio = true;
        let created = create_in_state(&state, request).expect("video session should start");
        assert_eq!(created.state, MediaLifecycleState::Running);
    }

    #[test]
    fn update_session_applies_quality_and_exclusions_without_teardown() {
        let state = test_state();
        create_in_state(&state, request()).expect("session should start");
        let snapshot = update_in_state(
            &state,
            UpdateMediaSessionRequest {
                quality: TransmissionQuality::Low,
                system_audio: false,
                excluded_apps: vec!["Discord".into(), "com.hnc.Discord".into()],
            },
        )
        .expect("update should succeed");
        assert_eq!(snapshot.state, MediaLifecycleState::Running);
        assert!(snapshot.session_id.is_some());
        // Quality preset is reflected in the derived resolution/frame_rate.
        assert_eq!(snapshot.resolution, Some(VideoResolution::P720));
        assert_eq!(snapshot.frame_rate, Some(FrameRate::Fps30));
        assert_eq!(
            snapshot.excluded_apps,
            vec!["Discord".to_string(), "com.hnc.Discord".to_string()]
        );
        assert!(!snapshot.system_audio);
        let stored = state
            .lock()
            .expect("test state")
            .session
            .as_ref()
            .expect("session should exist")
            .request
            .clone();
        assert_eq!(stored.quality, TransmissionQuality::Low);
        assert_eq!(
            stored.excluded_apps,
            vec!["Discord".to_string(), "com.hnc.Discord".to_string()]
        );
        assert!(!stored.system_audio);
    }

    #[test]
    fn update_session_cannot_add_audio_mid_session_without_a_channel() {
        let state = test_state();
        create_in_state(&state, request()).expect("session should start");
        let snapshot = update_in_state(
            &state,
            UpdateMediaSessionRequest {
                quality: TransmissionQuality::Medium,
                system_audio: true,
                excluded_apps: Vec::new(),
            },
        )
        .expect("update should not fail the session");
        assert_eq!(snapshot.state, MediaLifecycleState::Running);
        assert!(snapshot.detail.contains("cannot be added mid-session"));
        // Quality still applied even though audio could not be added.
        assert_eq!(snapshot.resolution, Some(VideoResolution::P1080));
        assert_eq!(snapshot.frame_rate, Some(FrameRate::Fps30));
    }

    #[test]
    fn update_session_without_an_active_session_fails() {
        let state = test_state();
        assert_eq!(
            update_in_state(
                &state,
                UpdateMediaSessionRequest {
                    quality: TransmissionQuality::High,
                    system_audio: false,
                    excluded_apps: Vec::new(),
                },
            ),
            Err(MediaEngineError::NoActiveSession)
        );
    }
}
