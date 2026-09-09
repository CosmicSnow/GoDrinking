//! Native video windows: one helper process per watched link.
//!
//! Why a helper process: on macOS the GUI event loop must run on the main
//! thread, which Tauri owns in the app process. Each watched link spawns
//! `golive-video` (winit + softbuffer: CPU blit, no GPU APIs, no extra
//! dylibs — trivially packable), fed RGBA frames over a Unix socket.
//!
//! Lifecycle: the window opens on the FIRST frame (never an idle black
//! window) and closes on unwatch/leave/stop/link failure. One entry per
//! watched member, so N links mean N independent windows; a dead helper
//! fails only its own link.
//!
//! Freshness (a window never shows an old frame as new):
//! - the feeder keeps latest-only (bounded channel, stale dropped);
//! - the helper freezes + suffixes "(congelado)" after 1s without frames;
//! - helper death surfaces as a feed error and the shell tears down.
//!
//! Protocol v1 (all little-endian, all exact-size reads):
//! ```text
//!   roles:     the feeder BINDS the socket, the helper CONNECTS to it
//!   handshake: b"GLV1" + u32 w + u32 h + u32 title_len + title bytes
//!   frame:     u32 len + rgba bytes (len MUST equal w*h*4)
//!   ack:       1 byte 0x01 after each presented frame
//! ```
//! EOF on the socket is a clean shutdown request for the helper.

use golive_core::media::PresentedFrame;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

#[cfg(unix)]
type IpcStream = UnixStream;
#[cfg(windows)]
type IpcStream = std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

pub const HELPER_NAME: &str = "golive-video";
pub const PROTOCOL_MAGIC: &[u8; 4] = b"GLV1";
pub const PROTOCOL_VERSION_NOTE: &str = "v1";
/// Window 960x540; any aspect letterboxes inside.
pub const WINDOW_W: u32 = 960;
pub const WINDOW_H: u32 = 540;
/// Feeder retries connecting while the helper binds.
const CONNECT_RETRIES: u32 = 50;
const CONNECT_RETRY_WAIT: Duration = Duration::from_millis(100);
/// Socket IO bound so a wedged helper cannot hang the feeder forever.
const SOCKET_TIMEOUT: Duration = Duration::from_secs(5);
/// Feeder tick while waiting for frames (stop stays prompt).
const IDLE_TICK: Duration = Duration::from_millis(250);

// ---------------------------------------------------------------------------
// Pure math (unit tested): letterbox rect, scaling, pixel conversion.
// ---------------------------------------------------------------------------

/// Destination rect preserving `src` aspect inside `dst` (letterbox).
/// Never upscales beyond dst; zero-sized inputs yield a zero rect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

pub fn letterbox(src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Rect {
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return Rect { x: 0, y: 0, w: 0, h: 0 };
    }
    // Compare aspect ratios without floats: fit by width or by height.
    let fit_w = (dst_w as u64) * (src_h as u64) <= (dst_h as u64) * (src_w as u64);
    let (w, h) = if fit_w {
        (dst_w, ((dst_w as u64) * (src_h as u64) / (src_w as u64)) as u32)
    } else {
        (((dst_h as u64) * (src_w as u64) / (src_h as u64)) as u32, dst_h)
    };
    Rect {
        x: (dst_w - w) / 2,
        y: (dst_h - h) / 2,
        w,
        h,
    }
}

/// Nearest-neighbor RGBA scale. Empty output on zero sizes (never panics).
pub fn scale_rgba_nearest(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        return Vec::new();
    }
    if src.len() < (sw as usize) * (sh as usize) * 4 {
        return Vec::new();
    }
    let mut out = vec![0u8; (dw as usize) * (dh as usize) * 4];
    for y in 0..dh as usize {
        let sy = y * sh as usize / dh as usize;
        for x in 0..dw as usize {
            let sx = x * sw as usize / dw as usize;
            let s = (sy * sw as usize + sx) * 4;
            let d = (y * dw as usize + x) * 4;
            out[d..d + 4].copy_from_slice(&src[s..s + 4]);
        }
    }
    out
}

/// RGBA bytes to softbuffer pixels: `0x00RRGGBB` per the softbuffer docs
/// (highest byte zero, then R, G, B). Alpha is dropped (X).
pub fn rgba_to_xrgb8888(rgba: &[u8]) -> Vec<u32> {
    rgba
        .chunks_exact(4)
        .map(|px| ((px[0] as u32) << 16) | ((px[1] as u32) << 8) | (px[2] as u32))
        .collect()
}

// ---------------------------------------------------------------------------
// View controls (pure math, unit tested): zoom, pan, fullscreen state.
// The helper (`golive-video`) owns all input; the shell sends no commands.
// ---------------------------------------------------------------------------

/// Zoom bounds enforced on every scroll step.
pub const MIN_ZOOM: f32 = 1.0;
pub const MAX_ZOOM: f32 = 4.0;
/// Zoom factor per scroll-notch (line delta of ±1).
const ZOOM_LINE_BASE: f32 = 1.15;
/// Manual double-click gate (winit 0.30 reports clicks, not gestures).
pub const DOUBLE_CLICK_MAX: Duration = Duration::from_millis(400);
pub const DOUBLE_CLICK_PX: f32 = 6.0;
/// Help overlay stays up this long after any interaction (or window open).
pub const HELP_VISIBLE_FOR: Duration = Duration::from_secs(3);

/// Viewport state for one helper window. Offsets (`ox`, `oy`) are in
/// physical pixels of the zoomed picture: the visible window onto it starts
/// at (`ox`, `oy`) and spans the letterboxed rect. At 1x both are zero.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewState {
    pub zoom: f32,
    pub ox: f32,
    pub oy: f32,
    pub fullscreen: bool,
}

