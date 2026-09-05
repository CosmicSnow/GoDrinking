use super::capabilities;
use super::control_plane::{
    EpochFence, LinkId, OfferEpochFence, OperationFence, OperationKind, SessionActor, SessionEpoch,
    ShareEpoch,
};
use super::fanout::MediaFanout;
use super::logger;
use super::peer_transport::{
    PeerSignal, PeerTransport, PeerTransportClient, PeerTransportInitError, PendingPeerTransport,
};
use super::pipeline::{NativePipeline, PreviewState};
use super::process_tap::{EncodedAudioPacket, ProcessTap};
use super::rendezvous::{StunarHost, StunarViewer};
use super::room::{DirectRoom, ExactOffer, ExactOfferMint, LanRoom, ViewerCount};
#[cfg(target_os = "macos")]
use super::screen_capture_kit::CaptureCancellationToken;
use super::session_gate::SessionGate;
use super::types::{
    CreateMediaSessionRequest, FrameRate, JoinMode, MediaLifecycleState, MediaSessionSnapshot,
    MediaSessionStats, NativeCaptureSource, NativeRunningApp, PeerTransportState,
    PreviewFrameEvent, RosterEntry, SourceIdUpdate, UpdateCredentialsRequest,
    UpdateMediaSessionRequest, VideoCodec, VideoResolution, ViewerLinkStats,
};
#[cfg(target_os = "windows")]
use super::windows_capture::CaptureCancellationToken;
use super::MediaCapabilities;
use std::collections::{HashMap, HashSet};
use std::fmt::{Display, Formatter};
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const CONTROL_QUEUE_CAPACITY: usize = 32;
const MAX_VIEWERS: usize = 8;

struct ViewerLink {
    id: String,
    nickname: String,
    peer: PeerTransport,
    /// Last offer SDP sent to this viewer (kept so a signal lost on the
    /// Rendezvous WS can be resent without minting a fresh peer).
    last_offer: Option<PeerSignal>,
    /// When the current offer was minted/last resent.
    offered_at: Instant,
    /// Bounded resends of an unanswered offer (see apply_stunar_accepts).
    offer_resends: u8,
    fence: EpochFence,
    offer_fence: OfferEpochFence,
    link_id: LinkId,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct MediaOffer {
    #[serde(flatten)]
    pub signal: PeerSignal,
    /// Opaque serialized OfferEpochFence. The WebView echoes this value; it
    /// must never derive a new fence from a later snapshot.
    pub offer_attempt: String,
}

impl MediaOffer {
    fn from_exact(offer: ExactOffer) -> Result<Self, String> {
        Ok(Self {
            signal: offer.signal,
            offer_attempt: serde_json::to_string(&offer.fence)
                .map_err(|error| error.to_string())?,
        })
    }
}

struct MintedOffer {
    signal: PeerSignal,
    fence: OfferEpochFence,
}

struct SessionRecord {
    id: String,
    generation: u64,
    session_epoch: SessionEpoch,
    share_epoch: ShareEpoch,
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
    #[cfg(test)]
    resource_lease: Option<ResourceLease>,
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct ResourceCounters {
    live_bundles: AtomicUsize,
    live_capture: AtomicUsize,
    live_audio: AtomicUsize,
    live_pipeline: AtomicUsize,
    live_fanout: AtomicUsize,
    live_peers: AtomicUsize,
    live_join_services: AtomicUsize,
    fail_cleanup: AtomicBool,
}

#[cfg(test)]
impl ResourceCounters {
    pub(crate) fn live_bundles(&self) -> usize {
        self.live_bundles.load(Ordering::Acquire)
    }

    pub(crate) fn live_capture(&self) -> usize {
        self.live_capture.load(Ordering::Acquire)
    }

    pub(crate) fn live_audio(&self) -> usize {
        self.live_audio.load(Ordering::Acquire)
    }

    pub(crate) fn live_pipeline(&self) -> usize {
        self.live_pipeline.load(Ordering::Acquire)
    }

    pub(crate) fn live_fanout(&self) -> usize {
        self.live_fanout.load(Ordering::Acquire)
    }

    pub(crate) fn live_peers(&self) -> usize {
        self.live_peers.load(Ordering::Acquire)
    }

    pub(crate) fn live_join_services(&self) -> usize {
        self.live_join_services.load(Ordering::Acquire)
    }

