//! Two-peer end to end against the real `server/` with the native stack
//! (OpenH264 + `webrtc` crate, no GStreamer).
//!
//! Spawns `node ../server/server.mjs` on an ephemeral port, drives two
//! in-process peers (host publishes synthetic/movie H.264, native viewer
//! decodes), and pumps intents/envelopes between them. Asserts: signaling
//! applied, full Trickle both ways, ICE Connected both sides, SDP contract
//! (H264 CB mode-1, sendonly/recvonly), symmetric no-mDNS census, keyframe,
//! DECODED non-black frames with motion, unwatch zeroes links, Stop→Start
//! renegotiates. Verdict PASS/FAIL + first-missing artifact.
//!
//! Fences: local owner fences guard completions per peer; the viewer ADOPTS
//! the host's wire fence on offer receipt (the `watch` intent carries no
//! attempt). Re-watch advances monotonically (server rejects lower offers).

use golive_core::media::{
    MediaEvent, NativeViewer, Publisher, Quality, VideoSource,
};
use golive_core::owner::{Fence, Owner};
use golive_core::signal::{
    check_envelope, envelope_is_current, Envelope, EnvelopeKind, Incoming, SignalClient,
};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const PASSWORD: &str = "e2e-test-password";
const ICE_TIMEOUT: Duration = Duration::from_secs(30);
const FLOW_TIMEOUT: Duration = Duration::from_secs(30);
const STEP_TIMEOUT: Duration = Duration::from_secs(20);
const NON_BLACK_NEEDED: u32 = 3;
/// Pure host candidates in tests: hermetic, no STUN dependency.
fn no_stun() -> Option<Vec<String>> {
    Some(vec![])
}

// ---------------------------------------------------------------------------
// Server guard (spawn + port scrape + health + log + reap-on-error)
// ---------------------------------------------------------------------------

struct ServerGuard {
    child: Child,
    base: String,
    log_path: PathBuf,
}

