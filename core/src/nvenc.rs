//! Windows hardware H.264 encode (NVENC via Media Foundation, then QSV/AMF).
//!
//! Same contract as `vt.rs`: this file compiles everywhere; the real backend
//! is Windows-only. `media.rs` stays cfg-light. Probe-then-use:
//! - `EngineKind::Auto` probes a tiny NV12 frame; success → GPU, else OpenH264.
//! - `EngineKind::Hardware` fails hard (`HwUnavailable`) when absent.
//! - `GOLIVE_DISABLE_HW=1` forces probe failure (deterministic tests).
//!
//! Prefers NVIDIA NVENC, then Intel QSV, then AMD AMF. Input is NV12
//! (converted from I420 by [`crate::vt::i420_to_nv12`]); output is Annex-B
//! with SPS/PPS on every IDR — identical to the software/VideoToolbox paths.

/// Rank hardware MFTs so Auto always picks the strongest GPU encoder first.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn classify_hw_encoder(friendly: &str) -> &'static str {
    let name = friendly.to_ascii_lowercase();
    if name.contains("nvidia") || name.contains("nvenc") {
        "nvenc"
    } else if name.contains("intel") || name.contains("quick") {
        "qsv"
    } else if name.contains("amd") || name.contains("amf") {
        "amf"
    } else {
        "mfhw"
    }
}

#[cfg(target_os = "windows")]
pub use backend::{probe_hardware, NvencEncoder};

#[cfg(target_os = "windows")]
mod backend {
    use super::classify_hw_encoder;

    fn encoder_rank(kind: &str) -> u8 {
        match kind {
            "nvenc" => 0,
            "qsv" => 1,
            "amf" => 2,
            _ => 3,
        }
    }
    use crate::media::{MediaError, MAX_DIM};
    use crate::vt::avcc_to_annexb;
    use std::mem::ManuallyDrop;
    use std::ptr;
    use std::slice;
    use std::sync::OnceLock;
    use std::thread;
    use std::time::{Duration, Instant};
    use windows::core::{Interface, GUID, HRESULT};
    use windows::Win32::Foundation::VARIANT_BOOL;
    use windows::Win32::Media::MediaFoundation::*;
    use windows::Win32::System::Com::{
        CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED,
    };
    use windows::Win32::System::Variant::{
        VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4,
    };

    const DISABLE_ENV: &str = "GOLIVE_DISABLE_HW";
    const INPUT_STREAM: u32 = 0;
    const OUTPUT_STREAM: u32 = 0;
    const HNS_PER_SECOND: i64 = 10_000_000;
    const MAX_SKIPS: u32 = 30;
    const MAX_EVENTS: usize = 32;

    // ICodecAPI is absent from windows 0.62 metadata (codecapi.h). Thin
    // wrapper for SetValue only — same IID/vtable as the SDK.
    #[repr(transparent)]
    #[derive(Clone)]
    struct ICodecApi(windows::core::IUnknown);

    unsafe impl Interface for ICodecApi {
        type Vtable = ICodecApiVtbl;
        const IID: GUID = GUID::from_u128(0x901db4c7_31ce_41a2_85dc_8fa0bf41b8da);
    }

    impl ICodecApi {
        unsafe fn set_value(&self, key: &GUID, value: &VARIANT) -> windows::core::Result<()> {
            unsafe { (Interface::vtable(self).SetValue)(Interface::as_raw(self), key, value).ok() }
        }
    }

    #[repr(C)]
    #[allow(non_snake_case)]
    struct ICodecApiVtbl {
        base__: windows::core::IUnknown_Vtbl,
        IsSupported: usize,
        IsModifiable: usize,
        GetParameterRange: usize,
        GetParameterValues: usize,
        GetDefaultValue: usize,
        GetValue: usize,
        SetValue: unsafe extern "system" fn(
            *mut core::ffi::c_void,
            *const GUID,
            *const VARIANT,
        ) -> HRESULT,
        RegisterForEvent: usize,
        UnregisterForEvent: usize,
        SetAllDefaults: usize,
        SetValueWithNotify: usize,
        SetAllDefaultsWithNotify: usize,
        GetAllSettings: usize,
        SetAllSettings: usize,
        SetAllSettingsWithNotify: usize,
    }

