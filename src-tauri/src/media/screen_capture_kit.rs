//! macOS ScreenCaptureKit source, permission, and stream adapter.
//!
//! The native callback copies only a small, derived RGB thumbnail into the
//! bounded media pipeline. The IOSurface-backed source buffer is never
//! serialized or sent through Tauri IPC.

#[cfg(target_os = "macos")]
use super::pipeline::NativeEncoderFrame;
use super::pipeline::{EncoderCommand, NativeFrame, PreviewDiagnostics};
use super::types::{
    CaptureSource, CreateMediaSessionRequest, FrameRate, NativeCaptureSource,
};
use serde::Serialize;
use std::fmt::{Display, Formatter};
use std::marker::PhantomData;
use std::sync::mpsc::SyncSender;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PendingNativeOperation {
    Start {
        generation: u64,
        operation_id: u64,
    },
    Stop {
        generation: u64,
        operation_id: u64,
        was_failed: bool,
    },
}

/// The generation/operation state machine used by the real native actor.
/// Keeping this reducer free of Objective-C handles makes late-event behavior
/// deterministic without inventing a second lifecycle implementation.
#[derive(Debug)]
pub(crate) struct NativeTransitionReducer {
    active_generation: Option<u64>,
    pending: Option<PendingNativeOperation>,
    lifecycle: CaptureLifecycle,
    shutdown_requested: bool,
}

impl NativeTransitionReducer {
    pub(crate) fn new() -> Self {
        Self {
            active_generation: None,
            pending: None,
            lifecycle: CaptureLifecycle::Idle,
            shutdown_requested: false,
        }
    }

    fn can_start(&self) -> bool {
        self.active_generation.is_none() && self.pending.is_none() && !self.shutdown_requested
    }

    fn begin_start(&mut self, generation: u64, operation_id: u64) -> bool {
        if !self.can_start() {
            return false;
        }
        self.pending = Some(PendingNativeOperation::Start {
            generation,
            operation_id,
        });
        self.lifecycle = CaptureLifecycle::Starting;
        true
    }

    fn start_timed_out(&mut self) {
        if matches!(self.pending, Some(PendingNativeOperation::Start { .. })) {
            self.lifecycle = CaptureLifecycle::Failed;
        }
    }

    fn complete_start(&mut self, generation: u64, operation_id: u64, success: bool) -> bool {
        if self.pending
            != Some(PendingNativeOperation::Start {
                generation,
                operation_id,
            })
        {
            return false;
        }
        self.pending = None;
        if success {
            self.active_generation = Some(generation);
            self.lifecycle = CaptureLifecycle::Running;
        } else {
            self.lifecycle = CaptureLifecycle::Idle;
        }
        true
    }

    fn begin_stop(&mut self, generation: u64, operation_id: u64) -> bool {
        if self.active_generation != Some(generation)
            || self.pending.is_some()
            || !matches!(
                self.lifecycle,
                CaptureLifecycle::Running
                    | CaptureLifecycle::Failed
                    | CaptureLifecycle::CleanupPending
            )
        {
            return false;
        }
        let was_failed = matches!(
            self.lifecycle,
            CaptureLifecycle::Failed | CaptureLifecycle::CleanupPending
        );
        self.pending = Some(PendingNativeOperation::Stop {
            generation,
            operation_id,
            was_failed,
        });
        self.lifecycle = CaptureLifecycle::Stopping;
        true
    }

    fn stop_timed_out(&mut self) {
        if matches!(self.pending, Some(PendingNativeOperation::Stop { .. })) {
            self.lifecycle = CaptureLifecycle::Stopping;
        }
    }

    fn stop_was_from_failed(&self, generation: u64, operation_id: u64) -> bool {
        matches!(
            self.pending,
            Some(PendingNativeOperation::Stop {
                generation: pending_generation,
                operation_id: pending_operation,
                was_failed: true,
            }) if pending_generation == generation && pending_operation == operation_id
        )
    }

    fn complete_stop(
        &mut self,
        generation: u64,
        operation_id: u64,
        native_success: bool,
        cleanup_success: bool,
    ) -> bool {
        let Some(PendingNativeOperation::Stop { was_failed, .. }) = self.pending else {
            return false;
        };
        if !matches!(
            self.pending,
            Some(PendingNativeOperation::Stop {
                generation: pending_generation,
                operation_id: pending_operation,
                ..
            }) if pending_generation == generation && pending_operation == operation_id
        ) {
            return false;
        }
        self.pending = None;
        if !native_success {
            self.lifecycle = if was_failed {
                CaptureLifecycle::Failed
            } else {
                CaptureLifecycle::Running
            };
        } else if cleanup_success {
            self.active_generation = None;
            self.lifecycle = CaptureLifecycle::Idle;
        } else {
            self.lifecycle = CaptureLifecycle::CleanupPending;
        }
        true
    }

    fn mark_cleanup_pending(&mut self, generation: u64) {
        self.active_generation = Some(generation);
        self.pending = None;
        self.lifecycle = CaptureLifecycle::CleanupPending;
    }

    fn terminate(&mut self, generation: u64) -> bool {
        let matches_current = self.active_generation == Some(generation)
            || matches!(
                self.pending,
                Some(PendingNativeOperation::Start {
                    generation: pending_generation,
                    ..
                }) if pending_generation == generation
            );
        if !matches_current {
            return false;
        }
        // A termination notification does not prove that the SCStream object
        // is safe to release. Retain ownership until an explicit stop has
        // completed, otherwise SCStream may be deallocated while still live.
        self.active_generation = Some(generation);
        self.pending = None;
        self.lifecycle = CaptureLifecycle::Failed;
        true
    }

    fn request_shutdown(&mut self) {
        self.shutdown_requested = true;
    }

    #[cfg(test)]
    fn lifecycle(&self) -> CaptureLifecycle {
        self.lifecycle
    }

    #[cfg(test)]
    fn active_generation(&self) -> Option<u64> {
        self.active_generation
    }
}

#[cfg(target_os = "macos")]
mod native {
    use super::*;
    use block2::RcBlock;
    use dispatch2::{DispatchQueue, DispatchRetained};
    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
    use objc2_core_media::{CMSampleBuffer, CMTime};
    use objc2_core_video::{
        kCVPixelFormatType_32BGRA, kCVReturnSuccess, CVPixelBuffer, CVPixelBufferGetBaseAddress,
        CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType,
        CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
        CVPixelBufferUnlockBaseAddress,
    };
    use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
    use objc2_screen_capture_kit::{
        SCContentFilter, SCContentSharingPicker, SCContentSharingPickerObserver, SCShareableContent,
        SCShareableContentStyle, SCStream, SCStreamConfiguration, SCStreamDelegate, SCStreamOutput,
        SCStreamOutputType, SCWindow,
    };
    use std::slice;
    use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
    use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    const PREVIEW_WIDTH: u32 = 160;
    const PREVIEW_HEIGHT: u32 = 90;
    const PREVIEW_INTERVAL_MICROS: u64 = 100_000;
    pub(crate) const CALLBACK_TIMEOUT: Duration = Duration::from_secs(10);
    const PICKER_TIMEOUT: Duration = Duration::from_secs(180);
    static MONOTONIC_START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

    const ACTOR_IDLE: u8 = 0;
    const ACTOR_STARTING: u8 = 1;
    const ACTOR_RUNNING: u8 = 2;
    const ACTOR_STOPPING: u8 = 3;
    const ACTOR_FAILED: u8 = 4;
    const ACTOR_CLEANUP_PENDING: u8 = 5;

    pub(crate) enum ActorCommand {
        Start {
            request: CreateMediaSessionRequest,
            capture_tx: SyncSender<NativeFrame>,
            encoder_tx: SyncSender<EncoderCommand>,
            diagnostics: Arc<PreviewDiagnostics>,
            generation: u64,
            operation_id: u64,
            response: SyncSender<Result<(), ScreenCaptureKitError>>,
        },
        StartCompleted {
            generation: u64,
            operation_id: u64,
            result: Result<(), ScreenCaptureKitError>,
        },
        Stop {
            generation: u64,
            operation_id: u64,
            response: SyncSender<Result<(), ScreenCaptureKitError>>,
        },
        StopCompleted {
            generation: u64,
            operation_id: u64,
            result: Result<(), ScreenCaptureKitError>,
        },
        Terminated {
            generation: u64,
            detail: String,
        },
        Shutdown,
    }

    pub(crate) struct ActorStatus {
        lifecycle: AtomicU8,
        failed_generation: AtomicU64,
        failure_detail: Mutex<Option<String>>,
    }

    impl ActorStatus {
        fn new() -> Self {
            Self {
                lifecycle: AtomicU8::new(ACTOR_IDLE),
                failed_generation: AtomicU64::new(0),
                failure_detail: Mutex::new(None),
            }
        }

        fn set(&self, lifecycle: u8) {
            if lifecycle == ACTOR_IDLE {
                self.failed_generation.store(0, Ordering::Release);
                if let Ok(mut detail) = self.failure_detail.lock() {
                    *detail = None;
                }
            }
            self.lifecycle.store(lifecycle, Ordering::Release);
        }

