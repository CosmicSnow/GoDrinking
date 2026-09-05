// Media Foundation H.264 hardware encoder for Windows native capture.
//
// This is the Discord-style path: on an RTX 3070 the HARDWARE MFT resolves to
// NVENC silicon, so 1080p60 costs almost no CPU. Everything here fails clean
// with a String error so the pipeline can fall back to OpenH264; a crash is
// never an option, so all struct layouts come from the official windows crate
// and every CodecAPI tweak is best-effort with a log line.
//
// Calling pattern is synchronous: feed one sample, drain outputs with a small
// retry budget (no B-frames, so output follows input within milliseconds).
// A synthetic gray frame self-test at construction proves the whole path
// before the session starts; Auto mode falls back to software when it fails.
// The self-test validates decodability (SPS profile compatible with the
// session codec plus an IDR), not just byte output: a "working" MFT with
// the wrong profile would otherwise black-screen viewers while the host
// preview looks fine.

use super::access_unit::{bgra_to_nv12, AccessUnitPushResult, AccessUnitQueue};
use super::logger;
use super::pipeline::{EncoderControl, NativeFrame, PipelineState};
use super::types::VideoCodec;
use super::windows_encoder::AnnexBConverter;
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::ptr::null_mut;
use std::sync::Arc;
use std::time::Duration;
use windows::core::{Interface, GUID};
use windows::Win32::Foundation::VARIANT_BOOL;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_MULTITHREADED,
};
use windows::Win32::System::Variant::{VARIANT, VARIANT_0_0, VT_BOOL, VT_UI4};

// H.264 Constrained Baseline profile for the output type. There is no
// High path: a driver that rejects Baseline fails construction so the
// pipeline falls back to OpenH264. Plain integers avoid depending on
// profile enum bindings.
const H264_BASELINE_PROFILE: u32 = 66;

// Upper bound for one encoder output sample. A 1080p IDR at 8 Mbps is far
// smaller; the drain loop grows the buffer if BUFFERTOOSMALL ever fires.
const OUTPUT_BUFFER_START: u32 = 4 * 1024 * 1024;
const OUTPUT_BUFFER_MAX: u32 = 16 * 1024 * 1024;

// Sync drain patience: 1ms sleeps, bounded, per encode call.
const DRAIN_RETRIES: u32 = 12;

struct ComGuard(bool);

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.0 {
            unsafe {
                CoUninitialize();
            }
        }
    }
}

fn hr(error: windows::core::Error) -> String {
    format!("0x{:08X} {}", error.code().0 as u32, error.message())
}

fn variant_ui4(value: u32) -> VARIANT {
    let mut inner = VARIANT_0_0::default();
    inner.vt = VT_UI4;
    unsafe {
        inner.Anonymous.ulVal = value;
    }
    let mut variant = VARIANT::default();
    unsafe {
        variant.Anonymous.Anonymous = ManuallyDrop::new(inner);
    }
    variant
}

fn variant_bool(value: bool) -> VARIANT {
    let mut inner = VARIANT_0_0::default();
    inner.vt = VT_BOOL;
    unsafe {
        inner.Anonymous.boolVal = VARIANT_BOOL(if value { -1 } else { 0 });
    }
    let mut variant = VARIANT::default();
    unsafe {
        variant.Anonymous.Anonymous = ManuallyDrop::new(inner);
    }
    variant
}

fn video_type(
    subtype: *const GUID,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: Option<u32>,
    profile: Option<u32>,
) -> Result<IMFMediaType, windows::core::Error> {
    unsafe {
        let media_type = MFCreateMediaType()?;
        media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        media_type.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        media_type.SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | height as u64)?;
        media_type.SetUINT64(&MF_MT_FRAME_RATE, ((fps as u64) << 32) | 1)?;
        media_type.SetUINT32(&MF_MT_INTERLACE_MODE, 2)?;
        if let Some(bitrate) = bitrate {
            media_type.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
        }
        if let Some(profile) = profile {
            media_type.SetUINT32(&MF_MT_MPEG2_PROFILE, profile)?;
        }
        Ok(media_type)
    }
}