    fn hw_err(detail: impl Into<String>) -> MediaError {
        MediaError::HwUnavailable(detail.into())
    }

    fn codec_err(detail: impl Into<String>) -> MediaError {
        MediaError::Codec(detail.into())
    }

    fn ensure_com_mf() -> Result<(), MediaError> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).map_err(|e| hw_err(format!("COM init {e}")))?;
        }
        static MF: OnceLock<Result<(), String>> = OnceLock::new();
        match MF.get_or_init(|| {
            unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }
                .map_err(|e| format!("MFStartup {e}"))
        }) {
            Ok(()) => Ok(()),
            Err(e) => Err(hw_err(e.clone())),
        }
    }

    pub fn probe_hardware() -> Result<(), MediaError> {
        if std::env::var_os(DISABLE_ENV).is_some() {
            return Err(hw_err("disabled by GOLIVE_DISABLE_HW (test hook)"));
        }
        let mut enc = NvencEncoder::new(320, 240, 500_000, 15, true)?;
        let mut nv12 = vec![16u8; 320 * 240];
        nv12.extend(std::iter::repeat(128u8).take(320 * 240 / 2));
        for _ in 0..8 {
            match enc.encode_nv12(&nv12) {
                Ok(Some(unit)) if !unit.is_empty() => return Ok(()),
                Ok(_) => {}
                Err(e) => return Err(e),
            }
        }
        Err(hw_err("probe produced no unit"))
    }

    pub struct NvencEncoder {
        transform: IMFTransform,
        events: IMFMediaEventGenerator,
        codec_api: ICodecApi,
        activation: IMFActivate,
        supplied_output: Option<IMFSample>,
        w: usize,
        h: usize,
        fps: u32,
        bitrate_bps: u32,
        pts: i64,
        sps_pps: Vec<u8>,
        consecutive_skips: u32,
        force_next: bool,
        need_input: bool,
        backend: &'static str,
    }

    unsafe impl Send for NvencEncoder {}

    impl NvencEncoder {
        pub fn new(
            w: usize,
            h: usize,
            bitrate_bps: u32,
            fps: u32,
            require_hw: bool,
        ) -> Result<Self, MediaError> {
            if std::env::var_os(DISABLE_ENV).is_some() {
                return Err(hw_err("disabled by GOLIVE_DISABLE_HW (test hook)"));
            }
            if !require_hw {
                return Err(hw_err("hardware required"));
            }
            if w < 2 || h < 2 || w > MAX_DIM as usize || h > MAX_DIM as usize || w % 2 != 0 || h % 2 != 0
            {
                return Err(codec_err(format!("hw dims must be even 2..={MAX_DIM}: {w}x{h}")));
            }
            ensure_com_mf()?;
            let mut activations = enumerate_hw_encoders()?;
            activations.sort_by_key(|item| encoder_rank(item.1));
            let mut last = hw_err("no NV12→H.264 hardware encoder");
            for (activate, kind) in activations {
                match Self::from_activation(activate, w, h, bitrate_bps, fps.max(1), kind) {
                    Ok(enc) => return Ok(enc),
                    Err(e) => last = e,
                }
            }
            Err(last)
        }

        fn from_activation(
            activation: IMFActivate,
            w: usize,
            h: usize,
            bitrate_bps: u32,
            fps: u32,
            backend: &'static str,
        ) -> Result<Self, MediaError> {
            let transform: IMFTransform = unsafe { activation.ActivateObject() }
                .map_err(|e| hw_err(format!("MFT activate {e}")))?;
            let events: IMFMediaEventGenerator = transform
                .cast()
                .map_err(|e| hw_err(format!("MFT not async {e}")))?;
            let codec_api: ICodecApi = transform
                .cast()
                .map_err(|e| hw_err(format!("MFT has no ICodecAPI {e}")))?;
            unlock_async(&transform)?;
            set_codec_u32(
                &codec_api,
                &CODECAPI_AVEncCommonRateControlMode,
                eAVEncCommonRateControlMode_CBR.0 as u32,
            )
            .ok();
            set_codec_u32(&codec_api, &CODECAPI_AVEncCommonMeanBitRate, bitrate_bps).ok();
            set_codec_u32(&codec_api, &CODECAPI_AVEncMPVGOPSize, fps.max(1) * 2).ok();
            set_codec_bool(&codec_api, &CODECAPI_AVLowLatencyMode, true).ok();
            configure_types(&transform, w, h, fps, bitrate_bps)?;
            let output_info = unsafe { transform.GetOutputStreamInfo(OUTPUT_STREAM) }
                .map_err(|e| hw_err(format!("output info {e}")))?;
            let provides = output_info.dwFlags
                & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0)
                    as u32
                != 0;
            let supplied_output = if provides {
                None
            } else {
                Some(empty_sample(output_info.cbSize.max(1))?)
            };
            let mut enc = Self {
                transform,
                events,
                codec_api,
                activation,
                supplied_output,
                w,
                h,
                fps,
                bitrate_bps,
                pts: 0,
                sps_pps: Vec::new(),
                consecutive_skips: 0,
                force_next: true,
                need_input: false,
                backend,
            };
            enc.start_stream()?;
            let _ = enc.bitrate_bps;
            Ok(enc)
        }

        pub fn backend_name(&self) -> &'static str {
            self.backend
        }

        pub fn dims(&self) -> (usize, usize) {
            (self.w, self.h)
        }

        pub fn force_intra(&mut self) {
            self.force_next = true;
        }

        pub fn encode_nv12(&mut self, nv12: &[u8]) -> Result<Option<Vec<u8>>, MediaError> {
            let expected = self.w * self.h * 3 / 2;
            if nv12.len() != expected {
                return Err(codec_err("nv12 size mismatch"));
            }
            self.wait_need_input()?;
            if self.force_next {
                set_codec_bool(&self.codec_api, &CODECAPI_AVEncVideoForceKeyFrame, true)
                    .map_err(|e| codec_err(format!("force keyframe {e}")))?;
                self.force_next = false;
            }
            let sample = nv12_sample(nv12, self.w, self.h, self.pts, self.fps)?;
            self.pts = self.pts.saturating_add(1);
            unsafe { self.transform.ProcessInput(INPUT_STREAM, &sample, 0) }
                .map_err(|e| codec_err(format!("ProcessInput {e}")))?;
            self.need_input = false;
            let wait = Duration::from_millis((4000 / u64::from(self.fps.max(1))).max(200));
            let deadline = Instant::now() + wait;
            for _ in 0..MAX_EVENTS {
                match self.next_event(deadline)? {
                    MftEvent::NeedInput => {
                        self.need_input = true;
                        self.consecutive_skips += 1;
                        if self.consecutive_skips > MAX_SKIPS {
                            return Err(codec_err("hw dropping every frame"));
                        }
                        return Ok(None);
                    }
                    MftEvent::HaveOutput => return self.process_output(),
                    MftEvent::Other => {}
                }
            }
            Err(codec_err("hw encode timeout"))
        }

        fn start_stream(&mut self) -> Result<(), MediaError> {
            unsafe {
                self.transform
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                    .and_then(|_| {
                        self.transform
                            .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                    })
            }
            .map_err(|e| hw_err(format!("stream start {e}")))?;
            self.need_input = false;
            self.wait_need_input()
        }

        fn wait_need_input(&mut self) -> Result<(), MediaError> {
            if self.need_input {
                return Ok(());
            }
            let wait = Duration::from_millis((4000 / u64::from(self.fps.max(1))).max(200));
            let deadline = Instant::now() + wait;
            for _ in 0..MAX_EVENTS {
                match self.next_event(deadline)? {
                    MftEvent::NeedInput => {
                        self.need_input = true;
                        return Ok(());
                    }
                    MftEvent::HaveOutput => {
                        let _ = self.process_output()?;
                    }
                    MftEvent::Other => {}
                }
            }
            Err(hw_err("MFT did not request input"))
        }

        fn next_event(&self, deadline: Instant) -> Result<MftEvent, MediaError> {
            loop {
                match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                    Ok(event) => {
                        let status = unsafe { event.GetStatus() }
                            .map_err(|e| codec_err(format!("event status {e}")))?;
                        status.ok().map_err(|e| codec_err(format!("async MFT {e}")))?;
                        let kind = unsafe { event.GetType() }
                            .map_err(|e| codec_err(format!("event type {e}")))?;
                        return Ok(if kind == METransformNeedInput.0 as u32 {
                            MftEvent::NeedInput
                        } else if kind == METransformHaveOutput.0 as u32 {
                            MftEvent::HaveOutput
                        } else {
                            MftEvent::Other
                        });
                    }
                    Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                        if Instant::now() >= deadline {
                            return Err(codec_err("hw encode timeout"));
                        }
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(e) => return Err(codec_err(format!("MFT event {e}"))),
                }
            }
        }

        fn process_output(&mut self) -> Result<Option<Vec<u8>>, MediaError> {
            if let Some(sample) = self.supplied_output.as_ref() {
                reset_sample(sample)?;
            }
            let supplied = self.supplied_output.clone();
            let mut output = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: OUTPUT_STREAM,
                pSample: ManuallyDrop::new(supplied),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            };
            let mut status = 0;
            let result = unsafe {
                self.transform
                    .ProcessOutput(0, slice::from_mut(&mut output), &mut status)
            };
            let sample = unsafe { ManuallyDrop::take(&mut output.pSample) };
            drop(unsafe { ManuallyDrop::take(&mut output.pEvents) });
            if let Err(e) = result {
                if e.code() == MF_E_TRANSFORM_STREAM_CHANGE {
                    self.handle_format_change()?;
                    return self.process_output();
                }
                if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT {
                    self.need_input = true;
                    return Ok(None);
                }
                return Err(codec_err(format!("ProcessOutput {e}")));
            }
            let Some(sample) = sample else {
                return Ok(None);
            };
            let clean = unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }.unwrap_or(0) != 0;
            let bytes = read_sample(&sample)?;
            let Some((mut annexb, is_idr)) = to_annexb(&bytes) else {
                return Err(codec_err("malformed H.264 unit"));
            };
            let is_idr = is_idr || clean;
            if is_idr {
                if let Some(header) = self.sequence_header()? {
                    self.sps_pps = header;
                }
                if !contains_nal(&annexb, 7) || !contains_nal(&annexb, 8) {
                    let mut full = self.sps_pps.clone();
                    full.extend_from_slice(&annexb);
                    annexb = full;
                }
            }
            if annexb.is_empty() {
                self.consecutive_skips += 1;
                if self.consecutive_skips > MAX_SKIPS {
                    return Err(codec_err("hw dropping every frame"));
                }
                return Ok(None);
            }
            self.consecutive_skips = 0;
            Ok(Some(annexb))
        }

        fn handle_format_change(&mut self) -> Result<(), MediaError> {
            let media_type = unsafe { self.transform.GetOutputAvailableType(OUTPUT_STREAM, 0) }
                .map_err(|e| codec_err(format!("output type change {e}")))?;
            unsafe { self.transform.SetOutputType(OUTPUT_STREAM, &media_type, 0) }
                .map_err(|e| codec_err(format!("re-SetOutputType {e}")))?;
            if let Some(header) = self.sequence_header()? {
                self.sps_pps = header;
            }
            Ok(())
        }

        fn sequence_header(&self) -> Result<Option<Vec<u8>>, MediaError> {
            let media_type = unsafe { self.transform.GetOutputCurrentType(OUTPUT_STREAM) }
                .map_err(|e| codec_err(format!("output type {e}")))?;
            let Ok(size) = (unsafe { media_type.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) }) else {
                return Ok(None);
            };
            if size == 0 {
                return Ok(None);
            }
            let mut bytes = vec![0u8; size as usize];
            unsafe { media_type.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut bytes, None) }
                .map_err(|e| codec_err(format!("sequence header {e}")))?;
            Ok(to_annexb(&bytes).map(|(unit, _)| unit).or_else(|| {
                if bytes.starts_with(&[0, 0, 1]) || bytes.starts_with(&[0, 0, 0, 1]) {
                    Some(bytes)
                } else {
                    None
                }
            }))
        }
    }

    impl Drop for NvencEncoder {
        fn drop(&mut self) {
            unsafe {
                let _ = self
                    .transform
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
                let _ = self
                    .transform
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
                let _ = self.activation.ShutdownObject();
            }
        }
    }

    enum MftEvent {
        NeedInput,
        HaveOutput,
        Other,
    }

    fn enumerate_hw_encoders() -> Result<Vec<(IMFActivate, &'static str)>, MediaError> {
        let input = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_NV12,
        };
        let output = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };
        let mut raw = ptr::null_mut();
        let mut count = 0u32;
        unsafe {
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_ASYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
                Some(&input),
                Some(&output),
                &mut raw,
                &mut count,
            )
        }
        .map_err(|e| hw_err(format!("MFTEnumEx {e}")))?;
        if raw.is_null() || count == 0 {
            if !raw.is_null() {
                unsafe { CoTaskMemFree(Some(raw.cast())) };
            }
            return Err(hw_err("no NV12→H.264 hardware encoder"));
        }
        let entries = unsafe { slice::from_raw_parts_mut(raw, count as usize) };
        let mut out = Vec::new();
        for slot in entries.iter_mut() {
            if let Some(activate) = slot.take() {
                let kind = classify_hw_encoder(&friendly_name(&activate));
                out.push((activate, kind));
            }
        }
        unsafe { CoTaskMemFree(Some(raw.cast())) };
        if out.is_empty() {
            Err(hw_err("no NV12→H.264 hardware encoder"))
        } else {
            Ok(out)
        }
    }

    fn friendly_name(activate: &IMFActivate) -> String {
        let mut raw = windows::core::PWSTR::null();
        let mut len = 0u32;
        if unsafe {
            activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut raw, &mut len)
        }
        .is_err()
            || raw.is_null()
        {
            return String::new();
        }
        let name = unsafe { raw.to_string() }.unwrap_or_default();
        unsafe { CoTaskMemFree(Some(raw.as_ptr().cast())) };
        name
    }

    fn unlock_async(transform: &IMFTransform) -> Result<(), MediaError> {
        let attributes = unsafe { transform.GetAttributes() }
            .map_err(|e| hw_err(format!("MFT attributes {e}")))?;
        let is_async = unsafe { attributes.GetUINT32(&MF_TRANSFORM_ASYNC) }.unwrap_or(0) != 0;
        if !is_async {
            return Err(hw_err("hardware MFT is not async"));
        }
        unsafe { attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }
            .map_err(|e| hw_err(format!("async unlock {e}")))
    }

    fn configure_types(
        transform: &IMFTransform,
        w: usize,
        h: usize,
        fps: u32,
        bitrate_bps: u32,
    ) -> Result<(), MediaError> {
        let output = video_type(MFVideoFormat_H264, w, h, fps, Some(bitrate_bps))?;
        unsafe { transform.SetOutputType(OUTPUT_STREAM, &output, 0) }
            .map_err(|e| hw_err(format!("SetOutputType {e}")))?;
        let input = video_type(MFVideoFormat_NV12, w, h, fps, None)?;
        unsafe { transform.SetInputType(INPUT_STREAM, &input, 0) }
            .map_err(|e| hw_err(format!("SetInputType {e}")))?;
        Ok(())
    }

    fn video_type(
        subtype: GUID,
        w: usize,
        h: usize,
        fps: u32,
        bitrate: Option<u32>,
    ) -> Result<IMFMediaType, MediaError> {
        let media_type =
            unsafe { MFCreateMediaType() }.map_err(|e| hw_err(format!("MFCreateMediaType {e}")))?;
        unsafe {
            media_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| hw_err(format!("major type {e}")))?;
            media_type
                .SetGUID(&MF_MT_SUBTYPE, &subtype)
                .map_err(|e| hw_err(format!("subtype {e}")))?;
            media_type
                .SetUINT64(&MF_MT_FRAME_SIZE, pack_pair(w as u32, h as u32))
                .map_err(|e| hw_err(format!("frame size {e}")))?;
            media_type
                .SetUINT64(&MF_MT_FRAME_RATE, pack_pair(fps.max(1), 1))
                .map_err(|e| hw_err(format!("frame rate {e}")))?;
            media_type
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(|e| hw_err(format!("interlace {e}")))?;
            if subtype == MFVideoFormat_NV12 {
                media_type
                    .SetUINT32(&MF_MT_DEFAULT_STRIDE, w as u32)
                    .map_err(|e| hw_err(format!("stride {e}")))?;
            }
            if let Some(bps) = bitrate {
                media_type
                    .SetUINT32(&MF_MT_AVG_BITRATE, bps)
                    .map_err(|e| hw_err(format!("bitrate {e}")))?;
                media_type
                    .SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32)
                    .ok();
            }
        }
        Ok(media_type)
    }

    fn empty_sample(capacity: u32) -> Result<IMFSample, MediaError> {
        let sample = unsafe { MFCreateSample() }.map_err(|e| hw_err(format!("sample {e}")))?;
        let buffer = unsafe { MFCreateMemoryBuffer(capacity) }
            .map_err(|e| hw_err(format!("buffer {e}")))?;
        unsafe { sample.AddBuffer(&buffer) }.map_err(|e| hw_err(format!("AddBuffer {e}")))?;
        Ok(sample)
    }

    fn nv12_sample(
        nv12: &[u8],
        w: usize,
        h: usize,
        pts: i64,
        fps: u32,
    ) -> Result<IMFSample, MediaError> {
        let sample = empty_sample(nv12.len() as u32)?;
        let buffer = unsafe { sample.GetBufferByIndex(0) }
            .map_err(|e| codec_err(format!("input buffer {e}")))?;
        let mut dest: *mut u8 = ptr::null_mut();
        let mut capacity = 0u32;
        unsafe { buffer.Lock(&mut dest, Some(&mut capacity), None) }
            .map_err(|e| codec_err(format!("lock {e}")))?;
        let copy = if nv12.len() <= capacity as usize {
            unsafe { slice::from_raw_parts_mut(dest, nv12.len()) }.copy_from_slice(nv12);
            Ok(())
        } else {
            Err(codec_err("input buffer too small"))
        };
        let unlock = unsafe { buffer.Unlock() };
        copy?;
        unlock.map_err(|e| codec_err(format!("unlock {e}")))?;
        unsafe { buffer.SetCurrentLength(nv12.len() as u32) }
            .map_err(|e| codec_err(format!("length {e}")))?;
        let duration = HNS_PER_SECOND / i64::from(fps.max(1));
        unsafe {
            sample
                .SetSampleTime(pts.saturating_mul(duration))
                .and_then(|_| sample.SetSampleDuration(duration))
        }
        .map_err(|e| codec_err(format!("timestamp {e}")))?;
        let _ = (w, h);
        Ok(sample)
    }

    fn reset_sample(sample: &IMFSample) -> Result<(), MediaError> {
        unsafe { sample.DeleteAllItems() }.map_err(|e| codec_err(format!("reset sample {e}")))?;
        let buffer = unsafe { sample.GetBufferByIndex(0) }
            .map_err(|e| codec_err(format!("reset buffer {e}")))?;
        unsafe { buffer.SetCurrentLength(0) }.map_err(|e| codec_err(format!("reset length {e}")))
    }

    fn read_sample(sample: &IMFSample) -> Result<Vec<u8>, MediaError> {
        let buffer = unsafe { sample.ConvertToContiguousBuffer() }
            .map_err(|e| codec_err(format!("contiguous {e}")))?;
        let length = unsafe { buffer.GetCurrentLength() }
            .map_err(|e| codec_err(format!("out length {e}")))?;
        if length == 0 {
            return Ok(Vec::new());
        }
        let mut src: *mut u8 = ptr::null_mut();
        unsafe { buffer.Lock(&mut src, None, None) }.map_err(|e| codec_err(format!("out lock {e}")))?;
        let bytes = unsafe { slice::from_raw_parts(src, length as usize) }.to_vec();
        unsafe { buffer.Unlock() }.map_err(|e| codec_err(format!("out unlock {e}")))?;
        Ok(bytes)
    }

    fn to_annexb(bytes: &[u8]) -> Option<(Vec<u8>, bool)> {
        if bytes.starts_with(&[0, 0, 0, 1]) || bytes.starts_with(&[0, 0, 1]) {
            Some((bytes.to_vec(), contains_nal(bytes, 5)))
        } else {
            avcc_to_annexb(bytes)
        }
    }

    fn contains_nal(annexb: &[u8], ty: u8) -> bool {
        let mut i = 0;
        while i + 4 < annexb.len() {
            let start = if annexb[i..].starts_with(&[0, 0, 0, 1]) {
                i + 4
            } else if annexb[i..].starts_with(&[0, 0, 1]) {
                i + 3
            } else {
                i += 1;
                continue;
            };
            if annexb.get(start).map(|b| b & 0x1F) == Some(ty) {
                return true;
            }
            i = start;
        }
        false
    }

    fn set_codec_u32(codec: &ICodecApi, key: &GUID, value: u32) -> Result<(), MediaError> {
        let variant = variant_u32(value);
        unsafe { codec.set_value(key, &variant) }
            .map_err(|e| hw_err(format!("codec u32 {e}")))
    }

    fn set_codec_bool(codec: &ICodecApi, key: &GUID, value: bool) -> Result<(), MediaError> {
        let variant = variant_bool(value);
        unsafe { codec.set_value(key, &variant) }
            .map_err(|e| hw_err(format!("codec bool {e}")))
    }

    fn variant_u32(value: u32) -> VARIANT {
        VARIANT {
            Anonymous: VARIANT_0 {
                Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                    vt: VT_UI4,
                    Anonymous: VARIANT_0_0_0 { ulVal: value },
                    ..Default::default()
                }),
            },
        }
    }

    fn variant_bool(value: bool) -> VARIANT {
        VARIANT {
            Anonymous: VARIANT_0 {
                Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                    vt: VT_BOOL,
                    Anonymous: VARIANT_0_0_0 {
                        boolVal: VARIANT_BOOL(if value { -1 } else { 0 }),
                    },
                    ..Default::default()
                }),
            },
        }
    }

    const fn pack_pair(high: u32, low: u32) -> u64 {
        ((high as u64) << 32) | low as u64
    }
}