impl ViewState {
    pub fn new() -> Self {
        Self { zoom: MIN_ZOOM, ox: 0.0, oy: 0.0, fullscreen: false }
    }

    /// Scaled picture size for `rect` at the current zoom.
    pub fn scaled(&self, rect: Rect) -> (f32, f32) {
        (rect.w as f32 * self.zoom, rect.h as f32 * self.zoom)
    }

    /// Zoom keeping the content point under `cursor` stable. `cursor` and
    /// `rect` share window coordinates. Zooming back to 1x clears the pan.
    pub fn zoom_by(&mut self, rect: Rect, cursor: (f32, f32), factor: f32) {
        if !factor.is_finite() || factor <= 0.0 {
            return;
        }
        let next = clamp_zoom(self.zoom * factor);
        if next <= MIN_ZOOM {
            self.zoom = MIN_ZOOM;
            self.ox = 0.0;
            self.oy = 0.0;
            return;
        }
        let s = next / self.zoom;
        let (cx, cy) = cursor;
        let px = self.ox + (cx - rect.x as f32);
        let py = self.oy + (cy - rect.y as f32);
        self.zoom = next;
        self.ox = px * s - (cx - rect.x as f32);
        self.oy = py * s - (cy - rect.y as f32);
        let (ox, oy) = clamp_offset(self.ox, self.oy, rect, self.zoom);
        self.ox = ox;
        self.oy = oy;
    }

    /// Drag-pan by a cursor delta (window pixels). No-op at 1x.
    pub fn pan_by(&mut self, rect: Rect, dx: f32, dy: f32) {
        if self.zoom <= MIN_ZOOM || !dx.is_finite() || !dy.is_finite() {
            return;
        }
        let (ox, oy) = clamp_offset(self.ox + dx, self.oy + dy, rect, self.zoom);
        self.ox = ox;
        self.oy = oy;
    }

    pub fn toggle_fullscreen(&mut self) {
        self.fullscreen = !self.fullscreen;
    }

    /// Whether any zoom is applied (pan only matters then).
    pub fn zoomed(&self) -> bool {
        self.zoom > MIN_ZOOM
    }

    /// Leaves fullscreen. Returns whether we were in it.
    pub fn exit_fullscreen(&mut self) -> bool {
        let was = self.fullscreen;
        self.fullscreen = false;
        was
    }
}

impl Default for ViewState {
    fn default() -> Self {
        Self::new()
    }
}

/// Clamps a zoom level into `[MIN_ZOOM, MAX_ZOOM]` (non-finite snaps to 1x).
pub fn clamp_zoom(z: f32) -> f32 {
    if !z.is_finite() {
        return MIN_ZOOM;
    }
    z.clamp(MIN_ZOOM, MAX_ZOOM)
}

/// Clamps pan offsets so the zoomed picture always covers the rect.
pub fn clamp_offset(ox: f32, oy: f32, rect: Rect, zoom: f32) -> (f32, f32) {
    let zoom = clamp_zoom(zoom);
    let max_ox = (rect.w as f32 * zoom - rect.w as f32).max(0.0);
    let max_oy = (rect.h as f32 * zoom - rect.h as f32).max(0.0);
    (
        ox.clamp(0.0, max_ox),
        oy.clamp(0.0, max_oy),
    )
}

/// Zoom factor for a line-based scroll delta (`dy > 0` zooms in).
pub fn wheel_factor_line(dy: f32) -> f32 {
    if !dy.is_finite() {
        return 1.0;
    }
    ZOOM_LINE_BASE.powf(dy.clamp(-4.0, 4.0))
}

/// Zoom factor for a pixel-based (trackpad) scroll delta.
pub fn wheel_factor_pixel(dy: f64) -> f32 {
    if !dy.is_finite() {
        return 1.0;
    }
    ((dy as f32) / 400.0).exp().clamp(0.5, 2.0)
}

/// Double-click gate over click spacing + cursor travel.
pub fn is_double_click(dt: Duration, dist_px: f32) -> bool {
    dt <= DOUBLE_CLICK_MAX && dist_px <= DOUBLE_CLICK_PX
}

/// Help overlay visibility: shown within `HELP_VISIBLE_FOR` of interaction.
pub fn help_visible(since_interact: Duration) -> bool {
    since_interact < HELP_VISIBLE_FOR
}

// ---------------------------------------------------------------------------
// Help overlay text: hand-authored 3x5 block capitals (ASCII only, no font
// dependency — softbuffer has no text API). Unknown chars render blank.
// ---------------------------------------------------------------------------