    pub(crate) fn fail_cleanup(&self, fail: bool) {
        self.fail_cleanup.store(fail, Ordering::Release);
    }
}

#[cfg(test)]
struct ResourceLease {
    counters: Arc<ResourceCounters>,
    capture: usize,
    audio: usize,
    pipeline: usize,
    fanout: usize,
    peers: usize,
    join_services: usize,
}

#[cfg(test)]
impl ResourceLease {
    fn new(
        counters: Arc<ResourceCounters>,
        capture: usize,
        audio: usize,
        pipeline: usize,
        fanout: usize,
        peers: usize,
        join_services: usize,
    ) -> Self {
        counters.live_bundles.fetch_add(1, Ordering::AcqRel);
        counters.live_capture.fetch_add(capture, Ordering::AcqRel);
        counters.live_audio.fetch_add(audio, Ordering::AcqRel);
        counters.live_pipeline.fetch_add(pipeline, Ordering::AcqRel);
        counters.live_fanout.fetch_add(fanout, Ordering::AcqRel);
        counters.live_peers.fetch_add(peers, Ordering::AcqRel);
        counters
            .live_join_services
            .fetch_add(join_services, Ordering::AcqRel);
        Self {
            counters,
            capture,
            audio,
            pipeline,
            fanout,
            peers,
            join_services,
        }
    }
}

#[cfg(test)]
impl Drop for ResourceLease {
    fn drop(&mut self) {
        self.counters.live_bundles.fetch_sub(1, Ordering::AcqRel);
        self.counters
            .live_capture
            .fetch_sub(self.capture, Ordering::AcqRel);
        self.counters
            .live_audio
            .fetch_sub(self.audio, Ordering::AcqRel);
        self.counters
            .live_pipeline
            .fetch_sub(self.pipeline, Ordering::AcqRel);
        self.counters
            .live_fanout
            .fetch_sub(self.fanout, Ordering::AcqRel);
        self.counters
            .live_peers
            .fetch_sub(self.peers, Ordering::AcqRel);
        self.counters
            .live_join_services
            .fetch_sub(self.join_services, Ordering::AcqRel);
    }
}

struct CleanupBundle {
    session: Option<SessionRecord>,
    error: Option<MediaEngineError>,
}

struct UpdatePlan {
    next: CreateMediaSessionRequest,
    restart_share: bool,
}

enum TransactionCompletion {
    StartSession {
        fence: OperationFence,
        result: Result<SessionRecord, MediaEngineError>,
        detail: String,
        response: SyncSender<Result<MediaSessionSnapshot, MediaEngineError>>,
    },
    StartShare {
        fence: OperationFence,
        session: Option<SessionRecord>,
        error: Option<MediaEngineError>,
        detail: String,
        response: SyncSender<Result<MediaSessionSnapshot, MediaEngineError>>,
    },
    StopShare {
        fence: OperationFence,
        session: Option<SessionRecord>,
        error: Option<MediaEngineError>,
        response: SyncSender<Result<MediaSessionSnapshot, MediaEngineError>>,
    },
    UpdateSession {
        fence: OperationFence,
        session: Option<SessionRecord>,
        error: Option<MediaEngineError>,
        detail: String,
        response: SyncSender<Result<MediaSessionSnapshot, MediaEngineError>>,
    },
    StopSession {
        fence: OperationFence,
        cleanup: CleanupBundle,
        response: SyncSender<Result<MediaSessionSnapshot, MediaEngineError>>,
    },
    StaleCleanup {
        fence: OperationFence,
        cleanup: CleanupBundle,
        waiters: Vec<SyncSender<Result<MediaSessionSnapshot, MediaEngineError>>>,
    },
}

struct EngineState {
    capabilities: MediaCapabilities,
    actor: SessionActor,
    session: Option<SessionRecord>,
    next_session_id: u64,
    detail: String,
    preview: Arc<PreviewState>,
    // Viewer-side Stunar WS, kept alive between ask and answer.
    stunar_viewer: Option<StunarViewer>,
    // Ingress back into the serialized actor. Tests use the direct fallback
    // because they invoke the state reducer without starting a worker.
    control_tx: Option<SyncSender<MediaCommand>>,
    pending_start: Option<Arc<AtomicBool>>,
    pending_share: Option<Arc<AtomicBool>>,
    pending_start_operation: Option<OperationFence>,
    pending_share_operation: Option<OperationFence>,
    pending_stop_operation: Option<OperationFence>,
    transition_snapshot: Option<MediaSessionSnapshot>,
    #[cfg(test)]
    operation_barrier: Option<Arc<std::sync::Barrier>>,
    #[cfg(test)]
    picker_barrier: Option<Arc<std::sync::Barrier>>,
    #[cfg(test)]
    resource_counters: Option<Arc<ResourceCounters>>,
    stop_waiters: Vec<SyncSender<Result<MediaSessionSnapshot, MediaEngineError>>>,
    pending_peer_cleanup: Vec<PendingPeerTransport>,
}

enum MediaCommand {
    Create {
        request: CreateMediaSessionRequest,
        response: SyncSender<Result<MediaSessionSnapshot, MediaEngineError>>,
    },
    Update {
        request: UpdateMediaSessionRequest,
        source_id_update: SourceIdUpdate,
        response: SyncSender<Result<MediaSessionSnapshot, MediaEngineError>>,
    },
    Stop {
        response: SyncSender<Result<MediaSessionSnapshot, MediaEngineError>>,
    },
    StartShare {
        response: SyncSender<Result<MediaSessionSnapshot, MediaEngineError>>,
    },
    StopShare {
        response: SyncSender<Result<MediaSessionSnapshot, MediaEngineError>>,
    },
    MintOffer {
        id: String,
        nickname: String,
        origin: Option<EpochFence>,
        response: SyncSender<Result<MediaOffer, String>>,
    },
    Kick {
        id: String,
        response: SyncSender<Result<MediaSessionSnapshot, MediaEngineError>>,
    },
    Admit {
        id: String,
        response: SyncSender<Result<MediaSessionSnapshot, MediaEngineError>>,
    },
    Reject {
        id: String,
        response: SyncSender<Result<MediaSessionSnapshot, MediaEngineError>>,
    },
    Credentials {
        request: UpdateCredentialsRequest,
        response: SyncSender<Result<MediaSessionSnapshot, MediaEngineError>>,
    },
    SetAnswer {
        answer: PeerSignal,
        offer_attempt: String,
        response: SyncSender<Result<(), MediaEngineError>>,
    },
    ClosePeers {
        response: SyncSender<Result<(), MediaEngineError>>,
    },
    AttachStunarViewer {
        viewer: StunarViewer,
        response: SyncSender<Result<(), MediaEngineError>>,
    },
    CloseStunarViewer {
        response: SyncSender<()>,
    },
    TransactionCompleted {
        completion: TransactionCompletion,
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

#[derive(Clone, Debug, Eq, PartialEq)]
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
            actor: SessionActor::new(),
            session: None,
            next_session_id: 1,
            preview: Arc::new(PreviewState::new()),
            stunar_viewer: None,
            control_tx: None,
            pending_start: None,
            pending_share: None,
            pending_start_operation: None,
            pending_share_operation: None,
            pending_stop_operation: None,
            transition_snapshot: None,
            #[cfg(test)]
            operation_barrier: None,
            #[cfg(test)]
            picker_barrier: None,
            #[cfg(test)]
            resource_counters: None,
            stop_waiters: Vec::new(),
            pending_peer_cleanup: Vec::new(),
        }));
        let (control_tx, control_rx) = sync_channel(CONTROL_QUEUE_CAPACITY);
        if let Ok(mut guard) = state.lock() {
            guard.control_tx = Some(control_tx.clone());
        }
        let worker_state = Arc::clone(&state);
        let worker_control_tx = control_tx.clone();
        thread::Builder::new()
            .name("godrinking-media-control".into())
            .spawn(move || worker_loop(control_rx, worker_state, worker_control_tx))
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
            // capabilities as granted. The process-loopback probe runs here
            // (command thread, cached after the first call) instead of the
            // constructor so no COM work ever blocks the main thread.
            let _ = app;
            let supported = super::process_tap::is_process_loopback_supported();
            if let Ok(mut state) = self.state.lock() {
                state.capabilities.process_loopback = supported;
            }
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
        // Snapshot is intentionally observational. Signaling and lifecycle
        // ingress is drained by the serialized worker, never by UI polling.
        self.state
            .lock()
            .map(|state| snapshot_from_state(&state))
            .unwrap_or_else(|_| MediaSessionSnapshot::idle("Native media state is unavailable."))
    }

    /// Stunar with Admission off accepts Viewers immediately on the
    /// Rendezvous, so there is no pending step to trigger the mint. Drained
    /// by the serialized worker: any accepted Viewer without a ViewerLink
    /// gets an offer minted and sent over the WS inbox.
    ///
    /// This also heals two join-path failures without user action:
    /// - Links whose id vanished from the Rendezvous roster (leave/kick/
    ///   timeout) while never connecting are dropped, so a rejoin mints a
    ///   fresh peer instead of hitting the stale entry (and the 8-viewer cap
    ///   is not eaten by ghosts).
    /// - An offer lost on the WS (minted + stored, answer never arrives,
    ///   peer stuck in New) is resent a bounded number of times.
    fn apply_stunar_accepts(&self) {
        const OFFER_RESEND_AFTER: Duration = Duration::from_secs(5);
        const MAX_OFFER_RESENDS: u8 = 6;
        let (to_mint, to_resend, dropped, stunar, origin) = {
            let mut guard = match self.state.lock() {
                Ok(guard) => guard,
                Err(_) => return,
            };
            let origin = guard.actor.fence(None);
            let Some(session) = guard.session.as_mut() else {
                return;
            };
            let Some(stunar) = session.stunar.clone() else {
                return;
            };
            let accepted = stunar.accepted_roster();
            let room_ids: HashSet<String> = stunar
                .room_roster()
                .into_iter()
                .map(|(id, _, _, _)| id)
                .collect();
            let self_id = stunar.self_id.clone();
            let accepted_ids: HashSet<&str> = accepted.iter().map(|(id, _)| id.as_str()).collect();
            let mut to_mint = Vec::new();
            if super::room_mode::auto_mint_on_accept(session.request.session_mode) {
                for (id, nickname) in &accepted {
                    if self_id.as_deref() == Some(id.as_str()) {
                        continue;
                    }
                    if !session.viewers.contains_key(id) {
                        to_mint.push((id.clone(), nickname.clone()));
                    }
                }
            }
            let mut to_resend: Vec<(String, PeerSignal, OfferEpochFence)> = Vec::new();
            let mut dropped_ids = Vec::new();
            let now = Instant::now();
            let roster_known = !room_ids.is_empty() || !accepted_ids.is_empty();
            for (id, link) in session.viewers.iter_mut() {
                if link.peer.status().state == PeerTransportState::Connected {
                    continue;
                }
                let in_roster = accepted_ids.contains(id.as_str()) || room_ids.contains(id);
                if super::room_mode::drop_unanswered_link(
                    session.request.session_mode,
                    roster_known,
                    in_roster,
                ) {
                    dropped_ids.push(id.clone());
                } else if link.peer.status().state == PeerTransportState::New {
                    if let Some(signal) = link.last_offer.clone() {
                        if link.offer_resends < MAX_OFFER_RESENDS
                            && now.duration_since(link.offered_at) >= OFFER_RESEND_AFTER
                        {
                            link.offered_at = now;
                            link.offer_resends += 1;
                            to_resend.push((id.clone(), signal, link.offer_fence));
                        }
                    }
                }
            }
            let mut dropped = Vec::new();
            let mut retired = Vec::new();
            for id in dropped_ids {
                if let Some(fanout) = session.fanout.as_ref() {
                    fanout.unsubscribe(&id);
                }
                if let Some(link) = session.viewers.remove(&id) {
                    retired.push(link.link_id);
                    logger::log("INFO", "roster", &format!("dropping stale viewer={id}"));
                    dropped.push(link);
                }
            }
            for link_id in retired {
                guard.actor.retire_link(link_id);
            }
            (to_mint, to_resend, dropped, stunar, origin)
        };
        // Peer close can block (teardown deadline); never hold the engine
        // lock for it.
        drop(dropped);
        for (id, nickname) in to_mint {
            let Ok(minted) = mint_viewer_offer_fenced(&self.state, &id, &nickname, Some(origin))
            else {
                continue;
            };
            stunar.remember_exact_offer_fence(&id, minted.fence);
            if let Err(error) =
                stunar.send_signal_with_offer_fence(&id, &minted.signal, minted.fence)
            {
                logger::log(
                    "WARN",
                    "mint offer",
                    &format!("send failed viewer={id}: {error}"),
                );
            }
        }
        for (id, signal, fence) in to_resend {
            logger::log(
                "INFO",
                "mint offer",
                &format!("resending unanswered offer viewer={id}"),
            );
            if let Err(error) = stunar.send_signal_with_offer_fence(&id, &signal, fence) {
                logger::log(
                    "WARN",
                    "mint offer",
                    &format!("resend failed viewer={id}: {error}"),
                );
            }
        }
    }

    /// Sala: mint an offer only when a member asked to watch our share.
    fn apply_stunar_watches(&self) {
        let (to_mint, dropped, stunar_host, send_via_viewer, origin) = {
            let mut guard = match self.state.lock() {
                Ok(guard) => guard,
                Err(_) => return,
            };
            let origin = guard.actor.fence(None);
            let mut watch = Vec::new();
            let mut unwatch = Vec::new();
            let capturing = guard
                .session
                .as_ref()
                .map(|session| session.native_capture_active)
                .unwrap_or(false);
            if let Some(session) = guard.session.as_ref() {
                if let Some(host) = session.stunar.as_ref() {
                    let (w, u) = host.take_watch_requests(capturing);
                    watch.extend(w);
                    unwatch.extend(u);
                }
            }
            let viewer_member_id = guard
                .stunar_viewer
                .as_ref()
                .and_then(|viewer| viewer.member_id.clone());
            let viewer_nicks: HashMap<String, String> = guard
                .stunar_viewer
                .as_ref()
                .map(|viewer| {
                    viewer
                        .room_roster()
                        .into_iter()
                        .map(|(id, nickname, _, _)| (id, nickname))
                        .collect()
                })
                .unwrap_or_default();
            if let Some(viewer) = guard.stunar_viewer.as_ref() {
                let (w, u) = viewer.take_watch_requests(capturing);
                watch.extend(w);
                unwatch.extend(u);
            }
            let Some(session) = guard.session.as_mut() else {
                return;
            };
            let self_id = session
                .stunar
                .as_ref()
                .and_then(|host| host.self_id.clone())
                .or(viewer_member_id);
            let mut dropped = Vec::new();
            let mut retired = Vec::new();
            for id in unwatch {
                if let Some(fanout) = session.fanout.as_ref() {
                    fanout.unsubscribe(&id);
                }
                if let Some(link) = session.viewers.remove(&id) {
                    retired.push(link.link_id);
                    dropped.push(link);
                }
            }
            let mut to_mint = Vec::new();
            if capturing {
                for id in watch {
                    if self_id.as_deref() == Some(id.as_str()) {
                        continue;
                    }
                    if session.viewers.contains_key(&id) {
                        continue;
                    }
                    let nickname = session
                        .stunar
                        .as_ref()
                        .and_then(|host| host.nickname_of(&id))
                        .or_else(|| viewer_nicks.get(&id).cloned())
                        .unwrap_or_else(|| id.clone());
                    to_mint.push((id, nickname));
                }
            }
            let stunar_host = session.stunar.clone();
            let send_via_viewer = session.stunar.is_none();
            for link_id in retired {
                guard.actor.retire_link(link_id);
            }
            (to_mint, dropped, stunar_host, send_via_viewer, origin)
        };
        drop(dropped);
        for (id, nickname) in to_mint {
            let Ok(minted) = mint_viewer_offer_fenced(&self.state, &id, &nickname, Some(origin))
            else {
                continue;
            };
            if let Some(host) = stunar_host.as_ref() {
                host.remember_exact_offer_fence(&id, minted.fence);
                let _ = host.send_signal_with_offer_fence(&id, &minted.signal, minted.fence);
            } else if send_via_viewer {
                if let Ok(state) = self.state.lock() {
                    if let Some(viewer) = state.stunar_viewer.as_ref() {
                        viewer.remember_offer_fence(&id, minted.fence.epoch);
                        let _ =
                            viewer.send_signal_with_offer_fence(&id, &minted.signal, minted.fence);
                    }
                }
            }
        }
    }

    fn apply_room_answer(&self) {
        let answers = {
            let Ok(state) = self.state.lock() else {
                return;
            };
            let mut answers = Vec::new();
            if let Some(session) = state.session.as_ref() {
                if let Some(room) = session.room.as_ref() {
                    answers.extend(
                        room.take_exact_answers()
                            .into_iter()
                            .map(|answer| (answer.signal, answer.fence)),
                    );
                }
                if let Some(room) = session.direct_room.as_ref() {
                    answers.extend(
                        room.take_exact_answers()
                            .into_iter()
                            .map(|answer| (answer.signal, answer.fence)),
                    );
                }
                if let Some(stunar) = session.stunar.as_ref() {
                    answers.extend(stunar.take_exact_answers());
                }
            }
            if let Some(viewer) = state.stunar_viewer.as_ref() {
                let _ = viewer.take_answers();
            }
            answers
        };
        if answers.is_empty() {
            return;
        }
        logger::log(
            "INFO",
            "room answer",
            &format!("received answers={}", answers.len()),
        );
        for (signal, fence) in answers {
            let answer_id = signal.id.clone().unwrap_or_default();
            let pending = self.state.lock().ok().and_then(|state| {
                if !state.actor.accepts_offer(fence) {
                    return None;
                }
                let session = state.session.as_ref()?;
                let id = signal.id.as_deref()?;
                let viewer = session.viewers.get(id)?;
                if viewer.offer_fence != fence {
                    return None;
                }
                Some((
                    viewer.peer.client(),
                    Arc::clone(&session._pipeline.encoder_control),
                ))
            });
            match pending {
                Some((client, control)) => {
                    logger::log(
                        "INFO",
                        "room answer",
                        &format!("applied answer id={answer_id}"),
                    );
                    let _ = client.set_answer(signal);
                    // Fresh IDR as the peer connects: the pump starts streaming
                    // on the first keyframe, so this bounds viewer join latency
                    // to one encode interval even on static screens.
                    control.request_keyframe();
                }
                None => {
                    logger::log(
                        "WARN",
                        "room answer",
                        &format!("dropped answer id={answer_id} (no matching viewer)"),
                    );
                }
            }
        }
    }

    pub fn kick_viewer(&self, id: &str) -> Result<MediaSessionSnapshot, MediaEngineError> {
        let (response_tx, response_rx) = sync_channel(1);
        self.control_tx
            .send(MediaCommand::Kick {
                id: id.to_owned(),
                response: response_tx,
            })
            .map_err(|_| MediaEngineError::QueueClosed)?;
        response_rx
            .recv()
            .map_err(|_| MediaEngineError::QueueClosed)?
    }

    fn kick_viewer_in_state(&self, id: &str) -> Result<MediaSessionSnapshot, MediaEngineError> {
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
        let dropped = if let Some(viewer) = session.viewers.remove(id) {
            if let Some(fanout) = session.fanout.as_ref() {
                fanout.unsubscribe(id);
            }
            Some(viewer)
        } else {
            None
        };
        if let Some(viewer) = dropped.as_ref() {
            state.actor.retire_link(viewer.link_id);
        }
        drop(state);
        // Peer close can block on the teardown deadline; never hold the
        // engine lock for it.
        drop(dropped);
        let state = self
            .state
            .lock()
            .map_err(|_| MediaEngineError::StatePoisoned)?;
        Ok(snapshot_from_state(&state))
    }

    /// Accepts a Pending Viewer. LAN/Direct wake the room's TCP thread via
    /// the SessionGate; Stunar tells the Rendezvous, then mints the offer and
    /// sends it over the WS inbox.
    pub fn admit_viewer(&self, id: &str) -> Result<MediaSessionSnapshot, MediaEngineError> {
        let (response_tx, response_rx) = sync_channel(1);
        self.control_tx
            .send(MediaCommand::Admit {
                id: id.to_owned(),
                response: response_tx,
            })
            .map_err(|_| MediaEngineError::QueueClosed)?;
        response_rx
            .recv()
            .map_err(|_| MediaEngineError::QueueClosed)?
    }

    fn admit_viewer_in_state(&self, id: &str) -> Result<MediaSessionSnapshot, MediaEngineError> {
        let stunar_path = {
            let state = self
                .state
                .lock()
                .map_err(|_| MediaEngineError::StatePoisoned)?;
            let origin = state.actor.fence(None);
            let session = state
                .session
                .as_ref()
                .ok_or(MediaEngineError::NoActiveSession)?;
            session.stunar.as_ref().map(|stunar| {
                let nickname = stunar
                    .pending_nickname(id)
                    .unwrap_or_else(|| "Viewer".to_owned());
                (stunar.clone(), nickname, origin)
            })
        };
        if let Some((stunar, nickname, origin)) = stunar_path {
            stunar
                .decide(id, true)
                .map_err(MediaEngineError::NativePeer)?;
            let minted = mint_viewer_offer_fenced(&self.state, id, &nickname, Some(origin))
                .map_err(MediaEngineError::NativePeer)?;
            stunar.remember_exact_offer_fence(id, minted.fence);
            stunar
                .send_signal_with_offer_fence(id, &minted.signal, minted.fence)
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
        let (response_tx, response_rx) = sync_channel(1);
        self.control_tx
            .send(MediaCommand::Reject {
                id: id.to_owned(),
                response: response_tx,
            })
            .map_err(|_| MediaEngineError::QueueClosed)?;
        response_rx
            .recv()
            .map_err(|_| MediaEngineError::QueueClosed)?
    }

    fn reject_viewer_in_state(&self, id: &str) -> Result<MediaSessionSnapshot, MediaEngineError> {
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
        let (response_tx, response_rx) = sync_channel(1);
        self.control_tx
            .send(MediaCommand::Credentials {
                request,
                response: response_tx,
            })
            .map_err(|_| MediaEngineError::QueueClosed)?;
        response_rx
            .recv()
            .map_err(|_| MediaEngineError::QueueClosed)?
    }

    /// Session-wide encoder diagnostics + per-viewer link diagnostics
    /// (state + RTT in ms) for the Host status popup. Peer status is read
    /// under the lock; RTT queries run after it is released so snapshot
    /// polling never blocks on stats collection.
    pub fn viewer_link_stats(&self) -> MediaSessionStats {
        let collected: (
            Vec<(String, String, PeerTransportState, PeerTransportClient)>,
            u32,
            Option<u32>,
            u32,
        ) = self
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
        let source_id_update = request
            .source_id
            .map(SourceIdUpdate::Set)
            .unwrap_or(SourceIdUpdate::Unchanged);
        self.update_session_with_source_id(request, source_id_update)
    }

    pub fn update_session_with_source_id(
        &self,
        request: UpdateMediaSessionRequest,
        source_id_update: SourceIdUpdate,
    ) -> Result<MediaSessionSnapshot, MediaEngineError> {
        let (response_tx, response_rx) = sync_channel(1);
        self.control_tx
            .send(MediaCommand::Update {
                request,
                source_id_update,
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

    pub fn set_peer_answer(
        &self,
        answer: PeerSignal,
        offer_attempt: String,
    ) -> Result<(), MediaEngineError> {
        let (response_tx, response_rx) = sync_channel(1);
        self.control_tx
            .send(MediaCommand::SetAnswer {
                answer,
                offer_attempt,
                response: response_tx,
            })
            .map_err(|_| MediaEngineError::QueueClosed)?;
        response_rx
            .recv()
            .map_err(|_| MediaEngineError::QueueClosed)?
    }

    fn set_peer_answer_in_state(
        &self,
        answer: PeerSignal,
        offer_attempt: String,
    ) -> Result<(), MediaEngineError> {
        let offer_fence: OfferEpochFence = serde_json::from_str(&offer_attempt)
            .map_err(|_| MediaEngineError::NativePeer("invalid offer attempt".into()))?;
        let id = answer
            .id
            .as_deref()
            .ok_or_else(|| MediaEngineError::NativePeer("answer is missing viewer id".into()))?
            .to_owned();
        let stale = {
            let state = self
                .state
                .lock()
                .map_err(|_| MediaEngineError::StatePoisoned)?;
            let session = state
                .session
                .as_ref()
                .ok_or(MediaEngineError::NoActiveSession)?;
            let viewer = session
                .viewers
                .get(&id)
                .ok_or_else(|| MediaEngineError::NativePeer("unknown viewer".into()))?;
            viewer.offer_fence != offer_fence || !state.actor.accepts_offer(offer_fence)
        };
        if stale {
            logger::log(
                "WARN",
                "answer",
                &format!("discarded stale viewer answer viewer={id}"),
            );
            return Err(MediaEngineError::NativePeer("stale viewer answer".into()));
        }
        let client = {
            let state = self
                .state
                .lock()
                .map_err(|_| MediaEngineError::StatePoisoned)?;
            state
                .session
                .as_ref()
                .and_then(|session| session.viewers.get(&id))
                .map(|viewer| viewer.peer.client())
                .ok_or_else(|| MediaEngineError::NativePeer("unknown viewer".into()))?
        };
        client
            .set_answer(answer)
            .map_err(MediaEngineError::NativePeer)
    }

    pub fn close_peer_transport(&self) -> Result<(), MediaEngineError> {
        let (response_tx, response_rx) = sync_channel(1);
        self.control_tx
            .send(MediaCommand::ClosePeers {
                response: response_tx,
            })
            .map_err(|_| MediaEngineError::QueueClosed)?;
        response_rx
            .recv()
            .map_err(|_| MediaEngineError::QueueClosed)?
    }

    fn close_peer_transport_in_state(&self) -> Result<(), MediaEngineError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| MediaEngineError::StatePoisoned)?;
        let session = state
            .session
            .as_mut()
            .ok_or(MediaEngineError::NoActiveSession)?;
        let ids: Vec<String> = session.viewers.keys().cloned().collect();
        let mut dropped = Vec::new();
        let mut retired = Vec::new();
        for id in ids {
            if let Some(fanout) = session.fanout.as_ref() {
                fanout.unsubscribe(&id);
            }
            if let Some(link) = session.viewers.remove(&id) {
                retired.push(link.link_id);
                dropped.push(link);
            }
        }
        for link_id in retired {
            state.actor.retire_link(link_id);
        }
        drop(state);
        drop(dropped);
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
    ) -> Result<(String, MediaOffer), MediaEngineError> {
        let (token, offer, viewer) =
            super::rendezvous::discover_stunar_room(base, code, password, nickname)
                .map_err(MediaEngineError::NativePeer)?;
        let offer_attempt = viewer.initial_offer_fence();
        let (response_tx, response_rx) = sync_channel(1);
        if self
            .control_tx
            .send(MediaCommand::AttachStunarViewer {
                viewer,
                response: response_tx,
            })
            .is_err()
        {
            return Err(MediaEngineError::QueueClosed);
        }
        response_rx
            .recv()
            .map_err(|_| MediaEngineError::QueueClosed)??;
        let offer = MediaOffer {
            signal: offer,
            offer_attempt: offer_attempt
                .map(|fence| serde_json::to_string(&fence).unwrap_or_default())
                .unwrap_or_default(),
        };
        Ok((token, offer))
    }

    pub fn discover_lan(
        &self,
        code: &str,
        password: &str,
        nickname: &str,
    ) -> Result<(String, MediaOffer, String), MediaEngineError> {
        let (host, offer, host_nickname) =
            super::room::discover_room_exact(code, password, nickname)
                .map_err(MediaEngineError::NativePeer)?;
        Ok((
            host.to_string(),
            MediaOffer::from_exact(offer).map_err(MediaEngineError::NativePeer)?,
            host_nickname,
        ))
    }

    pub fn discover_direct(
        &self,
        host: std::net::SocketAddr,
        password: &str,
        nickname: &str,
    ) -> Result<(String, MediaOffer, String), MediaEngineError> {
        let (offer, host_nickname) = super::room::discover_direct_exact(host, password, nickname)
            .map_err(MediaEngineError::NativePeer)?;
        Ok((
            host.to_string(),
            MediaOffer::from_exact(offer).map_err(MediaEngineError::NativePeer)?,
            host_nickname,
        ))
    }

    pub fn submit_room_answer(
        &self,
        host: std::net::SocketAddr,
        answer: PeerSignal,
        offer_attempt: String,
    ) -> Result<(), MediaEngineError> {
        let fence: OfferEpochFence = serde_json::from_str(&offer_attempt)
            .map_err(|_| MediaEngineError::NativePeer("invalid offer attempt".into()))?;
        super::room::submit_answer_exact(host, &answer, fence).map_err(MediaEngineError::NativePeer)
    }

    /// Viewer-side Stunar: sends the answer signal over the stored WS.
    pub fn submit_stunar_answer(
        &self,
        answer: PeerSignal,
        offer_attempt: String,
    ) -> Result<(), MediaEngineError> {
        let state = self
            .state
            .lock()
            .map_err(|_| MediaEngineError::StatePoisoned)?;
        let viewer = state
            .stunar_viewer
            .as_ref()
            .ok_or_else(|| MediaEngineError::NativePeer("no stunar session".into()))?;
        let fence: OfferEpochFence = serde_json::from_str(&offer_attempt)
            .map_err(|_| MediaEngineError::NativePeer("invalid offer attempt".into()))?;
        super::rendezvous::submit_stunar_answer_exact(viewer, &answer, fence)
            .map_err(MediaEngineError::NativePeer)
    }

    pub fn poll_incoming_offers(&self) -> Vec<super::rendezvous::StunarIncomingOffer> {
        let Ok(state) = self.state.lock() else {
            return Vec::new();
        };
        let mut offers = Vec::new();
        if let Some(host) = state
            .session
            .as_ref()
            .and_then(|session| session.stunar.as_ref())
        {
            offers.extend(host.take_incoming_offers());
        }
        if let Some(viewer) = state.stunar_viewer.as_ref() {
            offers.extend(viewer.take_incoming_offers());
        }
        offers
    }

    pub fn send_stunar_signal(&self, to: &str, signal: PeerSignal) -> Result<(), MediaEngineError> {
        let state = self
            .state
            .lock()
            .map_err(|_| MediaEngineError::StatePoisoned)?;
        if let Some(host) = state
            .session
            .as_ref()
            .and_then(|session| session.stunar.as_ref())
        {
            return host
                .send_signal(to, &signal)
                .map_err(MediaEngineError::NativePeer);
        }
        if let Some(viewer) = state.stunar_viewer.as_ref() {
            return viewer
                .send_signal(to, &signal)
                .map_err(MediaEngineError::NativePeer);
        }
        Err(MediaEngineError::NativePeer("no stunar session".into()))
    }

    pub fn send_stunar_signal_with_attempt(
        &self,
        to: &str,
        signal: PeerSignal,
        offer_attempt: String,
    ) -> Result<(), MediaEngineError> {
        let fence: OfferEpochFence = serde_json::from_str(&offer_attempt)
            .map_err(|_| MediaEngineError::NativePeer("invalid offer attempt".into()))?;
        let state = self
            .state
            .lock()
            .map_err(|_| MediaEngineError::StatePoisoned)?;
        if let Some(host) = state
            .session
            .as_ref()
            .and_then(|session| session.stunar.as_ref())
        {
            return host
                .send_signal_with_offer_fence(to, &signal, fence)
                .map_err(MediaEngineError::NativePeer);
        }
        if let Some(viewer) = state.stunar_viewer.as_ref() {
            return viewer
                .send_signal_with_offer_fence(to, &signal, fence)
                .map_err(MediaEngineError::NativePeer);
        }
        Err(MediaEngineError::NativePeer("no stunar session".into()))
    }

    pub fn offer_for_member(
        &self,
        id: &str,
        nickname: &str,
    ) -> Result<MediaOffer, MediaEngineError> {
        let (response_tx, response_rx) = sync_channel(1);
        self.control_tx
            .send(MediaCommand::MintOffer {
                id: id.to_owned(),
                nickname: nickname.to_owned(),
                origin: None,
                response: response_tx,
            })
            .map_err(|_| MediaEngineError::QueueClosed)?;
        response_rx
            .recv()
            .map_err(|_| MediaEngineError::QueueClosed)?
            .map_err(MediaEngineError::NativePeer)
            .and_then(|offer| Ok(offer))
    }

    pub fn announce_share(&self, start: bool) -> Result<(), MediaEngineError> {
        let state = self
            .state
            .lock()
            .map_err(|_| MediaEngineError::StatePoisoned)?;
        if let Some(host) = state
            .session
            .as_ref()
            .and_then(|session| session.stunar.as_ref())
        {
            let _ = host.send_share(start);
        }
        if let Some(viewer) = state.stunar_viewer.as_ref() {
            let _ = viewer.send_share(start);
        }
        Ok(())
    }

    pub fn request_watch(&self, to: &str, start: bool) -> Result<(), MediaEngineError> {
        let state = self
            .state
            .lock()
            .map_err(|_| MediaEngineError::StatePoisoned)?;
        if let Some(host) = state
            .session
            .as_ref()
            .and_then(|session| session.stunar.as_ref())
        {
            return host
                .send_watch(to, start)
                .map_err(MediaEngineError::NativePeer);
        }
        if let Some(viewer) = state.stunar_viewer.as_ref() {
            return viewer
                .send_watch(to, start)
                .map_err(MediaEngineError::NativePeer);
        }
        Err(MediaEngineError::NativePeer("no stunar session".into()))
    }

    pub fn start_share(&self) -> Result<MediaSessionSnapshot, MediaEngineError> {
        let (response_tx, response_rx) = sync_channel(1);
        self.control_tx
            .send(MediaCommand::StartShare {
                response: response_tx,
            })
            .map_err(|_| MediaEngineError::QueueClosed)?;
        response_rx
            .recv()
            .map_err(|_| MediaEngineError::QueueClosed)?
    }

    pub fn stop_share(&self) -> Result<MediaSessionSnapshot, MediaEngineError> {
        let (response_tx, response_rx) = sync_channel(1);
        self.control_tx
            .send(MediaCommand::StopShare {
                response: response_tx,
            })
            .map_err(|_| MediaEngineError::QueueClosed)?;
        response_rx
            .recv()
            .map_err(|_| MediaEngineError::QueueClosed)?
    }

    /// Viewer-side Stunar: explicit leave (roster drops now) + WS close.
    pub fn close_stunar_viewer(&self) {
        let (response_tx, response_rx) = sync_channel(1);
        if self
            .control_tx
            .send(MediaCommand::CloseStunarViewer {
                response: response_tx,
            })
            .is_ok()
        {
            let _ = response_rx.recv();
        }
    }
}

impl Default for MediaEngine {
    fn default() -> Self {
        Self::new()
    }
}

fn validate_start_config(
    state: &Arc<Mutex<EngineState>>,
    request: &CreateMediaSessionRequest,
) -> Result<(), MediaEngineError> {
    let supported = state
        .lock()
        .map_err(|_| MediaEngineError::StatePoisoned)?
        .capabilities
        .supported;
    if !supported {
        return Err(MediaEngineError::UnsupportedPlatform);
    }
    validate_canonical_config(request)?;
    if !request.attach_only && request.join_mode == JoinMode::Stunar {
        if request.rendezvous_url.is_none() {
            return Err(MediaEngineError::NativePeer(
                "Set the Stunar URL in settings.".into(),
            ));
        }
        if !valid_password(&request.password) {
            return Err(MediaEngineError::NativePeer(
                "Stunar requires a password (4-64 characters).".into(),
            ));
        }
    }
    Ok(())
}

/// Pure compatibility validator shared by Create, Share, and Update.
fn validate_canonical_config(request: &CreateMediaSessionRequest) -> Result<(), MediaEngineError> {
    if request.codec != VideoCodec::H264 {
        return Err(MediaEngineError::Unsupported(
            "goDrinking requires H.264 Constrained Baseline.".into(),
        ));
    }
    if request.effective_frame_rate().hertz() > 60 {
        return Err(MediaEngineError::Unsupported(
            "frame rates above 60 fps are not supported.".into(),
        ));
    }
    Ok(())
}

fn launch_start_session(
    state: &Arc<Mutex<EngineState>>,
    control_tx: &SyncSender<MediaCommand>,
    request: CreateMediaSessionRequest,
    response: SyncSender<Result<MediaSessionSnapshot, MediaEngineError>>,
) {
    if let Err(error) = validate_start_config(state, &request) {
        let _ = response.send(Err(error));
        return;
    }
    // Validation is complete before this platform side effect. The remaining
    // acquisition work runs in the transaction worker and returns a bundle.
    crate::media::firewall::ensure_firewall_for_host(request.join_mode);
    let cancel = Arc::new(AtomicBool::new(false));
    let (
        operation,
        session_epoch,
        share_epoch,
        id,
        capabilities,
        preview,
        operation_barrier,
        picker_barrier,
    ) = {
        let Ok(mut guard) = state.lock() else {
            let _ = response.send(Err(MediaEngineError::StatePoisoned));
            return;
        };
        if guard.session.is_some()
            || guard.pending_start.is_some()
            || guard.pending_share.is_some()
            || guard.actor.lifecycle != MediaLifecycleState::Idle
        {
            let _ = response.send(Err(MediaEngineError::SessionAlreadyActive));
            return;
        }
        let session_epoch = guard.actor.begin_session();
        if request.share_on_start {
            guard.actor.begin_share();
        }
        let operation_epoch = guard.actor.fence(None);
        let operation = guard
            .actor
            .reserve_operation_kind(operation_epoch, OperationKind::StartSession);
        let id = format!("native-{}", guard.next_session_id);
        guard.next_session_id = guard.next_session_id.saturating_add(1);
        let capabilities = guard.capabilities.clone();
        let share_epoch = guard.actor.share_epoch;
        guard.pending_start = Some(Arc::clone(&cancel));
        guard.pending_start_operation = Some(operation);
        (
            operation,
            session_epoch,
            share_epoch,
            id,
            capabilities,
            Arc::clone(&guard.preview),
            #[cfg(test)]
            guard.operation_barrier.clone(),
            #[cfg(not(test))]
            None,
            #[cfg(test)]
            guard.picker_barrier.clone(),
            #[cfg(not(test))]
            None,
        )
    };
    preview.begin_session();
    let worker_tx = control_tx.clone();
    let worker_state = Arc::clone(state);
    let failure_response = response.clone();
    let spawn = thread::Builder::new()
        .name("godrinking-session-start".into())
        .spawn(move || {
            let (result, detail) = match create_session_bundle(
                &worker_state,
                request,
                operation,
                session_epoch,
                share_epoch,
                id,
                capabilities,
                preview,
                cancel,
                operation_barrier,
                picker_barrier,
            ) {
                Ok((session, detail)) => (Ok(session), detail),
                Err(error) => (Err(error), String::new()),
            };
            let completion = MediaCommand::TransactionCompleted {
                completion: TransactionCompletion::StartSession {
                    fence: operation,
                    result,
                    detail,
                    response,
                },
            };
            if let Err(error) = worker_tx.send(completion) {
                if let MediaCommand::TransactionCompleted {
                    completion:
                        TransactionCompletion::StartSession {
                            result, response, ..
                        },
                } = error.0
                {
                    if let Ok(mut guard) = worker_state.lock() {
                        match result {
                            Ok(session) => {
                                guard.session = Some(session);
                                guard.actor.lifecycle = MediaLifecycleState::CleanupPending;
                                guard.detail =
                                    "Session start completion could not reach actor.".into();
                            }
                            Err(error) => {
                                guard.actor.lifecycle = MediaLifecycleState::Idle;
                                guard.detail = format!("Session start failed: {error}");
                            }
                        }
                    }
                    let _ = response.send(Err(MediaEngineError::QueueClosed));
                }
            }
        });
    if spawn.is_err() {
        if let Ok(mut guard) = state.lock() {
            guard.pending_start = None;
            guard.pending_start_operation = None;
            guard.actor.invalidate_session();
            guard.actor.lifecycle = MediaLifecycleState::Idle;
        }
        let _ = failure_response.send(Err(MediaEngineError::QueueClosed));
    }
}

fn launch_start_share(
    state: &Arc<Mutex<EngineState>>,
    control_tx: &SyncSender<MediaCommand>,
    response: SyncSender<Result<MediaSessionSnapshot, MediaEngineError>>,
) {
    let cancel = Arc::new(AtomicBool::new(false));
    let (operation, session, capabilities, preview) = {
        let Ok(mut guard) = state.lock() else {
            let _ = response.send(Err(MediaEngineError::StatePoisoned));
            return;
        };
        if guard.session.is_none() {
            let _ = response.send(Err(MediaEngineError::NoActiveSession));
            return;
        }
        if guard.pending_start.is_some() || guard.pending_share.is_some() {
            let _ = response.send(Err(MediaEngineError::SessionAlreadyActive));
            return;
        }
        let mut transition = snapshot_from_state(&guard);
        transition.state = MediaLifecycleState::Starting;
        transition.detail = "Share start is acquiring capture resources.".into();
        guard.transition_snapshot = Some(transition);
        let session = guard.session.take().expect("active session checked above");
        guard.actor.lifecycle = MediaLifecycleState::Starting;
        let (_, operation) = guard.actor.reserve_start_share();
        guard.pending_share = Some(Arc::clone(&cancel));
        guard.pending_share_operation = Some(operation);
        (
            operation,
            session,
            guard.capabilities.clone(),
            Arc::clone(&guard.preview),
        )
    };
    let worker_tx = control_tx.clone();
    let worker_state = Arc::clone(state);
    let failure_response = response.clone();
    let bundle = Arc::new(Mutex::new(Some(session)));
    let worker_bundle = Arc::clone(&bundle);
    let spawn = thread::Builder::new()
        .name("godrinking-share-start".into())
        .spawn(move || {
            let Some(session) = worker_bundle
                .lock()
                .ok()
                .and_then(|mut bundle| bundle.take())
            else {
                return;
            };
            let (session, error, detail) =
                match start_share_bundle(session, capabilities, preview, operation, cancel) {
                    Ok((session, detail)) => (Some(session), None, detail),
                    Err((session, error)) => (Some(session), Some(error), String::new()),
                };
            let completion = MediaCommand::TransactionCompleted {
                completion: TransactionCompletion::StartShare {
                    fence: operation,
                    session,
                    error,
                    detail,
                    response,
                },
            };
            if let Err(error) = worker_tx.send(completion) {
                if let MediaCommand::TransactionCompleted {
                    completion:
                        TransactionCompletion::StartShare {
                            session, response, ..
                        },
                } = error.0
                {
                    if let Ok(mut guard) = worker_state.lock() {
                        guard.session = session;
                        guard.actor.lifecycle = MediaLifecycleState::CleanupPending;
                        guard.detail = "Share start completion could not reach actor.".into();
                    }
                    let _ = response.send(Err(MediaEngineError::QueueClosed));
                }
            }
        });
    if spawn.is_err() {
        let session = bundle.lock().ok().and_then(|mut bundle| bundle.take());
        if let Ok(mut guard) = state.lock() {
            guard.pending_share = None;
            guard.pending_share_operation = None;
            guard.transition_snapshot = None;
            guard.actor.retire_operation(operation.operation);
            guard.actor.lifecycle = MediaLifecycleState::Running;
            guard.session = session;
        }
        let _ = failure_response.send(Err(MediaEngineError::QueueClosed));
    }
}

fn launch_stop_share(
    state: &Arc<Mutex<EngineState>>,
    control_tx: &SyncSender<MediaCommand>,
    response: SyncSender<Result<MediaSessionSnapshot, MediaEngineError>>,
) {
    #[cfg(test)]
    let stop_barrier = state
        .lock()
        .ok()
        .and_then(|guard| guard.operation_barrier.clone());
    let (operation, session) = {
        let Ok(mut guard) = state.lock() else {
            let _ = response.send(Err(MediaEngineError::StatePoisoned));
            return;
        };
        let Some(session) = guard.session.as_ref() else {
            let _ = response.send(Err(MediaEngineError::NoActiveSession));
            return;
        };
        let mut transition = snapshot_from_state(&guard);
        transition.state = MediaLifecycleState::Stopping;
        transition.detail = "Share cleanup is waiting for workers to quiesce.".into();
        guard.transition_snapshot = Some(transition);
        let session = guard.session.take().expect("session checked above");
        guard.actor.end_share();
        guard.actor.lifecycle = MediaLifecycleState::Stopping;
        let operation_epoch = guard.actor.fence(None);
        let operation = guard
            .actor
            .reserve_operation_kind(operation_epoch, OperationKind::StopShare);
        guard.pending_share = Some(Arc::new(AtomicBool::new(false)));
        guard.pending_share_operation = Some(operation);
        guard.preview.begin_session();
        (operation, session)
    };
    let worker_tx = control_tx.clone();
    let worker_state = Arc::clone(state);
    let failure_response = response.clone();
    let bundle = Arc::new(Mutex::new(Some(session)));
    let worker_bundle = Arc::clone(&bundle);
    let spawn = thread::Builder::new()
        .name("godrinking-share-stop".into())
        .spawn(move || {
            let Some(session) = worker_bundle
                .lock()
                .ok()
                .and_then(|mut bundle| bundle.take())
            else {
                return;
            };
            #[cfg(test)]
            if let Some(barrier) = stop_barrier {
                barrier.wait();
            }
            let (session, error) = match stop_share_bundle(session) {
                Ok(session) => (Some(session), None),
                Err((session, error)) => (Some(session), Some(error)),
            };
            let completion = MediaCommand::TransactionCompleted {
                completion: TransactionCompletion::StopShare {
                    fence: operation,
                    session,
                    error,
                    response,
                },
            };
            if let Err(error) = worker_tx.send(completion) {
                if let MediaCommand::TransactionCompleted {
                    completion:
                        TransactionCompletion::StopShare {
                            session, response, ..
                        },
                } = error.0
                {
                    if let Ok(mut guard) = worker_state.lock() {
                        guard.session = session;
                        guard.actor.lifecycle = MediaLifecycleState::CleanupPending;
                        guard.detail = "Share stop completion could not reach actor.".into();
                    }
                    let _ = response.send(Err(MediaEngineError::QueueClosed));
                }
            }
        });
    if spawn.is_err() {
        let session = bundle.lock().ok().and_then(|mut bundle| bundle.take());
        if let Ok(mut guard) = state.lock() {
            guard.pending_share = None;
            guard.pending_share_operation = None;
            guard.transition_snapshot = None;
            guard.actor.retire_operation(operation.operation);
            guard.actor.lifecycle = MediaLifecycleState::CleanupPending;
            guard.session = session;
        }
        let _ = failure_response.send(Err(MediaEngineError::QueueClosed));
    }
}

fn launch_update_session(
    state: &Arc<Mutex<EngineState>>,
    control_tx: &SyncSender<MediaCommand>,
    request: UpdateMediaSessionRequest,
    source_id_update: SourceIdUpdate,
    response: SyncSender<Result<MediaSessionSnapshot, MediaEngineError>>,
) {
    let cancel = Arc::new(AtomicBool::new(false));
    let (operation, session, capabilities, preview, plan) = {
        let Ok(mut guard) = state.lock() else {
            let _ = response.send(Err(MediaEngineError::StatePoisoned));
            return;
        };
        if guard.session.is_none() {
            let _ = response.send(Err(MediaEngineError::NoActiveSession));
            return;
        }
        let current = guard.session.as_ref().expect("session checked above");
        let plan = match build_update_plan(current, request, source_id_update) {
            Ok(plan) => plan,
            Err(error) => {
                let _ = response.send(Err(error));
                return;
            }
        };
        if guard.pending_start.is_some() || guard.pending_share.is_some() {
            let _ = response.send(Err(MediaEngineError::SessionAlreadyActive));
            return;
        }
        let mut transition = snapshot_from_state(&guard);
        transition.state = MediaLifecycleState::Starting;
        transition.detail = "Session update is acquiring the requested resources.".into();
        guard.transition_snapshot = Some(transition);
        let session = guard.session.take().expect("session checked above");
        if plan.restart_share && session.native_capture_active {
            guard.actor.end_share();
            guard.actor.begin_share();
            guard.preview.begin_session();
        }
        let operation_epoch = guard.actor.fence(None);
        let operation = guard
            .actor
            .reserve_operation_kind(operation_epoch, OperationKind::UpdateSession);
        guard.pending_share = Some(Arc::clone(&cancel));
        guard.pending_share_operation = Some(operation);
        (
            operation,
            session,
            guard.capabilities.clone(),
            Arc::clone(&guard.preview),
            plan,
        )
    };
    let worker_tx = control_tx.clone();
    let worker_state = Arc::clone(state);
    let failure_response = response.clone();
    let bundle = Arc::new(Mutex::new(Some(session)));
    let worker_bundle = Arc::clone(&bundle);
    let spawn = thread::Builder::new()
        .name("godrinking-session-update".into())
        .spawn(move || {
            let Some(session) = worker_bundle
                .lock()
                .ok()
                .and_then(|mut bundle| bundle.take())
            else {
                return;
            };
            let (session, error, detail) = match update_session_bundle(
                session,
                plan,
                capabilities,
                preview,
                operation,
                cancel,
            ) {
                Ok((session, detail)) => (Some(session), None, detail),
                Err((session, error)) => (Some(session), Some(error), String::new()),
            };
            let completion = MediaCommand::TransactionCompleted {
                completion: TransactionCompletion::UpdateSession {
                    fence: operation,
                    session,
                    error,
                    detail,
                    response,
                },
            };
            if let Err(error) = worker_tx.send(completion) {
                if let MediaCommand::TransactionCompleted {
                    completion:
                        TransactionCompletion::UpdateSession {
                            session, response, ..
                        },
                } = error.0
                {
                    if let Ok(mut guard) = worker_state.lock() {
                        guard.session = session;
                        guard.actor.lifecycle = MediaLifecycleState::CleanupPending;
                        guard.detail = "Session update completion could not reach actor.".into();
                    }
                    let _ = response.send(Err(MediaEngineError::QueueClosed));
                }
            }
        });
    if spawn.is_err() {
        let session = bundle.lock().ok().and_then(|mut bundle| bundle.take());
        if let Ok(mut guard) = state.lock() {
            guard.pending_share = None;
            guard.pending_share_operation = None;
            guard.transition_snapshot = None;
            guard.actor.retire_operation(operation.operation);
            guard.actor.lifecycle = MediaLifecycleState::Running;
            guard.session = session;
        }
        let _ = failure_response.send(Err(MediaEngineError::QueueClosed));
    }
}

fn build_update_plan(
    session: &SessionRecord,
    request: UpdateMediaSessionRequest,
    source_id_update: SourceIdUpdate,
) -> Result<UpdatePlan, MediaEngineError> {
    let mut candidate = session.request.clone();
    if let Some(source) = request.source {
        candidate.source = source;
    }
    match source_id_update {
        SourceIdUpdate::Set(id) => candidate.source_id = Some(id),
        SourceIdUpdate::Clear => candidate.source_id = None,
        SourceIdUpdate::Unchanged => {}
    }
    if let Some(resolution) = request.resolution {
        candidate.resolution = resolution;
    }
    if let Some(frame_rate) = request.frame_rate {
        candidate.frame_rate = frame_rate;
    }
    candidate.quality = request.quality;
    candidate.bitrate_bps = request.bitrate_bps;
    candidate.min_bitrate_bps = request.min_bitrate_bps;
    candidate.system_audio = request.system_audio;
    candidate.excluded_apps = request.excluded_apps.clone();
    candidate.codec = request.codec;
    candidate.encoder = request.encoder;
    validate_canonical_config(&candidate)?;
    let restart_share = session.native_capture_active
        && (candidate.source != session.request.source
            || candidate.source_id != session.request.source_id
            || candidate.resolution != session.request.resolution
            || candidate.frame_rate != session.request.frame_rate
            || candidate.codec != session.request.codec
            || candidate.encoder != session.request.encoder);
    Ok(UpdatePlan {
        next: candidate,
        restart_share,
    })
}

fn shutdown_audio_tap(session: &mut SessionRecord) -> Result<(), String> {
    let Some(mut tap) = session.audio_tap.take() else {
        return Ok(());
    };
    let status = tap.shutdown_and_join(Duration::from_secs(3));
    if status.quiesced && status.errors.is_empty() {
        return Ok(());
    }
    let mut detail = status
        .pending
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    detail.extend(status.errors);
    session.audio_tap = Some(tap);
    Err(if detail.is_empty() {
        "audio tap cleanup is not quiescent".into()
    } else {
        detail.join("; ")
    })
}

fn update_session_bundle(
    mut session: SessionRecord,
    plan: UpdatePlan,
    capabilities: MediaCapabilities,
    preview: Arc<PreviewState>,
    operation: OperationFence,
    cancel: Arc<AtomicBool>,
) -> Result<(SessionRecord, String), (SessionRecord, MediaEngineError)> {
    let next = plan.next;
    if plan.restart_share {
        if cancel.load(Ordering::Acquire) {
            return Err((
                session,
                MediaEngineError::NativeCapture("session update cancelled".into()),
            ));
        }
        let mut restarted = match stop_share_bundle(session) {
            Ok(session) => session,
            Err((session, error)) => return Err((session, error)),
        };
        restarted.request = next;
        return start_share_bundle(restarted, capabilities, preview, operation, cancel)
            .map_err(|(session, error)| (session, error));
    }
    let target = super::types::resolve_bitrate(next.quality, next.bitrate_bps);
    let floor = super::types::resolve_floor(target, next.min_bitrate_bps);
    if session.request.quality != next.quality || session.request.bitrate_bps != next.bitrate_bps {
        let _ = session._pipeline.set_bitrate(target);
        let _ = session._pipeline.force_keyframe();
    }
    if session.request.min_bitrate_bps != next.min_bitrate_bps {
        let _ = session._pipeline.set_floor(floor);
    }
    let mut audio_note = String::new();
    if next.system_audio && session.native_capture_active {
        if let Some(tx) = session.audio_tx.clone() {
            if let Err(error) = shutdown_audio_tap(&mut session) {
                return Err((session, MediaEngineError::NativeCapture(error)));
            }
            match ProcessTap::start(&next.excluded_apps, tx) {
                Ok(tap) => session.audio_tap = Some(tap),
                Err(error) => audio_note = format!(" System audio tap restart failed: {error}"),
            }
        } else {
            audio_note =
                " System audio cannot be added mid-session; restart the session to enable it."
                    .into();
        }
    } else if !next.system_audio {
        if let Err(error) = shutdown_audio_tap(&mut session) {
            return Err((session, MediaEngineError::NativeCapture(error)));
        }
    }
    session.request = next;
    if cancel.load(Ordering::Acquire) {
        return Err((
            session,
            MediaEngineError::NativeCapture("session update cancelled".into()),
        ));
    }
    Ok((
        session,
        format!("Session settings updated; capture and peer transport kept running.{audio_note}"),
    ))
}

fn spawn_cleanup_transaction(
    state: &Arc<Mutex<EngineState>>,
    control_tx: &SyncSender<MediaCommand>,
    fence: OperationFence,
    session: SessionRecord,
    response: SyncSender<Result<MediaSessionSnapshot, MediaEngineError>>,
) {
    let worker_tx = control_tx.clone();
    let failure_response = response.clone();
    let worker_state = Arc::clone(state);
    let bundle = Arc::new(Mutex::new(Some(session)));
    let worker_bundle = Arc::clone(&bundle);
    let spawn = thread::Builder::new()
        .name("godrinking-session-stop".into())
        .spawn(move || {
            let Some(session) = worker_bundle
                .lock()
                .ok()
                .and_then(|mut bundle| bundle.take())
            else {
                return;
            };
            let cleanup = cleanup_session_bundle(session);
            let completion = MediaCommand::TransactionCompleted {
                completion: TransactionCompletion::StopSession {
                    fence,
                    cleanup,
                    response,
                },
            };
            if let Err(error) = worker_tx.send(completion) {
                if let MediaCommand::TransactionCompleted {
                    completion:
                        TransactionCompletion::StopSession {
                            mut cleanup,
                            response,
                            ..
                        },
                } = error.0
                {
                    if let Some(session) = cleanup.session.take() {
                        if let Ok(mut bundle) = worker_bundle.lock() {
                            *bundle = Some(session);
                        }
                        if let Ok(mut guard) = worker_state.lock() {
                            guard.actor.lifecycle = MediaLifecycleState::CleanupPending;
                            guard.detail =
                                "Session cleanup completion could not reach actor.".into();
                        }
                    }
                    let _ = response.send(Err(MediaEngineError::QueueClosed));
                }
            }
        });
    if spawn.is_err() {
        // A thread could not be created. Retain the detached bundle in
        // CleanupPending rather than dropping/joining it on the actor thread.
        if let Ok(mut guard) = state.lock() {
            guard.actor.lifecycle = MediaLifecycleState::CleanupPending;
            guard.detail = "Session cleanup worker could not start.".into();
        }
        if let Some(session) = bundle.lock().ok().and_then(|mut bundle| bundle.take()) {
            if let Ok(mut guard) = state.lock() {
                guard.session = Some(session);
            }
        }
        let _ = failure_response.send(Err(MediaEngineError::QueueClosed));
    }
}

fn spawn_stale_cleanup_transaction(
    state: &Arc<Mutex<EngineState>>,
    control_tx: &SyncSender<MediaCommand>,
    fence: OperationFence,
    session: SessionRecord,
    waiters: Vec<SyncSender<Result<MediaSessionSnapshot, MediaEngineError>>>,
) {
    let worker_tx = control_tx.clone();
    let worker_state = Arc::clone(state);
    let bundle = Arc::new(Mutex::new(Some(session)));
    let waiters = Arc::new(Mutex::new(Some(waiters)));
    let worker_bundle = Arc::clone(&bundle);
    let worker_waiters = Arc::clone(&waiters);
    let spawn = thread::Builder::new()
        .name("godrinking-stale-cleanup".into())
        .spawn(move || {
            let session = worker_bundle
                .lock()
                .ok()
                .and_then(|mut bundle| bundle.take());
            let Some(session) = session else { return };
            let cleanup = cleanup_session_bundle(session);
            let waiters = worker_waiters
                .lock()
                .ok()
                .and_then(|mut waiters| waiters.take())
                .unwrap_or_default();
            let completion = MediaCommand::TransactionCompleted {
                completion: TransactionCompletion::StaleCleanup {
                    fence,
                    cleanup,
                    waiters,
                },
            };
            if let Err(error) = worker_tx.send(completion) {
                if let MediaCommand::TransactionCompleted {
                    completion:
                        TransactionCompletion::StaleCleanup {
                            mut cleanup,
                            waiters,
                            ..
                        },
                } = error.0
                {
                    if let Some(session) = cleanup.session.take() {
                        if let Ok(mut guard) = worker_bundle.lock() {
                            *guard = Some(session);
                        }
                        if let Ok(mut guard) = worker_state.lock() {
                            guard.actor.lifecycle = MediaLifecycleState::CleanupPending;
                            guard.detail = "Stale cleanup completion could not reach actor.".into();
                        }
                    }
                    for waiter in waiters {
                        let _ = waiter.send(Err(MediaEngineError::QueueClosed));
                    }
                }
            }
        });
    if spawn.is_err() {
        let session = bundle.lock().ok().and_then(|mut bundle| bundle.take());
        if let Ok(mut guard) = state.lock() {
            guard.session = session;
            guard.actor.lifecycle = MediaLifecycleState::CleanupPending;
            guard.detail = "Stale resource cleanup worker could not start.".into();
        }
        let waiters = waiters
            .lock()
            .ok()
            .and_then(|mut waiters| waiters.take())
            .unwrap_or_default();
        for waiter in waiters {
            let _ = waiter.send(Err(MediaEngineError::QueueClosed));
        }
    }
}

fn request_stop(
    state: &Arc<Mutex<EngineState>>,
    control_tx: &SyncSender<MediaCommand>,
    response: SyncSender<Result<MediaSessionSnapshot, MediaEngineError>>,
) {
    let cleanup = {
        let Ok(mut guard) = state.lock() else {
            let _ = response.send(Err(MediaEngineError::StatePoisoned));
            return;
        };
        if let Some(cancel) = guard.pending_start.as_ref() {
            cancel.store(true, Ordering::Release);
            guard.actor.invalidate_session();
            guard.stop_waiters.push(response);
            return;
        }
        if let Some(cancel) = guard.pending_share.as_ref() {
            cancel.store(true, Ordering::Release);
            guard.actor.invalidate_session();
            guard.stop_waiters.push(response);
            return;
        }
        if guard.session.is_none() {
            let _ = response.send(Err(MediaEngineError::NoActiveSession));
            return;
        }
        let mut transition = snapshot_from_state(&guard);
        transition.state = MediaLifecycleState::Stopping;
        transition.detail = "Session cleanup is waiting for resources to quiesce.".into();
        guard.transition_snapshot = Some(transition);
        guard.actor.invalidate_session();
        let stop_epoch = guard.actor.fence(None);
        let fence = guard
            .actor
            .reserve_operation_kind(stop_epoch, OperationKind::StopSession);
        guard.pending_stop_operation = Some(fence);
        (
            fence,
            guard.session.take().expect("active session checked above"),
        )
    };
    spawn_cleanup_transaction(state, control_tx, cleanup.0, cleanup.1, response);
}

fn accept_transaction_completion(
    state: &Arc<Mutex<EngineState>>,
    completion: TransactionCompletion,
) {
    match completion {
        TransactionCompletion::StartSession {
            fence,
            result,
            detail,
            response,
        } => {
            let mut stale_cleanup = None;
            let (reply, stop_waiters) = {
                let Ok(mut guard) = state.lock() else {
                    let _ = response.send(Err(MediaEngineError::StatePoisoned));
                    return;
                };
                let live = guard.pending_start_operation == Some(fence)
                    && guard
                        .actor
                        .accepts_operation_kind(fence, OperationKind::StartSession);
                guard.pending_start = None;
                guard.pending_start_operation = None;
                guard.transition_snapshot = None;
                guard.actor.retire_operation(fence.operation);
                if live {
                    match result {
                        Ok(session) => {
                            // Join services were committed in the transaction
                            // worker (create_session_bundle) before
                            // TransactionCompleted, so publishing here never
                            // runs network calls under the actor lock.
                            guard.session = Some(session);
                            guard.actor.lifecycle = MediaLifecycleState::Running;
                            guard.detail = detail;
                            (Ok(snapshot_from_state(&guard)), Vec::new())
                        }
                        Err(error) => {
                            guard.actor.lifecycle = MediaLifecycleState::Idle;
                            guard.detail = format!("Session start failed: {error}");
                            (Err(error), Vec::new())
                        }
                    }
                } else if !guard.pending_peer_cleanup.is_empty() {
                    guard.actor.lifecycle = MediaLifecycleState::CleanupPending;
                    guard.detail = "Session stopped; deferred peer cleanup is pending.".into();
                    (
                        Err(MediaEngineError::NativePeer(
                            "peer initialization cleanup is pending".into(),
                        )),
                        Vec::new(),
                    )
                } else {
                    log_transaction(
                        "WARN",
                        "stale completion",
                        fence,
                        "start completion discarded",
                    );
                    let waiters = std::mem::take(&mut guard.stop_waiters);
                    if let Ok(session) = result {
                        let mut transition = snapshot_from_state(&guard);
                        transition.state = MediaLifecycleState::Stopping;
                        transition.detail =
                            "Stale session start is cleaning up provisional resources.".into();
                        guard.transition_snapshot = Some(transition);
                        guard.actor.lifecycle = MediaLifecycleState::Stopping;
                        let cleanup_epoch = guard.actor.fence(None);
                        let cleanup_fence = guard
                            .actor
                            .reserve_operation_kind(cleanup_epoch, OperationKind::StopSession);
                        guard.pending_stop_operation = Some(cleanup_fence);
                        stale_cleanup = Some((cleanup_fence, session, waiters));
                        (
                            Err(MediaEngineError::NativeCapture(
                                "session start completion became stale".into(),
                            )),
                            Vec::new(),
                        )
                    } else {
                        guard.actor.lifecycle = MediaLifecycleState::Idle;
                        guard.detail = "Session start cancelled; resources released.".into();
                        (
                            Err(MediaEngineError::NativeCapture(
                                "session start completion became stale".into(),
                            )),
                            waiters,
                        )
                    }
                }
            };
            if let Some((cleanup_fence, session, waiters)) = stale_cleanup {
                let control_tx = state.lock().ok().and_then(|guard| guard.control_tx.clone());
                if let Some(control_tx) = control_tx {
                    spawn_stale_cleanup_transaction(
                        state,
                        &control_tx,
                        cleanup_fence,
                        session,
                        waiters,
                    );
                }
            }
            let _ = response.send(reply);
            if !stop_waiters.is_empty() {
                let snapshot = state.lock().ok().map(|guard| snapshot_from_state(&guard));
                for waiter in stop_waiters {
                    if let Some(snapshot) = snapshot.clone() {
                        let _ = waiter.send(Ok(snapshot));
                    }
                }
            }
        }
        TransactionCompletion::StopSession {
            fence,
            mut cleanup,
            response,
        } => {
            let mut stale_session: Option<SessionRecord> = None;
            let reply = {
                let Ok(mut guard) = state.lock() else {
                    let _ = response.send(Err(MediaEngineError::StatePoisoned));
                    return;
                };
                let live = guard.pending_stop_operation == Some(fence)
                    && guard
                        .actor
                        .accepts_operation_kind(fence, OperationKind::StopSession);
                guard.pending_stop_operation = None;
                guard.actor.retire_operation(fence.operation);
                guard.transition_snapshot = None;
                if !live {
                    stale_session = cleanup.session.take();
                    Err(MediaEngineError::NativeCapture(
                        "session cleanup completion became stale".into(),
                    ))
                } else if let Some(error) = cleanup.error.take() {
                    guard.session = cleanup.session.take();
                    guard.actor.lifecycle = MediaLifecycleState::CleanupPending;
                    guard.detail = format!("Session cleanup pending: {error}");
                    Err(error)
                } else {
                    guard.session = None;
                    guard.actor.lifecycle = MediaLifecycleState::Idle;
                    guard.detail = "Session stopped; native resources quiescent.".into();
                    guard.preview.begin_session();
                    Ok(snapshot_from_state(&guard))
                }
            };
            drop(stale_session);
            let _ = response.send(reply);
        }
        TransactionCompletion::StaleCleanup {
            fence,
            mut cleanup,
            waiters,
        } => {
            let mut stale_session: Option<SessionRecord> = None;
            let snapshot = {
                let Ok(mut guard) = state.lock() else {
                    return;
                };
                let live = guard.pending_stop_operation == Some(fence)
                    && guard
                        .actor
                        .accepts_operation_kind(fence, OperationKind::StopSession);
                guard.pending_stop_operation = None;
                guard.actor.retire_operation(fence.operation);
                guard.transition_snapshot = None;
                if !live {
                    log_transaction(
                        "WARN",
                        "stale completion",
                        fence,
                        "session cleanup discarded",
                    );
                    stale_session = cleanup.session.take();
                    None
                } else if let Some(error) = cleanup.error.take() {
                    guard.session = cleanup.session.take();
                    guard.actor.lifecycle = MediaLifecycleState::CleanupPending;
                    guard.detail = format!("Stale cleanup pending: {error}");
                    None
                } else {
                    guard.actor.lifecycle = MediaLifecycleState::Idle;
                    guard.detail = "Session start cancelled; resources released.".into();
                    Some(snapshot_from_state(&guard))
                }
            };
            drop(stale_session);
            for waiter in waiters {
                if let Some(snapshot) = snapshot.clone() {
                    let _ = waiter.send(Ok(snapshot));
                } else {
                    let _ = waiter.send(Err(MediaEngineError::NativeCapture(
                        "stale resource cleanup is pending".into(),
                    )));
                }
            }
        }
        TransactionCompletion::StartShare {
            fence,
            mut session,
            error,
            detail,
            response,
        } => {
            let mut stale_cleanup = None;
            let (reply, stop_waiters) = {
                let Ok(mut guard) = state.lock() else {
                    let _ = response.send(Err(MediaEngineError::StatePoisoned));
                    return;
                };
                let live = guard.pending_share_operation == Some(fence)
                    && guard
                        .actor
                        .accepts_operation_kind(fence, OperationKind::StartShare);
                guard.pending_share = None;
                guard.pending_share_operation = None;
                guard.actor.retire_operation(fence.operation);
                if !live {
                    log_transaction("WARN", "stale completion", fence, "share start discarded");
                    guard.transition_snapshot = None;
                    let waiters = std::mem::take(&mut guard.stop_waiters);
                    if let Some(session) = session.take() {
                        let mut transition = snapshot_from_state(&guard);
                        transition.state = MediaLifecycleState::Stopping;
                        transition.detail =
                            "Stale Share start is cleaning up provisional resources.".into();
                        guard.transition_snapshot = Some(transition);
                        guard.actor.lifecycle = MediaLifecycleState::Stopping;
                        let cleanup_epoch = guard.actor.fence(None);
                        let cleanup_fence = guard
                            .actor
                            .reserve_operation_kind(cleanup_epoch, OperationKind::StopSession);
                        guard.pending_stop_operation = Some(cleanup_fence);
                        stale_cleanup = Some((cleanup_fence, session, waiters));
                        (
                            Err(MediaEngineError::NativeCapture(
                                "share start completion became stale".into(),
                            )),
                            Vec::new(),
                        )
                    } else {
                        guard.actor.lifecycle = MediaLifecycleState::Idle;
                        guard.detail = "Share start cancelled; resources released.".into();
                        (
                            Err(MediaEngineError::NativeCapture(
                                "share start completion became stale".into(),
                            )),
                            waiters,
                        )
                    }
                } else if let Some(error) = error {
                    guard.session = session.take();
                    guard.transition_snapshot = None;
                    guard.actor.lifecycle = MediaLifecycleState::Running;
                    guard.detail = format!("Share start failed: {error}");
                    (Err(error), Vec::new())
                } else if let Some(session) = session.take() {
                    guard.session = Some(session);
                    guard.transition_snapshot = None;
                    guard.actor.lifecycle = MediaLifecycleState::Running;
                    guard.detail = detail;
                    (Ok(snapshot_from_state(&guard)), Vec::new())
                } else {
                    guard.transition_snapshot = None;
                    guard.actor.lifecycle = MediaLifecycleState::Running;
                    (
                        Err(MediaEngineError::NativeCapture(
                            "share start returned no resource bundle".into(),
                        )),
                        Vec::new(),
                    )
                }
            };
            // A stale completion's detached Share bundle is dropped only after
            // leaving the actor lock.
            if let Some((cleanup_fence, session, waiters)) = stale_cleanup {
                let control_tx = state.lock().ok().and_then(|guard| guard.control_tx.clone());
                if let Some(control_tx) = control_tx {
                    spawn_stale_cleanup_transaction(
                        state,
                        &control_tx,
                        cleanup_fence,
                        session,
                        waiters,
                    );
                }
            }
            drop(session);
            let _ = response.send(reply);
            if !stop_waiters.is_empty() {
                if let Some(snapshot) = state.lock().ok().map(|guard| snapshot_from_state(&guard)) {
                    for waiter in stop_waiters {
                        let _ = waiter.send(Ok(snapshot.clone()));
                    }
                }
            }
        }
        TransactionCompletion::StopShare {
            fence,
            mut session,
            error,
            response,
        } => {
            let mut stale_cleanup = None;
            let (reply, stop_waiters, notify_share_stopped) = {
                let Ok(mut guard) = state.lock() else {
                    let _ = response.send(Err(MediaEngineError::StatePoisoned));
                    return;
                };
                let live = guard.pending_share_operation == Some(fence)
                    && guard
                        .actor
                        .accepts_operation_kind(fence, OperationKind::StopShare);
                guard.pending_share = None;
                guard.pending_share_operation = None;
                guard.actor.retire_operation(fence.operation);
                guard.transition_snapshot = None;
                if !live {
                    log_transaction("WARN", "stale completion", fence, "share cleanup discarded");
                    let waiters = std::mem::take(&mut guard.stop_waiters);
                    if let Some(session) = session.take() {
                        let mut transition = snapshot_from_state(&guard);
                        transition.state = MediaLifecycleState::Stopping;
                        transition.detail =
                            "Stale Share cleanup is entering full session cleanup.".into();
                        guard.transition_snapshot = Some(transition);
                        guard.actor.lifecycle = MediaLifecycleState::Stopping;
                        let cleanup_epoch = guard.actor.fence(None);
                        let cleanup_fence = guard
                            .actor
                            .reserve_operation_kind(cleanup_epoch, OperationKind::StopSession);
                        guard.pending_stop_operation = Some(cleanup_fence);
                        stale_cleanup = Some((cleanup_fence, session, waiters));
                        (
                            Err(MediaEngineError::NativeCapture(
                                "share cleanup completion became stale".into(),
                            )),
                            Vec::new(),
                            false,
                        )
                    } else {
                        guard.actor.lifecycle = MediaLifecycleState::Idle;
                        guard.detail = "Share cleanup cancelled; resources released.".into();
                        (
                            Err(MediaEngineError::NativeCapture(
                                "share cleanup completion became stale".into(),
                            )),
                            waiters,
                            false,
                        )
                    }
                } else if let Some(error) = error {
                    guard.session = session.take();
                    guard.actor.lifecycle = MediaLifecycleState::CleanupPending;
                    guard.detail = format!("Share cleanup pending: {error}");
                    (Err(error), Vec::new(), false)
                } else if let Some(mut session) = session.take() {
                    session.share_epoch = fence.epoch.share;
                    guard.session = Some(session);
                    guard.actor.lifecycle = MediaLifecycleState::Running;
                    guard.detail = "Share stopped; resources quiescent.".into();
                    (Ok(snapshot_from_state(&guard)), Vec::new(), true)
                } else {
                    guard.actor.lifecycle = MediaLifecycleState::CleanupPending;
                    guard.detail = "Share cleanup returned no session bundle.".into();
                    (
                        Err(MediaEngineError::NativeCapture(
                            "share cleanup returned no resource bundle".into(),
                        )),
                        Vec::new(),
                        false,
                    )
                }
            };
            if let Some((cleanup_fence, session, waiters)) = stale_cleanup {
                let control_tx = state.lock().ok().and_then(|guard| guard.control_tx.clone());
                if let Some(control_tx) = control_tx {
                    spawn_stale_cleanup_transaction(
                        state,
                        &control_tx,
                        cleanup_fence,
                        session,
                        waiters,
                    );
                }
            }
            drop(session);
            let _ = response.send(reply);
            if notify_share_stopped {
                announce_viewer_share(state, false);
            }
            if !stop_waiters.is_empty() {
                if let Some(snapshot) = state.lock().ok().map(|guard| snapshot_from_state(&guard)) {
                    for waiter in stop_waiters {
                        let _ = waiter.send(Ok(snapshot.clone()));
                    }
                }
            }
        }
        TransactionCompletion::UpdateSession {
            fence,
            mut session,
            error,
            detail,
            response,
        } => {
            let mut stale_cleanup = None;
            let (reply, stop_waiters) = {
                let Ok(mut guard) = state.lock() else {
                    let _ = response.send(Err(MediaEngineError::StatePoisoned));
                    return;
                };
                let live = guard.pending_share_operation == Some(fence)
                    && guard
                        .actor
                        .accepts_operation_kind(fence, OperationKind::UpdateSession);
                guard.pending_share = None;
                guard.pending_share_operation = None;
                guard.actor.retire_operation(fence.operation);
                guard.transition_snapshot = None;
                if !live {
                    log_transaction(
                        "WARN",
                        "stale completion",
                        fence,
                        "session update discarded",
                    );
                    let waiters = std::mem::take(&mut guard.stop_waiters);
                    if let Some(session) = session.take() {
                        let mut transition = snapshot_from_state(&guard);
                        transition.state = MediaLifecycleState::Stopping;
                        transition.detail =
                            "Stale Share update is cleaning up provisional resources.".into();
                        guard.transition_snapshot = Some(transition);
                        guard.actor.lifecycle = MediaLifecycleState::Stopping;
                        let cleanup_epoch = guard.actor.fence(None);
                        let cleanup_fence = guard
                            .actor
                            .reserve_operation_kind(cleanup_epoch, OperationKind::StopSession);
                        guard.pending_stop_operation = Some(cleanup_fence);
                        stale_cleanup = Some((cleanup_fence, session, waiters));
                        (
                            Err(MediaEngineError::NativeCapture(
                                "session update completion became stale".into(),
                            )),
                            Vec::new(),
                        )
                    } else {
                        guard.actor.lifecycle = MediaLifecycleState::Idle;
                        guard.detail = "Session update cancelled; resources released.".into();
                        (
                            Err(MediaEngineError::NativeCapture(
                                "session update completion became stale".into(),
                            )),
                            waiters,
                        )
                    }
                } else if let Some(error) = error {
                    guard.session = session.take();
                    guard.actor.lifecycle = MediaLifecycleState::Running;
                    guard.detail = format!("Session update failed: {error}");
                    (Err(error), Vec::new())
                } else if let Some(session) = session.take() {
                    guard.session = Some(session);
                    guard.actor.lifecycle = MediaLifecycleState::Running;
                    guard.detail = detail;
                    (Ok(snapshot_from_state(&guard)), Vec::new())
                } else {
                    guard.actor.lifecycle = MediaLifecycleState::CleanupPending;
                    guard.detail = "Session update returned no resource bundle.".into();
                    (
                        Err(MediaEngineError::NativeCapture(
                            "session update returned no resource bundle".into(),
                        )),
                        Vec::new(),
                    )
                }
            };
            if let Some((cleanup_fence, session, waiters)) = stale_cleanup {
                let control_tx = state.lock().ok().and_then(|guard| guard.control_tx.clone());
                if let Some(control_tx) = control_tx {
                    spawn_stale_cleanup_transaction(
                        state,
                        &control_tx,
                        cleanup_fence,
                        session,
                        waiters,
                    );
                }
            }
            drop(session);
            let _ = response.send(reply);
            if !stop_waiters.is_empty() {
                if let Some(snapshot) = state.lock().ok().map(|guard| snapshot_from_state(&guard)) {
                    for waiter in stop_waiters {
                        let _ = waiter.send(Ok(snapshot.clone()));
                    }
                }
            }
        }
    }
}

fn worker_loop(
    receiver: Receiver<MediaCommand>,
    state: Arc<Mutex<EngineState>>,
    control_tx: SyncSender<MediaCommand>,
) {
    let engine = MediaEngine {
        control_tx,
        state: Arc::clone(&state),
    };
    loop {
        let command = match receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(command) => Some(command),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if let Some(command) = command {
            match command {
                MediaCommand::Create { request, response } => {
                    launch_start_session(&state, &engine.control_tx, request, response);
                }
                MediaCommand::Update {
                    request,
                    source_id_update,
                    response,
                } => {
                    launch_update_session(
                        &state,
                        &engine.control_tx,
                        request,
                        source_id_update,
                        response,
                    );
                }
                MediaCommand::Stop { response } => {
                    request_stop(&state, &engine.control_tx, response);
                }
                MediaCommand::StartShare { response } => {
                    launch_start_share(&state, &engine.control_tx, response);
                }
                MediaCommand::StopShare { response } => {
                    launch_stop_share(&state, &engine.control_tx, response);
                }
                MediaCommand::MintOffer {
                    id,
                    nickname,
                    origin,
                    response,
                } => {
                    let result = mint_viewer_offer_fenced(&state, &id, &nickname, origin).and_then(
                        |offer| {
                            MediaOffer::from_exact(ExactOffer {
                                signal: offer.signal,
                                fence: offer.fence,
                            })
                        },
                    );
                    let _ = response.send(result);
                }
                MediaCommand::Kick { id, response } => {
                    let _ = response.send(engine.kick_viewer_in_state(&id));
                }
                MediaCommand::Admit { id, response } => {
                    let _ = response.send(engine.admit_viewer_in_state(&id));
                }
                MediaCommand::Reject { id, response } => {
                    let _ = response.send(engine.reject_viewer_in_state(&id));
                }
                MediaCommand::Credentials { request, response } => {
                    let _ = response.send(update_credentials_in_state(&state, request));
                }
                MediaCommand::SetAnswer {
                    answer,
                    offer_attempt,
                    response,
                } => {
                    let _ = response.send(engine.set_peer_answer_in_state(answer, offer_attempt));
                }
                MediaCommand::ClosePeers { response } => {
                    let _ = response.send(engine.close_peer_transport_in_state());
                }
                MediaCommand::AttachStunarViewer { viewer, response } => {
                    let result = state
                        .lock()
                        .map_err(|_| MediaEngineError::StatePoisoned)
                        .and_then(|mut guard| {
                            if guard.stunar_viewer.is_some() {
                                Err(MediaEngineError::SessionAlreadyActive)
                            } else {
                                guard.stunar_viewer = Some(viewer);
                                Ok(())
                            }
                        });
                    let _ = response.send(result);
                }
                MediaCommand::CloseStunarViewer { response } => {
                    let viewer = state
                        .lock()
                        .ok()
                        .and_then(|mut guard| guard.stunar_viewer.take());
                    if let Some(viewer) = viewer {
                        viewer.leave();
                    }
                    let _ = response.send(());
                }
                MediaCommand::TransactionCompleted { completion } => {
                    accept_transaction_completion(&state, completion);
                }
            }
        }
        // Join workers publish into their bounded metadata mailboxes. This
        // pump is actor-owned and independent of snapshot polling.
        engine.apply_room_answer();
        engine.apply_stunar_accepts();
        engine.apply_stunar_watches();
        reap_failed_links(&state);
        retry_pending_peer_cleanup(&state);
        if let Ok(mut guard) = state.lock() {
            refresh_native_state(&mut guard);
        }
    }
}

fn retry_pending_peer_cleanup(state: &Arc<Mutex<EngineState>>) {
    let mut pending = state
        .lock()
        .ok()
        .map(|mut guard| std::mem::take(&mut guard.pending_peer_cleanup))
        .unwrap_or_default();
    let mut retained = Vec::new();
    for mut peer in pending.drain(..) {
        let status = peer.shutdown_and_join(Duration::from_millis(10));
        if !status.quiesced {
            retained.push(peer);
        }
    }
    if let Ok(mut guard) = state.lock() {
        guard.pending_peer_cleanup.extend(retained);
        if guard.pending_peer_cleanup.is_empty()
            && guard.session.is_none()
            && guard.actor.lifecycle == MediaLifecycleState::CleanupPending
        {
            guard.actor.lifecycle = MediaLifecycleState::Idle;
            guard.detail = "Deferred peer cleanup completed; resources quiescent.".into();
        }
    }
}

/// Stunar rooms always carry a Password (4-64 chars, counted like the
/// Rendezvous does: Unicode code points). LAN/Direct keep optional.
fn valid_password(password: &str) -> bool {
    let len = password.chars().count();
    (4..=64).contains(&len)
}

#[cfg(test)]
fn rollback_start(state: &Arc<Mutex<EngineState>>, epoch: SessionEpoch, reason: &str) {
    if let Ok(mut state) = state.lock() {
        if state.actor.session_epoch == epoch
            && state.actor.lifecycle == MediaLifecycleState::Starting
        {
            state.actor.lifecycle = MediaLifecycleState::Idle;
            state.detail = format!("Session start rolled back: {reason}");
            logger::log(
                "WARN",
                "session rollback",
                &format!("session_epoch={} reason={reason}", epoch.0),
            );
        }
    }
}

fn transaction_failed(operation: OperationFence, reason: &str) {
    log_transaction(
        "WARN",
        "transaction failure",
        operation,
        &format!("failed: {reason}"),
    );
}

fn log_transaction(level: &str, event: &str, operation: OperationFence, detail: &str) {
    let link = operation
        .epoch
        .link
        .map(|link| link.0.to_string())
        .unwrap_or_else(|| "none".into());
    logger::log(
        level,
        event,
        &format!(
            "session={} share={} link={} operation={} kind={:?} {detail}",
            operation.epoch.session.0,
            operation.epoch.share.0,
            link,
            operation.operation.0,
            operation.kind,
        ),
    );
}

#[cfg(target_os = "macos")]
fn stop_capture_for_rollback(
    adapter: &mut super::screen_capture_kit::ScreenCaptureKitAdapter,
    active: bool,
    generation: u64,
) -> Result<(), String> {
    if active {
        adapter
            .stop_capture(generation)
            .map_err(|error| error.to_string())
    } else {
        Ok(())
    }
}

/// Owns and releases a detached Session bundle. The actor never waits on a
/// native adapter, room, peer, fanout, or pipeline worker while synchronized.
fn cleanup_session_bundle(mut session: SessionRecord) -> CleanupBundle {
    logger::log(
        "INFO",
        "session cleanup",
        &format!("session={} stopping detached resources", session.id),
    );
    let mut ledger = CleanupLedger::new();
    #[cfg(test)]
    if session
        .resource_lease
        .as_ref()
        .map(|lease| lease.counters.fail_cleanup.load(Ordering::Acquire))
        .unwrap_or(false)
    {
        ledger.error("test resource cleanup failure".into());
    }

    #[cfg(target_os = "macos")]
    if let Err(error) = session.adapter.stop_capture(session.generation) {
        ledger.error(format!("capture: {error}"));
    }
    #[cfg(target_os = "windows")]
    if let Err(error) = session.adapter.stop_capture() {
        ledger.error(format!("capture: {error}"));
    }

    // Detach links while the bundle is private. Peer workers are then stopped
    // without holding the actor lock, and failed handles remain in the ledger.
    let mut viewers = std::mem::take(&mut session.viewers);
    for (id, mut link) in viewers.drain() {
        let status = link.peer.shutdown_and_join(Duration::from_secs(3));
        ledger.status("peer", status.quiesced, status.pending, status.errors);
        if !status.quiesced {
            session.viewers.insert(id, link);
        }
    }

    if let Some(mut fanout) = session.fanout.take() {
        let status = fanout.shutdown_and_join(Duration::from_secs(3));
        ledger.status("fanout", status.quiesced, status.pending, status.errors);
        if !status.quiesced {
            session.fanout = Some(fanout);
        }
    }
    if let Some(mut tap) = session.audio_tap.take() {
        let status = tap.shutdown_and_join(Duration::from_secs(3));
        ledger.status("audio tap", status.quiesced, status.pending, status.errors);
        if !status.quiesced {
            session.audio_tap = Some(tap);
        }
    }
    let pipeline_status = session._pipeline.shutdown_and_join(Duration::from_secs(3));
    ledger.status(
        "pipeline",
        pipeline_status.quiesced,
        pipeline_status.pending,
        pipeline_status.errors,
    );

    if let Some(mut room) = session.room.take() {
        let status = room.shutdown_and_join(Duration::from_secs(3));
        ledger.status("LAN room", status.quiesced, status.pending, status.errors);
        if !status.quiesced {
            session.room = Some(room);
        }
    }
    if let Some(mut room) = session.direct_room.take() {
        let status = room.shutdown_and_join(Duration::from_secs(3));
        ledger.status(
            "Direct room",
            status.quiesced,
            status.pending,
            status.errors,
        );
        if !status.quiesced {
            session.direct_room = Some(room);
        }
    }
    if let Some(stunar) = session.stunar.take() {
        match Arc::try_unwrap(stunar) {
            Ok(mut stunar) => {
                let status = stunar.shutdown_and_join(Duration::from_secs(3));
                ledger.status("Stunar", status.quiesced, status.pending, status.errors);
                if !status.quiesced {
                    session.stunar = Some(Arc::new(stunar));
                }
            }
            Err(stunar) => {
                stunar.close();
                ledger.error("Stunar remained shared during cleanup".into());
                session.stunar = Some(stunar);
            }
        }
    }

    if ledger.quiesced && ledger.errors.is_empty() && session.viewers.is_empty() {
        session.audio_tx = None;
        session.room = None;
        session.direct_room = None;
        session.stunar = None;
        drop(session);
        logger::log("INFO", "session cleanup", "resources quiescent");
        CleanupBundle {
            session: None,
            error: None,
        }
    } else {
        let error = ledger.summary();
        logger::log("ERROR", "session cleanup", &error);
        CleanupBundle {
            session: Some(session),
            error: Some(MediaEngineError::NativeCapture(error)),
        }
    }
}

struct CleanupLedger {
    quiesced: bool,
    pending: Vec<String>,
    errors: Vec<String>,
}

impl CleanupLedger {
    fn new() -> Self {
        Self {
            quiesced: true,
            pending: Vec::new(),
            errors: Vec::new(),
        }
    }

    fn status(
        &mut self,
        owner: &str,
        quiesced: bool,
        pending: Vec<&'static str>,
        errors: Vec<String>,
    ) {
        self.quiesced &= quiesced;
        self.pending
            .extend(pending.into_iter().map(|item| format!("{owner}: {item}")));
        self.errors
            .extend(errors.into_iter().map(|item| format!("{owner}: {item}")));
        if !quiesced {
            self.quiesced = false;
        }
    }

    fn error(&mut self, error: String) {
        self.quiesced = false;
        self.errors.push(error);
    }

    fn summary(&self) -> String {
        let mut parts = self.pending.clone();
        parts.extend(self.errors.clone());
        if parts.is_empty() {
            "resource cleanup is not quiescent".into()
        } else {
            parts.join("; ")
        }
    }
}

#[cfg(target_os = "windows")]
fn stop_capture_for_rollback(
    adapter: &mut super::windows_capture::WindowsCaptureAdapter,
    active: bool,
    _generation: u64,
) -> Result<(), String> {
    if active {
        adapter.stop_capture()
    } else {
        Ok(())
    }
}

#[cfg(test)]
fn start_cancelled(state: &Arc<Mutex<EngineState>>) -> bool {
    state
        .lock()
        .ok()
        .and_then(|guard| guard.pending_start.clone())
        .map(|cancel| cancel.load(Ordering::Acquire))
        .unwrap_or(false)
}

#[cfg(test)]
fn share_start_cancelled(state: &Arc<Mutex<EngineState>>) -> bool {
    state
        .lock()
        .ok()
        .and_then(|guard| guard.pending_share.clone())
        .map(|cancel| cancel.load(Ordering::Acquire))
        .unwrap_or(false)
}

fn create_session_bundle(
    state: &Arc<Mutex<EngineState>>,
    request: CreateMediaSessionRequest,
    operation: OperationFence,
    session_epoch: SessionEpoch,
    share_epoch: ShareEpoch,
    id: String,
    capabilities: MediaCapabilities,
    preview: Arc<PreviewState>,
    cancel: Arc<AtomicBool>,
    operation_barrier: Option<Arc<std::sync::Barrier>>,
    picker_barrier: Option<Arc<std::sync::Barrier>>,
) -> Result<(SessionRecord, String), MediaEngineError> {
    // This function runs outside the actor. It may read immutable capabilities
    // and use the actor ingress callback, but it never mutates EngineState.
    validate_start_config(state, &request)?;
    if let Some(barrier) = operation_barrier {
        barrier.wait();
    }
    if cancel.load(Ordering::Acquire) {
        return Err(MediaEngineError::NativeCapture(
            "session start cancelled".into(),
        ));
    }
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
    logger::log(
        "INFO",
        "session transaction",
        &format!("operation={} acquiring resources", operation.operation.0),
    );
    if cancel.load(Ordering::Acquire) {
        return Err(MediaEngineError::NativeCapture(
            "session start cancelled".into(),
        ));
    }
    let mut pipeline = NativePipeline::new(
        preview,
        request.resolution,
        request.frame_rate,
        request.quality,
        request.bitrate_bps,
        request.min_bitrate_bps,
        request.codec,
        request.encoder,
    );
    // The engine owns the Opus channel for the session lifetime. The peer
    // keeps the receiver even if the first tap fails, so a later update can
    // restart the tap against the same channel and feed the same audio track.
    let (audio_tx, audio_rx, audio_tap) = if request.system_audio {
        let (tx, rx) = sync_channel::<EncodedAudioPacket>(16);
        if request.share_on_start {
            match ProcessTap::start(&request.excluded_apps, tx.clone()) {
                Ok(tap) => (Some(tx), Some(rx), Some(tap)),
                Err(error) => {
                    eprintln!("[goDrinking] system audio tap unavailable: {error}");
                    (Some(tx), Some(rx), None)
                }
            }
        } else {
            // An unopened Share slot may keep the bounded audio channel, but
            // must not acquire the system-audio tap until Share starts.
            (Some(tx), Some(rx), None)
        }
    } else {
        (None, None, None)
    };
    let fanout = if capabilities.native_peer_transport_implemented {
        let fanout = MediaFanout::start(pipeline.take_access_unit_receiver(), audio_rx);
        // Fanout-driven IDR: per-viewer queue overflow and (re)joins force a
        // keyframe through the same coalesced flag the transport/PLI path
        // uses; the encoder consumes it in its Video arm.
        fanout.set_keyframe_control(Arc::clone(&pipeline.encoder_control));
        Some(fanout)
    } else {
        let _ = audio_rx;
        None
    };
    if let Some(barrier) = picker_barrier {
        barrier.wait();
    }
    if cancel.load(Ordering::Acquire) {
        transaction_failed(operation, "cancelled after pipeline acquisition");
        return Err(MediaEngineError::NativeCapture(
            "session start cancelled".into(),
        ));
    }
    #[cfg(target_os = "macos")]
    let mut adapter = super::screen_capture_kit::ScreenCaptureKitAdapter::new();
    #[cfg(target_os = "macos")]
    if request.share_on_start && capabilities.screen_capture_kit {
        if let Err(error) = adapter.start_capture_with_cancellation(
            &request,
            pipeline.capture_tx.clone(),
            pipeline.encoder_tx.clone(),
            pipeline.preview_diagnostics(),
            pipeline.generation,
            CaptureCancellationToken::from(Arc::clone(&cancel)),
        ) {
            transaction_failed(operation, &format!("native capture start failed: {error}"));
            return Err(MediaEngineError::NativeCapture(error.to_string()));
        }
    }
    #[cfg(target_os = "windows")]
    let mut adapter = super::windows_capture::WindowsCaptureAdapter::new();
    #[cfg(target_os = "windows")]
    if request.share_on_start && capabilities.windows_graphics_capture {
        if let Err(error) = adapter.start_capture_with_cancellation(
            &request,
            pipeline.capture_tx.clone(),
            pipeline.encoder_tx.clone(),
            pipeline.preview_diagnostics(),
            pipeline.generation,
            CaptureCancellationToken::from(Arc::clone(&cancel)),
        ) {
            transaction_failed(operation, &format!("native capture start failed: {error}"));
            return Err(MediaEngineError::NativeCapture(error));
        }
    }
    let native_capture_active = request.share_on_start
        && ((cfg!(target_os = "macos") && capabilities.screen_capture_kit)
            || (cfg!(target_os = "windows") && capabilities.windows_graphics_capture));
    if cancel.load(Ordering::Acquire) {
        let _ = stop_capture_for_rollback(&mut adapter, native_capture_active, pipeline.generation);
        transaction_failed(operation, "cancelled after capture acquisition");
        return Err(MediaEngineError::NativeCapture(
            "session start cancelled".into(),
        ));
    }
    // Broadcast does not publish a Stunar/LAN/Direct join service until the
    // provisional Share bundle has acquired capture and audio. Sala may open
    // without sharing, so its join service is also established here.
    let stunar = if request.attach_only {
        None
    } else if request.join_mode == JoinMode::Stunar {
        let base = request.rendezvous_url.as_deref().ok_or_else(|| {
            transaction_failed(operation, "missing stunar URL");
            MediaEngineError::NativePeer("Set the Stunar URL in settings.".into())
        })?;
        match StunarHost::start_inactive(
            base,
            &request.password,
            &request.nickname,
            request.admission,
            request.session_mode,
        ) {
            Ok(host) => Some(Arc::new(host)),
            Err(error) => {
                let cleanup = stop_capture_for_rollback(
                    &mut adapter,
                    native_capture_active,
                    pipeline.generation,
                );
                transaction_failed(operation, "stunar open failed");
                logger::log("ERROR", "stunar open", &error);
                if let Err(cleanup_error) = cleanup {
                    logger::log("ERROR", "session rollback", &cleanup_error);
                }
                return Err(MediaEngineError::NativePeer(error));
            }
        }
    } else {
        None
    };
    if cancel.load(Ordering::Acquire) {
        let _ = stop_capture_for_rollback(&mut adapter, native_capture_active, pipeline.generation);
        if let Some(host) = stunar.as_ref() {
            host.close();
        }
        transaction_failed(operation, "cancelled after join-service acquisition");
        return Err(MediaEngineError::NativeCapture(
            "session start cancelled".into(),
        ));
    }
    let gate = Arc::new(SessionGate::new(
        request.password.clone(),
        request.admission,
    ));
    let mint_state = Arc::clone(&state);
    let mint: ExactOfferMint = Arc::new(move |id: &str, nickname: &str| {
        let actor_tx = mint_state
            .lock()
            .map(|guard| guard.control_tx.clone())
            .map_err(|_| "media state is unavailable".to_owned())?;
        if let Some(actor_tx) = actor_tx {
            let (response_tx, response_rx) = sync_channel(1);
            actor_tx
                .send(MediaCommand::MintOffer {
                    id: id.to_owned(),
                    nickname: nickname.to_owned(),
                    origin: None,
                    response: response_tx,
                })
                .map_err(|_| "media worker is not available".to_owned())?;
            let offer = response_rx
                .recv()
                .map_err(|_| "media worker is not available".to_owned())??;
            let fence: OfferEpochFence = serde_json::from_str(&offer.offer_attempt)
                .map_err(|_| "minted offer has an invalid fence".to_owned())?;
            Ok(ExactOffer {
                signal: offer.signal,
                fence,
            })
        } else {
            mint_viewer_offer_fenced(&mint_state, id, nickname, None).map(|offer| ExactOffer {
                signal: offer.signal,
                fence: offer.fence,
            })
        }
    });
    let count_state = Arc::clone(&state);
    let viewer_count: ViewerCount = Arc::new(move || {
        count_state
            .lock()
            .ok()
            .and_then(|state| state.session.as_ref().map(|session| session.viewers.len()))
            .unwrap_or(0)
    });
    let room = if request.attach_only {
        None
    } else {
        match request.join_mode {
            JoinMode::Lan => {
                match LanRoom::start_inactive(
                    Arc::clone(&mint),
                    Arc::clone(&gate),
                    Arc::clone(&viewer_count),
                    request.nickname.clone(),
                ) {
                    Ok(room) => Some(room),
                    Err(error) => {
                        let cleanup = stop_capture_for_rollback(
                            &mut adapter,
                            native_capture_active,
                            pipeline.generation,
                        );
                        transaction_failed(operation, "LAN listener start failed");
                        if let Err(cleanup_error) = cleanup {
                            logger::log("ERROR", "session rollback", &cleanup_error);
                        }
                        if let Some(host) = stunar.as_ref() {
                            let _ = host.abort();
                        }
                        logger::log(
                            "ERROR",
                            "join service",
                            &format!("LAN listener failed: {error}"),
                        );
                        return Err(MediaEngineError::NativePeer(error));
                    }
                }
            }
            JoinMode::Direct | JoinMode::Stunar => None,
        }
    };
    let direct_room = if request.attach_only {
        None
    } else {
        match request.join_mode {
            JoinMode::Direct => {
                match DirectRoom::start_inactive(
                    mint,
                    Arc::clone(&gate),
                    viewer_count,
                    request.nickname.clone(),
                ) {
                    Ok(room) => Some(room),
                    Err(error) => {
                        let cleanup = stop_capture_for_rollback(
                            &mut adapter,
                            native_capture_active,
                            pipeline.generation,
                        );
                        transaction_failed(operation, "Direct listener start failed");
                        if let Err(cleanup_error) = cleanup {
                            logger::log("ERROR", "session rollback", &cleanup_error);
                        }
                        if let Some(host) = stunar.as_ref() {
                            let _ = host.abort();
                        }
                        logger::log(
                            "ERROR",
                            "join service",
                            &format!("Direct listener failed: {error}"),
                        );
                        return Err(MediaEngineError::NativePeer(error));
                    }
                }
            }
            JoinMode::Lan | JoinMode::Stunar => None,
        }
    };
    if cancel.load(Ordering::Acquire) {
        let _ = stop_capture_for_rollback(&mut adapter, native_capture_active, pipeline.generation);
        if let Some(host) = stunar.as_ref() {
            host.close();
        }
        transaction_failed(operation, "cancelled after listener acquisition");
        return Err(MediaEngineError::NativeCapture(
            "session start cancelled".into(),
        ));
    }
    // Join-service commit runs here in the transaction worker, never under
    // the EngineState lock. LAN/Direct activation is an atomic ingress flip;
    // Stunar activation is a blocking network call (post_commit) that must
    // not stall the actor pump (worker_loop) or snapshot polling. The Session
    // is only returned (and later published as Running) after a confirmed
    // commit, so a prepared Stunar lease stays externally unpublished until
    // the actor commit. On commit failure the lease is aborted and the
    // provisional bundle rolls back without publishing Running.
    if let Some(room) = room.as_ref() {
        room.activate();
    }
    if let Some(room) = direct_room.as_ref() {
        room.activate();
    }
    if let Some(host) = stunar.as_ref() {
        if cancel.load(Ordering::Acquire) {
            let _ =
                stop_capture_for_rollback(&mut adapter, native_capture_active, pipeline.generation);
            host.close();
            transaction_failed(operation, "cancelled before Stunar commit");
            return Err(MediaEngineError::NativeCapture(
                "session start cancelled".into(),
            ));
        }
        if let Err(error) = host.activate() {
            let _ =
                stop_capture_for_rollback(&mut adapter, native_capture_active, pipeline.generation);
            let _ = host.abort();
            logger::log("ERROR", "stunar commit", &error);
            transaction_failed(operation, "stunar commit failed");
            return Err(MediaEngineError::NativePeer(error));
        }
    }
    let audio_note = if request.system_audio && audio_tap.is_none() {
        " System audio could not start; video is still sharing."
    } else if request.system_audio {
        " System audio is captured with selected apps excluded."
    } else {
        ""
    };
    #[cfg(test)]
    let resource_counts = (
        usize::from(native_capture_active),
        usize::from(audio_tap.is_some()),
        1,
        usize::from(fanout.is_some()),
        0,
        usize::from(room.is_some())
            + usize::from(direct_room.is_some())
            + usize::from(stunar.is_some()),
    );
    let session = SessionRecord {
        id,
        generation: pipeline.generation,
        session_epoch,
        share_epoch,
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
        #[cfg(test)]
        resource_lease: state
            .lock()
            .ok()
            .and_then(|guard| guard.resource_counters.clone())
            .map(|counters| {
                ResourceLease::new(
                    counters,
                    resource_counts.0,
                    resource_counts.1,
                    resource_counts.2,
                    resource_counts.3,
                    resource_counts.4,
                    resource_counts.5,
                )
            }),
    };
    let detail = if !native_capture_active {
        "Room is open. Share your screen when you want.".into()
    } else if capabilities.native_capture_implemented {
        let share_note = match session.request.join_mode {
            JoinMode::Lan => "Share the session code on your LAN.",
            JoinMode::Direct => "Share your address and port.",
            JoinMode::Stunar => "Share the session code. Needs the relay.",
        };
        format!("Native capture is running. {share_note}{audio_note}")
    } else {
        "Control session is running; native capture is not implemented on this platform.".into()
    };
    logger::log(
        "INFO",
        "session transaction",
        &format!("operation={} resources ready", operation.operation.0),
    );
    Ok((session, format!("{detail}{audio_note}")))
}

#[cfg(test)]
fn create_in_state(
    state: &Arc<Mutex<EngineState>>,
    request: CreateMediaSessionRequest,
) -> Result<MediaSessionSnapshot, MediaEngineError> {
    validate_start_config(state, &request)?;
    crate::media::firewall::ensure_firewall_for_host(request.join_mode);
    if state
        .lock()
        .map_err(|_| MediaEngineError::StatePoisoned)?
        .session
        .is_some()
    {
        stop_in_state(state)?;
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let (operation, session_epoch, share_epoch, id, capabilities, preview) = {
        let mut guard = state.lock().map_err(|_| MediaEngineError::StatePoisoned)?;
        let session_epoch = guard.actor.begin_session();
        if request.share_on_start {
            guard.actor.begin_share();
        }
        let operation_epoch = guard.actor.fence(None);
        let operation = guard
            .actor
            .reserve_operation_kind(operation_epoch, OperationKind::StartSession);
        let capabilities = guard.capabilities.clone();
        let share_epoch = guard.actor.share_epoch;
        let id = format!("native-{}", guard.next_session_id);
        guard.next_session_id = guard.next_session_id.saturating_add(1);
        (
            operation,
            session_epoch,
            share_epoch,
            id,
            capabilities,
            Arc::clone(&guard.preview),
        )
    };
    preview.begin_session();
    let (session, detail) = create_session_bundle(
        state,
        request,
        operation,
        session_epoch,
        share_epoch,
        id,
        capabilities,
        preview,
        cancel,
        None,
        None,
    )?;
    let mut guard = state.lock().map_err(|_| MediaEngineError::StatePoisoned)?;
    guard.session = Some(session);
    guard.actor.lifecycle = MediaLifecycleState::Running;
    guard.detail = detail;
    Ok(snapshot_from_state(&guard))
}

#[cfg(test)]
fn format_resolution(resolution: VideoResolution) -> &'static str {
    match resolution {
        VideoResolution::P2160 => "2160p",
        VideoResolution::P1440 => "1440p",
        VideoResolution::P1080 => "1080p",
        VideoResolution::P720 => "720p",
        VideoResolution::P480 => "480p",
    }
}

#[cfg(test)]
fn format_frame_rate(frame_rate: FrameRate) -> &'static str {
    match frame_rate {
        FrameRate::Fps120 => "120fps",
        FrameRate::Fps60 => "60fps",
        FrameRate::Fps30 => "30fps",
    }
}

