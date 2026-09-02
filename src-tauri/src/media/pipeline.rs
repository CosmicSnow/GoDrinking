//! Bounded native media pipeline primitives.
//!
//! A platform capture adapter will produce `NativeFrame` values into the
//! bounded capture queue. The frame storage is reference counted so an
//! encoder can retain the same allocation without copying it. These private
//! types deliberately have no serde implementation: raw frames must never be
//! sent through Tauri IPC. The capture queue currently carries only bounded
//! derived preview thumbnails; full source frames remain native.

use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::access_unit::{AccessUnitQueue, AccessUnitReceiver};
use super::types::PreviewFrameEvent;
use super::types::{FrameRate, TransmissionQuality, VideoResolution};

#[cfg(target_os = "macos")]
use objc2::rc::Retained;
#[cfg(target_os = "macos")]
use objc2_core_video::CVPixelBuffer;

/// Pause after a real rate decrease before probing upward again.
const PROBE_QUIET: Duration = Duration::from_secs(3);
/// Recent RTCP loss freezes probing (REMB decreases still apply).
const LOSS_WINDOW: Duration = Duration::from_secs(5);
/// fraction-lost (0-255) above ~2% counts as real congestion for probing.
const LOSS_FREEZE_FRACTION: u8 = 5;

const CAPTURE_QUEUE_CAPACITY: usize = 3;
const ENCODER_QUEUE_CAPACITY: usize = 8;
const ACCESS_UNIT_QUEUE_CAPACITY: usize = 16;
const WORKER_COMPLETION_TIMEOUT: Duration = Duration::from_secs(3);

pub(crate) struct PipelineState {
    pub(crate) failed: AtomicBool,
    pub(crate) failure: Mutex<Option<String>>,
    pub(crate) accepting_callbacks: AtomicBool,
}

struct WorkerCompletion {
    done: Mutex<bool>,
    wake: Condvar,
}

impl PipelineState {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            failed: AtomicBool::new(false),
            failure: Mutex::new(None),
            accepting_callbacks: AtomicBool::new(true),
        })
    }

    pub(crate) fn fail(&self, error: impl Into<String>) {
        self.failed.store(true, Ordering::Release);
        if let Ok(mut failure) = self.failure.lock() {
            if failure.is_none() {
                *failure = Some(error.into());
            }
        }
    }

    pub(crate) fn is_failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    pub(crate) fn failure(&self) -> Option<String> {
        self.failure.lock().ok().and_then(|failure| failure.clone())
    }
}

#[allow(dead_code)]
pub(crate) struct NativeFrame {
    pub(crate) storage: Arc<[u8]>,
    pub(crate) timestamp_micros: u64,
    pub(crate) sequence: u64,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) generation: u64,
}

#[cfg(target_os = "macos")]
pub(crate) struct NativeEncoderFrame {
    pub(crate) pixel_buffer: Retained<CVPixelBuffer>,
    pub(crate) timestamp_micros: u64,
    pub(crate) generation: u64,
}

// Core Video buffers are reference-counted native objects. Ownership is moved
// exactly once from the capture callback to the dedicated encoder worker.
#[cfg(target_os = "macos")]
unsafe impl Send for NativeEncoderFrame {}

pub(crate) struct PreviewState {
    pub(crate) latest: Mutex<Option<PreviewFrameEvent>>,
    pub(crate) generation: AtomicU64,
    pub(crate) diagnostics: Arc<PreviewDiagnostics>,
}

#[derive(Debug)]
pub(crate) struct PreviewDiagnostics {
    pub(crate) callback_count: AtomicU64,
    pub(crate) frame_count: AtomicU64,
    pub(crate) dropped_count: AtomicU64,
    pub(crate) error: Mutex<Option<String>>,
}

impl PreviewDiagnostics {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            callback_count: AtomicU64::new(0),
            frame_count: AtomicU64::new(0),
            dropped_count: AtomicU64::new(0),
            error: Mutex::new(None),
        })
    }

    pub(crate) fn record_error(&self, error: impl Into<String>) {
        if let Ok(mut detail) = self.error.lock() {
            if detail.is_none() {
                *detail = Some(error.into());
            }
        }
    }

    pub(crate) fn error(&self) -> Option<String> {
        self.error.lock().ok().and_then(|detail| detail.clone())
    }

    fn reset(&self) {
        self.callback_count.store(0, Ordering::Release);
        self.frame_count.store(0, Ordering::Release);
        self.dropped_count.store(0, Ordering::Release);
        if let Ok(mut detail) = self.error.lock() {
            *detail = None;
        }
    }
}

