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

use super::pipeline::{EncoderCommand, NativeFrame, PreviewDiagnostics};
use super::types::{
    CaptureSource, CreateMediaSessionRequest, FrameRate, NativeCaptureSource, NativeRunningApp,
    NativeSourceKind,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::Arc;
use std::time::Duration;

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

/// The WGC frame handler. Lives on the capture thread inside a
/// `CaptureControl`; forwards every BGRA8 frame to the preview and encoder
/// queues sharing one `Arc<[u8]>` allocation.
struct CaptureHandler {
    capture_tx: SyncSender<NativeFrame>,
    encoder_tx: SyncSender<EncoderCommand>,
    diagnostics: Arc<PreviewDiagnostics>,
    generation: u64,
    sequence: AtomicU64,
    scratch: Vec<u8>,
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
            sequence: AtomicU64::new(0),
            scratch: Vec::new(),
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
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

        let mut scratch = std::mem::take(&mut self.scratch);
        let bytes = match frame.buffer() {
            Ok(buffer) => buffer.as_nopadding_buffer(&mut scratch).to_vec(),
            Err(error) => {
                self.diagnostics
                    .record_error(format!("Windows capture buffer read failed: {error}"));
                self.scratch = scratch;
                return Ok(());
            }
        };
        self.scratch = scratch;
        let storage: Arc<[u8]> = bytes.into();
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;

        // Encoder first: the encoder queue is the latency-critical path.
        let _ = self.encoder_tx.try_send(EncoderCommand::Video(NativeFrame {
            storage: Arc::clone(&storage),
            timestamp_micros,
            sequence,
            width,
            height,
            generation: self.generation,
        }));

        let result = self.capture_tx.try_send(NativeFrame {
            storage,
            timestamp_micros,
            sequence,
            width,
            height,
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
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        self.diagnostics
            .record_error("Windows capture source closed; capture ended.");
        Ok(())
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
        let flags = CaptureFlags {
            capture_tx,
            encoder_tx,
            diagnostics,
            generation,
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