        fn clear_failure(&self) {
            self.failed_generation.store(0, Ordering::Release);
            if let Ok(mut detail) = self.failure_detail.lock() {
                *detail = None;
            }
        }

        fn fail_generation(&self, generation: u64, detail: impl Into<String>) {
            self.failed_generation.store(generation, Ordering::Release);
            if let Ok(mut failure_detail) = self.failure_detail.lock() {
                *failure_detail = Some(detail.into());
            }
            self.set(ACTOR_FAILED);
        }

        fn cleanup_pending(&self, generation: u64, detail: impl Into<String>) {
            self.failed_generation.store(generation, Ordering::Release);
            if let Ok(mut failure_detail) = self.failure_detail.lock() {
                *failure_detail = Some(detail.into());
            }
            self.set(ACTOR_CLEANUP_PENDING);
        }

        fn failed_generation(&self) -> Option<u64> {
            match self.failed_generation.load(Ordering::Acquire) {
                0 => None,
                generation => Some(generation),
            }
        }

        fn failure_detail(&self) -> Option<String> {
            self.failure_detail
                .lock()
                .ok()
                .and_then(|detail| detail.clone())
        }

        pub(crate) fn get(&self) -> super::CaptureLifecycle {
            match self.lifecycle.load(Ordering::Acquire) {
                ACTOR_STARTING => super::CaptureLifecycle::Starting,
                ACTOR_RUNNING => super::CaptureLifecycle::Running,
                ACTOR_STOPPING => super::CaptureLifecycle::Stopping,
                ACTOR_FAILED => super::CaptureLifecycle::Failed,
                ACTOR_CLEANUP_PENDING => super::CaptureLifecycle::CleanupPending,
                _ => super::CaptureLifecycle::Idle,
            }
        }
    }

    #[derive(Debug)]
    struct StreamOutputIvars {
        capture_tx: SyncSender<NativeFrame>,
        encoder_tx: SyncSender<EncoderCommand>,
        actor_tx: SyncSender<ActorCommand>,
        diagnostics: Arc<PreviewDiagnostics>,
        generation: u64,
        last_preview_micros: AtomicU64,
        sequence: AtomicU64,
    }

