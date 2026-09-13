//! Desktop presentation routing. One bounded binary channel per surface; moving a
//! player never replaces its WebRTC session. GLV1 remains the headless harness.
use crate::AppState;
use golive_core::media::PresentedFrame;
use golive_core::trace::{Sample as TraceSample, Stage, Trace};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

#[derive(Clone, Serialize)]
pub struct PlayerState {
    pub member: String,
    pub popup: bool,
    pub volume: f32,
    pub muted: bool,
    pub title: String,
    pub mute_all: bool,
}
const ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

struct Sink {
    token: String,
    channel: Channel,
    flight: Option<(u32, u64, Instant)>,
}
struct Packet {
    channel: Channel,
    bytes: Vec<u8>,
    label: String,
    token: String,
    seq: u32,
}
pub(crate) struct Surface {
    pub state: PlayerState,
    popup_label: Option<String>,
    sinks: HashMap<String, Sink>,
    latest: Option<Arc<PresentedFrame>>,
    dirty: bool,
    seq: u32,
    pub presented: u64,
    pub stats: Mutex<crate::video::PresentStats>,
    trace: Trace,
    last_present: Option<Instant>,
    replaced: u64,
}
impl Surface {
    fn new(member: &str, title: &str, mute_all: bool) -> Self {
        Self {
            state: PlayerState {
                member: member.into(),
                title: title.into(),
                popup: false,
                volume: 1.0,
                muted: false,
                mute_all,
            },
            popup_label: None,
            sinks: HashMap::new(),
            latest: None,
            dirty: false,
            seq: 0,
            presented: 0,
            stats: Mutex::new(crate::video::PresentStats::default()),
            trace: Trace::new(Stage::Present),
            last_present: None,
            replaced: 0,
        }
    }
    fn offer(&mut self, frame: PresentedFrame) {
        if self.dirty && self.latest.is_some() {
            self.replaced += 1;
        }
        self.latest = Some(Arc::new(frame));
        self.dirty = true;
    }
    fn ack(&mut self, label: &str, token: &str, seq: u32, drawn: bool) -> Option<Packet> {
        let sink = self.sinks.get_mut(label)?;
        if sink.token != token || !sink.flight.is_some_and(|f| f.0 == seq) {
            return None;
        }
        let (_, bytes, sent) = sink.flight.take()?;
        let now = Instant::now();
        let gap = if drawn {
            self.last_present.replace(now).map(|last| now.duration_since(last).as_micros() as u64).unwrap_or(0)
        } else { 0 };
        self.trace.record(TraceSample {
            frames: drawn as u64,
            bytes: if drawn { bytes } else { 0 },
            dropped: std::mem::take(&mut self.replaced) + (!drawn) as u64,
            max_gap_us: gap,
            ..Default::default()
        }, Some(sent));
        if drawn {
            self.presented += 1;
            if let Ok(mut stats) = self.stats.lock() {
                stats.push(Instant::now(), bytes);
            }
        }
        self.dispatch()
    }
    fn dispatch(&mut self) -> Option<Packet> {
        let label = self.popup_label.as_deref().unwrap_or("main");
        let sink = self.sinks.get_mut(label)?;
        if let Some((_, _, sent)) = sink.flight {
            if sent.elapsed() >= ACK_TIMEOUT {
                sink.flight = None;
            } else {
                return None;
            }
        }
        if !self.dirty {
            return None;
        }
        let frame = self.latest.as_ref()?;
        self.dirty = false;
        self.seq = self.seq.wrapping_add(1);
        // LE u32 sequence / width / height, then tightly packed RGBA.
        let mut bytes = Vec::with_capacity(12 + frame.rgba.len());
        bytes.extend_from_slice(&self.seq.to_le_bytes());
        bytes.extend_from_slice(&(frame.w as u32).to_le_bytes());
        bytes.extend_from_slice(&(frame.h as u32).to_le_bytes());
        bytes.extend_from_slice(&frame.rgba);
        sink.flight = Some((self.seq, frame.rgba.len() as u64, Instant::now()));
        Some(Packet {
            channel: sink.channel.clone(),
            bytes,
            label: label.into(),
            token: sink.token.clone(),
            seq: self.seq,
        })
    }
}

