//! `golive-video`: single native video window fed by the app shell.
//!
//! The app process owns no GUI thread (Tauri owns main there), so each
//! watched link spawns one of these helpers — this binary OWNS its main
//! thread and runs the winit event loop here. Rendering is softbuffer CPU
//! blit (no GPU APIs, no extra dylibs): aspect preserved with letterbox,
//! `present` is vsync-backed where the platform supports it.
//!
//! Controls (all local to this window — the shell sends no commands):
//! scroll zooms 1x–4x centered on the cursor, left-drag pans while zoomed,
//! `F` or double-click toggles fullscreen, `Esc` leaves fullscreen. A
//! one-line help overlay shows for 3 s after any interaction.
//!
//! Wire protocol v1 (little-endian, exact-size reads; see `golive_app::video`):
//! handshake `GLV1` + u32 w + u32 h + u32 title_len + title, then per frame
//! u32 len + RGBA bytes, acking `0x01` after each present. EOF = clean exit.
//! No frame for 1s freezes the last picture and suffixes "(congelado)" —
//! a window never presents an old frame as new, and never spins an idle
//! black window (the shell only spawns us on the first frame).

use golive_app::video::{
    draw_text, help_visible, is_double_click, letterbox, rgba_to_xrgb8888, scale_rgba_nearest,
    text_height_px, text_width_px, ViewState, HELP_LINE, WINDOW_H, WINDOW_W,
};
use std::io::{Read, Write};
use std::num::NonZeroU32;
use std::sync::{mpsc, Arc};

#[cfg(unix)]
type IpcStream = std::os::unix::net::UnixStream;
#[cfg(windows)]
type IpcStream = std::net::TcpStream;
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Fullscreen, Window, WindowId};

/// No frame within this long: freeze + "(congelado)" title suffix.
const FROZEN_AFTER: Duration = Duration::from_secs(1);
/// Socket reads tick the frozen check even when silent.
const READ_TICK: Duration = Duration::from_millis(250);
/// Hard caps: protocol-level sanity, never large allocations from the wire.
const MAX_DIM: usize = 4096;
const MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;

type Disp = Arc<Window>;
type Surf = softbuffer::Surface<Disp, Disp>;

/// Reader thread to main loop: at most 2 frames in flight (latest-only is
/// enforced by the shell feeder; this bound keeps helper memory flat).
#[derive(Debug)]
enum Inbox {
    Frame(Vec<u8>),
    Gone,
}

/// Wake-up ping (winit 0.30 has no `wake_up`, only `send_event`).
#[derive(Debug)]
enum UserWake {
    Tick,
}

/// Help overlay text scale + padding (single bottom bar line).
const HELP_SCALE: u32 = 2;
const HELP_PAD: u32 = 6;

struct App {
    window: Option<Arc<Window>>,
    surface: Option<Surf>,
    /// Keep the display connection resident as long as the surface.
    /// (Declared after `surface` so it drops after it.)
    _context: Option<softbuffer::Context<Disp>>,
    base_title: String,
    frozen: bool,
    last_present: Instant,
    ack: Option<IpcStream>,
    inbox: mpsc::Receiver<Inbox>,
    gone: bool,
    src_w: u32,
    src_h: u32,
    view: ViewState,
    cursor: (f32, f32),
    dragging: bool,
    drag_last: (f32, f32),
    last_press: Option<(Instant, f32, f32)>,
    interacted_at: Instant,
    help_active: bool,
    /// Latest frame, kept for zoom/pan/fullscreen re-renders (re-blits send
    /// no ack and touch no clock: only newly presented frames do).
    last: Option<Vec<u8>>,
}