/// Replaces the complete local Share bundle.  The surrounding Session and
/// join service remain alive, but capture, audio, pipeline, fanout, and links
/// are all torn down before the fresh Share resources are published.
#[cfg(test)]
fn restart_share_slot(
    state: &Arc<Mutex<EngineState>>,
    request: CreateMediaSessionRequest,
) -> Result<MediaSessionSnapshot, MediaEngineError> {
    let (mut session, capabilities, preview, old_share) = {
        let mut guard = state.lock().map_err(|_| MediaEngineError::StatePoisoned)?;
        let session = guard
            .session
            .take()
            .ok_or(MediaEngineError::NoActiveSession)?;
        let old_share = session.native_capture_active;
        guard.actor.end_share();
        guard.actor.lifecycle = MediaLifecycleState::Starting;
        (
            session,
            guard.capabilities.clone(),
            Arc::clone(&guard.preview),
            old_share,
        )
    };
    let stunar = session.stunar.clone();
    if let Some(host) = stunar.as_ref() {
        let _ = host.send_share(false);
    }
    announce_viewer_share(state, false);

    #[cfg(target_os = "macos")]
    if old_share {
        if let Err(error) = session.adapter.stop_capture(session.generation) {
            restore_session(state, session)?;
            if let Ok(mut guard) = state.lock() {
                guard.actor.lifecycle = MediaLifecycleState::CleanupPending;
                guard.detail = format!("Share restart cleanup failed: {error}");
            }
            return Err(MediaEngineError::NativeCapture(error.to_string()));
        }
    }
    #[cfg(target_os = "windows")]
    if old_share {
        if let Err(error) = session.adapter.stop_capture() {
            restore_session(state, session)?;
            if let Ok(mut guard) = state.lock() {
                guard.actor.lifecycle = MediaLifecycleState::CleanupPending;
                guard.detail = format!("Share restart cleanup failed: {error}");
            }
            return Err(MediaEngineError::NativeCapture(error));
        }
    }
    if let Err(error) = shutdown_audio_tap(&mut session) {
        restore_session(state, session)?;
        if let Ok(mut guard) = state.lock() {
            guard.actor.lifecycle = MediaLifecycleState::CleanupPending;
            guard.detail = format!("Share restart cleanup pending: {error}");
        }
        return Err(MediaEngineError::NativeCapture(error));
    }

    let mut dropped = Vec::new();
    let ids: Vec<String> = session.viewers.keys().cloned().collect();
    if let Some(fanout) = session.fanout.as_ref() {
        for id in &ids {
            fanout.unsubscribe(id);
        }
    }
    for id in ids {
        if let Some(link) = session.viewers.remove(&id) {
            dropped.push(link);
        }
    }
    let old_fanout = session.fanout.take();
    drop(old_fanout);

    preview.begin_session();
    let pipeline = NativePipeline::new(
        Arc::clone(&preview),
        request.resolution,
        request.frame_rate,
        request.quality,
        request.bitrate_bps,
        request.min_bitrate_bps,
        request.codec,
        request.encoder,
    );
    let old_pipeline = std::mem::replace(&mut session._pipeline, pipeline);
    drop(old_pipeline);
    session.generation = session._pipeline.generation;

    let (audio_tx, audio_rx, audio_tap) = if request.system_audio {
        let (tx, rx) = sync_channel::<EncodedAudioPacket>(16);
        let tap = match ProcessTap::start(&request.excluded_apps, tx.clone()) {
            Ok(tap) => Some(tap),
            Err(error) => {
                logger::log("WARN", "share audio", &format!("tap start failed: {error}"));
                None
            }
        };
        (Some(tx), Some(rx), tap)
    } else {
        (None, None, None)
    };
    session.audio_tx = audio_tx;
    session.audio_tap = audio_tap;
    session.fanout = if capabilities.native_peer_transport_implemented {
        let fanout = MediaFanout::start(session._pipeline.take_access_unit_receiver(), audio_rx);
        // Same fanout-driven IDR wiring as session start (share restart
        // replaces both pipeline and fanout, so the new fanout needs the new
        // pipeline's flag).
        fanout.set_keyframe_control(Arc::clone(&session._pipeline.encoder_control));
        Some(fanout)
    } else {
        let _ = audio_rx;
        None
    };
    session.request = request.clone();
    session.native_capture_active = false;

    #[cfg(target_os = "macos")]
    if old_share {
        if let Err(error) = session.adapter.start_capture_with_cancellation(
            &request,
            session._pipeline.capture_tx.clone(),
            session._pipeline.encoder_tx.clone(),
            session._pipeline.preview_diagnostics(),
            session.generation,
            CaptureCancellationToken::new(),
        ) {
            session.request = request;
            restore_session(state, session)?;
            if let Ok(mut guard) = state.lock() {
                guard.actor.lifecycle = MediaLifecycleState::Running;
                guard.detail = format!("Share restart failed: {error}");
            }
            drop(dropped);
            return Err(MediaEngineError::NativeCapture(error.to_string()));
        }
    }
    #[cfg(target_os = "windows")]
    if old_share {
        if let Err(error) = session.adapter.start_capture_with_cancellation(
            &request,
            session._pipeline.capture_tx.clone(),
            session._pipeline.encoder_tx.clone(),
            session._pipeline.preview_diagnostics(),
            session.generation,
            CaptureCancellationToken::new(),
        ) {
            restore_session(state, session)?;
            if let Ok(mut guard) = state.lock() {
                guard.actor.lifecycle = MediaLifecycleState::Running;
                guard.detail = format!("Share restart failed: {error}");
            }
            drop(dropped);
            return Err(MediaEngineError::NativeCapture(error));
        }
    }
    if share_start_cancelled(state) {
        #[cfg(target_os = "macos")]
        if old_share {
            let _ = session.adapter.stop_capture(session.generation);
        }
        #[cfg(target_os = "windows")]
        if old_share {
            let _ = session.adapter.stop_capture();
        }
        let _ = shutdown_audio_tap(&mut session);
        session.native_capture_active = false;
        restore_session(state, session)?;
        drop(dropped);
        return Err(MediaEngineError::NativeCapture(
            "share restart cancelled".into(),
        ));
    }
    session.native_capture_active = old_share;
    {
        let mut guard = state.lock().map_err(|_| MediaEngineError::StatePoisoned)?;
        guard.actor.lifecycle = MediaLifecycleState::Running;
        if session.native_capture_active {
            session.share_epoch = guard.actor.begin_share();
        }
        guard.session = Some(session);
    }
    drop(dropped);
    if let Some(host) = stunar.as_ref() {
        let _ = host.send_share(old_share);
    }
    announce_viewer_share(state, old_share);
    let guard = state.lock().map_err(|_| MediaEngineError::StatePoisoned)?;
    Ok(snapshot_from_state(&guard))
}