impl PreviewState {
    pub(crate) fn new() -> Self {
        Self {
            latest: Mutex::new(None),
            generation: AtomicU64::new(0),
            diagnostics: PreviewDiagnostics::new(),
        }
    }

    pub(crate) fn begin_session(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.diagnostics.reset();
        if let Ok(mut latest) = self.latest.lock() {
            *latest = None;
        }
    }
}

#[allow(dead_code)]
pub(crate) enum EncoderCommand {
    #[cfg(target_os = "macos")]
    Video(NativeEncoderFrame),
    #[cfg(not(target_os = "macos"))]
    Video(NativeFrame),
    Flush,
    ForceKeyframe,
    SetBitrate(u32),
    Stop,
}

/// Coalesced encoder controls. Feedback bursts collapse to one keyframe
/// request and the newest bitrate, rather than filling a priority queue.
pub(crate) struct EncoderControl {
    keyframe_requested: AtomicBool,
    bitrate: Mutex<Option<u32>>,
    /// User/preset intent: the ceiling congestion control may not exceed.
    target: Mutex<u32>,
    /// Congestion floor: REMB is never followed below this.
    floor: Mutex<u32>,
    /// Last bitrate imposed by the adaptation loop (REMB or probe).
    /// None = no signal yet, encoder follows the target. Surfaced for
    /// diagnostics.
    congestion: Mutex<Option<u32>>,
    /// Last rate queued to the encoder (target, REMB or probe).
    applied: Mutex<u32>,
    /// When the adaptation loop last *lowered* the rate. Probing pauses
    /// briefly after a real decrease so it never fights fresh feedback.
    last_decrease_at: Mutex<Option<Instant>>,
    /// Worst fraction-lost (0-255) seen in RTCP Receiver Reports + when.
    /// Any recent loss freezes probing; REMB decreases still apply.
    last_loss: Mutex<(u8, Option<Instant>)>,
    stop_requested: AtomicBool,
}

impl EncoderControl {
    pub(crate) fn new(target: u32, floor: u32) -> Arc<Self> {
        Arc::new(Self {
            keyframe_requested: AtomicBool::new(false),
            bitrate: Mutex::new(None),
            target: Mutex::new(target),
            floor: Mutex::new(floor.min(target)),
            congestion: Mutex::new(None),
            applied: Mutex::new(target),
            last_decrease_at: Mutex::new(None),
            last_loss: Mutex::new((0, None)),
            stop_requested: AtomicBool::new(false),
        })
    }

    pub(crate) fn request_keyframe(&self) {
        self.keyframe_requested.store(true, Ordering::Release);
    }

    /// Explicit intent (preset change or user override): moves the target
    /// ceiling and applies it to the encoder.
    pub(crate) fn set_bitrate(&self, bitrate: u32) {
        if let Ok(mut target) = self.target.lock() {
            *target = bitrate;
        }
        if let Ok(mut floor) = self.floor.lock() {
            *floor = (*floor).min(bitrate);
        }
        if let Ok(mut congestion) = self.congestion.lock() {
            *congestion = None;
        }
        if let Ok(mut applied) = self.applied.lock() {
            *applied = bitrate;
        }
        if let Ok(mut pending) = self.bitrate.lock() {
            *pending = Some(bitrate);
        }
    }

    /// Live floor update: re-asserts the encoder at the raised floor so a
    /// previously collapsed stream recovers without waiting for REMB.
    pub(crate) fn set_floor(&self, floor: u32) {
        let (floor, target, current) = (
            floor,
            self.target(),
            self.congestion_bitrate(),
        );
        let floor = floor.min(target);
        if let Ok(mut slot) = self.floor.lock() {
            *slot = floor;
        }
        let reassert = current.unwrap_or(target).clamp(floor, target);
        if let Ok(mut applied) = self.applied.lock() {
            *applied = reassert;
        }
        if let Ok(mut pending) = self.bitrate.lock() {
            *pending = Some(reassert);
        }
    }

