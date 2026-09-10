//! VideoToolbox H.264 hardware encode (macOS only).
//!
//! This file compiles on every platform: the real backend is gated to
//! macOS, elsewhere a stub fails closed. Rationale: `media.rs` stays
//! cfg-free; platform gating lives here.
//!
//! Design (probe-then-use):
//! - `EngineKind::Auto` probes (tiny session + one real frame); success →
//!   hardware, failure → OpenH264 software + one log line.
//! - `EngineKind::Hardware` fails hard (`HwUnavailable`) when absent — never
//!   a silent software stream when hardware was demanded.
//! - Env override `GOLIVE_DISABLE_HW=1` forces probe failure (deterministic
//!   fallback tests; production never sets it).
//!
//! Session model: one VTCompressionSession per encoder, serial submit/wait
//! (no pipelining — hardware latency is single-digit ms, determinism wins).
//! Reorder is disabled, so completions arrive in submission order and a
//! plain FIFO holds. `encode()` waits for the next completion with a
//! bounded deadline, then:
//! - unit → Annex-B (+SPS/PPS on IDR) → `Some`
//! - encoder-dropped → `Ok(None)` transient skip (capped upstream)
//! - failure/timeout → fatal `Err` (fail-high mid-share)
//! Teardown: CompleteFrames(last pts) → Invalidate. Refcon safety: the
//! callback only touches refcounted shared state, which outlives the
//! session by construction (`Arc` field); CompleteFrames drains in-flight
//! callbacks before Invalidate — no use-after-free window.
//!
//! Input is NV12 (converted from I420 by [`i420_to_nv12`], pure + tested);
//! output is Annex-B with SPS/PPS prepended to every IDR — the same
//! contract as the software path, so the viewer never knows the backend.

/// `OSStatus` (SInt32) is not re-exported by the objc2 crates; alias it
/// locally (matches Apple's `<MacTypes.h>` definition).
#[allow(dead_code)]
type OSStatus = i32;

/// Bytes allowed in one `window_secs` of compressed output. OBS-style 1.5×
/// headroom so AverageBitRate is not starved by a tight hard cap.
pub fn data_rate_limit_bytes(bitrate_bps: u32, window_secs: f64, overshoot: f64) -> i32 {
    let bytes = (bitrate_bps as f64 / 8.0) * window_secs * overshoot;
    bytes.round().clamp(1.0, i32::MAX as f64) as i32
}

/// Convert planar I420 (tight, even dims) to NV12 (Y + interleaved UV).
/// Pure; the contract dims reaching here are always even (see normalize).
pub fn i420_to_nv12(w: usize, h: usize, y: &[u8], u: &[u8], v: &[u8]) -> Vec<u8> {
    debug_assert!(w % 2 == 0 && h % 2 == 0);
    debug_assert_eq!(y.len(), w * h);
    debug_assert_eq!(u.len(), w * h / 4);
    debug_assert_eq!(v.len(), w * h / 4);
    let mut out = vec![0u8; w * h * 3 / 2];
    out[..w * h].copy_from_slice(y);
    let uv = &mut out[w * h..];
    for i in 0..w * h / 4 {
        uv[2 * i] = u[i];
        uv[2 * i + 1] = v[i];
    }
    out
}

/// Convert an AVCC sample (u32-BE length-prefixed NALs) to Annex-B.
/// Returns `(bytes, is_idr)`. Pure; `None` on any malformation (a dropped
/// frame beats a misinterpreted one).
pub fn avcc_to_annexb(avcc: &[u8]) -> Option<(Vec<u8>, bool)> {
    const START: [u8; 4] = [0, 0, 0, 1];
    let mut out = Vec::with_capacity(avcc.len() + 64);
    let mut is_idr = false;
    let mut pos = 0;
    while pos + 4 <= avcc.len() {
        let len = u32::from_be_bytes(avcc[pos..pos + 4].try_into().ok()?) as usize;
        pos += 4;
        let end = pos.checked_add(len)?;
        if end > avcc.len() {
            return None;
        }
        let nal = &avcc[pos..end];
        if nal.is_empty() {
            return None;
        }
        if nal[0] & 0x1F == 5 {
            is_idr = true;
        }
        out.extend_from_slice(&START);
        out.extend_from_slice(nal);
        pos = end;
    }
    if pos != avcc.len() || out.is_empty() {
        return None;
    }
    Some((out, is_idr))
}

