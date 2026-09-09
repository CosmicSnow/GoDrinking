//! Thin Tauri shell over `golive-core`.
//!
//! Rules enforced here:
//! - No product logic: commands validate input, drive the core, and return.
//!   Sala/video decisions live in the core; orchestration between signal and
//!   media lives in [`pump`] (a future `core::runtime` candidate).
//! - Core events become Tauri events (`signal-event`, `media-event`) for the
//!   future UI. Commands never poll.
//! - Errors are redacted Strings. Passwords, tokens and SDP/candidates never
//!   reach logs or the frontend (envelopes carry kinds only).
//! - Locks are held briefly; never across `.await` (media sessions sit
//!   behind `tokio::sync::Mutex`, the sync core behind short std locks).

pub mod pump;
pub mod screen;
pub mod session_log;
pub mod video;

use golive_core::media::{NativeViewer, Publisher, Quality, QualityProfile, VideoSource};
use golive_core::owner::{Fence, Owner, OwnerSnapshot};
use golive_core::signal::SignalClient;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::mpsc;

pub const DEFAULT_SERVER: &str = "http://127.0.0.1:18790";

/// Share source selector. Screen capture arrives via the platform bridge
/// (`display:<id>` / `window:<id>`); synthetic + movie stay untouched.
#[derive(Clone, Debug)]
pub enum ShareSource {
    Synthetic,
    Movie(String),
    Display(String),
    Window(String),
}

impl ShareSource {
    fn parse(raw: &str) -> Result<Self, String> {
        if raw == "synthetic" {
            Ok(Self::Synthetic)
        } else if let Some(path) = raw.strip_prefix("movie:") {
            if path.is_empty() {
                Err("movie: needs a path".into())
            } else {
                Ok(Self::Movie(path.to_owned()))
            }
        } else if let Some(id) = raw.strip_prefix("display:") {
            if id.trim().is_empty() {
                Err("display: needs an id (list sources first)".into())
            } else {
                Ok(Self::Display(id.trim().to_owned()))
            }
        } else if let Some(id) = raw.strip_prefix("window:") {
            if id.trim().is_empty() {
                Err("window: needs an id (list sources first)".into())
            } else {
                Ok(Self::Window(id.trim().to_owned()))
            }
        } else {
            Err("fonte: 'synthetic', 'movie:/caminho', 'display:<id>' ou 'window:<id>'".into())
        }
    }
}

/// `set_quality` payload (UI lane builds it via `resolveDesired`).
/// Numerics are authoritative; `preset`, when present, must name a known
/// preset (`low|medium|high`) and is echoed for display only.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct SetQualityArgs {
    pub w: u32,
    pub h: u32,
    /// Accepts the canonical snake_case and the legacy camelCase spelling
    /// (older packaged frontends send `bitrateKbps`); both bind here so a
    /// stale UI never fails deserialization against a new backend.
    #[serde(alias = "bitrateKbps")]
    pub bitrate_kbps: u32,
    pub fps: u32,
    #[serde(default)]
    pub preset: Option<String>,
}

/// Redacted media counters observed by forward tasks (pollable fallback
/// next to push events; kinds and counts only).
#[derive(Clone, Default, Debug, serde::Serialize)]
pub struct MediaCounters {
    pub connected: bool,
    pub frames: u64,
    pub keyframes: u64,
    pub keyframes_seen: bool,
    /// Frames actually presented in native windows (acks from helpers).
    /// Distinct from `frames` (decoded): this is the presentation evidence.
    pub presented: u64,
    /// Per-link transmission stats (one entry per watched member with a
    /// native window). Consumed by the future UI lane; empty when idle.
    #[serde(default)]
    pub links: Vec<video::LinkStats>,
    /// Effective share quality (authoritative snapshot for the UI lane).
    /// `None` when not sharing. Generation bumps async via `media-event`.
    #[serde(default)]
    pub effective: Option<EffectiveQuality>,
    /// Live encode backend (`videotoolbox`/`openh264`) for the UI badge
    /// and diagnostics. First publisher reporting wins; `None` until the
    /// encode thread finishes its first build. Read on snapshot/event —
    /// never polled.
    #[serde(default)]
    pub backend: Option<String>,
    /// Why the software fallback, when `backend` is `openh264`.
    /// Triaged in-app (deterministic, no probe access needed): test hook,
    /// platform without VideoToolbox, or a failed probe (details in the
    /// session log). `None` for hardware or when there is no backend yet.
    #[serde(default)]
    pub backend_note: Option<String>,
}

/// Fallback reason for the UI badge. Deterministic triage from facts the
/// app owns (no probe access needed): test hook, platform, else the probe
/// itself failed (exact status lives in the session log). Pure + tested.
pub fn backend_note_for(backend: Option<&str>) -> Option<String> {
    match backend {
        None | Some("videotoolbox") => None,
        Some("openh264") => Some(
            if cfg!(not(target_os = "macos")) {
                "sem VideoToolbox nesta plataforma".to_owned()
            } else if std::env::var_os("GOLIVE_DISABLE_HW").is_some() {
                "hardware desabilitado (GOLIVE_DISABLE_HW)".to_owned()
            } else {
                "probe de hardware falhou — ver log de sessão".to_owned()
            },
        ),
        Some(_) => None,
    }
}

/// Effective share quality: the last profile accepted by `set_quality`
/// (or the `start_share` default) plus the encode generation fence.
/// Generation counts APPLIED reconfigs (rollbacks included) and arrives
/// async — see the `quality` media-event.
///
/// Note: odd requested dims are accepted and floored to even inside the
/// encoder (see `normalize_dims`) — the stored profile echoes the request.
#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct EffectiveQuality {
    pub profile: QualityProfile,
    pub generation: u64,
}
/// Roster member in the exact `RoomMember {id, nickname, master, share}`
/// shape the UI consumes. Same mapping `pump` uses for the roster
/// signal-event (`pump.rs` `Incoming::Roster`): fields copied verbatim from
/// the stored `signal.roster()` entries — no derivation from the owner
/// snapshot or links anywhere.
#[derive(Clone, Debug, serde::Serialize)]
pub struct RosterMember {
    pub id: String,
    pub nickname: String,
    pub master: bool,
    pub share: bool,
}

/// Share-modal thumbnail (lazy one-shot pull, never polled). Always
/// succeeds at the command boundary: capture failures (denial, gone
/// source, encode) come back as a null `data_url` — the modal lists
/// sources regardless. Never carries titles or pixels except inside the
/// data URL itself.
#[derive(Clone, Debug, serde::Serialize)]
pub struct SourcePreview {
    pub data_url: Option<String>,
    pub w: u32,
    pub h: u32,
}

/// Wire ids as decimal strings (adopted from envelopes, never logged).
#[derive(Clone, Default, Debug)]
pub struct WireIds {
    pub session: String,
    pub share: String,
    pub link: String,
    pub attempt: String,
}

/// One host link: its own PeerConnection + fences + trickle queue.
pub struct PublishSession {
    pub publisher: Arc<tokio::sync::Mutex<Publisher>>,
    pub owner_fence: Fence,
    pub wire: WireIds,
    pub remote_ready: bool,
    pub pending_remote: Vec<String>,
}