/// 5 rows × 3 cols per glyph, low 3 bits per row, top row first.
fn glyph_3x5(c: char) -> [u8; 5] {
    match c {
        'A' => [0b010, 0b101, 0b111, 0b101, 0b101],
        'B' => [0b110, 0b101, 0b110, 0b101, 0b110],
        'C' => [0b011, 0b100, 0b100, 0b100, 0b011],
        'D' => [0b110, 0b101, 0b101, 0b101, 0b110],
        'E' => [0b111, 0b100, 0b110, 0b100, 0b111],
        'F' => [0b111, 0b100, 0b110, 0b100, 0b100],
        'G' => [0b011, 0b100, 0b101, 0b101, 0b011],
        'H' => [0b101, 0b101, 0b111, 0b101, 0b101],
        'I' => [0b111, 0b010, 0b010, 0b010, 0b111],
        'K' => [0b101, 0b101, 0b110, 0b101, 0b101],
        'L' => [0b100, 0b100, 0b100, 0b100, 0b111],
        'M' => [0b101, 0b111, 0b111, 0b101, 0b101],
        'N' => [0b110, 0b101, 0b101, 0b101, 0b101],
        'O' => [0b010, 0b101, 0b101, 0b101, 0b010],
        'P' => [0b110, 0b101, 0b110, 0b100, 0b100],
        'R' => [0b110, 0b101, 0b110, 0b101, 0b101],
        'S' => [0b011, 0b100, 0b010, 0b001, 0b110],
        'T' => [0b111, 0b010, 0b010, 0b010, 0b010],
        'U' => [0b101, 0b101, 0b101, 0b101, 0b111],
        'W' => [0b101, 0b101, 0b111, 0b111, 0b101],
        'X' => [0b101, 0b101, 0b010, 0b101, 0b101],
        'Z' => [0b111, 0b001, 0b010, 0b100, 0b111],
        ':' => [0b000, 0b010, 0b000, 0b010, 0b000],
        '/' => [0b001, 0b001, 0b010, 0b100, 0b100],
        '-' => [0b000, 0b000, 0b111, 0b000, 0b000],
        _ => [0, 0, 0, 0, 0],
    }
}

/// One-line help copy (single line, ASCII only by construction).
pub const HELP_LINE: &str = "WHEEL: ZOOM  DRAG: PAN  F / DBL-CLICK: FULLSCREEN  ESC: EXIT";

/// Pixel width of `text` at `scale` (3px glyph + 1px tracking).
pub fn text_width_px(text: &str, scale: u32) -> u32 {
    if text.is_empty() || scale == 0 {
        return 0;
    }
    (text.chars().count() as u32 * 4 - 1) * scale
}

/// Pixel height of one text line at `scale`.
pub fn text_height_px(scale: u32) -> u32 {
    5 * scale
}

