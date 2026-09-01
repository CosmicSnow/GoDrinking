//! Stunar client: HTTPS + WebSocket to the Rendezvous (PROTOCOL.md §C).
//!
//! Host side (`StunarHost`): opens a room, heartbeats every 30s, keeps one
//! WS inbox open, and forwards viewer answers into the engine's answer
//! mailbox. Offers are minted by the engine and sent through the WS.
//!
//! Viewer side (`discover_stunar_room` / `submit_stunar_answer`): asks,
//! waits on the WS for accepted + offer (up to 65s), then sends the answer
//! signal. Media never touches the Rendezvous.

use super::peer_transport::{PeerSignal, PeerSignalKind};
use super::types::StunarState;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
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

fn ws_url(base: &str, role: &str, token: &str) -> Result<String, String> {
    let base = base.trim_end_matches('/');
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
        payload: serde_json::Value,
    },
    Close,
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
    outgoing: UnboundedSender<Outgoing>,
    shutdown: Arc<AtomicBool>,
    _worker: Option<JoinHandle<()>>,
}

#[derive(Clone)]
struct RosterViewer {
    nickname: String,
    state: String,
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
    ) -> Result<Self, String> {
        let runtime = current_thread_runtime()?;
        let client = http_client()?;
        let (host_token, code) =
            runtime.block_on(host_open(&client, base, password, nickname, admission))?;
        let state = Arc::new(Mutex::new(StunarState::Calling));
        let roster = Arc::new(Mutex::new(HashMap::new()));
        let answers = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (outgoing_tx, outgoing_rx) = unbounded_channel();
        let worker_base = base.to_owned();
        let worker_token = host_token.clone();
        let worker_state = Arc::clone(&state);
        let worker_roster = Arc::clone(&roster);
        let worker_answers = Arc::clone(&answers);
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
                    .filter(|(_, viewer)| viewer.state == "accepted")
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
        let runtime = current_thread_runtime()?;
        let client = http_client()?;
        runtime.block_on(post_decide(&client, &self.base, &self.host_token, id, "kick"))
    }

    /// Rotates the Password on the Rendezvous. `None` keeps the current
    /// value. The Room code is server-owned and never rotates. Connected
    /// Viewers keep their tokens; the WS is not touched.
    pub(crate) fn rotate(&self, password: Option<&str>) -> Result<(), String> {
        let runtime = current_thread_runtime()?;
        let client = http_client()?;
        runtime.block_on(post_rotate(&client, &self.base, &self.host_token, password))
    }

    /// Sends an offer signal to an accepted Viewer over the WS.
    pub(crate) fn send_signal(&self, viewer_id: &str, signal: &PeerSignal) -> Result<(), String> {
        let payload = json!({ "type": "offer", "sdp": signal.sdp });
        self.outgoing
            .send(Outgoing::Signal {
                viewer_id: viewer_id.to_owned(),
                payload,
            })
            .map_err(|_| "Stunar is unreachable.".to_owned())
    }

    /// Closes the room on the Rendezvous and stops the worker.
    pub(crate) fn close(&self) {
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
) -> Result<(String, String), String> {
    // The server generates the Room code; the Host never sends one.
    let body = json!({ "password": password, "nickname": nickname, "admission": admission });
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
        Ok((host_token, code))
    } else {
        Err("Stunar is unreachable.".into())
    }
}