    define_class!(
        #[unsafe(super(NSObject))]
        #[name = "GoDrinkingScreenCaptureOutput"]
        #[ivars = StreamOutputIvars]
        struct StreamOutput;

        unsafe impl NSObjectProtocol for StreamOutput {}

        unsafe impl SCStreamOutput for StreamOutput {
            #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
            unsafe fn stream_did_output_sample_buffer_of_type(
                &self,
                _stream: &SCStream,
                sample_buffer: &CMSampleBuffer,
                output_type: SCStreamOutputType,
            ) {
                if output_type != SCStreamOutputType::Screen {
                    return;
                }
                self.ivars()
                    .diagnostics
                    .callback_count
                    .fetch_add(1, Ordering::Relaxed);

                if !unsafe { sample_buffer.is_valid() } || !unsafe { sample_buffer.data_is_ready() }
                {
                    return;
                }

                let Some(image_buffer) = sample_buffer.image_buffer() else {
                    // Screen callbacks can include empty/incomplete buffers;
                    // ignore them rather than reporting a stream failure.
                    return;
                };
                let pixel_buffer: &CVPixelBuffer = &image_buffer;
                let timestamp_micros =
                    sample_timestamp_micros(sample_buffer).unwrap_or_else(monotonic_micros);
                let retained_pixel_buffer = unsafe {
                    Retained::retain(pixel_buffer as *const CVPixelBuffer as *mut CVPixelBuffer)
                };
                if let Some(pixel_buffer) = retained_pixel_buffer {
                    let _ = self.ivars().encoder_tx.try_send(EncoderCommand::Video(
                        NativeEncoderFrame {
                            pixel_buffer,
                            timestamp_micros,
                            generation: self.ivars().generation,
                        },
                    ));
                }

                let now = monotonic_micros();
                let last = self.ivars().last_preview_micros.load(Ordering::Relaxed);
                if last != 0 && now.saturating_sub(last) < PREVIEW_INTERVAL_MICROS {
                    return;
                }

                if CVPixelBufferGetPixelFormatType(pixel_buffer) != kCVPixelFormatType_32BGRA {
                    self.ivars().diagnostics.record_error(
                        "ScreenCaptureKit callback returned a non-BGRA pixel buffer.",
                    );
                    return;
                }

                if CVPixelBufferLockBaseAddress(pixel_buffer, CVPixelBufferLockFlags::ReadOnly)
                    != kCVReturnSuccess
                {
                    self.ivars()
                        .diagnostics
                        .record_error("ScreenCaptureKit could not lock the pixel buffer.");
                    return;
                }
                let width = CVPixelBufferGetWidth(pixel_buffer);
                let height = CVPixelBufferGetHeight(pixel_buffer);
                let bytes_per_row = CVPixelBufferGetBytesPerRow(pixel_buffer);
                let base_address = CVPixelBufferGetBaseAddress(pixel_buffer);
                let thumbnail = thumbnail_rgb(
                    base_address,
                    width,
                    height,
                    bytes_per_row,
                    PREVIEW_WIDTH,
                    PREVIEW_HEIGHT,
                );
                let _ =
                    CVPixelBufferUnlockBaseAddress(pixel_buffer, CVPixelBufferLockFlags::ReadOnly);
                let Some(storage) = thumbnail else {
                    self.ivars()
                        .diagnostics
                        .record_error("ScreenCaptureKit thumbnail conversion failed.");
                    return;
                };

                self.ivars()
                    .last_preview_micros
                    .store(now, Ordering::Relaxed);
                let sequence = self.ivars().sequence.fetch_add(1, Ordering::Relaxed) + 1;
                let result = self.ivars().capture_tx.try_send(NativeFrame {
                    storage: storage.into(),
                    timestamp_micros,
                    sequence,
                    width: PREVIEW_WIDTH,
                    height: PREVIEW_HEIGHT,
                    generation: self.ivars().generation,
                });
                match result {
                    Ok(()) => {
                        self.ivars()
                            .diagnostics
                            .frame_count
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    Err(std::sync::mpsc::TrySendError::Full(_)) => {
                        self.ivars()
                            .diagnostics
                            .dropped_count
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                        self.ivars()
                            .diagnostics
                            .record_error("Native preview queue is disconnected.");
                    }
                }
            }
        }

        unsafe impl SCStreamDelegate for StreamOutput {
            #[unsafe(method(stream:didStopWithError:))]
            unsafe fn stream_did_stop_with_error(&self, _stream: &SCStream, error: &NSError) {
                let detail = native_error("streamDidStop", error).to_string();
                self.ivars().diagnostics.record_error(detail.clone());
                let _ = self.ivars().actor_tx.try_send(ActorCommand::Terminated {
                    generation: self.ivars().generation,
                    detail,
                });
            }
        }
    );

    impl StreamOutput {
        fn new_with_actor(
            capture_tx: SyncSender<NativeFrame>,
            encoder_tx: SyncSender<EncoderCommand>,
            actor_tx: SyncSender<ActorCommand>,
            diagnostics: Arc<PreviewDiagnostics>,
            generation: u64,
        ) -> Retained<Self> {
            let this = Self::alloc().set_ivars(StreamOutputIvars {
                capture_tx,
                encoder_tx,
                actor_tx,
                diagnostics,
                generation,
                last_preview_micros: AtomicU64::new(0),
                sequence: AtomicU64::new(0),
            });
            unsafe { msg_send![super(this), init] }
        }
    }

    struct SendFilter(Retained<SCContentFilter>);
    unsafe impl Send for SendFilter {}

    enum PickerEvent {
        Selected(SendFilter),
        Cancelled,
        Failed(String),
    }

    struct PickerObserverIvars {
        tx: SyncSender<PickerEvent>,
    }

    define_class!(
        #[unsafe(super(NSObject))]
        #[name = "GoDrinkingContentPickerObserver"]
        #[ivars = PickerObserverIvars]
        struct PickerObserver;

        unsafe impl NSObjectProtocol for PickerObserver {}

        unsafe impl SCContentSharingPickerObserver for PickerObserver {
            #[unsafe(method(contentSharingPicker:didCancelForStream:))]
            unsafe fn content_sharing_picker_did_cancel_for_stream(
                &self,
                _picker: &SCContentSharingPicker,
                _stream: Option<&SCStream>,
            ) {
                let _ = self.ivars().tx.send(PickerEvent::Cancelled);
            }

            #[unsafe(method(contentSharingPicker:didUpdateWithFilter:forStream:))]
            unsafe fn content_sharing_picker_did_update_with_filter_for_stream(
                &self,
                _picker: &SCContentSharingPicker,
                filter: &SCContentFilter,
                _stream: Option<&SCStream>,
            ) {
                let retained = unsafe {
                    Retained::retain(filter as *const SCContentFilter as *mut SCContentFilter)
                };
                if let Some(filter) = retained {
                    let _ = self.ivars().tx.send(PickerEvent::Selected(SendFilter(filter)));
                } else {
                    let _ = self
                        .ivars()
                        .tx
                        .send(PickerEvent::Failed("picker returned an empty filter".into()));
                }
            }

            #[unsafe(method(contentSharingPickerStartDidFailWithError:))]
            unsafe fn content_sharing_picker_start_did_fail_with_error(&self, error: &NSError) {
                let _ = self
                    .ivars()
                    .tx
                    .send(PickerEvent::Failed(error.localizedDescription().to_string()));
            }
        }
    );

    unsafe impl Send for PickerObserver {}
    unsafe impl Sync for PickerObserver {}

    impl PickerObserver {
        fn new(tx: SyncSender<PickerEvent>) -> Retained<Self> {
            let this = Self::alloc().set_ivars(PickerObserverIvars { tx });
            unsafe { msg_send![super(this), init] }
        }
    }

    pub(crate) struct NativeCapture {
        generation: u64,
        stream: Retained<SCStream>,
        _output: Retained<StreamOutput>,
        _sample_queue: DispatchRetained<DispatchQueue>,
    }

    pub(crate) struct NativeCaptureActor {
        command_tx: SyncSender<ActorCommand>,
        status: Arc<ActorStatus>,
        next_operation: AtomicU64,
    }

    impl NativeCaptureActor {
        pub(crate) fn spawn() -> Self {
            let (command_tx, command_rx) = sync_channel(8);
            let status = Arc::new(ActorStatus::new());
            let actor_status = Arc::clone(&status);
            let actor_tx = command_tx.clone();
            thread::Builder::new()
                .name("godrinking-screencapturekit-actor".into())
                .spawn(move || actor_loop(command_rx, actor_tx, actor_status))
                .expect("failed to start ScreenCaptureKit actor");
            Self {
                command_tx,
                status,
                next_operation: AtomicU64::new(1),
            }
        }

        pub(crate) fn lifecycle(&self) -> super::CaptureLifecycle {
            self.status.get()
        }

        pub(crate) fn failure_detail(&self) -> Option<String> {
            self.status.failed_generation().map(|generation| {
                self.status.failure_detail().unwrap_or_else(|| match self.status.get() {
                    super::CaptureLifecycle::CleanupPending => format!(
                        "ScreenCaptureKit stream generation {generation} stopped, but output cleanup is pending; retry stop."
                    ),
                    _ => format!(
                        "ScreenCaptureKit stream generation {generation} terminated unexpectedly; stop and retry are available."
                    ),
                })
            })
        }

        pub(crate) fn start(
            &self,
            request: &CreateMediaSessionRequest,
            capture_tx: SyncSender<NativeFrame>,
            encoder_tx: SyncSender<EncoderCommand>,
            diagnostics: Arc<PreviewDiagnostics>,
            generation: u64,
        ) -> Result<(), ScreenCaptureKitError> {
            let operation_id = self.next_operation.fetch_add(1, Ordering::Relaxed);
            let (response_tx, response_rx) = sync_channel(1);
            self.command_tx
                .send(ActorCommand::Start {
                    request: request.clone(),
                    capture_tx,
                    encoder_tx,
                    diagnostics,
                    generation,
                    operation_id,
                    response: response_tx,
                })
                .map_err(|_| ScreenCaptureKitError::ActorUnavailable)?;
            response_rx
                .recv()
                .map_err(|_| ScreenCaptureKitError::ActorUnavailable)?
        }

        pub(crate) fn stop(&self, generation: u64) -> Result<(), ScreenCaptureKitError> {
            let operation_id = self.next_operation.fetch_add(1, Ordering::Relaxed);
            let (response_tx, response_rx) = sync_channel(1);
            self.command_tx
                .send(ActorCommand::Stop {
                    generation,
                    operation_id,
                    response: response_tx,
                })
                .map_err(|_| ScreenCaptureKitError::ActorUnavailable)?;
            response_rx
                .recv()
                .map_err(|_| ScreenCaptureKitError::ActorUnavailable)?
        }

        pub(crate) fn shutdown(&mut self) {
            let _ = self.command_tx.try_send(ActorCommand::Shutdown);
        }
    }

    fn actor_loop(
        receiver: Receiver<ActorCommand>,
        actor_tx: SyncSender<ActorCommand>,
        status: Arc<ActorStatus>,
    ) {
        let mut capture: Option<NativeCapture> = None;
        let mut pending_start: Option<(NativeCapture, u64, u64)> = None;
        let mut timed_out_stop: Option<(u64, u64)> = None;
        let mut cleanup_pending = false;
        let mut shutdown_requested = false;
        let mut reducer = NativeTransitionReducer::new();
        while let Ok(command) = receiver.recv() {
            match command {
                ActorCommand::Start {
                    request,
                    capture_tx,
                    encoder_tx,
                    diagnostics,
                    generation,
                    operation_id,
                    response,
                } => {
                    if capture.is_some()
                        || pending_start.is_some()
                        || timed_out_stop.is_some()
                        || cleanup_pending
                        || shutdown_requested
                        || !reducer.begin_start(generation, operation_id)
                    {
                        let _ = response.send(Err(ScreenCaptureKitError::StreamSetupFailed));
                        continue;
                    }
                    status.clear_failure();
                    status.set(ACTOR_STARTING);
                    match start_capture(
                        &request,
                        capture_tx,
                        encoder_tx,
                        actor_tx.clone(),
                        diagnostics,
                        generation,
                        operation_id,
                    ) {
                        Ok(StartAttempt::Complete(new_capture)) => {
                            capture = Some(new_capture);
                            let _ = reducer.complete_start(generation, operation_id, true);
                            status.set(ACTOR_RUNNING);
                            let _ = response.send(Ok(()));
                        }
                        Ok(StartAttempt::Pending(new_capture)) => {
                            pending_start = Some((new_capture, generation, operation_id));
                            reducer.start_timed_out();
                            status.set(ACTOR_FAILED);
                            let _ = response.send(Err(ScreenCaptureKitError::StreamStartTimedOut));
                        }
                        Ok(StartAttempt::Failed(new_capture, error)) => {
                            if let Err(stop_error) = stop_capture(
                                &new_capture,
                                actor_tx.clone(),
                                generation,
                                operation_id,
                            ) {
                                capture = Some(new_capture);
                                cleanup_pending = true;
                                reducer.complete_start(generation, operation_id, false);
                                reducer.mark_cleanup_pending(generation);
                                status.cleanup_pending(
                                    generation,
                                    format!(
                                        "ScreenCaptureKit start failed ({error}); stop is pending: {stop_error}"
                                    ),
                                );
                            } else {
                                reducer.complete_start(generation, operation_id, false);
                                status.set(ACTOR_IDLE);
                            }
                            let _ = response.send(Err(error));
                        }
                        Err(error) => {
                            reducer.complete_start(generation, operation_id, false);
                            status.set(ACTOR_IDLE);
                            let _ = response.send(Err(error));
                        }
                    }
                }
                ActorCommand::StartCompleted {
                    generation,
                    operation_id,
                    result,
                } => {
                    let Some((new_capture, pending_generation, pending_operation)) =
                        pending_start.take()
                    else {
                        continue;
                    };
                    if generation != pending_generation || operation_id != pending_operation {
                        pending_start = Some((new_capture, pending_generation, pending_operation));
                        continue;
                    }
                    if !reducer.complete_start(generation, operation_id, result.is_ok()) {
                        pending_start = Some((new_capture, pending_generation, pending_operation));
                        continue;
                    }
                    match result {
                        Ok(()) => {
                            capture = Some(new_capture);
                            if shutdown_requested {
                                let _ = reducer.begin_stop(generation, 0);
                                let active_capture = capture.as_ref().expect("capture was stored");
                                match stop_capture(active_capture, actor_tx.clone(), generation, 0)
                                {
                                    Ok(()) => {
                                        capture = None;
                                        let _ = reducer.complete_stop(generation, 0, true, true);
                                        status.set(ACTOR_IDLE);
                                        break;
                                    }
                                    Err(error) => {
                                        if error == ScreenCaptureKitError::StreamStopTimedOut {
                                            timed_out_stop = Some((generation, 0));
                                            reducer.stop_timed_out();
                                            status.set(ACTOR_STOPPING);
                                        } else if error.is_cleanup_failure() {
                                            cleanup_pending = true;
                                            let _ =
                                                reducer.complete_stop(generation, 0, true, false);
                                            status.cleanup_pending(
                                                generation,
                                                format!(
                                                    "ScreenCaptureKit output cleanup failed: {error}"
                                                ),
                                            );
                                        } else {
                                            let _ =
                                                reducer.complete_stop(generation, 0, false, true);
                                            status.fail_generation(generation, error.to_string());
                                        }
                                    }
                                }
                            } else {
                                status.set(ACTOR_RUNNING);
                            }
                        }
                        Err(error) => {
                            if let Err(stop_error) = stop_capture(
                                &new_capture,
                                actor_tx.clone(),
                                generation,
                                operation_id,
                            ) {
                                capture = Some(new_capture);
                                status.fail_generation(
                                    generation,
                                    format!(
                                        "ScreenCaptureKit stream start failed ({error}); stop is pending: {stop_error}"
                                    ),
                                );
                                reducer.mark_cleanup_pending(generation);
                                cleanup_pending = true;
                            } else {
                                status.set(ACTOR_IDLE);
                                if shutdown_requested {
                                    break;
                                }
                            }
                        }
                    }
                }
                ActorCommand::Stop {
                    generation,
                    operation_id,
                    response,
                } => {
                    if timed_out_stop.is_some() || pending_start.is_some() {
                        let _ = response.send(Err(ScreenCaptureKitError::OperationPending));
                        continue;
                    }
                    let Some(active_capture) = capture.as_ref() else {
                        status.set(ACTOR_IDLE);
                        let _ = response.send(Ok(()));
                        continue;
                    };
                    if active_capture.generation != generation {
                        let _ = response.send(Err(ScreenCaptureKitError::StaleGeneration));
                        continue;
                    }
                    if !reducer.begin_stop(generation, operation_id) {
                        let _ = response.send(Err(ScreenCaptureKitError::OperationPending));
                        continue;
                    }
                    let was_failed = matches!(
                        status.get(),
                        super::CaptureLifecycle::Failed | super::CaptureLifecycle::CleanupPending
                    );
                    cleanup_pending = false;
                    status.set(ACTOR_STOPPING);
                    match stop_capture(active_capture, actor_tx.clone(), generation, operation_id) {
                        Ok(()) => {
                            capture = None;
                            let _ = reducer.complete_stop(generation, operation_id, true, true);
                            status.set(ACTOR_IDLE);
                            let _ = response.send(Ok(()));
                        }
                        Err(error) => {
                            if error == ScreenCaptureKitError::StreamStopTimedOut {
                                timed_out_stop = Some((generation, operation_id));
                                reducer.stop_timed_out();
                                status.set(ACTOR_STOPPING);
                            } else if error.is_cleanup_failure() {
                                cleanup_pending = true;
                                let _ =
                                    reducer.complete_stop(generation, operation_id, true, false);
                                status.cleanup_pending(
                                    generation,
                                    format!("ScreenCaptureKit output cleanup failed: {error}"),
                                );
                            } else {
                                let _ =
                                    reducer.complete_stop(generation, operation_id, false, true);
                                status.set(if was_failed {
                                    ACTOR_FAILED
                                } else {
                                    ACTOR_RUNNING
                                });
                            }
                            let _ = response.send(Err(error));
                        }
                    }
                }
                ActorCommand::StopCompleted {
                    generation,
                    operation_id,
                    result,
                } => {
                    if timed_out_stop != Some((generation, operation_id)) {
                        continue;
                    }
                    timed_out_stop = None;
                    let Some(active_capture) = capture.as_ref() else {
                        continue;
                    };
                    match result {
                        Ok(()) => {
                            if let Err(error) = remove_stream_output(active_capture) {
                                cleanup_pending = true;
                                let _ =
                                    reducer.complete_stop(generation, operation_id, true, false);
                                status.cleanup_pending(
                                    generation,
                                    format!(
                                        "ScreenCaptureKit output cleanup failed after stopping: {error}"
                                    ),
                                );
                            } else {
                                capture = None;
                                cleanup_pending = false;
                                let _ = reducer.complete_stop(generation, operation_id, true, true);
                                status.set(ACTOR_IDLE);
                                if shutdown_requested {
                                    break;
                                }
                            }
                        }
                        Err(error) => {
                            let was_failed = reducer.stop_was_from_failed(generation, operation_id);
                            let _ = reducer.complete_stop(generation, operation_id, false, true);
                            if was_failed || shutdown_requested {
                                status.fail_generation(generation, error.to_string());
                            } else {
                                status.set(ACTOR_RUNNING);
                            }
                        }
                    }
                }
                ActorCommand::Terminated { generation, detail } => {
                    let reducer_matches = reducer.terminate(generation);
                    if let Some((pending_capture, pending_generation, pending_operation)) =
                        pending_start.take()
                    {
                        if pending_generation != generation || !reducer_matches {
                            pending_start =
                                Some((pending_capture, pending_generation, pending_operation));
                            continue;
                        }
                        // didStopWithError is not an ownership handoff. Keep
                        // the stream until an explicit stop has completed.
                        capture = Some(pending_capture);
                        cleanup_pending = false;
                        status.fail_generation(generation, detail);
                        continue;
                    }
                    if reducer_matches
                        && capture
                            .as_ref()
                            .is_some_and(|active_capture| active_capture.generation == generation)
                    {
                        timed_out_stop = None;
                        cleanup_pending = false;
                        status.fail_generation(generation, detail);
                    }
                }
                ActorCommand::Shutdown => {
                    shutdown_requested = true;
                    reducer.request_shutdown();
                    if pending_start.is_some() {
                        continue;
                    }
                    if let Some(active_capture) = capture.as_ref() {
                        let generation = active_capture.generation;
                        if !reducer.begin_stop(generation, 0) {
                            continue;
                        }
                        match stop_capture(active_capture, actor_tx.clone(), generation, 0) {
                            Ok(()) => {
                                capture = None;
                                let _ = reducer.complete_stop(generation, 0, true, true);
                                status.set(ACTOR_IDLE);
                                break;
                            }
                            Err(error) => {
                                if error == ScreenCaptureKitError::StreamStopTimedOut {
                                    timed_out_stop = Some((generation, 0));
                                    reducer.stop_timed_out();
                                    status.set(ACTOR_STOPPING);
                                } else if error.is_cleanup_failure() {
                                    cleanup_pending = true;
                                    let _ = reducer.complete_stop(generation, 0, true, false);
                                    status.cleanup_pending(
                                        generation,
                                        format!("ScreenCaptureKit output cleanup failed: {error}"),
                                    );
                                } else {
                                    let was_failed = matches!(
                                        status.get(),
                                        super::CaptureLifecycle::Failed
                                            | super::CaptureLifecycle::CleanupPending
                                    );
                                    let _ = reducer.complete_stop(generation, 0, false, true);
                                    if was_failed {
                                        status.fail_generation(generation, error.to_string());
                                    } else {
                                        status.set(ACTOR_RUNNING);
                                    }
                                }
                                continue;
                            }
                        }
                    } else {
                        status.set(ACTOR_IDLE);
                        break;
                    }
                }
            }
        }
        if let Some(active_capture) = capture.take() {
            // The command channel disappeared while native ownership was
            // still held. Keep retrying native stop rather than dropping a
            // possibly-live SCStream.
            loop {
                match stop_capture(
                    &active_capture,
                    actor_tx.clone(),
                    active_capture.generation,
                    0,
                ) {
                    Ok(()) => {
                        status.set(ACTOR_IDLE);
                        break;
                    }
                    Err(error) => {
                        status.cleanup_pending(
                            active_capture.generation,
                            format!("ScreenCaptureKit final stop is pending: {error}"),
                        );
                        thread::sleep(Duration::from_millis(100));
                    }
                }
            }
        } else {
            status.set(ACTOR_IDLE);
        }
    }

    pub(crate) fn enumerate_sources() -> Result<Vec<NativeCaptureSource>, ScreenCaptureKitError> {
        // Never call getShareableContent here: on current macOS it pops the
        // lock-dialog TCC even when Screen Recording is already enabled.
        Ok(Vec::new())
    }

    pub(crate) fn enumerate_running_apps() -> Result<Vec<crate::media::types::NativeRunningApp>, ScreenCaptureKitError> {
        // NSWorkspace runningApplications carries name, pid, and bundle id so
        // audio exclusion can match helper processes (e.g. Discord). The
        // window list remains a fallback for names/pids. Neither path calls
        // getShareableContent, which can pop the TCC lock dialog.
        let apps = super::running_apps_from_workspace();
        if apps.is_empty() {
            Ok(super::running_apps_from_window_list())
        } else {
            Ok(apps)
        }
    }

    fn is_tcc_denied(error: &ScreenCaptureKitError) -> bool {
        matches!(
            error,
            ScreenCaptureKitError::NativeFailure { code: -3801, .. }
                | ScreenCaptureKitError::ScreenRecordingNotGranted
        )
    }

    fn pick_filter_with_system_picker(
        request: &CreateMediaSessionRequest,
    ) -> Result<(Retained<SCContentFilter>, (u32, u32)), ScreenCaptureKitError> {
        let (tx, rx) = sync_channel(1);
        let observer = PickerObserver::new(tx);
        let style = match request.source {
            CaptureSource::Window => SCShareableContentStyle::Window,
            _ => SCShareableContentStyle::Display,
        };
        // Never exec_sync onto main from the media worker: the UI command may
        // be waiting on this thread, and picker callbacks need the run loop.
        DispatchQueue::main().exec_async({
            let observer = observer.clone();
            move || {
                let picker = unsafe { SCContentSharingPicker::sharedPicker() };
                unsafe {
                    picker.setActive(true);
                    picker.addObserver(ProtocolObject::from_ref(&*observer));
                    picker.presentPickerUsingContentStyle(style);
                }
            }
        });
        let event = rx
            .recv_timeout(PICKER_TIMEOUT)
            .map_err(|_| ScreenCaptureKitError::StreamStartTimedOut)?;
        DispatchQueue::main().exec_async({
            let observer = observer.clone();
            move || {
                let picker = unsafe { SCContentSharingPicker::sharedPicker() };
                unsafe {
                    picker.removeObserver(ProtocolObject::from_ref(&*observer));
                }
            }
        });
        match event {
            PickerEvent::Selected(SendFilter(filter)) => {
                let (requested_width, requested_height) = request.capture_cap();
                Ok((filter, (requested_width, requested_height)))
            }
            PickerEvent::Cancelled => Err(ScreenCaptureKitError::PickerCancelled),
            PickerEvent::Failed(detail) => Err(ScreenCaptureKitError::NativeFailure {
                operation: "presentPicker",
                domain: "SCContentSharingPicker".into(),
                code: 0,
                detail,
            }),
        }
    }

    pub(crate) fn start_capture(
        request: &CreateMediaSessionRequest,
        capture_tx: SyncSender<NativeFrame>,
        encoder_tx: SyncSender<EncoderCommand>,
        actor_tx: SyncSender<ActorCommand>,
        diagnostics: Arc<PreviewDiagnostics>,
        generation: u64,
        operation_id: u64,
    ) -> Result<StartAttempt, ScreenCaptureKitError> {
        let (filter, source_dimensions) = pick_filter_with_system_picker(request)?;
        let configuration = stream_configuration(request, source_dimensions);
        eprintln!(
            "[goDrinking] starting ScreenCaptureKit source={:?} source_id={:?} dimensions={}x{}",
            request.source, request.source_id, source_dimensions.0, source_dimensions.1
        );
        let output = StreamOutput::new_with_actor(
            capture_tx,
            encoder_tx,
            actor_tx.clone(),
            diagnostics,
            generation,
        );
        let delegate: &ProtocolObject<dyn SCStreamDelegate> = ProtocolObject::from_ref(&*output);
        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &configuration,
                Some(delegate),
            )
        };
        let sample_queue = DispatchQueue::new("com.godrinking.screen-capture-samples", None);
        unsafe {
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    ProtocolObject::from_ref(&*output),
                    SCStreamOutputType::Screen,
                    Some(&sample_queue),
                )
                .map_err(|error| native_error("addStreamOutput", &error))?;
        }

        let (response_tx, response_rx) = sync_channel(1);
        let completion_actor_tx = actor_tx.clone();
        let completion = RcBlock::new(move |error: *mut NSError| {
            let result = if error.is_null() {
                Ok(())
            } else {
                let error = native_error("startCapture", unsafe { &*error });
                eprintln!("[goDrinking] ScreenCaptureKit start failed: {error}");
                Err(error)
            };
            let _ = response_tx.send(result.clone());
            let _ = completion_actor_tx.try_send(ActorCommand::StartCompleted {
                generation,
                operation_id,
                result: result.clone(),
            });
        });
        unsafe { stream.startCaptureWithCompletionHandler(Some(&completion)) };
        let capture = NativeCapture {
            generation,
            stream,
            _output: output,
            _sample_queue: sample_queue,
        };
        match response_rx.recv_timeout(CALLBACK_TIMEOUT) {
            Ok(Ok(())) => Ok(StartAttempt::Complete(capture)),
            Ok(Err(error)) => Ok(StartAttempt::Failed(capture, error)),
            Err(_) => Ok(StartAttempt::Pending(capture)),
        }
    }

    pub(crate) fn stop_capture(
        capture: &NativeCapture,
        actor_tx: SyncSender<ActorCommand>,
        generation: u64,
        operation_id: u64,
    ) -> Result<(), ScreenCaptureKitError> {
        let (response_tx, response_rx) = sync_channel(1);
        let completion = RcBlock::new(move |error: *mut NSError| {
            let result = if error.is_null() {
                Ok(())
            } else {
                Err(native_error("stopCapture", unsafe { &*error }))
            };
            let _ = response_tx.send(result.clone());
            let _ = actor_tx.try_send(ActorCommand::StopCompleted {
                generation,
                operation_id,
                result: result.clone(),
            });
        });
        unsafe {
            capture
                .stream
                .stopCaptureWithCompletionHandler(Some(&completion))
        };
        let result = response_rx
            .recv_timeout(CALLBACK_TIMEOUT)
            .map_err(|_| ScreenCaptureKitError::StreamStopTimedOut)?;
        result?;
        unsafe {
            capture.stream.removeStreamOutput_type_error(
                ProtocolObject::from_ref(&*capture._output),
                SCStreamOutputType::Screen,
            )
        }
        .map_err(|error| native_error("removeStreamOutput", &error))?;
        capture._sample_queue.barrier_sync(|| {});
        Ok(())
    }

    pub(crate) enum StartAttempt {
        Complete(NativeCapture),
        Failed(NativeCapture, ScreenCaptureKitError),
        Pending(NativeCapture),
    }

    /// Removes the callback only after `stopCapture` has completed.
    fn remove_stream_output(capture: &NativeCapture) -> Result<(), ScreenCaptureKitError> {
        unsafe {
            capture.stream.removeStreamOutput_type_error(
                ProtocolObject::from_ref(&*capture._output),
                SCStreamOutputType::Screen,
            )
        }
        .map_err(|error| native_error("removeStreamOutput", &error))?;
        capture._sample_queue.barrier_sync(|| {});
        Ok(())
    }

    fn native_error(operation: &'static str, error: &NSError) -> ScreenCaptureKitError {
        ScreenCaptureKitError::NativeFailure {
            operation,
            domain: error.domain().to_string(),
            code: error.code() as isize,
            detail: error.localizedDescription().to_string(),
        }
    }

    fn shareable_content() -> Result<Retained<SCShareableContent>, ScreenCaptureKitError> {
        let (response_tx, response_rx) = sync_channel(1);
        let completion = RcBlock::new(
            move |content: *mut SCShareableContent, error: *mut NSError| {
                let result = if !error.is_null() {
                    Err(native_error("getShareableContent", unsafe { &*error }))
                } else if content.is_null() {
                    Err(ScreenCaptureKitError::SourceEnumerationFailed)
                } else {
                    unsafe { Retained::retain(content) }
                        .ok_or(ScreenCaptureKitError::SourceEnumerationFailed)
                };
                let _ = response_tx.send(result);
            },
        );
        unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&completion) };
        response_rx
            .recv_timeout(CALLBACK_TIMEOUT)
            .map_err(|_| ScreenCaptureKitError::SourceEnumerationTimedOut)?
    }

    pub(crate) fn request_shareable_content_probe(
        response_tx: SyncSender<Result<(), ScreenCaptureKitError>>,
    ) {
        let completion = RcBlock::new(
            move |content: *mut SCShareableContent, error: *mut NSError| {
                let result = if !error.is_null() {
                    Err(native_error("getShareableContent", unsafe { &*error }))
                } else if content.is_null() {
                    Err(ScreenCaptureKitError::SourceEnumerationFailed)
                } else {
                    Ok(())
                };
                let _ = response_tx.send(result);
            },
        );
        unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&completion) };
    }

    fn source_dimensions(
        content: &SCShareableContent,
        request: &CreateMediaSessionRequest,
    ) -> Result<(u32, u32), ScreenCaptureKitError> {
        let requested_id = request.source_id;
        match request.source {
            CaptureSource::Screen => {
                let displays = unsafe { content.displays() };
                let display = (0..displays.count())
                    .map(|index| displays.objectAtIndex(index))
                    .find(|display| {
                        requested_id.map_or(true, |id| unsafe { display.displayID() as u64 == id })
                    })
                    .ok_or(ScreenCaptureKitError::NoMatchingSource)?;
                let width = unsafe { display.width() }.max(1) as u32;
                let height = unsafe { display.height() }.max(1) as u32;
                Ok((width, height))
            }
            CaptureSource::Window => {
                let windows = unsafe { content.windows() };
                let window = (0..windows.count())
                    .map(|index| windows.objectAtIndex(index))
                    .find(|window| {
                        requested_id.map_or(true, |id| unsafe { window.windowID() as u64 == id })
                    })
                    .ok_or(ScreenCaptureKitError::NoMatchingSource)?;
                let frame = unsafe { window.frame() };
                let width = frame.size.width.max(1.0).round() as u32;
                let height = frame.size.height.max(1.0).round() as u32;
                Ok((width, height))
            }
            CaptureSource::Game => Err(ScreenCaptureKitError::UnsupportedSource),
        }
    }

    fn content_filter(
        content: &SCShareableContent,
        request: &CreateMediaSessionRequest,
    ) -> Result<Retained<SCContentFilter>, ScreenCaptureKitError> {
        let requested_id = request.source_id;
        match request.source {
            CaptureSource::Screen => {
                let displays = unsafe { content.displays() };
                let display = (0..displays.count())
                    .map(|index| displays.objectAtIndex(index))
                    .find(|display| {
                        requested_id.map_or(true, |id| unsafe { display.displayID() as u64 == id })
                    })
                    .ok_or(ScreenCaptureKitError::NoMatchingSource)?;
                let excluded_windows = NSArray::<SCWindow>::new();
                Ok(unsafe {
                    SCContentFilter::initWithDisplay_excludingWindows(
                        SCContentFilter::alloc(),
                        &display,
                        &excluded_windows,
                    )
                })
            }
            CaptureSource::Window => {
                let windows = unsafe { content.windows() };
                let window = (0..windows.count())
                    .map(|index| windows.objectAtIndex(index))
                    .find(|window| {
                        requested_id.map_or(true, |id| unsafe { window.windowID() as u64 == id })
                    })
                    .ok_or(ScreenCaptureKitError::NoMatchingSource)?;
                Ok(unsafe {
                    SCContentFilter::initWithDesktopIndependentWindow(
                        SCContentFilter::alloc(),
                        &window,
                    )
                })
            }
            CaptureSource::Game => Err(ScreenCaptureKitError::UnsupportedSource),
        }
    }

    fn stream_configuration(
        request: &CreateMediaSessionRequest,
        source_dimensions: (u32, u32),
    ) -> Retained<SCStreamConfiguration> {
        let configuration = unsafe { SCStreamConfiguration::new() };
        let (requested_width, requested_height) = request.capture_cap();
        let (width, height) = super::super::types::fitted_even_size(
            source_dimensions.0,
            source_dimensions.1,
            requested_width,
            requested_height,
        );
        let interval = match request.effective_frame_rate() {
            FrameRate::Fps60 => unsafe { CMTime::new(1, 60) },
            FrameRate::Fps30 => unsafe { CMTime::new(1, 30) },
        };
        unsafe {
            configuration.setWidth(width as usize);
            configuration.setHeight(height as usize);
            configuration.setPixelFormat(kCVPixelFormatType_32BGRA);
            configuration.setMinimumFrameInterval(interval);
            configuration.setQueueDepth(3);
            configuration.setCapturesAudio(false);
        }
        configuration
    }

    fn thumbnail_rgb(
        base_address: *mut std::ffi::c_void,
        width: usize,
        height: usize,
        bytes_per_row: usize,
        output_width: u32,
        output_height: u32,
    ) -> Option<Vec<u8>> {
        if base_address.is_null()
            || width == 0
            || height == 0
            || bytes_per_row < width.checked_mul(4)?
        {
            return None;
        }
        let byte_len = bytes_per_row.checked_mul(height)?;
        let source = unsafe { slice::from_raw_parts(base_address.cast::<u8>(), byte_len) };
        let mut output = vec![
            0_u8;
            (output_width as usize)
                .checked_mul(output_height as usize)?
                .checked_mul(3)?
        ];
        for y in 0..output_height as usize {
            let source_y = y * height / output_height as usize;
            for x in 0..output_width as usize {
                let source_x = x * width / output_width as usize;
                let source_offset = source_y * bytes_per_row + source_x * 4;
                let output_offset = (y * output_width as usize + x) * 3;
                output[output_offset] = source[source_offset + 2];
                output[output_offset + 1] = source[source_offset + 1];
                output[output_offset + 2] = source[source_offset];
            }
        }
        Some(output)
    }

    fn sample_timestamp_micros(sample_buffer: &CMSampleBuffer) -> Option<u64> {
        let time: CMTime = unsafe { sample_buffer.presentation_time_stamp() };
        if time.timescale <= 0 || time.value < 0 {
            return None;
        }
        u64::try_from((time.value as u128).checked_mul(1_000_000)? / time.timescale as u128).ok()
    }

    fn monotonic_micros() -> u64 {
        MONOTONIC_START
            .get_or_init(Instant::now)
            .elapsed()
            .as_micros()
            .min(u64::MAX as u128) as u64
    }
}