#[cfg(target_os = "macos")]
pub use backend::{probe_hardware, VtEncoder};

#[cfg(target_os = "macos")]
mod backend {
    use super::{avcc_to_annexb, OSStatus};
    use crate::media::MediaError;
    use objc2_core_foundation::{
        CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, kCFBooleanFalse, kCFBooleanTrue,
        kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks,
    };
    use objc2_core_media::{kCMVideoCodecType_H264, CMTime, CMTimeFlags, CMSampleBuffer};
    use objc2_core_video::{CVImageBuffer, CVPixelBuffer, CVPixelBufferPool};
    use objc2_video_toolbox::{
        kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
        kVTCompressionPropertyKey_DataRateLimits, kVTCompressionPropertyKey_ExpectedFrameRate,
        kVTCompressionPropertyKey_MaxKeyFrameInterval,
        kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
        kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
        kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
        kVTEncodeFrameOptionKey_ForceKeyFrame,
        kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel,
        kVTProfileLevel_H264_High_AutoLevel,
        kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
        VTCompressionSession, VTEncodeInfoFlags, VTSession, VTSessionSetProperty,
    };
    use std::collections::VecDeque;
    use std::ffi::c_void;
    use std::ptr::{null_mut, NonNull};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    /// Env hook forcing probe failure (deterministic fallback tests).
    const DISABLE_ENV: &str = "GOLIVE_DISABLE_HW";
    /// Consecutive transient skips before the session is declared wedged.
    const MAX_SKIPS: u32 = 30;

    /// Completed (or dropped/failed) unit from the output callback.
    enum Completed {
        Unit {
            annexb: Vec<u8>,
            is_idr: bool,
            sps_pps: Option<Vec<u8>>,
        },
        Dropped,
        Failed(String),
    }

    /// Shared callback state. Only refcounted, lock-guarded data crosses
    /// threads here — never raw session pointers.
    struct CbState {
        queue: Mutex<VecDeque<Completed>>,
        notify: Condvar,
    }

    impl CbState {
        fn push(&self, item: Completed) {
            self.queue.lock().expect("cb queue poisoned").push_back(item);
            self.notify.notify_one();
        }

        fn wait_pop(&self, deadline: Instant) -> Option<Completed> {
            let mut queue = self.queue.lock().expect("cb queue poisoned");
            loop {
                if let Some(item) = queue.pop_front() {
                    return Some(item);
                }
                if Instant::now() >= deadline {
                    return None;
                }
                let (guard, _) = self
                    .notify
                    .wait_timeout(queue, deadline.saturating_duration_since(Instant::now()))
                    .expect("condvar poisoned");
                queue = guard;
            }
        }
    }

    /// Probe: tiny session + one real black frame. Success proves a hardware
    /// H.264 encoder end to end (create → encode → unit out).
    pub fn probe_hardware() -> Result<(), MediaError> {
        if std::env::var_os(DISABLE_ENV).is_some() {
            return Err(MediaError::HwUnavailable(
                "disabled by GOLIVE_DISABLE_HW (test hook)".into(),
            ));
        }
        let mut enc = VtEncoder::new(320, 240, 500_000, 15, true)?;
        let mut nv12 = vec![16u8; 320 * 240];
        nv12.extend(std::iter::repeat(128u8).take(320 * 240 / 2));
        let result = enc.encode_nv12(&nv12);
        enc.close();
        match result {
            Ok(Some(unit)) if !unit.is_empty() => Ok(()),
            Ok(_) => Err(MediaError::HwUnavailable("probe produced no unit".into())),
            Err(e) => Err(e),
        }
    }

    /// Hardware H.264 encoder. Single-threaded use (the encode thread owns
    /// it); callbacks only touch shared `CbState`. `unsafe impl Send` is
    /// justified by that discipline + ordered teardown in `close`/`Drop`.
    pub struct VtEncoder {
        session: *mut VTCompressionSession,
        pool: CFRetained<CVPixelBufferPool>,
        state: Arc<CbState>,
        w: usize,
        h: usize,
        fps: u32,
        bitrate_bps: u32,
        pts: u64,
        sps_pps: Vec<u8>,
        consecutive_skips: u32,
        force_next: bool,
        closed: bool,
    }