/// Applies live settings to the active session. Capture, the room, and the
/// WebRTC peer are never torn down: quality is a live bitrate/keyframe update
/// and audio changes recreate only the process tap against the engine-owned
/// Opus channel.
#[cfg(test)]
fn update_in_state(
    state: &Arc<Mutex<EngineState>>,
    request: UpdateMediaSessionRequest,
) -> Result<MediaSessionSnapshot, MediaEngineError> {
    let restart_request = {
        let guard = state.lock().map_err(|_| MediaEngineError::StatePoisoned)?;
        let session = guard
            .session
            .as_ref()
            .ok_or(MediaEngineError::NoActiveSession)?;
        let resolution = request.resolution.unwrap_or(session.request.resolution);
        let frame_rate = request.frame_rate.unwrap_or(session.request.frame_rate);
        if session.native_capture_active
            && (resolution != session.request.resolution
                || frame_rate != session.request.frame_rate)
        {
            let mut updated = session.request.clone();
            updated.resolution = resolution;
            updated.frame_rate = frame_rate;
            updated.quality = request.quality;
            updated.bitrate_bps = request.bitrate_bps;
            updated.min_bitrate_bps = request.min_bitrate_bps;
            updated.system_audio = request.system_audio;
            updated.excluded_apps = request.excluded_apps.clone();
            Some(updated)
        } else {
            None
        }
    };
    if let Some(restart_request) = restart_request {
        return restart_share_slot(state, restart_request);
    }
    let mut state = state.lock().map_err(|_| MediaEngineError::StatePoisoned)?;
    let Some(session) = state.session.as_mut() else {
        return Err(MediaEngineError::NoActiveSession);
    };

    // 1. Quality/bitrate: live bitrate + keyframe. Resolution/frame_rate
    // are fixed at Start (like codec/encoder) and never rewritten here,
    // so an explicit choice survives live bitrate tweaks. An explicit
    // bitrate override wins over the preset for the encoder target.
    let target_changed = session.request.quality != request.quality
        || session.request.bitrate_bps != request.bitrate_bps;
    let target = super::types::resolve_bitrate(request.quality, request.bitrate_bps);
    let floor = super::types::resolve_floor(target, request.min_bitrate_bps);
    if target_changed {
        let _ = session._pipeline.set_bitrate(target);
        let _ = session._pipeline.force_keyframe();
        session.request.quality = request.quality;
        session.request.bitrate_bps = request.bitrate_bps;
    }
    if session.request.min_bitrate_bps != request.min_bitrate_bps {
        // A raised floor re-asserts the encoder immediately so a collapsed
        // stream recovers without waiting for the next REMB.
        let _ = session._pipeline.set_floor(floor);
        session.request.min_bitrate_bps = request.min_bitrate_bps;
    }

    // 1b. A stopped Share has no capture to restart. Store the requested
    // dimensions for the next Share start; an active Share took the complete
    // restart path above.
    let mut reconfig_note = String::new();
    let new_resolution = request.resolution.unwrap_or(session.request.resolution);
    let new_frame_rate = request.frame_rate.unwrap_or(session.request.frame_rate);
    if new_resolution != session.request.resolution || new_frame_rate != session.request.frame_rate
    {
        session.request.resolution = new_resolution;
        session.request.frame_rate = new_frame_rate;
        reconfig_note = format!(
            " Share is stopped; next start will use {} {}.",
            format_resolution(new_resolution),
            format_frame_rate(new_frame_rate)
        );
    }

    // 2. Audio: recreate only the process tap. The peer keeps its original
    // receiver, so a restarted tap feeds the same audio track.
    let mut audio_note = String::new();
    if request.system_audio {
        if session.native_capture_active {
            if let Some(tx) = session.audio_tx.clone() {
                if let Err(error) = shutdown_audio_tap(&mut *session) {
                    return Err(MediaEngineError::NativeCapture(error));
                }
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
        } else if session.audio_tx.is_none() {
            audio_note =
                " System audio cannot be added mid-session; restart the session to enable it."
                    .into();
        } else {
            if let Err(error) = shutdown_audio_tap(&mut *session) {
                return Err(MediaEngineError::NativeCapture(error));
            }
        }
    } else {
        // Silence: drop the tap. The peer keeps its (now silent) audio track.
        if let Err(error) = shutdown_audio_tap(&mut *session) {
            return Err(MediaEngineError::NativeCapture(error));
        }
    }
    session.request.system_audio = request.system_audio;
    session.request.excluded_apps = request.excluded_apps;

    let mut detail =
        "Session settings updated; capture and peer transport kept running.".to_string();
    detail.push_str(&reconfig_note);
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

fn start_share_bundle(
    mut session: SessionRecord,
    capabilities: MediaCapabilities,
    preview: Arc<PreviewState>,
    operation: OperationFence,
    cancel: Arc<AtomicBool>,
) -> Result<(SessionRecord, String), (SessionRecord, MediaEngineError)> {
    if session.native_capture_active {
        return Ok((session, "Share is already running.".into()));
    }
    let request = session.request.clone();
    // A stopped Share owns no running pipeline/fanout. Reconstruct both here,
    // at the next real Share start, before capture is acquired.
    let pipeline_status = session._pipeline.shutdown_and_join(Duration::from_secs(3));
    if !pipeline_status.quiesced || !pipeline_status.errors.is_empty() {
        return Err((
            session,
            MediaEngineError::NativeCapture("Share pipeline cleanup is pending".into()),
        ));
    }
    if let Some(mut fanout) = session.fanout.take() {
        let status = fanout.shutdown_and_join(Duration::from_secs(3));
        if !status.quiesced || !status.errors.is_empty() {
            session.fanout = Some(fanout);
            return Err((
                session,
                MediaEngineError::NativePeer("Share fanout cleanup is pending".into()),
            ));
        }
    }
    if let Err(error) = shutdown_audio_tap(&mut session) {
        return Err((session, MediaEngineError::NativeCapture(error)));
    }
    let pipeline = NativePipeline::new(
        preview,
        request.resolution,
        request.frame_rate,
        request.quality,
        request.bitrate_bps,
        request.min_bitrate_bps,
        request.codec,
        request.encoder,
    );
    let old_pipeline = std::mem::replace(&mut session._pipeline, pipeline);
    drop(old_pipeline);
    let (audio_tx, audio_rx, audio_tap) = if request.system_audio {
        let (tx, rx) = sync_channel::<EncodedAudioPacket>(16);
        let tap = ProcessTap::start(&request.excluded_apps, tx.clone()).ok();
        (Some(tx), Some(rx), tap)
    } else {
        (None, None, None)
    };
    session.audio_tx = audio_tx;
    session.audio_tap = audio_tap;
    session.fanout = if capabilities.native_peer_transport_implemented {
        let fanout = MediaFanout::start(session._pipeline.take_access_unit_receiver(), audio_rx);
        // Same fanout-driven IDR wiring as session start.
        fanout.set_keyframe_control(Arc::clone(&session._pipeline.encoder_control));
        Some(fanout)
    } else {
        let _ = audio_rx;
        None
    };
    let generation = session._pipeline.generation;
    #[cfg(target_os = "macos")]
    if capabilities.screen_capture_kit {
        if let Err(error) = session.adapter.start_capture_with_cancellation(
            &request,
            session._pipeline.capture_tx.clone(),
            session._pipeline.encoder_tx.clone(),
            session._pipeline.preview_diagnostics(),
            generation,
            CaptureCancellationToken::from(Arc::clone(&cancel)),
        ) {
            return Err((session, MediaEngineError::NativeCapture(error.to_string())));
        }
    }
    #[cfg(target_os = "windows")]
    if capabilities.windows_graphics_capture {
        if let Err(error) = session.adapter.start_capture_with_cancellation(
            &request,
            session._pipeline.capture_tx.clone(),
            session._pipeline.encoder_tx.clone(),
            session._pipeline.preview_diagnostics(),
            generation,
            CaptureCancellationToken::from(Arc::clone(&cancel)),
        ) {
            return Err((session, MediaEngineError::NativeCapture(error)));
        }
    }
    let active = (cfg!(target_os = "macos") && capabilities.screen_capture_kit)
        || (cfg!(target_os = "windows") && capabilities.windows_graphics_capture);
    if cancel.load(Ordering::Acquire) {
        let _ = stop_capture_for_rollback(&mut session.adapter, active, generation);
        session.native_capture_active = false;
        return Err((
            session,
            MediaEngineError::NativeCapture("share start cancelled".into()),
        ));
    }
    if active && request.system_audio && session.audio_tap.is_none() {
        if let Some(audio_tx) = session.audio_tx.clone() {
            match ProcessTap::start(&request.excluded_apps, audio_tx) {
                Ok(tap) => session.audio_tap = Some(tap),
                Err(error) => {
                    eprintln!("[goDrinking] system audio tap unavailable: {error}");
                }
            }
        }
    }
    if cancel.load(Ordering::Acquire) {
        let _ = stop_capture_for_rollback(&mut session.adapter, active, generation);
        let _ = shutdown_audio_tap(&mut session);
        session.native_capture_active = false;
        return Err((
            session,
            MediaEngineError::NativeCapture("share start cancelled".into()),
        ));
    }
    session.native_capture_active = active;
    session.share_epoch = operation.epoch.share;
    let detail = if active {
        "Native capture is running.".into()
    } else {
        "Share slot is open; native capture is unavailable.".into()
    };
    Ok((session, detail))
}

fn stop_share_bundle(
    mut session: SessionRecord,
) -> Result<SessionRecord, (SessionRecord, MediaEngineError)> {
    let mut ledger = CleanupLedger::new();
    #[cfg(target_os = "macos")]
    if session.native_capture_active {
        if let Err(error) = session.adapter.stop_capture(session.generation) {
            ledger.error(format!("capture: {error}"));
        }
    }
    #[cfg(target_os = "windows")]
    if session.native_capture_active {
        if let Err(error) = session.adapter.stop_capture() {
            ledger.error(format!("capture: {error}"));
        }
    }
    if let Some(mut tap) = session.audio_tap.take() {
        let status = tap.shutdown_and_join(Duration::from_secs(3));
        ledger.status("audio tap", status.quiesced, status.pending, status.errors);
        if !status.quiesced {
            session.audio_tap = Some(tap);
        }
    }
    let ids: Vec<String> = session.viewers.keys().cloned().collect();
    if let Some(fanout) = session.fanout.as_ref() {
        for id in &ids {
            fanout.unsubscribe(id);
        }
    }
    for id in ids {
        if let Some(mut link) = session.viewers.remove(&id) {
            let status = link.peer.shutdown_and_join(Duration::from_secs(3));
            ledger.status("peer", status.quiesced, status.pending, status.errors);
            if !status.quiesced {
                session.viewers.insert(id, link);
            }
        }
    }
    if let Some(mut fanout) = session.fanout.take() {
        let status = fanout.shutdown_and_join(Duration::from_secs(3));
        ledger.status("fanout", status.quiesced, status.pending, status.errors);
        if !status.quiesced {
            session.fanout = Some(fanout);
        }
    }
    let pipeline_status = session._pipeline.shutdown_and_join(Duration::from_secs(3));
    ledger.status(
        "pipeline",
        pipeline_status.quiesced,
        pipeline_status.pending,
        pipeline_status.errors,
    );
    if !ledger.quiesced || !ledger.errors.is_empty() || !session.viewers.is_empty() {
        return Err((
            session,
            MediaEngineError::NativeCapture(format!("Share cleanup pending: {}", ledger.summary())),
        ));
    }

    session.native_capture_active = false;
    session.audio_tx = None;
    Ok(session)
}

#[cfg(test)]
fn start_share_in_state(
    state: &Arc<Mutex<EngineState>>,
) -> Result<MediaSessionSnapshot, MediaEngineError> {
    // Take the session out of the lock: the macOS picker blocks until the
    // user picks a display, and snapshot()/IPC must keep answering.
    if share_start_cancelled(state) {
        return Err(MediaEngineError::NativeCapture(
            "share start cancelled".into(),
        ));
    }
    let (mut session, capabilities) = {
        let mut guard = state.lock().map_err(|_| MediaEngineError::StatePoisoned)?;
        let session = guard
            .session
            .take()
            .ok_or(MediaEngineError::NoActiveSession)?;
        (session, guard.capabilities.clone())
    };
    if share_start_cancelled(state) {
        restore_session(state, session)?;
        return Err(MediaEngineError::NativeCapture(
            "share start cancelled".into(),
        ));
    }
    if session.native_capture_active {
        if let Some(host) = session.stunar.as_ref() {
            let _ = host.send_share(true);
        }
        restore_session(state, session)?;
        announce_viewer_share(state, true);
        let guard = state.lock().map_err(|_| MediaEngineError::StatePoisoned)?;
        return Ok(snapshot_from_state(&guard));
    }
    let request = session.request.clone();
    let capture_tx = session._pipeline.capture_tx.clone();
    let encoder_tx = session._pipeline.encoder_tx.clone();
    let diagnostics = session._pipeline.preview_diagnostics();
    let generation = session.generation;
    #[cfg(target_os = "macos")]
    if capabilities.screen_capture_kit {
        if let Err(error) =
            session
                .adapter
                .start_capture(&request, capture_tx, encoder_tx, diagnostics, generation)
        {
            restore_session(state, session)?;
            return Err(MediaEngineError::NativeCapture(error.to_string()));
        }
    }
    #[cfg(target_os = "windows")]
    if capabilities.windows_graphics_capture {
        if let Err(error) =
            session
                .adapter
                .start_capture(&request, capture_tx, encoder_tx, diagnostics, generation)
        {
            restore_session(state, session)?;
            return Err(MediaEngineError::NativeCapture(error));
        }
    }
    if share_start_cancelled(state) {
        #[cfg(target_os = "macos")]
        if capabilities.screen_capture_kit {
            let _ = session.adapter.stop_capture(session.generation);
        }
        #[cfg(target_os = "windows")]
        if capabilities.windows_graphics_capture {
            let _ = session.adapter.stop_capture();
        }
        session.native_capture_active = false;
        restore_session(state, session)?;
        return Err(MediaEngineError::NativeCapture(
            "share start cancelled".into(),
        ));
    }
    session.native_capture_active = (cfg!(target_os = "macos") && capabilities.screen_capture_kit)
        || (cfg!(target_os = "windows") && capabilities.windows_graphics_capture);
    if session.request.system_audio && session.audio_tap.is_none() {
        if let Some(tx) = session.audio_tx.clone() {
            match ProcessTap::start(&session.request.excluded_apps, tx) {
                Ok(tap) => session.audio_tap = Some(tap),
                Err(error) => logger::log(
                    "WARN",
                    "share audio",
                    &format!("system audio tap unavailable: {error}"),
                ),
            }
        }
    }
    if share_start_cancelled(state) {
        let _ = shutdown_audio_tap(&mut session);
        #[cfg(target_os = "macos")]
        if capabilities.screen_capture_kit {
            let _ = session.adapter.stop_capture(session.generation);
        }
        #[cfg(target_os = "windows")]
        if capabilities.windows_graphics_capture {
            let _ = session.adapter.stop_capture();
        }
        session.native_capture_active = false;
        restore_session(state, session)?;
        return Err(MediaEngineError::NativeCapture(
            "share start cancelled".into(),
        ));
    }
    if session.native_capture_active {
        let mut guard = state.lock().map_err(|_| MediaEngineError::StatePoisoned)?;
        session.share_epoch = guard.actor.begin_share();
    }
    if let Some(host) = session.stunar.as_ref() {
        let _ = host.send_share(true);
    }
    restore_session(state, session)?;
    announce_viewer_share(state, true);
    let guard = state.lock().map_err(|_| MediaEngineError::StatePoisoned)?;
    Ok(snapshot_from_state(&guard))
}

#[cfg(test)]
fn restore_session(
    state: &Arc<Mutex<EngineState>>,
    session: SessionRecord,
) -> Result<(), MediaEngineError> {
    let mut guard = state.lock().map_err(|_| MediaEngineError::StatePoisoned)?;
    guard.session = Some(session);
    Ok(())
}

#[cfg(test)]
fn stop_share_in_state(
    state: &Arc<Mutex<EngineState>>,
) -> Result<MediaSessionSnapshot, MediaEngineError> {
    let mut session = {
        let mut guard = state.lock().map_err(|_| MediaEngineError::StatePoisoned)?;
        guard
            .session
            .take()
            .ok_or(MediaEngineError::NoActiveSession)?
    };
    #[cfg(target_os = "macos")]
    if session.native_capture_active {
        if let Err(error) = session.adapter.stop_capture(session.generation) {
            let _ = shutdown_audio_tap(&mut session);
            restore_session(state, session)?;
            if let Ok(mut guard) = state.lock() {
                guard.actor.lifecycle = MediaLifecycleState::CleanupPending;
                guard.detail = format!("Share cleanup pending: {error}");
            }
            return Err(MediaEngineError::NativeCapture(error.to_string()));
        }
    }
    #[cfg(target_os = "windows")]
    if session.native_capture_active {
        if let Err(error) = session.adapter.stop_capture() {
            let _ = shutdown_audio_tap(&mut session);
            restore_session(state, session)?;
            if let Ok(mut guard) = state.lock() {
                guard.actor.lifecycle = MediaLifecycleState::CleanupPending;
                guard.detail = format!("Share cleanup pending: {error}");
            }
            return Err(MediaEngineError::NativeCapture(error));
        }
    }
    session.native_capture_active = false;
    // A stopped Share owns no system-audio tap.  Drop it before restoring the
    // room record; ProcessTap joins its worker in Drop.
    if let Err(error) = shutdown_audio_tap(&mut session) {
        restore_session(state, session)?;
        if let Ok(mut guard) = state.lock() {
            guard.actor.lifecycle = MediaLifecycleState::CleanupPending;
            guard.detail = format!("Share cleanup pending: {error}");
        }
        return Err(MediaEngineError::NativeCapture(error));
    }
    {
        let mut guard = state.lock().map_err(|_| MediaEngineError::StatePoisoned)?;
        guard.actor.end_share();
    }
    let mut dropped = Vec::new();
    let ids: Vec<String> = session.viewers.keys().cloned().collect();
    for id in ids {
        if let Some(fanout) = session.fanout.as_ref() {
            fanout.unsubscribe(&id);
        }
        if let Some(link) = session.viewers.remove(&id) {
            // The link is detached while the actor owns the roster. Its Drop
            // runs after this function releases the state lock.
            dropped.push(link);
        }
    }
    if let Some(host) = session.stunar.as_ref() {
        let _ = host.send_share(false);
    }
    restore_session(state, session)?;
    drop(dropped);
    announce_viewer_share(state, false);
    let guard = state.lock().map_err(|_| MediaEngineError::StatePoisoned)?;
    Ok(snapshot_from_state(&guard))
}

fn announce_viewer_share(state: &Arc<Mutex<EngineState>>, start: bool) {
    if let Ok(guard) = state.lock() {
        if let Some(viewer) = guard.stunar_viewer.as_ref() {
            let _ = viewer.send_share(start);
        }
    }
}

#[cfg(test)]
fn stop_in_state(
    state: &Arc<Mutex<EngineState>>,
) -> Result<MediaSessionSnapshot, MediaEngineError> {
    logger::log("INFO", "session", "stop");
    let mut session = {
        let mut state = state.lock().map_err(|_| MediaEngineError::StatePoisoned)?;
        if state.session.is_none() {
            return Err(MediaEngineError::NoActiveSession);
        }
        if state.actor.lifecycle != MediaLifecycleState::Stopping {
            state.actor.invalidate_session();
        }
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
            state.actor.lifecycle = match native_lifecycle {
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
            state.actor.lifecycle = MediaLifecycleState::Running;
            state.detail = format!("Native capture stop failed and is retryable: {error}");
        }
        return Err(MediaEngineError::NativeCapture(error));
    }

    let mut state = state.lock().map_err(|_| MediaEngineError::StatePoisoned)?;
    state.actor.lifecycle = MediaLifecycleState::Idle;
    state.detail = "Session stopped; native pipeline handles released.".into();
    state.preview.begin_session();
    Ok(snapshot_from_state(&state))
}

fn merge_viewer_roster(roster: &mut Vec<RosterEntry>, viewer: &super::rendezvous::StunarViewer) {
    for (id, nickname, master, share) in viewer.room_roster() {
        if let Some(entry) = roster.iter_mut().find(|entry| entry.id == id) {
            entry.master = master;
            entry.share = share;
        } else {
            roster.push(RosterEntry {
                id,
                nickname,
                state: if share {
                    PeerTransportState::Connected
                } else {
                    PeerTransportState::New
                },
                master,
                share,
            });
        }
    }
}

fn snapshot_from_state(state: &EngineState) -> MediaSessionSnapshot {
    let Some(session) = state.session.as_ref() else {
        if let Some(snapshot) = state.transition_snapshot.as_ref() {
            return snapshot.clone();
        }
        if let Some(viewer) = state.stunar_viewer.as_ref() {
            let mut roster = Vec::new();
            merge_viewer_roster(&mut roster, viewer);
            let mut snap = MediaSessionSnapshot::idle(state.detail.clone());
            snap.state = MediaLifecycleState::Running;
            snap.detail = state.detail.clone();
            snap.roster = roster;
            snap.self_id = viewer.member_id.clone();
            snap.session_mode = if viewer.mode == "room" {
                super::room_mode::SessionMode::Room
            } else {
                super::room_mode::SessionMode::Broadcast
            };
            snap.join_mode = JoinMode::Stunar;
            return snap;
        }
        return MediaSessionSnapshot {
            state: state.actor.lifecycle,
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
                master: false,
                share: false,
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
            master: false,
            share: false,
        });
    }
    // Stunar pending Viewers live on the Rendezvous, not in the gate.
    if let Some(stunar) = session.stunar.as_ref() {
        for (id, nickname) in stunar.pending_roster() {
            roster.push(RosterEntry {
                id,
                nickname,
                state: PeerTransportState::Pending,
                master: false,
                share: false,
            });
        }
        for (id, nickname, master, share) in stunar.room_roster() {
            if let Some(entry) = roster.iter_mut().find(|entry| entry.id == id) {
                entry.master = master;
                entry.share = share;
            } else {
                roster.push(RosterEntry {
                    id,
                    nickname,
                    state: if share {
                        PeerTransportState::Connected
                    } else {
                        PeerTransportState::New
                    },
                    master,
                    share,
                });
            }
        }
    }
    if let Some(viewer) = state.stunar_viewer.as_ref() {
        merge_viewer_roster(&mut roster, viewer);
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
            state.actor.lifecycle
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
        self_id: session
            .stunar
            .as_ref()
            .and_then(|host| host.self_id.clone())
            .or_else(|| {
                state
                    .stunar_viewer
                    .as_ref()
                    .and_then(|viewer| viewer.member_id.clone())
            }),
        password_set: session.gate.password_set(),
        admission: session.gate.admission(),
        join_mode: session.request.join_mode,
        session_mode: session.request.session_mode,
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
            state.actor.lifecycle = MediaLifecycleState::Failed;
            state.detail = detail;
            // A dead encoder with live capture streams black to connected
            // viewers while burning CPU: stop capture so the failure is a
            // visible Failed state, never a silent black room.
            #[cfg(target_os = "macos")]
            let _ = session.adapter.stop_capture(session.generation);
            #[cfg(not(target_os = "macos"))]
            let _ = session.adapter.stop_capture();
        }
    }
    #[cfg(target_os = "macos")]
    if let Some(session) = state.session.as_ref() {
        if session.adapter.lifecycle() == super::screen_capture_kit::CaptureLifecycle::Failed {
            state.actor.lifecycle = MediaLifecycleState::Failed;
            if let Some(detail) = session.adapter.failure_detail() {
                state.detail = detail;
            }
        } else if session.adapter.lifecycle()
            == super::screen_capture_kit::CaptureLifecycle::CleanupPending
        {
            state.actor.lifecycle = MediaLifecycleState::CleanupPending;
            if let Some(detail) = session.adapter.failure_detail() {
                state.detail = detail;
            }
        }
    }
}