/// Shared shell state. Managed as `Arc<AppState>` so background tasks and
/// tests can hold it without a Tauri app.
pub struct AppState {
    inner: Mutex<Inner>,
    /// Session file log (packaged verification). Disabled until `run_with`
    /// inits it from the platform log dir; silent no-op before that (and
    /// in every unit test).
    session_log: Mutex<session_log::SessionLog>,
}

/// Self-driving test plan. ONLY constructible from the `--e2e-plan` CLI
/// flag: without it the app has zero test behavior (normal UI path).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct E2ePlan {
    /// "host" or "viewer".
    pub role: String,
    /// Rendezvous base, e.g. http://127.0.0.1:18790.
    pub server: String,
    /// Room password for the run (local test only).
    pub password: String,
    /// Display name for the run.
    pub nickname: String,
    /// File where the host publishes the room code.
    pub code_file: String,
    /// File where this instance reports JSON status.
    pub status_file: String,
}

impl E2ePlan {
    /// Parses `--e2e-plan '<json>'` from process args. Returns `None` when
    /// the flag is absent (normal app). Errors on malformed plan/role.
    pub fn from_args(args: impl Iterator<Item = String>) -> Result<Option<Self>, String> {
        let args: Vec<String> = args.collect();
        let Some(pos) = args.iter().position(|a| a == "--e2e-plan") else {
            return Ok(None);
        };
        let raw = args
            .get(pos + 1)
            .ok_or_else(|| "--e2e-plan needs a JSON argument".to_string())?;
        let plan: E2ePlan =
            serde_json::from_str(raw).map_err(|e| format!("bad --e2e-plan JSON: {e}"))?;
        if plan.role != "host" && plan.role != "viewer" {
            return Err("e2e plan role must be 'host' or 'viewer'".to_string());
        }
        for (name, value) in [
            ("server", &plan.server),
            ("password", &plan.password),
            ("nickname", &plan.nickname),
            ("code_file", &plan.code_file),
            ("status_file", &plan.status_file),
        ] {
            if value.trim().is_empty() {
                return Err(format!("e2e plan field '{name}' must not be empty"));
            }
        }
        Ok(Some(plan))
    }
}

struct Inner {
    server_base: String,
    owner: Owner,
    signal: Option<SignalClient>,
    publishers: HashMap<String, PublishSession>,
    viewer: Option<Arc<tokio::sync::Mutex<NativeViewer>>>,
    viewer_media_task: Option<tokio::task::JoinHandle<()>>,
    adopted: Option<WireIds>,
    viewer_fence: Option<Fence>,
    viewer_remote_ready: bool,
    viewer_pending_remote: Vec<String>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    /// Present only when launched with `--e2e-plan`. Gates every test-only
    /// command; the normal UI path never sees it.
    e2e_plan: Option<E2ePlan>,
    /// Running screen-capture bridge (Display/Window shares). Owned here so
    /// stop_share/leave always tear it down — never orphaned.
    screen_bridge: Option<screen::BridgeHandle>,
    /// Backend-observed media counters (forward tasks bump these).
    media_counters: MediaCounters,
    /// One native video window (+ its latest-only feed) per watched member.
    /// N links mean N independent windows; a dead helper fails one link.
    video_windows: HashMap<String, video::VideoWindow>,
    video_feeds: HashMap<String, video::FramePush>,
    /// Per-link decode observations (bumped in the `on_frame` callback:
    /// decoded count + latest dims). Joined with the windows above into
    /// [`video::LinkStats`] on snapshot/emit — no polling anywhere.
    link_tracks: HashMap<String, LinkTrack>,
    /// Effective share quality (last accepted `set_quality` profile, or the
    /// `start_share` default). `None` when not sharing; generation bumps
    /// async as the encode thread applies reconfigs (see `media-event`
    /// `quality`). Cleared on stop/leave with the publishers.
    share_profile: Option<EffectiveQuality>,
    /// Live capture profile shared with the screen bridge (fps clamp +
    /// target dims, re-clamped by `set_quality` without restarting the OS
    /// stream). `None` for synthetic/movie (no bridge) and when idle.
    share_capture: Option<Arc<Mutex<QualityProfile>>>,
    /// Session-log dedupe: last backend name logged + whether the ICE
    /// census line went out (log once per process, not per Stats event).
    last_logged_backend: Option<String>,
    census_logged: bool,
}

