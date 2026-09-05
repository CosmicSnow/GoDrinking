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
    NativeSourceKind, VideoCodec, VideoResolution,
};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows_capture::capture::{
    CaptureControl, Context, GraphicsCaptureApiError, GraphicsCaptureApiHandler,
};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::{GraphicsCaptureApi, InternalCaptureControl};
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    GraphicsCaptureItemType, MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

/// Cancellation shared by native Start and Stop callers.
#[derive(Clone, Debug, Default)]
pub struct CaptureCancellationToken(Arc<AtomicBool>);

impl CaptureCancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_atomic(flag: Arc<AtomicBool>) -> Self {
        Self(flag)
    }

    pub fn cancel(&self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Acquire)
    }
}

impl From<Arc<AtomicBool>> for CaptureCancellationToken {
    fn from(flag: Arc<AtomicBool>) -> Self {
        Self::from_atomic(flag)
    }
}

pub type StartCancellationToken = CaptureCancellationToken;

pub(crate) type CaptureShutdownStatus = super::process_tap::ShutdownStatus;

fn shutdown_quiescent() -> CaptureShutdownStatus {
    CaptureShutdownStatus {
        quiesced: true,
        pending: Vec::new(),
        errors: Vec::new(),
    }
}

fn shutdown_pending(component: &'static str) -> CaptureShutdownStatus {
    CaptureShutdownStatus {
        quiesced: false,
        pending: vec![component],
        errors: Vec::new(),
    }
}