async fn post_heartbeat(
    client: &reqwest::Client,
    base: &str,
    host_token: &str,
) -> Result<(), String> {
    let response = client
        .post(format!("{base}/v1/host/heartbeat"))
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

async fn post_decide(
    client: &reqwest::Client,
    base: &str,
    host_token: &str,
    viewer_id: &str,
    action: &str,
) -> Result<(), String> {
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
                if let Ok(mut state) = state.lock() {
                    *state = StunarState::Live;
                }
                loop {
                    tokio::select! {
                        incoming = ws.next() => {
                            match incoming {
                                Some(Ok(Message::Text(text))) => {
                                    handle_host_message(&text, &roster, &answers);
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
                                Some(Outgoing::Signal { viewer_id, payload }) => {
                                    let text = json!({
                                        "t": "signal",
                                        "viewer_id": viewer_id,
                                        "payload": payload,
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
            Err(_) => {
                if let Ok(mut state) = state.lock() {
                    *state = StunarState::Unreachable;
                }
            }
        }
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        tokio::time::sleep(WS_RECONNECT_DELAY).await;
    }
    heartbeat.abort();
}

fn handle_host_message(
    text: &str,
    roster: &Arc<Mutex<HashMap<String, RosterViewer>>>,
    answers: &Arc<Mutex<Vec<PeerSignal>>>,
) {
    let Ok(msg) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    let Some(kind) = msg["t"].as_str() else {
        return;
    };
    match kind {
        "pending" => {
            if let (Some(id), Some(nickname)) = (msg["viewer_id"].as_str(), msg["nickname"].as_str())
            {
                if let Ok(mut roster) = roster.lock() {
                    roster.insert(
                        id.to_owned(),
                        RosterViewer {
                            nickname: nickname.to_owned(),
                            state: "pending".into(),
                        },
                    );
                }
            }
        }
        // The Rendezvous roster is authoritative: rebuild the map so stale
        // entries (accepted/rejected/kicked elsewhere) disappear.
        "roster" => {
            let mut next = HashMap::new();
            if let Some(entries) = msg["entries"].as_array() {
                for entry in entries {
                    if let (Some(id), Some(nickname), Some(state)) = (
                        entry["id"].as_str(),
                        entry["nickname"].as_str(),
                        entry["state"].as_str(),
                    ) {
                        next.insert(
                            id.to_owned(),
                            RosterViewer {
                                nickname: nickname.to_owned(),
                                state: state.to_owned(),
                            },
                        );
                    }
                }
            }
            if let Ok(mut roster) = roster.lock() {
                *roster = next;
            }
        }
        "signal" => {
            if let (Some(id), Some(payload)) = (msg["viewer_id"].as_str(), msg["payload"].as_object())
            {
                if payload.get("type").and_then(|kind| kind.as_str()) == Some("answer") {
                    if let Some(sdp) = payload.get("sdp").and_then(|sdp| sdp.as_str()) {
                        let signal = PeerSignal {
                            kind: PeerSignalKind::Answer,
                            sdp: sdp.to_owned(),
                            id: Some(id.to_owned()),
                        };
                        if let Ok(mut answers) = answers.lock() {
                            answers.push(signal);
                        }
                    }
                }
            }
        }
        // "gone": the room died (GC or close elsewhere). The heartbeat keeps
        // failing and the UI shows Relay unreachable; the Host can Stop.
        _ => {}
    }
}

// --- Viewer ----------------------------------------------------------------

/// The Viewer's Stunar WS connection, kept alive between ask and answer.
/// Owns its tokio runtime: the stream's reactor must outlive the struct.
pub(crate) struct StunarViewer {
    ws: WsStream,
    runtime: tokio::runtime::Runtime,
}

/// Asks the Rendezvous and waits (up to 65s) for accepted + offer.
/// Returns the viewer_token, the offer, and the open WS for the answer.
pub(crate) fn discover_stunar_room(
    base: &str,
    code: &str,
    password: &str,
    nickname: &str,
) -> Result<(String, PeerSignal, StunarViewer), String> {
    let runtime = current_thread_runtime()?;
    let result: Result<(String, PeerSignal, WsStream), String> = runtime.block_on(async {
        let client = http_client()?;
        // Stunar rooms always have a Password; the ask always carries it.
        let body = json!({ "code": code, "password": password, "nickname": nickname });
        let response = client
            .post(format!("{base}/v1/viewer/ask"))
            .json(&body)
            .send()
            .await
            .map_err(|_| "Stunar is unreachable.".to_owned())?;
        let status = response.status();
        let json: serde_json::Value = response
            .json()
            .await
            .map_err(|_| "Stunar is unreachable.".to_owned())?;
        if !status.is_success() || json["ok"] != true {
            return Err(match json["error"].as_str() {
                Some("full") => "This session is full.".into(),
                _ => "Could not join.".into(),
            });
        }
        let token = json["viewer_token"]
            .as_str()
            .ok_or_else(|| "Could not join.".to_owned())?
            .to_owned();
        let ws_url = ws_url(base, "viewer", &token)?;
        let mut ws = connect_ws(&ws_url).await.map_err(|_| "Stunar is unreachable.".to_owned())?;
        let deadline = tokio::time::Instant::now() + VIEWER_WAIT_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err("The host declined.".into());
            }
            let incoming = tokio::time::timeout(remaining, ws.next())
                .await
                .map_err(|_| "The host declined.".to_owned())?
                .ok_or_else(|| "Stunar is unreachable.".to_owned())?
                .map_err(|_| "Stunar is unreachable.".to_owned())?;
            let Message::Text(text) = incoming else {
                continue;
            };
            let Ok(msg) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            match msg["t"].as_str() {
                Some("accepted") => continue,
                Some("rejected") => return Err("The host declined.".into()),
                Some("gone") => return Err("Could not join.".into()),
                Some("signal") => {
                    let payload = &msg["payload"];
                    if payload.get("type").and_then(|kind| kind.as_str()) == Some("offer") {
                        if let Some(sdp) = payload.get("sdp").and_then(|sdp| sdp.as_str()) {
                            let offer = PeerSignal {
                                kind: PeerSignalKind::Offer,
                                sdp: sdp.to_owned(),
                                id: None,
                            };
                            return Ok((token, offer, ws));
                        }
                    }
                }
                _ => continue,
            }
        }
    });
    let (token, offer, ws) = result?;
    Ok((token, offer, StunarViewer { ws, runtime }))
}

/// Sends the answer signal over the Viewer WS and closes it.
pub(crate) fn submit_stunar_answer(mut viewer: StunarViewer, answer: &PeerSignal) -> Result<(), String> {
    viewer.runtime.block_on(async {
        let payload = json!({ "type": "answer", "sdp": answer.sdp });
        let text = json!({ "t": "signal", "payload": payload }).to_string();
        viewer
            .ws
            .send(Message::Text(text.into()))
            .await
            .map_err(|_| "Stunar is unreachable.".to_owned())?;
        let _ = viewer.ws.close(None).await;
        Ok(())
    })
}