    /// Congestion feedback (REMB): may lower the encoder below the target
    /// but never raise it above what the user/preset asked for.
    pub(crate) fn set_congestion_bitrate(&self, bitrate: u32) {
        let ceiling = self.target().max(250_000);
        let floor = self.floor().min(ceiling);
        let bitrate = bitrate.clamp(floor, ceiling);
        let decreased = bitrate < self.applied();
        if let Ok(mut congestion) = self.congestion.lock() {
            *congestion = Some(bitrate);
        }
        if let Ok(mut applied) = self.applied.lock() {
            *applied = bitrate;
        }
        if decreased {
            if let Ok(mut at) = self.last_decrease_at.lock() {
                *at = Some(Instant::now());
            }
        }
        if let Ok(mut pending) = self.bitrate.lock() {
            *pending = Some(bitrate);
        }
    }

    /// Records the actual encoder target (e.g. after the first frame fixes
    /// the pixel-scaled bitrate) without queueing a redundant update.
    pub(crate) fn set_target(&self, bitrate: u32) {
        if let Ok(mut target) = self.target.lock() {
            *target = bitrate;
        }
    }

    pub(crate) fn target(&self) -> u32 {
        self.target.lock().ok().map(|target| *target).unwrap_or(0)
    }

    pub(crate) fn floor(&self) -> u32 {
        self.floor.lock().ok().map(|floor| *floor).unwrap_or(250_000)
    }

    pub(crate) fn applied(&self) -> u32 {
        self.applied.lock().ok().map(|applied| *applied).unwrap_or(0)
    }

    /// Records the rate actually programmed into a fresh encoder (e.g. the
    /// pixel-scaled bitrate at the first frame) so probing starts from the
    /// real base instead of the unscaled target.
    pub(crate) fn note_applied(&self, bitrate: u32) {
        if let Ok(mut applied) = self.applied.lock() {
            *applied = bitrate;
        }
    }

    /// Records RTCP Receiver Report loss (fraction-lost 0-255).
    pub(crate) fn note_loss(&self, fraction_lost: u8) {
        if let Ok(mut last) = self.last_loss.lock() {
            let now = Instant::now();
            match last.1 {
                Some(at) if now.duration_since(at) < LOSS_WINDOW => {
                    last.0 = last.0.max(fraction_lost);
                }
                _ => *last = (fraction_lost, Some(now)),
            }
        }
    }

    /// Sender-side probe: when the path looks clean (no recent decrease,
    /// no recent loss) and we sit below the target, suggest the next step
    /// up (+25%, min +200 kbps). REMB decreases still win immediately.
    /// `now` is a parameter so unit tests can time-travel.
    pub(crate) fn probe_candidate(&self, now: Instant) -> Option<u32> {
        if let Ok(decrease) = self.last_decrease_at.lock() {
            if let Some(at) = *decrease {
                if now.duration_since(at) < PROBE_QUIET {
                    return None;
                }
            }
        }
        if let Ok(loss) = self.last_loss.lock() {
            if loss.0 > LOSS_FREEZE_FRACTION {
                if let Some(at) = loss.1 {
                    if now.duration_since(at) < LOSS_WINDOW {
                        return None;
                    }
                }
            }
        }
        let applied = self.applied();
        let target = self.target();
        if applied >= target {
            return None;
        }
        let step = (applied / 4).max(200_000);
        Some(applied.saturating_add(step).min(target))
    }

    /// Queues a probe step without moving the target or floor.
    pub(crate) fn apply_probe(&self, bitrate: u32) {
        if let Ok(mut congestion) = self.congestion.lock() {
            *congestion = Some(bitrate);
        }
        if let Ok(mut applied) = self.applied.lock() {
            *applied = bitrate;
        }
        if let Ok(mut pending) = self.bitrate.lock() {
            *pending = Some(bitrate);
        }
    }

    pub(crate) fn congestion_bitrate(&self) -> Option<u32> {
        self.congestion.lock().ok().and_then(|congestion| *congestion)
    }

    pub(crate) fn request_stop(&self) {
        self.stop_requested.store(true, Ordering::Release);
    }

    fn take_keyframe(&self) -> bool {
        self.keyframe_requested.swap(false, Ordering::AcqRel)
    }

