//! Windows Graphics Capture (WGC) source, enumeration, and stream adapter.
//!
//! The adapter drives `windows-capture`'s free-threaded capture pipeline on a
//! dedicated thread. Each BGRA8 frame is copied once into an `Arc<[u8]>` and
//! handed to both the bounded preview queue and the OpenH264 encoder queue
//! (sharing the same storage). The capture session is owned by the adapter and
//! stopped explicitly on session stop or drop, so no capture thread leaks.
//!
//! Windows has no mix-minus system-audio tap; per-app audio exclusion is
//! reported as unsupported by `capabilities.rs`.

use super::logger;
use super::pipeline::{EncoderCommand, NativeFrame, PreviewDiagnostics};
use super::types::{
    CaptureSource, CreateMediaSessionRequest, FrameRate, NativeCaptureSource, NativeRunningApp,
    NativeSourceKind, VideoResolution,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    GraphicsCaptureItemType, MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

/// A capture source resolved from a session request.
enum CaptureTarget {
    Monitor(Monitor),
    Window(Window),
}

/// Handler flags carried through `Settings` into `CaptureHandler::new`.
struct CaptureFlags {
    capture_tx: SyncSender<NativeFrame>,
    encoder_tx: SyncSender<EncoderCommand>,
    diagnostics: Arc<PreviewDiagnostics>,
    generation: u64,
    target_width: u32,
    target_height: u32,
    min_frame_interval: Duration,
}

/// Error type required by `GraphicsCaptureApiHandler`. Frame-handling errors
/// are logged and swallowed (returning `Ok`) so a single bad frame never ends
/// the capture session; this type is only used for fatal setup failures.
#[derive(Debug)]
pub(crate) struct CaptureError(pub(crate) String);

impl std::fmt::Display for CaptureError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for CaptureError {}

// Fixed preview thumbnail size and RGB8 layout, mirroring the macOS path
// (160x90x3 = ~43KB per poll instead of a full-res BGRA frame).
const PREVIEW_WIDTH: u32 = 160;
const PREVIEW_HEIGHT: u32 = 90;

// Session encode ceiling per axis, from the request resolution. Source
// frames are fit inside preserving aspect ratio (a 5120x1440 ultrawide
// becomes 1920x540 at High), always even for the encoder.
fn encode_ceiling(resolution: VideoResolution) -> (u32, u32) {
    match resolution {
        VideoResolution::P1080 => (1920, 1080),
        VideoResolution::P720 => (1280, 720),
    }
}

fn fit_within(src_width: u32, src_height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    if src_width < 2 || src_height < 2 {
        return (2, 2);
    }
    if src_width <= max_width && src_height <= max_height {
        return ((src_width & !1).max(2), (src_height & !1).max(2));
    }
    let scale = (max_width as f64 / src_width as f64).min(max_height as f64 / src_height as f64);
    let width = ((src_width as f64 * scale) as u32).max(2) & !1;
    let height = ((src_height as f64 * scale) as u32).max(2) & !1;
    (width.max(2), height.max(2))
}

// Nearest-neighbor BGRA downscale. Sizes that already match are still
// resampled (same cost profile, no extra branch in the hot path).
fn downscale_bgra(
    src: &[u8],
    src_width: u32,
    src_height: u32,
    dst_width: u32,
    dst_height: u32,
) -> Option<Vec<u8>> {
    let src_w = src_width as usize;
    let src_h = src_height as usize;
    let dst_w = dst_width as usize;
    let dst_h = dst_height as usize;
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return None;
    }
    let stride = src_w.checked_mul(4)?;
    if src.len() < stride.checked_mul(src_h)? {
        return None;
    }
    let mut dst = vec![0_u8; dst_w.checked_mul(dst_h)?.checked_mul(4)?];
    for y in 0..dst_h {
        let src_y = y * src_h / dst_h;
        for x in 0..dst_w {
            let src_x = x * src_w / dst_w;
            let src_offset = src_y * stride + src_x * 4;
            let dst_offset = (y * dst_w + x) * 4;
            dst[dst_offset..dst_offset + 4].copy_from_slice(&src[src_offset..src_offset + 4]);
        }
    }
    Some(dst)
}