/// Decode-side observation for one watched member.
#[derive(Clone, Debug, Default)]
pub struct LinkTrack {
    pub title: String,
    pub decoded: u64,
    pub w: u32,
    pub h: u32,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                server_base: DEFAULT_SERVER.to_owned(),
                owner: Owner::new(),
                signal: None,
                publishers: HashMap::new(),
                viewer: None,
                viewer_media_task: None,
                adopted: None,
                viewer_fence: None,
                viewer_remote_ready: false,
                viewer_pending_remote: Vec::new(),
                tasks: Vec::new(),
                e2e_plan: None,
                media_counters: MediaCounters::default(),
                video_windows: HashMap::new(),
                video_feeds: HashMap::new(),
                link_tracks: HashMap::new(),
                share_profile: None,
                share_capture: None,
                last_logged_backend: None,
                census_logged: false,
                screen_bridge: None,
            }),
            session_log: Mutex::new(session_log::SessionLog::disabled()),
        }
    }

    /// Installs the session file log (called once from `run_with` setup).
    pub fn set_session_log(&self, log: session_log::SessionLog) {
        if let Ok(mut slot) = self.session_log.lock() {
            *slot = log;
        }
    }

    /// One redacted milestone line (no-op until `set_session_log`).
    pub fn session_log(&self, line: String) {
        if let Ok(guard) = self.session_log.lock() {
            guard.log(&line);
        }
    }

    /// Installs the self-driving plan (called once at startup from CLI args).
    pub fn set_e2e_plan(&self, plan: E2ePlan) -> Result<(), String> {
        self.inner
            .lock()
            .map_err(|_| "state lock poisoned".to_string())?
            .e2e_plan = Some(plan);
        Ok(())
    }

    fn require_e2e_plan(&self) -> Result<E2ePlan, String> {
        self.inner
            .lock()
            .map_err(|_| "state lock poisoned".to_string())?
            .e2e_plan
            .clone()
            .ok_or_else(|| "e2e not active".to_string())
    }

    /// Sets the rendezvous base URL (`http(s)://host:port`). Validated.
    pub fn set_server(&self, base: &str) -> Result<String, String> {
        let base = base.trim().trim_end_matches('/').to_owned();
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            return Err("server must start with http:// or https://".into());
        }
        if base.len() > 256 {
            return Err("server URL too long".into());
        }
        self.inner
            .lock()
            .map_err(|_| "state lock poisoned".to_string())?
            .server_base = base.clone();
        Ok(base)
    }

    fn base(&self) -> Result<String, String> {
        self.inner
            .lock()
            .map_err(|_| "state lock poisoned".to_string())
            .map(|inner| inner.server_base.clone())
    }

    /// Creates a room on the rendezvous and starts the signal pump.
    /// Returns the room code (not a secret).
    pub async fn create_room(
        self: &Arc<Self>,
        app: Option<AppHandle>,
        nickname: &str,
        password: &str,
    ) -> Result<String, String> {
        let base = self.base()?;
        let join = {
            let inner = self
                .inner
                .lock()
                .map_err(|_| "state lock poisoned".to_string())?;
            inner.owner.begin_join().map_err(redact_owner)?
        };
        let signal = SignalClient::create_room(&base, nickname, password, false)
            .map_err(|e| format!("create room: {e}"))?;
        let code = signal.code().to_owned();
        {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| "state lock poisoned".to_string())?;
            inner
                .owner
                .complete_opened(&join)
                .map_err(redact_owner)?;
            inner.signal = Some(signal);
            // Test-only handoff: publish the room code where the plan says,
            // so a second self-driven instance can join without humans.
            if let Some(plan) = inner.e2e_plan.clone() {
                if let Some(parent) = std::path::Path::new(&plan.code_file).parent() {
                    if !parent.as_os_str().is_empty() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                }
                let _ = std::fs::write(&plan.code_file, &code);
            }
            inner.tasks.push(pump::spawn_pump(Arc::clone(self), app));
        }
        Ok(code)
    }

    /// Joins a room by code. Returns our member id (not a secret).
    pub async fn join_room(
        self: &Arc<Self>,
        app: Option<AppHandle>,
        code: &str,
        nickname: &str,
        password: &str,
    ) -> Result<String, String> {
        let base = self.base()?;
        let join = {
            let inner = self
                .inner
                .lock()
                .map_err(|_| "state lock poisoned".to_string())?;
            inner.owner.begin_join().map_err(redact_owner)?
        };
        let signal = SignalClient::join_room(&base, code, nickname, password)
            .map_err(|e| format!("join room: {e}"))?;
        let member_id = signal.member_id().to_owned();
        {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| "state lock poisoned".to_string())?;
            inner
                .owner
                .complete_opened(&join)
                .map_err(redact_owner)?;
            inner.signal = Some(signal);
            inner.tasks.push(pump::spawn_pump(Arc::clone(self), app));
        }
        Ok(member_id)
    }

    /// Leaves the room: tasks aborted, media stopped, session closed
    /// best-effort. Idempotent.
    pub async fn leave(self: &Arc<Self>) -> Result<(), String> {
        let (signal, publishers, viewer, screen_bridge) = {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| "state lock poisoned".to_string())?;
            for task in inner.tasks.drain(..) {
                task.abort();
            }
            if let Some(task) = inner.viewer_media_task.take() {
                task.abort();
            }
            inner.share_profile = None;
            inner.share_capture = None;
            (
                inner.signal.take(),
                std::mem::take(&mut inner.publishers),
                inner.viewer.take(),
                inner.screen_bridge.take(),
            )
        };
        // Outside the lock: network + media teardown.
        if let Some(mut signal) = signal {
            signal.leave();
            signal.shutdown();
        }
        for (_, session) in publishers {
            session.publisher.lock().await.stop().await;
        }
        if let Some(viewer) = viewer {
            viewer.lock().await.stop().await;
        }
        if let Some(mut bridge) = screen_bridge {
            bridge.stop();
        }
        // Native video windows close with the session (no orphan windows).
        self.close_all_video_windows();
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "state lock poisoned".to_string())?;
        inner.adopted = None;
        inner.viewer_fence = None;
        inner.viewer_remote_ready = false;
        inner.viewer_pending_remote.clear();
        // Best-effort close; absence of a session is not an error here.
        if let Ok(close) = inner.owner.begin_close() {
            let _ = inner.owner.complete_closed(&close);
        }
        drop(inner);
        self.session_log("leave".to_string());
        Ok(())
    }

    /// Starts sharing. One encode pipeline per watcher is created lazily on
    /// watch intents (fanout-via-shared-track is the next core refactor).
    pub async fn start_share(
        self: &Arc<Self>,
        app: Option<AppHandle>,
        source: &str,
    ) -> Result<(), String> {
        let source = ShareSource::parse(source)?;
        // Live capture profile for the bridge (starts at the default; the
        // bridge thread shares it so `set_quality` re-clamps mid-share).
        let live_profile = Arc::new(Mutex::new(Quality::P720.profile()));
        // Resolve the core source BEFORE touching lifecycle (pre-flight):
        // bridge setup may prompt/fail, and a failure must leave no
        // half-started share behind (Starting has no path back to Stopped).
        enum Resolved {
            Direct(VideoSource),
            Bridged {
                video: VideoSource,
                bridge: screen::BridgeHandle,
            },
        }
        let resolved = match &source {
            ShareSource::Synthetic => Resolved::Direct(VideoSource::SyntheticBall),
            ShareSource::Movie(path) => {
                if !std::path::Path::new(path).exists() {
                    return Err("movie file not found".into());
                }
                Resolved::Direct(VideoSource::MovieFile(path.into()))
            }
            ShareSource::Display(id) => {
                let (rx, bridge, label) = screen::start_capture_for(
                    golive_platform::SourceKind::Display,
                    id,
                    Quality::P720.profile(),
                    Arc::clone(&live_profile),
                )
                .map_err(|e| e.to_string())?;
                Resolved::Bridged {
                    video: VideoSource::External(
                        golive_core::media::ExternalSource { rx, label },
                    ),
                    bridge,
                }
            }
            ShareSource::Window(id) => {
                let (rx, bridge, label) = screen::start_capture_for(
                    golive_platform::SourceKind::Window,
                    id,
                    Quality::P720.profile(),
                    Arc::clone(&live_profile),
                )
                .map_err(|e| e.to_string())?;
                Resolved::Bridged {
                    video: VideoSource::External(
                        golive_core::media::ExternalSource { rx, label },
                    ),
                    bridge,
                }
            }
        };
        let (video_source, mut bridge) = match resolved {
            Resolved::Direct(video) => (video, None),
            Resolved::Bridged { video, bridge } => (video, Some(bridge)),
        };
        // From here on, every failure path stops the bridge (if any).
        let start = {
            let inner = self
                .inner
                .lock()
                .map_err(|_| "state lock poisoned".to_string())?;
            if inner.signal.is_none() {
                if let Some(bridge) = bridge.as_mut() {
                    bridge.stop();
                }
                return Err("not in a room".into());
            }
            match inner.owner.begin_share_start() {
                Ok(start) => start,
                Err(e) => {
                    drop(inner);
                    if let Some(bridge) = bridge.as_mut() {
                        bridge.stop();
                    }
                    return Err(redact_owner(e));
                }
            }
        };
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let publisher = match Publisher::start(video_source, Quality::P720, None, event_tx).await {
            Ok(publisher) => publisher,
            Err(e) => {
                if let Some(bridge) = bridge.as_mut() {
                    bridge.stop();
                }
                return Err(format!("publisher: {e}"));
            }
        };
        let publisher = Arc::new(tokio::sync::Mutex::new(publisher));
        {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| "state lock poisoned".to_string())?;
            inner
                .owner
                .complete_share_live(&start)
                .map_err(redact_owner)?;
            if let Some(signal) = inner.signal.as_ref() {
                let _ = signal.announce_share(true);
            }
            inner.tasks.push(pump::spawn_forward(
                Arc::clone(self),
                app,
                event_rx,
                pump::ForwardTarget::Share,
            ));
            // Stash the idle publisher as the template for per-watcher links.
            // (MVP: first watch reuses it; see pump.)
            inner.publishers.insert(
                String::new(),
                PublishSession {
                    publisher,
                    owner_fence: start,
                    wire: WireIds::default(),
                    remote_ready: false,
                    pending_remote: Vec::new(),
                },
            );
            // Default effective quality: MEDIUM (== Quality::P720, the fixed
            // start profile). `set_quality` moves it live from here.
            inner.share_profile = Some(EffectiveQuality {
                profile: Quality::P720.profile(),
                generation: 0,
            });
            // The bridge (if any) shares the live profile from here on.
            inner.share_capture = bridge.is_some().then(|| Arc::clone(&live_profile));
            // Own the bridge from here: stop_share/leave always tear it down.
            // (A previous bridge cannot exist: stop clears it, and start
            // while live fails at begin_share_start above. Defensive stop
            // anyway — never orphan an OS stream.)
            let stale = std::mem::replace(&mut inner.screen_bridge, bridge);
            drop(inner);
            if let Some(mut stale) = stale {
                stale.stop();
            }
        }
        // Milestone: source KIND only (never paths/ids) + start profile.
        let kind = match &source {
            ShareSource::Synthetic => "synthetic",
            ShareSource::Movie(_) => "movie",
            ShareSource::Display(_) => "display",
            ShareSource::Window(_) => "window",
        };
        let start_profile = Quality::P720.profile();
        self.session_log(format!(
            "share start kind={kind} profile={}x{}@{}",
            start_profile.w, start_profile.h, start_profile.fps
        ));
        Ok(())
    }

    /// Stops sharing: media stopped, bridge torn down, links cleared
    /// deterministically.
    pub async fn stop_share(self: &Arc<Self>) -> Result<(), String> {
        let publishers = {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| "state lock poisoned".to_string())?;
            inner.share_profile = None;
            inner.share_capture = None;
            std::mem::take(&mut inner.publishers)
        };
        for (_, session) in publishers {
            session.publisher.lock().await.stop().await;
        }
        {
            let mut bridge = {
                self.inner
                    .lock()
                    .map_err(|_| "state lock poisoned".to_string())?
                    .screen_bridge
                    .take()
            };
            if let Some(bridge) = bridge.as_mut() {
                bridge.stop();
            }
        }
        let inner = self
            .inner
            .lock()
            .map_err(|_| "state lock poisoned".to_string())?;
        // Idempotent: completing an already-stopped share is a no-op.
        if inner.owner.snapshot().share.id.is_some() {
            if let Ok(stop) = inner.owner.begin_share_stop() {
                let _ = inner.owner.complete_share_stopped(&stop);
            }
        } else {
            let _ = inner.owner.complete_share_stopped(&Fence::idle());
        }
        if let Some(signal) = inner.signal.as_ref() {
            let _ = signal.announce_share(false);
        }
        drop(inner);
        self.session_log("share stop".to_string());
        Ok(())
    }

    /// Live quality switch without re-signaling.
    ///
    /// Why a separate command (and `start_share` keeps its `{source}`-only
    /// wire shape): starting owns irreversible lifecycle pre-flight (owner
    /// fence Starting→Live has no path back, bridge acquisition, atomic
    /// teardown on failure). Quality is a property of a LIVE share; mixing
    /// profile validation into start would entangle it with those teardown
    /// paths. `set_quality` covers both the pre-watch template publisher
    /// and adopted per-watcher links.
    ///
    /// Semantics: validate-first via [`QualityProfile::validate`] (typed,
    /// redacted — numbers only), then transactional reconfig on every live
    /// publisher (core rebuilds + forces IDR + bumps the generation fence;
    /// same m-line, no re-signaling). Core apply is atomic per encoder
    /// (build-new-then-swap: a failed rebuild keeps the old encoder
    /// running). On partial multi-publisher failure this rolls back the
    /// ones that succeeded (best-effort, each rollback is itself a fenced
    /// reconfig) and leaves `share_profile` untouched — the encoder is
    /// never left in an intermediate state.
    ///
    /// Returns the new effective profile (pre-bump generation); the
    /// authoritative generation arrives async via `media-event` `quality`
    /// (emitted here optimistically and again by the forward task when it
    /// observes the fence bump) and via `get_media_counters.effective`.
    pub async fn set_quality(
        self: &Arc<Self>,
        app: Option<AppHandle>,
        args: SetQualityArgs,
    ) -> Result<EffectiveQuality, String> {
        if let Some(preset) = args.preset.as_deref() {
            match preset {
                "low" | "medium" | "high" => {}
                _ => return Err(format!("unknown preset '{preset}' (low|medium|high)")),
            }
        }
        let profile = QualityProfile {
            w: args.w,
            h: args.h,
            bitrate_kbps: args.bitrate_kbps,
            fps: args.fps,
        };
        profile
            .validate()
            .map_err(|e| format!("qualidade: {e}"))?;
        // Snapshot publishers + previous effective under one short lock;
        // no lock is held across the awaits below.
        let (publishers, previous) = {
            let inner = self
                .inner
                .lock()
                .map_err(|_| "state lock poisoned".to_string())?;
            if inner.publishers.is_empty() {
                return Err("not sharing".into());
            }
            let publishers: Vec<Arc<tokio::sync::Mutex<Publisher>>> = inner
                .publishers
                .values()
                .map(|session| Arc::clone(&session.publisher))
                .collect();
            let previous = inner.share_profile.unwrap_or(EffectiveQuality {
                profile: Quality::P720.profile(),
                generation: 0,
            });
            (publishers, previous)
        };
        // Apply to every live publisher (template + adopted links).
        let mut applied = 0usize;
        let mut apply_error: Option<String> = None;
        for publisher in &publishers {
            match publisher.lock().await.reconfigure(profile) {
                Ok(()) => applied += 1,
                Err(e) => {
                    apply_error = Some(format!("qualidade: {e}"));
                    break;
                }
            }
        }
        if let Some(error) = apply_error {
            // Best-effort rollback: restore the previous profile on the
            // publishers that already moved (each rollback is fenced too).
            for publisher in publishers.iter().take(applied) {
                let _ = publisher.lock().await.reconfigure(previous.profile);
            }
            return Err(error);
        }
        // Restart the capture stream at the new profile (if bridged).
        // Transactional: the new SCK stream starts first, so a failure
        // leaves the old stream running and only needs a publisher
        // rollback. The handle is taken out for the blocking call (no
        // long-held lock) and put back after, unless the share died
        // under us (leave raced: stop instead of resurrecting).
        let mut bridge = {
            match self.inner.lock() {
                Ok(mut inner) => inner.screen_bridge.take(),
                Err(_) => None,
            }
        };
        if let Some(handle) = bridge.as_mut() {
            if let Err(e) = handle.reconfigure(profile) {
                for publisher in publishers.iter().take(applied) {
                    let _ = publisher.lock().await.reconfigure(previous.profile);
                }
                if let Ok(mut inner) = self.inner.lock() {
                    inner.screen_bridge = bridge;
                }
                return Err(format!("qualidade: captura: {e}"));
            }
        }
        let effective = EffectiveQuality { profile, generation: previous.generation };
        {
            if let Ok(mut inner) = self.inner.lock() {
                if inner.publishers.is_empty() {
                    // Share died mid-switch: stop the (reconfigured) bridge
                    // instead of resurrecting it, report cleanly.
                    if let Some(mut handle) = bridge.take() {
                        handle.stop();
                    }
                    inner.screen_bridge = None;
                    return Err("not sharing".into());
                }
                inner.share_profile = Some(effective);
                // Re-clamp capture in place (bridge reads it per tick).
                if let Some(live) = inner.share_capture.as_ref() {
                    if let Ok(mut guard) = live.lock() {
                        *guard = profile;
                    }
                }
                inner.screen_bridge = bridge;
            }
        }
        if let Some(app) = app {
            let _ = app.emit(
                "media-event",
                &serde_json::json!({
                    "kind": "quality",
                    "profile": profile,
                    "generation": effective.generation,
                }),
            );
        }
        self.session_log(format!(
            "quality profile={}x{}@{} generation={}",
            profile.w, profile.h, profile.fps, effective.generation
        ));
        Ok(effective)
    }

    /// Registers watch intent for a member (viewer side).
    pub async fn watch(self: &Arc<Self>, member: &str) -> Result<(), String> {
        if member.trim().is_empty() {
            return Err("member must not be empty".into());
        }
        let fence = {
            let inner = self
                .inner
                .lock()
                .map_err(|_| "state lock poisoned".to_string())?;
            inner.owner.watch(member).map_err(redact_owner)?
        };
        {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| "state lock poisoned".to_string())?;
            inner.viewer_fence = Some(fence);
            if let Some(signal) = inner.signal.as_ref() {
                signal.watch(member, true).map_err(|e| format!("watch: {e}"))?;
            } else {
                return Err("not in a room".into());
            }
        }
        self.session_log(format!("watch member={}", session_log::short_id(member)));
        Ok(())
    }

    /// Removes our watch intent and tears down viewer media.
    pub async fn unwatch(self: &Arc<Self>, member: &str) -> Result<(), String> {
        let viewer = {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| "state lock poisoned".to_string())?;
            if let Some(signal) = inner.signal.as_ref() {
                let _ = signal.watch(member, false);
            }
            inner.viewer.take()
        };
        if let Some(viewer) = viewer {
            viewer.lock().await.stop().await;
        }
        // The watched link's window closes here (never lingers unwatched).
        self.close_video_window(member);
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "state lock poisoned".to_string())?;
        if let Ok(fence) = inner.owner.unwatch(member) {
            let _ = inner.owner.complete_link_removed(&fence);
        }
        inner.adopted = None;
        inner.viewer_fence = None;
        inner.viewer_remote_ready = false;
        inner.viewer_pending_remote.clear();
        drop(inner);
        self.session_log(format!("unwatch member={}", session_log::short_id(member)));
        Ok(())
    }

    /// Lists capture sources for the UI select. Empty/error surfaces typed
    /// (the UI shows the reason instead of failing mute). May trigger the
    /// OS permission prompt on first use — call from explicit user action.
    pub async fn list_sources(&self) -> Result<Vec<screen::ListedSource>, String> {
        // enumerate() can block on the OS (prompt wait); keep it off the
        // async worker via spawn_blocking.
        tokio::task::spawn_blocking(screen::enumerate_sources)
            .await
            .map_err(|e| format!("listagem: {e}"))?
            .map_err(|e| e.to_string())
    }

    /// Compile-time capture capabilities (UI disable-with-reason).
    /// No OS contact: safe to call any time, never prompts.
    pub fn source_capabilities(&self) -> screen::Capabilities {
        screen::capabilities()
    }

    /// Immutable snapshot for the UI. Locks only to clone.
    pub fn get_snapshot(&self) -> Result<OwnerSnapshot, String> {
        self.inner
            .lock()
            .map_err(|_| "state lock poisoned".to_string())
            .map(|inner| inner.owner.snapshot())
    }

    /// Rich roster pull for the UI (explicit callers only — never polled).
    /// Reads the same stored `signal.roster()` the pump maps into the
    /// `roster` signal-event, so a UI that mounted after that emit (listeners
    /// attach on room entry; Tauri events have no backlog) still sees
    /// nicknames on entry. Empty when offline (no signal client yet) or when
    /// the lock is poisoned — the event listener fills it in later.
    pub fn get_roster(&self) -> Vec<RosterMember> {
        let inner = match self.inner.lock() {
            Ok(inner) => inner,
            Err(_) => return Vec::new(),
        };
        match inner.signal.as_ref() {
            Some(signal) => signal
                .roster()
                .entries
                .iter()
                .map(|e| RosterMember {
                    id: e.id.clone(),
                    nickname: e.nickname.clone(),
                    master: e.master,
                    share: e.share,
                })
                .collect(),
            None => Vec::new(),
        }
    }

    /// One-shot source thumbnail for the share modal (explicit UI pull,
    /// never polled). Infallible at the boundary: unknown kind and every
    /// backend failure (denial, gone source, empty grab, encode) come back
    /// as a null `data_url` with 0x0 dims — the modal lists sources
    /// regardless. Never logs titles or pixels.
    pub fn preview_source(&self, kind: &str, id: &str) -> SourcePreview {
        let kind = match kind {
            "display" => golive_platform::SourceKind::Display,
            "window" => golive_platform::SourceKind::Window,
            _ => {
                return SourcePreview { data_url: None, w: 0, h: 0 };
            }
        };
        match screen::preview_source(kind, id) {
            Ok(preview) => SourcePreview {
                data_url: Some(preview.data_url),
                w: preview.w,
                h: preview.h,
            },
            Err(_) => SourcePreview { data_url: None, w: 0, h: 0 },
        }
    }

    /// Backend-observed media counters (observational, redacted). Pollable
    /// fallback next to push events. `presented` is summed live from the
    /// native windows (acks), the rest is bumped by forward tasks.
    pub fn get_media_counters(&self) -> Result<MediaCounters, String> {
        self.inner
            .lock()
            .map_err(|_| "state lock poisoned".to_string())
            .map(|inner| {
                let mut counters = inner.media_counters.clone();
                counters.presented = inner.video_windows.values().map(|w| w.presented()).sum();
                counters.links = Self::link_stats_locked(&inner);
                counters.effective = inner.share_profile;
                counters.backend = Self::encode_backend_locked(&inner);
                counters.backend_note = backend_note_for(counters.backend.as_deref());
                counters
            })
    }

    /// First live encoder backend across publishers (non-blocking read;
    /// `None` until a build lands). All publishers share the engine, so
    /// first-reporter is representative.
    fn encode_backend_locked(inner: &Inner) -> Option<String> {
        inner
            .publishers
            .values()
            .filter_map(|session| session.publisher.try_lock().ok())
            .filter_map(|publisher| publisher.backend())
            .map(|name| name.to_owned())
            .next()
    }

    /// Builds per-link stats from decode tracks + native windows. Sorted by
    /// member for stable snapshots.
    fn link_stats_locked(inner: &Inner) -> Vec<video::LinkStats> {
        let mut links: Vec<video::LinkStats> = inner
            .video_windows
            .iter()
            .map(|(member, window)| {
                let track = inner.link_tracks.get(member);
                let decoded = track.map(|t| t.decoded).unwrap_or_else(|| window.pushed());
                let presented = window.presented();
                let (w, h) = window.resolution();
                video::LinkStats {
                    member: member.clone(),
                    title: track
                        .map(|t| t.title.clone())
                        .unwrap_or_else(|| window.title().to_owned()),
                    codec: video::LINK_CODEC.to_owned(),
                    width: track.map(|t| t.w).filter(|w| *w > 0).unwrap_or(w),
                    height: track.map(|t| t.h).filter(|h| *h > 0).unwrap_or(h),
                    decoded,
                    presented,
                    dropped: decoded.saturating_sub(presented),
                    render_fps: window.render_fps(),
                    bitrate_bps: window.bitrate_bps(),
                    bitrate_note: video::BITRATE_NOTE.to_owned(),
                    delay_estimate_ms: None,
                    delay_note: video::DELAY_NOTE.to_owned(),
                    dropped_note: video::DROPPED_NOTE.to_owned(),
                }
            })
            .collect();
        links.sort_by(|a, b| a.member.cmp(&b.member));
        links
    }

    /// Per-link stats for one member (used by the stats event emit).
    pub fn link_stats_for(&self, member: &str) -> Option<video::LinkStats> {
        self.inner
            .lock()
            .ok()
            .and_then(|inner| Self::link_stats_locked(&inner).into_iter().find(|l| l.member == member))
    }

    /// Records one decoded frame for a watched member (called from the
    /// `on_frame` present callback: event-driven, never polled).
    pub fn note_link_frame(&self, member: &str, title: &str, w: u32, h: u32) {
        if let Ok(mut inner) = self.inner.lock() {
            let track = inner.link_tracks.entry(member.to_owned()).or_default();
            track.decoded += 1;
            if track.title.is_empty() {
                track.title = title.to_owned();
            }
            if w > 0 && h > 0 {
                track.w = w;
                track.h = h;
            }
        }
    }

    /// Tears down (and forgets) the video window + feed for one member.
    /// Idempotent; bounded (feeder joins promptly, child reaped).
    pub fn close_video_window(&self, member: &str) {
        let mut window = {
            match self.inner.lock() {
                Ok(mut inner) => {
                    inner.video_feeds.remove(member);
                    inner.link_tracks.remove(member);
                    inner.video_windows.remove(member)
                }
                Err(_) => None,
            }
        };
        if let Some(window) = window.as_mut() {
            window.stop();
        }
    }

    /// Tears down all video windows. Idempotent; bounded.
    pub fn close_all_video_windows(&self) {
        let mut windows = match self.inner.lock() {
            Ok(mut inner) => {
                inner.video_feeds.clear();
                inner.link_tracks.clear();
                std::mem::take(&mut inner.video_windows)
            }
            Err(_) => return,
        };
        for (_, mut window) in windows.drain() {
            window.stop();
        }
    }

    // -- test-only e2e surface (all gated on the CLI plan) ----------------

    /// Returns the active self-driving plan, or `None` in normal runs.
    /// The frontend boots the e2e driver only when this is `Some`.
    pub fn get_e2e_plan(&self) -> Option<E2ePlan> {
        self.inner.lock().ok()?.e2e_plan.clone()
    }

    /// Writes a JSON status payload to the plan's status file. Gated on the
    /// plan; payloads carrying secret-looking keys are refused so artifacts
    /// stay clean even if the driver has a bug.
    pub fn e2e_status(&self, payload: &str) -> Result<(), String> {
        let plan = self.require_e2e_plan()?;
        if payload.len() > 4096 {
            return Err("e2e status payload too large".into());
        }
        let value: serde_json::Value =
            serde_json::from_str(payload).map_err(|_| "e2e status must be JSON".to_string())?;
        if !value.is_object() {
            return Err("e2e status must be a JSON object".into());
        }
        let lowered = payload.to_ascii_lowercase();
        for banned in ["password", "token", "sdp", "candidate"] {
            if lowered.contains(banned) {
                return Err(format!("e2e status must not contain '{banned}'"));
            }
        }
        let path = std::path::Path::new(&plan.status_file);
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("e2e status dir: {e}"))?;
            }
        }
        std::fs::write(path, payload).map_err(|e| format!("e2e status write: {e}"))?;
        Ok(())
    }

    /// Reads the room code file published by the host run. Gated on the
    /// plan; the code must look like a room code or it is rejected.
    pub fn e2e_read_code(&self) -> Result<String, String> {
        let plan = self.require_e2e_plan()?;
        let raw = std::fs::read_to_string(&plan.code_file)
            .map_err(|_| "e2e code not published yet".to_string())?;
        let code = raw.trim().to_owned();
        if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err("e2e code malformed".into());
        }
        Ok(code.to_uppercase())
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