fn set_codec_u32(api: &ICodecAPI, id: *const GUID, value: u32, what: &str) {
    let variant = variant_ui4(value);
    match unsafe { api.SetValue(id, &variant) } {
        Ok(()) => {}
        Err(error) => eprintln!(
            "[goDrinking] MF codec option {what} unavailable: {}",
            hr(error)
        ),
    }
}

pub(crate) struct MfH264Encoder {
    transform: IMFTransform,
    codec_api: Option<ICodecAPI>,
    converter: AnnexBConverter,
    output: AccessUnitQueue,
    control: Arc<EncoderControl>,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    use_nv12: bool,
    profile_high: bool,
    output_buffer_size: u32,
    _com: ComGuard,
}

impl MfH264Encoder {
    pub(crate) fn new(
        width: u32,
        height: u32,
        bitrate: u32,
        fps: u32,
        _video_codec: VideoCodec,
        output: AccessUnitQueue,
        _state: Arc<PipelineState>,
        control: Arc<EncoderControl>,
    ) -> Result<Self, String> {
        if width < 2 || height < 2 {
            return Err("MF encoder frame is too small".into());
        }
        // S_OK and S_FALSE (already initialized) are both fine to ignore.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let _com = ComGuard(true);
        unsafe {
            MFStartup(MF_VERSION, 0).map_err(hr)?;
        }
        let (transform, friendly_name) = Self::open_hardware_encoder()
            .map_err(|error| format!("MF hardware MFT open failed: {error}"))?;
        eprintln!("[goDrinking] MF hardware encoder: {friendly_name}");

        // Unlock async MFTs when present; harmless for synchronous ones.
        if let Ok(attributes) = unsafe { transform.GetAttributes() } {
            let is_async = unsafe { attributes.GetUINT32(&MF_TRANSFORM_ASYNC) }.unwrap_or(0);
            if is_async != 0 {
                let _ = unsafe { attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) };
            }
        }

        // NV12 input first: our `bgra_to_nv12` conversion is BT.709
        // limited-range (Y 16-235, UV 16-240), deterministic across drivers,
        // and the hardware enumeration above already requires NV12 support.
        // RGB32 is only a fallback for MFTs that reject NV12 despite
        // advertising it. Either way the emitted bitstream must stay
        // Constrained Baseline (checked in the self-test below).
        let input_nv12 =
            video_type(&MFVideoFormat_NV12, width, height, fps, None, None).map_err(hr)?;
        let mut use_nv12 = true;
        if unsafe { transform.SetInputType(0, &input_nv12, 0) }.is_err() {
            // Mirrored to the log file: stderr is invisible in release/portable runs.
            let message = "MF NV12 input rejected, trying RGB32".to_owned();
            eprintln!("[goDrinking] {message}");
            logger::log("WARN", "mf encoder", &message);
            let input_rgb32 =
                video_type(&MFVideoFormat_RGB32, width, height, fps, None, None).map_err(hr)?;
            unsafe {
                transform
                    .SetInputType(0, &input_rgb32, 0)
                    .map_err(|error| format!("MF RGB32 input rejected: {}", hr(error)))?
            }
            use_nv12 = false;
        }

        // Phase-2B product codec: H.264 Constrained Baseline only
        // (MF_MT_MPEG2_PROFILE 66, SDP 42e02a), identical to the VideoToolbox
        // and OpenH264 arms. The session codec is validated to H264 before
        // acquisition, so the request value is intentionally ignored: there
        // is no High fallback. If the driver rejects Baseline, construction
        // fails and the pipeline falls back to OpenH264 software (Auto) or
        // fails loudly (explicit Hardware) instead of emitting a High
        // bitstream the transport drops.
        let output_type = video_type(
            &MFVideoFormat_H264,
            width,
            height,
            fps,
            Some(bitrate),
            Some(H264_BASELINE_PROFILE),
        )
        .map_err(hr)?;
        unsafe {
            transform
                .SetOutputType(0, &output_type, 0)
                .map_err(|error| {
                    format!(
                        "MF Baseline output type rejected (no High fallback): {}",
                        hr(error)
                    )
                })?
        }
        let profile_high = false;

