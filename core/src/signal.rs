//! Signaling client for `server/` (PROTOCOL.md v1): REST join + WebSocket.
//!
//! Transport choices (documented, minimal deps):
//! - Blocking HTTP via `ureq` and a synchronous `tungstenite` socket owned
//!   by ONE thread. No async runtime in the core: the domain owner is
//!   already serialized, and threads + bounded channels fit it exactly.
//! - `ws://` uses `TcpStream::connect_timeout` + handshake, so a dead
//!   server fails fast. `wss://` goes through tungstenite's TLS path
//!   (rustls native roots) for the production rendezvous.
//! - Heartbeat every 10 s (server expects 30 s, expires at 5 min); reader
//!   answers server pings and quits promptly on shutdown.
//!
//! The client NEVER sends media: only intents (heartbeat, share flags,
//! watch, signal envelopes, leave). Passwords and tokens are accepted as
//! arguments but never logged; SDP/candidates only cross as opaque strings
//! inside validated envelopes.

use std::collections::VecDeque;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Limits (mirror the contract)
// ---------------------------------------------------------------------------

/// Max envelope JSON: 64 KiB, like the server.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;
/// Max candidate payload: 8 KiB, like the server.
pub const MAX_CANDIDATE_BYTES: usize = 8 * 1024;
/// Heartbeat cadence (server expects 30 s).
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
/// Reader poll quantum so shutdown stays prompt.
const READ_TIMEOUT: Duration = Duration::from_secs(1);
/// Bounded inbox/outbox depth: backpressure instead of unbounded growth.
const CHANNEL_CAP: usize = 256;

// ---------------------------------------------------------------------------
// Envelopes (exact server shapes)
// ---------------------------------------------------------------------------

/// Signal kind. `ice-complete` has a hyphen on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EnvelopeKind {
    Offer,
    Answer,
    Candidate,
    #[serde(rename = "ice-complete")]
    IceComplete,
}

/// Versioned signal envelope. Unknown keys are rejected on parse, exactly
/// like the server rejects them on route.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    #[serde(rename = "type")]
    pub kind: EnvelopeKind,
    pub session: String,
    pub share: String,
    pub link: String,
    pub attempt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sdp: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnvelopeError {
    Malformed(String),
    Stale { reason: &'static str },
    TooLarge,
    RelayRefused,
}

impl std::fmt::Display for EnvelopeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EnvelopeError::Malformed(detail) => write!(f, "malformed envelope: {detail}"),
            EnvelopeError::Stale { reason } => write!(f, "stale envelope ({reason})"),
            EnvelopeError::TooLarge => write!(f, "envelope exceeds size limits"),
            EnvelopeError::RelayRefused => write!(f, "relay candidates are rejected (no TURN)"),
        }
    }
}

impl std::error::Error for EnvelopeError {}

fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256
}

/// Shape + budget validation. Mirrors the server rules so malformed or
/// over-budget payloads never reach the wire (defense in depth; the server
/// validates too).
pub fn check_envelope(envelope: &Envelope) -> Result<(), EnvelopeError> {
    for id in [
        &envelope.session,
        &envelope.share,
        &envelope.link,
        &envelope.attempt,
    ] {
        if !valid_id(id) {
            return Err(EnvelopeError::Malformed("bad id".into()));
        }
    }
    match envelope.kind {
        EnvelopeKind::Offer | EnvelopeKind::Answer => match &envelope.sdp {
            Some(sdp) if !sdp.is_empty() && sdp.len() <= MAX_MESSAGE_BYTES => Ok(()),
            _ => Err(EnvelopeError::Malformed(
                "offer/answer requires a bounded sdp".into(),
            )),
        },
        EnvelopeKind::Candidate => match &envelope.candidate {
            Some(candidate)
                if !candidate.is_empty() && candidate.len() <= MAX_CANDIDATE_BYTES =>
            {
                if candidate.to_ascii_lowercase().contains("typ relay") {
                    return Err(EnvelopeError::RelayRefused);
                }
                Ok(())
            }
            _ => Err(EnvelopeError::Malformed(
                "candidate requires a bounded payload".into(),
            )),
        },
        EnvelopeKind::IceComplete => Ok(()),
    }
}