/// Owner errors rendered without internals (states are enums, safe to show).
fn redact_owner(e: golive_core::owner::OwnerError) -> String {
    format!("{e:?}")
}

// ---------------------------------------------------------------------------
// Tauri commands: validated pass-through only.
// ---------------------------------------------------------------------------

#[tauri::command]
async fn create_room(
    state: State<'_, Arc<AppState>>,
    app: AppHandle,
    nickname: String,
    password: String,
) -> Result<String, String> {
    state.create_room(Some(app), &nickname, &password).await
}

#[tauri::command]
async fn join_room(
    state: State<'_, Arc<AppState>>,
    app: AppHandle,
    code: String,
    nickname: String,
    password: String,
) -> Result<String, String> {
    state
        .join_room(Some(app), &code, &nickname, &password)
        .await
}

#[tauri::command]
async fn leave(state: State<'_, Arc<AppState>>) -> Result<(), String> {
    state.leave().await
}

#[tauri::command]
async fn start_share(
    state: State<'_, Arc<AppState>>,
    app: AppHandle,
    source: String,
) -> Result<(), String> {
    state.start_share(Some(app), &source).await
}

#[tauri::command]
async fn stop_share(state: State<'_, Arc<AppState>>) -> Result<(), String> {
    state.stop_share().await
}