    unsafe impl Send for VtEncoder {}

    impl VtEncoder {
        /// Build a session. `require_hw` pins hardware (probe + explicit
        /// mode); without it VT may silently pick software — we never call
        /// it that way (see engine selection). The `GOLIVE_DISABLE_HW`
        /// hook fails closed here too (deterministic tests).
        pub fn new(
            w: usize,
            h: usize,
            bitrate_bps: u32,
            fps: u32,
            require_hw: bool,
        ) -> Result<Self, MediaError> {
            if std::env::var_os(DISABLE_ENV).is_some() {
                return Err(MediaError::HwUnavailable(
                    "disabled by GOLIVE_DISABLE_HW (test hook)".into(),
                ));
            }
            if w < 2 || h < 2 || w > 4096 || h > 4096 || w % 2 != 0 || h % 2 != 0 {
                return Err(MediaError::Codec(format!(
                    "hw dims must be even 2..=4096: {w}x{h}"
                )));
            }
            let state = Arc::new(CbState {
                queue: Mutex::new(VecDeque::new()),
                notify: Condvar::new(),
            });
            // Refcon borrows the Arc without owning: valid while `state`
            // (struct field) is alive, and CompleteFrames drains in-flight
            // callbacks before teardown completes (see close/Drop).
            let refcon = Arc::as_ptr(&state) as *mut c_void;
            let session = unsafe { Self::create_session(w, h, require_hw, refcon) }?;
            let pool = unsafe {
                (*session)
                    .pixel_buffer_pool()
                    .ok_or_else(|| MediaError::HwUnavailable("no pixel buffer pool".into()))?
            };
            let mut enc = Self {
                session,
                pool,
                state,
                w,
                h,
                fps: fps.max(1),
                bitrate_bps,
                pts: 0,
                sps_pps: Vec::new(),
                consecutive_skips: 0,
                force_next: false,
                closed: false,
            };
            if let Err(e) = enc.setup() {
                enc.teardown();
                return Err(e);
            }
            Ok(enc)
        }

        unsafe fn create_session(
            w: usize,
            h: usize,
            require_hw: bool,
            refcon: *mut c_void,
        ) -> Result<*mut VTCompressionSession, MediaError> {
            // Encoder-spec dict pinning hardware (None = system may pick SW).
            let spec: Option<CFRetained<CFDictionary>> = if require_hw {
                let key = kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder;
                // SAFETY: by-value read of an Apple-owned constant (Copy).
                // (No `.as_ref()`: that would leave `&&CFBoolean` and the
                // pointer cast below would take the reference's address.)
                let val: &CFBoolean = unsafe { kCFBooleanTrue }.ok_or_else(|| {
                    MediaError::HwUnavailable("no CFBoolean true".into())
                })?;
                let mut keys = [key as *const _ as *const c_void];
                let mut vals = [val as *const _ as *const c_void];
                let dict = CFDictionary::new(
                    None,
                    keys.as_mut_ptr(),
                    vals.as_mut_ptr(),
                    1,
                    &kCFTypeDictionaryKeyCallBacks,
                    &kCFTypeDictionaryValueCallBacks,
                );
                Some(dict.ok_or_else(|| {
                    MediaError::HwUnavailable("encoder spec dict failed".into())
                })?)
            } else {
                None
            };
            let mut raw: *mut VTCompressionSession = null_mut();
            let status = VTCompressionSession::create(
                None,
                w as i32,
                h as i32,
                kCMVideoCodecType_H264,
                spec.as_deref(),
                None,
                None,
                Some(vt_output_callback),
                refcon,
                NonNull::new(&mut raw).unwrap(),
            );
            if status != 0 || raw.is_null() {
                return Err(MediaError::HwUnavailable(format!(
                    "session create status {status}"
                )));
            }
            Ok(raw)
        }

        fn session_ref(&self) -> &VTCompressionSession {
            // SAFETY: non-null from successful create; alive until close/Drop.
            unsafe { &*self.session }
        }

