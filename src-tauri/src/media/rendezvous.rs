//! Stunar client: HTTPS + WebSocket to the Rendezvous (PROTOCOL.md §C).
//!
//! Host side (`StunarHost`): opens a room, heartbeats every 30s, keeps one
//! WS inbox open, and forwards viewer answers into the engine's answer
//! mailbox. Offers are minted by the engine and sent through the WS.
//!
//! Viewer side (`discover_stunar_room` / `submit_stunar_answer`): asks,
//! waits on the WS for accepted + offer (up to 65s), then sends the answer
//! signal. Media never touches the Rendezvous.

use super::logger;
use super::peer_transport::{PeerSignal, PeerSignalKind};
use super::types::StunarState;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
const WS_RECONNECT_DELAY: Duration = Duration::from_secs(2);
const VIEWER_WAIT_TIMEOUT: Duration = Duration::from_secs(65);
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|error| error.to_string())
}

fn normalize_base(base: &str) -> String {
    base.trim().trim_end_matches('/').to_owned()
}

fn ws_url(base: &str, role: &str, token: &str) -> Result<String, String> {
    let base = normalize_base(base);
    let (scheme, rest) = if let Some(rest) = base.strip_prefix("https://") {
        ("wss", rest)
    } else if let Some(rest) = base.strip_prefix("http://") {
        ("ws", rest)
    } else {
        return Err("invalid rendezvous URL".into());
    };
    Ok(format!("{scheme}://{rest}/v1/ws?role={role}&token={token}"))
}

async fn connect_ws(url: &str) -> Result<WsStream, String> {
    let (ws, _) = connect_async(url).await.map_err(|error| error.to_string())?;
    Ok(ws)
}

fn current_thread_runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())
}

// --- Host ------------------------------------------------------------------

enum Outgoing {
    Signal {
        viewer_id: String,
        to: Option<String>,
        payload: serde_json::Value,
    },
    Share {
        start: bool,
    },
    Watch {
        to: String,
        start: bool,
    },
    Close,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct StunarIncomingOffer {
    pub from: String,
    pub sdp: String,
}

#[derive(Clone, Debug, Default)]
struct WsInbox {
    roster: Option<HashMap<String, RosterViewer>>,
    master_id: Option<String>,
    answers: Vec<PeerSignal>,
    offers: Vec<StunarIncomingOffer>,
    watch_from: Vec<String>,
    unwatch_from: Vec<String>,
    you_are_master: bool,
    gone: bool,
    kicked: bool,
}

fn apply_ws_message(text: &str, inbox: &mut WsInbox) {
    let Ok(msg) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    let Some(kind) = msg["t"].as_str() else {
        return;
    };
    match kind {
        "pending" => {
            if let (Some(id), Some(nickname)) =
                (msg["viewer_id"].as_str(), msg["nickname"].as_str())
            {
                let mut roster = inbox.roster.take().unwrap_or_default();
                roster.insert(
                    id.to_owned(),
                    RosterViewer {
                        nickname: nickname.to_owned(),
                        state: "pending".into(),
                        master: false,
                        share: false,
                    },
                );
                inbox.roster = Some(roster);
            }
        }
        "roster" => {
            let mut next = HashMap::new();
            if let Some(entries) = msg["entries"].as_array() {
                for entry in entries {
                    if let (Some(id), Some(nickname)) =
                        (entry["id"].as_str(), entry["nickname"].as_str())
                    {
                        let state = entry["state"].as_str().unwrap_or("accepted");
                        next.insert(
                            id.to_owned(),
                            RosterViewer {
                                nickname: nickname.to_owned(),
                                state: state.to_owned(),
                                master: entry["master"].as_bool().unwrap_or(false),
                                share: entry["share"].as_bool().unwrap_or(false)
                                    || state == "sharing",
                            },
                        );
                    }
                }
            }
            inbox.master_id = msg["master_id"].as_str().map(str::to_owned);
            inbox.roster = Some(next);
        }
        "signal" => {
            let payload = &msg["payload"];
            let sdp = payload.get("sdp").and_then(|sdp| sdp.as_str());
            let kind = payload.get("type").and_then(|kind| kind.as_str());
            let from = msg["from"]
                .as_str()
                .or_else(|| msg["viewer_id"].as_str())
                .unwrap_or("");
            let Some(sdp) = sdp else { return };
            if kind == Some("answer") {
                inbox.answers.push(PeerSignal {
                    kind: PeerSignalKind::Answer,
                    sdp: sdp.to_owned(),
                    id: if from.is_empty() {
                        None
                    } else {
                        Some(from.to_owned())
                    },
                });
            } else if kind == Some("offer") && !from.is_empty() {
                inbox.offers.push(StunarIncomingOffer {
                    from: from.to_owned(),
                    sdp: sdp.to_owned(),
                });
            }
        }
        "you-are-master" => inbox.you_are_master = true,
        "gone" => inbox.gone = true,
        "kicked" | "rejected" => inbox.kicked = true,
        "watch" => {
            if let Some(from) = msg["from"].as_str() {
                inbox.watch_from.push(from.to_owned());
            }
        }
        "unwatch" => {
            if let Some(from) = msg["from"].as_str() {
                inbox.unwatch_from.push(from.to_owned());
            }
        }
        _ => {}
    }
}

/// The Host's Stunar connection: open + heartbeat + WS inbox.
pub(crate) struct StunarHost {
    base: String,
    /// Room code assigned by the Rendezvous at open. Server-owned: the Host
    /// never chooses or rotates it; it lives until the room dies.
    code: Mutex<String>,
    host_token: String,
    state: Arc<Mutex<StunarState>>,
    /// viewer_id -> (nickname, "pending" | "accepted"), synced from the
    /// Rendezvous roster (authoritative) and the pending/decide messages.
    roster: Arc<Mutex<HashMap<String, RosterViewer>>>,
    answers: Arc<Mutex<Vec<PeerSignal>>>,
    incoming_offers: Arc<Mutex<Vec<StunarIncomingOffer>>>,
    watch_from: Arc<Mutex<Vec<String>>>,
    unwatch_from: Arc<Mutex<Vec<String>>>,
    master_id: Arc<Mutex<Option<String>>>,
    pub(crate) self_id: Option<String>,
    outgoing: UnboundedSender<Outgoing>,
    shutdown: Arc<AtomicBool>,
    _worker: Option<JoinHandle<()>>,
}

#[derive(Clone, Debug)]
struct RosterViewer {
    nickname: String,
    state: String,
    master: bool,
    share: bool,
}

impl StunarHost {
    /// Opens the room on the Rendezvous and starts the heartbeat + WS worker.
    /// The Rendezvous generates the Room code and returns it with the
    /// host_token; the caller reads it back via `code()`.
    pub(crate) fn start(
        base: &str,
        password: &str,
        nickname: &str,
        admission: bool,
        session_mode: super::room_mode::SessionMode,
    ) -> Result<Self, String> {
        logger::begin_session("host", "stunar");
        logger::log(
            "INFO",
            "stunar open",
            &format!("base={base} nickname={nickname} admission={admission} mode={session_mode:?}"),
        );
        let runtime = current_thread_runtime()?;
        let client = http_client()?;
        let (host_token, code, self_id) =
            runtime.block_on(host_open(&client, base, password, nickname, admission, session_mode))?;
        let state = Arc::new(Mutex::new(StunarState::Calling));
        let roster = Arc::new(Mutex::new(HashMap::new()));
        let answers = Arc::new(Mutex::new(Vec::new()));
        let incoming_offers = Arc::new(Mutex::new(Vec::new()));
        let watch_from = Arc::new(Mutex::new(Vec::new()));
        let unwatch_from = Arc::new(Mutex::new(Vec::new()));
        let master_id = Arc::new(Mutex::new(None));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (outgoing_tx, outgoing_rx) = unbounded_channel();
        let worker_base = base.to_owned();
        let worker_token = host_token.clone();
        let worker_state = Arc::clone(&state);
        let worker_roster = Arc::clone(&roster);
        let worker_answers = Arc::clone(&answers);
        let worker_offers = Arc::clone(&incoming_offers);
        let worker_watch = Arc::clone(&watch_from);
        let worker_unwatch = Arc::clone(&unwatch_from);
        let worker_master = Arc::clone(&master_id);
        let worker_shutdown = Arc::clone(&shutdown);
        let worker = thread::Builder::new()
            .name("godrinking-stunar-host".into())
            .spawn(move || {
                let Ok(runtime) = current_thread_runtime() else {
                    return;
                };
                let _ = runtime.block_on(host_worker(
                    worker_base,
                    worker_token,
                    worker_state,
                    worker_roster,
                    worker_answers,
                    worker_offers,
                    worker_watch,
                    worker_unwatch,
                    worker_master,
                    worker_shutdown,
                    outgoing_rx,
                ));
            })
            .map_err(|error| error.to_string())?;
        Ok(Self {
            base: base.to_owned(),
            code: Mutex::new(code),
            host_token,
            state,
            roster,
            answers,
            incoming_offers,
            watch_from,
            unwatch_from,
            master_id,
            self_id,
            outgoing: outgoing_tx,
            shutdown,
            _worker: Some(worker),
        })
    }