#[tauri::command]
async fn set_quality(
    state: State<'_, Arc<AppState>>,
    app: AppHandle,
    w: u32,
    h: u32,
    bitrate_kbps: u32,
    fps: u32,
    preset: Option<String>,
) -> Result<EffectiveQuality, String> {
    state
        .set_quality(Some(app), SetQualityArgs { w, h, bitrate_kbps, fps, preset })
        .await
}

#[tauri::command]
async fn list_sources(state: State<'_, Arc<AppState>>) -> Result<Vec<screen::ListedSource>, String> {
    state.list_sources().await
}

#[tauri::command]
fn source_capabilities(state: State<'_, Arc<AppState>>) -> screen::Capabilities {
    state.source_capabilities()
}

#[tauri::command]
async fn watch(state: State<'_, Arc<AppState>>, member: String) -> Result<(), String> {
    state.watch(&member).await
}

#[tauri::command]
async fn unwatch(state: State<'_, Arc<AppState>>, member: String) -> Result<(), String> {
    state.unwatch(&member).await
}

#[tauri::command]
fn get_snapshot(state: State<'_, Arc<AppState>>) -> Result<OwnerSnapshot, String> {
    state.get_snapshot()
}

#[tauri::command]
fn get_roster(state: State<'_, Arc<AppState>>) -> Vec<RosterMember> {
    state.get_roster()
}