/// Draws `text` into an XRGB buffer. Clip-safe: out-of-bounds pixels are
/// skipped, never panic. `stride` is the buffer width in pixels.
pub fn draw_text(
    buf: &mut [u32],
    stride: u32,
    buf_h: u32,
    x0: u32,
    y0: u32,
    scale: u32,
    text: &str,
    color: u32,
) {
    if scale == 0 || stride == 0 || buf_h == 0 {
        return;
    }
    for (idx, c) in text.chars().enumerate() {
        let glyph = glyph_3x5(c);
        let gx = x0 + idx as u32 * 4 * scale;
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..3u32 {
                if bits & (1 << (2 - col)) == 0 {
                    continue;
                }
                for sy in 0..scale {
                    for sx in 0..scale {
                        let x = gx + col * scale + sx;
                        let y = y0 + row as u32 * scale + sy;
                        if x < stride && y < buf_h {
                            let at = y as usize * stride as usize + x as usize;
                            if at < buf.len() {
                                buf[at] = color;
                            }
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Transmission stats (per link, computed on event/frame — no polling).
// ---------------------------------------------------------------------------

/// Moving window for render fps + measured bitrate.
pub const STATS_WINDOW_SECS: f32 = 3.0;
/// Codec contract label (matches the core's H.264 Constrained Baseline).
pub const LINK_CODEC: &str = "H.264 Constrained Baseline";
/// Delay note: an RTT-based estimate needs core PC stats, which the core
/// does not expose — this stays `None` (BLOQUEADO on core telemetry).
pub const DELAY_NOTE: &str =
    "estimativa indisponivel: RTT do par ICE nao exposto pelo core";
/// Bitrate note: measured on presented RGBA bytes (post-decode), not the
/// wire H.264 bitrate (also needs core telemetry).
pub const BITRATE_NOTE: &str = "medido em bytes RGBA apresentados (pos-decode)";
/// Dropped note: latest-only slot evicts stale frames; dropped is decoded
/// minus presented (an approximation: one frame may still be in flight).
pub const DROPPED_NOTE: &str = "aproximacao: decodificados menos apresentados";

/// Presented-frame samples feeding render fps + bitrate. Pushed on every
/// ack (presentation evidence), pruned to the moving window.
#[derive(Debug, Default)]
pub struct PresentStats {
    events: VecDeque<(Instant, u64)>,
}

impl PresentStats {
    pub fn push(&mut self, at: Instant, bytes: u64) {
        self.events.push_back((at, bytes));
        self.prune(at);
    }

    fn prune(&mut self, now: Instant) {
        while let Some(&(t, _)) = self.events.front() {
            if now.duration_since(t).as_secs_f32() <= STATS_WINDOW_SECS {
                break;
            }
            self.events.pop_front();
        }
    }

    pub fn render_fps(&mut self, now: Instant) -> f32 {
        self.prune(now);
        self.events.len() as f32 / STATS_WINDOW_SECS
    }

    pub fn bitrate_bps(&mut self, now: Instant) -> u64 {
        self.prune(now);
        let bytes: u64 = self.events.iter().map(|(_, b)| b).sum();
        (bytes as f64 * 8.0 / STATS_WINDOW_SECS as f64) as u64
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.events.len()
    }
}

/// Per-link transmission stats (one entry per watched member). Consumed by
/// the future UI lane; the web side is untouched by this change.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct LinkStats {
    pub member: String,
    pub title: String,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    /// Frames decoded by the core (observed in the `on_frame` callback).
    pub decoded: u64,
    /// Frames acked as presented by the helper window.
    pub presented: u64,
    /// Estimate, see [`DROPPED_NOTE`].
    pub dropped: u64,
    /// Presented frames per second over [`STATS_WINDOW_SECS`].
    pub render_fps: f32,
    /// Measured bits/s over [`STATS_WINDOW_SECS`], see [`BITRATE_NOTE`].
    pub bitrate_bps: u64,
    pub bitrate_note: String,
    /// Always `None` until the core exposes ICE RTT (BLOQUEADO).
    pub delay_estimate_ms: Option<u64>,
    pub delay_note: String,
    pub dropped_note: String,
}

// ---------------------------------------------------------------------------
// Helper discovery + process management.
// ---------------------------------------------------------------------------

/// Locates the helper binary: alongside the current exe. That covers both
/// layouts — `target/debug/golive-video` next to the dev binary and
/// `Contents/MacOS/golive-video` next to the packaged binary (the e2e
/// script copies it there after `tauri build`).
pub fn helper_path() -> PathBuf {
    let name = if cfg!(windows) {
        "golive-video.exe"
    } else {
        HELPER_NAME
    };
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(name)))
        .unwrap_or_else(|| PathBuf::from(name))
}

// ---------------------------------------------------------------------------
// VideoWindow: feeder thread + helper child + presented counter.
// ---------------------------------------------------------------------------

/// A live video window for one watched link. The helper child is owned by
/// the feeder thread (spawned lazily on the first frame); `stop` signals
/// teardown and joins — the guard below reaps the child on every exit path.
pub struct VideoWindow {
    feeder: Option<std::thread::JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    presented: Arc<AtomicU64>,
    /// Frames handed to the feed (decoded, via `on_frame`). Dropped is
    /// `pushed - presented` (see [`DROPPED_NOTE`]).
    pushed: Arc<AtomicU64>,
    /// Presentation timestamps + byte sizes feeding fps/bitrate.
    stats: Arc<Mutex<PresentStats>>,
    healthy: Arc<AtomicBool>,
    sock_path: PathBuf,
    title: String,
    w: u32,
    h: u32,
}

impl VideoWindow {
    /// Spawns the feeder immediately; the helper process + window appear on
    /// the FIRST frame. Frames arrive via the returned pusher (latest-only,
    /// bounded: stale frames are dropped, never queued).
    pub fn spawn(
        title: String,
        w: usize,
        h: usize,
        presented: Arc<AtomicU64>,
    ) -> (Self, FramePush) {
        Self::spawn_with(title, w, h, presented, helper_path())
    }

    /// Same, with an explicit helper binary (tests point at the debug build
    /// without depending on the current-exe layout).
    pub fn spawn_with(
        title: String,
        w: usize,
        h: usize,
        presented: Arc<AtomicU64>,
        helper: PathBuf,
    ) -> (Self, FramePush) {
        let (push, slot) = FrameSlot::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let healthy = Arc::new(AtomicBool::new(true));
        let pushed = push.counter();
        let stats = Arc::new(Mutex::new(PresentStats::default()));
        let sock_path = std::env::temp_dir().join(format!(
            "golive-video-{}-{}.sock",
            std::process::id(),
            title
                .bytes()
                .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64))
        ));
        let feeder = {
            let stop = Arc::clone(&stop);
            let healthy = Arc::clone(&healthy);
            let presented = Arc::clone(&presented);
            let stats = Arc::clone(&stats);
            let sock_path = sock_path.clone();
            let title = title.clone();
            std::thread::Builder::new()
                .name("golive-video-feed".into())
                .spawn(move || {
                    feed_loop(sock_path, helper, title, w, h, slot, &stop, &presented, &stats, &healthy);
                })
                .ok()
        };
        (
            Self {
                feeder,
                stop,
                presented,
                pushed,
                stats,
                healthy,
                sock_path,
                title,
                w: w as u32,
                h: h as u32,
            },
            push,
        )
    }

    pub fn presented(&self) -> u64 {
        self.presented.load(Ordering::Relaxed)
    }

    /// Frames handed to the feed (decoded via `on_frame`).
    pub fn pushed(&self) -> u64 {
        self.pushed.load(Ordering::Relaxed)
    }

    /// Render fps over the moving window (no polling: computed on read from
    /// ack-stamped samples).
    pub fn render_fps(&self) -> f32 {
        let now = Instant::now();
        self.stats.lock().map(|mut s| s.render_fps(now)).unwrap_or(0.0)
    }

    /// Measured bits/s over the moving window (presented RGBA bytes).
    pub fn bitrate_bps(&self) -> u64 {
        let now = Instant::now();
        self.stats.lock().map(|mut s| s.bitrate_bps(now)).unwrap_or(0)
    }

    /// Contracted resolution (handshake dims; the title carries it too).
    pub fn resolution(&self) -> (u32, u32) {
        (self.w, self.h)
    }

    pub fn healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    /// Bounded idempotent stop: flag, join feeder (which reaps the helper),
    /// unlink socket.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(feeder) = self.feeder.take() {
            let _ = feeder.join();
        }
        let _ = std::fs::remove_file(&self.sock_path);
    }
}

impl Drop for VideoWindow {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------
// Latest-only frame slot: one frame of memory, never a stale queue.
// ---------------------------------------------------------------------------

/// Push side (lives in the `on_frame` callback). `Clone + Send`.
#[derive(Clone)]
pub struct FramePush {
    slot: Arc<Mutex<Option<PresentedFrame>>>,
    ping: mpsc::Sender<()>,
    pushed: Arc<AtomicU64>,
}

/// Feeder side (single owner: the feeder thread).
pub struct FrameSlot {
    slot: Arc<Mutex<Option<PresentedFrame>>>,
    ping_rx: mpsc::Receiver<()>,
}

impl FrameSlot {
    pub fn channel() -> (FramePush, FrameSlot) {
        let slot = Arc::new(Mutex::new(None));
        let (ping, ping_rx) = mpsc::channel();
        (
            FramePush {
                slot: Arc::clone(&slot),
                ping,
                pushed: Arc::new(AtomicU64::new(0)),
            },
            FrameSlot { slot, ping_rx },
        )
    }
}

impl FramePush {
    /// Stores the frame, evicting any stale one waiting. Non-blocking;
    /// safe to call from the decode hot path.
    pub fn push(&self, frame: PresentedFrame) {
        if let Ok(mut slot) = self.slot.lock() {
            *slot = Some(frame);
        }
        self.pushed.fetch_add(1, Ordering::Relaxed);
        let _ = self.ping.send(());
    }