/// Detach failed Viewers while synchronized, then let `PeerTransport::Drop`
/// stop/join its workers after the state lock has been released.
fn reap_failed_links(state: &Arc<Mutex<EngineState>>) {
    let dropped = {
        let Ok(mut guard) = state.lock() else { return };
        let Some(session) = guard.session.as_mut() else {
            return;
        };
        let failed: Vec<String> = session
            .viewers
            .iter()
            .filter(|(_, viewer)| viewer.peer.status().state == PeerTransportState::Failed)
            .map(|(id, _)| id.clone())
            .collect();
        let mut dropped = Vec::new();
        let mut retired = Vec::new();
        for id in failed {
            if let Some(fanout) = session.fanout.as_ref() {
                fanout.unsubscribe(&id);
            }
            if let Some(link) = session.viewers.remove(&id) {
                let link_session = link.fence.session.0;
                retired.push(link.link_id);
                logger::log(
                    "WARN",
                    "peer failed",
                    &format!("viewer={id} session_epoch={link_session} isolated"),
                );
                dropped.push(link);
            }
        }
        for link_id in retired {
            guard.actor.retire_link(link_id);
        }
        dropped
    };
    drop(dropped);
}

fn mint_viewer_offer(
    state: &Arc<Mutex<EngineState>>,
    id: &str,
    nickname: &str,
    origin: Option<EpochFence>,
) -> Result<PeerSignal, String> {
    mint_viewer_offer_fenced(state, id, nickname, origin).map(|minted| minted.signal)
}