impl App {
    fn render_frame(&mut self, rgba: &[u8]) {
        self.last = Some(rgba.to_vec());
        if self.blit() {
            self.last_present = Instant::now();
            if self.frozen {
                self.frozen = false;
                if let Some(window) = self.window.as_ref() {
                    window.set_title(&self.base_title);
                }
            }
            if let Some(ack) = self.ack.as_mut() {
                // Best effort: a failed ack means the shell is gone;
                // the reader thread will notice EOF and end us.
                let _ = ack.write_all(&[0x01]);
            }
        }
    }

    /// Re-blits the latest frame (zoom/pan/fullscreen/resize/frozen-tick).
    /// Never acks, never touches the frozen clock.
    fn render_last(&mut self) {
        if self.last.is_some() {
            self.blit();
        }
    }

    /// Zoom-aware blit + one-line help overlay. Returns whether a buffer was
    /// presented.
    fn blit(&mut self) -> bool {
        let (window, surface) = match (self.window.as_ref(), self.surface.as_mut()) {
            (Some(window), Some(surface)) => (window, surface),
            _ => return false,
        };
        let size = window.inner_size();
        let (vw, vh) = (size.width, size.height);
        if vw == 0 || vh == 0 {
            return false;
        }
        let rgba = match self.last.clone() {
            Some(rgba) => rgba,
            None => return false,
        };
        let rect = letterbox(self.src_w, self.src_h, vw, vh);
        if rect.w == 0 || rect.h == 0 {
            return false;
        }
        // Zoomed picture size, then the visible window onto it at (ox, oy).
        let sw = (rect.w as f32 * self.view.zoom).round() as u32;
        let sh = (rect.h as f32 * self.view.zoom).round() as u32;
        if sw == 0 || sh == 0 {
            return false;
        }
        let scaled = if sw == self.src_w && sh == self.src_h {
            rgba
        } else {
            scale_rgba_nearest(&rgba, self.src_w, self.src_h, sw, sh)
        };
        let pixels = rgba_to_xrgb8888(&scaled);
        if pixels.len() != sw as usize * sh as usize {
            return false;
        }
        let presented = if let Ok(mut buffer) = surface.buffer_mut() {
            // Black bars are the default: fill all, then blit the visible
            // slice of the zoomed picture.
            buffer.fill(0);
            let ox = self.view.ox.round() as i64;
            let oy = self.view.oy.round() as i64;
            let dx0 = rect.x as i64 - ox;
            let dy0 = rect.y as i64 - oy;
            let stride = vw as usize;
            for (row, chunk) in pixels.chunks_exact(sw as usize).enumerate() {
                let y = dy0 + row as i64;
                if y < 0 || y >= vh as i64 {
                    continue;
                }
                let skip = (-dx0).max(0) as usize;
                let dst_x = dx0.max(0) as usize;
                if skip >= chunk.len() || dst_x >= stride {
                    continue;
                }
                let len = (chunk.len() - skip).min(stride - dst_x);
                let start = y as usize * stride + dst_x;
                if start + len <= buffer.len() {
                    buffer[start..start + len].copy_from_slice(&chunk[skip..skip + len]);
                }
            }
            // One-line help bar (bottom, 3 s after any interaction).
            if self.help_active && help_visible(self.interacted_at.elapsed()) {
                let bar_h = text_height_px(HELP_SCALE) + HELP_PAD * 2;
                if vh >= bar_h && vw > 20 {
                    let y0 = vh - bar_h;
                    for y in y0..vh {
                        let row = y as usize * stride;
                        for x in 0..vw as usize {
                            if row + x < buffer.len() {
                                buffer[row + x] = 0x00111111;
                            }
                        }
                    }
                    let tw = text_width_px(HELP_LINE, HELP_SCALE);
                    let tx = HELP_PAD.min(vw.saturating_sub(tw + HELP_PAD));
                    draw_text(
                        &mut buffer[..],
                        vw,
                        vh,
                        tx,
                        y0 + HELP_PAD,
                        HELP_SCALE,
                        HELP_LINE,
                        0x00FFFFFF,
                    );
                }
            }
            buffer.present().is_ok()
        } else {
            false
        };
        presented
    }