#[cfg(not(target_os = "windows"))]
#[allow(dead_code)]
mod backend {
    use crate::media::MediaError;

    pub fn probe_hardware() -> Result<(), MediaError> {
        Err(MediaError::HwUnavailable("NVENC is Windows-only".into()))
    }

    pub struct NvencEncoder;

    impl NvencEncoder {
        pub fn new(
            _w: usize,
            _h: usize,
            _bitrate_bps: u32,
            _fps: u32,
            _require_hw: bool,
        ) -> Result<Self, MediaError> {
            Err(MediaError::HwUnavailable("NVENC is Windows-only".into()))
        }

        pub fn force_intra(&mut self) {}

        pub fn encode_nv12(&mut self, _nv12: &[u8]) -> Result<Option<Vec<u8>>, MediaError> {
            Err(MediaError::HwUnavailable("NVENC is Windows-only".into()))
        }

        pub fn dims(&self) -> (usize, usize) {
            (0, 0)
        }

        pub fn backend_name(&self) -> &'static str {
            "nvenc"
        }
    }
}

#[cfg(not(target_os = "windows"))]
#[allow(unused_imports)]
pub use backend::{probe_hardware, NvencEncoder};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_prefers_vendor_names() {
        assert_eq!(classify_hw_encoder("NVIDIA H.264 Encoder MFT"), "nvenc");
        assert_eq!(
            classify_hw_encoder("Intel® Quick Sync Video H.264 Encoder MFT"),
            "qsv"
        );
        assert_eq!(classify_hw_encoder("AMDh264Encoder"), "amf");
        assert_eq!(classify_hw_encoder("Mystery GPU Encoder"), "mfhw");
    }
}
