//! Safe Rust ownership for the Swift VideoToolbox C-ABI shim.

use super::access_unit::{
    AccessUnitPushResult, AccessUnitQueue, AvccAnnexBConverter, AvccError, EncodedAccessUnit,
    HevcAnnexBConverter,
};
use super::pipeline::{EncoderControl, PipelineState};
use super::timestamp::to_90khz;
use super::types::VideoCodec;
use std::ffi::c_void;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

#[cfg(target_os = "macos")]
use objc2_core_video::CVPixelBuffer;

#[cfg(target_os = "macos")]
type EncodedCallback = extern "C" fn(*mut c_void, *const u8, usize, i64, i32, u8);
#[cfg(target_os = "macos")]
type ErrorCallback = extern "C" fn(*mut c_void, i32);

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn golive_vt_encoder_create(
        width: i32,
        height: i32,
        bitrate: i32,
        frame_rate: i32,
        codec: i32,
        callback: EncodedCallback,
        error_callback: ErrorCallback,
        callback_context: *mut c_void,
    ) -> *mut c_void;
    fn golive_vt_encoder_encode(
        encoder: *mut c_void,
        pixel_buffer: *mut CVPixelBuffer,
        pts_value: i64,
        pts_timescale: i32,
    ) -> i32;
    fn golive_vt_encoder_flush(encoder: *mut c_void) -> i32;
    fn golive_vt_encoder_force_keyframe(encoder: *mut c_void) -> i32;
    fn golive_vt_encoder_set_bitrate(encoder: *mut c_void, bitrate: i32) -> i32;
    fn golive_vt_encoder_destroy(encoder: *mut c_void);
    fn golive_vt_supports_av1() -> bool;
}

/// Probes VideoToolbox for an AV1 encoder (hardware on M3+, macOS 13+).
/// Used for capability reporting; the H.264 path is unaffected.
pub(crate) fn av1_encode_supported() -> bool {
    // SAFETY: probe creates and releases a scratch session, no shared state.
    unsafe { golive_vt_supports_av1() }
}

#[derive(Debug, Clone)]
pub(crate) struct VideoToolboxError(pub(crate) String);

impl std::fmt::Display for VideoToolboxError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for VideoToolboxError {}

#[cfg(target_os = "macos")]
impl PipelineState {
    fn report_error(&self, error: VideoToolboxError) {
        self.fail(error.to_string());
    }

    pub(crate) fn take_error(&self) -> Option<VideoToolboxError> {
        self.failure().map(VideoToolboxError)
    }
}

#[cfg(target_os = "macos")]
enum Converter {
    H264(AvccAnnexBConverter),
    Hevc(HevcAnnexBConverter),
}

#[cfg(target_os = "macos")]
impl Converter {
    fn for_codec(codec: VideoCodec) -> Self {
        match codec {
            VideoCodec::H264 => Self::H264(AvccAnnexBConverter::default()),
            VideoCodec::H264High => Self::H264(AvccAnnexBConverter::high_profile()),
            VideoCodec::Hevc => Self::Hevc(HevcAnnexBConverter::default()),
        }
    }

    fn convert(
        &mut self,
        bytes: &[u8],
        timestamp_90khz: u64,
        keyframe: bool,
    ) -> Result<EncodedAccessUnit, AvccError> {
        match self {
            Self::H264(converter) => converter.convert(bytes, timestamp_90khz, keyframe),
            Self::Hevc(converter) => converter.convert(bytes, timestamp_90khz, keyframe),
        }
    }
}

#[cfg(target_os = "macos")]
struct CallbackContext {
    output: AccessUnitQueue,
    converter: Mutex<Converter>,
    state: Arc<PipelineState>,
    control: Arc<EncoderControl>,
}

#[cfg(target_os = "macos")]
pub(crate) struct VideoToolboxEncoder {
    handle: *mut c_void,
    callback_context: *mut CallbackContext,
    state: Arc<PipelineState>,
}

#[cfg(target_os = "macos")]
unsafe impl Send for VideoToolboxEncoder {}

#[cfg(target_os = "macos")]
impl VideoToolboxEncoder {
    pub(crate) fn new(
        width: u32,
        height: u32,
        bitrate: u32,
        frame_rate: u32,
        codec: VideoCodec,
        output: AccessUnitQueue,
        state: Arc<PipelineState>,
        control: Arc<EncoderControl>,
    ) -> Result<Self, VideoToolboxError> {
        let callback_context = Box::into_raw(Box::new(CallbackContext {
            output,
            converter: Mutex::new(Converter::for_codec(codec)),
            state: Arc::clone(&state),
            control,
        }));
        let codec_flag = match codec {
            VideoCodec::H264 => 0,
            VideoCodec::Hevc => 1,
            VideoCodec::H264High => 2,
        };
        let handle = unsafe {
            golive_vt_encoder_create(
                width as i32,
                height as i32,
                bitrate as i32,
                frame_rate as i32,
                codec_flag,
                encoded_callback,
                encoder_error_callback,
                callback_context.cast(),
            )
        };
        if handle.is_null() {
            unsafe { drop(Box::from_raw(callback_context)) };
            return Err(VideoToolboxError(
                "VideoToolbox compression session creation failed".into(),
            ));
        }
        Ok(Self {
            handle,
            callback_context,
            state,
        })
    }