    fn check_frozen(&mut self) {
        if self.frozen || self.last_present.elapsed() < FROZEN_AFTER {
            return;
        }
        self.frozen = true;
        if let Some(window) = self.window.as_ref() {
            window.set_title(&format!("{} (congelado)", self.base_title));
        }
    }

    /// Marks a user interaction (restarts the 3 s help overlay).
    fn touch_help(&mut self) {
        self.interacted_at = Instant::now();
        self.help_active = true;
    }

    fn apply_fullscreen(&mut self) {
        if let Some(window) = self.window.as_ref() {
            window.set_fullscreen(if self.view.fullscreen {
                Some(Fullscreen::Borderless(None))
            } else {
                None
            });
        }
    }

    fn letterbox_rect(&self) -> Option<golive_app::video::Rect> {
        self.window.as_ref().map(|window| {
            let size = window.inner_size();
            letterbox(self.src_w, self.src_h, size.width, size.height)
        })
    }

    fn shutdown(&mut self, event_loop: &ActiveEventLoop) {
        event_loop.exit();
    }
}

impl ApplicationHandler<UserWake> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title(&self.base_title)
            .with_inner_size(LogicalSize::new(WINDOW_W as f64, WINDOW_H as f64))
            .with_resizable(false);
        let window: Arc<Window> = match event_loop.create_window(attrs) {
            Ok(window) => Arc::new(window),
            Err(e) => {
                eprintln!("golive-video: window create failed: {e}");
                event_loop.exit();
                return;
            }
        };
        let context = match softbuffer::Context::new(Arc::clone(&window)) {
            Ok(context) => context,
            Err(e) => {
                eprintln!("golive-video: softbuffer context failed: {e}");
                event_loop.exit();
                return;
            }
        };
        let mut surface = match Surf::new(&context, Arc::clone(&window)) {
            Ok(surface) => surface,
            Err(e) => {
                eprintln!("golive-video: softbuffer surface failed: {e}");
                event_loop.exit();
                return;
            }
        };
        let size = window.inner_size();
        if let (Some(vw), Some(vh)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
        {
            if surface.resize(vw, vh).is_err() {
                eprintln!("golive-video: surface resize failed");
                event_loop.exit();
                return;
            }
        }
        self.window = Some(window);
        self._context = Some(context);
        self.surface = Some(surface);
        self.last_present = Instant::now();
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => self.shutdown(event_loop),
            WindowEvent::Resized(size) => {
                if let Some(surface) = self.surface.as_mut() {
                    if let (Some(w), Some(h)) =
                        (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
                    {
                        let _ = surface.resize(w, h);
                    }
                }
                self.render_last();
            }
            WindowEvent::CursorMoved { position, .. } => {
                let (x, y) = (position.x as f32, position.y as f32);
                if self.dragging {
                    let dx = x - self.drag_last.0;
                    let dy = y - self.drag_last.1;
                    if dx != 0.0 || dy != 0.0 {
                        if let Some(rect) = self.letterbox_rect() {
                            self.view.pan_by(rect, dx, dy);
                        }
                        self.touch_help();
                        self.render_last();
                    }
                    self.drag_last = (x, y);
                }
                self.cursor = (x, y);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let factor = match delta {
                    MouseScrollDelta::LineDelta(_, dy) => {
                        golive_app::video::wheel_factor_line(dy)
                    }
                    MouseScrollDelta::PixelDelta(pos) => {
                        golive_app::video::wheel_factor_pixel(pos.y)
                    }
                };
                if factor != 1.0 {
                    if let Some(rect) = self.letterbox_rect() {
                        let cursor = self.cursor;
                        self.view.zoom_by(rect, cursor, factor);
                    }
                    // Re-anchor an in-progress drag onto the new zoom.
                    self.drag_last = self.cursor;
                    self.touch_help();
                    self.render_last();
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if button != MouseButton::Left {
                    return;
                }
                match state {
                    ElementState::Pressed => {
                        let now = Instant::now();
                        let (x, y) = self.cursor;
                        let dbl = match self.last_press {
                            Some((t, lx, ly)) => {
                                let dist = ((x - lx).powi(2) + (y - ly).powi(2)).sqrt();
                                is_double_click(now.duration_since(t), dist)
                            }
                            None => false,
                        };
                        if dbl {
                            self.last_press = None;
                            self.view.toggle_fullscreen();
                            self.apply_fullscreen();
                            self.touch_help();
                            self.render_last();
                        } else {
                            self.last_press = Some((now, x, y));
                            if self.view.zoomed() {
                                self.dragging = true;
                                self.drag_last = (x, y);
                            }
                        }
                    }
                    ElementState::Released => {
                        self.dragging = false;
                    }
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != ElementState::Pressed || event.repeat {
                    return;
                }
                match event.physical_key {
                    PhysicalKey::Code(KeyCode::KeyF) => {
                        self.view.toggle_fullscreen();
                        self.apply_fullscreen();
                        self.touch_help();
                        self.render_last();
                    }
                    PhysicalKey::Code(KeyCode::Escape) => {
                        if self.view.exit_fullscreen() {
                            self.apply_fullscreen();
                            self.touch_help();
                            self.render_last();
                        }
                    }
                    _ => {}
                }
            }
            WindowEvent::RedrawRequested => {
                self.render_last();
            }
            _ => {}
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, _event: UserWake) {
        // A reader tick: drain inbox below in about_to_wait. (Payloads ride
        // the channel, not the event, so bursts stay bounded.)
        let _ = event_loop;
        self.drain_inbox();
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.drain_inbox();
        if self.gone_flag() {
            self.shutdown(event_loop);
            return;
        }
        // Help overlay expiry repaints without a new frame.
        if self.help_active && !help_visible(self.interacted_at.elapsed()) {
            self.help_active = false;
            self.render_last();
        }
        self.check_frozen();
    }
}

impl App {
    fn drain_inbox(&mut self) {
        // Drain inbox (reader wakes us per frame; normally one item).
        while let Ok(msg) = self.inbox.try_recv() {
            match msg {
                Inbox::Frame(rgba) => self.render_frame(&rgba),
                Inbox::Gone => {
                    self.gone = true;
                    break;
                }
            }
        }
    }

    fn gone_flag(&self) -> bool {
        self.gone
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("golive-video: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let sock_path = std::env::args()
        .nth(1)
        .ok_or_else(|| "usage: golive-video <socket-path>".to_string())?;
    let mut sock = connect_ipc(&sock_path).map_err(|e| format!("connect: {e}"))?;
    sock.set_read_timeout(Some(READ_TICK))
        .map_err(|e| format!("socket timeout: {e}"))?;
    let (title, src_w, src_h) = read_handshake(&mut sock)?;

    let event_loop = EventLoop::<UserWake>::with_user_event()
        .build()
        .map_err(|e| format!("event loop: {e}"))?;
    let proxy = event_loop.create_proxy();

    // Acks go out on a clone so the reader never blocks on writes.
    let ack = sock
        .try_clone()
        .map_err(|e| format!("ack stream: {e}"))?;

    // Reader thread: frames in, wake-ups out. Bounded handoff (cap 2);
    // latest-only is enforced by the shell feeder upstream.
    let (tx, rx) = mpsc::sync_channel::<Inbox>(2);
    std::thread::Builder::new()
        .name("golive-video-read".into())
        .spawn(move || {
            let tick = || {
                let _ = proxy.send_event(UserWake::Tick);
            };
            loop {
                match read_frame(&mut sock, src_w, src_h) {
                    Ok(Some(rgba)) => {
                        if tx.send(Inbox::Frame(rgba)).is_err() {
                            break;
                        }
                        tick();
                    }
                    // Quiet tick: main loop owns the frozen clock.
                    Ok(None) => {
                        tick();
                    }
                    Err(_) => break,
                }
            }
            let _ = tx.send(Inbox::Gone);
            tick();
        })
        .map_err(|e| format!("reader thread: {e}"))?;

    let mut app = App {
        window: None,
        surface: None,
        _context: None,
        base_title: format!("GoLive — {title} · {src_w}x{src_h}"),
        frozen: false,
        last_present: Instant::now(),
        ack: Some(ack),
        inbox: rx,
        gone: false,
        src_w,
        src_h,
        view: ViewState::new(),
        cursor: (0.0, 0.0),
        dragging: false,
        drag_last: (0.0, 0.0),
        last_press: None,
        interacted_at: Instant::now(),
        help_active: true,
        last: None,
    };
    event_loop.set_control_flow(ControlFlow::Wait);
    event_loop
        .run_app(&mut app)
        .map_err(|e| format!("run: {e}"))?;
    Ok(())
}

fn connect_ipc(addr: &str) -> std::io::Result<IpcStream> {
    #[cfg(unix)]
    {
        std::os::unix::net::UnixStream::connect(addr)
    }
    #[cfg(windows)]
    {
        let sock = std::net::TcpStream::connect(addr)?;
        sock.set_nodelay(true)?;
        Ok(sock)
    }
}

fn read_handshake(sock: &mut IpcStream) -> Result<(String, u32, u32), String> {
    // Blocking reads with an overall bound: the shell connects promptly.
    sock.set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| format!("socket timeout: {e}"))?;
    let mut header = [0u8; 16];
    sock.read_exact(&mut header)
        .map_err(|e| format!("handshake: {e}"))?;
    if &header[0..4] != b"GLV1" {
        return Err("bad protocol magic".to_string());
    }
    let w = u32::from_le_bytes(header[4..8].try_into().unwrap());
    let h = u32::from_le_bytes(header[8..12].try_into().unwrap());
    let title_len = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
    if w == 0 || h == 0 || (w as usize) > MAX_DIM || (h as usize) > MAX_DIM || title_len > 256 {
        return Err("bad handshake sizes".to_string());
    }
    let mut title = vec![0u8; title_len];
    sock.read_exact(&mut title)
        .map_err(|e| format!("handshake title: {e}"))?;
    let title = String::from_utf8_lossy(&title).into_owned();
    sock.set_read_timeout(Some(READ_TICK))
        .map_err(|e| format!("socket timeout: {e}"))?;
    Ok((title, w, h))
}

/// Reads one frame for the contracted size. Ok(Some) = full frame,
/// Ok(None) = quiet tick, Err = session over (EOF, timeout-mid-frame,
/// size mismatch — the shell tears down; never render partial data).
fn read_frame(sock: &mut IpcStream, w: u32, h: u32) -> Result<Option<Vec<u8>>, String> {
    let expected = (w as usize)
        .checked_mul(h as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(|| "bad contracted size".to_string())?;
    let mut len_buf = [0u8; 4];
    let mut filled = 0;
    while filled < 4 {
        match sock.read(&mut len_buf[filled..]) {
            Ok(0) => return Err("peer closed".to_string()),
            Ok(n) => filled += n,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                if filled == 0 {
                    return Ok(None);
                }
                return Err("truncated length".to_string());
            }
            Err(e) => return Err(format!("read: {e}")),
        }
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len != expected {
        return Err(format!("frame size {len} != contracted {expected}"));
    }
    if len > MAX_FRAME_BYTES {
        return Err("frame too large".to_string());
    }
    let mut rgba = vec![0u8; len];
    if let Err(e) = sock.read_exact(&mut rgba) {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            return Err("peer closed".to_string());
        }
        return Err(format!("read frame: {e}"));
    }
    Ok(Some(rgba))
}