    fn take_bitrate(&self) -> Option<u32> {
        self.bitrate
            .lock()
            .ok()
            .and_then(|mut pending| pending.take())
    }

    fn stop_requested(&self) -> bool {
        self.stop_requested.load(Ordering::Acquire)
    }
}

#[allow(dead_code)]
pub(crate) struct NativePipeline {
    pub(crate) capture_tx: SyncSender<NativeFrame>,
    pub(crate) encoder_tx: SyncSender<EncoderCommand>,
    pub(crate) encoder_control: Arc<EncoderControl>,
    access_unit_rx: Option<AccessUnitReceiver>,
    pub(crate) generation: u64,
    pub(crate) state: Arc<PipelineState>,
    shutdown: Arc<AtomicBool>,
    preview_completion: Arc<WorkerCompletion>,
    encoder_completion: Arc<WorkerCompletion>,
    preview_diagnostics: Arc<PreviewDiagnostics>,
    preview_worker: Option<JoinHandle<()>>,
    encoder_worker: Option<JoinHandle<()>>,
}

impl NativePipeline {
    pub(crate) fn new(
        preview: Arc<PreviewState>,
        resolution: VideoResolution,
        _frame_rate: FrameRate,
        quality: TransmissionQuality,
        bitrate_bps: Option<u32>,
        min_bitrate_bps: Option<u32>,
    ) -> Self {
        let generation = preview.generation.load(Ordering::Acquire);
        let (capture_tx, capture_rx) = sync_channel::<NativeFrame>(CAPTURE_QUEUE_CAPACITY);
        let (encoder_tx, encoder_rx) = sync_channel(ENCODER_QUEUE_CAPACITY);
        let (access_unit_tx, access_unit_rx) = AccessUnitQueue::bounded(ACCESS_UNIT_QUEUE_CAPACITY);
        let pipeline_state = PipelineState::new();
        let initial_target = super::types::resolve_bitrate(quality, bitrate_bps);
        let encoder_control = EncoderControl::new(
            initial_target,
            super::types::resolve_floor(initial_target, min_bitrate_bps),
        );
        let shutdown = Arc::new(AtomicBool::new(false));
        let preview_completion = Arc::new(WorkerCompletion {
            done: Mutex::new(false),
            wake: Condvar::new(),
        });
        let encoder_completion = Arc::new(WorkerCompletion {
            done: Mutex::new(false),
            wake: Condvar::new(),
        });
        let preview_diagnostics = Arc::clone(&preview.diagnostics);
        let worker_state = Arc::clone(&pipeline_state);
        let worker_shutdown = Arc::clone(&shutdown);
        let worker_control = Arc::clone(&encoder_control);
        let preview_shutdown = Arc::clone(&shutdown);
        let encoder_completion_worker = Arc::clone(&encoder_completion);
        let preview_completion_worker = Arc::clone(&preview_completion);
        let (width, height) = match resolution {
            VideoResolution::P1080 => (1920, 1080),
            VideoResolution::P720 => (1280, 720),
        };
        // The quality preset wins over `frame_rate` for the encoder fps.
        let fps = match quality.frame_rate() {
            FrameRate::Fps60 => 60,
            FrameRate::Fps30 => 30,
        };
        let encoder_worker = thread::Builder::new()
            .name("godrinking-media-encoder".into())
            .spawn(move || {
                encoder_worker_loop(
                    encoder_rx,
                    access_unit_tx,
                    width,
                    height,
                    fps,
                    quality,
                    bitrate_bps,
                    generation,
                    worker_state,
                    worker_shutdown,
                    worker_control,
                );
                mark_worker_complete(&encoder_completion_worker);
            })
            .expect("failed to start media encoder worker");
        let preview_worker = thread::Builder::new()
            .name("godrinking-media-preview".into())
            .spawn(move || {
                loop {
                    let frame = match capture_rx.recv_timeout(Duration::from_millis(10)) {
                        Ok(frame) => frame,
                        Err(RecvTimeoutError::Timeout)
                            if !preview_shutdown.load(Ordering::Acquire) =>
                        {
                            continue;
                        }
                        Err(_) => break,
                    };
                    if frame.generation != generation {
                        continue;
                    }
                    let event = PreviewFrameEvent {
                        sequence: frame.sequence,
                        timestamp_micros: frame.timestamp_micros,
                        width: frame.width,
                        height: frame.height,
                        encoding: "rgb8_thumbnail".into(),
                        payload: frame.storage.to_vec(),
                    };
                    if let Ok(mut latest) = preview.latest.lock() {
                        if preview.generation.load(Ordering::Acquire) == generation {
                            *latest = Some(event);
                        }
                    }
                }
                mark_worker_complete(&preview_completion_worker);
            })
            .expect("failed to start media preview worker");
        Self {
            capture_tx,
            encoder_tx,
            encoder_control,
            access_unit_rx: Some(access_unit_rx),
            generation,
            state: pipeline_state,
            shutdown,
            preview_completion,
            encoder_completion,
            preview_diagnostics,
            preview_worker: Some(preview_worker),
            encoder_worker: Some(encoder_worker),
        }
    }

