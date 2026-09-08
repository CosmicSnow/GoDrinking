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
pub mod video;

use golive_core::media::{NativeViewer, Publisher, Quality, VideoSource};
use golive_core::owner::{Fence, Owner, OwnerSnapshot};
use golive_core::signal::SignalClient;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, State};
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
                screen_bridge: None,
            }),
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
        Ok(())
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
                counters
            })
    }

    /// Tears down (and forgets) the video window + feed for one member.
    /// Idempotent; bounded (feeder joins promptly, child reaped).
    pub fn close_video_window(&self, member: &str) {
        let mut window = {
            match self.inner.lock() {
                Ok(mut inner) => {
                    inner.video_feeds.remove(member);
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
    tauri::Builder::default()
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            create_room,
            join_room,
            leave,
            start_share,
            stop_share,
            list_sources,
            source_capabilities,
            watch,
            unwatch,
            get_snapshot,
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