        fn as_vt_session(&self) -> &VTSession {
            // SAFETY: a compression session IS-A session (Apple documents
            // session helpers as accepting compression sessions).
            unsafe { &*(self.session as *const VTCompressionSession as *const VTSession) }
        }

        fn cf_number(value: i32) -> CFRetained<CFNumber> {
            CFNumber::new_i32(value)
        }

        fn set_number(
            &self,
            key: &'static objc2_core_foundation::CFString,
            value: i32,
        ) -> Result<(), MediaError> {
            let num = Self::cf_number(value);
            // SAFETY: CFNumber derefs to CFType (toll-free root); key/value
            // types match what VTSessionSetProperty documents.
            let status = unsafe { VTSessionSetProperty(self.as_vt_session(), key, Some(&num)) };
            if status != 0 {
                return Err(MediaError::HwUnavailable(format!(
                    "property rejected, status {status}"
                )));
            }
            Ok(())
        }

        fn set_bool(
            &self,
            key: &'static objc2_core_foundation::CFString,
            value: bool,
        ) -> Result<(), MediaError> {
            // SAFETY: by-value reads of Apple-owned constants (Copy).
            let val: &CFBoolean = unsafe {
                if value {
                    kCFBooleanTrue
                } else {
                    kCFBooleanFalse
                }
            }
            .ok_or_else(|| MediaError::HwUnavailable("no CFBoolean".into()))?;
            // SAFETY: key/value types match what VTSessionSetProperty documents.
            let status = unsafe { VTSessionSetProperty(self.as_vt_session(), key, Some(val)) };
            if status != 0 {
                return Err(MediaError::HwUnavailable(format!(
                    "property rejected, status {status}"
                )));
            }
            Ok(())
        }

        fn set_data_rate_limits(&self) -> Result<(), MediaError> {
            const WINDOW_SECS: f64 = 1.0;
            const OVERSHOOT: f64 = 1.5;
            let bytes = CFNumber::new_i32(super::data_rate_limit_bytes(
                self.bitrate_bps.max(100_000),
                WINDOW_SECS,
                OVERSHOOT,
            ));
            let window = CFNumber::new_f64(WINDOW_SECS);
            let limits = CFArray::from_retained_objects(&[bytes, window]);
            // SAFETY: session alive; key/value match VT DataRateLimits (bytes, seconds).
            let status = unsafe {
                VTSessionSetProperty(
                    self.as_vt_session(),
                    &*kVTCompressionPropertyKey_DataRateLimits,
                    Some(&limits),
                )
            };
            // -12900 = kVTPropertyNotSupportedErr: keep ABR, do not fail share.
            if status != 0 && status != -12900 {
                return Err(MediaError::HwUnavailable(format!(
                    "data rate limits rejected, status {status}"
                )));
            }
            Ok(())
        }

        fn setup(&mut self) -> Result<(), MediaError> {
            self.set_number(
                unsafe { &*kVTCompressionPropertyKey_AverageBitRate },
                self.bitrate_bps.max(100_000) as i32,
            )?;
            // OBS: DataRateLimits so ABR actually spends the requested bits.
            // Unsupported on some GPUs — continue, never fail the session.
            self.set_data_rate_limits()?;
            self.set_number(
                unsafe { &*kVTCompressionPropertyKey_MaxKeyFrameInterval },
                (2 * self.fps as u32).max(2) as i32,
            )?;
            let _ = self.set_number(
                unsafe { &*kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration },
                2,
            );
            self.set_bool(unsafe { &*kVTCompressionPropertyKey_RealTime }, true)?;
            self.set_bool(
                unsafe { &*kVTCompressionPropertyKey_AllowFrameReordering },
                false,
            )?;
            let _ = self.set_bool(
                unsafe { &*kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality },
                false,
            );
            self.set_number(
                unsafe { &*kVTCompressionPropertyKey_ExpectedFrameRate },
                self.fps as i32,
            )?;
            // High matches OBS default (CABAC + 8×8). SDP still advertises
            // Constrained Baseline; OpenH264 on our viewer decodes High.
            // Fall back if this GPU rejects High.
            // SAFETY: session alive (field); Apple-owned constant strings.
            let high = unsafe {
                VTSessionSetProperty(
                    self.as_vt_session(),
                    &*kVTCompressionPropertyKey_ProfileLevel,
                    Some(&*kVTProfileLevel_H264_High_AutoLevel),
                )
            };
            if high != 0 {
                let status = unsafe {
                    VTSessionSetProperty(
                        self.as_vt_session(),
                        &*kVTCompressionPropertyKey_ProfileLevel,
                        Some(&*kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel),
                    )
                };
                if status != 0 {
                    return Err(MediaError::HwUnavailable(format!(
                        "profile rejected, status {status}"
                    )));
                }
            }
            let status = unsafe { self.session_ref().prepare_to_encode_frames() };
            if status != 0 {
                return Err(MediaError::HwUnavailable(format!(
                    "prepare status {status}"
                )));
            }
            Ok(())
        }

