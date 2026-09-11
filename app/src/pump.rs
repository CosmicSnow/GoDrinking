//! Signal/media orchestration for the shell.
//!
//! The pump drains the core signal inbox on a timer and drives the
//! offer/answer/trickle exchange against per-watcher publishers (host) or
//! the native viewer (watcher). Forward tasks turn core media events into
//! redacted Tauri events. All wire ids stay in memory; emitted payloads
//! carry kinds and counts only — never SDP, candidates, or tokens.
//!
//! MVP scope: one publisher per watcher (each encodes independently).
//! The planned core refactor is fanout-via-shared-track (one encode
//! pipeline, one lightweight PC per watcher).

use super::{AppState, WireIds};
use golive_core::media::{MediaEvent, NativeViewer};
use golive_core::owner::Fence;
use golive_core::signal::{check_envelope, envelope_is_current, Envelope, EnvelopeKind, Incoming};
use std::sync::{Arc, Weak};

use std::time::Duration;
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;

type PublisherOrigin = Weak<tokio::sync::Mutex<super::Publisher>>;

const PUMP_INTERVAL: Duration = Duration::from_millis(50);

/// Which session a forward task serves (for owner fence attribution).
#[derive(Clone, Copy)]
pub enum ForwardTarget {
    Share,
    Watch,
}

/// Starts the signal pump. Cheap when idle (no room = skip).
pub fn spawn_pump(state: Arc<AppState>, app: Option<AppHandle>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(PUMP_INTERVAL);
        loop {
            interval.tick().await;
            let messages = {
                let inner = match state.inner.lock() {
                    Ok(inner) => inner,
                    Err(_) => break,
                };
                match inner.signal.as_ref() {
                    Some(signal) => signal.drain(),
                    None => continue,
                }
            };
            for message in messages {
                handle_signal(&state, &app, message).await;
            }
        }
    })
}

fn origin_is_current(inner: &super::Inner, origin: Option<&PublisherOrigin>) -> bool {
    origin.is_some_and(|origin| inner.publishers.values()
        .any(|session| origin.ptr_eq(&Arc::downgrade(&session.publisher))))
}

/// Forwards one session's media events: owner attribution + redacted emit.
pub fn spawn_forward(
    state: Arc<AppState>,
    app: Option<AppHandle>,
    mut events: mpsc::UnboundedReceiver<MediaEvent>,
    target: ForwardTarget,
    origin: Option<PublisherOrigin>,
    watch_alive: Option<Arc<std::sync::atomic::AtomicBool>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            let _viewer_operation = match target {
                ForwardTarget::Watch => Some(state.operations.lock().await),
                ForwardTarget::Share => None,
            };
            if watch_alive.as_ref().is_some_and(|alive| !alive.load(std::sync::atomic::Ordering::Acquire)) {
                continue;
            }
            // A stopped/replaced publisher may still have queued one-shot events.
            if matches!(target, ForwardTarget::Share) && !state.inner.lock().is_ok_and(|inner| {
                origin_is_current(&inner, origin.as_ref())
            }) {
                continue;
            }
            match event {
                MediaEvent::IceConnected => {
                    apply_ice_connected(&state, target, origin.as_ref());
                    emit(&app, "media-event", &serde_json::json!({"kind": "ice-connected"}));
                }
                MediaEvent::VideoFrame { non_black, motion } => {
                    bump_frame(&state, non_black);
                    emit(
                        &app,
                        "media-event",
                        &serde_json::json!({"kind": "frame", "non_black": non_black, "motion": motion}),
                    );
                }
                MediaEvent::Keyframe => {
                    bump_keyframe(&state);
                    emit(&app, "media-event", &serde_json::json!({"kind": "keyframe"}));
                }
                MediaEvent::Stats(stats) => {
                    merge_stats(&state, stats.frames_decoded, stats.keyframes_decoded, stats.ice_connected);
                    if stats.ice_connected {
                        apply_ice_connected(&state, target, origin.as_ref());
                    }
                    // Generation fence (host side): the encode thread bumps
                    // it on every applied reconfig. First observer wins the
                    // authoritative `quality` event; the snapshot
                    // (`get_media_counters.effective`) reflects it too.
                    if matches!(target, ForwardTarget::Share) && stats.generation > 0 {
                        let bumped = {
                            let mut bumped = None;
                            if let Ok(mut inner) = state.inner.lock() {
                                if !origin_is_current(&inner, origin.as_ref()) {
                                    continue;
                                }
                                let current = inner.share_profile.map(|s| s.generation).unwrap_or(0);
                                if stats.generation > current {
                                    if let Some(share) = inner.share_profile.as_mut() {
                                        share.generation = stats.generation;
                                        bumped = Some(*share);
                                    }
                                }
                            }
                            bumped
                        };
                        if let Some(effective) = bumped {
                            state.session_log(format!(
                                "quality applied generation={} profile={}x{}@{}",
                                effective.generation,
                                effective.profile.w,
                                effective.profile.h,
                                effective.profile.fps,
                            ));
                            emit(
                                &app,
                                "media-event",
                                &serde_json::json!({
                                    "kind": "quality",
                                    "profile": effective.profile,
                                    "generation": effective.generation,
                                }),
                            );
                        }
                    }
                    // Presented is summed live from the native windows.
                    let presented = {
                        state
                            .inner
                            .lock()
                            .map(|inner| {
                                inner.video_windows.values().map(|w| w.presented()).sum::<u64>()
                            })
                            .unwrap_or(0)
                    };
                    // Per-link transmission stats for the watched member
                    // (viewer side only; the host has no windows).
                    let links = match target {
                        ForwardTarget::Watch => {
                            let member = state
                                .inner
                                .lock()
                                .ok()
                                .and_then(|inner| inner.owner.watchers().first().cloned())
                                .unwrap_or_default();
                            if member.is_empty() {
                                Vec::new()
                            } else {
                                state
                                    .link_stats_for(&member)
                                    .map(|link| vec![link])
                                    .unwrap_or_default()
                            }
                        }
                        ForwardTarget::Share => Vec::new(),
                    };
                    // Live encode backend (host only; the viewer has no
                    // encoder). Best-effort read on the event, never polled.
                    // First sighting (and every change) also lands in the
                    // session file for packaged verification.
                    let backend = match target {
                        ForwardTarget::Share => state
                            .inner
                            .lock()
                            .ok()
                            .and_then(|inner| {
                                inner
                                    .publishers
                                    .values()
                                    .filter_map(|session| session.publisher.try_lock().ok())
                                    .filter_map(|publisher| publisher.backend())
                                    .map(|name| name.to_owned())
                                    .next()
                            }),
                        ForwardTarget::Watch => None,
                    };
                    if matches!(target, ForwardTarget::Share) {
                        if let Ok(mut inner) = state.inner.lock() {
                            let seen = backend.clone();
                            if seen.is_some() && seen != inner.last_logged_backend {
                                inner.last_logged_backend = seen.clone();
                                if let Some(name) = seen {
                                    state.session_log(format!("backend {name}"));
                                }
                            }
                            // Redacted ICE census, once per process: kinds
                            // and counts only (never addresses — the struct
                            // cannot even hold them).
                            if !inner.census_logged {
                                let total = stats.census.host
                                    + stats.census.srflx
                                    + stats.census.other_typ;
                                if total > 0 {
                                    inner.census_logged = true;
                                    state.session_log(format!(
                                        "ice census host={} srflx={} other={} udp={} tcp={} ip4={} ip6={}",
                                        stats.census.host,
                                        stats.census.srflx,
                                        stats.census.other_typ,
                                        stats.census.udp,
                                        stats.census.tcp,
                                        stats.census.ip4,
                                        stats.census.ip6,
                                    ));
                                }
                            }
                        }
                    }
                    let backend_note = super::backend_note_for(backend.as_deref());
                    emit(
                        &app,
                        "media-event",
                        &serde_json::json!({
                            "kind": "stats",
                            "frames": stats.frames_decoded,
                            "keyframes": stats.keyframes_decoded,
                            "ice": stats.ice_connected,
                            "host": stats.census.host,
                            "srflx": stats.census.srflx,
                            "presented": presented,
                            "links": links,
                            "backend": backend,
                            "backend_note": backend_note,
                        }),
                    );
                }
                MediaEvent::LocalSdp(_) => {
                    // Offer/answer SDP travels via explicit envelopes.
                }
                MediaEvent::IceCandidate { candidate } => {
                    // Trickle-out per session: host candidates go to the
                    // adopted watcher; viewer candidates go to our watch
                    // target (adopted fence). Dropping either side stalls
                    // ICE in "negotiating" forever.
                    match target {
                        ForwardTarget::Watch => {
                            forward_viewer_candidate(&state, candidate).await;
                        }
                        ForwardTarget::Share => {
                            forward_host_candidate(&state, candidate, origin.as_ref()).await;
                        }
                    }
                }
                MediaEvent::IceGatheringComplete => {
                    match target {
                        ForwardTarget::Watch => {
                            forward_viewer_gathering_complete(&state).await;
                        }
                        ForwardTarget::Share => {
                            forward_host_gathering_complete(&state, origin.as_ref()).await;
                            emit(
                                &app,
                                "media-event",
                                &serde_json::json!({"kind": "gathering-complete"}),
                            );
                        }
                    }
                }
                MediaEvent::Error(_) => {
                    // Detail stays out of the frontend; kind only.
                    emit(&app, "media-event", &serde_json::json!({"kind": "error"}));
                }
            }
        }
    })
}