/// Result of the non-interactive Screen Recording permission check.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenRecordingAuthorization {
    Granted,
    NotGranted,
    Unsupported,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ScreenCaptureKitAvailability {
    pub framework_available: bool,
    pub authorization: ScreenRecordingAuthorization,
    pub source_enumeration_available: bool,
    pub detail: String,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScreenCaptureKitError {
    UnsupportedPlatform,
    ScreenRecordingNotGranted,
    SourceEnumerationFailed,
    SourceEnumerationTimedOut,
    NoMatchingSource,
    UnsupportedSource,
    StaleGeneration,
    StreamSetupFailed,
    NativeFailure {
        operation: &'static str,
        domain: String,
        code: isize,
        detail: String,
    },
    StreamStartTimedOut,
    StreamStopTimedOut,
    #[cfg(not(target_os = "macos"))]
    NoActiveCapture,
    OperationPending,
    ActorUnavailable,
    PickerCancelled,
}

impl Display for ScreenCaptureKitError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::UnsupportedPlatform => "ScreenCaptureKit is unsupported on this platform",
            Self::ScreenRecordingNotGranted => "Screen Recording authorization is not granted",
            Self::SourceEnumerationFailed => "ScreenCaptureKit source enumeration failed",
            Self::SourceEnumerationTimedOut => "ScreenCaptureKit source enumeration timed out",
            Self::NoMatchingSource => "the requested ScreenCaptureKit source was not found",
            Self::UnsupportedSource => "the requested capture source is unsupported",
            Self::StaleGeneration => "the ScreenCaptureKit operation belongs to an old session",
            Self::StreamSetupFailed => "ScreenCaptureKit stream output setup failed",
            Self::NativeFailure {
                operation,
                domain,
                code,
                detail,
            } => {
                return write!(
                    formatter,
                    "ScreenCaptureKit {operation} failed ({domain}:{code}): {detail}"
                )
            }
            Self::StreamStartTimedOut => "ScreenCaptureKit stream start timed out",
            Self::StreamStopTimedOut => "ScreenCaptureKit stream stop timed out",
            #[cfg(not(target_os = "macos"))]
            Self::NoActiveCapture => "ScreenCaptureKit has no active capture",
            Self::OperationPending => "ScreenCaptureKit has an operation or cleanup pending",
            Self::ActorUnavailable => "ScreenCaptureKit capture actor is unavailable",
            Self::PickerCancelled => "screen picker was cancelled",
        };
        formatter.write_str(message)
    }
}