        /// Force the next encoded unit to start with an IDR. Implemented as a
        /// per-frame option (`kVTEncodeFrameOptionKey_ForceKeyFrame`) on the
        /// next submit — there is no session-level force-keyframe property.
        pub fn force_intra(&mut self) {
            self.force_next = true;
        }

        /// One-entry frame-props dict forcing a keyframe on this submit.
        fn force_keyframe_props() -> Result<CFRetained<CFDictionary>, MediaError> {
            // SAFETY: Apple-owned constants; single-entry CFDictionary of
            // CFString → CFBoolean (toll-free, type callbacks handle retain).
            unsafe {
                let key = &*kVTEncodeFrameOptionKey_ForceKeyFrame;
                // SAFETY: by-value read of an Apple-owned constant (Copy).
                let val: &CFBoolean = kCFBooleanTrue.ok_or_else(|| {
                    MediaError::HwUnavailable("no CFBoolean".into())
                })?;
                let mut keys = [key as *const _ as *const c_void];
                let mut vals = [val as *const _ as *const c_void];
                CFDictionary::new(
                    None,
                    keys.as_mut_ptr(),
                    vals.as_mut_ptr(),
                    1,
                    &kCFTypeDictionaryKeyCallBacks,
                    &kCFTypeDictionaryValueCallBacks,
                )
                .ok_or_else(|| MediaError::HwUnavailable("frame props dict failed".into()))
            }
        }

        /// Encode one NV12 frame (w*h + w*h/2 bytes). Returns the Annex-B
        /// unit, or `None` on a transient encoder drop (capped upstream).
        /// Fatal errors (timeouts included) are `Err` — fail-high mid-share.
        pub fn encode_nv12(&mut self, nv12: &[u8]) -> Result<Option<Vec<u8>>, MediaError> {
            if self.closed {
                return Err(MediaError::Codec("encoder closed".into()));
            }
            let expect = self.w * self.h * 3 / 2;
            if nv12.len() != expect {
                return Err(MediaError::Codec(format!(
                    "nv12 size {} != {}x{} frame",
                    nv12.len(),
                    self.w,
                    self.h
                )));
            }
            let pixel = self.copy_to_pool(nv12)?;
            let done = self.submit_owned(pixel);
            return done;
        }

        /// Zero-copy submit of a caller-retained pixel buffer (SCK sample
        /// straight through: no pool copy, no NV12 staging — VT converts
        /// BGRA internally). The buffer is released on return; dims MUST
        /// match the session (the caller verifies, double-checked here).
        pub fn encode_cv_pixel_buffer(
            &mut self,
            pixel: CFRetained<CVPixelBuffer>,
        ) -> Result<Option<Vec<u8>>, MediaError> {
            if self.closed {
                return Err(MediaError::Codec("encoder closed".into()));
            }
            // width/height via the safe wrappers (macos-only backend).
            let (w, h) = (
                objc2_core_video::CVPixelBufferGetWidth(&pixel),
                objc2_core_video::CVPixelBufferGetHeight(&pixel),
            );
            if (w, h) != (self.w, self.h) {
                return Err(MediaError::Codec(format!(
                    "hw buffer {w}x{h} != session {}x{}",
                    self.w, self.h
                )));
            }
            self.submit_owned(pixel)
        }