    pub(crate) fn force_keyframe(&self) -> Result<(), String> {
        self.encoder_control.request_keyframe();
        Ok(())
    }

    pub(crate) fn take_access_unit_receiver(&mut self) -> AccessUnitReceiver {
        self.access_unit_rx
            .take()
            .expect("access-unit receiver can only be claimed once")
    }

    pub(crate) fn set_bitrate(&self, bitrate: u32) -> Result<(), String> {
        self.encoder_control.set_bitrate(bitrate);
        Ok(())
    }

    /// Current encoder bitrate target (preset or user override, as applied
    /// to the actual frame size). Surfaced in session snapshots.
    pub(crate) fn bitrate_target(&self) -> u32 {
        self.encoder_control.target()
    }

    /// Last congestion-imposed bitrate, if any REMB signal arrived.
    pub(crate) fn bitrate_congestion(&self) -> Option<u32> {
        self.encoder_control.congestion_bitrate()
    }

    pub(crate) fn set_floor(&self, floor: u32) -> Result<(), String> {
        self.encoder_control.set_floor(floor);
        Ok(())
    }

    /// Current congestion floor.
    pub(crate) fn bitrate_floor(&self) -> u32 {
        self.encoder_control.floor()
    }

    pub(crate) fn preview_diagnostics(&self) -> Arc<PreviewDiagnostics> {
        Arc::clone(&self.preview_diagnostics)
    }
}

fn mark_worker_complete(completion: &WorkerCompletion) {
    if let Ok(mut done) = completion.done.lock() {
        *done = true;
        completion.wake.notify_all();
    }
}

fn finish_worker(worker: JoinHandle<()>, completion: &Arc<WorkerCompletion>) {
    let Ok(mut done) = completion.done.lock() else {
        drop(worker);
        return;
    };
    if !*done {
        let Ok((next, _)) = completion
            .wake
            .wait_timeout(done, WORKER_COMPLETION_TIMEOUT)
        else {
            drop(worker);
            return;
        };
        done = next;
    }
    let completed = *done;
    drop(done);
    if completed {
        let _ = worker.join();
    } else {
        drop(worker);
    }
}