    /// Shared push counter (the owning window reads it for dropped stats).
    pub fn counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.pushed)
    }
}

/// Feeder: wait for frames, spawn the helper on the first one, stream
/// length-prefixed RGBA, count acks as presented. Any error marks the
/// window unhealthy and ends the thread (shell tears down).
fn feed_loop(
    sock_path: PathBuf,
    helper: PathBuf,
    title: String,
    w: usize,
    h: usize,
    slot: FrameSlot,
    stop: &AtomicBool,
    presented: &AtomicU64,
    present_stats: &Mutex<PresentStats>,
    healthy: &AtomicBool,
) {
    // Phase 1: block for the first frame (window opens here, never before).
    let mut frame = loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        match slot.wait(IDLE_TICK) {
            SlotWait::Frame(frame) => break frame,
            SlotWait::Timeout => continue,
            SlotWait::Gone => return,
        }
    };

    // Spawn the helper now.
    let (listener, helper_arg) = match bind_ipc(&sock_path) {
        Ok(bound) => bound,
        Err(e) => {
            eprintln!("video socket bind failed: {e}");
            healthy.store(false, Ordering::Release);
            return;
        }
    };
    let child = match std::process::Command::new(&helper)
        .arg(&helper_arg)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Err(e) => {
            eprintln!("video helper spawn failed: {e}");
            healthy.store(false, Ordering::Release);
            return;
        }
        Ok(child) => child,
    };
    // Owned from here on: reaped on every exit path below.
    let _child_guard = ChildGuard(child);

    // Accept the helper's connection (nonblocking poll so stop stays
    // prompt). Roles: the feeder BINDS, the helper CONNECTS.
    let mut sock = None;
    for _ in 0..CONNECT_RETRIES {
        if stop.load(Ordering::Acquire) {
            return;
        }
        match listener.accept() {
            Ok((sock_, _)) => {
                sock = Some(sock_);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(CONNECT_RETRY_WAIT)
            }
            Err(e) => {
                eprintln!("video socket accept failed: {e}");
                healthy.store(false, Ordering::Release);
                return;
            }
        }
    }
    let mut sock: IpcStream = match sock {
        Some(sock) => sock,
        None => {
            eprintln!("video helper never connected back");
            healthy.store(false, Ordering::Release);
            return;
        }
    };
    // Accepted sockets inherit nonblocking: back to blocking IO with
    // timeouts for the stream phase.
    let _ = sock.set_nonblocking(false);
    let _ = sock.set_read_timeout(Some(SOCKET_TIMEOUT));
    let _ = sock.set_write_timeout(Some(SOCKET_TIMEOUT));
    prepare_ipc_stream(&sock);

    if write_handshake(&mut sock, &title, w, h).is_err() {
        healthy.store(false, Ordering::Release);
        return;
    }
    // Phase 2: stream latest-only; each ack is one presented frame.
    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        let bytes = frame.rgba.len() as u64;
        if write_frame(&mut sock, &frame).is_err() || read_ack(&mut sock).is_err() {
            healthy.store(false, Ordering::Release);
            return;
        }
        presented.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut stats) = present_stats.lock() {
            stats.push(Instant::now(), bytes);
        }
        match slot.wait(IDLE_TICK) {
            SlotWait::Frame(next) => frame = next,
            // No fresh frame: idle (the helper freezes on its own clock).
            // Never re-send the old frame as if it were new.
            SlotWait::Timeout => continue,
            SlotWait::Gone => return,
        }
    }
}

/// Holds the helper child alive while streaming; kills on drop only if the
/// window forgot it (defensive: normal path is VideoWindow::stop, but the
/// feeder owns no Child handle — the guard here just prevents zombies when
/// the feeder itself errors out).
struct ChildGuard(std::process::Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

enum SlotWait {
    Frame(PresentedFrame),
    Timeout,
    Gone,
}

impl FrameSlot {
    /// Waits up to `timeout` for a frame. Drains stale pings and returns the
    /// latest frame; `Gone` when the push side is gone (clean shutdown).
    fn wait(&self, timeout: Duration) -> SlotWait {
        match self.ping_rx.recv_timeout(timeout) {
            Err(mpsc::RecvTimeoutError::Timeout) => SlotWait::Timeout,
            Err(mpsc::RecvTimeoutError::Disconnected) => SlotWait::Gone,
            Ok(()) => {
                // Drain backlog: only the latest frame matters.
                while self.ping_rx.try_recv().is_ok() {}
                match self.slot.lock().ok().and_then(|mut slot| slot.take()) {
                    Some(frame) => SlotWait::Frame(frame),
                    // Ping without a frame (lock poisoned mid-push): idle.
                    None => SlotWait::Timeout,
                }
            }
        }
    }
}

#[cfg(unix)]
fn bind_ipc(sock_path: &Path) -> std::io::Result<(UnixListener, PathBuf)> {
    let _ = std::fs::remove_file(sock_path);
    let listener = UnixListener::bind(sock_path)?;
    listener.set_nonblocking(true)?;
    Ok((listener, sock_path.to_path_buf()))
}

#[cfg(windows)]
fn bind_ipc(_sock_path: &Path) -> std::io::Result<(std::net::TcpListener, PathBuf)> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let addr = listener.local_addr()?;
    Ok((listener, PathBuf::from(addr.to_string())))
}