impl ScreenCaptureKitError {
    fn is_cleanup_failure(&self) -> bool {
        matches!(
            self,
            Self::NativeFailure {
                operation: "removeStreamOutput",
                ..
            }
        )
    }
}

pub struct ScreenCaptureKitAdapter {
    availability: ScreenCaptureKitAvailability,
    lifecycle: CaptureLifecycle,
    #[cfg(target_os = "macos")]
    actor: native::NativeCaptureActor,
    #[cfg(target_os = "macos")]
    _framework_marker: PhantomData<fn() -> objc2_screen_capture_kit::SCShareableContent>,
    #[cfg(not(target_os = "macos"))]
    _framework_marker: PhantomData<fn()>,
}

impl ScreenCaptureKitAdapter {
    pub fn new() -> Self {
        Self {
            availability: detect_availability(),
            lifecycle: CaptureLifecycle::Idle,
            #[cfg(target_os = "macos")]
            actor: native::NativeCaptureActor::spawn(),
            _framework_marker: PhantomData,
        }
    }

    pub fn availability(&self) -> ScreenCaptureKitAvailability {
        self.availability.clone()
    }

    pub(crate) fn failure_detail(&self) -> Option<String> {
        #[cfg(target_os = "macos")]
        {
            return self.actor.failure_detail();
        }
        #[cfg(not(target_os = "macos"))]
        {
            None
        }
    }