impl Drop for NativePipeline {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        self.encoder_control.request_stop();
        if let Some(worker) = self.encoder_worker.take() {
            finish_worker(worker, &self.encoder_completion);
        }
        if let Some(worker) = self.preview_worker.take() {
            finish_worker(worker, &self.preview_completion);
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn encoder_bitrate(
    width: u32,
    height: u32,
    quality: TransmissionQuality,
    override_bps: Option<u32>,
) -> u32 {
    // An explicit user value is used as-is (clamped): it expresses intent
    // for the whole frame, so pixel scaling must not shrink it.
    if let Some(bps) = override_bps.filter(|bps| *bps > 0) {
        return super::types::clamp_bitrate_bps(bps);
    }
    // Start from the quality preset and scale by the actual pixel count
    // relative to the preset's capture cap, clamped so the preset is the
    // ceiling and small sources never starve the encoder.
    let preset = quality.bitrate();
    let (max_width, max_height) = quality.max_dimensions();
    let pixels = (width as u64).saturating_mul(height as u64).max(1);
    let cap_pixels = (max_width as u64).saturating_mul(max_height as u64).max(1);
    let scaled = ((preset as u64).saturating_mul(pixels) / cap_pixels) as u32;
    scaled.clamp(preset / 4, preset)
}

#[cfg(target_os = "macos")]
fn pixel_buffer_size(buffer: &CVPixelBuffer) -> (u32, u32) {
    use objc2_core_video::{CVPixelBufferGetHeight, CVPixelBufferGetWidth};
    let width = (CVPixelBufferGetWidth(buffer) as u32).max(2) & !1;
    let height = (CVPixelBufferGetHeight(buffer) as u32).max(2) & !1;
    (width.max(2), height.max(2))
}

#[cfg(target_os = "macos")]
fn encoder_worker_loop(
    receiver: Receiver<EncoderCommand>,
    output: AccessUnitQueue,
    _width: u32,
    _height: u32,
    fps: u32,
    quality: TransmissionQuality,
    bitrate_bps: Option<u32>,
    generation: u64,
    state: Arc<PipelineState>,
    shutdown: Arc<AtomicBool>,
    control: Arc<EncoderControl>,
) {
    let mut encoder: Option<super::video_toolbox::VideoToolboxEncoder> = None;
    let mut pending_output = Some(output);
    loop {
        if control.stop_requested() {
            if let Some(encoder) = encoder.as_mut() {
                let _ = encoder.flush();
            }
            return;
        }
        if let Some(bitrate) = control.take_bitrate() {
            if let Some(encoder) = encoder.as_mut() {
                let _ = encoder.set_bitrate(bitrate);
            }
        }
        if control.take_keyframe() {
            if let Some(encoder) = encoder.as_mut() {
                let _ = encoder.force_keyframe();
            }
        }
        if state.is_failed() {
            break;
        }
        let command = match receiver.recv_timeout(Duration::from_millis(10)) {
            Ok(command) => command,
            Err(RecvTimeoutError::Timeout) => {
                if shutdown.load(Ordering::Acquire) {
                    if let Some(encoder) = encoder.as_mut() {
                        let _ = encoder.flush();
                    }
                    break;
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };
        match command {
            EncoderCommand::Video(frame) => {
                if frame.generation != generation {
                    continue;
                }
                if encoder.is_none() {
                    let Some(output) = pending_output.take() else {
                        continue;
                    };
                    let size = pixel_buffer_size(&frame.pixel_buffer);
                    let initial = encoder_bitrate(size.0, size.1, quality, bitrate_bps);
                    control.set_target(initial);
                    control.note_applied(initial);
                    match super::video_toolbox::VideoToolboxEncoder::new(
                        size.0,
                        size.1,
                        initial,
                        fps,
                        output,
                        Arc::clone(&state),
                        Arc::clone(&control),
                    ) {
                        Ok(next) => {
                            encoder = Some(next);
                            control.request_keyframe();
                        }
                        Err(error) => {
                            state.fail(format!("VideoToolbox initialization failed: {error}"));
                            return;
                        }
                    }
                }
                let Some(active) = encoder.as_mut() else {
                    continue;
                };
                if let Err(error) = active.encode(
                    &*frame.pixel_buffer as *const CVPixelBuffer as *mut CVPixelBuffer,
                    frame.timestamp_micros as i64,
                    1_000_000,
                ) {
                    eprintln!("[goDrinking] VideoToolbox encode skipped: {error}");
                    control.request_keyframe();
                }
            }
            EncoderCommand::Flush => {
                if let Some(encoder) = encoder.as_mut() {
                    let _ = encoder.flush();
                }
            }
            EncoderCommand::ForceKeyframe => {
                control.request_keyframe();
            }
            EncoderCommand::SetBitrate(bitrate) => {
                control.set_bitrate(bitrate);
            }
            EncoderCommand::Stop => {
                control.request_stop();
            }
        }
    }
}

#[cfg(target_os = "windows")]
fn encoder_worker_loop(
    receiver: Receiver<EncoderCommand>,
    output: AccessUnitQueue,
    _width: u32,
    _height: u32,
    fps: u32,
    quality: TransmissionQuality,
    bitrate_bps: Option<u32>,
    generation: u64,
    state: Arc<PipelineState>,
    shutdown: Arc<AtomicBool>,
    control: Arc<EncoderControl>,
) {
    let mut encoder: Option<super::windows_encoder::OpenH264Encoder> = None;
    let mut pending_output = Some(output);
    loop {
        if control.stop_requested() {
            return;
        }
        if let Some(bitrate) = control.take_bitrate() {
            if let Some(encoder) = encoder.as_mut() {
                let _ = encoder.set_bitrate(bitrate);
            }
        }
        if control.take_keyframe() {
            if let Some(encoder) = encoder.as_mut() {
                encoder.force_keyframe();
            }
        }
        if state.is_failed() {
            break;
        }
        let command = match receiver.recv_timeout(Duration::from_millis(10)) {
            Ok(command) => command,
            Err(RecvTimeoutError::Timeout) => {
                if shutdown.load(Ordering::Acquire) {
                    break;
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };
        match command {
            EncoderCommand::Video(frame) => {
                if frame.generation != generation {
                    continue;
                }
                if encoder.is_none() {
                    let Some(output) = pending_output.take() else {
                        continue;
                    };
                    let width = frame.width.max(2) & !1;
                    let height = frame.height.max(2) & !1;
                    let initial = encoder_bitrate(width, height, quality, bitrate_bps);
                    control.set_target(initial);
                    control.note_applied(initial);
                    match super::windows_encoder::OpenH264Encoder::new(
                        width,
                        height,
                        initial,
                        fps,
                        output,
                        Arc::clone(&state),
                        Arc::clone(&control),
                    ) {
                        Ok(next) => {
                            encoder = Some(next);
                            control.request_keyframe();
                        }
                        Err(error) => {
                            state.fail(format!("OpenH264 initialization failed: {error}"));
                            return;
                        }
                    }
                }
                let Some(active) = encoder.as_mut() else {
                    continue;
                };
                if let Err(error) = active.encode(&frame) {
                    eprintln!("[goDrinking] OpenH264 encode skipped: {error}");
                    control.request_keyframe();
                }
            }
            EncoderCommand::Flush => {}
            EncoderCommand::ForceKeyframe => {
                control.request_keyframe();
            }
            EncoderCommand::SetBitrate(bitrate) => {
                control.set_bitrate(bitrate);
            }
            EncoderCommand::Stop => {
                control.request_stop();
            }
        }
    }
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
fn encoder_worker_loop(
    receiver: Receiver<EncoderCommand>,
    _output: AccessUnitQueue,
    _width: u32,
    _height: u32,
    _fps: u32,
    _quality: TransmissionQuality,
    _bitrate_bps: Option<u32>,
    _generation: u64,
    _state: Arc<PipelineState>,
    _shutdown: Arc<AtomicBool>,
    _control: Arc<EncoderControl>,
) {
    while receiver.recv().is_ok() {}
}

#[cfg(test)]
mod tests {
    use super::super::types::{FrameRate, TransmissionQuality, VideoResolution};
    use super::{EncoderCommand, EncoderControl, NativeFrame, NativePipeline, PreviewState};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[test]
    fn probing_climbs_toward_target_when_path_is_clean() {
        let control = EncoderControl::new(20_000_000, 1_000_000);
        control.set_congestion_bitrate(4_000_000);
        assert_eq!(control.applied(), 4_000_000);
        // Fresh decrease: quiet period holds.
        assert_eq!(control.probe_candidate(Instant::now()), None);
        // After quiet: +25% steps toward the target.
        let later = Instant::now() + Duration::from_secs(10);
        assert_eq!(control.probe_candidate(later), Some(5_000_000));
        control.apply_probe(5_000_000);
        assert_eq!(control.probe_candidate(later), Some(6_250_000));
    }

    #[test]
    fn probing_freezes_on_recent_loss_and_caps_at_target() {
        let control = EncoderControl::new(8_000_000, 1_000_000);
        control.set_congestion_bitrate(4_000_000);
        control.note_loss(20);
        let later = Instant::now() + Duration::from_secs(4);
        // Quiet (3s) passed but loss window (5s) still holds.
        assert_eq!(control.probe_candidate(later), None);
        // Loss expired: probing resumes.
        let much_later = Instant::now() + Duration::from_secs(30);
        assert_eq!(control.probe_candidate(much_later), Some(5_000_000));
        // Never above the target.
        control.set_congestion_bitrate(7_900_000);
        control.note_loss(0);
        assert_eq!(control.probe_candidate(much_later), Some(8_000_000));
        control.apply_probe(8_000_000);
        assert_eq!(control.probe_candidate(much_later), None);
    }

    #[test]
    fn congestion_respects_floor_and_floor_raise_reasserts() {
        let control = EncoderControl::new(20_000_000, 1_000_000);
        control.set_congestion_bitrate(100_000);
        assert_eq!(control.applied(), 1_000_000);
        assert_eq!(control.congestion_bitrate(), Some(1_000_000));
        control.set_floor(5_000_000);
        assert_eq!(control.floor(), 5_000_000);
        assert_eq!(control.take_bitrate(), Some(5_000_000));
    }

    #[test]
    fn pipeline_uses_bounded_channels_and_shared_frame_storage() {
        let preview = std::sync::Arc::new(PreviewState::new());
        preview.begin_session();
        let pipeline = NativePipeline::new(
            preview,
            VideoResolution::P1080,
            FrameRate::Fps60,
            TransmissionQuality::High,
            None,
            None,
        );
        let storage: Arc<[u8]> = Arc::from([1_u8, 2, 3]);
        let frame = NativeFrame {
            storage: Arc::clone(&storage),
            timestamp_micros: 10,
            sequence: 1,
            width: 1,
            height: 1,
            generation: 1,
        };
        assert_eq!(Arc::strong_count(&storage), 2);
        pipeline
            .capture_tx
            .try_send(frame)
            .expect("capture queue should accept its first frame");
        let command = EncoderCommand::Flush;
        pipeline
            .encoder_tx
            .try_send(command)
            .expect("encoder queue should accept control messages");
        assert_eq!(Arc::strong_count(&storage), 2);
    }

    #[test]
    fn pipeline_publishes_only_the_latest_derived_preview() {
        let preview = std::sync::Arc::new(PreviewState::new());
        preview.begin_session();
        let pipeline = NativePipeline::new(
            std::sync::Arc::clone(&preview),
            VideoResolution::P1080,
            FrameRate::Fps60,
            TransmissionQuality::High,
            None,
            None,
        );
        pipeline
            .capture_tx
            .try_send(NativeFrame {
                storage: Arc::from([9_u8, 8, 7]),
                timestamp_micros: 22,
                sequence: 4,
                width: 2,
                height: 1,
                generation: 1,
            })
            .expect("capture queue should accept the preview");

        for _ in 0..100 {
            if let Some(event) = preview.latest.lock().expect("preview state").clone() {
                assert_eq!(event.sequence, 4);
                assert_eq!(event.encoding, "rgb8_thumbnail");
                assert_eq!(event.payload, vec![9, 8, 7]);
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        panic!("preview worker did not publish the frame");
    }

    #[test]
    fn pipeline_drops_frames_after_the_preview_session_changes() {
        let preview = std::sync::Arc::new(PreviewState::new());
        preview.begin_session();
        let pipeline = NativePipeline::new(
            std::sync::Arc::clone(&preview),
            VideoResolution::P1080,
            FrameRate::Fps60,
            TransmissionQuality::High,
            None,
            None,
        );
        preview.begin_session();
        pipeline
            .capture_tx
            .try_send(NativeFrame {
                storage: Arc::from([1_u8, 2, 3]),
                timestamp_micros: 30,
                sequence: 5,
                width: 2,
                height: 1,
                generation: 1,
            })
            .expect("capture queue should accept the stale preview");

        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(preview.latest.lock().expect("preview state").is_none());
    }

    #[test]
    fn pipeline_drop_quiesces_workers() {
        let preview = std::sync::Arc::new(PreviewState::new());
        preview.begin_session();
        let pipeline = NativePipeline::new(
            preview,
            VideoResolution::P720,
            FrameRate::Fps30,
            TransmissionQuality::Low,
            None,
            None,
        );
        drop(pipeline);
    }

    #[test]
    fn encoder_control_coalesces_feedback_bursts_without_saturation() {
        let control = EncoderControl::new(8_000_000, 1_000_000);
        for bitrate in 250_000..260_000 {
            control.request_keyframe();
            control.set_bitrate(bitrate);
        }
        assert!(control.take_keyframe());
        assert!(!control.take_keyframe());
        assert_eq!(control.take_bitrate(), Some(259_999));
        assert_eq!(control.take_bitrate(), None);
        control.request_stop();
        assert!(control.stop_requested());
    }
}