// Small RGB thumbnail derived from a BGRA source frame for the preview
// queue. Same nearest-neighbor sampling the macOS path uses.
fn thumbnail_rgb(src: &[u8], src_width: u32, src_height: u32) -> Option<Vec<u8>> {
    let src_w = src_width as usize;
    let src_h = src_height as usize;
    let dst_w = PREVIEW_WIDTH as usize;
    let dst_h = PREVIEW_HEIGHT as usize;
    if src_w == 0 || src_h == 0 {
        return None;
    }
    let stride = src_w.checked_mul(4)?;
    if src.len() < stride.checked_mul(src_h)? {
        return None;
    }
    let mut dst = vec![0_u8; dst_w * dst_h * 3];
    for y in 0..dst_h {
        let src_y = y * src_h / dst_h;
        for x in 0..dst_w {
            let src_x = x * src_w / dst_w;
            let src_offset = src_y * stride + src_x * 4;
            let dst_offset = (y * dst_w + x) * 3;
            dst[dst_offset] = src[src_offset + 2];
            dst[dst_offset + 1] = src[src_offset + 1];
            dst[dst_offset + 2] = src[src_offset];
        }
    }
    Some(dst)
}

/// The WGC frame handler. Lives on the capture thread inside a
/// `CaptureControl`; forwards every BGRA8 frame to the preview and encoder
/// queues sharing one `Arc<[u8]>` allocation.
struct CaptureHandler {
    capture_tx: SyncSender<NativeFrame>,
    encoder_tx: SyncSender<EncoderCommand>,
    diagnostics: Arc<PreviewDiagnostics>,
    generation: u64,
    target_width: u32,
    target_height: u32,
    min_frame_interval: Duration,
    last_processed: Option<Instant>,
    sequence: AtomicU64,
    scratch: Vec<u8>,
    callbacks: u64,
    paced_out: u64,
    sent_to_encoder: u64,
    encoder_full: u64,
    encoder_gone: u64,
    slow_frames: u64,
}

impl GraphicsCaptureApiHandler for CaptureHandler {
    type Flags = CaptureFlags;
    type Error = CaptureError;