    #[allow(dead_code)]
    pub fn lifecycle(&self) -> CaptureLifecycle {
        #[cfg(target_os = "macos")]
        {
            self.actor.lifecycle()
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.lifecycle
        }
    }

    pub(crate) fn enumerate_sources(
        &self,
    ) -> Result<Vec<NativeCaptureSource>, ScreenCaptureKitError> {
        if !self.availability.framework_available {
            return Err(ScreenCaptureKitError::UnsupportedPlatform);
        }
        if !self.availability.source_enumeration_available {
            return Err(ScreenCaptureKitError::ScreenRecordingNotGranted);
        }
        #[cfg(target_os = "macos")]
        {
            return native::enumerate_sources();
        }
        #[cfg(not(target_os = "macos"))]
        unreachable!("unsupported platforms return above")
    }

    pub(crate) fn enumerate_running_apps(
        &self,
    ) -> Result<Vec<crate::media::types::NativeRunningApp>, ScreenCaptureKitError> {
        if !self.availability.source_enumeration_available {
            return Err(ScreenCaptureKitError::ScreenRecordingNotGranted);
        }
        #[cfg(target_os = "macos")]
        {
            return native::enumerate_running_apps();
        }
        #[cfg(not(target_os = "macos"))]
        Ok(Vec::new())
    }

    pub(crate) fn request_permission(
        &mut self,
        app: &tauri::AppHandle,
    ) -> ScreenCaptureKitAvailability {
        #[cfg(target_os = "macos")]
        {
            self.availability = detect_availability();
            if self.availability.authorization != ScreenRecordingAuthorization::Granted {
                let _ = std::process::Command::new("open")
                    .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")
                    .spawn();
                self.availability = availability_with_status(
                    ScreenRecordingAuthorization::NotGranted,
                    "Screen Recording is off. Enable goDrinking in System Settings → Privacy & Security → Screen Recording, then quit and reopen the app.",
                );
            }
        }
        self.availability.clone()
    }