fn mint_viewer_offer_fenced(
    state: &Arc<Mutex<EngineState>>,
    id: &str,
    nickname: &str,
    origin: Option<EpochFence>,
) -> Result<MintedOffer, String> {
    logger::log(
        "INFO",
        "mint offer",
        &format!("viewer={id} nickname={nickname}"),
    );
    let (
        video_rx,
        audio_rx,
        encoder_control,
        frame_duration,
        join_mode,
        video_codec,
        fence,
        offer_fence,
        link_id,
    ) = {
        let mut guard = state
            .lock()
            .map_err(|_| "media state is unavailable".to_owned())?;
        if guard.session.is_none() {
            return Err("no media session is active".to_owned());
        }
        if let Some(origin) = origin {
            let current = EpochFence {
                session: guard.actor.session_epoch,
                share: guard.actor.share_epoch,
                link: None,
            };
            if current != origin {
                guard.actor.discard("room-offer", origin);
                return Err("stale room offer".into());
            }
        }
        let session = guard
            .session
            .as_ref()
            .ok_or_else(|| "no media session is active".to_owned())?;
        if session.viewers.len() >= MAX_VIEWERS {
            logger::log("WARN", "mint offer", "session is full (8 viewers)");
            return Err("session is full".into());
        }
        if session.viewers.contains_key(id) {
            return Err("viewer already has a link".into());
        }
        let link_id = guard.actor.begin_link();
        let fence = guard.actor.fence(Some(link_id));
        let offer_fence = guard
            .actor
            .begin_offer_attempt(link_id)
            .ok_or_else(|| "could not allocate offer attempt".to_owned())?;
        let session = guard
            .session
            .as_mut()
            .ok_or_else(|| "no media session is active".to_owned())?;
        let fanout = session
            .fanout
            .as_ref()
            .ok_or_else(|| "peer transport is unavailable".to_owned())?;
        let (video_rx, audio_rx) = fanout.subscribe(id);
        let frame_rate = u64::from(session.request.effective_frame_rate().hertz());
        (
            video_rx,
            audio_rx,
            Arc::clone(&session._pipeline.encoder_control),
            Duration::from_nanos(1_000_000_000 / frame_rate),
            session.request.join_mode,
            session.request.codec,
            fence,
            offer_fence,
            link_id,
        )
    };
    let peer = match PeerTransport::new_with_initialization(
        video_rx,
        audio_rx,
        Arc::clone(&encoder_control),
        frame_duration,
        join_mode,
        video_codec,
    ) {
        Ok(peer) => peer,
        Err(PeerTransportInitError::Failed(error)) => {
            unsubscribe_fanout(state, id, fence);
            retire_offer_attempt(state, offer_fence.attempt);
            retire_link(state, link_id);
            return Err(error);
        }
        Err(PeerTransportInitError::Pending(pending)) => {
            unsubscribe_fanout(state, id, fence);
            retire_offer_attempt(state, offer_fence.attempt);
            retire_link(state, link_id);
            if let Ok(mut guard) = state.lock() {
                guard.pending_peer_cleanup.push(pending);
            }
            return Err("WebRTC peer initialization cleanup is pending".into());
        }
    };
    let mut signal = match peer.client().create_offer() {
        Ok(signal) => signal,
        Err(error) => {
            drop(peer);
            unsubscribe_fanout(state, id, fence);
            retire_offer_attempt(state, offer_fence.attempt);
            retire_link(state, link_id);
            return Err(error.to_string());
        }
    };
    signal.id = Some(id.to_owned());
    let mut guard = state
        .lock()
        .map_err(|_| "media state is unavailable".to_owned())?;
    let actor_accepts = guard.actor.accepts_offer(offer_fence);
    let stale = match guard.session.as_ref() {
        Some(session) => {
            session.viewers.len() >= MAX_VIEWERS
                || session.session_epoch != fence.session
                || session.share_epoch != fence.share
                || !actor_accepts
        }
        None => true,
    };
    if stale {
        guard.actor.discard("peer-create", fence);
        guard.actor.retire_offer_attempt(offer_fence.attempt);
        guard.actor.retire_link(link_id);
        if let Some(session) = guard.session.as_ref() {
            if session.session_epoch == fence.session && session.share_epoch == fence.share {
                if let Some(fanout) = session.fanout.as_ref() {
                    fanout.unsubscribe(id);
                }
            }
        }
        logger::log("WARN", "mint offer", "discarded stale or full viewer offer");
        drop(guard);
        drop(peer);
        return Err("viewer offer became stale or session is full".into());
    }
    let session = guard
        .session
        .as_mut()
        .ok_or_else(|| "no media session is active".to_owned())?;
    session.viewers.insert(
        id.to_owned(),
        ViewerLink {
            id: id.to_owned(),
            nickname: nickname.to_owned(),
            peer,
            last_offer: Some(signal.clone()),
            offered_at: Instant::now(),
            offer_resends: 0,
            fence,
            offer_fence,
            link_id,
        },
    );
    // A new viewer must get SPS/PPS + IDR immediately: on static screens the
    // encoder emits mostly SKIP frames, so without a forced keyframe the
    // viewer would wait (up to the intra period) for decodable data.
    encoder_control.request_keyframe();
    Ok(MintedOffer {
        fence: offer_fence,
        signal,
    })
}