        let codec_api: Option<ICodecAPI> = transform.cast().ok();
        if let Some(api) = &codec_api {
            // Constrained Baseline contract, identical to the VT/OpenH264
            // arms: CBR, GOP = fps (~1s), no B-frames, CAVLC (CABAC off is
            // implied by Baseline; set explicitly so a driver default can
            // never drift), low-delay. Forced IDRs arrive via
            // `force_keyframe` (join / PLI-FIR / queue-overflow flag consumed
            // in the pipeline Video arm).
            set_codec_u32(
                api,
                &CODECAPI_AVEncCommonRateControlMode,
                eAVEncCommonRateControlMode_CBR.0 as u32,
                "rate control CBR",
            );
            set_codec_u32(
                api,
                &CODECAPI_AVEncCommonMeanBitRate,
                bitrate,
                "mean bitrate",
            );
            set_codec_u32(api, &CODECAPI_AVEncCommonMaxBitRate, bitrate, "max bitrate");
            set_codec_u32(api, &CODECAPI_AVEncMPVGOPSize, fps.max(1), "GOP size");
            set_codec_u32(
                api,
                &CODECAPI_AVEncMPVDefaultBPictureCount,
                0,
                "B-frames off",
            );
            set_codec_u32(
                api,
                &CODECAPI_AVEncH264CABACEnable,
                0,
                "CABAC off (Baseline CAVLC)",
            );
            set_codec_u32(api, &CODECAPI_AVLowLatencyMode, 1, "low latency");
        }

        unsafe {
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(hr)?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(hr)?;
        }