#[tauri::command]
fn preview_source(state: State<'_, Arc<AppState>>, kind: String, id: String) -> SourcePreview {
    state.preview_source(&kind, &id)
}

#[tauri::command]
fn get_media_counters(state: State<'_, Arc<AppState>>) -> Result<MediaCounters, String> {
    state.get_media_counters()
}

#[tauri::command]
fn set_server(state: State<'_, Arc<AppState>>, base: String) -> Result<String, String> {
    state.set_server(&base)
}

#[tauri::command]
fn get_e2e_plan(state: State<'_, Arc<AppState>>) -> Option<E2ePlan> {
    state.get_e2e_plan()
}

#[tauri::command]
fn e2e_status(state: State<'_, Arc<AppState>>, payload: String) -> Result<(), String> {
    state.e2e_status(&payload)
}

#[tauri::command]
fn e2e_read_code(state: State<'_, Arc<AppState>>) -> Result<String, String> {
    state.e2e_read_code()
}

/// Tauri entry point with an explicit state (tests inject their own).
pub fn run_with(state: Arc<AppState>) {
    let log_state = Arc::clone(&state);
    tauri::Builder::default()
        .manage(state)
        .setup(move |app| {
            match app.path().app_log_dir() {
                Ok(dir) => log_state.set_session_log(session_log::SessionLog::init_in(&dir)),
                Err(e) => eprintln!("golive: log dir unavailable: {e}"),
            }
            log_state.session_log("session start".to_string());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            create_room,
            join_room,
            leave,
            start_share,
            stop_share,
            set_quality,
            list_sources,
            source_capabilities,
            watch,
            unwatch,
            get_snapshot,
            get_roster,
            preview_source,
            get_media_counters,
            set_server,
            get_e2e_plan,
            e2e_status,
            e2e_read_code,
        ])
        .run(tauri::generate_context!())
        .expect("tauri runtime");
}