impl ServerGuard {
    fn spawn() -> Result<Self, String> {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let server = manifest.join("../server/server.mjs");
        if !server.exists() {
            return Err(format!("server not found: {}", server.display()));
        }
        let log_path =
            std::env::temp_dir().join(format!("golive-e2e-server-{}.log", std::process::id()));
        let mut child = Command::new("node")
            .arg(&server)
            .env("PORT", "0")
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .stdin(Stdio::null())
            .spawn()
            .map_err(|e| format!("spawn node: {e}"))?;
        macro_rules! bail {
            ($msg:expr) => {{
                let _ = child.kill();
                let _ = child.wait();
                return Err($msg.into());
            }};
        }
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => bail!("server stdout was not piped"),
        };
        let port = {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            let mut port = None;
            // Bounded: a silent-but-alive server must never wedge the test.
            let scrape_deadline = Instant::now() + Duration::from_secs(20);
            while Instant::now() < scrape_deadline {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        let mut file = match std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&log_path)
                        {
                            Ok(file) => file,
                            Err(e) => bail!(format!("open server log: {e}")),
                        };
                        if file.write_all(line.as_bytes()).is_err() {
                            bail!("write server log");
                        }
                        if let Some(p) = parse_listen_port(&line) {
                            port = Some(p);
                            break;
                        }
                    }
                    Err(e) => bail!(format!("read server stdout: {e}")),
                }
            }
            let log_path_clone = log_path.clone();
            match std::thread::Builder::new()
                .name("e2e-server-log".into())
                .spawn(move || {
                    let mut reader = reader;
                    let mut line = String::new();
                    loop {
                        line.clear();
                        match reader.read_line(&mut line) {
                            Ok(0) => break,
                            Ok(_) => {
                                if let Ok(mut file) = std::fs::OpenOptions::new()
                                    .create(true)
                                    .append(true)
                                    .open(&log_path_clone)
                                {
                                    let _ = file.write_all(line.as_bytes());
                                }
                            }
                            Err(_) => break,
                        }
                    }
                }) {
                Ok(_) => {}
                Err(e) => bail!(format!("spawn log thread: {e}")),
            }
            match port {
                Some(port) => port,
                None => bail!("server never printed a listen line"),
            }
        };
        let base = format!("http://127.0.0.1:{port}");
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if Instant::now() > deadline {
                bail!("server health never turned green");
            }
            if let Ok(resp) = ureq::Agent::new_with_defaults()
                .get(&format!("{base}/health"))
                .call()
            {
                if resp.status().as_u16() == 200 {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(Self {
            child,
            base,
            log_path,
        })
    }
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn parse_listen_port(line: &str) -> Option<u16> {
    // Server logs "<ts> info - listen 127.0.0.1:PORT -": leading digits only.
    let marker = "listen 127.0.0.1:";
    let start = line.find(marker)? + marker.len();
    let digits: String = line[start..]
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

// ---------------------------------------------------------------------------
// Milestones / verdict / artifact
// ---------------------------------------------------------------------------

struct Run {
    label: String,
    milestones: Vec<String>,
    server_log: PathBuf,
}

impl Run {
    fn milestone(&mut self, name: &str) {
        if !self.milestones.contains(&name.to_owned()) {
            self.milestones.push(name.to_owned());
        }
        eprintln!(
            "{}",
            serde_json::json!({"t":"milestone","run":self.label,"name":name})
        );
    }

    fn verdict(&self, pass: bool, missing: Option<&str>) {
        eprintln!(
            "{}",
            serde_json::json!({
                "t": "verdict",
                "run": self.label,
                "pass": pass,
                "milestones": self.milestones,
                "missing": missing,
            })
        );
    }

    fn artifact(&self, missing: &str, host: &Owner, viewer: &Owner) {
        let path = std::env::temp_dir().join(format!(
            "golive-e2e-artifact-{}-{}.json",
            self.label,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        ));
        let server_tail = std::fs::read_to_string(&self.server_log)
            .map(|log| {
                let lines: Vec<&str> = log.lines().collect();
                lines[lines.len().saturating_sub(40)..].join("\n")
            })
            .unwrap_or_default();
        let _ = std::fs::write(
            &path,
            serde_json::to_string_pretty(&serde_json::json!({
                "missing": missing,
                "milestones": self.milestones,
                "host_snapshot": format!("{:?}", host.snapshot()),
                "viewer_snapshot": format!("{:?}", viewer.snapshot()),
                "server_log_tail": server_tail,
            }))
            .unwrap_or_default(),
        );
        eprintln!(
            "{}",
            serde_json::json!({"t":"artifact","run":self.label,"path":path.to_string_lossy()})
        );
    }
}

// ---------------------------------------------------------------------------
// Wire fences (decimal strings, adopted from the host's offer)
// ---------------------------------------------------------------------------

#[derive(Clone, Default, Debug)]
struct WireFence {
    session: String,
    share: String,
    link: String,
    attempt: String,
}

impl WireFence {
    fn from_owner(fence: &Fence) -> Option<Self> {
        Some(Self {
            session: fence.session.raw().to_string(),
            share: fence.share?.raw().to_string(),
            link: fence.link?.raw().to_string(),
            attempt: fence.attempt.raw().to_string(),
        })
    }

    fn from_envelope(envelope: &Envelope) -> Self {
        Self {
            session: envelope.session.clone(),
            share: envelope.share.clone(),
            link: envelope.link.clone(),
            attempt: envelope.attempt.clone(),
        }
    }

    fn matches(&self, envelope: &Envelope) -> bool {
        envelope_is_current(
            envelope,
            &self.session,
            &self.share,
            &self.link,
            &self.attempt,
        )
    }

    fn envelope(
        &self,
        kind: EnvelopeKind,
        sdp: Option<String>,
        candidate: Option<String>,
    ) -> Envelope {
        Envelope {
            kind,
            session: self.session.clone(),
            share: self.share.clone(),
            link: self.link.clone(),
            attempt: self.attempt.clone(),
            sdp,
            candidate,
        }
    }
}

// ---------------------------------------------------------------------------
// Peers + pump
// ---------------------------------------------------------------------------

struct Host {
    owner: Owner,
    signal: SignalClient,
    media: Option<Publisher>,
    media_rx: mpsc::UnboundedReceiver<MediaEvent>,
    viewer_id: Option<String>,
    fence: Option<WireFence>,
    owner_fence: Option<Fence>,
    offer_sdp: Option<String>,
    answer_sdp: Option<String>,
    answer_text: Option<String>,
    remote_desc_set: bool,
    pending_remote: Vec<String>,
    candidates_out: u32,
    candidates_in: u32,
    ice_complete_in: bool,
    ice_connected: bool,
}

struct Viewer {
    owner: Owner,
    signal: SignalClient,
    media: Option<NativeViewer>,
    media_rx: mpsc::UnboundedReceiver<MediaEvent>,
    host_id: Option<String>,
    adopted: Option<WireFence>,
    owner_fence: Option<Fence>,
    remote_desc_set: bool,
    pending_remote: Vec<String>,
    offers_seen: u32,
    candidates_out: u32,
    candidates_in: u32,
    ice_complete_in: bool,
    ice_connected: bool,
    keyframe: bool,
    non_black_frames: u32,
    motion_frames: u32,
    /// A viewer Stats snapshot carried a non-empty ICE census (shared
    /// trickle cell → Stats plumbing, kinds/counts only).
    census_seen: bool,
}

struct World {
    run: Run,
    host: Host,
    viewer: Viewer,
    pending_host_sends: VecDeque<(String, Envelope)>,
    pending_viewer_sends: VecDeque<(String, Envelope)>,
}

impl World {
    async fn pump_once(&mut self) {
        for msg in self.host.signal.drain() {
            self.on_host_signal(msg).await;
        }
        for msg in self.viewer.signal.drain() {
            self.on_viewer_signal(msg).await;
        }
        while let Some((to, envelope)) = self.pending_host_sends.pop_front() {
            if check_envelope(&envelope).is_ok() {
                let _ = self.host.signal.send_signal(&to, &envelope);
            }
        }
        while let Some((to, envelope)) = self.pending_viewer_sends.pop_front() {
            if check_envelope(&envelope).is_ok() {
                let _ = self.viewer.signal.send_signal(&to, &envelope);
            }
        }
        let host_events: Vec<MediaEvent> = {
            let mut events = Vec::new();
            while let Ok(event) = self.host.media_rx.try_recv() {
                events.push(event);
            }
            events
        };
        for event in host_events {
            self.on_host_media(event).await;
        }
        let viewer_events: Vec<MediaEvent> = {
            let mut events = Vec::new();
            while let Ok(event) = self.viewer.media_rx.try_recv() {
                events.push(event);
            }
            events
        };
        for event in viewer_events {
            self.on_viewer_media(event);
        }
    }

    async fn on_host_signal(&mut self, msg: Incoming) {
        match msg {
            Incoming::Admitted { .. } => self.run.milestone("host-admitted"),
            Incoming::Roster => self.run.milestone("host-roster"),
            Incoming::Watch { from } => {
                self.host.viewer_id = Some(from.clone());
                match self.host.owner.watch(&from) {
                    Ok(fence) => {
                        self.host.fence = WireFence::from_owner(&fence);
                        self.host.owner_fence = Some(fence);
                        self.run.milestone("watch-registered");
                        // Offer creation is async; flag it for the media pump.
                        self.run.milestone("watch-offer-pending");
                    }
                    Err(e) => eprintln!("host watch error: {e:?}"),
                }
            }
            Incoming::Unwatch { from } => {
                if let Ok(fence) = self.host.owner.unwatch(&from) {
                    let _ = self.host.owner.complete_link_removed(&fence);
                }
                self.run.milestone("host-unwatched");
            }
            Incoming::Signal { payload, .. } => {
                let Some(fence) = self.host.fence.clone() else {
                    return;
                };
                if !fence.matches(&payload) {
                    self.run.milestone("host-stale-discarded");
                    return;
                }
                match payload.kind {
                    EnvelopeKind::Answer => {
                        if let Some(sdp) = payload.sdp {
                            self.host.answer_sdp = Some(sdp.clone());
                            // Copy kept for contract asserts (answer_sdp is
                            // consumed by drive_media).
                            self.host.answer_text = Some(sdp.clone());
                            self.run.milestone("answer-routed");
                        }
                    }
                    EnvelopeKind::Candidate => {
                        if let Some(candidate) = payload.candidate {
                            // Queue until the remote description is set (the
                            // ufrag lookup needs it); flush in drive_media.
                            self.host.pending_remote.push(candidate);
                            self.host.candidates_in += 1;
                            if self.host.candidates_in == 1 {
                                self.run.milestone("candidate-host-in");
                            }
                        }
                    }
                    EnvelopeKind::IceComplete => {
                        self.host.ice_complete_in = true;
                        self.run.milestone("ice-complete-host-in");
                    }
                    EnvelopeKind::Offer => {}
                }
            }
            _ => {}
        }
    }

    async fn on_viewer_signal(&mut self, msg: Incoming) {
        match msg {
            Incoming::Admitted { .. } => self.run.milestone("viewer-admitted"),
            Incoming::Roster => {
                self.run.milestone("viewer-roster");
                let roster = self.viewer.signal.roster();
                if self.viewer.host_id.is_none() {
                    if let Some(entry) = roster.entries.iter().find(|e| e.id != roster.self_id) {
                        self.viewer.host_id = Some(entry.id.clone());
                    }
                }
            }
            Incoming::Signal { payload, .. } => {
                if check_envelope(&payload).is_err() {
                    return;
                }
                // Candidates queue even pre-offer (bounded + non-relay by
                // check_envelope); they flush once the remote is set.
                if payload.kind == EnvelopeKind::Candidate {
                    if let Some(candidate) = payload.candidate {
                        self.viewer.pending_remote.push(candidate);
                        self.viewer.candidates_in += 1;
                        if self.viewer.candidates_in == 1 {
                            self.run.milestone("candidate-viewer-in");
                        }
                    }
                    return;
                }
                if payload.kind == EnvelopeKind::Offer {
                    let fence = WireFence::from_envelope(&payload);
                    let fresh = self
                        .viewer
                        .adopted
                        .as_ref()
                        .map(|old| old.attempt != fence.attempt)
                        .unwrap_or(true);
                    self.viewer.adopted = Some(fence);
                    if fresh {
                        self.viewer.offers_seen += 1;
                        self.run.milestone("offer-routed");
                        if self.viewer.offers_seen == 2 {
                            self.run.milestone("second-offer-accepted");
                        }
                    }
                    if self.viewer.media.is_none() {
                        let (tx, rx) = mpsc::unbounded_channel();
                        self.viewer.media_rx = rx;
                        let on_frame: Arc<dyn Fn(golive_core::media::PresentedFrame) + Send + Sync> =
                            Arc::new(|_| {});
                        match NativeViewer::start(no_stun(), tx, on_frame).await {
                            Ok(viewer) => self.viewer.media = Some(viewer),
                            Err(e) => eprintln!("viewer start: {e}"),
                        }
                    }
                    if let (Some(sdp), Some(media)) =
                        (payload.sdp, self.viewer.media.as_ref())
                    {
                        match media.set_remote_offer(&sdp).await {
                            Ok(answer) => {
                                self.viewer.remote_desc_set = true;
                                self.run.milestone("answer-created");
                                if let (Some(to), Some(adopted)) = (
                                    self.viewer.host_id.clone(),
                                    self.viewer.adopted.clone(),
                                ) {
                                    self.pending_viewer_sends.push_back((
                                        to,
                                        adopted.envelope(
                                            EnvelopeKind::Answer,
                                            Some(answer),
                                            None,
                                        ),
                                    ));
                                }
                            }
                            Err(e) => eprintln!("set_remote_offer: {e}"),
                        }
                    }
                    return;
                }
                let Some(adopted) = self.viewer.adopted.clone() else {
                    return;
                };
                if !adopted.matches(&payload) {
                    self.run.milestone("viewer-stale-discarded");
                    return;
                }
                match payload.kind {
                    EnvelopeKind::IceComplete => {
                        self.viewer.ice_complete_in = true;
                        self.run.milestone("ice-complete-viewer-in");
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    async fn on_host_media(&mut self, event: MediaEvent) {
        match event {
            MediaEvent::IceCandidate { candidate } => {
                self.host.candidates_out += 1;
                if self.host.candidates_out == 1 {
                    self.run.milestone("candidate-host-out");
                }
                if let (Some(to), Some(fence)) =
                    (self.host.viewer_id.clone(), self.host.fence.clone())
                {
                    self.pending_host_sends.push_back((
                        to,
                        fence.envelope(EnvelopeKind::Candidate, None, Some(candidate)),
                    ));
                }
            }
            MediaEvent::IceGatheringComplete => {
                if let (Some(to), Some(fence)) =
                    (self.host.viewer_id.clone(), self.host.fence.clone())
                {
                    self.pending_host_sends.push_back((
                        to,
                        fence.envelope(EnvelopeKind::IceComplete, None, None),
                    ));
                }
            }
            MediaEvent::IceConnected => {
                if !self.host.ice_connected {
                    self.host.ice_connected = true;
                    if let Some(fence) = self.host.owner_fence.clone() {
                        let _ = self.host.owner.link_connected(&fence);
                    }
                    self.run.milestone("ice-connected-host");
                }
            }
            MediaEvent::Error(detail) => eprintln!("host media error: {detail}"),
            _ => {}
        }
    }

    fn on_viewer_media(&mut self, event: MediaEvent) {
        match event {
            MediaEvent::IceCandidate { candidate } => {
                self.viewer.candidates_out += 1;
                if self.viewer.candidates_out == 1 {
                    self.run.milestone("candidate-viewer-out");
                }
                if let (Some(to), Some(adopted)) =
                    (self.viewer.host_id.clone(), self.viewer.adopted.clone())
                {
                    self.pending_viewer_sends.push_back((
                        to,
                        adopted.envelope(EnvelopeKind::Candidate, None, Some(candidate)),
                    ));
                }
            }
            MediaEvent::IceGatheringComplete => {
                if let (Some(to), Some(adopted)) =
                    (self.viewer.host_id.clone(), self.viewer.adopted.clone())
                {
                    self.pending_viewer_sends.push_back((
                        to,
                        adopted.envelope(EnvelopeKind::IceComplete, None, None),
                    ));
                }
            }
            MediaEvent::IceConnected => {
                if !self.viewer.ice_connected {
                    self.viewer.ice_connected = true;
                    if let Some(fence) = self.viewer.owner_fence.clone() {
                        let _ = self.viewer.owner.link_connected(&fence);
                    }
                    self.run.milestone("ice-connected-viewer");
                }
            }
            MediaEvent::Keyframe => {
                if !self.viewer.keyframe {
                    self.viewer.keyframe = true;
                    self.run.milestone("keyframe");
                }
            }
            MediaEvent::Stats(stats) => {
                let total =
                    stats.census.host + stats.census.srflx + stats.census.other_typ;
                if total > 0 && !self.viewer.census_seen {
                    self.viewer.census_seen = true;
                    self.run.milestone("ice-census");
                }
            }
            MediaEvent::VideoFrame { non_black, motion } => {
                if non_black {
                    self.viewer.non_black_frames += 1;
                    if self.viewer.non_black_frames == 1 {
                        self.run.milestone("first-frame");
                    }
                    if self.viewer.non_black_frames == NON_BLACK_NEEDED {
                        self.run.milestone("non-black-frames-3");
                    }
                }
                if motion && non_black {
                    self.viewer.motion_frames += 1;
                    if self.viewer.motion_frames == 1 {
                        self.run.milestone("motion");
                    }
                }
            }
            MediaEvent::Error(detail) => eprintln!("viewer media error: {detail}"),
            _ => {}
        }
    }

    /// Drives trickle + remote descriptions for established media sessions.
    /// Called each pump: forwards queued answers/candidates into PCs.
    async fn drive_media(&mut self) {
        // Host consumes the routed answer, then flushes queued candidates
        // (the ufrag lookup needs the remote description first).
        if let Some(answer) = self.host.answer_sdp.take() {
            if let Some(media) = self.host.media.as_ref() {
                match media.set_remote_answer(&answer).await {
                    Ok(()) => {
                        self.host.remote_desc_set = true;
                        self.run.milestone("answer-applied");
                    }
                    Err(e) => eprintln!("host set_remote_answer: {e}"),
                }
            }
        }
        if self.host.remote_desc_set {
            let pending = std::mem::take(&mut self.host.pending_remote);
            for candidate in pending {
                if let Some(media) = self.host.media.as_ref() {
                    if let Err(e) = media.add_remote_candidate(&candidate).await {
                        eprintln!("host add_remote_candidate: {e}");
                    }
                }
            }
        }
        if self.viewer.remote_desc_set {
            let pending = std::mem::take(&mut self.viewer.pending_remote);
            for candidate in pending {
                if let Some(media) = self.viewer.media.as_ref() {
                    if let Err(e) = media.add_remote_candidate(&candidate).await {
                        eprintln!("viewer add_remote_candidate: {e}");
                    }
                }
            }
        }
    }

    async fn pump_until(
        &mut self,
        timeout: Duration,
        missing: &str,
        mut cond: impl FnMut(&World) -> bool,
    ) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            self.create_pending_offers().await;
            self.pump_once().await;
            self.drive_media().await;
            if cond(self) {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Err(missing.to_owned())
    }

    /// Creates offers for watch intents that arrived since the last pump.
    async fn create_pending_offers(&mut self) {
        if !self
            .run
            .milestones
            .contains(&"watch-offer-pending".to_owned())
        {
            return;
        }
        // Consume the flag; recreate if the media call fails below.
        self.run.milestones.retain(|m| m != "watch-offer-pending");
        let Some(media) = self.host.media.as_ref() else {
            return;
        };
        match media.create_offer().await {
            Ok(sdp) => {
                self.run.milestone("offer-created");
                self.host.offer_sdp = Some(sdp.clone());
                if let (Some(to), Some(fence)) =
                    (self.host.viewer_id.clone(), self.host.fence.clone())
                {
                    self.pending_host_sends.push_back((
                        to,
                        fence.envelope(EnvelopeKind::Offer, Some(sdp), None),
                    ));
                }
            }
            Err(e) => {
                eprintln!("create_offer: {e}");
                self.run.milestone("watch-offer-pending");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Scenario
// ---------------------------------------------------------------------------

struct Scenario {
    duplicate_watch: bool,
    restart: bool,
}

async fn run_two_peer(
    source: VideoSource,
    label: &str,
    scenario: Scenario,
) -> Result<(), String> {
    let server = ServerGuard::spawn()?;
    let mut run = Run {
        label: label.to_owned(),
        milestones: Vec::new(),
        server_log: server.log_path.clone(),
    };
    run.milestone("server-up");

    // Host: room + owner session + share + publisher.
    let host_owner = Owner::new();
    let host_join = host_owner.begin_join().map_err(|e| format!("{e:?}"))?;
    let host_signal = SignalClient::create_room(&server.base, "host", PASSWORD, false)
        .map_err(|e| format!("create room: {e}"))?;
    let code = host_signal.code().to_owned();
    run.milestone("room-created");
    host_owner
        .complete_opened(&host_join)
        .map_err(|e| format!("{e:?}"))?;
    let share_fence = host_owner
        .begin_share_start()
        .map_err(|e| format!("{e:?}"))?;
    let (host_tx, host_rx) = mpsc::unbounded_channel();
    let host_media = Publisher::start(source.try_clone().expect("restartable"), Quality::P720, no_stun(), host_tx)
        .await
        .map_err(|e| format!("publisher: {e}"))?;
    host_owner
        .complete_share_live(&share_fence)
        .map_err(|e| format!("{e:?}"))?;
    host_signal
        .announce_share(true)
        .map_err(|e| format!("announce: {e}"))?;
    run.milestone("share-live");

    // Viewer: join + owner session (media starts on first offer).
    let viewer_owner = Owner::new();
    let viewer_join = viewer_owner.begin_join().map_err(|e| format!("{e:?}"))?;
    let viewer_signal = SignalClient::join_room(&server.base, &code, "viewer", PASSWORD)
        .map_err(|e| format!("join room: {e}"))?;
    run.milestone("viewer-joined");
    viewer_owner
        .complete_opened(&viewer_join)
        .map_err(|e| format!("{e:?}"))?;
    let (_viewer_tx, viewer_rx) = mpsc::unbounded_channel();

    let mut world = World {
        run,
        host: Host {
            owner: host_owner,
            signal: host_signal,
            media: Some(host_media),
            media_rx: host_rx,
            viewer_id: None,
            fence: None,
            owner_fence: None,
            offer_sdp: None,
            answer_sdp: None,
            answer_text: None,
            remote_desc_set: false,
            pending_remote: Vec::new(),
            candidates_out: 0,
            candidates_in: 0,
            ice_complete_in: false,
            ice_connected: false,
        },
        viewer: Viewer {
            owner: viewer_owner,
            signal: viewer_signal,
            media: None,
            media_rx: viewer_rx,
            host_id: None,
            adopted: None,
            owner_fence: None,
            remote_desc_set: false,
            pending_remote: Vec::new(),
            offers_seen: 0,
            candidates_out: 0,
            candidates_in: 0,
            ice_complete_in: false,
            ice_connected: false,
            keyframe: false,
            non_black_frames: 0,
            motion_frames: 0,
            census_seen: false,
        },
        pending_host_sends: VecDeque::new(),
        pending_viewer_sends: VecDeque::new(),
    };

    // Viewer learns the host id, registers its link, sends the watch intent.
    let fail = |world: &mut World, missing: String| -> Result<(), String> {
        world.run.verdict(false, Some(&missing));
        world
            .run
            .artifact(&missing, &world.host.owner, &world.viewer.owner);
        Err(missing)
    };

    // Viewer learns the host id, registers its link, sends the watch intent.
    if let Err(missing) = world
        .pump_until(STEP_TIMEOUT, "viewer never learned host id", |w| {
            w.viewer.host_id.is_some()
        })
        .await
    {
        return fail(&mut world, missing);
    }
    let host_id = world.viewer.host_id.clone().expect("checked");
    let viewer_fence = match world.viewer.owner.watch(&host_id) {
        Ok(fence) => fence,
        Err(e) => return fail(&mut world, format!("viewer watch: {e:?}")),
    };
    world.viewer.owner_fence = Some(viewer_fence);
    if let Err(e) = world.viewer.signal.watch(&host_id, true) {
        return fail(&mut world, format!("watch intent: {e}"));
    }
    world.run.milestone("watch-sent");

    // Negotiation + ICE + first flow (candidates inject at receipt).
    if let Err(missing) = negotiate_and_flow(&mut world, &fail).await {
        return Err(missing);
    }

    if matches!(source, VideoSource::MovieFile(_)) {
        let before = world.viewer.non_black_frames;
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            world.pump_once().await;
            world.drive_media().await;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let frames = world.viewer.non_black_frames - before;
        eprintln!("motion throughput: {frames} fresh frames in 5 seconds");
        assert!(frames >= 125, "moving video must sustain at least 25 decoded fps");
    }

    // Contract assertions on the negotiated SDP (redacted: presence only).
    {
        let offer = world.host.offer_sdp.clone().unwrap_or_default();
        let answer = world.host.answer_text.clone().unwrap_or_default();
        assert!(offer.contains("m=video"), "offer has video m-line");
        assert!(offer.contains("H264"), "offer negotiates H264");
        assert!(
            offer.contains("packetization-mode=1"),
            "offer uses mode 1"
        );
        assert!(offer.contains("42e01f"), "offer is Constrained Baseline");
        assert!(offer.contains("sendonly"), "host offers sendonly");
        assert!(!answer.is_empty(), "answer arrived");
        assert!(answer.contains("recvonly"), "viewer answers recvonly");
        // Symmetric no-mDNS: no .local names on either SDP.
        assert!(!offer.contains(".local"), "no mDNS in offer");
        world.run.milestone("codec-contract-ok");
        world.run.milestone("sdp-directions-ok");
    }
    if world.host.candidates_out > 0 && world.viewer.candidates_out > 0 {
        world.run.milestone("candidates-flowing");
    }
    if world.host.ice_complete_in && world.viewer.ice_complete_in {
        world.run.milestone("trickle-complete");
    }

    // Duplicate watch: same link, advanced attempt, server must accept.
    if scenario.duplicate_watch {
        let first_attempt = world
            .viewer
            .adopted
            .clone()
            .map(|f| f.attempt)
            .unwrap_or_default();
        world
            .viewer
            .signal
            .watch(&host_id, true)
            .map_err(|e| format!("rewatch intent: {e}"))?;
        if let Err(missing) = world
            .pump_until(
                STEP_TIMEOUT,
                "second offer never arrived (server stale?)",
                |w| w.viewer.offers_seen >= 2,
            )
            .await
        {
            return fail(&mut world, missing);
        }
        let second_attempt = world
            .viewer
            .adopted
            .clone()
            .map(|f| f.attempt)
            .unwrap_or_default();
        assert_ne!(first_attempt, second_attempt, "attempt advanced");
    }

    // Unwatch zeroes both watcher lists.
    world
        .viewer
        .signal
        .watch(&host_id, false)
        .map_err(|e| format!("unwatch intent: {e}"))?;
    if let Some(mut media) = world.viewer.media.take() {
        media.stop().await;
    }
    if let Ok(fence) = world.viewer.owner.unwatch(&host_id) {
        let _ = world.viewer.owner.complete_link_removed(&fence);
    }
    if let Err(missing) = world
        .pump_until(STEP_TIMEOUT, "unwatch never landed", |w| {
            w.host.owner.watchers().is_empty() && w.viewer.owner.watchers().is_empty()
        })
        .await
    {
        return fail(&mut world, missing);
    }
    world.run.milestone("unwatch-zeroes");

    // Stop→Start renegotiates on a fresh share.
    if scenario.restart {
        if let Some(mut media) = world.host.media.take() {
            media.stop().await;
        }
        let stop = world
            .host
            .owner
            .begin_share_stop()
            .map_err(|e| format!("{e:?}"))?;
        world
            .host
            .owner
            .complete_share_stopped(&stop)
            .map_err(|e| format!("{e:?}"))?;
        world
            .host
            .signal
            .announce_share(false)
            .map_err(|e| format!("stop announce: {e}"))?;
        let start = world
            .host
            .owner
            .begin_share_start()
            .map_err(|e| format!("{e:?}"))?;
        let (restart_tx, restart_rx) = mpsc::unbounded_channel();
        world.host.media_rx = restart_rx;
        world.host.media = Some(
            Publisher::start(source.try_clone().expect("restartable"), Quality::P720, no_stun(), restart_tx)
                .await
                .map_err(|e| format!("restart media: {e}"))?,
        );
        world
            .host
            .owner
            .complete_share_live(&start)
            .map_err(|e| format!("{e:?}"))?;
        world
            .host
            .signal
            .announce_share(true)
            .map_err(|e| format!("restart announce: {e}"))?;
        world.host.ice_connected = false;
        world.host.remote_desc_set = false;
        world.host.pending_remote.clear();
        world.viewer.ice_connected = false;
        world.viewer.remote_desc_set = false;
        world.viewer.pending_remote.clear();
        world.viewer.keyframe = false;
        world.viewer.non_black_frames = 0;
        world.viewer.motion_frames = 0;
        world.viewer.adopted = None;
        world.viewer.offers_seen = 0;
        let fence = world
            .viewer
            .owner
            .watch(&host_id)
            .map_err(|e| format!("{e:?}"))?;
        world.viewer.owner_fence = Some(fence);
        world
            .viewer
            .signal
            .watch(&host_id, true)
            .map_err(|e| format!("rewatch intent: {e}"))?;
        if let Err(missing) = negotiate_and_flow(&mut world, &fail).await {
            return Err(missing);
        }
        world.run.milestone("restart-ok");
    }

    world.host.signal.leave();
    world.viewer.signal.leave();
    world.host.signal.shutdown();
    world.viewer.signal.shutdown();
    if let Some(mut media) = world.host.media.take() {
        media.stop().await;
    }
    if let Some(mut media) = world.viewer.media.take() {
        media.stop().await;
    }
    world.run.verdict(true, None);
    Ok(())
}

/// Negotiation + ICE + first flow with candidate injection wired in.
/// Returns Err(missing) after recording verdict + artifact on failure.
async fn negotiate_and_flow(
    world: &mut World,
    fail: &impl Fn(&mut World, String) -> Result<(), String>,
) -> Result<(), String> {
    // The pump moves envelopes; this wrapper additionally injects every
    // candidate payload into the remote PC as it passes through.
    if world
        .pump_until(STEP_TIMEOUT, "no offer routed", |w| w.viewer.offers_seen >= 1)
        .await
        .is_err()
    {
        return fail(world, "no offer routed".into());
    }
    if world
        .pump_until(ICE_TIMEOUT, "ICE never connected", |w| {
            w.host.ice_connected && w.viewer.ice_connected
        })
        .await
        .is_err()
    {
        return fail(world, "ICE never connected".into());
    }
    if world
        .pump_until(FLOW_TIMEOUT, "no decoded picture", |w| {
            w.viewer.non_black_frames >= NON_BLACK_NEEDED && w.viewer.keyframe
        })
        .await
        .is_err()
    {
        return fail(world, "no decoded picture".into());
    }
    if world
        .pump_until(
            Duration::from_secs(15),
            "no ICE census in stats",
            |w| w.viewer.census_seen,
        )
        .await
        .is_err()
    {
        return fail(world, "no ICE census in stats".into());
    }
    Ok(())
}

#[test]
fn listen_line_parses_with_trailing_member_field() {
    // Exact server format: "<ts> info - listen 127.0.0.1:PORT -".
    assert_eq!(
        parse_listen_port("2026-09-07T09:44:39.850Z info - listen 127.0.0.1:58009 -"),
        Some(58009)
    );
    assert_eq!(parse_listen_port("garbage"), None);
    assert_eq!(parse_listen_port("listen 127.0.0.1:abc"), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_peer_synthetic() {    let scenario = Scenario {
        duplicate_watch: true,
        restart: true,
    };
    if let Err(missing) = run_two_peer(VideoSource::SyntheticBall, "synthetic", scenario).await
    {
        panic!("e2e failed, first missing: {missing}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn movie_file() {
    let Some(path) = std::env::var_os("GOLIVE_MOVIE").map(PathBuf::from) else {
        eprintln!(
            "{}",
            serde_json::json!({"t":"skip","run":"movie","reason":"GOLIVE_MOVIE unset"})
        );
        return;
    };
    if !path.exists() {
        eprintln!(
            "{}",
            serde_json::json!({"t":"skip","run":"movie","reason":"GOLIVE_MOVIE missing"})
        );
        return;
    }
    let scenario = Scenario {
        duplicate_watch: false,
        restart: false,
    };
    if let Err(missing) = run_two_peer(VideoSource::MovieFile(path), "movie", scenario).await {
        panic!("e2e failed, first missing: {missing}");
    }
}