fn send_packet(state: &AppState, member: &str, packet: Option<Packet>) {
    let Some(packet) = packet else { return };
    // Webview evaluation can block. Never hold the shared room state while sending.
    if packet
        .channel
        .send(InvokeResponseBody::Raw(packet.bytes))
        .is_err()
    {
        if let Ok(mut inner) = state.inner.lock() {
            if let Some(sink) = inner
                .players
                .get_mut(member)
                .and_then(|p| p.sinks.get_mut(&packet.label))
            {
                if sink.token == packet.token && sink.flight.is_some_and(|(seq, _, _)| seq == packet.seq) {
                    sink.flight = None;
                }
            }
        }
    }
}

impl AppState {
    /// Returns false only for the legacy headless test surface.
    pub(crate) fn present_inline(
        &self,
        member: &str,
        frame: PresentedFrame,
        alive: &std::sync::atomic::AtomicBool,
    ) -> bool {
        let Ok(mut inner) = self.inner.lock() else {
            return true;
        };
        if inner.desktop.is_none() {
            return false;
        }
        if !alive.load(std::sync::atomic::Ordering::Acquire) {
            return true;
        }
        let title = inner
            .link_tracks
            .get(member)
            .map(|t| t.title.clone())
            .unwrap_or_default();
        let mute_all = inner.player_mute_all;
        let surface = inner
            .players
            .entry(member.into())
            .or_insert_with(|| Surface::new(member, &title, mute_all));
        if surface.state.title.is_empty() {
            surface.state.title = title;
        }
        surface.offer(frame);
        let packet = surface.dispatch();
        drop(inner);
        send_packet(self, member, packet);
        true
    }
    pub(crate) fn remove_player(&self, member: &str) {
        let removed = self.inner.lock().ok().and_then(|mut inner| {
            inner
                .players
                .remove(member)
                .map(|surface| (inner.desktop.clone(), surface.popup_label))
        });
        if let Some((Some(app), label)) = removed {
            if let Some(window) = label.and_then(|l| app.get_webview_window(&l)) {
                let _ = window.close();
            }
            let _ = app.emit("player-ended", member);
        }
    }
}
fn allowed(surface: &Surface, window: &WebviewWindow) -> bool {
    window.label() == "main" || surface.popup_label.as_deref() == Some(window.label())
}
fn publish(app: &AppHandle, state: &PlayerState) {
    let _ = app.emit("player-state", state);
}

#[tauri::command]
pub fn player_attach(
    state: State<'_, Arc<AppState>>,
    window: WebviewWindow,
    member: String,
    token: String,
    channel: Channel,
) -> Result<PlayerState, String> {
    let mut inner = state.inner.lock().map_err(|_| "player unavailable")?;
    if !inner.viewers.contains_key(&member) {
        return Err("transmissão não está sendo assistida".into());
    }
    let title = inner
        .link_tracks
        .get(&member)
        .map(|t| t.title.clone())
        .unwrap_or_default();
    let mute_all = inner.player_mute_all;
    let surface = inner
        .players
        .entry(member.clone())
        .or_insert_with(|| Surface::new(&member, &title, mute_all));
    if !allowed(surface, &window) {
        return Err("player unavailable".into());
    }
    surface.sinks.insert(
        window.label().into(),
        Sink {
            token,
            channel,
            flight: None,
        },
    );
    surface.dirty = true;
    let snapshot = surface.state.clone();
    let packet = surface.dispatch();
    drop(inner);
    send_packet(&state, &member, packet);
    Ok(snapshot)
}
#[tauri::command]
pub fn player_detach(
    state: State<'_, Arc<AppState>>,
    window: WebviewWindow,
    member: String,
    token: String,
) {
    if let Ok(mut inner) = state.inner.lock() {
        if let Some(surface) = inner.players.get_mut(&member) {
            if surface
                .sinks
                .get(window.label())
                .is_some_and(|s| s.token == token)
            {
                surface.sinks.remove(window.label());
            }
        }
    }
}
#[tauri::command]
pub fn player_ack(
    state: State<'_, Arc<AppState>>,
    window: WebviewWindow,
    member: String,
    token: String,
    seq: u32,
    drawn: bool,
) {
    let packet = state.inner.lock().ok().and_then(|mut inner| {
        inner
            .players
            .get_mut(&member)?
            .ack(window.label(), &token, seq, drawn)
    });
    send_packet(&state, &member, packet);
}
#[tauri::command]
pub fn player_context(
    state: State<'_, Arc<AppState>>,
    window: WebviewWindow,
) -> Result<PlayerState, String> {
    state
        .inner
        .lock()
        .map_err(|_| "player unavailable")?
        .players
        .values()
        .find(|p| p.popup_label.as_deref() == Some(window.label()))
        .map(|p| p.state.clone())
        .ok_or("player unavailable".into())
}
#[tauri::command]
pub fn player_audio(
    state: State<'_, Arc<AppState>>,
    app: AppHandle,
    window: WebviewWindow,
    member: String,
    volume: f32,
    muted: bool,
) -> Result<(), String> {
    if !volume.is_finite() || !(0.0..=1.0).contains(&volume) {
        return Err("volume inválido".into());
    }
    let snapshot = {
        let mut inner = state.inner.lock().map_err(|_| "player unavailable")?;
        let surface = inner.players.get_mut(&member).ok_or("player unavailable")?;
        if !allowed(surface, &window) {
            return Err("player unavailable".into());
        }
        surface.state.volume = volume;
        surface.state.muted = muted;
        let snapshot = surface.state.clone();
        if let Some(playback) = inner.viewers.get(&member).and_then(|s| s.playback.as_ref()) {
            playback.set_gain(if muted || snapshot.mute_all {
                0.0
            } else {
                volume
            });
        }
        snapshot
    };
    publish(&app, &snapshot);
    Ok(())
}
#[tauri::command]
pub fn player_mute_all(
    state: State<'_, Arc<AppState>>,
    app: AppHandle,
    window: WebviewWindow,
    muted: bool,
) -> Result<(), String> {
    if window.label() != "main" {
        return Err("player unavailable".into());
    }
    let snapshots = {
        let mut inner = state.inner.lock().map_err(|_| "player unavailable")?;
        inner.player_mute_all = muted;
        let snapshots: Vec<_> = inner
            .players
            .values_mut()
            .map(|p| {
                p.state.mute_all = muted;
                p.state.clone()
            })
            .collect();
        for p in &snapshots {
            if let Some(playback) = inner
                .viewers
                .get(&p.member)
                .and_then(|s| s.playback.as_ref())
            {
                playback.set_gain(if muted || p.muted { 0.0 } else { p.volume });
            }
        }
        snapshots
    };
    for snapshot in snapshots {
        publish(&app, &snapshot);
    }
    Ok(())
}