/// Tauri entry point. Parses `--e2e-plan '<json>'` (absent = normal app).
pub fn run() {
    let state = Arc::new(AppState::new());
    match E2ePlan::from_args(std::env::args()) {
        Ok(Some(plan)) => {
            if let Err(e) = state.set_e2e_plan(plan) {
                eprintln!("e2e plan install failed: {e}");
                std::process::exit(2);
            }
        }
        Ok(None) => {}
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    }
    run_with(state);
}

#[cfg(test)]
mod e2e_plan_tests {
    use super::*;

    fn args(extra: &[&str]) -> impl Iterator<Item = String> {
        let mut v = vec!["golive-app".to_owned()];
        v.extend(extra.iter().map(|s| s.to_string()));
        v.into_iter()
    }

    const PLAN: &str = r#"{"role":"host","server":"http://127.0.0.1:1","password":"pw","nickname":"n","code_file":"c","status_file":"s"}"#;

    #[test]
    fn absent_flag_means_normal_app() {
        assert_eq!(E2ePlan::from_args(args(&[])).unwrap(), None);
        assert_eq!(E2ePlan::from_args(args(&["--other"])).unwrap(), None);
    }

    #[test]
    fn parses_host_and_viewer() {
        let plan = E2ePlan::from_args(args(&["--e2e-plan", PLAN]))
            .unwrap()
            .unwrap();
        assert_eq!(plan.role, "host");
        let viewer = PLAN.replace("\"host\"", "\"viewer\"");
        let plan = E2ePlan::from_args(args(&["--e2e-plan", &viewer]))
            .unwrap()
            .unwrap();
        assert_eq!(plan.role, "viewer");
    }

    #[test]
    fn rejects_bad_role_json_and_empty_fields() {
        assert!(E2ePlan::from_args(args(&["--e2e-plan"])).is_err());
        assert!(E2ePlan::from_args(args(&["--e2e-plan", "{}"])).is_err());
        let bad_role = PLAN.replace("\"host\"", "\"cameraman\"");
        assert!(E2ePlan::from_args(args(&["--e2e-plan", &bad_role])).is_err());
        let empty = PLAN.replace("\"n\"", "\"\"");
        assert!(E2ePlan::from_args(args(&["--e2e-plan", &empty])).is_err());
    }

    #[test]
    fn test_only_commands_need_a_plan() {
        let state = AppState::new();
        assert_eq!(state.get_e2e_plan(), None);
        assert_eq!(state.e2e_status("{}"), Err("e2e not active".to_string()));
        assert_eq!(state.e2e_read_code(), Err("e2e not active".to_string()));
    }