    pub(crate) fn encode(
        &mut self,
        pixel_buffer: *mut CVPixelBuffer,
        pts_value: i64,
        pts_timescale: i32,
    ) -> Result<(), VideoToolboxError> {
        self.raise_callback_error()?;
        let status = unsafe {
            golive_vt_encoder_encode(self.handle, pixel_buffer, pts_value, pts_timescale)
        };
        if status != 0 {
            return Err(VideoToolboxError(format!(
                "VideoToolbox encode failed with OSStatus {status}"
            )));
        }
        self.raise_callback_error()
    }

    pub(crate) fn flush(&mut self) -> Result<(), VideoToolboxError> {
        let status = unsafe { golive_vt_encoder_flush(self.handle) };
        if status != 0 {
            return Err(VideoToolboxError(format!(
                "VideoToolbox flush failed with OSStatus {status}"
            )));
        }
        self.raise_callback_error()
    }

    pub(crate) fn force_keyframe(&mut self) -> Result<(), VideoToolboxError> {
        let status = unsafe { golive_vt_encoder_force_keyframe(self.handle) };
        if status != 0 {
            return Err(VideoToolboxError(format!(
                "VideoToolbox force-keyframe failed with OSStatus {status}"
            )));
        }
        Ok(())
    }

    pub(crate) fn set_bitrate(&mut self, bitrate: u32) -> Result<(), VideoToolboxError> {
        let status = unsafe { golive_vt_encoder_set_bitrate(self.handle, bitrate as i32) };
        if status != 0 {
            return Err(VideoToolboxError(format!(
                "VideoToolbox bitrate update failed with OSStatus {status}"
            )));
        }
        Ok(())
    }

    fn raise_callback_error(&self) -> Result<(), VideoToolboxError> {
        self.state.take_error().map_or(Ok(()), Err)
    }
}

#[cfg(target_os = "macos")]
impl Drop for VideoToolboxEncoder {
    fn drop(&mut self) {
        // The Swift destroy operation calls CompleteFrames before invalidating
        // the session. VideoToolbox guarantees no output callback remains in
        // flight when CompleteFrames returns, so the raw callback context is
        // released only after the callback quiescence point.
        self.state
            .accepting_callbacks
            .store(false, Ordering::Release);
        unsafe {
            (*self.callback_context).output.close();
            golive_vt_encoder_destroy(self.handle);
            drop(Box::from_raw(self.callback_context));
        }
    }
}

#[cfg(target_os = "macos")]
extern "C" fn encoded_callback(
    context: *mut c_void,
    data: *const u8,
    len: usize,
    pts_value: i64,
    pts_timescale: i32,
    keyframe: u8,
) {
    if context.is_null() {
        return;
    }
    let context = unsafe { &*context.cast::<CallbackContext>() };
    if !context.state.accepting_callbacks.load(Ordering::Acquire) {
        return;
    }
    if data.is_null() || len == 0 {
        context.state.report_error(VideoToolboxError(
            "VideoToolbox callback returned an empty access unit".into(),
        ));
        return;
    }
    let Some(timestamp_90khz) = to_90khz(pts_value, pts_timescale) else {
        context.state.report_error(VideoToolboxError(
            "VideoToolbox returned an invalid PTS".into(),
        ));
        return;
    };
    let bytes = unsafe { std::slice::from_raw_parts(data, len) };
    let Ok(mut converter) = context.converter.lock() else {
        context.state.report_error(VideoToolboxError(
            "converter lock was poisoned".into(),
        ));
        return;
    };
    let unit = match converter.convert(bytes, timestamp_90khz, keyframe != 0) {
        Ok(unit) => unit,
        Err(error) => {
            eprintln!("[goDrinking] access-unit skipped: {error}");
            return;
        }
    };
    match context.output.try_push(unit) {
        AccessUnitPushResult::Enqueued => {}
        AccessUnitPushResult::DroppedUntilKeyframe => {
            context.control.request_keyframe();
        }
        AccessUnitPushResult::Closed => {}
    }
}

#[cfg(target_os = "macos")]
extern "C" fn encoder_error_callback(context: *mut c_void, status: i32) {
    if context.is_null() {
        return;
    }
    let context = unsafe { &*context.cast::<CallbackContext>() };
    if context.state.accepting_callbacks.load(Ordering::Acquire) {
        context.state.report_error(VideoToolboxError(format!(
            "VideoToolbox callback failed with OSStatus {status}"
        )));
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) struct VideoToolboxEncoder;