fn return_to_room(state: &AppState, app: &AppHandle, member: &str, label: &str) {
    let snapshot = state.inner.lock().ok().and_then(|mut inner| {
        let surface = inner.players.get_mut(member)?;
        if surface.popup_label.as_deref() != Some(label) {
            return None;
        }
        surface.popup_label = None;
        surface.state.popup = false;
        surface.sinks.remove(label);
        surface.dirty = true;
        let packet = surface.dispatch();
        Some((surface.state.clone(), packet))
    });
    if let Some((snapshot, packet)) = snapshot {
        send_packet(state, member, packet);
        publish(app, &snapshot);
    }
}
#[tauri::command]
pub async fn player_popup(
    state: State<'_, Arc<AppState>>,
    app: AppHandle,
    window: WebviewWindow,
    member: String,
    popup: bool,
) -> Result<(), String> {
    // Serialize with unwatch/leave and another move; never hold Inner across window creation.
    let _operation = state.operations.lock().await;
    let (label, title) = {
        let mut inner = state.inner.lock().map_err(|_| "player unavailable")?;
        let seq = inner.next_video_seq;
        inner.next_video_seq += 1;
        let surface = inner.players.get_mut(&member).ok_or("player unavailable")?;
        if !allowed(surface, &window) {
            return Err("player unavailable".into());
        }
        if !popup {
            let label = surface.popup_label.clone();
            drop(inner);
            if let Some(label) = label {
                return_to_room(&state, &app, &member, &label);
                if let Some(window) = app.get_webview_window(&label) {
                    let _ = window.close();
                }
            }
            return Ok(());
        }
        if let Some(label) = &surface.popup_label {
            let label = label.clone();
            drop(inner);
            if let Some(window) = app.get_webview_window(&label) {
                let _ = window.set_focus();
            }
            return Ok(());
        }
        let label = format!("player-{seq}");
        surface.popup_label = Some(label.clone());
        surface.state.popup = true;
        (label, surface.state.title.clone())
    };
    let built =
        WebviewWindowBuilder::new(&app, &label, WebviewUrl::App("index.html?player=1".into()))
            .title(if title.is_empty() { "goDrinking" } else { &title })
            .inner_size(960.0, 600.0)
            .min_inner_size(400.0, 280.0)
            .build();
    let popup_window = match built {
        Ok(window) => window,
        Err(_) => {
            return_to_room(&state, &app, &member, &label);
            return Err("Não foi possível abrir o pop-up.".into());
        }
    };
    let owner = Arc::clone(&state);
    let app_clone = app.clone();
    let member_clone = member.clone();
    let label_clone = label.clone();
    popup_window.on_window_event(move |event| {
        if matches!(event, tauri::WindowEvent::Destroyed) {
            return_to_room(&owner, &app_clone, &member_clone, &label_clone);
        }
    });
    if let Ok(inner) = state.inner.lock() {
        if let Some(p) = inner.players.get(&member) {
            publish(&app, &p.state);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn surface(member: &str) -> Surface {
        let mut s = Surface::new(member, member, false);
        for label in ["main", "player-1"] {
            s.sinks.insert(
                label.into(),
                Sink {
                    token: label.into(),
                    flight: None,
                    channel: Channel::new(|_| Ok(())),
                },
            );
        }
        s
    }
    fn frame(value: u8, w: usize) -> PresentedFrame {
        PresentedFrame {
            w,
            h: 1,
            rgba: vec![value; w * 4],
        }
    }
    #[test]
    fn stalled_surface_keeps_only_latest_frame_and_rejects_stale_acks() {
        let mut s = surface("a");
        s.offer(frame(1, 2));
        let first = s.dispatch().unwrap();
        for n in 2..100 {
            s.offer(frame(n, 4));
            assert!(s.dispatch().is_none());
        }
        assert!(s.ack("main", "retired-token", first.seq, true).is_none());
        assert!(s.ack("main", "main", first.seq + 1, true).is_none());
        assert_eq!(s.presented, 0);
        let next = s.ack("main", "main", first.seq, true).unwrap();
        assert_eq!(u32::from_le_bytes(next.bytes[4..8].try_into().unwrap()), 4);
        assert_eq!(&next.bytes[12..], &[99; 16]);
        assert_eq!(s.presented, 1);
        assert!(s.ack("main", "main", first.seq, true).is_none());
        assert_eq!(s.presented, 1);
    }
    #[test]
    fn popup_moves_only_one_of_three_streams_and_returns_without_new_session() {
        let mut streams = [surface("a"), surface("b"), surface("c")];
        streams[0].popup_label = Some("player-1".into());
        for (index, s) in streams.iter_mut().enumerate() {
            s.offer(frame(index as u8, 2));
            let packet = s.dispatch().unwrap();
            assert_eq!(packet.label, if index == 0 { "player-1" } else { "main" });
            s.ack(&packet.label, &packet.token, packet.seq, true);
        }
        streams[0].popup_label = None;
        streams[0].offer(frame(42, 2));
        assert_eq!(streams[0].dispatch().unwrap().label, "main");
        assert_eq!(streams[1].presented, 1);
        assert_eq!(streams[2].presented, 1);
    }
    #[test]
    fn idle_video_moves_using_cached_frame_without_replaying_in_same_surface() {
        let mut s = surface("a");
        s.offer(frame(7, 2));
        let packet = s.dispatch().unwrap();
        assert!(s.ack("main", "main", packet.seq, true).is_none());
        assert!(s.dispatch().is_none());
        s.popup_label = Some("player-1".into());
        s.dirty = true;
        let moved = s.dispatch().unwrap();
        assert_eq!(&moved.bytes[12..], &[7; 8]);
        assert_eq!(moved.label, "player-1");
    }
    #[test]
    fn replacement_sink_cannot_be_released_by_old_mount() {
        let mut s = surface("a");
        s.offer(frame(1, 2));
        let first = s.dispatch().unwrap();
        s.sinks.get_mut("main").unwrap().token = "new-mount".into();
        assert!(s.ack("main", &first.token, first.seq, true).is_none());
        assert_eq!(s.presented, 0);
    }
    #[test]
    fn expired_flight_releases_the_surface_without_an_ack() {
        let mut s = surface("a");
        s.offer(frame(1, 2));
        let first = s.dispatch().unwrap();
        s.offer(frame(9, 2));
        assert!(s.dispatch().is_none());
        s.sinks.get_mut("main").unwrap().flight = Some((
            first.seq,
            8,
            Instant::now() - ACK_TIMEOUT - std::time::Duration::from_millis(1),
        ));
        let next = s.dispatch().unwrap();
        assert_ne!(next.seq, first.seq);
        assert_eq!(&next.bytes[12..], &[9; 8]);
    }
}