        /// Shared submit: PTS stamp, force-intra props, encode, bounded
        /// completion wait. Takes the buffer by value (pool-owned or
        /// caller-retained) and releases it before waiting.
        fn submit_owned(
            &mut self,
            pixel: CFRetained<CVPixelBuffer>,
        ) -> Result<Option<Vec<u8>>, MediaError> {
            let ticks = 90_000u64 / self.fps.max(1) as u64;
            let pts = CMTime {
                value: (self.pts * ticks) as i64,
                timescale: 90_000,
                flags: CMTimeFlags::Valid,
                epoch: 0,
            };
            self.pts += 1;
            let image: &CVImageBuffer = unsafe {
                // Toll-free: CVPixelBuffer IS-A CVImageBuffer (documented).
                &*(pixel.as_ref() as *const CVPixelBuffer as *const CVImageBuffer)
            };
            let mut info_flags = VTEncodeInfoFlags(0);
            // A pending force-intra rides this submit only (cleared even on
            // submit failure — the next fresh IDR covers recovery anyway).
            let frame_props = if self.force_next {
                self.force_next = false;
                Some(Self::force_keyframe_props()?)
            } else {
                None
            };
            let status = unsafe {
                self.session_ref().encode_frame(
                    image,
                    pts,
                    pts_duration(self.fps),
                    frame_props.as_deref(),
                    std::ptr::null_mut(),
                    &mut info_flags,
                )
            };
            drop(pixel);
            if status != 0 {
                return Err(MediaError::Codec(format!("hw encode status {status}")));
            }
            match self.wait_completed() {
                result => result,
            }
        }

        /// Wait for the next completion, bounded: 4 frame intervals, minimum
        /// 200ms. Timeout is fatal (a realtime encoder stuck longer is
        /// wedged — fail-high, never wedge the loop). IDR units leave with
        /// fresh SPS/PPS prepended (cache refreshed from the sample).
        fn wait_completed(&mut self) -> Result<Option<Vec<u8>>, MediaError> {
            let wait = Duration::from_millis((4000 / self.fps.max(1) as u64).max(200));
            let deadline = Instant::now() + wait;
            match self.state.wait_pop(deadline) {
                Some(Completed::Unit { annexb, is_idr, sps_pps }) => {
                    self.consecutive_skips = 0;
                    if !is_idr {
                        return Ok(Some(annexb));
                    }
                    if let Some(fresh) = sps_pps {
                        self.sps_pps = fresh;
                    }
                    let mut full = self.sps_pps.clone();
                    full.extend_from_slice(&annexb);
                    Ok(Some(full))
                }
                Some(Completed::Dropped) => {
                    self.consecutive_skips += 1;
                    if self.consecutive_skips > MAX_SKIPS {
                        Err(MediaError::Codec("hw dropping every frame".into()))
                    } else {
                        Ok(None)
                    }
                }
                Some(Completed::Failed(detail)) => {
                    Err(MediaError::Codec(format!("hw encode failed: {detail}")))
                }
                None => Err(MediaError::Codec("hw encode timeout".into())),
            }
        }

        fn copy_to_pool(
            &self,
            nv12: &[u8],
        ) -> Result<objc2_core_foundation::CFRetained<CVPixelBuffer>, MediaError> {
            use objc2_core_video::*;
            let mut raw: *mut CVPixelBuffer = null_mut();
            let status = unsafe {
                CVPixelBufferPool::create_pixel_buffer(
                    None,
                    &self.pool,
                    NonNull::new(&mut raw).unwrap(),
                )
            };
            if status != 0 || raw.is_null() {
                return Err(MediaError::Codec(format!("pixel pool status {status}")));
            }
            // SAFETY: Create rule (+1) adopted exactly once here.
            let pixel: objc2_core_foundation::CFRetained<CVPixelBuffer> =
                unsafe { objc2_core_foundation::CFRetained::from_raw(NonNull::new(raw).unwrap()) };
            let status =
                unsafe { CVPixelBufferLockBaseAddress(&pixel, CVPixelBufferLockFlags::empty()) };
            if status != 0 {
                return Err(MediaError::Codec(format!("pixel lock status {status}")));
            }
            let copied = unsafe {
                let y_base = CVPixelBufferGetBaseAddressOfPlane(&pixel, 0) as *mut u8;
                let uv_base = CVPixelBufferGetBaseAddressOfPlane(&pixel, 1) as *mut u8;
                if y_base.is_null() || uv_base.is_null() {
                    false
                } else {
                    let y_stride = CVPixelBufferGetBytesPerRowOfPlane(&pixel, 0);
                    let uv_stride = CVPixelBufferGetBytesPerRowOfPlane(&pixel, 1);
                    let (w, h) = (self.w, self.h);
                    for row in 0..h {
                        let dst = y_base.add(row * y_stride);
                        let src = &nv12[row * w..row * w + w];
                        std::ptr::copy_nonoverlapping(src.as_ptr(), dst, w);
                    }
                    for row in 0..h / 2 {
                        let dst = uv_base.add(row * uv_stride);
                        let src = &nv12[w * h + row * w..w * h + row * w + w];
                        std::ptr::copy_nonoverlapping(src.as_ptr(), dst, w);
                    }
                    true
                }
            };
            unsafe { CVPixelBufferUnlockBaseAddress(&pixel, CVPixelBufferLockFlags::empty()) };
            if !copied {
                return Err(MediaError::Codec("pixel copy failed".into()));
            }
            Ok(pixel)
        }

