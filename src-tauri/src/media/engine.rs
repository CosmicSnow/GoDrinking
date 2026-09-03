use super::capabilities;
use super::fanout::MediaFanout;
use super::logger;
use super::peer_transport::{PeerSignal, PeerTransport, PeerTransportClient};
use super::pipeline::{NativePipeline, PreviewState};
use super::process_tap::{EncodedAudioPacket, ProcessTap};
use super::rendezvous::{StunarHost, StunarViewer};
use super::room::{DirectRoom, LanRoom, OfferMint, ViewerCount};
use super::session_gate::SessionGate;
use super::types::{
    CreateMediaSessionRequest, FrameRate, JoinMode, MediaLifecycleState, MediaSessionSnapshot,
    NativeCaptureSource, NativeRunningApp, PeerTransportState, PreviewFrameEvent, RosterEntry,
    TransmissionQuality, UpdateCredentialsRequest, UpdateMediaSessionRequest, VideoCodec,
    VideoResolution, MediaSessionStats, ViewerLinkStats,
};
use super::MediaCapabilities;
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const CONTROL_QUEUE_CAPACITY: usize = 32;
const MAX_VIEWERS: usize = 8;

struct ViewerLink {
    id: String,
    nickname: String,
    peer: PeerTransport,
}

struct SessionRecord {
    id: String,
    generation: u64,
    request: CreateMediaSessionRequest,
    viewers: HashMap<String, ViewerLink>,
    fanout: Option<MediaFanout>,
    // Kept alive with the session so capture and preview workers share the
    // bounded pipeline ownership boundary.
    _pipeline: NativePipeline,
    #[cfg(target_os = "macos")]
    adapter: super::screen_capture_kit::ScreenCaptureKitAdapter,
    #[cfg(target_os = "windows")]
    adapter: super::windows_capture::WindowsCaptureAdapter,
    native_capture_active: bool,
    room: Option<LanRoom>,
    direct_room: Option<DirectRoom>,
    // Stunar mode: the Host's Rendezvous connection (open + heartbeat + WS).
    stunar: Option<Arc<StunarHost>>,
    // Password/Admission/Ignore list for the Session. Shared with the room's
    // TCP threads; connected Viewers are never touched by credential updates.
    gate: Arc<SessionGate>,
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
    // Viewer-side Stunar WS, kept alive between ask and answer.
    stunar_viewer: Option<StunarViewer>,
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
    Unsupported(String),
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
            Self::Unsupported(message) => return formatter.write_str(message),
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
            stunar_viewer: None,
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
                av1_encode_supported: false,
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
        self.apply_stunar_accepts();
        self.state
            .lock()
            .map(|mut state| {
                refresh_native_state(&mut state);
                snapshot_from_state(&state)
            })
            .unwrap_or_else(|_| MediaSessionSnapshot::idle("Native media state is unavailable."))
    }

    /// Stunar with Admission off accepts Viewers immediately on the
    /// Rendezvous, so there is no pending step to trigger the mint. Polled
    /// from `snapshot()`: any accepted Viewer without a ViewerLink gets an
    /// offer minted and sent over the WS inbox.
    fn apply_stunar_accepts(&self) {
        let to_mint = self.state.lock().ok().and_then(|state| {
            let session = state.session.as_ref()?;
            let stunar = session.stunar.as_ref()?;
            let accepted = stunar.accepted_roster();
            Some(
                accepted
                    .into_iter()
                    .filter(|(id, _)| !session.viewers.contains_key(id))
                    .collect::<Vec<_>>(),
            )
        });
        let Some(to_mint) = to_mint else {
            return;
        };
        for (id, nickname) in to_mint {
            let Ok(signal) = mint_viewer_offer(&self.state, &id, &nickname) else {
                continue;
            };
            let stunar = self.state.lock().ok().and_then(|state| {
                state
                    .session
                    .as_ref()
                    .and_then(|session| session.stunar.clone())
            });
            if let Some(stunar) = stunar {
                let _ = stunar.send_signal(&id, &signal);
            }
        }
    }

    fn apply_room_answer(&self) {
        let pending = self.state.lock().ok().and_then(|state| {
            let session = state.session.as_ref()?;
            session
                .room
                .as_ref()
                .map(|room| room.take_answers())
                .or_else(|| session.direct_room.as_ref().map(|room| room.take_answers()))
                .or_else(|| session.stunar.as_ref().map(|stunar| stunar.take_answers()))
        });
        let Some(answers) = pending else {
            return;
        };
        for answer in answers {
            let client = self.state.lock().ok().and_then(|state| {
                let session = state.session.as_ref()?;
                let id = answer.id.as_deref()?;
                Some(session.viewers.get(id)?.peer.client())
            });
            if let Some(client) = client {
                let _ = client.set_answer(answer);
            }
        }
    }

    pub fn kick_viewer(&self, id: &str) -> Result<MediaSessionSnapshot, MediaEngineError> {
        logger::log("INFO", "kick", &format!("viewer={id}"));
        let stunar = {
            let state = self
                .state
                .lock()
                .map_err(|_| MediaEngineError::StatePoisoned)?;
            state
                .session
                .as_ref()
                .and_then(|session| session.stunar.clone())
        };
        if let Some(stunar) = stunar {
            // Best-effort: the Viewer learns the kick via the Rendezvous WS.
            let _ = stunar.kick(id);
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| MediaEngineError::StatePoisoned)?;
        let session = state
            .session
            .as_mut()
            .ok_or(MediaEngineError::NoActiveSession)?;
        if let Some(viewer) = session.viewers.remove(id) {
            if let Some(fanout) = session.fanout.as_ref() {
                fanout.unsubscribe(id);
            }
            drop(viewer);
        }
        Ok(snapshot_from_state(&state))
    }

    /// Accepts a Pending Viewer. LAN/Direct wake the room's TCP thread via
    /// the SessionGate; Stunar tells the Rendezvous, then mints the offer and
    /// sends it over the WS inbox.
    pub fn admit_viewer(&self, id: &str) -> Result<MediaSessionSnapshot, MediaEngineError> {
        let stunar_path = {
            let state = self
                .state
                .lock()
                .map_err(|_| MediaEngineError::StatePoisoned)?;
            let session = state
                .session
                .as_ref()
                .ok_or(MediaEngineError::NoActiveSession)?;
            session.stunar.as_ref().map(|stunar| {
                let nickname = stunar
                    .pending_nickname(id)
                    .unwrap_or_else(|| "Viewer".to_owned());
                (stunar.clone(), nickname)
            })
        };
        if let Some((stunar, nickname)) = stunar_path {
            stunar
                .decide(id, true)
                .map_err(MediaEngineError::NativePeer)?;
            let signal = mint_viewer_offer(&self.state, id, &nickname)
                .map_err(MediaEngineError::NativePeer)?;
            stunar
                .send_signal(id, &signal)
                .map_err(MediaEngineError::NativePeer)?;
            let state = self
                .state
                .lock()
                .map_err(|_| MediaEngineError::StatePoisoned)?;
            return Ok(snapshot_from_state(&state));
        }
        let state = self
            .state
            .lock()
            .map_err(|_| MediaEngineError::StatePoisoned)?;
        let session = state
            .session
            .as_ref()
            .ok_or(MediaEngineError::NoActiveSession)?;
        session.gate.decide(id, true);
        Ok(snapshot_from_state(&state))
    }

    /// Rejects a Pending Viewer. LAN/Direct send `REJECT` on the room TCP;
    /// Stunar tells the Rendezvous (the Viewer's WS gets `rejected`).
    pub fn reject_viewer(&self, id: &str) -> Result<MediaSessionSnapshot, MediaEngineError> {
        let stunar = {
            let state = self
                .state
                .lock()
                .map_err(|_| MediaEngineError::StatePoisoned)?;
            state
                .session
                .as_ref()
                .and_then(|session| session.stunar.clone())
        };
        if let Some(stunar) = stunar {
            stunar
                .decide(id, false)
                .map_err(MediaEngineError::NativePeer)?;
            let state = self
                .state
                .lock()
                .map_err(|_| MediaEngineError::StatePoisoned)?;
            return Ok(snapshot_from_state(&state));
        }
        let state = self
            .state
            .lock()
            .map_err(|_| MediaEngineError::StatePoisoned)?;
        let session = state
            .session
            .as_ref()
            .ok_or(MediaEngineError::NoActiveSession)?;
        session.gate.decide(id, false);
        Ok(snapshot_from_state(&state))
    }

    /// Rotates Session credentials live (PRD-18) without touching Connected
    /// Viewers, capture, the fanout, or the peers. Only new requests use the
    /// new values:
    /// - LAN/Direct: Password updates the gate for new AUTH; Admission toggles
    ///   live. No Room code rotation: the code is server-owned on Stunar and
    ///   fixed for the Session lifetime everywhere.
    /// - Stunar: Password rotates on the Rendezvous (same room, same
    ///   host_token, viewer tokens repointed) and is mandatory (4-64).
    ///   Admission is fixed at open there.
    pub fn update_session_credentials(
        &self,
        request: UpdateCredentialsRequest,
    ) -> Result<MediaSessionSnapshot, MediaEngineError> {
        update_credentials_in_state(&self.state, request)
    }

    /// Session-wide encoder diagnostics + per-viewer link diagnostics
    /// (state + RTT in ms) for the Host status popup. Peer status is read
    /// under the lock; RTT queries run after it is released so snapshot
    /// polling never blocks on stats collection.
    pub fn viewer_link_stats(&self) -> MediaSessionStats {
        let collected: (Vec<(String, String, PeerTransportState, PeerTransportClient)>, u32, Option<u32>, u32) = self
            .state
            .lock()
            .ok()
            .and_then(|state| {
                state.session.as_ref().map(|session| {
                    let links = session
                        .viewers
                        .iter()
                        .map(|(id, viewer)| {
                            let status = viewer.peer.status();
                            (
                                id.clone(),
                                viewer.nickname.clone(),
                                status.state,
                                viewer.peer.client(),
                            )
                        })
                        .collect();
                    (
                        links,
                        session._pipeline.bitrate_target(),
                        session._pipeline.bitrate_congestion(),
                        session._pipeline.bitrate_floor(),
                    )
                })
            })
            .unwrap_or_default();
        let (links, target_bps, congestion_bps, floor_bps) = collected;
        let links = links
            .into_iter()
            .map(|(id, nickname, state, client)| {
                let rtt_ms = client.rtt_ms().map(|rtt| (rtt * 10.0).round() / 10.0);
                ViewerLinkStats {
                    id,
                    nickname,
                    state,
                    rtt_ms,
                }
            })
            .collect();
        MediaSessionStats {
            links,
            target_bps,
            congestion_bps,
            floor_bps,
        }
    }

    pub fn running_applications(&self) -> Result<Vec<NativeRunningApp>, MediaEngineError> {
        Ok(super::process_tap::running_applications())
    }

    pub fn publish_peer_offer(&self, _signal: PeerSignal) -> Result<(), MediaEngineError> {
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
        Err(MediaEngineError::NativePeer(
            "offers are created when a viewer joins".into(),
        ))
    }

    pub fn accept_peer_offer(&self, _offer: PeerSignal) -> Result<PeerSignal, MediaEngineError> {
        Err(MediaEngineError::NativePeer(
            "native accept_offer is unused; the viewer is browser WebRTC".into(),
        ))
    }

    pub fn set_peer_answer(&self, answer: PeerSignal) -> Result<(), MediaEngineError> {
        let client = {
            let state = self
                .state
                .lock()
                .map_err(|_| MediaEngineError::StatePoisoned)?;
            let session = state
                .session
                .as_ref()
                .ok_or(MediaEngineError::NoActiveSession)?;
            let id = answer
                .id
                .as_deref()
                .ok_or_else(|| MediaEngineError::NativePeer("answer is missing viewer id".into()))?;
            session
                .viewers
                .get(id)
                .map(|viewer| viewer.peer.client())
                .ok_or_else(|| MediaEngineError::NativePeer("unknown viewer".into()))?
        };
        client
            .set_answer(answer)
            .map_err(MediaEngineError::NativePeer)
    }

    pub fn close_peer_transport(&self) -> Result<(), MediaEngineError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| MediaEngineError::StatePoisoned)?;
        let session = state
            .session
            .as_mut()
            .ok_or(MediaEngineError::NoActiveSession)?;
        let ids: Vec<String> = session.viewers.keys().cloned().collect();
        for id in ids {
            if let Some(fanout) = session.fanout.as_ref() {
                fanout.unsubscribe(&id);
            }
            session.viewers.remove(&id);
        }
        Ok(())
    }

    /// Viewer-side Stunar: asks the Rendezvous and waits for the offer.
    /// Returns the viewer_token (used as the "host" handle for the answer)
    /// and keeps the WS open for `submit_stunar_answer`.
    pub fn discover_stunar(
        &self,
        base: &str,
        code: &str,
        password: &str,
        nickname: &str,
    ) -> Result<(String, PeerSignal), MediaEngineError> {
        let (token, offer, viewer) = super::rendezvous::discover_stunar_room(
            base, code, password, nickname,
        )
        .map_err(MediaEngineError::NativePeer)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| MediaEngineError::StatePoisoned)?;
        state.stunar_viewer = Some(viewer);
        Ok((token, offer))
    }

    /// Viewer-side Stunar: sends the answer signal over the stored WS.
    pub fn submit_stunar_answer(&self, answer: PeerSignal) -> Result<(), MediaEngineError> {
        let viewer = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| MediaEngineError::StatePoisoned)?;
            state
                .stunar_viewer
                .take()
                .ok_or_else(|| MediaEngineError::NativePeer("no stunar session".into()))?
        };
        super::rendezvous::submit_stunar_answer(viewer, &answer)
            .map_err(MediaEngineError::NativePeer)
    }

    /// Viewer-side Stunar: drops the stored WS (disconnect).
    pub fn close_stunar_viewer(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.stunar_viewer = None;
        }
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