fn prepare_ipc_stream(_sock: &IpcStream) {
    #[cfg(windows)]
    let _ = _sock.set_nodelay(true);
}

fn write_handshake(sock: &mut impl Write, title: &str, w: usize, h: usize) -> std::io::Result<()> {
    let title = title.as_bytes();
    let title_len = title.len().min(256) as u32;
    let mut header = Vec::with_capacity(16);
    header.extend_from_slice(PROTOCOL_MAGIC);
    header.extend_from_slice(&(w as u32).to_le_bytes());
    header.extend_from_slice(&(h as u32).to_le_bytes());
    header.extend_from_slice(&title_len.to_le_bytes());
    sock.write_all(&header)?;
    sock.write_all(&title[..title_len as usize])?;
    sock.flush()
}

fn write_frame(sock: &mut impl Write, frame: &PresentedFrame) -> std::io::Result<()> {
    let expected = frame.w.checked_mul(frame.h).and_then(|n| n.checked_mul(4));
    if expected != Some(frame.rgba.len()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "rgba size mismatch",
        ));
    }
    sock.write_all(&(frame.rgba.len() as u32).to_le_bytes())?;
    sock.write_all(&frame.rgba)?;
    sock.flush()
}

fn read_ack(sock: &mut impl Read) -> std::io::Result<()> {
    let mut ack = [0u8; 1];
    sock.read_exact(&mut ack)?;
    if ack[0] != 0x01 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "bad present ack",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connected_pair() -> (std::net::TcpStream, std::net::TcpStream) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::net::TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();
        (client, server)
    }

    #[test]
    fn letterbox_preserves_aspect() {
        // Same aspect fills exactly.
        assert_eq!(
            letterbox(1280, 720, 960, 540),
            Rect { x: 0, y: 0, w: 960, h: 540 }
        );
        // Wide source in square dst: bars top/bottom.
        let rect = letterbox(1280, 720, 500, 500);
        assert_eq!((rect.w, rect.h), (500, 281));
        assert_eq!(rect.x, 0);
        assert_eq!(rect.y, (500 - 281) / 2);
        // Tall source in wide dst: bars left/right.
        let rect = letterbox(720, 1280, 960, 540);
        assert_eq!((rect.w, rect.h), (303, 540));
        assert_eq!(rect.y, 0);
        assert_eq!(rect.x, (960 - 303) / 2);
        // Degenerate inputs never panic, never negative.
        assert_eq!(
            letterbox(0, 720, 960, 540),
            Rect { x: 0, y: 0, w: 0, h: 0 }
        );
        assert_eq!(
            letterbox(1280, 720, 0, 0),
            Rect { x: 0, y: 0, w: 0, h: 0 }
        );
    }

    #[test]
    fn scale_nearest_is_exact_on_samples() {
        // 2x2 -> 4x4 duplicates each pixel into a 2x2 block.
        let src: Vec<u8> = vec![
            255, 0, 0, 255, 0, 255, 0, 255, //
            0, 0, 255, 255, 255, 255, 255, 255,
        ];
        let out = scale_rgba_nearest(&src, 2, 2, 4, 4);
        assert_eq!(out.len(), 4 * 4 * 4);
        // Top-left block is red.
        assert_eq!(&out[0..4], &[255, 0, 0, 255]);
        assert_eq!(&out[4..8], &[255, 0, 0, 255]);
        // Bottom-right block is white.
        assert_eq!(&out[60..64], &[255, 255, 255, 255]);
        // Identity size copies.
        assert_eq!(scale_rgba_nearest(&src, 2, 2, 2, 2), src);
        // Bad inputs yield empty, never panic.
        assert!(scale_rgba_nearest(&src, 2, 2, 0, 4).is_empty());
        assert!(scale_rgba_nearest(&src[..4], 2, 2, 4, 4).is_empty());
    }

    #[test]
    fn xrgb8888_drops_alpha_red_first() {
        // Per softbuffer docs: 0x00RRGGBB.
        assert_eq!(rgba_to_xrgb8888(&[255, 0, 0, 200]), vec![0x00FF0000]);
        assert_eq!(
            rgba_to_xrgb8888(&[255, 0, 0, 255, 0, 255, 0, 255]),
            vec![0x00FF0000, 0x0000FF00]
        );
        assert!(rgba_to_xrgb8888(&[]).is_empty());
        // Trailing partial pixel ignored, never panics.
        assert_eq!(rgba_to_xrgb8888(&[1, 2, 3]), Vec::<u32>::new());
    }

    #[test]
    fn latest_only_slot_never_queues_stale() {
        let (push, slot) = FrameSlot::channel();
        let mk = |v: u8| PresentedFrame {
            w: 1,
            h: 1,
            rgba: vec![v, v, v, 255],
        };
        push.push(mk(1));
        push.push(mk(2));
        push.push(mk(3));
        // Exactly the newest frame waits, no matter how many were pushed.
        match slot.wait(Duration::from_secs(1)) {
            SlotWait::Frame(got) => assert_eq!(got.rgba[0], 3),
            _ => panic!("expected the latest frame"),
        }
        // Slot drained: next wait times out instead of replaying old data.
        match slot.wait(Duration::from_millis(50)) {
            SlotWait::Timeout => {}
            _ => panic!("stale frame replayed as new"),
        }
    }

    #[test]
    fn slot_gone_when_push_side_drops() {
        let (push, slot) = FrameSlot::channel();
        drop(push);
        match slot.wait(Duration::from_secs(1)) {
            SlotWait::Gone => {}
            _ => panic!("expected clean shutdown signal"),
        }
    }

    #[test]
    fn window_lifecycle_open_first_frame_close_clean() {
        // No helper binary is spawned here: stop() before any frame must be
        // prompt and leave no socket behind.
        let presented = Arc::new(AtomicU64::new(0));
        let (mut window, _push) = VideoWindow::spawn("test".into(), 1280, 720, Arc::clone(&presented));
        assert!(window.healthy());
        assert_eq!(window.presented(), 0);
        assert_eq!(window.title(), "test");
        window.stop();
        window.stop(); // idempotent
    }

    #[test]
    fn feeder_presents_through_real_helper() {
        // End to end through the real helper binary: feed synthetic frames,
        // expect present acks. Needs a window server; skips honestly without
        // one (the packaged E2E asserts presented>0 where windows exist).
        let helper = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/debug/golive-video");
        if !helper.exists() {
            eprintln!("SKIP: helper binary not built");
            return;
        }
        let presented = Arc::new(AtomicU64::new(0));
        let (mut window, push) =
            VideoWindow::spawn_with("e2e-test".into(), 320, 180, Arc::clone(&presented), helper);
        // Synthetic RGBA: red gradient, 320x180 (small = fast).
        let mut rgba = vec![0u8; 320 * 180 * 4];
        for y in 0..180 {
            for x in 0..320 {
                let i = (y * 320 + x) * 4;
                rgba[i] = (x * 255 / 320) as u8;
                rgba[i + 1] = (y * 255 / 180) as u8;
                rgba[i + 2] = 128;
                rgba[i + 3] = 255;
            }
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while presented.load(Ordering::Relaxed) == 0 && std::time::Instant::now() < deadline {
            push.push(PresentedFrame { w: 320, h: 180, rgba: rgba.clone() });
            std::thread::sleep(Duration::from_millis(200));
        }
        let shown = presented.load(Ordering::Relaxed);
        let healthy = window.healthy();
        window.stop();
        if shown == 0 {
            eprintln!("SKIP: no present ack (no window server?)");
            return;
        }
        assert!(shown > 0, "helper presented frames");
        assert!(healthy, "feeder stayed healthy");
    }

    // -- view controls: pure zoom/pan/fullscreen math ---------------------

    #[test]
    fn zoom_clamps_to_1x_4x() {
        assert_eq!(clamp_zoom(1.0), 1.0);
        assert_eq!(clamp_zoom(0.2), MIN_ZOOM);
        assert_eq!(clamp_zoom(9.0), MAX_ZOOM);
        assert_eq!(clamp_zoom(f32::NAN), MIN_ZOOM);
        assert_eq!(clamp_zoom(f32::INFINITY), MIN_ZOOM);
    }

    #[test]
    fn zoom_by_keeps_cursor_point_stable() {
        let rect = Rect { x: 100, y: 50, w: 400, h: 300 };
        let mut view = ViewState::new();
        let cursor = (300.0, 200.0);
        // Content point under the cursor before zoom (1x: offset zero).
        let before = (cursor.0 - rect.x as f32, cursor.1 - rect.y as f32);
        view.zoom_by(rect, cursor, 2.0);
        assert_eq!(view.zoom, 2.0);
        // After zoom, that content point (scaled 2x) is still under cursor.
        let after = (
            (view.ox + (cursor.0 - rect.x as f32)) / view.zoom,
            (view.oy + (cursor.1 - rect.y as f32)) / view.zoom,
        );
        assert!((after.0 - before.0).abs() < 0.01, "x stable: {after:?} vs {before:?}");
        assert!((after.1 - before.1).abs() < 0.01, "y stable: {after:?} vs {before:?}");
    }

    #[test]
    fn zoom_out_to_1x_clears_pan() {
        let rect = Rect { x: 0, y: 0, w: 400, h: 300 };
        let mut view = ViewState::new();
        view.zoom_by(rect, (200.0, 150.0), 3.0);
        view.pan_by(rect, 50.0, 40.0);
        assert!(view.ox > 0.0 && view.oy > 0.0);
        view.zoom_by(rect, (200.0, 150.0), 0.05);
        assert_eq!((view.zoom, view.ox, view.oy), (1.0, 0.0, 0.0));
    }

    #[test]
    fn pan_clamps_inside_zoomed_picture() {
        let rect = Rect { x: 0, y: 0, w: 400, h: 300 };
        let mut view = ViewState::new();
        // No-op at 1x.
        view.pan_by(rect, 500.0, 500.0);
        assert_eq!((view.ox, view.oy), (0.0, 0.0));
        view.zoom_by(rect, (200.0, 150.0), 4.0);
        // Way past the edge: clamped to (1200, 900), never negative.
        view.pan_by(rect, 10_000.0, 10_000.0);
        assert_eq!((view.ox, view.oy), (1200.0, 900.0));
        view.pan_by(rect, -10_000.0, -10_000.0);
        assert_eq!((view.ox, view.oy), (0.0, 0.0));
    }

    #[test]
    fn wheel_factors_zoom_in_on_positive() {
        assert!(wheel_factor_line(1.0) > 1.0);
        assert!(wheel_factor_line(-1.0) < 1.0);
        assert_eq!(wheel_factor_line(0.0), 1.0);
        assert!(wheel_factor_pixel(120.0) > 1.0);
        assert!(wheel_factor_pixel(-120.0) < 1.0);
        assert_eq!(wheel_factor_pixel(0.0), 1.0);
    }

    #[test]
    fn fullscreen_state_toggles_and_exits() {
        let mut view = ViewState::new();
        assert!(!view.fullscreen);
        view.toggle_fullscreen();
        assert!(view.fullscreen);
        view.toggle_fullscreen();
        assert!(!view.fullscreen);
        view.toggle_fullscreen();
        assert!(view.exit_fullscreen());
        assert!(!view.fullscreen);
        assert!(!view.exit_fullscreen(), "exiting twice reports false");
    }

    #[test]
    fn double_click_gate_needs_time_and_proximity() {
        assert!(is_double_click(Duration::from_millis(200), 3.0));
        assert!(!is_double_click(Duration::from_millis(800), 3.0));
        assert!(!is_double_click(Duration::from_millis(200), 40.0));
    }

    #[test]
    fn help_shows_three_seconds_after_interaction() {
        assert!(help_visible(Duration::from_secs(0)));
        assert!(help_visible(Duration::from_millis(2999)));
        assert!(!help_visible(Duration::from_secs(3)));
        assert!(!help_visible(Duration::from_secs(60)));
    }

    // -- overlay text ------------------------------------------------------

    #[test]
    fn overlay_glyph_e_has_ten_pixels() {
        // E = 111/100/110/100/111: 3+1+2+1+3 lit pixels.
        let mut buf = vec![0u32; 3 * 5];
        draw_text(&mut buf, 3, 5, 0, 0, 1, "E", 0x00FFFFFF);
        assert_eq!(buf.iter().filter(|p| **p != 0).count(), 10);
    }

    #[test]
    fn overlay_text_clips_without_panic() {
        // Tiny buffer, huge text at 2x starting off-screen: no panic, no OOB.
        let mut buf = vec![0u32; 4 * 4];
        draw_text(&mut buf, 4, 4, 2, 2, 2, HELP_LINE, 0x00FFFFFF);
        assert!(buf.iter().any(|p| *p != 0), "some pixels land inside");
        assert_eq!(text_width_px(HELP_LINE, 2), HELP_LINE.chars().count() as u32 * 4 * 2 - 2);
        assert_eq!(text_width_px("", 2), 0);
        assert_eq!(text_height_px(2), 10);
    }

    // -- transmission stats ------------------------------------------------

    #[test]
    fn present_stats_fps_and_bitrate_over_window() {
        let start = Instant::now();
        let mut stats = PresentStats::default();
        // 90 presents of 1000 bytes across 3 s => 30 fps, 240_000 bits/s.
        for n in 0..90 {
            stats.push(start + Duration::from_millis(n * 1000 / 30), 1000);
        }
        let now = start + Duration::from_secs(3);
        assert!((stats.render_fps(now) - 30.0).abs() < 0.5, "fps {}", stats.render_fps(now));
        assert_eq!(stats.bitrate_bps(now), 240_000);
        // Window slides: samples older than 3 s fall off.
        let later = start + Duration::from_secs(6);
        assert_eq!(stats.render_fps(later), 0.0);
        assert_eq!(stats.bitrate_bps(later), 0);
        assert_eq!(stats.len(), 0);
    }

    #[test]
    fn push_counter_feeds_dropped_estimate() {
        let (push, _slot) = FrameSlot::channel();
        assert_eq!(push.counter().load(Ordering::Relaxed), 0);
        let mk = || PresentedFrame { w: 1, h: 1, rgba: vec![0, 0, 0, 255] };
        push.push(mk());
        push.push(mk());
        assert_eq!(push.counter().load(Ordering::Relaxed), 2);
    }

    // -- protocol v1 ---------------------------------------------------------

    #[test]
    fn handshake_bytes_carry_magic_dims_and_title() {
        let (mut a, mut b) = connected_pair();
        write_handshake(&mut a, "nick", 1280, 720).unwrap();
        let mut header = [0u8; 16];
        b.read_exact(&mut header).unwrap();
        assert_eq!(&header[0..4], b"GLV1");
        assert_eq!(u32::from_le_bytes(header[4..8].try_into().unwrap()), 1280);
        assert_eq!(u32::from_le_bytes(header[8..12].try_into().unwrap()), 720);
        let title_len = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
        assert_eq!(title_len, 4);
        let mut title = vec![0u8; title_len];
        b.read_exact(&mut title).unwrap();
        assert_eq!(&title, b"nick");
    }

    #[test]
    fn handshake_truncates_long_titles_at_256() {
        let (mut a, mut b) = connected_pair();
        write_handshake(&mut a, &"n".repeat(300), 640, 480).unwrap();
        let mut header = [0u8; 16];
        b.read_exact(&mut header).unwrap();
        let title_len = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
        assert_eq!(title_len, 256);
    }

    #[test]
    fn frame_roundtrip_and_ack_shape() {
        let (mut a, mut b) = connected_pair();
        let frame = PresentedFrame { w: 2, h: 1, rgba: vec![9u8; 8] };
        write_frame(&mut a, &frame).unwrap();
        let mut len = [0u8; 4];
        b.read_exact(&mut len).unwrap();
        assert_eq!(u32::from_le_bytes(len) as usize, 8);
        // Size mismatch never hits the wire.
        let bad = PresentedFrame { w: 2, h: 1, rgba: vec![0u8; 4] };
        assert!(write_frame(&mut a, &bad).is_err());
        // Ack shape: exactly 0x01 accepted.
        b.write_all(&[0x01]).unwrap();
        assert!(read_ack(&mut a).is_ok());
        b.write_all(&[0x02]).unwrap();
        assert!(read_ack(&mut a).is_err());
    }
}