        /// Graceful close: drain completions, invalidate. Re-entrant safe
        /// (second call is a no-op via the closed flag).
        pub fn close(&mut self) {
            if self.closed {
                return;
            }
            self.closed = true;
            self.teardown();
        }

        fn teardown(&mut self) {
            unsafe {
                let _ = self.session_ref().complete_frames(CMTime {
                    value: i64::MAX / 2,
                    timescale: 90_000,
                    flags: CMTimeFlags::Valid,
                    epoch: 0,
                });
                self.session_ref().invalidate();
            }
        }

        pub fn dims(&self) -> (usize, usize) {
            (self.w, self.h)
        }
    }

    impl Drop for VtEncoder {
        fn drop(&mut self) {
            // Best-effort ordered teardown (documented; see module docs).
            // complete_frames may block briefly flushing tail frames.
            self.teardown();
        }
    }

    fn pts_duration(fps: u32) -> CMTime {
        CMTime {
            value: (90_000u64 / fps.max(1) as u64) as i64,
            timescale: 90_000,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        }
    }

    /// Output callback: parse one sample into Annex-B (+SPS/PPS on IDR),
    /// tag drops/failures, push FIFO (no-reorder enforced → order holds).
    unsafe extern "C-unwind" fn vt_output_callback(
        refcon: *mut c_void,
        _source_refcon: *mut c_void,
        status: OSStatus,
        _info: VTEncodeInfoFlags,
        sample: *mut CMSampleBuffer,
    ) {
        let state: &CbState = unsafe { &*(refcon as *const CbState) };
        let done = if status != 0 {
            Completed::Failed(format!("status {status}"))
        } else if sample.is_null() {
            Completed::Dropped
        } else {
            // SAFETY: borrowed for parsing only; no retain, no escape.
            match unsafe { parse_sample(&*sample) } {
                Some((unit, is_idr, sps_pps)) => Completed::Unit { annexb: unit, is_idr, sps_pps },
                None => Completed::Dropped,
            }
        };
        state.push(done);
    }

    /// Sample → (Annex-B, is_idr, fresh SPS/PPS on IDR or None).
    unsafe fn parse_sample(
        sample: &CMSampleBuffer,
    ) -> Option<(Vec<u8>, bool, Option<Vec<u8>>)> {
        let block = sample.data_buffer()?;
        let mut len = 0usize;
        let mut total = 0usize;
        let mut ptr: *mut std::ffi::c_char = null_mut();
        if block.data_pointer(0, &mut len, &mut total, &mut ptr) != 0 || ptr.is_null() {
            return None;
        }
        let avcc = unsafe { std::slice::from_raw_parts(ptr as *const u8, total) };
        let (annexb, is_idr) = avcc_to_annexb(avcc)?;
        let sps_pps = if is_idr {
            extract_param_sets(sample)
        } else {
            None
        };
        Some((annexb, is_idr, sps_pps))
    }