    fn new(context: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            capture_tx: context.flags.capture_tx,
            encoder_tx: context.flags.encoder_tx,
            diagnostics: context.flags.diagnostics,
            generation: context.flags.generation,
            target_width: context.flags.target_width,
            target_height: context.flags.target_height,
            min_frame_interval: context.flags.min_frame_interval,
            last_processed: None,
            sequence: AtomicU64::new(0),
            scratch: Vec::new(),
            callbacks: 0,
            paced_out: 0,
            sent_to_encoder: 0,
            encoder_full: 0,
            encoder_gone: 0,
            slow_frames: 0,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        self.callbacks += 1;
        // Pace BEFORE any expensive work: giant sources (5120x1440 ultrawide
        // is 29MB/frame) cost more per callback than the source frame
        // interval, so unconstrained delivery falls behind forever and the
        // encoder starves while the preview (cheap thumbnails) looks alive.
        // Skipping early keeps a stable rate at any resolution.
        let now = Instant::now();
        if let Some(last) = self.last_processed {
            if now.duration_since(last) < self.min_frame_interval {
                self.paced_out += 1;
                self.maybe_log_summary();
                return Ok(());
            }
        }
        self.last_processed = Some(now);
        let start = Instant::now();
        self.diagnostics
            .callback_count
            .fetch_add(1, Ordering::Relaxed);
        let width = frame.width();
        let height = frame.height();
        if width == 0 || height == 0 {
            return Ok(());
        }
        let timestamp_micros = frame
            .timestamp()
            .ok()
            .and_then(|time| u64::try_from(time.Duration / 10).ok())
            .unwrap_or_else(monotonic_micros);

        // Downscale before the Arc: a 5120x1440 ultrawide frame is 29MB and
        // must never reach the queues (1.7GB/s of garbage at 60fps froze the
        // whole app). The borrowed source pixels are resampled into a
        // session-size BGRA frame plus a small RGB preview thumbnail.
        let (enc_width, enc_height) = fit_within(width, height, self.target_width, self.target_height);
        let mut scratch = std::mem::take(&mut self.scratch);
        let downsized = match frame.buffer() {
            Ok(buffer) => {
                let source = buffer.as_nopadding_buffer(&mut scratch);
                let encoded = downscale_bgra(source, width, height, enc_width, enc_height);
                let thumb = thumbnail_rgb(source, width, height);
                match (encoded, thumb) {
                    (Some(encoded), Some(thumb)) => Some((encoded, thumb)),
                    _ => {
                        self.diagnostics.record_error(
                            "Windows capture downscale failed: undersized frame buffer.",
                        );
                        None
                    }
                }
            }
            Err(error) => {
                self.diagnostics
                    .record_error(format!("Windows capture buffer read failed: {error}"));
                None
            }
        };
        self.scratch = scratch;
        let Some((encoded, thumb)) = downsized else {
            return Ok(());
        };
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;

        // Encoder first: the encoder queue is the latency-critical path.
        match self.encoder_tx.try_send(EncoderCommand::Video(NativeFrame {
            storage: encoded.into(),
            timestamp_micros,
            sequence,
            width: enc_width,
            height: enc_height,
            generation: self.generation,
        })) {
            Ok(()) => {
                self.sent_to_encoder += 1;
            }
            Err(TrySendError::Full(_)) => {
                self.encoder_full += 1;
            }
            Err(TrySendError::Disconnected(_)) => {
                self.encoder_gone += 1;
                if self.encoder_gone == 1 {
                    logger::log(
                        "ERROR",
                        "capture",
                        "encoder queue disconnected; frames will not reach the encoder",
                    );
                }
            }
        }

        let result = self.capture_tx.try_send(NativeFrame {
            storage: thumb.into(),
            timestamp_micros,
            sequence,
            width: PREVIEW_WIDTH,
            height: PREVIEW_HEIGHT,
            generation: self.generation,
        });
        match result {
            Ok(()) => {
                self.diagnostics
                    .frame_count
                    .fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Full(_)) => {
                self.diagnostics
                    .dropped_count
                    .fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {
                self.diagnostics
                    .record_error("Native preview queue is disconnected.");
            }
        }
        let elapsed = start.elapsed();
        if elapsed > Duration::from_millis(25) {
            self.slow_frames += 1;
            logger::log(
                "WARN",
                "capture",
                &format!(
                    "slow frame ({}ms for {}x{} -> {}x{}); pacing protects the rate",
                    elapsed.as_millis(),
                    width,
                    height,
                    enc_width,
                    enc_height
                ),
            );
        }
        self.maybe_log_summary();
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        self.diagnostics
            .record_error("Windows capture source closed; capture ended.");
        Ok(())
    }
}

impl CaptureHandler {
    fn maybe_log_summary(&mut self) {
        if self.callbacks == 1 || self.callbacks % 600 == 0 {
            logger::log(
                "INFO",
                "capture",
                &format!(
                    "frames: callbacks={} paced_out={} sent_to_encoder={} encoder_full={} encoder_gone={} slow={}",
                    self.callbacks,
                    self.paced_out,
                    self.sent_to_encoder,
                    self.encoder_full,
                    self.encoder_gone,
                    self.slow_frames
                ),
            );
        }
    }
}

/// Owns the active WGC capture session. Dropping the adapter stops the capture
/// thread, so a session that is never explicitly stopped cannot leak.
pub(crate) struct WindowsCaptureAdapter {
    capture: Option<CaptureControl<CaptureHandler, CaptureError>>,
}

impl WindowsCaptureAdapter {
    pub(crate) fn new() -> Self {
        Self { capture: None }
    }

    pub(crate) fn start_capture(
        &mut self,
        request: &CreateMediaSessionRequest,
        capture_tx: SyncSender<NativeFrame>,
        encoder_tx: SyncSender<EncoderCommand>,
        diagnostics: Arc<PreviewDiagnostics>,
        generation: u64,
    ) -> Result<(), String> {
        if self.capture.is_some() {
            return Err("Windows capture is already active".into());
        }
        let fps = match request.effective_frame_rate() {
            FrameRate::Fps60 => 60,
            FrameRate::Fps30 => 30,
        };
        let (target_width, target_height) = encode_ceiling(request.resolution);
        let min_frame_interval = Duration::from_micros(1_000_000 / fps as u64);
        logger::log(
            "INFO",
            "capture",
            &format!(
                "start Windows capture (target {target_width}x{target_height}, {fps}fps, min interval {}ms)",
                min_frame_interval.as_millis()
            ),
        );
        let flags = CaptureFlags {
            capture_tx,
            encoder_tx,
            diagnostics,
            generation,
            target_width,
            target_height,
            min_frame_interval,
        };
        let control = match resolve_target(request)? {
            CaptureTarget::Monitor(monitor) => {
                start_free_threaded(monitor, flags, fps)?
            }
            CaptureTarget::Window(window) => start_free_threaded(window, flags, fps)?,
        };
        eprintln!(
            "[goDrinking] starting Windows Graphics Capture source={:?} source_id={:?}",
            request.source, request.source_id
        );
        self.capture = Some(control);
        Ok(())
    }