/// Stunar rooms always carry a Password (4-64 chars, counted like the
/// Rendezvous does: Unicode code points). LAN/Direct keep optional.
fn valid_password(password: &str) -> bool {
    let len = password.chars().count();
    (4..=64).contains(&len)
}

fn create_in_state(
    state: &Arc<Mutex<EngineState>>,
    request: CreateMediaSessionRequest,
) -> Result<MediaSessionSnapshot, MediaEngineError> {
    logger::begin_session(
        "host",
        match request.join_mode {
            JoinMode::Lan => "lan",
            JoinMode::Direct => "direct",
            JoinMode::Stunar => "stunar",
        },
    );
    logger::log(
        "INFO",
        "create session",
        &format!(
            "join_mode={:?} rendezvous_url={} admission={}",
            request.join_mode,
            request.rendezvous_url.as_deref().unwrap_or("none"),
            request.admission,
        ),
    );
    // Firewall: só Direct/LAN precisam inbound TCP; Stunar é só outbound.
    crate::media::firewall::ensure_firewall_for_host(request.join_mode);
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
    // Stunar opens the room on the Rendezvous before anything else starts,
    // so a failure (missing URL, unreachable relay) aborts cleanly. The
    // Rendezvous generates the Room code; the Host never sends one.
    let stunar = match request.join_mode {
        JoinMode::Stunar => {
            let base = request.rendezvous_url.as_deref().ok_or_else(|| {
                MediaEngineError::NativePeer("Set the Stunar URL in settings.".into())
            })?;
            // Password is mandatory (4-64) on every Stunar room; the server
            // rejects open without it, so fail fast with a clear message.
            if !valid_password(&request.password) {
                logger::log(
                    "ERROR",
                    "create session",
                    "stunar password rejected locally (4-64 characters)",
                );
                return Err(MediaEngineError::NativePeer(
                    "Stunar requires a password (4-64 characters).".into(),
                ));
            }
            Some(Arc::new(
                StunarHost::start(base, &request.password, &request.nickname, request.admission)
                    .map_err(|error| {
                        logger::log("ERROR", "stunar open", &error);
                        MediaEngineError::NativePeer(error)
                    })?,
            ))
        }
        JoinMode::Lan | JoinMode::Direct => None,
    };
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

    // HEVC and H.264 High need a VideoToolbox encoder: reject them on
    // Windows at Start so the session never begins with an encoder nobody
    // can feed (Windows uses Baseline-only OpenH264).
    if request.codec != VideoCodec::H264 && cfg!(target_os = "windows") {
        return Err(MediaEngineError::Unsupported(
            "HEVC and H.264 High are macOS-only for now (Windows uses OpenH264 Baseline)".into(),
        ));
    }
    preview.begin_session();
    let mut pipeline = NativePipeline::new(
        preview,
        request.resolution,
        request.frame_rate,
        request.quality,
        request.bitrate_bps,
        request.min_bitrate_bps,
        request.codec,
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
    let fanout = if capabilities.native_peer_transport_implemented {
        Some(MediaFanout::start(
            pipeline.take_access_unit_receiver(),
            audio_rx,
        ))
    } else {
        let _ = audio_rx;
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
    let gate = Arc::new(SessionGate::new(request.password.clone(), request.admission));
    let mint_state = Arc::clone(&state);
    let mint: OfferMint = Arc::new(move |id: &str, nickname: &str| {
        mint_viewer_offer(&mint_state, id, nickname)
    });
    let count_state = Arc::clone(&state);
    let viewer_count: ViewerCount = Arc::new(move || {
        count_state
            .lock()
            .ok()
            .and_then(|state| state.session.as_ref().map(|session| session.viewers.len()))
            .unwrap_or(0)
    });
    let room = match request.join_mode {
        JoinMode::Lan => {
            LanRoom::start(
                Arc::clone(&mint),
                Arc::clone(&gate),
                Arc::clone(&viewer_count),
                request.nickname.clone(),
            )
            .ok()
        }
        JoinMode::Direct | JoinMode::Stunar => None,
    };
    let direct_room = match request.join_mode {
        JoinMode::Direct => {
            DirectRoom::start(mint, Arc::clone(&gate), viewer_count, request.nickname.clone()).ok()
        }
        JoinMode::Lan | JoinMode::Stunar => None,
    };
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
        viewers: HashMap::new(),
        fanout,
        _pipeline: pipeline,
        #[cfg(target_os = "macos")]
        adapter,
        #[cfg(target_os = "windows")]
        adapter,
        native_capture_active,
        room,
        direct_room,
        stunar,
        gate,
        audio_tap,
        audio_tx,
    });
    state.lifecycle = MediaLifecycleState::Running;
    state.detail = if capabilities.native_capture_implemented {
        let share_note = match state.session.as_ref().expect("session was set").request.join_mode
        {
            JoinMode::Lan => "Share the session code on your LAN.",
            JoinMode::Direct => "Share your address and port.",
            JoinMode::Stunar => "Share the session code. Needs the relay.",
        };
        format!("Native capture is running. {share_note}{audio_note}")
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

    // 1. Quality/bitrate: live bitrate + keyframe. The stored request (and
    // its derived resolution/frame_rate) follow the preset so snapshots
    // reflect the effective settings. An explicit bitrate override wins
    // over the preset for the encoder target.
    let target_changed = session.request.quality != request.quality
        || session.request.bitrate_bps != request.bitrate_bps;
    let target = super::types::resolve_bitrate(request.quality, request.bitrate_bps);
    let floor = super::types::resolve_floor(target, request.min_bitrate_bps);
    if target_changed {
        let _ = session._pipeline.set_bitrate(target);
        let _ = session._pipeline.force_keyframe();
        session.request.quality = request.quality;
        session.request.bitrate_bps = request.bitrate_bps;
        session.request.resolution = match request.quality {
            TransmissionQuality::Low => VideoResolution::P720,
            TransmissionQuality::Medium | TransmissionQuality::High => VideoResolution::P1080,
        };
        session.request.frame_rate = match request.quality {
            TransmissionQuality::High => FrameRate::Fps60,
            TransmissionQuality::Low | TransmissionQuality::Medium => FrameRate::Fps30,
        };
    }
    if session.request.min_bitrate_bps != request.min_bitrate_bps {
        // A raised floor re-asserts the encoder immediately so a collapsed
        // stream recovers without waiting for the next REMB.
        let _ = session._pipeline.set_floor(floor);
        session.request.min_bitrate_bps = request.min_bitrate_bps;
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

/// Rotates Session credentials live (PRD-18) without touching Connected
/// Viewers, capture, the fanout, or the peers. Only new requests use the
/// new values:
/// - LAN/Direct: Password updates the gate for new AUTH; Admission toggles
///   live. No Room code rotation.
/// - Stunar: Password rotates on the Rendezvous (same room, same host_token,
///   viewer tokens repointed) and is mandatory (4-64). Admission is fixed at
///   open there.
fn update_credentials_in_state(
    state: &Arc<Mutex<EngineState>>,
    request: UpdateCredentialsRequest,
) -> Result<MediaSessionSnapshot, MediaEngineError> {
    let stunar = {
        let state = state.lock().map_err(|_| MediaEngineError::StatePoisoned)?;
        let session = state
            .session
            .as_ref()
            .ok_or(MediaEngineError::NoActiveSession)?;
        session.stunar.clone()
    };
    // Stunar rotate is a network call; run it without the state lock.
    if let Some(stunar) = stunar {
        if let Some(password) = request.password.as_deref() {
            // Stunar rooms always have a Password; the server rejects empty
            // or short ones, so fail fast with a clear message.
            if !valid_password(password) {
                return Err(MediaEngineError::NativePeer(
                    "Stunar requires a password (4-64 characters).".into(),
                ));
            }
            stunar
                .rotate(Some(password))
                .map_err(MediaEngineError::NativePeer)?;
            logger::log("INFO", "rotate", "stunar password rotated");
        }
    }
    // Local gate: new AUTH uses the new Password; Connected Viewers stay.
    let state = state.lock().map_err(|_| MediaEngineError::StatePoisoned)?;
    let session = state
        .session
        .as_ref()
        .ok_or(MediaEngineError::NoActiveSession)?;
    if let Some(password) = request.password.as_deref() {
        session.gate.set_password(password.to_owned());
        logger::log("INFO", "credentials", "password updated (lan/direct)");
    }
    if let Some(admission) = request.admission {
        session.gate.set_admission(admission);
        logger::log("INFO", "credentials", &format!("admission={admission}"));
    }
    Ok(snapshot_from_state(&state))
}

fn stop_in_state(
    state: &Arc<Mutex<EngineState>>,
) -> Result<MediaSessionSnapshot, MediaEngineError> {
    logger::log("INFO", "session", "stop");
    let mut session = {
        let mut state = state.lock().map_err(|_| MediaEngineError::StatePoisoned)?;
        if state.session.is_none() {
            return Err(MediaEngineError::NoActiveSession);
        }
        state.lifecycle = MediaLifecycleState::Stopping;
        state.session.take().expect("active session checked above")
    };
    // Stunar: close the room on the Rendezvous (best-effort) and stop the
    // heartbeat/WS worker before releasing the rest of the session.
    if let Some(stunar) = session.stunar.as_ref() {
        stunar.close();
    }
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
    let mut roster: Vec<RosterEntry> = session
        .viewers
        .values()
        .map(|viewer| {
            let status = viewer.peer.status();
            RosterEntry {
                id: viewer.id.clone(),
                nickname: viewer.nickname.clone(),
                state: status.state,
            }
        })
        .collect();
    // Pending Viewers (Admission on) have no PeerTransport yet; they appear
    // with the dedicated Pending state so the UI can offer Accept/Decline.
    for (id, nickname) in session.gate.pending_roster() {
        roster.push(RosterEntry {
            id,
            nickname,
            state: PeerTransportState::Pending,
        });
    }
    // Stunar pending Viewers live on the Rendezvous, not in the gate.
    if let Some(stunar) = session.stunar.as_ref() {
        for (id, nickname) in stunar.pending_roster() {
            roster.push(RosterEntry {
                id,
                nickname,
                state: PeerTransportState::Pending,
            });
        }
    }
    let peer_status = session
        .viewers
        .values()
        .map(|viewer| viewer.peer.status())
        .max_by_key(|status| match status.state {
            PeerTransportState::Connected => 5,
            PeerTransportState::Connecting => 4,
            PeerTransportState::New | PeerTransportState::Starting => 3,
            PeerTransportState::Failed => 2,
            PeerTransportState::Disconnected | PeerTransportState::Closed => 1,
            PeerTransportState::Pending | PeerTransportState::Disabled => 0,
        });
    let preview_diagnostics = session._pipeline.preview_diagnostics();
    MediaSessionSnapshot {
        state: if native_cleanup_pending {
            MediaLifecycleState::CleanupPending
        } else if native_failed {
            MediaLifecycleState::Failed
        } else {
            state.lifecycle
        },
        session_id: Some(session.id.clone()),
        source: Some(session.request.source),
        source_id: session.request.source_id,
        resolution: Some(session.request.resolution),
        frame_rate: Some(session.request.frame_rate),
        bitrate_bps: session._pipeline.bitrate_target(),
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
        session_code: session
            .room
            .as_ref()
            .map(|room| room.code())
            .or_else(|| session.stunar.as_ref().map(|stunar| stunar.code())),
        lan_addresses: session
            .room
            .as_ref()
            .map(|_| LanRoom::addresses())
            .unwrap_or_default(),
        lan_port: session.room.as_ref().map(|room| room.port),
        roster,
        password_set: session.gate.password_set(),
        admission: session.gate.admission(),
        join_mode: session.request.join_mode,
        direct_listen_port: session.direct_room.as_ref().map(|room| room.port),
        direct_addresses: session
            .direct_room
            .as_ref()
            .map(|room| room.addresses())
            .unwrap_or_default(),
        direct_mapping: session
            .direct_room
            .as_ref()
            .map(|room| room.mapping)
            .unwrap_or(false),
        stunar_state: session.stunar.as_ref().map(|stunar| stunar.state()),
    }
}

fn refresh_native_state(state: &mut EngineState) {
    if let Some(session) = state.session.as_mut() {
        if let Some(detail) = session._pipeline.state.failure() {
            state.lifecycle = MediaLifecycleState::Failed;
            state.detail = detail;
        }
        // A Viewer whose WebRTC peer failed is gone for good (the Viewer
        // re-joins with a fresh connection). Drop it so the Roster and the
        // fanout do not keep a dead entry.
        let failed: Vec<String> = session
            .viewers
            .iter()
            .filter(|(_, viewer)| viewer.peer.status().state == PeerTransportState::Failed)
            .map(|(id, _)| id.clone())
            .collect();
        for id in failed {
            if let Some(fanout) = session.fanout.as_ref() {
                fanout.unsubscribe(&id);
            }
            session.viewers.remove(&id);
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

fn mint_viewer_offer(
    state: &Arc<Mutex<EngineState>>,
    id: &str,
    nickname: &str,
) -> Result<PeerSignal, String> {
    logger::log("INFO", "mint offer", &format!("viewer={id} nickname={nickname}"));
    let (video_rx, audio_rx, encoder_control, frame_duration, join_mode, video_codec) = {
        let mut guard = state.lock().map_err(|_| "media state is unavailable".to_owned())?;
        let session = guard
            .session
            .as_mut()
            .ok_or_else(|| "no media session is active".to_owned())?;
        if session.viewers.len() >= MAX_VIEWERS {
            logger::log("WARN", "mint offer", "session is full (8 viewers)");
            return Err("session is full".into());
        }
        let fanout = session
            .fanout
            .as_ref()
            .ok_or_else(|| "peer transport is unavailable".to_owned())?;
        let (video_rx, audio_rx) = fanout.subscribe(id);
        let frame_rate = match session.request.effective_frame_rate() {
            FrameRate::Fps60 => 60,
            FrameRate::Fps30 => 30,
        };
        (
            video_rx,
            audio_rx,
            Arc::clone(&session._pipeline.encoder_control),
            Duration::from_nanos(1_000_000_000 / frame_rate),
            session.request.join_mode,
            session.request.codec,
        )
    };
    let peer = PeerTransport::new(
        video_rx,
        audio_rx,
        encoder_control,
        frame_duration,
        join_mode,
        video_codec,
    )?;
    let mut signal = peer
        .client()
        .create_offer()
        .map_err(|error| error.to_string())?;
    signal.id = Some(id.to_owned());
    let mut guard = state.lock().map_err(|_| "media state is unavailable".to_owned())?;
    let session = guard
        .session
        .as_mut()
        .ok_or_else(|| "no media session is active".to_owned())?;
    if session.viewers.len() >= MAX_VIEWERS {
        if let Some(fanout) = session.fanout.as_ref() {
            fanout.unsubscribe(id);
        }
        logger::log("WARN", "mint offer", "session is full (8 viewers)");
        return Err("session is full".into());
    }
    session.viewers.insert(
        id.to_owned(),
        ViewerLink {
            id: id.to_owned(),
            nickname: nickname.to_owned(),
            peer,
        },
    );
    Ok(signal)
}

#[cfg(test)]
mod tests {
    use super::super::capabilities::AppAudioExclusionSupport;
    use super::super::capabilities::MediaCapabilities;
    use super::super::pipeline::PreviewState;
    use super::super::types::MediaLifecycleState;
    use super::super::types::{
        CaptureSource, FrameRate, JoinMode, PreviewFrameEvent, TransmissionQuality,
        UpdateCredentialsRequest, UpdateMediaSessionRequest, VideoCodec, VideoResolution,
    };
use super::{
    create_in_state, refresh_native_state, snapshot_from_state, stop_in_state,
    update_credentials_in_state, update_in_state, EngineState, MediaEngineError,
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
                av1_encode_supported: false,
                detail: "test".into(),
            },
            lifecycle: MediaLifecycleState::Idle,
            session: None,
            next_session_id: 1,
            detail: "idle".into(),
            preview: Arc::new(PreviewState::new()),
            stunar_viewer: None,
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
            bitrate_bps: None,
            min_bitrate_bps: None,
            codec: super::super::types::VideoCodec::H264,
            password: String::new(),
            nickname: "Host".into(),
            admission: false,
            join_mode: JoinMode::Lan,
            rendezvous_url: None,
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
    fn stunar_without_url_is_rejected_before_starting() {
        let state = test_state();
        let mut request = request();
        request.join_mode = JoinMode::Stunar;
        request.rendezvous_url = None;
        assert_eq!(
            create_in_state(&state, request),
            Err(MediaEngineError::NativePeer(
                "Set the Stunar URL in settings.".into()
            ))
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
                bitrate_bps: None,
                min_bitrate_bps: None,
                codec: VideoCodec::H264,
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
                bitrate_bps: None,
                min_bitrate_bps: None,
                codec: VideoCodec::H264,
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
                    bitrate_bps: None,
                    min_bitrate_bps: None,
                    codec: VideoCodec::H264,
                    system_audio: false,
                    excluded_apps: Vec::new(),
                },
            ),
            Err(MediaEngineError::NoActiveSession)
        );
    }

    #[test]
    fn rotate_credentials_keeps_the_session_and_updates_the_password() {
        let state = test_state();
        create_in_state(&state, request()).expect("session should start");
        let snapshot = update_credentials_in_state(
            &state,
            UpdateCredentialsRequest {
                password: Some("nova".into()),
                admission: None,
            },
        )
        .expect("rotate should succeed");
        assert_eq!(snapshot.state, MediaLifecycleState::Running);
        assert!(snapshot.password_set);
        // The session (and its room) is still alive.
        let stored = state.lock().expect("test state");
        let session = stored.session.as_ref().expect("session should exist");
        assert!(session.gate.password_set());
    }

    #[test]
    fn stunar_requires_a_password_before_starting() {
        let state = test_state();
        let mut request = request();
        request.join_mode = JoinMode::Stunar;
        request.rendezvous_url = Some("http://127.0.0.1:8787".into());
        // Empty and too-short passwords are rejected before any network call.
        request.password = String::new();
        assert_eq!(
            create_in_state(&state, request.clone()),
            Err(MediaEngineError::NativePeer(
                "Stunar requires a password (4-64 characters).".into()
            ))
        );
        request.password = "abc".into();
        assert_eq!(
            create_in_state(&state, request),
            Err(MediaEngineError::NativePeer(
                "Stunar requires a password (4-64 characters).".into()
            ))
        );
        assert_eq!(
            state.lock().expect("test state").lifecycle,
            MediaLifecycleState::Idle
        );
    }
}