/// Parse + validate one envelope value (already size-checked by the caller).
pub fn parse_envelope(raw: &serde_json::Value) -> Result<Envelope, EnvelopeError> {
    let envelope: Envelope =
        serde_json::from_value(raw.clone()).map_err(|e| EnvelopeError::Malformed(e.to_string()))?;
    check_envelope(&envelope)?;
    Ok(envelope)
}

/// Attempt check: true only when all four ids equal the link's current
/// fence. False means stale — discard with no lifecycle effect.
pub fn envelope_is_current(
    envelope: &Envelope,
    session: &str,
    share: &str,
    link: &str,
    attempt: &str,
) -> bool {
    envelope.session == session
        && envelope.share == share
        && envelope.link == link
        && envelope.attempt == attempt
}

// ---------------------------------------------------------------------------
// Roster + inbox types
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberEntry {
    pub id: String,
    pub nickname: String,
    pub master: bool,
    pub share: bool,
}

#[derive(Clone, Debug, Default)]
pub struct RosterState {
    pub entries: Vec<MemberEntry>,
    pub master: Option<String>,
    pub self_id: String,
    pub admitted: bool,
}

#[derive(Clone, Debug)]
pub enum Incoming {
    Admitted { member_id: String },
    Pending,
    Roster,
    Signal {
        from: String,
        to: String,
        payload: Envelope,
    },
    Watch {
        from: String,
    },
    Unwatch {
        from: String,
    },
    Kicked,
    Gone,
}

#[derive(Clone, Debug)]
enum Outgoing {
    AnnounceShare(bool),
    Watch { to: String, start: bool },
    Signal { to: String, payload: Envelope },
    Close,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignalError {
    Http(String),
    Ws(String),
    Denied(String),
    Closed,
}

impl std::fmt::Display for SignalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SignalError::Http(detail) => write!(f, "rendezvous http: {detail}"),
            SignalError::Ws(detail) => write!(f, "rendezvous ws: {detail}"),
            SignalError::Denied(detail) => write!(f, "denied: {detail}"),
            SignalError::Closed => write!(f, "signaling closed"),
        }
    }
}

impl std::error::Error for SignalError {}

// ---------------------------------------------------------------------------
// REST (blocking, bounded)
// ---------------------------------------------------------------------------

fn http_post(base: &str, path: &str, body: serde_json::Value) -> Result<serde_json::Value, SignalError> {
    let agent = ureq::Agent::new_with_defaults();
    let url = format!("{}{}", base.trim_end_matches('/'), path);
    let body_len = body.to_string().len();
    if body_len > MAX_MESSAGE_BYTES {
        return Err(SignalError::Http("request too large".into()));
    }
    let mut response = agent
        .post(&url)
        .send_json(body)
        .map_err(|e| SignalError::Http(e.to_string()))?;
    let status = response.status();
    let json: serde_json::Value = response
        .body_mut()
        .read_json()
        .map_err(|e| SignalError::Http(e.to_string()))?;
    if !(200..300).contains(&status.as_u16()) {
        let reason = json
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("denied");
        return Err(SignalError::Denied(reason.to_owned()));
    }
    if json.get("ok") != Some(&serde_json::Value::Bool(true)) {
        return Err(SignalError::Denied("not ok".into()));
    }
    Ok(json)
}