    pub(crate) fn probe_permission(
        &mut self,
        app: &tauri::AppHandle,
    ) -> ScreenCaptureKitAvailability {
        #[cfg(target_os = "macos")]
        {
            // Startup and focus refreshes must remain non-interactive.
            // SCShareableContent can trigger TCC UI on newer macOS releases;
            // only the explicit permission command may invoke that probe.
            self.availability = probe_core_graphics_access_on_main(app, &self.availability);
        }
        self.availability.clone()
    }

    pub(crate) fn start_capture(
        &mut self,
        request: &CreateMediaSessionRequest,
        capture_tx: SyncSender<NativeFrame>,
        encoder_tx: SyncSender<EncoderCommand>,
        diagnostics: Arc<PreviewDiagnostics>,
        generation: u64,
    ) -> Result<(), ScreenCaptureKitError> {
        if !self.availability.framework_available {
            return Err(ScreenCaptureKitError::UnsupportedPlatform);
        }
        if !self.availability.source_enumeration_available {
            return Err(ScreenCaptureKitError::ScreenRecordingNotGranted);
        }
        if self.lifecycle() != CaptureLifecycle::Idle
            && self.lifecycle() != CaptureLifecycle::Failed
        {
            return Err(ScreenCaptureKitError::StreamSetupFailed);
        }
        self.lifecycle = CaptureLifecycle::Starting;
        #[cfg(target_os = "macos")]
        {
            match self
                .actor
                .start(request, capture_tx, encoder_tx, diagnostics, generation)
            {
                Ok(()) => {
                    self.lifecycle = CaptureLifecycle::Running;
                    Ok(())
                }
                Err(error) => {
                    self.lifecycle = self.actor.lifecycle();
                    Err(error)
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        unreachable!("unsupported platforms return above")
    }

    pub(crate) fn stop_capture(&mut self, generation: u64) -> Result<(), ScreenCaptureKitError> {
        #[cfg(target_os = "macos")]
        {
            match self.actor.stop(generation) {
                Ok(()) => {
                    self.lifecycle = CaptureLifecycle::Idle;
                    Ok(())
                }
                Err(error) => {
                    self.lifecycle = self.actor.lifecycle();
                    Err(error)
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            if self.lifecycle == CaptureLifecycle::Idle {
                Ok(())
            } else {
                self.lifecycle = CaptureLifecycle::Failed;
                Err(ScreenCaptureKitError::NoActiveCapture)
            }
        }
    }
}

#[cfg(target_os = "macos")]
impl Drop for ScreenCaptureKitAdapter {
    fn drop(&mut self) {
        self.actor.shutdown();
    }
}

impl Default for ScreenCaptureKitAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_os = "macos")]
fn availability_with_status(
    authorization: ScreenRecordingAuthorization,
    detail: impl Into<String>,
) -> ScreenCaptureKitAvailability {
    ScreenCaptureKitAvailability {
        framework_available: true,
        source_enumeration_available: authorization == ScreenRecordingAuthorization::Granted,
        authorization,
        detail: detail.into(),
    }
}

#[cfg(target_os = "macos")]
fn unavailable_after_probe(
    previous: &ScreenCaptureKitAvailability,
    detail: impl Into<String>,
) -> ScreenCaptureKitAvailability {
    let mut availability = previous.clone();
    availability.detail = detail.into();
    availability
}

#[cfg(target_os = "macos")]
fn probe_shareable_content_on_main(
    app: &tauri::AppHandle,
    previous: &ScreenCaptureKitAvailability,
) -> ScreenCaptureKitAvailability {
    use std::sync::mpsc::sync_channel;

    let (response_tx, response_rx) = sync_channel(1);
    if let Err(error) = app.run_on_main_thread(move || {
        native::request_shareable_content_probe(response_tx);
    }) {
        return unavailable_after_probe(
            previous,
            format!("Screen Recording probe could not run on the Tauri main thread: {error}"),
        );
    }

    match response_rx.recv_timeout(native::CALLBACK_TIMEOUT) {
        Ok(Ok(())) => availability_with_status(
            ScreenRecordingAuthorization::Granted,
            "ScreenCaptureKit returned shareable content; Screen Recording authorization is granted and source enumeration is available.",
        ),
        Ok(Err(error)) => availability_with_status(
            ScreenRecordingAuthorization::NotGranted,
            format!(
                "ScreenCaptureKit shareable-content probe completed without authorization: {error}"
            ),
        ),
        Err(_) => unavailable_after_probe(
            previous,
            "ScreenCaptureKit shareable-content probe timed out; the previous authorization status was preserved.",
        ),
    }
}

#[cfg(target_os = "macos")]
fn request_core_graphics_access_on_main(
    app: &tauri::AppHandle,
    previous: &ScreenCaptureKitAvailability,
) -> ScreenCaptureKitAvailability {
    use std::sync::mpsc::sync_channel;

    let (response_tx, response_rx) = sync_channel(1);
    if let Err(error) = app.run_on_main_thread(move || {
        let granted = objc2_core_graphics::CGRequestScreenCaptureAccess();
        let _ = response_tx.send(granted);
    }) {
        return unavailable_after_probe(
            previous,
            format!("Screen Recording request could not run on the Tauri main thread: {error}"),
        );
    }

    match response_rx.recv_timeout(native::CALLBACK_TIMEOUT) {
        Ok(_request_result) => detect_availability(),
        Err(_) => unavailable_after_probe(
            previous,
            "Screen Recording request timed out; the previous authorization status was preserved.",
        ),
    }
}

#[cfg(target_os = "macos")]
fn probe_core_graphics_access_on_main(
    app: &tauri::AppHandle,
    previous: &ScreenCaptureKitAvailability,
) -> ScreenCaptureKitAvailability {
    use std::sync::mpsc::sync_channel;

    let (response_tx, response_rx) = sync_channel(1);
    if let Err(error) = app.run_on_main_thread(move || {
        let granted = screen_recording_is_granted();
        let _ = response_tx.send(granted);
    }) {
        return unavailable_after_probe(
            previous,
            format!("Screen Recording probe could not run on the Tauri main thread: {error}"),
        );
    }

    match response_rx.recv_timeout(native::CALLBACK_TIMEOUT) {
        Ok(granted) => {
            if granted {
                availability_with_status(
                    ScreenRecordingAuthorization::Granted,
                    "Screen Recording is granted; source enumeration is available.",
                )
            } else {
                availability_with_status(
                    ScreenRecordingAuthorization::NotGranted,
                    "Screen Recording is not granted; source enumeration is unavailable.",
                )
            }
        }
        Err(_) => unavailable_after_probe(
            previous,
            "Screen Recording probe timed out; the previous authorization status was preserved.",
        ),
    }
}

#[cfg(target_os = "macos")]
fn screen_recording_is_granted() -> bool {
    objc2_core_graphics::CGPreflightScreenCaptureAccess() || foreign_window_titles_visible()
}

#[cfg(target_os = "macos")]
fn foreign_window_titles_visible() -> bool {
    use core_foundation::array::CFArray;
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::number::CFNumber;
    use core_foundation::string::CFString;

    const ON_SCREEN_ONLY: u32 = 1;
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGWindowListCopyWindowInfo(
            option: u32,
            relative_to_window: u32,
        ) -> core_foundation::array::CFArrayRef;
    }

    let raw = unsafe { CGWindowListCopyWindowInfo(ON_SCREEN_ONLY, 0) };
    if raw.is_null() {
        return false;
    }
    let windows: CFArray<CFDictionary<CFString, CFType>> =
        unsafe { CFArray::wrap_under_create_rule(raw) };
    let my_pid = i64::from(std::process::id());
    let name_key = CFString::new("kCGWindowName");
    let pid_key = CFString::new("kCGWindowOwnerPID");
    for window in &windows {
        let pid = window
            .find(&pid_key)
            .and_then(|value| value.downcast::<CFNumber>())
            .and_then(|number| number.to_i64())
            .unwrap_or(my_pid);
        if pid == my_pid {
            continue;
        }
        let Some(name) = window
            .find(&name_key)
            .and_then(|value| value.downcast::<CFString>())
        else {
            continue;
        };
        if !name.to_string().is_empty() {
            return true;
        }
    }
    false
}

#[cfg(target_os = "macos")]
fn running_apps_from_workspace() -> Vec<crate::media::types::NativeRunningApp> {
    use objc2_app_kit::{NSRunningApplication, NSWorkspace};
    use objc2_foundation::NSArray;

    let workspace = NSWorkspace::sharedWorkspace();
    let apps: &NSArray<NSRunningApplication> = &workspace.runningApplications();
    let mut result = Vec::with_capacity(apps.count() as usize);
    for index in 0..apps.count() {
        let app = apps.objectAtIndex(index);
        let pid = app.processIdentifier();
        if pid <= 0 {
            continue;
        }
        let name = app
            .localizedName()
            .map(|name| name.to_string())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| format!("pid {pid}"));
        let bundle_id = app.bundleIdentifier().map(|bundle| bundle.to_string());
        result.push(crate::media::types::NativeRunningApp {
            name,
            bundle_id,
            pid,
            emitting_audio: super::process_tap::pid_is_emitting_output(pid),
        });
    }
    dedupe_apps_by_pid(result)
}

#[cfg(target_os = "macos")]
fn running_apps_from_window_list() -> Vec<crate::media::types::NativeRunningApp> {
    use core_foundation::array::CFArray;
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::number::CFNumber;
    use core_foundation::string::CFString;

    const ON_SCREEN_ONLY: u32 = 1;
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGWindowListCopyWindowInfo(
            option: u32,
            relative_to_window: u32,
        ) -> core_foundation::array::CFArrayRef;
    }