fn emit(app: &Option<AppHandle>, event: &str, payload: &serde_json::Value) {
    if let Some(app) = app {
        if let Err(e) = app.emit(event, payload) {
            eprintln!("[e2e-probe] emit {event} failed: {e}");
        }
    }
}

/// Marks the live link Connected. Share uses the adopted watcher (never the
/// idle template / HashMap::next — that left ICE "negotiating" forever).
fn ice_fence(inner: &super::Inner, target: ForwardTarget, origin: Option<&PublisherOrigin>) -> Option<Fence> {
    match target {
        ForwardTarget::Watch => inner.viewer_fence,
        ForwardTarget::Share => {
            let watcher = select_host_trickle_target(
                inner.publishers.iter().map(|(k, s)| (k.as_str(), &s.wire, &s.publisher)),
                origin?,
            )?
            .0;
            inner.publishers.get(&watcher).map(|session| session.owner_fence)
        }
    }
}

fn apply_ice_connected(state: &Arc<AppState>, target: ForwardTarget, origin: Option<&PublisherOrigin>) {
    let fence = {
        let inner = match state.inner.lock() {
            Ok(inner) => inner,
            Err(_) => return,
        };
        ice_fence(&inner, target, origin)
    };
    if let Some(fence) = fence {
        if let Ok(inner) = state.inner.lock() {
            let _ = inner.owner.link_connected(&fence);
        }
    }
    bump_connected(state);
    state.session_log("ice connected".to_string());
}

/// Backend-observed counters (polled by `get_media_counters`; kinds only).
fn bump_connected(state: &Arc<AppState>) {
    if let Ok(mut inner) = state.inner.lock() {
        inner.media_counters.connected = true;
    }
}

fn bump_frame(state: &Arc<AppState>, non_black: bool) {
    if !non_black {
        return;
    }
    if let Ok(mut inner) = state.inner.lock() {
        inner.media_counters.frames += 1;
    }
}

fn bump_keyframe(state: &Arc<AppState>) {
    if let Ok(mut inner) = state.inner.lock() {
        inner.media_counters.keyframes += 1;
        inner.media_counters.keyframes_seen = true;
    }
}

fn merge_stats(state: &Arc<AppState>, frames: u64, keyframes: u64, ice: bool) {
    if let Ok(mut inner) = state.inner.lock() {
        let counters = &mut inner.media_counters;
        counters.frames = counters.frames.max(frames);
        counters.keyframes = counters.keyframes.max(keyframes);
        counters.connected = counters.connected || ice;
    }
}

/// Viewer trickle-out: adopted fence + our watch target as `to`.
async fn forward_viewer_candidate(state: &Arc<AppState>, candidate: String) {
    let (to, ids) = {
        let inner = match state.inner.lock() {
            Ok(inner) => inner,
            Err(_) => return,
        };
        let Some(ids) = inner.adopted.clone() else {
            return;
        };
        let to = inner.owner.watchers().first().cloned().unwrap_or_default();
        (to, ids)
    };
    if to.is_empty() {
        return;
    }
    let envelope = envelope(&ids, EnvelopeKind::Candidate, None, Some(candidate));
    if check_envelope(&envelope).is_ok() {
        let inner = match state.inner.lock() {
            Ok(inner) => inner,
            Err(_) => return,
        };
        if let Some(signal) = inner.signal.as_ref() {
            let _ = signal.send_signal(&to, &envelope);
        }
    }
}

async fn forward_viewer_gathering_complete(state: &Arc<AppState>) {
    let (to, ids) = {
        let inner = match state.inner.lock() {
            Ok(inner) => inner,
            Err(_) => return,
        };
        let Some(ids) = inner.adopted.clone() else {
            return;
        };
        let to = inner.owner.watchers().first().cloned().unwrap_or_default();
        (to, ids)
    };
    if to.is_empty() {
        return;
    }
    let envelope = envelope(&ids, EnvelopeKind::IceComplete, None, None);
    if check_envelope(&envelope).is_ok() {
        let inner = match state.inner.lock() {
            Ok(inner) => inner,
            Err(_) => return,
        };
        if let Some(signal) = inner.signal.as_ref() {
            let _ = signal.send_signal(&to, &envelope);
        }
    }
}