fn shutdown_error(error: impl Into<String>) -> CaptureShutdownStatus {
    CaptureShutdownStatus {
        quiesced: false,
        pending: Vec::new(),
        errors: vec![error.into()],
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureLifecycle {
    Idle,
    Starting,
    Running,
    Stopping,
    Failed,
    CleanupPending,
}

/// A capture source resolved from a session request.
enum CaptureTarget {
    Monitor(Monitor),
    Window(Window),
}

/// Handler flags carried through `Settings` into `CaptureHandler::new`.
#[derive(Clone)]
struct CaptureFlags {
    capture_tx: SyncSender<NativeFrame>,
    encoder_tx: SyncSender<EncoderCommand>,
    diagnostics: Arc<PreviewDiagnostics>,
    generation: u64,
    target_width: u32,
    target_height: u32,
    baseline_cap: bool,
    min_frame_interval: Duration,
    cancellation: CaptureCancellationToken,
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
// frames are fit to the final encode size (Baseline additionally capped
// to 1920 wide, everything macroblock-aligned), so the encoder queue
// never re-scales: a 5120x1440 ultrawide arrives as 1920x528 Baseline.
fn encode_ceiling(resolution: VideoResolution) -> (u32, u32) {
    match resolution {
        VideoResolution::P2160 => (3840, 2160),
        VideoResolution::P1440 => (2560, 1440),
        VideoResolution::P1080 => (1920, 1080),
        VideoResolution::P720 => (1280, 720),
        VideoResolution::P480 => (854, 480),
    }
}

fn fit_within(
    src_width: u32,
    src_height: u32,
    max_width: u32,
    max_height: u32,
    baseline: bool,
) -> (u32, u32) {
    crate::media::types::final_encode_size(src_width, src_height, max_width, max_height, baseline)
}

// Nearest-neighbor BGRA downscale over a possibly-padded source (row
// stride in bytes, zero-copy friendly). Fixed-point stepping: no division
// in the hot loop. A 5120x1440 -> 1920x540 resample with per-pixel
// division cost ~50ms here and starved the encoder on ultrawide.
fn downscale_bgra(
    src: &[u8],
    src_stride: usize,
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
    let row_bytes = src_w.checked_mul(4)?;
    if src_stride < row_bytes {
        return None;
    }
    if src.len()
        < src_stride
            .checked_mul(src_h.saturating_sub(1))?
            .checked_add(row_bytes)?
    {
        return None;
    }
    let mut dst = vec![0_u8; dst_w.checked_mul(dst_h)?.checked_mul(4)?];
    let x_step = ((src_w as u64) << 16) / dst_w as u64;
    let y_step = ((src_h as u64) << 16) / dst_h as u64;
    let mut src_y_fp = 0u64;
    for y in 0..dst_h {
        let src_y = (src_y_fp >> 16) as usize;
        src_y_fp += y_step;
        let src_row_start = src_y * src_stride;
        let dst_row_start = y * dst_w * 4;
        let mut src_x_fp = 0u64;
        for x in 0..dst_w {
            let src_x = (src_x_fp >> 16) as usize;
            src_x_fp += x_step;
            let src_offset = src_row_start + src_x * 4;
            let dst_offset = dst_row_start + x * 4;
            dst[dst_offset..dst_offset + 4].copy_from_slice(&src[src_offset..src_offset + 4]);
        }
    }
    Some(dst)
}

// Small RGB thumbnail derived from a BGRA source frame for the preview
// queue. Same nearest-neighbor sampling the macOS path uses. Host
// convenience only: this RGB thumbnail is never proof of viewer color
// correctness (the canonical path is NV12 BT.709 limited into the encoder).
fn thumbnail_rgb(
    src: &[u8],
    src_stride: usize,
    src_width: u32,
    src_height: u32,
) -> Option<Vec<u8>> {
    let src_w = src_width as usize;
    let src_h = src_height as usize;
    let dst_w = PREVIEW_WIDTH as usize;
    let dst_h = PREVIEW_HEIGHT as usize;
    if src_w == 0 || src_h == 0 {
        return None;
    }
    let row_bytes = src_w.checked_mul(4)?;
    if src_stride < row_bytes {
        return None;
    }
    if src.len()
        < src_stride
            .checked_mul(src_h.saturating_sub(1))?
            .checked_add(row_bytes)?
    {
        return None;
    }
    let mut dst = vec![0_u8; dst_w * dst_h * 3];
    let x_step = ((src_w as u64) << 16) / dst_w as u64;
    let y_step = ((src_h as u64) << 16) / dst_h as u64;
    let mut src_y_fp = 0u64;
    for y in 0..dst_h {
        let src_y = (src_y_fp >> 16) as usize;
        src_y_fp += y_step;
        let src_row_start = src_y * src_stride;
        let dst_row_start = y * dst_w * 3;
        let mut src_x_fp = 0u64;
        for x in 0..dst_w {
            let src_x = (src_x_fp >> 16) as usize;
            src_x_fp += x_step;
            let src_offset = src_row_start + src_x * 4;
            let dst_offset = dst_row_start + x * 3;
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
    baseline_cap: bool,
    min_frame_interval: Duration,
    cancellation: CaptureCancellationToken,
    last_processed: Option<Instant>,
    sequence: AtomicU64,
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
            baseline_cap: context.flags.baseline_cap,
            min_frame_interval: context.flags.min_frame_interval,
            cancellation: context.flags.cancellation,
            last_processed: None,
            sequence: AtomicU64::new(0),
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
        if self.cancellation.is_cancelled() {
            _capture_control.stop();
            return Ok(());
        }
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
        // whole app). The mapped source pixels are resampled in place into a
        // session-size BGRA frame plus a small RGB preview thumbnail, with
        // no full-frame copy (stride-aware, zero-copy even with padding).
        let (enc_width, enc_height) = fit_within(
            width,
            height,
            self.target_width,
            self.target_height,
            self.baseline_cap,
        );
        let t_buf_start = Instant::now();
        let mut fb = match frame.buffer() {
            Ok(fb) => fb,
            Err(error) => {
                self.diagnostics
                    .record_error(format!("Windows capture buffer read failed: {error}"));
                return Ok(());
            }
        };
        let buf_ms = t_buf_start.elapsed().as_millis();
        let src_stride = fb.row_pitch() as usize;
        let t_conv_start = Instant::now();
        let source = fb.as_raw_buffer();
        let encoded = downscale_bgra(source, src_stride, width, height, enc_width, enc_height);
        let thumb = thumbnail_rgb(source, src_stride, width, height);
        let conv_ms = t_conv_start.elapsed().as_millis();
        let downsized = match (encoded, thumb) {
            (Some(encoded), Some(thumb)) => Some((encoded, thumb)),
            _ => {
                self.diagnostics
                    .record_error("Windows capture downscale failed: undersized frame buffer.");
                None
            }
        };
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
                self.diagnostics.frame_count.fetch_add(1, Ordering::Relaxed);
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
        // Threshold at 100ms: ultrawide frames cost ~50ms here, which is
        // expected (not spam-worthy); only real stalls get a line, the rest
        // is counted in `slow` for the periodic summary.
        if elapsed > Duration::from_millis(100) {
            self.slow_frames += 1;
            logger::log(
                "WARN",
                "capture",
                &format!(
                    "slow frame ({}ms = buf {}ms + conv {}ms for {}x{} -> {}x{}); pacing protects the rate",
                    elapsed.as_millis(),
                    buf_ms,
                    conv_ms,
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
    capture: Option<WindowsCaptureWorker>,
    lifecycle: CaptureLifecycle,
}

type CaptureThreadResult = Result<(), GraphicsCaptureApiError<CaptureError>>;

/// Owns both halves of the windows-capture worker. The upstream
/// `CaptureControl::stop` consumes its control before reporting a posting
/// error, so retaining these handles here lets us report Pending and retry
/// without detaching a COM-affine thread.
struct WindowsCaptureWorker {
    thread: Option<JoinHandle<CaptureThreadResult>>,
    halt: Arc<AtomicBool>,
}

impl WindowsCaptureWorker {
    fn from_control(control: CaptureControl<CaptureHandler, CaptureError>) -> Self {
        let halt = control.halt_handle();
        let thread = control.into_thread_handle();
        Self {
            thread: Some(thread),
            halt,
        }
    }

    fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }

    fn request_stop(&self) -> Result<(), String> {
        use std::os::windows::prelude::AsRawHandle;
        use windows::Win32::Foundation::{ERROR_INVALID_THREAD_ID, HANDLE, LPARAM, WPARAM};
        use windows::Win32::System::Threading::GetThreadId;
        use windows::Win32::UI::WindowsAndMessaging::{PostThreadMessageW, WM_QUIT};

        self.halt.store(true, std::sync::atomic::Ordering::Release);
        let Some(thread) = self.thread.as_ref() else {
            return Ok(());
        };
        if thread.is_finished() {
            return Ok(());
        }
        let thread_id = unsafe { GetThreadId(HANDLE(thread.as_raw_handle())) };
        if thread_id == 0 {
            return Err("Windows capture worker thread id is unavailable".into());
        }
        loop {
            match unsafe {
                PostThreadMessageW(thread_id, WM_QUIT, WPARAM::default(), LPARAM::default())
            } {
                Ok(()) => return Ok(()),
                Err(error)
                    if error.code()
                        == windows::core::HRESULT::from_win32(ERROR_INVALID_THREAD_ID.0) =>
                {
                    if thread.is_finished() {
                        return Ok(());
                    }
                    thread::yield_now();
                }
                Err(error) => return Err(format!("failed to post worker shutdown: {error}")),
            }
        }
    }

    fn wait(&mut self, timeout: Duration) -> CaptureShutdownStatus {
        let deadline = Instant::now() + timeout;
        while !self.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        if !self.is_finished() {
            return shutdown_pending("windows capture worker");
        }
        let Some(thread) = self.thread.take() else {
            return shutdown_quiescent();
        };
        match thread.join() {
            Ok(Ok(())) => shutdown_quiescent(),
            Ok(Err(error)) => shutdown_error(error.to_string()),
            Err(_) => shutdown_error("Windows capture worker panicked"),
        }
    }
}

impl Drop for WindowsCaptureWorker {
    fn drop(&mut self) {
        if self.thread.is_some() {
            let _ = self.request_stop();
            // A worker owns WinRT/COM objects and must not be detached.
            let _ = self.wait(Duration::from_secs(10));
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }
}

impl WindowsCaptureAdapter {
    pub(crate) fn new() -> Self {
        Self {
            capture: None,
            lifecycle: CaptureLifecycle::Idle,
        }
    }

    pub(crate) fn lifecycle(&self) -> CaptureLifecycle {
        self.lifecycle
    }

    pub(crate) fn start_capture(
        &mut self,
        request: &CreateMediaSessionRequest,
        capture_tx: SyncSender<NativeFrame>,
        encoder_tx: SyncSender<EncoderCommand>,
        diagnostics: Arc<PreviewDiagnostics>,
        generation: u64,
    ) -> Result<(), String> {
        self.start_capture_with_cancellation(
            request,
            capture_tx,
            encoder_tx,
            diagnostics,
            generation,
            CaptureCancellationToken::new(),
        )
    }

    pub(crate) fn start_capture_with_cancellation(
        &mut self,
        request: &CreateMediaSessionRequest,
        capture_tx: SyncSender<NativeFrame>,
        encoder_tx: SyncSender<EncoderCommand>,
        diagnostics: Arc<PreviewDiagnostics>,
        generation: u64,
        cancellation: CaptureCancellationToken,
    ) -> Result<(), String> {
        if self.capture.is_some() {
            return Err("Windows capture is already active".into());
        }
        if cancellation.is_cancelled() {
            self.lifecycle = CaptureLifecycle::Idle;
            return Err("Windows capture start was cancelled".into());
        }
        // 60 fps envelope, rejected before acquisition (the Share slot never
        // starts, so no restart semantics are involved).
        let fps = request.effective_frame_rate().hertz();
        if !super::pipeline::fps_within_envelope(fps) {
            self.lifecycle = CaptureLifecycle::Idle;
            return Err(format!(
                "frame rate {fps} fps exceeds the 60 fps product envelope"
            ));
        }
        self.lifecycle = CaptureLifecycle::Starting;
        let (target_width, target_height) = encode_ceiling(request.resolution);
        // Baseline sessions encode at most 1920 wide: fit straight to the
        // final size here so the encoder never re-scales (and the MFT gets
        // macroblock-aligned input it accepts).
        let baseline_cap = matches!(request.codec, VideoCodec::H264);
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
            baseline_cap,
            min_frame_interval,
            cancellation: cancellation.clone(),
        };
        let control = match resolve_target(request).and_then(|target| {
            if cancellation.is_cancelled() {
                return Err("Windows capture start was cancelled".into());
            }
            match target {
                CaptureTarget::Monitor(monitor) => start_free_threaded(monitor, flags, fps),
                CaptureTarget::Window(window) => start_free_threaded(window, flags, fps),
            }
        }) {
            Ok(control) => control,
            Err(error) => {
                self.lifecycle = CaptureLifecycle::Idle;
                return Err(error);
            }
        };
        let worker = WindowsCaptureWorker::from_control(control);
        if cancellation.is_cancelled() {
            self.capture = Some(worker);
            let _ = self.stop_capture_with_timeout(Duration::from_secs(10));
            return Err("Windows capture start was cancelled".into());
        }
        eprintln!(
            "[goDrinking] starting Windows Graphics Capture source={:?} source_id={:?}",
            request.source, request.source_id
        );
        self.capture = Some(worker);
        self.lifecycle = CaptureLifecycle::Running;
        Ok(())
    }

    pub(crate) fn stop_capture(&mut self) -> Result<(), String> {
        let status = self.stop_capture_with_timeout(Duration::from_secs(10));
        if status.quiesced {
            Ok(())
        } else if status.errors.is_empty() {
            Err("Windows capture stop is pending".into())
        } else {
            Err(format!(
                "Windows capture stop failed: {}",
                status.errors.join("; ")
            ))
        }
    }

    pub(crate) fn stop_capture_with_timeout(&mut self, timeout: Duration) -> CaptureShutdownStatus {
        let Some(worker) = self.capture.as_mut() else {
            self.lifecycle = CaptureLifecycle::Idle;
            return shutdown_quiescent();
        };
        self.lifecycle = CaptureLifecycle::Stopping;
        if let Err(error) = worker.request_stop() {
            self.lifecycle = if worker.is_finished() {
                CaptureLifecycle::Failed
            } else {
                CaptureLifecycle::CleanupPending
            };
            return shutdown_error(error);
        }
        let result = worker.wait(timeout);
        match &result {
            status if status.quiesced => {
                self.capture = None;
                self.lifecycle = CaptureLifecycle::Idle;
            }
            status if status.errors.is_empty() => self.lifecycle = CaptureLifecycle::CleanupPending,
            _ => self.lifecycle = CaptureLifecycle::Failed,
        }
        result
    }

    pub(crate) fn shutdown(&mut self) -> CaptureShutdownStatus {
        self.shutdown_with_timeout(Duration::from_secs(10))
    }

    pub(crate) fn shutdown_with_timeout(&mut self, timeout: Duration) -> CaptureShutdownStatus {
        self.stop_capture_with_timeout(timeout)
    }

    pub(crate) fn enumerate_sources() -> Result<Vec<NativeCaptureSource>, String> {
        let mut sources = Vec::new();
        let monitors = Monitor::enumerate()
            .map_err(|error| format!("Windows monitor enumeration failed: {error}"))?;
        for (index, monitor) in monitors.iter().enumerate() {
            let width = monitor.width().ok().map(u64::from);
            let height = monitor.height().ok().map(u64::from);
            let title = monitor.name().ok().or_else(|| monitor.device_string().ok());
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
        apps.sort_by(|left, right| {
            left.name
                .to_ascii_lowercase()
                .cmp(&right.name.to_ascii_lowercase())
        });
        apps.dedup_by(|left, right| left.pid == right.pid);
        Ok(apps)
    }
}

impl Drop for WindowsCaptureAdapter {
    fn drop(&mut self) {
        let _ = self.shutdown();
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
    T: TryInto<GraphicsCaptureItemType> + Clone + Send + 'static,
{
    let custom_interval = Duration::from_micros(1_000_000 / u64::from(fps.max(1)));
    // Older Windows 10 builds implement WGC but not the MinUpdateInterval
    // property, so probing first avoids a doomed `Custom` attempt there.
    let custom_supported =
        GraphicsCaptureApi::is_minimum_update_interval_supported().unwrap_or(true);
    // The system draws a colored border around the captured item; users find
    // it ugly, so turn it off where the OS allows (same probe-and-fallback).
    let borderless_supported = GraphicsCaptureApi::is_border_settings_supported().unwrap_or(true);
    if !borderless_supported {
        logger::log(
            "WARN",
            "capture",
            "capture border toggle unsupported on this Windows build, keeping the system default border",
        );
    }

    let build_settings = |item: T, flags: CaptureFlags, custom: bool, borderless: bool| {
        Settings::new(
            item,
            CursorCaptureSettings::Default,
            if borderless {
                DrawBorderSettings::WithoutBorder
            } else {
                DrawBorderSettings::Default
            },
            SecondaryWindowSettings::Default,
            if custom {
                MinimumUpdateIntervalSettings::Custom(custom_interval)
            } else {
                MinimumUpdateIntervalSettings::Default
            },
            DirtyRegionSettings::Default,
            ColorFormat::Bgra8,
            flags,
        )
    };

    if !custom_supported {
        logger::log(
            "WARN",
            "capture",
            "minimum update interval unsupported on this Windows build, starting with Default interval",
        );
        let fallback = build_settings(item, flags, false, borderless_supported);
        return CaptureHandler::start_free_threaded(fallback)
            .map_err(|error| format!("Windows Graphics Capture start failed: {error}"));
    }

    let settings = build_settings(
        item.clone(),
        flags.clone(),
        custom_supported,
        borderless_supported,
    );
    match CaptureHandler::start_free_threaded(settings) {
        Ok(control) => Ok(control),
        Err(error) => {
            let message = error.to_string();
            let interval_fallback = custom_supported && is_min_interval_unsupported(&message);
            let border_fallback = borderless_supported && message.to_lowercase().contains("border");
            if interval_fallback || border_fallback {
                logger::log(
                    "WARN",
                    "capture",
                    &format!(
                        "capture setting rejected by this Windows build, retrying with Default: {message}"
                    ),
                );
                let fallback = build_settings(
                    item,
                    flags,
                    custom_supported && !interval_fallback,
                    borderless_supported && !border_fallback,
                );
                CaptureHandler::start_free_threaded(fallback)
                    .map_err(|error| format!("Windows Graphics Capture start failed: {error}"))
            } else {
                Err(format!("Windows Graphics Capture start failed: {message}"))
            }
        }
    }
}

/// Matches the `windows-capture` `MinimumUpdateIntervalUnsupported` error
/// without depending on its exact enum shape at the call site.
fn is_min_interval_unsupported(message: &str) -> bool {
    message.to_lowercase().contains("minimum update interval")
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

#[cfg(test)]
mod tests {
    use super::{fit_within, CaptureCancellationToken, CaptureLifecycle};

    #[test]
    fn encode_fit_applies_baseline_cap_and_preserves_aspect() {
        // 5120x1440 ultrawide arrives as 1920x528 Baseline, never 16:9.
        assert_eq!(fit_within(5120, 1440, 1920, 1080, true), (1920, 528));
        let (w, h) = fit_within(3440, 1440, 1920, 1080, true);
        assert_eq!(w, 1920);
        assert_eq!((w % 16, h % 16), (0, 0));
        assert!(((w as f64) / (h as f64) - 3440.0 / 1440.0).abs() < 0.03);
        // Non-Baseline keeps the wider pixel-budget fit.
        let (w, h) = fit_within(5120, 1440, 1920, 1080, false);
        assert!(w * h <= 1920 * 1080);
        assert_eq!((w % 16, h % 16), (0, 0));
        // Broadcast sizes pass through untouched.
        assert_eq!(fit_within(1920, 1080, 1920, 1080, true), (1920, 1080));
        assert_eq!(fit_within(1280, 720, 1920, 1080, true), (1280, 720));
    }

    #[test]
    fn sixty_fps_envelope_is_checked_before_acquisition() {
        use super::super::pipeline::fps_within_envelope;
        assert!(fps_within_envelope(30));
        assert!(fps_within_envelope(60));
        assert!(!fps_within_envelope(120));
    }

    #[test]
    fn start_cancellation_token_is_observed_by_all_clones() {
        let token = CaptureCancellationToken::new();
        let clone = token.clone();
        assert!(!clone.is_cancelled());
        token.cancel();
        assert!(clone.is_cancelled());
    }

    #[test]
    fn shutdown_status_distinguishes_quiescent_pending_and_error() {
        let quiescent = super::shutdown_quiescent();
        assert!(quiescent.quiesced);
        let pending = super::shutdown_pending("worker");
        assert!(!pending.quiesced);
        assert_eq!(pending.pending, vec!["worker"]);
        let error = super::shutdown_error("worker failure");
        assert!(!error.quiesced);
        assert_eq!(error.errors, vec!["worker failure"]);
        assert_eq!(
            CaptureLifecycle::CleanupPending,
            CaptureLifecycle::CleanupPending
        );
    }
}