        let mut encoder = Self {
            transform,
            codec_api,
            converter: AnnexBConverter::default(),
            output,
            control,
            width,
            height,
            fps,
            bitrate,
            use_nv12,
            profile_high,
            output_buffer_size: OUTPUT_BUFFER_START,
            _com,
        };
        // Self-test: a gray frame must produce output, otherwise this MFT is
        // not usable here and Auto mode falls back to OpenH264.
        let test_pixels = if use_nv12 {
            let bgra = vec![128_u8; (width as usize) * (height as usize) * 4];
            bgra_to_nv12(&bgra, width, height).ok_or("MF self-test conversion failed")?
        } else {
            vec![128_u8; (width as usize) * (height as usize) * 4]
        };
        let units = encoder.encode_bytes(&test_pixels, 0)?;
        if units.is_empty() {
            let message = "MF encoder self-test produced no output".to_owned();
            logger::log(
                "WARN",
                "mf encoder",
                &format!("{message}; falling back to software"),
            );
            return Err(message);
        }
        // Validate decodability, not just byte output: the transport drops
        // every sample whose SPS profile disagrees with the session codec
        // and waits for a keyframe before sending anything. A hardware MFT
        // that "succeeds" with the wrong profile (or never emits SPS/IDR)
        // would black-screen every viewer while the host preview looks
        // fine, with no error anywhere. Fail the self-test instead so Auto
        // falls back to OpenH264. Only Constrained Baseline (0x42) passes.
        let mut saw_keyframe = false;
        let mut profile: Option<String> = None;
        for sample in &units {
            if let Some(unit) = encoder.converter.convert(sample, 0, false) {
                saw_keyframe = saw_keyframe || unit.keyframe;
                if profile.is_none() {
                    profile = unit.profile_level_id.clone();
                }
            }
        }
        let profile_ok = profile.as_deref().is_some_and(|id| {
            let id = id.to_ascii_lowercase();
            id.len() == 6 && id.starts_with("42")
        });
        eprintln!(
            "[goDrinking] MF encoder self-test ok ({} byte first sample, nv12={}, profile={:?}, keyframe={})",
            units[0].len(),
            use_nv12,
            profile,
            saw_keyframe
        );
        logger::log(
            "INFO",
            "mf encoder",
            &format!(
                "self-test ok ({friendly_name}, nv12={use_nv12}, profile={profile:?}, keyframe={saw_keyframe})"
            ),
        );
        if !profile_ok || !saw_keyframe {
            let message = format!(
                "MF encoder self-test failed decodability check (profile={profile:?} want Baseline, keyframe={saw_keyframe})"
            );
            logger::log(
                "WARN",
                "mf encoder",
                &format!("{message}; falling back to software"),
            );
            return Err(message);
        }
        Ok(encoder)
    }

    fn open_hardware_encoder() -> Result<(IMFTransform, String), String> {
        let input_info = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_NV12,
        };
        let output_info = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };
        let mut activates: *mut Option<IMFActivate> = null_mut();
        let mut count: u32 = 0;
        unsafe {
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG_HARDWARE,
                Some(&input_info as *const MFT_REGISTER_TYPE_INFO),
                Some(&output_info as *const MFT_REGISTER_TYPE_INFO),
                &mut activates,
                &mut count,
            )
            .map_err(hr)?;
        }
        if activates.is_null() || count == 0 {
            return Err("no hardware H.264 MFT found".into());
        }
        let first: IMFActivate =
            unsafe { (*activates).clone() }.ok_or("empty hardware MFT entry")?;
        unsafe {
            CoTaskMemFree(Some(activates as *const c_void));
        }
        let mut name_buffer = [0u16; 256];
        let mut name_length: u32 = 0;
        let name = match unsafe {
            first.GetString(
                &MFT_FRIENDLY_NAME_Attribute,
                &mut name_buffer,
                Some(&mut name_length),
            )
        } {
            Ok(()) => String::from_utf16_lossy(
                &name_buffer[..name_length as usize % (name_buffer.len() + 1)],
            )
            .trim_end_matches(char::from(0))
            .to_string(),
            Err(_) => "unknown".into(),
        };
        let transform: IMFTransform = unsafe { first.ActivateObject().map_err(hr)? };
        Ok((transform, name))
    }

    pub(crate) fn is_high_profile(&self) -> bool {
        self.profile_high
    }

    pub(crate) fn encode(&mut self, frame: &NativeFrame) -> Result<(), String> {
        if frame.width != self.width || frame.height != self.height {
            return Err(format!(
                "MF encoder frame size changed ({}x{} vs {}x{})",
                frame.width, frame.height, self.width, self.height
            ));
        }
        let bytes: Vec<u8> = if self.use_nv12 {
            bgra_to_nv12(frame.storage.as_ref(), frame.width, frame.height)
                .ok_or("MF NV12 conversion failed")?
        } else {
            let expected = (frame.width as usize) * (frame.height as usize) * 4;
            if frame.storage.len() < expected {
                return Err("MF frame storage is undersized".into());
            }
            frame.storage.as_ref()[..expected].to_vec()
        };
        let timestamp_90khz = frame.timestamp_micros.saturating_mul(9) / 100;
        // No B-frames, so output order matches input order: units out of this
        // call carry this frame timestamp.
        for sample in self.encode_bytes(&bytes, frame.timestamp_micros)? {
            let converted = self.converter.convert(&sample, timestamp_90khz, false);
            let Some(converted) = converted else {
                continue;
            };
            match self.output.try_push(converted) {
                AccessUnitPushResult::Enqueued => {}
                AccessUnitPushResult::DroppedUntilKeyframe => {
                    self.control.request_keyframe();
                }
                AccessUnitPushResult::Closed => {}
            }
        }
        Ok(())
    }

    // Feed one raw frame (RGB32 or NV12 bytes) and collect every output unit,
    // waiting briefly for the hardware when it lags behind the input.
    fn encode_bytes(
        &mut self,
        bytes: &[u8],
        timestamp_micros: u64,
    ) -> Result<Vec<Vec<u8>>, String> {
        self.feed_sample(bytes, timestamp_micros)?;
        let mut units = Vec::new();
        for _ in 0..DRAIN_RETRIES {
            let drained = self.drain_available()?;
            let empty = drained.is_empty();
            units.extend(drained);
            if !empty {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        // One last non-blocking drain so a slow first frame still surfaces.
        if units.is_empty() {
            units.extend(self.drain_available()?);
        }
        Ok(units)
    }

    fn feed_sample(&mut self, bytes: &[u8], timestamp_micros: u64) -> Result<(), String> {
        let sample = self.make_sample(bytes, timestamp_micros)?;
        match unsafe { self.transform.ProcessInput(0, &sample, 0) } {
            Ok(()) => Ok(()),
            Err(error) if error.code() == MF_E_NOTACCEPTING => {
                let _ = self.drain_available()?;
                let retry = self.make_sample(bytes, timestamp_micros)?;
                unsafe {
                    self.transform.ProcessInput(0, &retry, 0).map_err(hr)?;
                }
                Ok(())
            }
            Err(error) => Err(hr(error)),
        }
    }

    fn make_sample(&self, bytes: &[u8], timestamp_micros: u64) -> Result<IMFSample, String> {
        unsafe {
            let buffer = MFCreateMemoryBuffer(bytes.len() as u32).map_err(hr)?;
            {
                let mut pointer: *mut u8 = null_mut();
                let mut max_length: u32 = 0;
                let mut current_length: u32 = 0;
                buffer
                    .Lock(
                        &mut pointer,
                        Some(&mut max_length),
                        Some(&mut current_length),
                    )
                    .map_err(hr)?;
                if max_length < bytes.len() as u32 {
                    buffer.Unlock().map_err(hr)?;
                    return Err("MF input buffer too small".into());
                }
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer, bytes.len());
                buffer.SetCurrentLength(bytes.len() as u32).map_err(hr)?;
                buffer.Unlock().map_err(hr)?;
            }
            let sample = MFCreateSample().map_err(hr)?;
            sample.AddBuffer(&buffer).map_err(hr)?;
            sample
                .SetSampleTime(timestamp_micros as i64 * 10)
                .map_err(hr)?;
            sample
                .SetSampleDuration(10_000_000 / self.fps.max(1) as i64)
                .map_err(hr)?;
            Ok(sample)
        }
    }

    fn drain_available(&mut self) -> Result<Vec<Vec<u8>>, String> {
        let mut units = Vec::new();
        loop {
            let sample = unsafe {
                let buffer = MFCreateMemoryBuffer(self.output_buffer_size).map_err(hr)?;
                let mft_sample = MFCreateSample().map_err(hr)?;
                mft_sample.AddBuffer(&buffer).map_err(hr)?;
                let mut output = MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: ManuallyDrop::new(Some(mft_sample)),
                    dwStatus: 0,
                    pEvents: ManuallyDrop::new(None),
                };
                let mut status_flags: u32 = 0;
                match self.transform.ProcessOutput(
                    0,
                    std::slice::from_mut(&mut output),
                    &mut status_flags,
                ) {
                    Ok(()) => {}
                    Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                    Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                        eprintln!("[goDrinking] MF encoder stream changed mid-session");
                        break;
                    }
                    Err(error) if error.code() == MF_E_BUFFERTOOSMALL => {
                        if self.output_buffer_size >= OUTPUT_BUFFER_MAX {
                            return Err("MF encoder output exceeds buffer cap".into());
                        }
                        self.output_buffer_size =
                            (self.output_buffer_size * 2).min(OUTPUT_BUFFER_MAX);
                        continue;
                    }
                    Err(error) => {
                        eprintln!("[goDrinking] MF encoder output failed: {}", hr(error));
                        break;
                    }
                }
                let taken = ManuallyDrop::take(&mut output.pSample);
                let Some(taken) = taken else {
                    break;
                };
                let contiguous = taken.ConvertToContiguousBuffer().map_err(hr)?;
                let mut pointer: *mut u8 = null_mut();
                let mut max_length: u32 = 0;
                let mut current_length: u32 = 0;
                contiguous
                    .Lock(
                        &mut pointer,
                        Some(&mut max_length),
                        Some(&mut current_length),
                    )
                    .map_err(hr)?;
                let data = std::slice::from_raw_parts(pointer, current_length as usize).to_vec();
                contiguous.Unlock().map_err(hr)?;
                data
            };
            // Raw Annex-B sample; the caller converts and timestamps.
            units.push(sample);
        }
        Ok(units)
    }

    pub(crate) fn force_keyframe(&mut self) {
        if let Some(api) = &self.codec_api {
            let variant = variant_bool(true);
            match unsafe { api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &variant) } {
                Ok(()) => {}
                Err(error) => {
                    eprintln!("[goDrinking] MF force keyframe unavailable: {}", hr(error))
                }
            }
        }
    }

    pub(crate) fn set_bitrate(&mut self, bitrate: u32) -> Result<(), String> {
        self.bitrate = bitrate;
        if let Some(api) = &self.codec_api {
            set_codec_u32(
                api,
                &CODECAPI_AVEncCommonMeanBitRate,
                bitrate,
                "mean bitrate",
            );
            set_codec_u32(api, &CODECAPI_AVEncCommonMaxBitRate, bitrate, "max bitrate");
        }
        Ok(())
    }
}

impl Drop for MfH264Encoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
    }
}