/// Address only the publisher that owns this callback; adoption may move its
/// map key from the idle template to a watcher without changing its identity.
fn select_host_trickle_target<'a, T: 'a>(
    entries: impl Iterator<Item = (&'a str, &'a WireIds, &'a Arc<T>)>,
    origin: &Weak<T>,
) -> Option<(String, WireIds)> {
    entries
        .filter(|(key, ids, publisher)| !key.is_empty() && !ids.session.is_empty()
            && origin.ptr_eq(&Arc::downgrade(publisher)))
        .map(|(key, ids, _)| (key.to_owned(), ids.clone()))
        .next()
}

/// Host trickle-out: adopted per-watcher link's wire ids + watcher as `to`.
/// Mirrors `forward_viewer_candidate` (same envelope shape/validation; the
/// viewer already understands Candidate + ice-complete per PROTOCOL.md).
async fn forward_host_candidate(state: &Arc<AppState>, candidate: String, origin: Option<&PublisherOrigin>) {
    let (to, ids) = {
        let inner = match state.inner.lock() {
            Ok(inner) => inner,
            Err(_) => return,
        };
        match select_host_trickle_target(
            inner.publishers.iter().map(|(k, s)| (k.as_str(), &s.wire, &s.publisher)),
            match origin { Some(origin) => origin, None => return },
        ) {
            Some(target) => target,
            None => return,
        }
    };
    let envelope = envelope(&ids, EnvelopeKind::Candidate, None, Some(candidate));
    if check_envelope(&envelope).is_ok() {
        let inner = match state.inner.lock() {
            Ok(inner) => inner,
            Err(_) => return,
        };
        if let Some(signal) = inner.signal.as_ref() {
            let _ = signal.send_signal(&to, &envelope);
        }
    }
}

/// Host gathering-complete: same `ice-complete` envelope the viewer path
/// already emits (the viewer validates the fence and needs no PC action).
async fn forward_host_gathering_complete(state: &Arc<AppState>, origin: Option<&PublisherOrigin>) {
    let (to, ids) = {
        let inner = match state.inner.lock() {
            Ok(inner) => inner,
            Err(_) => return,
        };
        match select_host_trickle_target(
            inner.publishers.iter().map(|(k, s)| (k.as_str(), &s.wire, &s.publisher)),
            match origin { Some(origin) => origin, None => return },
        ) {
            Some(target) => target,
            None => return,
        }
    };
    let envelope = envelope(&ids, EnvelopeKind::IceComplete, None, None);
    if check_envelope(&envelope).is_ok() {
        let inner = match state.inner.lock() {
            Ok(inner) => inner,
            Err(_) => return,
        };
        if let Some(signal) = inner.signal.as_ref() {
            let _ = signal.send_signal(&to, &envelope);
        }
    }
}

fn wire_ids(fence: &Fence) -> Option<WireIds> {
    Some(WireIds {
        session: fence.session.raw().to_string(),
        share: fence.share?.raw().to_string(),
        link: fence.link?.raw().to_string(),
        attempt: fence.attempt.raw().to_string(),
    })
}

fn envelope(ids: &WireIds, kind: EnvelopeKind, sdp: Option<String>, candidate: Option<String>) -> Envelope {
    Envelope {
        kind,
        session: ids.session.clone(),
        share: ids.share.clone(),
        link: ids.link.clone(),
        attempt: ids.attempt.clone(),
        sdp,
        candidate,
    }
}

fn current(ids: &WireIds, payload: &Envelope) -> bool {
    envelope_is_current(payload, &ids.session, &ids.share, &ids.link, &ids.attempt)
}

async fn handle_signal(state: &Arc<AppState>, app: &Option<AppHandle>, message: Incoming) {
    match message {
        Incoming::Admitted { .. } => {
            emit(app, "signal-event", &serde_json::json!({"kind": "admitted"}));
        }
        Incoming::Pending => {
            emit(app, "signal-event", &serde_json::json!({"kind": "pending"}));
        }
        Incoming::Roster => {
            let roster = {
                let inner = match state.inner.lock() {
                    Ok(inner) => inner,
                    Err(_) => return,
                };
                match inner.signal.as_ref() {
                    Some(signal) => signal.roster(),
                    None => return,
                }
            };
            emit(
                app,
                "signal-event",
                &serde_json::json!({
                    "kind": "roster",
                    "entries": roster.entries.iter().map(|e| serde_json::json!({
                        "id": e.id, "nickname": e.nickname,
                        "master": e.master, "share": e.share,
                    })).collect::<Vec<_>>(),
                    "master": roster.master,
                }),
            );
        }
        Incoming::Watch { from } => {
            emit(app, "signal-event", &serde_json::json!({"kind": "watch", "from": from}));
            on_watch(state, app, &from).await;
        }
        Incoming::Unwatch { from } => {
            emit(app, "signal-event", &serde_json::json!({"kind": "unwatch", "from": from}));
            on_unwatch(state, &from).await;
        }
        Incoming::Signal { from, payload, .. } => {
            if check_envelope(&payload).is_err() {
                return;
            }
            emit(
                app,
                "signal-event",
                &serde_json::json!({"kind": "signal", "from": from, "type": format!("{:?}", payload.kind)}),
            );
            on_envelope(state, app, &from, &payload).await;
        }
        Incoming::Kicked => {
            emit(app, "signal-event", &serde_json::json!({"kind": "kicked"}));
        }
        Incoming::Gone => {
            emit(app, "signal-event", &serde_json::json!({"kind": "gone"}));
        }
    }
}

/// Host path: register the watcher, ensure its publisher, offer.
async fn on_watch(state: &Arc<AppState>, app: &Option<AppHandle>, watcher: &str) {
    let _operation = state.operations.lock().await;
    let fence = {
        let inner = match state.inner.lock() {
            Ok(inner) => inner,
            Err(_) => return,
        };
        match inner.owner.watch(watcher) {
            Ok(fence) => fence,
            Err(_) => return,
        }
    };
    let Some(ids) = wire_ids(&fence) else { return };
    // Ensure a publisher for this watcher (MVP: one encode each).
    let has_publisher = {
        let inner = match state.inner.lock() {
            Ok(inner) => inner,
            Err(_) => return,
        };
        inner.publishers.contains_key(watcher)
    };
    if !has_publisher {
        // Single-shot template: the first watch adopts it. Later watches
        // (re-watch after unwatch, a second concurrent peer) build a FRESH
        // session from the stored share source — the template is gone and
        // must never be re-fabricated from thin air (a synthetic fallback
        // here would silently share the wrong source).
        let adopted_template = {
            let mut inner = match state.inner.lock() {
                Ok(inner) => inner,
                Err(_) => return,
            };
            if let Some(mut template) = inner.publishers.remove("") {
                template.owner_fence = fence;
                template.wire = ids.clone();
                inner.publishers.insert(watcher.to_owned(), template);
                true
            } else {
                false
            }
        };
        if !adopted_template && !state
            .adopt_fresh_session(app.clone(), watcher, fence, ids.clone())
            .await
        {
            // Share not live (or the rebuild failed): refuse quietly, as
            // before — no offer, no fence advance.
            return;
        }
    } else {
        let mut inner = match state.inner.lock() {
            Ok(inner) => inner,
            Err(_) => return,
        };
        if let Some(session) = inner.publishers.get_mut(watcher) {
            session.owner_fence = fence;
            session.wire = ids.clone();
            session.remote_ready = false;
            session.pending_remote.clear();
        }
    }
    // Offer from this watcher's publisher (guard scope ends first).
    let publisher = {
        let inner = match state.inner.lock() {
            Ok(inner) => inner,
            Err(_) => return,
        };
        let Some(session) = inner.publishers.get(watcher) else {
            return;
        };
        Arc::clone(&session.publisher)
    };
    let sdp = {
        match publisher.lock().await.create_offer().await {
            Ok(sdp) => sdp,
            Err(_) => return,
        }
    };
    let envelope = envelope(&ids, EnvelopeKind::Offer, Some(sdp), None);
    if check_envelope(&envelope).is_ok() {
        let inner = match state.inner.lock() {
            Ok(inner) => inner,
            Err(_) => return,
        };
        if let Some(signal) = inner.signal.as_ref() {
            let _ = signal.send_signal(watcher, &envelope);
        }
    }
}

/// Host path: drop the watcher's publisher and link.
async fn on_unwatch(state: &Arc<AppState>, watcher: &str) {
    let _operation = state.operations.lock().await;
    let session = {
        let mut inner = match state.inner.lock() {
            Ok(inner) => inner,
            Err(_) => return,
        };
        inner.publishers.remove(watcher)
    };
    if let Some(mut session) = session {
        session.publisher.lock().await.stop().await;
        if let Some(bridge) = session.bridge.as_mut() {
            bridge.stop();
        }
    }
    if let Ok(inner) = state.inner.lock() {
        if let Ok(fence) = inner.owner.unwatch(watcher) {
            let _ = inner.owner.complete_link_removed(&fence);
        }
    }
}

/// Present routing for one decoded frame: reuse the live window while its
/// spawn-contracted dims still fit the frame. `None` (no window yet) and
/// dim changes take the spawn path; identical dims reuse the feed, so
/// bitrate/fps-only applies never disturb presentation.
fn window_fits(contracted: Option<(u32, u32)>, w: u32, h: u32) -> bool {
    matches!(contracted, Some((cw, ch)) if cw == w && ch == h)
}

/// Respawn log line: per-link window sequence + dims direction only —
/// never member ids, names, titles, pixels, or tokens. Consecutive lines
/// with consecutive seqs and mirrored directions mean one flapping link;
/// non-consecutive seqs mean interleaved links spawned in between.
fn respawn_line(seq: u64, old: (u32, u32), new: (u32, u32)) -> String {
    format!("present window respawn #{seq} {}x{} -> {}x{}", old.0, old.1, new.0, new.1)
}

/// Pushes one decoded frame to a watched member's present window. When the
/// frame dims no longer fit the live window's spawn contract (the encoder
/// rebuilt at new dims on quality-apply, which the helper rejects with
/// "frame size != contracted" and then exits), the dead window is closed
/// and a fresh one spawns through the same first-frame path — automatic
/// recovery with no user action and no re-signaling. Dims only in
/// diagnostics; never pixels, titles, or tokens.
fn push_present_frame(
    state: &Arc<AppState>,
    watcher: &str,
    title: &str,
    presented: &Arc<std::sync::atomic::AtomicU64>,
    frame: golive_core::media::PresentedFrame,
    alive: &std::sync::atomic::AtomicBool,
) {
    if !alive.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    // Event-driven decode observation (feeds per-link stats; no polling
    // anywhere on this path).
    state.note_link_frame(watcher, title, frame.w as u32, frame.h as u32, alive);
    let (fw, fh) = (frame.w as u32, frame.h as u32);
    let mut dead: Option<crate::video::VideoWindow> = None;
    let mut push: Option<crate::video::FramePush> = None;
    // Sequence reserved for the replacement window (also consumed by the
    // first-frame spawn, which logs nothing). 0 means "reusing".
    let mut spawn_seq: u64 = 0;
    {
        let mut inner = match state.inner.lock() {
            Ok(inner) => inner,
            Err(_) => return,
        };
        if !alive.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        let contracted = inner.video_windows.get(watcher).map(|w| w.resolution());
        if window_fits(contracted, fw, fh) {
            push = inner.video_feeds.get(watcher).cloned();
        }
        if push.is_none() {
            // First frame, or the contract no longer fits: drop the stale
            // entries here; the dead window stops outside the lock below
            // (never join a feeder thread while holding state).
            dead = inner.video_windows.remove(watcher);
            inner.video_feeds.remove(watcher);
            inner.video_seq.remove(watcher);
            spawn_seq = inner.next_video_seq;
            inner.next_video_seq += 1;
        }
    }
    if let Some(mut dead) = dead {
        let (dw, dh) = dead.resolution();
        dead.stop();
        state.session_log(respawn_line(spawn_seq, (dw, dh), (fw, fh)));
    }
    let push = match push {
        Some(push) => push,
        None => {
            let (window, push) = crate::video::VideoWindow::spawn(
                title.to_owned(),
                frame.w,
                frame.h,
                Arc::clone(presented),
            );
            if let Ok(mut inner) = state.inner.lock() {
                if !alive.load(std::sync::atomic::Ordering::Acquire) {
                    return;
                }
                inner.video_seq.insert(watcher.to_owned(), spawn_seq);
                inner.video_windows.insert(watcher.to_owned(), window);
                inner.video_feeds.insert(watcher.to_owned(), push.clone());
            }
            push
        }
    };
    push.push(frame);
}

/// Route one validated envelope: host link or viewer adoption.
async fn on_envelope(
    state: &Arc<AppState>,
    app: &Option<AppHandle>,
    from: &str,
    payload: &Envelope,
) {
    let _operation = state.operations.lock().await;
    // Host link?
    let is_host_link = {
        let inner = match state.inner.lock() {
            Ok(inner) => inner,
            Err(_) => return,
        };
        inner.publishers.contains_key(from)
    };
    if is_host_link {
        on_host_envelope(state, from, payload).await;
    } else {
        on_viewer_envelope(state, app, payload).await;
    }
}

async fn on_host_envelope(state: &Arc<AppState>, watcher: &str, payload: &Envelope) {
    let ready = {
        let inner = match state.inner.lock() {
            Ok(inner) => inner,
            Err(_) => return,
        };
        let Some(session) = inner.publishers.get(watcher) else {
            return;
        };
        if !current(&session.wire, payload) {
            return;
        }
        session.remote_ready
    };
    match payload.kind {
        EnvelopeKind::Answer => {
            if let Some(sdp) = payload.sdp.clone() {
                let (publisher, ok) = {
                    let inner = match state.inner.lock() {
                        Ok(inner) => inner,
                        Err(_) => return,
                    };
                    match inner.publishers.get(watcher) {
                        Some(session) => (Arc::clone(&session.publisher), true),
                        None => return,
                    }
                };
                let _ = ok;
                if publisher.lock().await.set_remote_answer(&sdp).await.is_ok() {
                    // Narrow scope: take the queue, drop the guard, then
                    // await. std guards never cross an await (also keeps the
                    // pump future Send).
                    let pending = {
                        let mut inner = match state.inner.lock() {
                            Ok(inner) => inner,
                            Err(_) => return,
                        };
                        match inner.publishers.get_mut(watcher) {
                            Some(session) => {
                                session.remote_ready = true;
                                std::mem::take(&mut session.pending_remote)
                            }
                            None => return,
                        }
                    };
                    for candidate in pending {
                        let _ = publisher
                            .lock()
                            .await
                            .add_remote_candidate(&candidate)
                            .await;
                    }
                }
            }
        }
        EnvelopeKind::Candidate => {
            if let Some(candidate) = payload.candidate.clone() {
                if ready {
                    // Clone out, drop the guard, then await.
                    let publisher = {
                        let inner = match state.inner.lock() {
                            Ok(inner) => inner,
                            Err(_) => return,
                        };
                        match inner.publishers.get(watcher) {
                            Some(session) => Arc::clone(&session.publisher),
                            None => return,
                        }
                    };
                    let _ = publisher.lock().await.add_remote_candidate(&candidate).await;
                } else {
                    // Queue until the answer lands (ufrag needs it).
                    if let Ok(mut inner) = state.inner.lock() {
                        if let Some(session) = inner.publishers.get_mut(watcher) {
                            session.pending_remote.push(candidate);
                        }
                    }
                }
            }
        }
        EnvelopeKind::IceComplete => {}
        EnvelopeKind::Offer => {}
    }
}

/// Resolves a watched member id to a window title via the roster.
/// Falls back to a short id prefix (member ids are not secrets).
fn watcher_nickname(state: &Arc<AppState>, watcher: &str) -> String {
    let nickname = {
        let inner = match state.inner.lock() {
            Ok(inner) => inner,
            Err(_) => return watcher.to_owned(),
        };
        inner
            .signal
            .as_ref()
            .map(|signal| signal.roster())
            .and_then(|roster| {
                roster
                    .entries
                    .iter()
                    .find(|entry| entry.id == watcher)
                    .map(|entry| entry.nickname.clone())
            })
            .unwrap_or_default()
    };
    if nickname.trim().is_empty() {
        format!("watch {}", &watcher[..watcher.len().min(8)])
    } else {
        nickname
    }
}

async fn on_viewer_envelope(
    state: &Arc<AppState>,
    app: &Option<AppHandle>,
    payload: &Envelope,
) {
    if payload.kind == EnvelopeKind::Candidate {
        // Pre-offer (no adopted fence yet): queue for the offer flush.
        // Post-offer: fence-check, then apply when the remote is ready
        // (mirror of the host path in `on_host_envelope`) — otherwise
        // queue. Without the ready branch, host candidates trickled after
        // the answer would sit queued forever and ICE would never close.
        // (Size + non-relay already rejected by `check_envelope` upstream.)
        if let Some(candidate) = payload.candidate.clone() {
            let (adopted, ready, viewer) = {
                let inner = match state.inner.lock() {
                    Ok(inner) => inner,
                    Err(_) => return,
                };
                (
                    inner.adopted.clone(),
                    inner.viewer_remote_ready,
                    inner.viewer.clone(),
                )
            };
            match adopted {
                None => {
                    if let Ok(mut inner) = state.inner.lock() {
                        inner.viewer_pending_remote.push(candidate);
                    }
                }
                Some(ids) => {
                    if !current(&ids, payload) {
                        return;
                    }
                    match (ready, viewer) {
                        (true, Some(viewer)) => {
                            let _ = viewer.lock().await.add_remote_candidate(&candidate).await;
                        }
                        _ => {
                            if let Ok(mut inner) = state.inner.lock() {
                                inner.viewer_pending_remote.push(candidate);
                            }
                        }
                    }
                }
            }
        }
        return;
    }
    // Non-offer envelopes need an adopted fence to validate against.
    // (The first offer establishes it, so offers skip this gate.)
    if payload.kind != EnvelopeKind::Offer {
        let adopted = {
            let inner = match state.inner.lock() {
                Ok(inner) => inner,
                Err(_) => return,
            };
            inner.adopted.clone()
        };
        let Some(ids) = adopted else { return };
        if !current(&ids, payload) {
            return;
        }
        // The host's ice-complete needs no action (our gathering state is
        // reported by our own forward task).
        return;
    }
    // Offer: adopt, ensure viewer, answer.
    let ids = WireIds {
        session: payload.session.clone(),
        share: payload.share.clone(),
        link: payload.link.clone(),
        attempt: payload.attempt.clone(),
    };
    {
        let mut inner = match state.inner.lock() {
            Ok(inner) => inner,
            Err(_) => return,
        };
        inner.adopted = Some(ids.clone());
    }
    let has_viewer = {
        let inner = match state.inner.lock() {
            Ok(inner) => inner,
            Err(_) => return,
        };
        inner.viewer.is_some()
    };
    // Viewer needs the host id to answer: our watch target (single viewer
    // link in MVP). Resolve first: the present path below is keyed by it.
    let to = {
        let inner = match state.inner.lock() {
            Ok(inner) => inner,
            Err(_) => return,
        };
        inner.owner.watchers().first().cloned().unwrap_or_default()
    };
    if to.is_empty() {
        return;
    }
    if !has_viewer {
        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        // Present path: latest-only feed per watched member. The native
        // window opens on the FIRST presented frame (never an idle black
        // window); title carries the publisher nickname.
        let watcher = to.clone();
        let title = watcher_nickname(state, &watcher);
            let on_frame = {
                let state = Arc::clone(state);
                let alive = Arc::clone(&alive);
                let presented = Arc::new(std::sync::atomic::AtomicU64::new(0));
                Arc::new(move |frame: golive_core::media::PresentedFrame| {
                    push_present_frame(&state, &watcher, &title, &presented, frame, &alive);
            })
        };
        let playback = crate::audio::ViewerPlayback::start();
        let on_audio = playback
            .as_ref()
            .map(|_| crate::audio::playback_callback());
        let viewer = match NativeViewer::start_with_audio(None, event_tx, on_frame, on_audio).await {
            Ok(viewer) => Arc::new(tokio::sync::Mutex::new(viewer)),
            Err(_) => return,
        };
        {
            let mut inner = match state.inner.lock() {
                Ok(inner) => inner,
                Err(_) => return,
            };
            inner.viewer_playback = None;
            inner.viewer = Some(Arc::clone(&viewer));
            inner.viewer_alive = Some(Arc::clone(&alive));
            inner.viewer_playback = playback;
            inner.tasks.push(spawn_forward(
                Arc::clone(state),
                app.clone(),
                event_rx,
                ForwardTarget::Watch,
                None,
                Some(alive),
            ));
        }
    }
    let viewer = {
        let inner = match state.inner.lock() {
            Ok(inner) => inner,
            Err(_) => return,
        };
        let Some(viewer) = inner.viewer.clone() else {
            return;
        };
        viewer
    };
    let Some(sdp) = payload.sdp.clone() else {
        return;
    };
    // Bound statement (not match scrutinee): temporaries drop here, before
    // the arms below reuse `viewer`.
    let answer = viewer.lock().await.set_remote_offer(&sdp).await;
    match answer {
        Ok(answer) => {
            let pending = {
                let mut inner = match state.inner.lock() {
                    Ok(inner) => inner,
                    Err(_) => return,
                };
                inner.viewer_remote_ready = true;
                std::mem::take(&mut inner.viewer_pending_remote)
            };
            for candidate in pending {
                let _ = viewer.lock().await.add_remote_candidate(&candidate).await;
            }
            let envelope = envelope(&ids, EnvelopeKind::Answer, Some(answer), None);
            if check_envelope(&envelope).is_ok() {
                let inner = match state.inner.lock() {
                    Ok(inner) => inner,
                    Err(_) => return,
                };
                if let Some(signal) = inner.signal.as_ref() {
                    let _ = signal.send_signal(&to, &envelope);
                }
            }
        }
            Err(_) => {
                state.session_log("answer failed".to_string());
            }
    }
}

#[cfg(test)]
mod present_respawn_tests {
    use super::*;

    fn frame(w: usize, h: usize) -> golive_core::media::PresentedFrame {
        golive_core::media::PresentedFrame {
            w,
            h,
            rgba: vec![128u8; w * h * 4],
        }
    }

    #[test]
    fn retired_present_callback_does_not_reopen_a_window() {
        let state = Arc::new(AppState::new());
        let presented = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let alive = std::sync::atomic::AtomicBool::new(false);
        push_present_frame(&state, "watcher", "watcher", &presented, frame(2, 2), &alive);
        let inner = state.inner.lock().unwrap();
        assert!(inner.video_windows.is_empty());
        assert!(inner.video_feeds.is_empty());
        assert!(inner.link_tracks.is_empty());
    }

    #[test]
    fn fit_decision_reuses_match_respawns_mismatch() {
        // Same dims (bitrate/fps-only applies): reuse, old path untouched.
        assert!(window_fits(Some((1280, 720)), 1280, 720));
        // Encoder rebuilt at new dims: respawn.
        assert!(!window_fits(Some((1280, 720)), 640, 360));
        // No window yet (first frame): spawn path.
        assert!(!window_fits(None, 1280, 720));
        assert!(!window_fits(None, 0, 0));
    }

    #[test]
    fn respawn_line_carries_seq_and_direction_only() {
        // Attribution without identity: seq + dims direction, never member
        // ids, names, titles, or tokens.
        let line = respawn_line(7, (1280, 720), (1920, 1080));
        assert_eq!(line, "present window respawn #7 1280x720 -> 1920x1080");
        let back = respawn_line(8, (1920, 1080), (1280, 720));
        assert_eq!(back, "present window respawn #8 1920x1080 -> 1280x720");
        for token in ["watcher", "nick", "token", "sdp", "title"] {
            assert!(!line.contains(token), "identity leak: {token}");
        }
    }

    #[test]
    fn dim_change_respawns_window_and_accepts_new_frame() {
        // Regression: on quality-apply with different dims the helper would
        // reject the first new-dims frame ("frame size != contracted") and
        // die, freezing presentation forever. Routing two frames at
        // different dims through the on_frame decision must respawn the
        // window and accept the new frame — no user action, no re-signaling.
        // Spawning parks the feeder before any helper launch (no frame has
        // flowed), so no window server is needed for the routing asserts.
        let state = Arc::new(AppState::new());
        let presented = Arc::new(std::sync::atomic::AtomicU64::new(0));
        {
            let mut inner = state.inner.lock().expect("state lock");
            let (window, push) = crate::video::VideoWindow::spawn(
                "watcher".to_owned(),
                64,
                36,
                Arc::clone(&presented),
            );
            inner.video_windows.insert("watcher".to_owned(), window);
            inner.video_feeds.insert("watcher".to_owned(), push);
        }
        // Same-dims frame: exact old path, same window, feed untouched.
        push_present_frame(&state, "watcher", "watcher", &presented, frame(64, 36), &std::sync::atomic::AtomicBool::new(true));
        {
            let inner = state.inner.lock().expect("state lock");
            assert_eq!(inner.video_windows.get("watcher").expect("window").resolution(), (64, 36));
            assert_eq!(inner.video_windows.get("watcher").expect("window").pushed(), 1);
        }
        // New-dims frame (post quality-apply): the stale 64x36 window is
        // replaced, and the frame lands in the fresh feed. The replacement
        // reserves the next per-link sequence for the respawn line.
        push_present_frame(&state, "watcher", "watcher", &presented, frame(32, 18), &std::sync::atomic::AtomicBool::new(true));
        {
            let inner = state.inner.lock().expect("state lock");
            let window = inner.video_windows.get("watcher").expect("respawned window");
            assert_eq!(window.resolution(), (32, 18), "contract follows the new dims");
            assert_eq!(window.pushed(), 1, "new frame accepted by the fresh feed");
            assert_eq!(inner.video_seq.get("watcher"), Some(&1), "respawn carries seq #1");
            assert_eq!(inner.next_video_seq, 2, "counter advances past the reservation");
        }
        // Steady state again: same-dims frames reuse without respawn.
        push_present_frame(&state, "watcher", "watcher", &presented, frame(32, 18), &std::sync::atomic::AtomicBool::new(true));
        {
            let inner = state.inner.lock().expect("state lock");
            let window = inner.video_windows.get("watcher").expect("window");
            assert_eq!(window.resolution(), (32, 18));
            assert_eq!(window.pushed(), 2, "reuse feeds the live window");
            assert_eq!(inner.video_seq.get("watcher"), Some(&1), "reuse reserves nothing");
        }
        // A second link gets its own sequence: interleaved links stay
        // distinguishable from one flapping link in the session log.
        push_present_frame(&state, "other", "other", &presented, frame(64, 36), &std::sync::atomic::AtomicBool::new(true));
        {
            let inner = state.inner.lock().expect("state lock");
            assert_eq!(inner.video_seq.get("other"), Some(&2), "second link gets seq #2");
        }
        state.close_video_window("watcher");
        state.close_video_window("other");
        {
            let inner = state.inner.lock().expect("state lock");
            assert!(!inner.video_seq.contains_key("watcher"), "close forgets the seq");
            assert!(!inner.video_seq.contains_key("other"), "close forgets the seq");
        }
    }
}

#[cfg(test)]
mod host_trickle_tests {
    use super::*;

    fn wire(session: &str) -> WireIds {
        WireIds {
            session: session.to_owned(),
            share: "1".to_owned(),
            link: "2".to_owned(),
            attempt: "3".to_owned(),
        }
    }

    #[test]
    fn template_without_watcher_is_dropped() {
        // Idle template (key "", empty wire): no watcher adopted yet.
        let template = WireIds::default();
        let publisher = Arc::new(());
        let entries = [("", &template, &publisher)];
        assert!(
            select_host_trickle_target(entries.iter().copied(), &Arc::downgrade(&publisher)).is_none()
        );
    }

    #[test]
    fn adopted_link_is_addressed_to_its_watcher() {
        let template = WireIds::default();
        let live = wire("7");
        let publisher = Arc::new(());
        let entries = [("", &template, &publisher), ("watcher-abc", &live, &publisher)];
        let (to, ids) = select_host_trickle_target(entries.iter().copied(), &Arc::downgrade(&publisher))
            .expect("adopted link selected");
        assert_eq!(to, "watcher-abc");
        assert_eq!(ids.session, "7");
    }

    #[test]
    fn no_links_selects_nothing() {
        let publisher = Arc::new(());
        let entries: [(&str, &WireIds, &Arc<()>); 0] = [];
        assert!(
            select_host_trickle_target(entries.iter().copied(), &Arc::downgrade(&publisher)).is_none()
        );
    }

    #[test]
    fn callback_origin_survives_adoption_and_never_targets_another_publisher() {
        let first = Arc::new(());
        let second = Arc::new(());
        let origin = Arc::downgrade(&second);
        let a = wire("first");
        let b = wire("second");
        let entries = [("a", &a, &first), ("b", &b, &second)];
        let (to, ids) = select_host_trickle_target(entries.into_iter(), &origin).unwrap();
        assert_eq!((to.as_str(), ids.session.as_str()), ("b", "second"));
        assert!(select_host_trickle_target([("a", &a, &first)].into_iter(), &origin).is_none());
        assert!(select_host_trickle_target([("", &b, &second)].into_iter(), &origin).is_none());
        assert!(select_host_trickle_target([("b", &b, &second)].into_iter(), &origin).is_some());
    }

    #[test]
    fn host_candidate_and_ice_complete_envelopes_validate() {
        // Same envelope shapes the viewer path already understands
        // (PROTOCOL.md): candidate + ice-complete, no SDP anywhere.
        let ids = wire("7");
        let candidate = envelope(
            &ids,
            EnvelopeKind::Candidate,
            None,
            Some("candidate:1 1 udp 1 203.0.113.5 9 typ host".to_owned()),
        );
        assert!(check_envelope(&candidate).is_ok());
        assert!(current(&ids, &candidate));
        let complete = envelope(&ids, EnvelopeKind::IceComplete, None, None);
        assert!(check_envelope(&complete).is_ok());
        assert!(current(&ids, &complete));
    }

    #[test]
    fn relay_candidates_stay_rejected_on_host_path() {
        // No TURN in this product: relay candidates never reach the wire.
        let ids = wire("7");
        let relay = envelope(
            &ids,
            EnvelopeKind::Candidate,
            None,
            Some("candidate:9 1 udp 1 203.0.113.7 9 typ relay".to_owned()),
        );
        assert!(check_envelope(&relay).is_err());
    }

    #[test]
    fn stale_host_candidates_miss_the_current_fence() {
        let ids = wire("7");
        let mut stale = envelope(
            &ids,
            EnvelopeKind::Candidate,
            None,
            Some("candidate:1 1 udp 1 203.0.113.5 9 typ host".to_owned()),
        );
        stale.attempt = "2".to_owned();
        assert!(!current(&ids, &stale));
    }
}

#[cfg(test)]
mod rewatch_tests {
    use super::*;
    use crate::{EffectiveQuality, ShareSource};
    use golive_core::media::Quality;
    use golive_core::state::{LinkState, ShareState};
    use std::time::Duration;

    /// Opens session + live share on the owner only (no room/signal), so
    /// on_watch/on_unwatch run their real paths inside a `--lib` test.
    fn open_live_share(state: &AppState) {
        let inner = state.inner.lock().expect("state lock");
        let join = inner.owner.begin_join().expect("begin_join");
        inner.owner.complete_opened(&join).expect("opened");
        let start = inner.owner.begin_share_start().expect("begin_share_start");
        inner.owner.complete_share_live(&start).expect("live");
    }

    /// Seeds exactly what start_share leaves behind for a synthetic share:
    /// stored source + default effective + idle template publisher with its
    /// Share forward task. Real encode, no signal, no helper process — needs
    /// no honest-skip (synthetic never touches a window server).
    async fn seed_synthetic_template(state: &Arc<AppState>) {
        let live = Arc::new(std::sync::Mutex::new(Quality::P720.profile()));
        let (publisher, bridge, event_rx) =
            AppState::build_source_session(&ShareSource::Synthetic, Quality::P720.profile(), &live, None)
                .await
                .expect("template builds");
        assert!(bridge.is_none(), "synthetic owns no bridge");
        let publisher = Arc::new(tokio::sync::Mutex::new(publisher));
        {
            let mut inner = state.inner.lock().expect("state lock");
            inner.share_source = Some(ShareSource::Synthetic);
            inner.share_profile = Some(EffectiveQuality {
                profile: Quality::P720.profile(),
                generation: 0,
            });
            inner.publishers.insert(
                String::new(),
                crate::PublishSession {
                    publisher: Arc::clone(&publisher),
                    owner_fence: golive_core::owner::Fence::idle(),
                    wire: WireIds::default(),
                    remote_ready: false,
                    pending_remote: Vec::new(),
                    bridge: None,
                },
            );
            inner.tasks.push(spawn_forward(
                Arc::clone(state),
                None,
                event_rx,
                ForwardTarget::Share,
                Some(Arc::downgrade(&publisher)),
                None,
            ));
        }
    }

    /// The adopted wire fence must carry real ids (the pre-fix silent-refuse
    /// left no session at all; a half-adopted one would carry empties).
    fn assert_wire_live(state: &AppState, watcher: &str) {
        let inner = state.inner.lock().expect("state lock");
        let session = inner.publishers.get(watcher).expect("session live");
        assert!(!session.wire.session.is_empty(), "adopted wire carries session");
        assert!(!session.wire.share.is_empty(), "adopted wire carries share");
        assert!(!session.wire.link.is_empty(), "adopted wire carries link");
        assert!(!session.wire.attempt.is_empty(), "adopted wire carries attempt");
    }

    /// Reports the stored owner fence as transport-connected (stands in for
    /// the ICE Connected report the forward task delivers in production).
    fn mark_connected(state: &AppState, watcher: &str) {
        let fence = {
            state
                .inner
                .lock()
                .expect("state lock")
                .publishers
                .get(watcher)
                .expect("watcher session")
                .owner_fence
        };
        state
            .inner
            .lock()
            .expect("state lock")
            .owner
            .link_connected(&fence)
            .expect("link connected");
        let link = state
            .get_snapshot()
            .expect("snapshot")
            .links
            .into_iter()
            .find(|l| l.watcher == watcher)
            .expect("link snapshot");
        assert_eq!(link.state, LinkState::Connected, "fence lands on the link");
    }

    /// Waits until the Share forward task observes more host-side keyframes
    /// than `past` (the encode loop reports its own IDRs; publishers never
    /// decode, so `frames` stays 0 here — keyframes are the host liveness
    /// signal). Event-driven counters, never polled media.
    async fn wait_for_keyframes(state: &Arc<AppState>, past: u64, secs: u64) -> u64 {
        let deadline = std::time::Instant::now() + Duration::from_secs(secs);
        loop {
            let keyframes = state.get_media_counters().expect("counters").keyframes;
            if keyframes > past {
                return keyframes;
            }
            if std::time::Instant::now() > deadline {
                panic!("no keyframes flowed within {secs}s (past={past} now={keyframes})");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    #[test]
    fn stored_source_survives_watch_cycles() {
        // No media: the stored descriptor (+ shared live profile) must
        // outlive every watch/unwatch, and the share must stay Live.
        let state = AppState::new();
        open_live_share(&state);
        {
            let mut inner = state.inner.lock().expect("state lock");
            inner.share_source = Some(ShareSource::Display("d1".into()));
            inner.share_capture =
                Some(Arc::new(std::sync::Mutex::new(Quality::P720.profile())));
            inner.share_profile = Some(EffectiveQuality {
                profile: Quality::P720.profile(),
                generation: 0,
            });
        }
        for watcher in ["a", "b", "a"] {
            let fence = {
                let inner = state.inner.lock().expect("state lock");
                inner.owner.watch(watcher).expect("watch")
            };
            {
                let inner = state.inner.lock().expect("state lock");
                inner.owner.link_connected(&fence).expect("connected");
            }
            {
                let inner = state.inner.lock().expect("state lock");
                let fence = inner.owner.unwatch(watcher).expect("unwatch");
                inner.owner.complete_link_removed(&fence).expect("removed");
            }
            let inner = state.inner.lock().expect("state lock");
            assert!(
                matches!(inner.share_source, Some(ShareSource::Display(_))),
                "source kept after {watcher}"
            );
            assert!(inner.share_capture.is_some(), "live profile kept after {watcher}");
            assert_eq!(
                inner.owner.snapshot().share.state,
                ShareState::Live,
                "share live after {watcher}"
            );
        }
    }

    #[tokio::test]
    async fn rewatch_after_unwatch_gets_fresh_offer_and_frames() {
        // Exact user path: share → watch → connected → unwatch → re-watch →
        // new offer + connected + frames. Pre-fix the re-watch hit the
        // silent-refuse (template consumed, never restored): no publisher, no
        // offer, both sides stuck Negotiating forever.
        let state = Arc::new(AppState::new());
        open_live_share(&state);
        seed_synthetic_template(&state).await;

        // share → watch → connected → frames (template adoption path).
        on_watch(&state, &None, "w1").await;
        {
            let inner = state.inner.lock().expect("state lock");
            assert!(inner.publishers.contains_key("w1"), "first watch adopts");
            assert!(!inner.publishers.contains_key(""), "template consumed");
        }
        assert_wire_live(&state, "w1");
        mark_connected(&state, "w1");
        let frames1 = wait_for_keyframes(&state, 0, 20).await;

        // A second concurrent peer builds FRESH (previously silent-refuse).
        on_watch(&state, &None, "w2").await;
        {
            let inner = state.inner.lock().expect("state lock");
            assert!(inner.publishers.contains_key("w1"), "first link undisturbed");
            assert!(inner.publishers.contains_key("w2"), "second peer builds fresh");
        }

        // unwatch → template gone for good, share still live, source kept.
        on_unwatch(&state, "w1").await;
        on_unwatch(&state, "w2").await;
        {
            let inner = state.inner.lock().expect("state lock");
            assert!(inner.publishers.is_empty(), "both sessions dropped");
            assert_eq!(
                inner.owner.snapshot().share.state,
                ShareState::Live,
                "share survives unwatches"
            );
            assert!(
                matches!(inner.share_source, Some(ShareSource::Synthetic)),
                "stored source survives unwatches"
            );
        }

        // re-watch → NEW offer-capable publisher → connected → frames again.
        on_watch(&state, &None, "w1").await;
        {
            let inner = state.inner.lock().expect("state lock");
            assert!(
                inner.publishers.contains_key("w1"),
                "re-watch rebuilds (was silent-refuse)"
            );
        }
        assert_wire_live(&state, "w1");
        let publisher = {
            state
                .inner
                .lock()
                .expect("state lock")
                .publishers
                .get("w1")
                .expect("rebuilt session")
                .publisher
                .clone()
        };
        let sdp = publisher.lock().await.create_offer().await.expect("fresh offers");
        assert!(!sdp.is_empty(), "new offer produced on the rebuilt link");
        mark_connected(&state, "w1");
        let frames2 = wait_for_keyframes(&state, frames1, 20).await;
        assert!(frames2 > frames1, "keyframes flow on the rebuilt link");

        // Teardown through the real stop (also proves stop_share handles
        // rebuilt sessions + their forward tasks).
        state.stop_share().await.expect("stop_share");
        {
            let inner = state.inner.lock().expect("state lock");
            assert!(inner.publishers.is_empty());
            assert_eq!(inner.owner.snapshot().share.state, ShareState::Stopped);
            assert!(inner.share_source.is_none(), "stop clears the stored source");
        }
        state.leave().await.expect("leave");
    }
}

#[cfg(test)]
mod retired_events_tests {
    use super::*;

    #[tokio::test]
    async fn retired_viewer_events_cannot_mark_new_watch_connected() {
        let state = Arc::new(AppState::new());
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(MediaEvent::IceConnected).unwrap();
        drop(tx);
        let alive = Arc::new(std::sync::atomic::AtomicBool::new(false));
        spawn_forward(Arc::clone(&state), None, rx, ForwardTarget::Watch, None, Some(alive)).await.unwrap();
        assert!(!state.inner.lock().unwrap().media_counters.connected);
    }

    #[tokio::test]
    async fn retired_publisher_stats_cannot_consume_new_share_generation() {
        let state = Arc::new(AppState::new());
        let expected = super::super::EffectiveQuality {
            profile: golive_core::media::Quality::P720.profile(), generation: 0,
        };
        state.inner.lock().unwrap().share_profile = Some(expected);
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(MediaEvent::Stats(golive_core::media::MediaStats {
            generation: 99, ..Default::default()
        })).unwrap();
        drop(tx);
        spawn_forward(Arc::clone(&state), None, rx, ForwardTarget::Share, Some(Weak::new()), None).await.unwrap();
        assert_eq!(state.inner.lock().unwrap().share_profile.unwrap().generation, 0);
    }
}