/// Best-effort fanout cleanup for a mint that failed after subscribing
/// (PeerTransport::new / create_offer error): without it every failed join
/// attempt leaks a dead per-viewer queue.
fn unsubscribe_fanout(state: &Arc<Mutex<EngineState>>, id: &str, fence: EpochFence) {
    if let Ok(guard) = state.lock() {
        if let Some(session) = guard.session.as_ref() {
            if session.session_epoch == fence.session && session.share_epoch == fence.share {
                if let Some(fanout) = session.fanout.as_ref() {
                    fanout.unsubscribe(id);
                }
            }
        }
    }
}

fn retire_link(state: &Arc<Mutex<EngineState>>, link_id: LinkId) {
    if let Ok(mut guard) = state.lock() {
        guard.actor.retire_link(link_id);
    }
}

fn retire_offer_attempt(
    state: &Arc<Mutex<EngineState>>,
    attempt: super::control_plane::OfferAttemptId,
) {
    if let Ok(mut guard) = state.lock() {
        guard.actor.retire_offer_attempt(attempt);
    }
}

#[cfg(test)]
mod tests {
    use super::super::capabilities::AppAudioExclusionSupport;
    use super::super::capabilities::MediaCapabilities;
    use super::super::control_plane::SessionActor;
    use super::super::peer_transport::{PeerSignal, PeerSignalKind};
    use super::super::pipeline::PreviewState;
    use super::super::types::MediaLifecycleState;
    use super::super::types::{
        CaptureSource, FrameRate, JoinMode, PreviewFrameEvent, SourceIdUpdate, TransmissionQuality,
        UpdateCredentialsRequest, UpdateMediaSessionRequest, VideoCodec, VideoEncoder,
        VideoResolution,
    };
    use super::{
        create_in_state, refresh_native_state, snapshot_from_state, start_share_in_state,
        stop_in_state, update_credentials_in_state, update_in_state, worker_loop, EngineState,
        EpochFence, MediaEngine, MediaEngineError, ResourceCounters, CONTROL_QUEUE_CAPACITY,
    };
    use std::sync::mpsc::sync_channel;
    use std::sync::{Arc, Barrier, Mutex};
    use std::thread;
    use std::time::Duration;

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
                native_peer_transport_implemented: true,
                av1_encode_supported: false,
                detail: "test".into(),
            },
            actor: SessionActor::new(),
            session: None,
            next_session_id: 1,
            detail: "idle".into(),
            preview: Arc::new(PreviewState::new()),
            stunar_viewer: None,
            control_tx: None,
            pending_start: None,
            pending_share: None,
            pending_start_operation: None,
            pending_share_operation: None,
            pending_stop_operation: None,
            transition_snapshot: None,
            operation_barrier: None,
            picker_barrier: None,
            resource_counters: None,
            stop_waiters: Vec::new(),
            pending_peer_cleanup: Vec::new(),
        }))
    }

    pub(crate) fn worker_engine(
        operation_barrier: Option<Arc<Barrier>>,
        picker_barrier: Option<Arc<Barrier>>,
    ) -> MediaEngine {
        worker_engine_with_counters(operation_barrier, picker_barrier, None)
    }

    pub(crate) fn worker_engine_with_counters(
        operation_barrier: Option<Arc<Barrier>>,
        picker_barrier: Option<Arc<Barrier>>,
        resource_counters: Option<Arc<ResourceCounters>>,
    ) -> MediaEngine {
        let state = test_state();
        if let Ok(mut guard) = state.lock() {
            guard.operation_barrier = operation_barrier;
            guard.picker_barrier = picker_barrier;
            guard.resource_counters = resource_counters;
        }
        let (control_tx, control_rx) = sync_channel(CONTROL_QUEUE_CAPACITY);
        state.lock().expect("test state").control_tx = Some(control_tx.clone());
        let worker_state = Arc::clone(&state);
        let worker_tx = control_tx.clone();
        thread::Builder::new()
            .name("godrinking-test-media-control".into())
            .spawn(move || worker_loop(control_rx, worker_state, worker_tx))
            .expect("test media worker should start");
        MediaEngine { control_tx, state }
    }

    pub(crate) fn wait_for_state(engine: &MediaEngine, expected: MediaLifecycleState) {
        // Generous budget for loaded runners: the full suite spawns hundreds
        // of worker threads in parallel, and a fixed 2s budget flakes the
        // stop-during-acquisition test when the actor thread is descheduled.
        // The assertions stay exact (state + zero-resource ledger), so a
        // longer wait cannot fake a pass; it only delays a real failure.
        for _ in 0..3_000 {
            if engine.snapshot().state == expected {
                return;
            }
            thread::sleep(Duration::from_millis(2));
        }
        panic!("media worker did not reach {expected:?}");
    }

    #[test]
    fn public_worker_stop_during_start_waits_for_stale_completion() {
        let barrier = Arc::new(Barrier::new(2));
        let engine = worker_engine(Some(Arc::clone(&barrier)), None);
        let mut request = request();
        request.share_on_start = false;
        let starter = engine.clone();
        let create = thread::spawn(move || starter.create_session(request));
        wait_for_state(&engine, MediaLifecycleState::Starting);

        let stopper = engine.clone();
        let stop = thread::spawn(move || stopper.stop_session());
        wait_for_state(&engine, MediaLifecycleState::Stopping);
        barrier.wait();

        assert!(create.join().expect("start thread").is_err());
        let stopped = stop.join().expect("stop thread").expect("stop completes");
        assert_eq!(stopped.state, MediaLifecycleState::Idle);
        assert!(engine.snapshot().session_id.is_none());
    }

    #[test]
    fn public_worker_stop_during_picker_releases_provisional_resources() {
        let picker = Arc::new(Barrier::new(2));
        let engine = worker_engine(None, Some(Arc::clone(&picker)));
        let mut request = request();
        request.share_on_start = true;
        let starter = engine.clone();
        let create = thread::spawn(move || starter.create_session(request));
        wait_for_state(&engine, MediaLifecycleState::Starting);

        let stopper = engine.clone();
        let stop = thread::spawn(move || stopper.stop_session());
        wait_for_state(&engine, MediaLifecycleState::Stopping);
        picker.wait();

        assert!(create.join().expect("picker start thread").is_err());
        let stopped = stop.join().expect("stop thread").expect("stop completes");
        assert_eq!(stopped.state, MediaLifecycleState::Idle);
        assert!(stopped.session_id.is_none());
    }

    #[test]
    fn public_worker_update_preserves_explicit_source_id_intent() {
        let engine = worker_engine(None, None);
        let mut request = request();
        request.share_on_start = false;
        engine
            .create_session(request)
            .expect("session should start without capture");

        let updated = engine
            .update_session_with_source_id(
                UpdateMediaSessionRequest {
                    source: None,
                    source_id: None,
                    quality: TransmissionQuality::High,
                    bitrate_bps: None,
                    min_bitrate_bps: None,
                    resolution: None,
                    frame_rate: None,
                    codec: VideoCodec::H264,
                    encoder: VideoEncoder::Auto,
                    system_audio: false,
                    excluded_apps: Vec::new(),
                },
                SourceIdUpdate::Set(7),
            )
            .expect("source ID update should succeed");
        assert_eq!(updated.source_id, Some(7));

        let cleared = engine
            .update_session_with_source_id(
                UpdateMediaSessionRequest {
                    source: None,
                    source_id: None,
                    quality: TransmissionQuality::High,
                    bitrate_bps: None,
                    min_bitrate_bps: None,
                    resolution: None,
                    frame_rate: None,
                    codec: VideoCodec::H264,
                    encoder: VideoEncoder::Auto,
                    system_audio: false,
                    excluded_apps: Vec::new(),
                },
                SourceIdUpdate::Clear,
            )
            .expect("source ID clear should succeed");
        assert_eq!(cleared.source_id, None);
    }

    #[test]
    fn public_worker_cleanup_failure_stays_pending_until_retry() {
        let counters = Arc::new(ResourceCounters::default());
        let engine = worker_engine_with_counters(None, None, Some(Arc::clone(&counters)));
        let mut request = request();
        request.share_on_start = false;
        engine
            .create_session(request)
            .expect("session should start");
        assert_eq!(counters.live_bundles(), 1);
        assert_eq!(counters.live_capture(), 0);
        assert_eq!(counters.live_audio(), 0);
        assert_eq!(counters.live_pipeline(), 1);
        assert_eq!(counters.live_fanout(), 1);
        assert_eq!(counters.live_peers(), 0);
        assert_eq!(counters.live_join_services(), 1);

        counters.fail_cleanup(true);
        assert!(engine.stop_session().is_err());
        assert_eq!(engine.snapshot().state, MediaLifecycleState::CleanupPending);
        assert_eq!(counters.live_bundles(), 1);

        counters.fail_cleanup(false);
        engine.stop_session().expect("cleanup retry should succeed");
        assert_eq!(engine.snapshot().state, MediaLifecycleState::Idle);
        assert_eq!(counters.live_bundles(), 0);
        assert_eq!(counters.live_capture(), 0);
        assert_eq!(counters.live_audio(), 0);
        assert_eq!(counters.live_pipeline(), 0);
        assert_eq!(counters.live_fanout(), 0);
        assert_eq!(counters.live_peers(), 0);
        assert_eq!(counters.live_join_services(), 0);
    }

    #[test]
    fn public_worker_repeated_start_stop_returns_zero_resource_bundles() {
        let counters = Arc::new(ResourceCounters::default());
        let engine = worker_engine_with_counters(None, None, Some(Arc::clone(&counters)));
        for _ in 0..3 {
            let mut request = request();
            request.share_on_start = false;
            engine
                .create_session(request)
                .expect("session should start");
            engine.stop_session().expect("session should stop");
            assert_eq!(counters.live_bundles(), 0);
            assert_eq!(counters.live_capture(), 0);
            assert_eq!(counters.live_audio(), 0);
            assert_eq!(counters.live_pipeline(), 0);
            assert_eq!(counters.live_fanout(), 0);
            assert_eq!(counters.live_peers(), 0);
            assert_eq!(counters.live_join_services(), 0);
        }
    }

    #[test]
    fn public_worker_rejects_non_h264_and_frame_rates_over_sixty() {
        let engine = worker_engine(None, None);
        let mut codec_request = request();
        codec_request.codec = VideoCodec::Av1;
        assert!(matches!(
            engine.create_session(codec_request),
            Err(MediaEngineError::Unsupported(_))
        ));

        let mut frame_rate_request = request();
        frame_rate_request.frame_rate = FrameRate::Fps120;
        assert!(matches!(
            engine.create_session(frame_rate_request),
            Err(MediaEngineError::Unsupported(_))
        ));
        assert_eq!(engine.snapshot().state, MediaLifecycleState::Idle);
    }

    #[test]
    fn public_worker_stale_answer_does_not_detach_replacement_link() {
        let engine = worker_engine(None, None);
        let mut request = request();
        request.share_on_start = false;
        engine
            .create_session(request)
            .expect("session should start");

        let old_offer = engine
            .offer_for_member("viewer-1", "Viewer")
            .expect("first offer should be created");
        engine
            .kick_viewer("viewer-1")
            .expect("first link should be removed");
        let new_offer = engine
            .offer_for_member("viewer-1", "Viewer")
            .expect("replacement offer should be created");

        let stale_answer = PeerSignal {
            kind: PeerSignalKind::Answer,
            sdp: String::new(),
            id: Some("viewer-1".into()),
        };
        assert!(engine
            .set_peer_answer(stale_answer, old_offer.offer_attempt.clone())
            .is_err());
        assert!(engine
            .snapshot()
            .roster
            .iter()
            .any(|entry| entry.id == "viewer-1"));
        assert_ne!(old_offer.offer_attempt, new_offer.offer_attempt);
    }

    pub(crate) fn request() -> super::super::types::CreateMediaSessionRequest {
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
            encoder: super::super::types::VideoEncoder::Auto,
            password: String::new(),
            nickname: "Host".into(),
            admission: false,
            join_mode: JoinMode::Lan,
            rendezvous_url: None,
            session_mode: super::super::room_mode::SessionMode::Broadcast,
            attach_only: false,
            share_on_start: true,
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
    fn sala_can_open_without_sharing() {
        let state = test_state();
        let mut open = request();
        open.session_mode = super::super::room_mode::SessionMode::Room;
        open.share_on_start = false;
        let created = create_in_state(&state, open).expect("open room");
        assert_eq!(created.state, MediaLifecycleState::Running);
        assert!(!created.native_capture_active);
    }

    #[test]
    fn sala_share_later_keeps_the_room() {
        let state = test_state();
        let mut open = request();
        open.session_mode = super::super::room_mode::SessionMode::Room;
        open.share_on_start = false;
        let created = create_in_state(&state, open).expect("open room");
        let shared = start_share_in_state(&state).expect("share later");
        assert_eq!(shared.session_id, created.session_id);
        assert_eq!(shared.state, MediaLifecycleState::Running);
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
            state.lock().expect("test state").actor.lifecycle,
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
            state.lock().expect("test state").actor.lifecycle,
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
                source: None,
                source_id: None,
                quality: TransmissionQuality::Low,
                bitrate_bps: None,
                min_bitrate_bps: None,
                resolution: None,
                frame_rate: None,
                codec: VideoCodec::H264,
                encoder: VideoEncoder::Auto,
                system_audio: false,
                excluded_apps: vec!["Discord".into(), "com.hnc.Discord".into()],
            },
        )
        .expect("update should succeed");
        assert_eq!(snapshot.state, MediaLifecycleState::Running);
        assert!(snapshot.session_id.is_some());
        // Resolution/frame_rate are fixed at Start: a live quality update
        // must not rewrite them (the request() helper starts P1080/Fps60).
        assert_eq!(snapshot.resolution, Some(VideoResolution::P1080));
        assert_eq!(snapshot.frame_rate, Some(FrameRate::Fps60));
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
                source: None,
                source_id: None,
                quality: TransmissionQuality::Medium,
                bitrate_bps: None,
                min_bitrate_bps: None,
                resolution: None,
                frame_rate: None,
                codec: VideoCodec::H264,
                encoder: VideoEncoder::Auto,
                system_audio: true,
                excluded_apps: Vec::new(),
            },
        )
        .expect("update should not fail the session");
        assert_eq!(snapshot.state, MediaLifecycleState::Running);
        assert!(snapshot.detail.contains("cannot be added mid-session"));
        // Quality still applied even though audio could not be added;
        // resolution/frame_rate stay at the Start values (P1080/Fps60).
        assert_eq!(snapshot.resolution, Some(VideoResolution::P1080));
        assert_eq!(snapshot.frame_rate, Some(FrameRate::Fps60));
    }

    #[test]
    fn update_session_without_an_active_session_fails() {
        let state = test_state();
        assert_eq!(
            update_in_state(
                &state,
                UpdateMediaSessionRequest {
                    source: None,
                    source_id: None,
                    quality: TransmissionQuality::High,
                    bitrate_bps: None,
                    min_bitrate_bps: None,
                    resolution: None,
                    frame_rate: None,
                    codec: VideoCodec::H264,
                    encoder: VideoEncoder::Auto,
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
            state.lock().expect("test state").actor.lifecycle,
            MediaLifecycleState::Idle
        );
    }

    #[test]
    fn snapshot_is_observational_and_does_not_advance_actor_epochs() {
        let state = test_state();
        let (session, share, link, discarded) = {
            let mut guard = state.lock().expect("test state");
            let session = guard.actor.begin_session();
            let share = guard.actor.begin_share();
            let link = guard.actor.begin_link();
            (session, share, link, guard.actor.discarded_events())
        };
        let _ = snapshot_from_state(&state.lock().expect("test state"));
        let guard = state.lock().expect("test state");
        assert_eq!(guard.actor.session_epoch, session);
        assert_eq!(guard.actor.share_epoch, share);
        assert!(guard.actor.accepts(EpochFence {
            session,
            share,
            link: Some(link)
        }));
        assert_eq!(guard.actor.discarded_events(), discarded);
    }
}

#[cfg(test)]
#[path = "media_engine_integration_test.rs"]
mod media_engine_integration_test;