    pub(crate) fn code(&self) -> String {
        self.code
            .lock()
            .map(|code| code.clone())
            .unwrap_or_default()
    }

    pub(crate) fn state(&self) -> StunarState {
        self.state
            .lock()
            .map(|state| *state)
            .unwrap_or(StunarState::Unreachable)
    }

    pub(crate) fn take_answers(&self) -> Vec<PeerSignal> {
        self.answers
            .lock()
            .map(|mut answers| std::mem::take(&mut *answers))
            .unwrap_or_default()
    }

    pub(crate) fn pending_roster(&self) -> Vec<(String, String)> {
        self.roster
            .lock()
            .map(|roster| {
                roster
                    .iter()
                    .filter(|(_, viewer)| viewer.state == "pending")
                    .map(|(id, viewer)| (id.clone(), viewer.nickname.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Accepted Viewers known to the Rendezvous. The engine mints an offer
    /// for any of these that does not have a ViewerLink yet (Admission off
    /// accepts immediately, so there is no pending step to trigger the mint).
    pub(crate) fn accepted_roster(&self) -> Vec<(String, String)> {
        self.roster
            .lock()
            .map(|roster| {
                roster
                    .iter()
                    .filter(|(_, viewer)| {
                        viewer.state == "accepted" || viewer.state == "sharing"
                    })
                    .map(|(id, viewer)| (id.clone(), viewer.nickname.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) fn pending_nickname(&self, id: &str) -> Option<String> {
        self.roster.lock().ok().and_then(|roster| {
            roster
                .get(id)
                .filter(|viewer| viewer.state == "pending")
                .map(|viewer| viewer.nickname.clone())
        })
    }

    /// Accepts or rejects a pending Viewer on the Rendezvous. On success the
    /// roster entry moves to accepted (or disappears); the engine mints the
    /// offer for accepted ones.
    pub(crate) fn decide(&self, id: &str, accept: bool) -> Result<(), String> {
        let action = if accept { "accept" } else { "reject" };
        logger::log("INFO", "stunar decide", &format!("viewer={id} action={action}"));
        let runtime = current_thread_runtime()?;
        let client = http_client()?;
        runtime.block_on(post_decide(&client, &self.base, &self.host_token, id, action))?;
        if let Ok(mut roster) = self.roster.lock() {
            if accept {
                if let Some(viewer) = roster.get_mut(id) {
                    viewer.state = "accepted".into();
                }
            } else {
                roster.remove(id);
            }
        }
        Ok(())
    }

    pub(crate) fn kick(&self, id: &str) -> Result<(), String> {
        logger::log("INFO", "stunar kick", &format!("viewer={id}"));
        let runtime = current_thread_runtime()?;
        let client = http_client()?;
        runtime.block_on(post_decide(&client, &self.base, &self.host_token, id, "kick"))
    }

    /// Rotates the Password on the Rendezvous. `None` keeps the current
    /// value. The Room code is server-owned and never rotates. Connected
    /// Viewers keep their tokens; the WS is not touched.
    pub(crate) fn rotate(&self, password: Option<&str>) -> Result<(), String> {
        logger::log("INFO", "stunar rotate", "password rotated");
        let runtime = current_thread_runtime()?;
        let client = http_client()?;
        runtime.block_on(post_rotate(&client, &self.base, &self.host_token, password))
    }

    /// Sends an offer signal to an accepted Viewer over the WS.
    pub(crate) fn send_signal(&self, viewer_id: &str, signal: &PeerSignal) -> Result<(), String> {
        let kind = match signal.kind {
            PeerSignalKind::Offer => "offer",
            PeerSignalKind::Answer => "answer",
        };
        let payload = json!({ "type": kind, "sdp": signal.sdp });
        self.outgoing
            .send(Outgoing::Signal {
                viewer_id: viewer_id.to_owned(),
                to: Some(viewer_id.to_owned()),
                payload,
            })
            .map_err(|_| "Stunar is unreachable.".to_owned())
    }

    pub(crate) fn send_share(&self, start: bool) -> Result<(), String> {
        self.outgoing
            .send(Outgoing::Share { start })
            .map_err(|_| "Stunar is unreachable.".to_owned())
    }

    pub(crate) fn send_watch(&self, to: &str, start: bool) -> Result<(), String> {
        self.outgoing
            .send(Outgoing::Watch {
                to: to.to_owned(),
                start,
            })
            .map_err(|_| "Stunar is unreachable.".to_owned())
    }

    pub(crate) fn take_incoming_offers(&self) -> Vec<StunarIncomingOffer> {
        self.incoming_offers
            .lock()
            .map(|mut offers| std::mem::take(&mut *offers))
            .unwrap_or_default()
    }

    pub(crate) fn take_watch_requests(&self, take_watch: bool) -> (Vec<String>, Vec<String>) {
        let watch = if take_watch {
            self.watch_from
                .lock()
                .map(|mut list| std::mem::take(&mut *list))
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let unwatch = self
            .unwatch_from
            .lock()
            .map(|mut list| std::mem::take(&mut *list))
            .unwrap_or_default();
        (watch, unwatch)
    }

    pub(crate) fn nickname_of(&self, id: &str) -> Option<String> {
        self.roster
            .lock()
            .ok()
            .and_then(|roster| roster.get(id).map(|viewer| viewer.nickname.clone()))
    }

    pub(crate) fn master_id(&self) -> Option<String> {
        self.master_id.lock().ok().and_then(|id| id.clone())
    }

    pub(crate) fn room_roster(&self) -> Vec<(String, String, bool, bool)> {
        self.roster
            .lock()
            .map(|roster| {
                roster
                    .iter()
                    .map(|(id, viewer)| {
                        (
                            id.clone(),
                            viewer.nickname.clone(),
                            viewer.master,
                            viewer.share,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Closes the room on the Rendezvous and stops the worker.
    pub(crate) fn close(&self) {
        logger::log("INFO", "stunar close", "room closed");
        self.shutdown.store(true, Ordering::Release);
        let _ = self.outgoing.send(Outgoing::Close);
        if let Ok(runtime) = current_thread_runtime() {
            if let Ok(client) = http_client() {
                let _ = runtime.block_on(post_close(&client, &self.base, &self.host_token));
            }
        }
    }
}

impl Drop for StunarHost {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = self.outgoing.send(Outgoing::Close);
    }
}

async fn host_open(
    client: &reqwest::Client,
    base: &str,
    password: &str,
    nickname: &str,
    admission: bool,
    session_mode: super::room_mode::SessionMode,
) -> Result<(String, String, Option<String>), String> {
    let base = normalize_base(base);
    // The server generates the Room code; the Host never sends one.
    let mode = match session_mode {
        super::room_mode::SessionMode::Room => "room",
        super::room_mode::SessionMode::Broadcast => "broadcast",
    };
    let body = json!({ "password": password, "nickname": nickname, "admission": admission, "mode": mode });
    let response = client
        .post(format!("{base}/v1/host/open"))
        .json(&body)
        .send()
        .await
        .map_err(|_| "Stunar is unreachable.".to_owned())?;
    let status = response.status();
    let json: serde_json::Value = response
        .json()
        .await
        .map_err(|_| "Stunar is unreachable.".to_owned())?;
    if status.is_success() && json["ok"] == true {
        let host_token = json["host_token"]
            .as_str()
            .map(|token| token.to_owned())
            .ok_or_else(|| "Stunar is unreachable.".to_owned())?;
        let code = json["code"]
            .as_str()
            .map(|code| code.to_owned())
            .ok_or_else(|| "Stunar is unreachable.".to_owned())?;
        let member_id = json["member_id"].as_str().map(str::to_owned);
        logger::log(
            "INFO",
            "stunar open response",
            &format!("status={status} code={code}"),
        );
        Ok((host_token, code, member_id))
    } else {
        let raw = json["error"].as_str().unwrap_or("unknown");
        logger::log(
            "ERROR",
            "stunar open response",
            &format!("status={status} error={raw}"),
        );
        Err("Stunar is unreachable.".into())
    }
}

async fn post_heartbeat(
    client: &reqwest::Client,
    base: &str,
    host_token: &str,
) -> Result<(), String> {
    let base = normalize_base(base);
    let response = client
        .post(format!("{base}/v1/host/heartbeat"))
        .json(&json!({ "host_token": host_token }))
        .send()
        .await
        .map_err(|_| "Stunar is unreachable.".to_owned())?;
    if response.status().is_success() {
        Ok(())
    } else {
        logger::log(
            "WARN",
            "stunar heartbeat",
            &format!("status={}", response.status()),
        );
        Err("Stunar is unreachable.".into())
    }
}

async fn post_member_heartbeat(
    client: &reqwest::Client,
    base: &str,
    token: &str,
) -> Result<(), String> {
    let base = normalize_base(base);
    let response = client
        .post(format!("{base}/v1/member/heartbeat"))
        .json(&json!({ "token": token }))
        .send()
        .await
        .map_err(|_| "Stunar is unreachable.".to_owned())?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err("Stunar is unreachable.".into())
    }
}

/// Best-effort explicit leave so the roster drops this viewer immediately
/// instead of lingering as a ghost until the heartbeat TTL sweeps it.
async fn post_member_leave(client: &reqwest::Client, base: &str, token: &str) -> Result<(), String> {
    let base = normalize_base(base);
    let response = client
        .post(format!("{base}/v1/member/leave"))
        .json(&json!({ "token": token }))
        .send()
        .await
        .map_err(|_| "Stunar is unreachable.".to_owned())?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err("Stunar is unreachable.".into())
    }
}

async fn post_decide(
    client: &reqwest::Client,
    base: &str,
    host_token: &str,
    viewer_id: &str,
    action: &str,
) -> Result<(), String> {
    let base = normalize_base(base);
    let response = client
        .post(format!("{base}/v1/host/decide"))
        .json(&json!({ "host_token": host_token, "viewer_id": viewer_id, "action": action }))
        .send()
        .await
        .map_err(|_| "Stunar is unreachable.".to_owned())?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err("Stunar is unreachable.".into())
    }
}

async fn post_rotate(
    client: &reqwest::Client,
    base: &str,
    host_token: &str,
    password: Option<&str>,
) -> Result<(), String> {
    let base = normalize_base(base);
    // The code is server-owned; rotate only changes the Password.
    let mut body = json!({ "host_token": host_token });
    if let Some(password) = password {
        body["password"] = json!(password);
    }
    let response = client
        .post(format!("{base}/v1/host/rotate"))
        .json(&body)
        .send()
        .await
        .map_err(|_| "Stunar is unreachable.".to_owned())?;
    if response.status().is_success() {
        return Ok(());
    }
    Err("Stunar is unreachable.".into())
}

async fn post_close(client: &reqwest::Client, base: &str, host_token: &str) -> Result<(), String> {
    let base = normalize_base(base);
    let response = client
        .post(format!("{base}/v1/host/close"))
        .json(&json!({ "host_token": host_token }))
        .send()
        .await
        .map_err(|_| "Stunar is unreachable.".to_owned())?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err("Stunar is unreachable.".into())
    }
}

async fn host_worker(
    base: String,
    host_token: String,
    state: Arc<Mutex<StunarState>>,
    roster: Arc<Mutex<HashMap<String, RosterViewer>>>,
    answers: Arc<Mutex<Vec<PeerSignal>>>,
    incoming_offers: Arc<Mutex<Vec<StunarIncomingOffer>>>,
    watch_from: Arc<Mutex<Vec<String>>>,
    unwatch_from: Arc<Mutex<Vec<String>>>,
    master_id: Arc<Mutex<Option<String>>>,
    shutdown: Arc<AtomicBool>,
    mut outgoing: UnboundedReceiver<Outgoing>,
) {
    let client = http_client().ok();
    let hb_base = base.clone();
    let hb_token = host_token.clone();
    let hb_state = Arc::clone(&state);
    let hb_shutdown = Arc::clone(&shutdown);
    let heartbeat = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(HEARTBEAT_INTERVAL);
        ticker.tick().await; // skip the immediate first tick
        loop {
            ticker.tick().await;
            if hb_shutdown.load(Ordering::Acquire) {
                break;
            }
            let ok = match &client {
                Some(client) => post_heartbeat(client, &hb_base, &hb_token).await.is_ok(),
                None => false,
            };
            if !ok {
                if let Ok(mut state) = hb_state.lock() {
                    *state = StunarState::Unreachable;
                }
                logger::log("WARN", "stunar heartbeat", "failed; relay marked unreachable");
            }
        }
    });
    let ws_url = match ws_url(&base, "host", &host_token) {
        Ok(url) => url,
        Err(_) => {
            heartbeat.abort();
            return;
        }
    };
    while !shutdown.load(Ordering::Acquire) {
        match connect_ws(&ws_url).await {
            Ok(mut ws) => {
                logger::log("INFO", "stunar ws", "connected (host inbox)");
                if let Ok(mut state) = state.lock() {
                    *state = StunarState::Live;
                }
                loop {
                    tokio::select! {
                        incoming = ws.next() => {
                            match incoming {
                                Some(Ok(Message::Text(text))) => {
                                    handle_host_message(
                                        &text,
                                        &roster,
                                        &answers,
                                        &incoming_offers,
                                        &watch_from,
                                        &unwatch_from,
                                        &master_id,
                                    );
                                }
                                Some(Ok(Message::Ping(_))) => {
                                    let _ = ws.send(Message::Pong(Vec::new().into())).await;
                                }
                                Some(Ok(_)) => {}
                                Some(Err(_)) | None => break,
                            }
                        }
                        outgoing = outgoing.recv() => {
                            match outgoing {
                                Some(Outgoing::Signal { viewer_id, to, payload }) => {
                                    let mut body = json!({
                                        "t": "signal",
                                        "viewer_id": viewer_id,
                                        "payload": payload,
                                    });
                                    if let Some(to) = to {
                                        body["to"] = json!(to);
                                    }
                                    if ws.send(Message::Text(body.to_string().into())).await.is_err() {
                                        break;
                                    }
                                }
                                Some(Outgoing::Share { start }) => {
                                    let text = json!({
                                        "t": if start { "share-start" } else { "share-stop" },
                                    })
                                    .to_string();
                                    if ws.send(Message::Text(text.into())).await.is_err() {
                                        break;
                                    }
                                }
                                Some(Outgoing::Watch { to, start }) => {
                                    let text = json!({
                                        "t": if start { "watch" } else { "unwatch" },
                                        "to": to,
                                    })
                                    .to_string();
                                    if ws.send(Message::Text(text.into())).await.is_err() {
                                        break;
                                    }
                                }
                                Some(Outgoing::Close) => {
                                    let _ = ws.close(None).await;
                                    heartbeat.abort();
                                    return;
                                }
                                None => {
                                    heartbeat.abort();
                                    return;
                                }
                            }
                        }
                    }
                }
            }
            Err(error) => {
                logger::log("WARN", "stunar ws", &format!("connect failed: {error}"));
                if let Ok(mut state) = state.lock() {
                    *state = StunarState::Unreachable;
                }
            }
        }
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        logger::log("WARN", "stunar ws", "closed; reconnecting");
        tokio::time::sleep(WS_RECONNECT_DELAY).await;
    }
    heartbeat.abort();
}

fn apply_inbox_side_effects(
    inbox: WsInbox,
    roster: &Arc<Mutex<HashMap<String, RosterViewer>>>,
    answers: &Arc<Mutex<Vec<PeerSignal>>>,
    incoming_offers: &Arc<Mutex<Vec<StunarIncomingOffer>>>,
    watch_from: &Arc<Mutex<Vec<String>>>,
    unwatch_from: &Arc<Mutex<Vec<String>>>,
    master_id: &Arc<Mutex<Option<String>>>,
) {
    if let Some(next) = inbox.roster {
        logger::log(
            "INFO",
            "stunar ws message",
            &format!("roster entries={}", next.len()),
        );
        if let Ok(mut roster) = roster.lock() {
            *roster = next;
        }
    }
    if inbox.master_id.is_some() {
        if let Ok(mut slot) = master_id.lock() {
            *slot = inbox.master_id;
        }
    }
    if !inbox.answers.is_empty() {
        if let Ok(mut answers) = answers.lock() {
            answers.extend(inbox.answers);
        }
    }
    if !inbox.offers.is_empty() {
        logger::log(
            "INFO",
            "stunar ws message",
            &format!("incoming offers={}", inbox.offers.len()),
        );
        if let Ok(mut offers) = incoming_offers.lock() {
            offers.extend(inbox.offers);
        }
    }
    if !inbox.watch_from.is_empty() {
        if let Ok(mut list) = watch_from.lock() {
            list.extend(inbox.watch_from);
        }
    }
    if !inbox.unwatch_from.is_empty() {
        if let Ok(mut list) = unwatch_from.lock() {
            list.extend(inbox.unwatch_from);
        }
    }
    if inbox.gone {
        logger::log("WARN", "stunar ws message", "gone (room died)");
    }
}

fn handle_host_message(
    text: &str,
    roster: &Arc<Mutex<HashMap<String, RosterViewer>>>,
    answers: &Arc<Mutex<Vec<PeerSignal>>>,
    incoming_offers: &Arc<Mutex<Vec<StunarIncomingOffer>>>,
    watch_from: &Arc<Mutex<Vec<String>>>,
    unwatch_from: &Arc<Mutex<Vec<String>>>,
    master_id: &Arc<Mutex<Option<String>>>,
) {
    let mut inbox = WsInbox::default();
    apply_ws_message(text, &mut inbox);
    apply_inbox_side_effects(
        inbox,
        roster,
        answers,
        incoming_offers,
        watch_from,
        unwatch_from,
        master_id,
    );
}

// --- Viewer ----------------------------------------------------------------

/// The Viewer's Stunar WS connection. After the first offer, a worker keeps
/// the socket open so Sala members can receive more offers and send their own.
pub(crate) struct StunarViewer {
    outgoing: UnboundedSender<Outgoing>,
    incoming_offers: Arc<Mutex<Vec<StunarIncomingOffer>>>,
    answers: Arc<Mutex<Vec<PeerSignal>>>,
    roster: Arc<Mutex<HashMap<String, RosterViewer>>>,
    watch_from: Arc<Mutex<Vec<String>>>,
    unwatch_from: Arc<Mutex<Vec<String>>>,
    master_id: Arc<Mutex<Option<String>>>,
    shutdown: Arc<AtomicBool>,
    base: String,
    pub(crate) token: String,
    pub(crate) member_id: Option<String>,
    pub(crate) mode: String,
    _worker: Option<JoinHandle<()>>,
}

impl StunarViewer {
    pub(crate) fn send_signal(&self, to: &str, signal: &PeerSignal) -> Result<(), String> {
        let kind = match signal.kind {
            PeerSignalKind::Offer => "offer",
            PeerSignalKind::Answer => "answer",
        };
        let payload = json!({ "type": kind, "sdp": signal.sdp });
        self.outgoing
            .send(Outgoing::Signal {
                viewer_id: to.to_owned(),
                to: if to.is_empty() { None } else { Some(to.to_owned()) },
                payload,
            })
            .map_err(|_| "Stunar is unreachable.".to_owned())
    }

    pub(crate) fn send_share(&self, start: bool) -> Result<(), String> {
        self.outgoing
            .send(Outgoing::Share { start })
            .map_err(|_| "Stunar is unreachable.".to_owned())
    }

    pub(crate) fn send_watch(&self, to: &str, start: bool) -> Result<(), String> {
        self.outgoing
            .send(Outgoing::Watch {
                to: to.to_owned(),
                start,
            })
            .map_err(|_| "Stunar is unreachable.".to_owned())
    }

    pub(crate) fn take_incoming_offers(&self) -> Vec<StunarIncomingOffer> {
        self.incoming_offers
            .lock()
            .map(|mut offers| std::mem::take(&mut *offers))
            .unwrap_or_default()
    }

    pub(crate) fn take_watch_requests(&self, take_watch: bool) -> (Vec<String>, Vec<String>) {
        let watch = if take_watch {
            self.watch_from
                .lock()
                .map(|mut list| std::mem::take(&mut *list))
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let unwatch = self
            .unwatch_from
            .lock()
            .map(|mut list| std::mem::take(&mut *list))
            .unwrap_or_default();
        (watch, unwatch)
    }

    pub(crate) fn nickname_of(&self, id: &str) -> Option<String> {
        self.roster
            .lock()
            .ok()
            .and_then(|roster| roster.get(id).map(|viewer| viewer.nickname.clone()))
    }

    pub(crate) fn room_roster(&self) -> Vec<(String, String, bool, bool)> {
        self.roster
            .lock()
            .map(|roster| {
                roster
                    .iter()
                    .map(|(id, viewer)| {
                        (
                            id.clone(),
                            viewer.nickname.clone(),
                            viewer.master,
                            viewer.share,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) fn take_answers(&self) -> Vec<PeerSignal> {
        self.answers
            .lock()
            .map(|mut answers| std::mem::take(&mut *answers))
            .unwrap_or_default()
    }

    /// Explicit disconnect: tells the Rendezvous to drop this viewer from
    /// the roster now (best-effort) and stops the worker. Without this the
    /// entry lingers as a ghost and the host keeps minting dead offers.
    pub(crate) fn leave(&self) {
        logger::log("INFO", "stunar leave", "viewer leaving");
        self.shutdown.store(true, Ordering::Release);
        let _ = self.outgoing.send(Outgoing::Close);
        if let Ok(runtime) = current_thread_runtime() {
            if let Ok(client) = http_client() {
                let _ = runtime.block_on(post_member_leave(&client, &self.base, &self.token));
            }
        }
    }
}

impl Drop for StunarViewer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = self.outgoing.send(Outgoing::Close);
    }
}

/// Asks the Rendezvous and waits (up to 65s) for accepted + offer.
/// Returns the viewer_token, the offer, and the open WS for the answer.
pub(crate) fn discover_stunar_room(
    base: &str,
    code: &str,
    password: &str,
    nickname: &str,
) -> Result<(String, PeerSignal, StunarViewer), String> {
    let base = normalize_base(base);
    logger::begin_session("viewer", "stunar");
    logger::log(
        "INFO",
        "stunar ask",
        &format!("base={base} code={code} nickname={nickname}"),
    );
    let incoming_offers = Arc::new(Mutex::new(Vec::new()));
    let answers = Arc::new(Mutex::new(Vec::new()));
    let roster = Arc::new(Mutex::new(HashMap::new()));
    let watch_from = Arc::new(Mutex::new(Vec::new()));
    let unwatch_from = Arc::new(Mutex::new(Vec::new()));
    let master_id = Arc::new(Mutex::new(None));
    let shutdown = Arc::new(AtomicBool::new(false));
    let (outgoing_tx, outgoing_rx) = unbounded_channel();
    let (ready_tx, ready_rx) = sync_channel(1);
    let worker_offers = Arc::clone(&incoming_offers);
    let worker_answers = Arc::clone(&answers);
    let worker_roster = Arc::clone(&roster);
    let worker_watch = Arc::clone(&watch_from);
    let worker_unwatch = Arc::clone(&unwatch_from);
    let worker_master = Arc::clone(&master_id);
    let worker_shutdown = Arc::clone(&shutdown);
    let worker_base = base.clone();
    let worker_code = code.to_owned();
    let worker_password = password.to_owned();
    let worker_nickname = nickname.to_owned();
    let worker = thread::Builder::new()
        .name("godrinking-stunar-viewer".into())
        .spawn(move || {
            let runtime = match current_thread_runtime() {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                    return;
                }
            };
            runtime.block_on(async move {
                match viewer_handshake(
                    &worker_base,
                    &worker_code,
                    &worker_password,
                    &worker_nickname,
                    &worker_roster,
                    &worker_offers,
                    &worker_answers,
                    &worker_watch,
                    &worker_unwatch,
                    &worker_master,
                )
                .await
                {
                    Ok((token, offer, ws, member_id, mode)) => {
                        let hb_token = token.clone();
                        let _ = ready_tx.send(Ok((token, offer, member_id, mode)));
                        viewer_worker(
                            ws,
                            worker_roster,
                            worker_offers,
                            worker_answers,
                            worker_watch,
                            worker_unwatch,
                            worker_master,
                            worker_shutdown,
                            outgoing_rx,
                            worker_base,
                            hb_token,
                        )
                        .await;
                        logger::log("INFO", "stunar ws", "viewer worker stopped");
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                    }
                }
            });
        })
        .map_err(|error| error.to_string())?;
    let (token, offer, member_id, mode) = ready_rx
        .recv_timeout(VIEWER_WAIT_TIMEOUT + HTTP_TIMEOUT + Duration::from_secs(5))
        .map_err(|_| {
            logger::log("ERROR", "stunar ask", "timed out waiting for the viewer handshake");
            "Stunar is unreachable.".to_owned()
        })??;
    return Ok((
        token.clone(),
        offer,
        StunarViewer {
            outgoing: outgoing_tx,
            incoming_offers,
            answers,
            roster,
            watch_from,
            unwatch_from,
            master_id,
            shutdown,
            base: base.clone(),
            token,
            member_id,
            mode,
            _worker: Some(worker),
        },
    ));
}

async fn viewer_handshake(
    base: &str,
    code: &str,
    password: &str,
    nickname: &str,
    roster: &Arc<Mutex<HashMap<String, RosterViewer>>>,
    incoming_offers: &Arc<Mutex<Vec<StunarIncomingOffer>>>,
    answers: &Arc<Mutex<Vec<PeerSignal>>>,
    watch_from: &Arc<Mutex<Vec<String>>>,
    unwatch_from: &Arc<Mutex<Vec<String>>>,
    master_id: &Arc<Mutex<Option<String>>>,
) -> Result<(String, PeerSignal, WsStream, Option<String>, String), String> {
    let client = http_client()?;
    let body = json!({ "code": code, "password": password, "nickname": nickname });
    let response = client
        .post(format!("{base}/v1/viewer/ask"))
        .json(&body)
        .send()
        .await
        .map_err(|error| {
            logger::log(
                "ERROR",
                "stunar ask",
                &format!("network failure: {error} (unreachable)"),
            );
            "Stunar is unreachable.".to_owned()
        })?;
    let status = response.status();
    let json: serde_json::Value = response
        .json()
        .await
        .map_err(|_| "Stunar is unreachable.".to_owned())?;
    if !status.is_success() || json["ok"] != true {
        let raw = json["error"].as_str().unwrap_or("unknown");
        logger::log(
            "ERROR",
            "stunar ask response",
            &format!("status={status} error={raw}"),
        );
        return Err(match raw {
            "full" => "This session is full.".into(),
            "invalid" => "Could not join. (server: invalid — check the password)".into(),
            "busy" => "Could not join. (server: busy — too many attempts, wait a bit)".into(),
            "denied" => "Could not join. (server: denied — wrong code or password)".into(),
            _ => "Could not join.".into(),
        });
    }
    logger::log("INFO", "stunar ask response", "accepted; viewer token issued");
    let token = json["viewer_token"]
        .as_str()
        .ok_or_else(|| "Could not join.".to_owned())?
        .to_owned();
    let member_id = json["member_id"].as_str().map(str::to_owned);
    let mode = json["mode"].as_str().unwrap_or("broadcast").to_owned();
    let ws_url = ws_url(base, "viewer", &token)?;
    let mut ws = connect_ws(&ws_url).await.map_err(|error| {
        logger::log("ERROR", "stunar ws", &format!("connect failed: {error}"));
        "Stunar is unreachable.".to_owned()
    })?;
    logger::log("INFO", "stunar ws", "connected (viewer)");
    let deadline = tokio::time::Instant::now() + VIEWER_WAIT_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            logger::log("WARN", "stunar ws", "timed out waiting for the offer (65s)");
            return Err("The host declined.".into());
        }
        let incoming = tokio::time::timeout(remaining, ws.next())
            .await
            .map_err(|_| {
                logger::log("WARN", "stunar ws", "timed out waiting for the offer (65s)");
                "The host declined.".to_owned()
            })?
            .ok_or_else(|| {
                logger::log("WARN", "stunar ws", "closed while waiting for the offer");
                "Stunar is unreachable.".to_owned()
            })?
            .map_err(|error| {
                logger::log("WARN", "stunar ws", &format!("error: {error}"));
                "Stunar is unreachable.".to_owned()
            })?;
        let Message::Text(text) = incoming else {
            continue;
        };
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let mut inbox = WsInbox::default();
        apply_ws_message(&text, &mut inbox);
        let event = match msg["t"].as_str() {
            Some("accepted") => super::room_mode::HandshakeEvent::Accepted,
            Some("rejected") => super::room_mode::HandshakeEvent::Rejected,
            Some("gone") => super::room_mode::HandshakeEvent::Gone,
            Some("signal")
                if inbox.offers.first().is_some()
                    || msg["payload"].get("type").and_then(|kind| kind.as_str())
                        == Some("offer") =>
            {
                super::room_mode::HandshakeEvent::Offer
            }
            _ => {
                apply_inbox_side_effects(
                    inbox,
                    roster,
                    answers,
                    incoming_offers,
                    watch_from,
                    unwatch_from,
                    master_id,
                );
                continue;
            }
        };
        match super::room_mode::handshake_outcome(&mode, event) {
            super::room_mode::HandshakeOutcome::KeepWaiting => {
                logger::log("INFO", "stunar ws message", "accepted (waiting for offer)");
                apply_inbox_side_effects(
                    inbox,
                    roster,
                    answers,
                    incoming_offers,
                    watch_from,
                    unwatch_from,
                    master_id,
                );
            }
            super::room_mode::HandshakeOutcome::ReadyWithoutOffer => {
                logger::log("INFO", "stunar ws message", "accepted (sala, no offer yet)");
                apply_inbox_side_effects(
                    inbox,
                    roster,
                    answers,
                    incoming_offers,
                    watch_from,
                    unwatch_from,
                    master_id,
                );
                let offer = PeerSignal {
                    kind: PeerSignalKind::Offer,
                    sdp: String::new(),
                    id: None,
                };
                return Ok((token, offer, ws, member_id, mode));
            }
            super::room_mode::HandshakeOutcome::ReadyWithOffer => {
                inbox.offers.clear();
                apply_inbox_side_effects(
                    inbox,
                    roster,
                    answers,
                    incoming_offers,
                    watch_from,
                    unwatch_from,
                    master_id,
                );
                let payload = &msg["payload"];
                if payload.get("type").and_then(|kind| kind.as_str()) == Some("offer") {
                    if let Some(sdp) = payload.get("sdp").and_then(|sdp| sdp.as_str()) {
                        logger::log("INFO", "stunar ws message", "offer received");
                        let from = msg["from"].as_str().map(str::to_owned);
                        let offer = PeerSignal {
                            kind: PeerSignalKind::Offer,
                            sdp: sdp.to_owned(),
                            id: from,
                        };
                        return Ok((token, offer, ws, member_id, mode));
                    }
                }
            }
            super::room_mode::HandshakeOutcome::Rejected => {
                logger::log("WARN", "stunar ws message", "rejected (host declined)");
                return Err("The host declined.".into());
            }
            super::room_mode::HandshakeOutcome::Gone => {
                logger::log("ERROR", "stunar ws message", "gone (room died)");
                return Err("Could not join.".into());
            }
        }
    }
}

async fn viewer_worker(
    mut ws: WsStream,
    roster: Arc<Mutex<HashMap<String, RosterViewer>>>,
    incoming_offers: Arc<Mutex<Vec<StunarIncomingOffer>>>,
    answers: Arc<Mutex<Vec<PeerSignal>>>,
    watch_from: Arc<Mutex<Vec<String>>>,
    unwatch_from: Arc<Mutex<Vec<String>>>,
    master_id: Arc<Mutex<Option<String>>>,
    shutdown: Arc<AtomicBool>,
    mut outgoing: UnboundedReceiver<Outgoing>,
    hb_base: String,
    hb_token: String,
) {
    let hb_shutdown = Arc::clone(&shutdown);
    let heartbeat = tokio::spawn(async move {
        let Ok(client) = http_client() else {
            return;
        };
        let mut ticker = tokio::time::interval(HEARTBEAT_INTERVAL);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            if hb_shutdown.load(Ordering::Acquire) {
                break;
            }
            let _ = post_member_heartbeat(&client, &hb_base, &hb_token).await;
        }
    });
    while !shutdown.load(Ordering::Acquire) {
        tokio::select! {
            incoming = ws.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        let mut inbox = WsInbox::default();
                        apply_ws_message(&text, &mut inbox);
                        let gone = inbox.gone || inbox.kicked;
                        apply_inbox_side_effects(
                            inbox,
                            &roster,
                            &answers,
                            &incoming_offers,
                            &watch_from,
                            &unwatch_from,
                            &master_id,
                        );
                        if gone {
                            break;
                        }
                    }
                    Some(Ok(Message::Ping(_))) => {
                        let _ = ws.send(Message::Pong(Vec::new().into())).await;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        logger::log("WARN", "stunar ws", &format!("viewer inbox error: {error}"));
                        break;
                    }
                    None => {
                        logger::log("WARN", "stunar ws", "viewer inbox closed");
                        break;
                    }
                }
            }
            outgoing = outgoing.recv() => {
                match outgoing {
                    Some(Outgoing::Signal { viewer_id, to, payload }) => {
                        let mut body = json!({ "t": "signal", "payload": payload, "viewer_id": viewer_id });
                        if let Some(to) = to {
                            body["to"] = json!(to);
                        }
                        if ws.send(Message::Text(body.to_string().into())).await.is_err() {
                            logger::log("WARN", "stunar ws", "viewer signal send failed");
                            break;
                        }
                    }
                    Some(Outgoing::Share { start }) => {
                        let text = json!({ "t": if start { "share-start" } else { "share-stop" } }).to_string();
                        if ws.send(Message::Text(text.into())).await.is_err() {
                            logger::log("WARN", "stunar ws", "viewer share announce failed");
                            break;
                        }
                    }
                    Some(Outgoing::Watch { to, start }) => {
                        let text = json!({
                            "t": if start { "watch" } else { "unwatch" },
                            "to": to,
                        })
                        .to_string();
                        if ws.send(Message::Text(text.into())).await.is_err() {
                            logger::log("WARN", "stunar ws", "viewer watch send failed");
                            break;
                        }
                    }
                    Some(Outgoing::Close) | None => {
                        let _ = ws.close(None).await;
                        heartbeat.abort();
                        return;
                    }
                }
            }
        }
    }
    heartbeat.abort();
}

/// Sends the answer signal over the Viewer WS. The inbox stays open.
pub(crate) fn submit_stunar_answer(viewer: &StunarViewer, answer: &PeerSignal) -> Result<(), String> {
    logger::log("INFO", "stunar answer", "sending answer signal");
    let to = answer.id.clone().unwrap_or_default();
    viewer.send_signal(if to.is_empty() { "" } else { &to }, answer)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{apply_ws_message, WsInbox};

    #[test]
    fn room_offer_from_a_member_is_an_incoming_offer_not_an_answer() {
        let mut inbox = WsInbox::default();
        apply_ws_message(
            r#"{"t":"signal","from":"cyd","to":"ada","payload":{"type":"offer","sdp":"v=0"}}"#,
            &mut inbox,
        );
        assert_eq!(inbox.offers.len(), 1);
        assert_eq!(inbox.offers[0].from, "cyd");
        assert!(inbox.answers.is_empty());
    }

    #[test]
    fn broadcast_answer_uses_viewer_id() {
        let mut inbox = WsInbox::default();
        apply_ws_message(
            r#"{"t":"signal","viewer_id":"bob","payload":{"type":"answer","sdp":"v=0"}}"#,
            &mut inbox,
        );
        assert_eq!(inbox.answers.len(), 1);
        assert_eq!(inbox.answers[0].id.as_deref(), Some("bob"));
        assert!(inbox.offers.is_empty());
    }

    #[test]
    fn roster_carries_master_and_share() {
        let mut inbox = WsInbox::default();
        apply_ws_message(
            r#"{"t":"roster","master_id":"ada","mode":"room","entries":[{"id":"ada","nickname":"Ada","state":"sharing","master":true,"share":true},{"id":"bob","nickname":"Bob","state":"accepted","master":false,"share":false}]}"#,
            &mut inbox,
        );
        let roster = inbox.roster.expect("roster");
        assert!(roster["ada"].master && roster["ada"].share);
        assert!(!roster["bob"].master && !roster["bob"].share);
        assert_eq!(inbox.master_id.as_deref(), Some("ada"));
    }

    #[test]
    fn watch_request_carries_the_asker() {
        let mut inbox = WsInbox::default();
        apply_ws_message(r#"{"t":"watch","from":"bob","to":"ada"}"#, &mut inbox);
        assert_eq!(inbox.watch_from, vec!["bob".to_string()]);
        apply_ws_message(r#"{"t":"unwatch","from":"bob","to":"ada"}"#, &mut inbox);
        assert_eq!(inbox.unwatch_from, vec!["bob".to_string()]);
    }
}