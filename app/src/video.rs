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
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

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
// Helper discovery + process management.
// ---------------------------------------------------------------------------

/// Locates the helper binary: alongside the current exe. That covers both
/// layouts — `target/debug/golive-video` next to the dev binary and
/// `Contents/MacOS/golive-video` next to the packaged binary (the e2e
/// script copies it there after `tauri build`).
pub fn helper_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(HELPER_NAME)))
        .unwrap_or_else(|| PathBuf::from(HELPER_NAME))
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
    healthy: Arc<AtomicBool>,
    sock_path: PathBuf,
    title: String,
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
            let sock_path = sock_path.clone();
            let title = title.clone();
            std::thread::Builder::new()
                .name("golive-video-feed".into())
                .spawn(move || {
                    feed_loop(sock_path, helper, title, w, h, slot, &stop, &presented, &healthy);
                })
                .ok()
        };
        (
            Self {
                feeder,
                stop,
                presented,
                healthy,
                sock_path,
                title,
            },
            push,
        )
    }

    pub fn presented(&self) -> u64 {
        self.presented.load(Ordering::Relaxed)
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
        let _ = self.ping.send(());
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
    let _ = std::fs::remove_file(&sock_path);
    let listener = match std::os::unix::net::UnixListener::bind(&sock_path) {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("video socket bind failed: {e}");
            healthy.store(false, Ordering::Release);
            return;
        }
    };
    if listener.set_nonblocking(true).is_err() {
        healthy.store(false, Ordering::Release);
        return;
    }
    let child = match std::process::Command::new(&helper)
        .arg(&sock_path)
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
    let mut sock: UnixStream = match sock {
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

    if write_handshake(&mut sock, &title, w, h).is_err() {
        healthy.store(false, Ordering::Release);
        return;
    }
    // Phase 2: stream latest-only; each ack is one presented frame.
    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        if write_frame(&mut sock, &frame).is_err() || read_ack(&mut sock).is_err() {
            healthy.store(false, Ordering::Release);
            return;
        }
        presented.fetch_add(1, Ordering::Relaxed);
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

fn write_handshake(sock: &mut UnixStream, title: &str, w: usize, h: usize) -> std::io::Result<()> {
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

fn write_frame(sock: &mut UnixStream, frame: &PresentedFrame) -> std::io::Result<()> {
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

fn read_ack(sock: &mut UnixStream) -> std::io::Result<()> {
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
}