    #[test]
    fn status_refuses_secrets_and_non_objects() {
        let state = AppState::new();
        state
            .set_e2e_plan(
                E2ePlan::from_args(args(&["--e2e-plan", PLAN]))
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
        assert!(state.e2e_status("[1,2]").is_err());
        assert!(state.e2e_status(r#"{"token":"x"}"#).is_err());
        assert!(state.e2e_status(r#"{"sdp":"v=0"}"#).is_err());
        // Clean payloads to a temp file succeed.
        let dir = std::env::temp_dir().join("golive-e2e-unit");
        let _ = std::fs::create_dir_all(&dir);
        let status = dir.join("s.json");
        let mut plan = E2ePlan::from_args(args(&["--e2e-plan", PLAN])).unwrap().unwrap();
        plan.status_file = status.to_string_lossy().into_owned();
        let state = AppState::new();
        state.set_e2e_plan(plan).unwrap();
        state.e2e_status(r#"{"state":"ok"}"#).unwrap();
        assert_eq!(
            std::fs::read_to_string(&status).unwrap(),
            r#"{"state":"ok"}"#
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod share_source_tests {
    use super::*;

    #[test]
    fn parses_all_four_kinds() {
        assert!(matches!(ShareSource::parse("synthetic").unwrap(), ShareSource::Synthetic));
        assert!(matches!(
            ShareSource::parse("movie:/tmp/a.mp4").unwrap(),
            ShareSource::Movie(_)
        ));
        assert!(matches!(
            ShareSource::parse("display:1").unwrap(),
            ShareSource::Display(_)
        ));
        assert!(matches!(
            ShareSource::parse("window:42").unwrap(),
            ShareSource::Window(_)
        ));
    }

    #[test]
    fn rejects_empty_ids_and_unknown() {
        assert!(ShareSource::parse("display:").is_err());
        assert!(ShareSource::parse("display:   ").is_err());
        assert!(ShareSource::parse("window:").is_err());
        assert!(ShareSource::parse("movie:").is_err());
        assert!(ShareSource::parse("screen").is_err());
        assert!(ShareSource::parse("").is_err());
    }

    // NOTE: Display/Window must never resolve to synthetic silently. That
    // invariant holds structurally: start_share matches ShareSource
    // exhaustively with no wildcard arm, and both capture arms go through
    // screen::start_capture_for (OS-backed, typed errors). A silent fallback
    // cannot compile here without touching that match.
}

#[cfg(test)]
mod roster_tests {
    use super::*;

    #[test]
    fn offline_roster_pull_is_empty_never_errors() {
        // No signal client yet (fresh state): pull returns empty instead of
        // erroring — the event listener fills the roster in once WS connects.
        let state = AppState::new();
        assert!(state.get_roster().is_empty());
    }

    #[test]
    fn preview_unknown_kind_is_null_never_errors() {
        // No OS contact on this path: unknown kind short-circuits to a null
        // thumbnail (the modal lists sources regardless).
        let state = AppState::new();
        let preview = state.preview_source("bogus", "1");
        assert!(preview.data_url.is_none());
        assert_eq!((preview.w, preview.h), (0, 0));
    }
}

#[cfg(test)]
mod quality_tests {
    use super::*;
    use golive_core::media::EngineKind;
    use std::time::Duration;

    fn args(w: u32, h: u32, bitrate_kbps: u32, fps: u32) -> SetQualityArgs {
        SetQualityArgs { w, h, bitrate_kbps, fps, preset: None }
    }

    /// Wire regression: canonical snake_case and legacy camelCase spellings
    /// deserialize to the same args struct (kept tolerant for any caller).

    /// Payload tolerance: flat snake and legacy camel spellings bind.

    /// Seeds one live software publisher (synthetic) + default effective,
    /// bypassing room/signal: `set_quality` only touches publishers.
    /// Returns the state plus the publisher's event channel (proves IDR +
    /// fence without any forward task).
    async fn live_state() -> (
        Arc<AppState>,
        tokio::sync::mpsc::UnboundedReceiver<golive_core::media::MediaEvent>,
    ) {
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let publisher = Publisher::start_with_profile(
            VideoSource::SyntheticBall,
            Quality::P720.profile(),
            EngineKind::Software,
            None,
            event_tx,
        )
        .await
        .expect("test publisher starts");
        let state = Arc::new(AppState::new());
        {
            let mut inner = state.inner.lock().expect("state lock");
            inner.publishers.insert(
                "watcher".into(),
                PublishSession {
                    publisher: Arc::new(tokio::sync::Mutex::new(publisher)),
                    owner_fence: Fence::idle(),
                    wire: WireIds::default(),
                    remote_ready: false,
                    pending_remote: Vec::new(),
                },
            );
            inner.share_profile = Some(EffectiveQuality {
                profile: Quality::P720.profile(),
                generation: 0,
            });
        }
        (state, event_rx)
    }

    async fn recv_timeout(
        event_rx: &mut tokio::sync::mpsc::UnboundedReceiver<golive_core::media::MediaEvent>,
        secs: u64,
    ) -> Option<golive_core::media::MediaEvent> {
        tokio::time::timeout(Duration::from_secs(secs), event_rx.recv())
            .await
            .ok()
            .flatten()
    }

    #[tokio::test]
    async fn valid_applies_with_idr_and_fence_snapshot_reflects() {
        let (state, mut event_rx) = live_state().await;
        // Liveness first (initial IDR), so the post-reconfig IDR is
        // attributable below.
        let mut live = false;
        for _ in 0..150 {
            match recv_timeout(&mut event_rx, 1).await {
                Some(golive_core::media::MediaEvent::Keyframe) => {
                    live = true;
                    break;
                }
                Some(golive_core::media::MediaEvent::Error(detail)) => {
                    panic!("encode failed: {detail}")
                }
                _ => {}
            }
        }
        assert!(live, "stream alive before set_quality");
        // Valid profile applies through the command method (no AppHandle).
        let effective = state
            .set_quality(None, args(640, 360, 1000, 15))
            .await
            .expect("valid profile applies");
        assert_eq!((effective.profile.w, effective.profile.h), (640, 360));
        assert_eq!(effective.profile.bitrate_kbps, 1000);
        // Fence + forced IDR land on the publisher's own channel.
        let mut gen_seen = false;
        let mut post_idr = false;
        for _ in 0..200 {
            match recv_timeout(&mut event_rx, 1).await {
                Some(golive_core::media::MediaEvent::Stats(stats)) => {
                    if stats.generation == 1 {
                        gen_seen = true;
                    }
                }
                Some(golive_core::media::MediaEvent::Keyframe) => {
                    if gen_seen {
                        post_idr = true;
                        break;
                    }
                }
                _ => {}
            }
            if gen_seen && post_idr {
                break;
            }
        }
        assert!(gen_seen, "generation fence bumps");
        assert!(post_idr, "forced IDR after apply");
        // Snapshot reflects the effective profile (generation follows via
        // the forward task; the stored profile is authoritative here) plus
        // the live encode backend for the UI badge/diagnostics.
        let counters = state.get_media_counters().expect("counters");
        let stored = counters.effective.expect("effective present");
        assert_eq!((stored.profile.w, stored.profile.h), (640, 360));
        assert_eq!(counters.backend.as_deref(), Some("openh264"));
        // Teardown: stop the publisher explicitly (encode thread joins).
        let publisher = {
            state
                .inner
                .lock()
                .expect("state lock")
                .publishers
                .remove("watcher")
                .map(|session| session.publisher)
        };
        if let Some(publisher) = publisher {
            publisher.lock().await.stop().await;
        }
    }

    #[tokio::test]
    async fn invalid_rejects_without_touching_stream() {
        let (state, _event_rx) = live_state().await;
        // Odd dims are ACCEPTED (core floors to even in the encoder); only
        // ranges + unknown presets reject. Errors are typed + redacted.
        let odd = state
            .set_quality(None, args(641, 360, 1000, 15))
            .await
            .expect("odd dims accepted, normalized downstream");
        assert_eq!(odd.profile.w, 641);
        // Out-of-range bitrate/fps + unknown preset reject without touching
        // the stream.
        let err = state
            .set_quality(None, args(640, 360, 50, 15))
            .await
            .expect_err("bitrate range rejected");
        assert!(err.starts_with("qualidade:"), "{err}");
        assert!(state.set_quality(None, args(640, 360, 50, 15)).await.is_err());
        assert!(state.set_quality(None, args(640, 360, 1000, 0)).await.is_err());
        assert!(
            state
                .set_quality(
                    None,
                    SetQualityArgs { preset: Some("ultra".into()), ..args(640, 360, 1000, 15) }
                )
                .await
                .is_err()
        );
        // Effective tracks the last ACCEPTED profile (odd included); the
        // rejects above left it untouched, and the encoder still runs:
        // a later valid switch applies cleanly.
        let stored = state
            .get_media_counters()
            .expect("counters")
            .effective
            .expect("effective present");
        assert_eq!((stored.profile.w, stored.profile.h), (641, 360));
        assert_eq!(stored.generation, 0);
        state
            .set_quality(None, args(480, 270, 800, 15))
            .await
            .expect("encoder alive: later valid applies");
        let stored = state
            .get_media_counters()
            .expect("counters")
            .effective
            .expect("effective present");
        assert_eq!((stored.profile.w, stored.profile.h), (480, 270));
        let publisher = {
            state
                .inner
                .lock()
                .expect("state lock")
                .publishers
                .remove("watcher")
                .map(|session| session.publisher)
        };
        if let Some(publisher) = publisher {
            publisher.lock().await.stop().await;
        }
    }

    #[tokio::test]
    async fn rejects_when_not_sharing() {
        let state = Arc::new(AppState::new());
        let err = state
            .set_quality(None, args(640, 360, 1000, 15))
            .await
            .expect_err("no publishers");
        assert_eq!(err, "not sharing");
        assert!(state.get_media_counters().expect("counters").effective.is_none());
    }

    #[test]
    fn backend_note_triages_fallback_deterministically() {
        // Hardware and absence carry no note.
        assert_eq!(backend_note_for(None), None);
        assert_eq!(backend_note_for(Some("videotoolbox")), None);
        assert_eq!(backend_note_for(Some("whatever")), None);
        // Software fallback always explains itself, never with secrets.
        let hook_was_set = std::env::var_os("GOLIVE_DISABLE_HW").is_some();
        std::env::set_var("GOLIVE_DISABLE_HW", "1");
        let hooked = backend_note_for(Some("openh264")).expect("note");
        assert!(hooked.contains("GOLIVE_DISABLE_HW"), "{hooked}");
        if hook_was_set {
            std::env::set_var("GOLIVE_DISABLE_HW", "1");
        } else {
            std::env::remove_var("GOLIVE_DISABLE_HW");
        }
        let plain = backend_note_for(Some("openh264")).expect("note");
        assert!(!plain.is_empty());
        for note in [hooked, plain] {
            let lower = note.to_lowercase();
            for banned in ["sdp", "candidate", "token", "password", "192.168"] {
                assert!(!lower.contains(banned), "secret-adjacent in note: {note}");
            }
        }
    }
}