    let raw = unsafe { CGWindowListCopyWindowInfo(ON_SCREEN_ONLY, 0) };
    if raw.is_null() {
        return Vec::new();
    }
    let windows: CFArray<CFDictionary<CFString, CFType>> =
        unsafe { CFArray::wrap_under_create_rule(raw) };
    let owner_key = CFString::new("kCGWindowOwnerName");
    let pid_key = CFString::new("kCGWindowOwnerPID");
    let mut apps = Vec::new();
    for window in &windows {
        let pid = window
            .find(&pid_key)
            .and_then(|value| value.downcast::<CFNumber>())
            .and_then(|number| number.to_i64())
            .unwrap_or(0);
        if pid <= 0 {
            continue;
        }
        let name = window
            .find(&owner_key)
            .and_then(|value| value.downcast::<CFString>())
            .map(|value| value.to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| format!("pid {pid}"));
        apps.push(crate::media::types::NativeRunningApp {
            name,
            bundle_id: None,
            pid: pid as i32,
            emitting_audio: super::process_tap::pid_is_emitting_output(pid as i32),
        });
    }
    dedupe_apps_by_pid(apps)
}

/// Merges duplicate pids, preferring `emitting_audio = true` when any entry
/// for the pid reports running output, and keeping the first non-empty bundle
/// id. Sorted by name for stable UI ordering.
#[cfg(target_os = "macos")]
fn dedupe_apps_by_pid(
    apps: Vec<crate::media::types::NativeRunningApp>,
) -> Vec<crate::media::types::NativeRunningApp> {
    use std::collections::HashMap;
    let mut by_pid: HashMap<i32, crate::media::types::NativeRunningApp> = HashMap::new();
    for app in apps {
        match by_pid.get_mut(&app.pid) {
            Some(existing) => {
                existing.emitting_audio = existing.emitting_audio || app.emitting_audio;
                if existing.bundle_id.is_none() {
                    existing.bundle_id = app.bundle_id;
                }
            }
            None => {
                by_pid.insert(app.pid, app);
            }
        }
    }
    let mut result: Vec<_> = by_pid.into_values().collect();
    result.sort_by(|left, right| left.name.to_ascii_lowercase().cmp(&right.name.to_ascii_lowercase()));
    result
}

#[cfg(target_os = "macos")]
fn detect_availability() -> ScreenCaptureKitAvailability {
    let authorization = if screen_recording_is_granted() {
        ScreenRecordingAuthorization::Granted
    } else {
        ScreenRecordingAuthorization::NotGranted
    };
    let source_enumeration_available = authorization == ScreenRecordingAuthorization::Granted;
    ScreenCaptureKitAvailability {
        framework_available: true,
        authorization,
        source_enumeration_available,
        detail: if source_enumeration_available {
            "ScreenCaptureKit is linked and authorized; display/window source enumeration and selected-source capture are available.".into()
        } else {
            "ScreenCaptureKit is linked, but Screen Recording authorization is not granted; source enumeration and capture are unavailable.".into()
        },
    }
}

#[cfg(not(target_os = "macos"))]
fn detect_availability() -> ScreenCaptureKitAvailability {
    ScreenCaptureKitAvailability {
        framework_available: false,
        authorization: ScreenRecordingAuthorization::Unsupported,
        source_enumeration_available: false,
        detail: "ScreenCaptureKit is only available on macOS; native source enumeration and capture are unimplemented on this platform.".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::{CaptureLifecycle, NativeTransitionReducer, ScreenCaptureKitAdapter};

    #[test]
    fn adapter_does_not_claim_a_running_stream_without_successful_start() {
        let adapter = ScreenCaptureKitAdapter::new();
        assert_eq!(adapter.lifecycle(), CaptureLifecycle::Idle);
    }

    #[test]
    fn real_reducer_retries_after_start_failure() {
        let mut reducer = NativeTransitionReducer::new();
        assert!(reducer.begin_start(1, 1));
        assert!(reducer.complete_start(1, 1, false));
        assert_eq!(reducer.lifecycle(), CaptureLifecycle::Idle);
        assert!(reducer.begin_start(1, 2));
        assert!(reducer.complete_start(1, 2, true));
        assert_eq!(reducer.lifecycle(), CaptureLifecycle::Running);
    }

    #[test]
    fn real_reducer_resolves_stop_timeout_late() {
        let mut reducer = NativeTransitionReducer::new();
        reducer.begin_start(1, 1);
        reducer.complete_start(1, 1, true);
        assert!(reducer.begin_stop(1, 2));
        reducer.stop_timed_out();
        assert_eq!(reducer.lifecycle(), CaptureLifecycle::Stopping);
        assert!(!reducer.begin_stop(1, 3));
        assert!(reducer.complete_stop(1, 2, true, true));
        assert_eq!(reducer.lifecycle(), CaptureLifecycle::Idle);
        assert!(reducer.active_generation().is_none());
    }

    #[test]
    fn real_reducer_ignores_old_termination_but_fails_current_stream() {
        let mut reducer = NativeTransitionReducer::new();
        reducer.begin_start(2, 1);
        reducer.complete_start(2, 1, true);
        assert!(!reducer.terminate(1));
        assert_eq!(reducer.lifecycle(), CaptureLifecycle::Running);
        assert_eq!(reducer.active_generation(), Some(2));
        assert!(reducer.terminate(2));
        assert_eq!(reducer.lifecycle(), CaptureLifecycle::Failed);
        assert_eq!(reducer.active_generation(), Some(2));
        assert!(reducer.begin_stop(2, 3));
        assert!(reducer.complete_stop(2, 3, true, true));
        assert_eq!(reducer.lifecycle(), CaptureLifecycle::Idle);
        assert!(reducer.active_generation().is_none());
    }

    #[test]
    fn real_reducer_handles_pending_start_termination_and_shutdown() {
        let mut reducer = NativeTransitionReducer::new();
        reducer.begin_start(1, 1);
        reducer.start_timed_out();
        reducer.request_shutdown();
        assert!(reducer.terminate(1));
        assert_eq!(reducer.lifecycle(), CaptureLifecycle::Failed);
        assert_eq!(reducer.active_generation(), Some(1));
        assert!(!reducer.can_start());
        assert!(reducer.begin_stop(1, 2));
        assert!(reducer.complete_stop(1, 2, true, true));
        assert_eq!(reducer.lifecycle(), CaptureLifecycle::Idle);
    }

    #[test]
    fn real_reducer_never_reports_running_after_cleanup_failure() {
        let mut reducer = NativeTransitionReducer::new();
        reducer.begin_start(1, 1);
        reducer.complete_start(1, 1, true);
        reducer.begin_stop(1, 2);
        assert!(reducer.complete_stop(1, 2, true, false));
        assert_eq!(reducer.lifecycle(), CaptureLifecycle::CleanupPending);
        assert!(reducer.begin_stop(1, 3));
        assert!(reducer.complete_stop(1, 3, true, true));
        assert_eq!(reducer.lifecycle(), CaptureLifecycle::Idle);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn unsupported_start_failure_is_retryable() {
        use super::super::pipeline::{EncoderCommand, NativeFrame};
        use super::super::types::{
            CaptureSource, CreateMediaSessionRequest, FrameRate, VideoResolution,
        };
        use std::sync::mpsc::sync_channel;

        let request = CreateMediaSessionRequest {
            source: CaptureSource::Screen,
            source_id: None,
            resolution: VideoResolution::P720,
            frame_rate: FrameRate::Fps30,
            system_audio: false,
            excluded_apps: Vec::new(),
            quality: super::super::types::TransmissionQuality::Low,
            bitrate_bps: None,
            min_bitrate_bps: None,
            password: String::new(),
            nickname: "Host".into(),
            admission: false,
            join_mode: super::super::types::JoinMode::Lan,
            rendezvous_url: None,
        };
        let mut adapter = ScreenCaptureKitAdapter::new();
        let (capture_tx, _capture_rx) = sync_channel::<NativeFrame>(1);
        let (encoder_tx, _encoder_rx) = sync_channel::<EncoderCommand>(1);
        assert!(adapter
            .start_capture(&request, capture_tx.clone(), encoder_tx.clone(), 1)
            .is_err());
        assert_eq!(adapter.lifecycle(), CaptureLifecycle::Idle);
        assert!(adapter
            .start_capture(&request, capture_tx, encoder_tx, 1)
            .is_err());
        assert_eq!(adapter.lifecycle(), CaptureLifecycle::Idle);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn non_macos_state_is_explicitly_unsupported() {
        let adapter = ScreenCaptureKitAdapter::new();
        let availability = adapter.availability();
        assert!(!availability.framework_available);
        assert!(!availability.source_enumeration_available);
        assert!(!availability.detail.is_empty());
    }
}