    pub(crate) fn stop_capture(&mut self) -> Result<(), String> {
        if let Some(control) = self.capture.take() {
            control
                .stop()
                .map_err(|error| format!("Windows capture stop failed: {error}"))?;
        }
        Ok(())
    }

    pub(crate) fn enumerate_sources() -> Result<Vec<NativeCaptureSource>, String> {
        let mut sources = Vec::new();
        let monitors = Monitor::enumerate()
            .map_err(|error| format!("Windows monitor enumeration failed: {error}"))?;
        for (index, monitor) in monitors.iter().enumerate() {
            let width = monitor.width().ok().map(u64::from);
            let height = monitor.height().ok().map(u64::from);
            let title = monitor
                .name()
                .ok()
                .or_else(|| monitor.device_string().ok());
            sources.push(NativeCaptureSource {
                id: (index + 1) as u64,
                kind: NativeSourceKind::Display,
                title,
                application_name: None,
                width,
                height,
            });
        }
        if let Ok(windows) = Window::enumerate() {
            for window in windows {
                let width = window.width().ok().map(|value| value.max(0) as u64);
                let height = window.height().ok().map(|value| value.max(0) as u64);
                sources.push(NativeCaptureSource {
                    id: window.as_raw_hwnd() as u64,
                    kind: NativeSourceKind::Window,
                    title: window.title().ok().filter(|title| !title.is_empty()),
                    application_name: window.process_name().ok(),
                    width,
                    height,
                });
            }
        }
        Ok(sources)
    }

    pub(crate) fn enumerate_running_apps() -> Result<Vec<NativeRunningApp>, String> {
        let mut apps = Vec::new();
        if let Ok(windows) = Window::enumerate() {
            for window in windows {
                let Ok(pid) = window.process_id() else {
                    continue;
                };
                let name = window
                    .process_name()
                    .ok()
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| format!("pid {pid}"));
                apps.push(NativeRunningApp {
                    name,
                    bundle_id: None,
                    pid: pid as i32,
                    emitting_audio: false,
                });
            }
        }
        apps.sort_by(|left, right| left.name.to_ascii_lowercase().cmp(&right.name.to_ascii_lowercase()));
        apps.dedup_by(|left, right| left.pid == right.pid);
        Ok(apps)
    }
}

impl Drop for WindowsCaptureAdapter {
    fn drop(&mut self) {
        if let Some(control) = self.capture.take() {
            let _ = control.stop();
        }
    }
}

impl Default for WindowsCaptureAdapter {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolves the session request to a concrete capture target. Windows capture
/// falls back to the primary monitor when a requested window is gone.
fn resolve_target(request: &CreateMediaSessionRequest) -> Result<CaptureTarget, String> {
    match request.source {
        CaptureSource::Window => {
            if let Some(id) = request.source_id {
                if let Ok(windows) = Window::enumerate() {
                    if let Some(window) = windows
                        .into_iter()
                        .find(|window| window.as_raw_hwnd() as u64 == id)
                    {
                        return Ok(CaptureTarget::Window(window));
                    }
                }
            }
            let monitor = Monitor::primary()
                .map_err(|error| format!("no primary monitor available: {error}"))?;
            Ok(CaptureTarget::Monitor(monitor))
        }
        CaptureSource::Screen => {
            let monitor = match request.source_id {
                Some(index) if index > 0 => Monitor::from_index(index as usize)
                    .map_err(|error| format!("monitor {index} not found: {error}"))?,
                _ => Monitor::primary()
                    .map_err(|error| format!("no primary monitor available: {error}"))?,
            };
            Ok(CaptureTarget::Monitor(monitor))
        }
        CaptureSource::Game => Err("game capture is unsupported on Windows".into()),
    }
}

fn start_free_threaded<T>(
    item: T,
    flags: CaptureFlags,
    fps: u32,
) -> Result<CaptureControl<CaptureHandler, CaptureError>, String>
where
    T: TryInto<GraphicsCaptureItemType> + Send + 'static,
{
    let settings = Settings::new(
        item,
        CursorCaptureSettings::Default,
        DrawBorderSettings::Default,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Custom(Duration::from_micros(1_000_000 / u64::from(fps))),
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        flags,
    );
    CaptureHandler::start_free_threaded(settings)
        .map_err(|error| format!("Windows Graphics Capture start failed: {error}"))
}

fn monotonic_micros() -> u64 {
    use std::time::Instant;
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(Instant::now)
        .elapsed()
        .as_micros()
        .min(u64::MAX as u128) as u64
}