fn str_field(json: &serde_json::Value, key: &str) -> Result<String, SignalError> {
    json.get(key)
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .ok_or_else(|| SignalError::Http(format!("missing field {key}")))
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Live signaling connection: one WS thread, heartbeat, bounded inbox.
/// Tokens/passwords live here but are never logged.
pub struct SignalClient {
    member_id: String,
    code: String,
    tx: mpsc::SyncSender<Outgoing>,
    inbox: Arc<Mutex<VecDeque<Incoming>>>,
    roster: Arc<Mutex<RosterState>>,
    shutdown: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    base: String,
    token: String,
}

impl SignalClient {
    /// Host path: create the room, then connect.
    pub fn create_room(
        base: &str,
        nickname: &str,
        password: &str,
        admission: bool,
    ) -> Result<Self, SignalError> {
        let json = http_post(
            base,
            "/v1/rooms",
            serde_json::json!({
                "nickname": nickname,
                "password": password,
                "admission": admission,
            }),
        )?;
        let code = str_field(&json, "code")?;
        let member_id = str_field(&json, "memberId")?;
        let token = str_field(&json, "token")?;
        Self::connect(base, code, member_id, token)
    }

    /// Viewer path: join by code, then connect.
    pub fn join_room(
        base: &str,
        code: &str,
        nickname: &str,
        password: &str,
    ) -> Result<Self, SignalError> {
        let json = http_post(
            base,
            &format!("/v1/rooms/{code}/join"),
            serde_json::json!({ "nickname": nickname, "password": password }),
        )?;
        let member_id = str_field(&json, "memberId")?;
        let token = str_field(&json, "token")?;
        Self::connect(base, code.to_owned(), member_id, token)
    }

    fn connect(
        base: &str,
        code: String,
        member_id: String,
        token: String,
    ) -> Result<Self, SignalError> {
        let base = base.trim_end_matches('/').to_owned();
        let ws_url = if let Some(rest) = base.strip_prefix("https://") {
            format!("wss://{rest}/?token={token}")
        } else if let Some(rest) = base.strip_prefix("http://") {
            format!("ws://{rest}/?token={token}")
        } else {
            return Err(SignalError::Ws("bad base url".into()));
        };
        // One socket type on both paths: plain TCP is wrapped so ws and wss
        // share the worker. TCP connects are bounded; the read timeout keeps
        // shutdown prompt on both (TLS included, via the inner socket).
        let socket = if ws_url.starts_with("ws://") {
            let host_port = ws_url
                .strip_prefix("ws://")
                .and_then(|rest| rest.split(['/', '?']).next())
                .ok_or_else(|| SignalError::Ws("bad ws url".into()))?;
            let addr: SocketAddr = host_port
                .to_socket_addrs()
                .map_err(|e| SignalError::Ws(e.to_string()))?
                .next()
                .ok_or_else(|| SignalError::Ws("unresolvable host".into()))?;
            let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(8))
                .map_err(|e| SignalError::Ws(e.to_string()))?;
            stream
                .set_read_timeout(Some(READ_TIMEOUT))
                .map_err(|e| SignalError::Ws(e.to_string()))?;
            let (socket, _) = tungstenite::client::client(
                &ws_url[..],
                tungstenite::stream::MaybeTlsStream::Plain(stream),
            )
            .map_err(|e| SignalError::Ws(e.to_string()))?;
            socket
        } else {
            let (mut socket, _) = tungstenite::connect(&ws_url)
                .map_err(|e| SignalError::Ws(e.to_string()))?;
            match socket.get_mut() {
                tungstenite::stream::MaybeTlsStream::Plain(stream) => stream
                    .set_read_timeout(Some(READ_TIMEOUT))
                    .map_err(|e| SignalError::Ws(e.to_string()))?,
                tungstenite::stream::MaybeTlsStream::Rustls(stream) => stream
                    .sock
                    .set_read_timeout(Some(READ_TIMEOUT))
                    .map_err(|e| SignalError::Ws(e.to_string()))?,
                _ => {}
            }
            socket
        };

        let (tx, rx) = mpsc::sync_channel::<Outgoing>(CHANNEL_CAP);
        let inbox = Arc::new(Mutex::new(VecDeque::new()));
        let roster = Arc::new(Mutex::new(RosterState {
            self_id: member_id.clone(),
            ..Default::default()
        }));
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker = {
            let inbox = Arc::clone(&inbox);
            let roster = Arc::clone(&roster);
            let shutdown = Arc::clone(&shutdown);
            thread::Builder::new()
                .name("golive-signal".into())
                .spawn(move || {
                    socket_loop(socket, rx, inbox, roster, &shutdown);
                })
                .map_err(|e| SignalError::Ws(e.to_string()))?
        };
        Ok(Self {
            member_id,
            code,
            tx,
            inbox,
            roster,
            shutdown,
            worker: Some(worker),
            base,
            token,
        })
    }

    pub fn member_id(&self) -> &str {
        &self.member_id
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    pub fn roster(&self) -> RosterState {
        self.roster.lock().map(|r| r.clone()).unwrap_or_default()
    }

    /// Drains the inbox (non-blocking). The owner/media pump calls this.
    pub fn drain(&self) -> Vec<Incoming> {
        self.inbox
            .lock()
            .map(|mut inbox| inbox.drain(..).collect())
            .unwrap_or_default()
    }

    fn send(&self, msg: Outgoing) -> Result<(), SignalError> {
        self.tx.try_send(msg).map_err(|_| SignalError::Closed)
    }

    pub fn announce_share(&self, live: bool) -> Result<(), SignalError> {
        self.send(Outgoing::AnnounceShare(live))
    }

    pub fn watch(&self, to: &str, start: bool) -> Result<(), SignalError> {
        self.send(Outgoing::Watch {
            to: to.to_owned(),
            start,
        })
    }

    pub fn send_signal(&self, to: &str, payload: &Envelope) -> Result<(), SignalError> {
        check_envelope(payload).map_err(|e| SignalError::Ws(e.to_string()))?;
        self.send(Outgoing::Signal {
            to: to.to_owned(),
            payload: payload.clone(),
        })
    }

    /// Best-effort leave (REST) + socket close. Never fails loudly.
    pub fn leave(&self) {
        let _ = http_post(
            &self.base,
            &format!("/v1/rooms/{}/leave", self.code),
            serde_json::json!({ "token": self.token }),
        );
        let _ = self.send(Outgoing::Close);
    }

    pub fn shutdown(&mut self) {
        self.leave();
        self.shutdown.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for SignalClient {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn push_inbox(inbox: &Arc<Mutex<VecDeque<Incoming>>>, msg: Incoming) {
    if let Ok(mut queue) = inbox.lock() {
        if queue.len() >= CHANNEL_CAP {
            queue.pop_front();
        }
        queue.push_back(msg);
    }
}

fn apply_server_message(
    text: &str,
    inbox: &Arc<Mutex<VecDeque<Incoming>>>,
    roster: &Arc<Mutex<RosterState>>,
) {
    if text.len() > MAX_MESSAGE_BYTES {
        return;
    }
    let Ok(msg) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    let Some(kind) = msg.get("t").and_then(|t| t.as_str()) else {
        return;
    };
    match kind {
        "admitted" => {
            if let Some(id) = msg.get("member_id").and_then(|v| v.as_str()) {
                if let Ok(mut state) = roster.lock() {
                    state.admitted = true;
                }
                push_inbox(inbox, Incoming::Admitted {
                    member_id: id.to_owned(),
                });
            }
        }
        "pending" => {
            push_inbox(inbox, Incoming::Pending);
        }
        "roster" => {
            let entries = msg
                .get("entries")
                .and_then(|v| v.as_array())
                .map(|list| {
                    list.iter()
                        .filter_map(|entry| {
                            Some(MemberEntry {
                                id: entry.get("id")?.as_str()?.to_owned(),
                                nickname: entry
                                    .get("nickname")?
                                    .as_str()
                                    .unwrap_or("")
                                    .to_owned(),
                                master: entry
                                    .get("master")
                                    .and_then(|v| v.as_bool())
                                    .unwrap_or(false),
                                share: entry
                                    .get("share")
                                    .and_then(|v| v.as_bool())
                                    .unwrap_or(false),
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let master = msg
                .get("master_id")
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            if let Ok(mut state) = roster.lock() {
                state.entries = entries;
                state.master = master;
            }
            push_inbox(inbox, Incoming::Roster);
        }
        "signal" => {
            let from = msg.get("from").and_then(|v| v.as_str()).unwrap_or("");
            let to = msg.get("to").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(raw) = msg.get("payload") {
                if let Ok(payload) = parse_envelope(raw) {
                    push_inbox(inbox, Incoming::Signal {
                        from: from.to_owned(),
                        to: to.to_owned(),
                        payload,
                    });
                }
            }
        }
        "watch" => {
            if let Some(from) = msg.get("from").and_then(|v| v.as_str()) {
                push_inbox(inbox, Incoming::Watch {
                    from: from.to_owned(),
                });
            }
        }
        "unwatch" => {
            if let Some(from) = msg.get("from").and_then(|v| v.as_str()) {
                push_inbox(inbox, Incoming::Unwatch {
                    from: from.to_owned(),
                });
            }
        }
        "kicked" => push_inbox(inbox, Incoming::Kicked),
        "gone" => push_inbox(inbox, Incoming::Gone),
        _ => {}
    }
}

fn send_json(
    socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>,
    value: serde_json::Value,
) -> std::io::Result<()> {
    use tungstenite::Message;
    let broken = |e: tungstenite::Error| {
        std::io::Error::new(std::io::ErrorKind::BrokenPipe, e.to_string())
    };
    socket
        .send(Message::Text(value.to_string().into()))
        .map_err(broken)?;
    socket.flush().map_err(broken)
}

fn socket_loop(
    mut socket: tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>,
    rx: mpsc::Receiver<Outgoing>,
    inbox: Arc<Mutex<VecDeque<Incoming>>>,
    roster: Arc<Mutex<RosterState>>,
    shutdown: &AtomicBool,
) {
    use tungstenite::Message;
    let mut last_heartbeat = std::time::Instant::now()
        .checked_sub(HEARTBEAT_INTERVAL)
        .unwrap_or_else(std::time::Instant::now);
    loop {
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        // Outbound (non-blocking drain).
        while let Ok(msg) = rx.try_recv() {
            // No farewell payload: the protocol has no such message (leave
            // already went out over REST). Just close.
            if matches!(msg, Outgoing::Close) {
                let _ = socket.close(None);
                return;
            }
            let value = match msg {
                Outgoing::AnnounceShare(true) => serde_json::json!({ "t": "announce-share" }),
                Outgoing::AnnounceShare(false) => serde_json::json!({ "t": "stop-share" }),
                Outgoing::Watch { to, start } => serde_json::json!({
                    "t": if start { "watch" } else { "unwatch" },
                    "to": to,
                }),
                Outgoing::Signal { to, payload } => {
                    match serde_json::to_value(&payload) {
                        Ok(payload) => serde_json::json!({
                            "t": payload.get("type").cloned().unwrap_or_default(),
                            "to": to,
                            "payload": payload,
                        }),
                        Err(_) => continue,
                    }
                }
                Outgoing::Close => continue,
            };
            if send_json(&mut socket, value).is_err() {
                break;
            }
        }
        if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
            last_heartbeat = std::time::Instant::now();
            let _ = send_json(&mut socket, serde_json::json!({ "t": "heartbeat" }));
        }
        // Inbound (read timeout keeps shutdown prompt).
        match socket.read() {
            Ok(Message::Text(text)) => {
                apply_server_message(&text, &inbox, &roster);
            }
            Ok(Message::Binary(data)) => {
                if let Ok(text) = String::from_utf8(data.into()) {
                    apply_server_message(&text, &inbox, &roster);
                }
            }
            Ok(Message::Ping(data)) => {
                let _ = socket.send(Message::Pong(data));
            }
            Ok(Message::Pong(_)) => {}
            Ok(Message::Close(_)) => break,
            Ok(Message::Frame(_)) => {}
            Err(tungstenite::Error::Io(error))
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(_) => break,
        }
    }
    let _ = socket.close(None);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(kind: EnvelopeKind) -> Envelope {
        Envelope {
            kind,
            session: "sess".into(),
            share: "share".into(),
            link: "link".into(),
            attempt: "att-1".into(),
            sdp: matches!(kind, EnvelopeKind::Offer | EnvelopeKind::Answer).then(|| "v=0".into()),
            candidate: matches!(kind, EnvelopeKind::Candidate)
                .then(|| "candidate:1 1 udp 1 192.0.2.1 9 typ host".into()),
        }
    }

    #[test]
    fn all_four_kinds_validate() {
        for kind in [
            EnvelopeKind::Offer,
            EnvelopeKind::Answer,
            EnvelopeKind::Candidate,
            EnvelopeKind::IceComplete,
        ] {
            check_envelope(&envelope(kind)).expect("valid");
            let line = serde_json::to_string(&envelope(kind)).expect("serialize");
            assert!(line.len() <= MAX_MESSAGE_BYTES);
        }
    }

    #[test]
    fn unknown_kinds_and_extra_keys_rejected() {
        assert!(matches!(
            parse_envelope(&serde_json::json!({"type":"trickle","session":"a","share":"b","link":"c","attempt":"d"})),
            Err(EnvelopeError::Malformed(_))
        ));
        assert!(matches!(
            parse_envelope(&serde_json::json!({"type":"offer","session":"a","share":"b","link":"c","attempt":"d","sdp":"x","extra":1})),
            Err(EnvelopeError::Malformed(_))
        ));
        assert!(matches!(
            parse_envelope(&serde_json::json!({"type":"offer","session":"a","share":"b","link":"c","attempt":"d"})),
            Err(EnvelopeError::Malformed(_))
        ));
    }

    #[test]
    fn candidate_limits_and_relay_refusal() {
        let big = "c".repeat(MAX_CANDIDATE_BYTES + 1);
        assert!(matches!(
            check_envelope(&Envelope {
                candidate: Some(big),
                ..envelope(EnvelopeKind::Candidate)
            }),
            Err(EnvelopeError::Malformed(_))
        ));
        assert!(matches!(
            check_envelope(&Envelope {
                candidate: Some("candidate:9 1 udp 1 203.0.113.7 9 typ relay".into()),
                ..envelope(EnvelopeKind::Candidate)
            }),
            Err(EnvelopeError::RelayRefused)
        ));
        assert!(matches!(
            check_envelope(&Envelope {
                session: String::new(),
                ..envelope(EnvelopeKind::IceComplete)
            }),
            Err(EnvelopeError::Malformed(_))
        ));
    }

    #[test]
    fn attempt_fence_detects_stale() {
        let payload = envelope(EnvelopeKind::Answer);
        assert!(envelope_is_current(&payload, "sess", "share", "link", "att-1"));
        assert!(!envelope_is_current(&payload, "sess", "share", "link", "att-2"));
        assert!(!envelope_is_current(&payload, "sess", "other", "link", "att-1"));
    }

    #[test]
    fn server_messages_route_without_panics() {
        let inbox = Arc::new(Mutex::new(VecDeque::new()));
        let roster = Arc::new(Mutex::new(RosterState::default()));
        // Malformed input never panics, never enqueues.
        for bad in ["", "{", "[1,2]", "{\"t\":42}", &"x".repeat(MAX_MESSAGE_BYTES + 1)] {
            apply_server_message(bad, &inbox, &roster);
        }
        assert!(inbox.lock().unwrap().is_empty());
        apply_server_message(
            r#"{"t":"roster","entries":[{"id":"a","nickname":"Ana","master":true,"share":true}],"master_id":"a"}"#,
            &inbox,
            &roster,
        );
        let state = roster.lock().unwrap().clone();
        assert_eq!(state.master.as_deref(), Some("a"));
        assert!(state.entries[0].share);
    }
}