    /// SPS+PPS as Annex-B prefix from the sample's format description.
    /// `None` (skip refresh, keep cache) on any anomaly — never fatal here;
    /// the frame itself still flows.
    unsafe fn extract_param_sets(sample: &CMSampleBuffer) -> Option<Vec<u8>> {
        use objc2_core_media::CMVideoFormatDescriptionGetH264ParameterSetAtIndex;
        let desc = sample.format_description()?;
        let mut out = Vec::new();
        for index in [0usize, 1usize] {
            let mut ps_ptr: *const u8 = null_mut();
            let mut ps_len = 0usize;
            let mut ps_count = 0usize;
            let mut hdr_len = 0i32;
            if CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                &desc,
                index,
                &mut ps_ptr,
                &mut ps_len,
                &mut ps_count,
                &mut hdr_len,
            ) != 0
                || ps_ptr.is_null()
            {
                return None;
            }
            let ps = unsafe { std::slice::from_raw_parts(ps_ptr, ps_len) };
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(ps);
        }
        Some(out)
    }
}

#[cfg(not(target_os = "macos"))]
mod backend {
    //! Stub: every constructor fails closed with `HwUnavailable`.
    use crate::media::MediaError;

    pub fn probe_hardware() -> Result<(), MediaError> {
        Err(MediaError::HwUnavailable("VideoToolbox is macOS-only".into()))
    }

    pub struct VtEncoder;

    impl VtEncoder {
        pub fn new(
            _w: usize,
            _h: usize,
            _bitrate_bps: u32,
            _fps: u32,
            _require_hw: bool,
        ) -> Result<Self, MediaError> {
            Err(MediaError::HwUnavailable("VideoToolbox is macOS-only".into()))
        }

        pub fn force_intra(&mut self) {}

        pub fn encode_nv12(&mut self, _nv12: &[u8]) -> Result<Option<Vec<u8>>, MediaError> {
            Err(MediaError::HwUnavailable("VideoToolbox is macOS-only".into()))
        }

        pub fn close(&mut self) {}

        pub fn dims(&self) -> (usize, usize) {
            (0, 0)
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub use backend::{probe_hardware, VtEncoder};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_rate_limit_matches_obs_headroom() {
        assert_eq!(data_rate_limit_bytes(8_000_000, 1.0, 1.5), 1_500_000);
        assert_eq!(data_rate_limit_bytes(0, 1.0, 1.5), 1);
    }

    #[test]
    fn nv12_interleaves_chroma() {
        // 4x2: Y 0..7, U all 10, V all 20.
        let y: Vec<u8> = (0..8).collect();
        let u = vec![10u8; 2];
        let v = vec![20u8; 2];
        let nv12 = i420_to_nv12(4, 2, &y, &u, &v);
        assert_eq!(nv12.len(), 4 * 2 * 3 / 2);
        assert_eq!(&nv12[..8], &y[..]);
        assert_eq!(&nv12[8..], &[10, 20, 10, 20]);
    }

    #[test]
    fn avcc_splits_lengths_to_start_codes() {
        // Two NALs: [len=3]"ABC" (type 1, slice) + [len=2] IDR-ish.
        let idr = [0x65u8, 0xAA];
        let mut avcc = Vec::new();
        avcc.extend_from_slice(&3u32.to_be_bytes());
        avcc.extend_from_slice(&[0x41, 0x42, 0x43]);
        avcc.extend_from_slice(&2u32.to_be_bytes());
        avcc.extend_from_slice(&idr);
        let (annexb, is_idr) = avcc_to_annexb(&avcc).expect("parses");
        assert!(is_idr);
        assert_eq!(
            annexb,
            vec![0, 0, 0, 1, 0x41, 0x42, 0x43, 0, 0, 0, 1, 0x65, 0xAA]
        );
    }

    #[test]
    fn avcc_rejects_truncation_and_empties() {
        assert!(avcc_to_annexb(&[]).is_none());
        assert!(avcc_to_annexb(&[0, 0]).is_none());
        let mut truncated = vec![];
        truncated.extend_from_slice(&10u32.to_be_bytes());
        truncated.extend_from_slice(&[0x41, 0x42]);
        assert!(avcc_to_annexb(&truncated).is_none());
        let mut empty_nal = vec![];
        empty_nal.extend_from_slice(&0u32.to_be_bytes());
        assert!(avcc_to_annexb(&empty_nal).is_none());
    }
}
