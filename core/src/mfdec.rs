//! Windows hardware H.264 decode (DXVA via a Media Foundation decoder MFT).
//!
//! Mirror of the encoder side (`nvenc.rs`): this file compiles everywhere;
//! the real backend is Windows-only. Selection is probe-then-use with
//! transparent software fallback — a failed probe or a mid-stream hardware
//! fault never blacks the picture, the caller just keeps decoding in
//! software (see `media::H264Decoder`).
//!
//! Wire contract: Annex-B access units in (our ecosystem disables B-frames
//! on encode, so output order == input order — no reordering buffer).
//! Pictures come out as tight NV12; RGBA conversion is the pure
//! [`nv12_to_rgba`] below (tested on every platform).
//!
//! Setup is lazy: `new()` only enumerates + activates. The first IDR unit
//! carries SPS/PPS, which become the input sequence header; the output
//! size is adopted from the MFT (with `MF_E_TRANSFORM_STREAM_CHANGE`
//! handled, so mid-stream resolution switches survive).

/// One decoded picture in tight NV12 (`w*h*3/2` bytes, no padding).
pub struct DecodedNv12 {
    pub w: usize,
    pub h: usize,
    pub nv12: Vec<u8>,
}

/// NV12 (BT.601 limited range, the MFT output) to packed RGBA. Pure;
/// `None` on degenerate or short input (never panics). Integer BT.601:
///
/// ```text
/// R = (298*(Y-16) + 409*(V-128) + 128) >> 8
/// G = (298*(Y-16) - 100*(U-128) - 208*(V-128) + 128) >> 8
/// B = (298*(Y-16) + 516*(U-128) + 128) >> 8
/// ```
pub fn nv12_to_rgba(w: usize, h: usize, nv12: &[u8]) -> Option<Vec<u8>> {
    if w < 2 || h < 2 || w % 2 != 0 || h % 2 != 0 {
        return None;
    }
    if nv12.len() != w * h * 3 / 2 {
        return None;
    }
    fn clamp8(v: i32) -> u8 {
        v.clamp(0, 255) as u8
    }
    let (y_plane, uv_plane) = nv12.split_at(w * h);
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let c = y_plane[y * w + x] as i32 - 16;
            let d = uv_plane[(y / 2) * w + (x & !1)] as i32 - 128;
            let e = uv_plane[(y / 2) * w + (x & !1) + 1] as i32 - 128;
            let o = (y * w + x) * 4;
            out[o] = clamp8((298 * c + 409 * e + 128) >> 8);
            out[o + 1] = clamp8((298 * c - 100 * d - 208 * e + 128) >> 8);
            out[o + 2] = clamp8((298 * c + 516 * d + 128) >> 8);
            out[o + 3] = 255;
        }
    }
    Some(out)
}

#[cfg(target_os = "windows")]
pub use backend::{probe_hardware, MfDecoder};

#[cfg(target_os = "windows")]
mod backend {
    use super::DecodedNv12;
    use crate::media::{annexb_nals, nal_type, MediaError, MAX_DIM};
    use std::mem::ManuallyDrop;
    use std::ptr;
    use std::slice;
    use std::sync::OnceLock;
    use windows::core::Interface;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::{
        D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0,
        D3D_FEATURE_LEVEL_11_1,
    };
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, ID3D11Device, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
    };
    use windows::Win32::Media::MediaFoundation::*;
    use windows::Win32::System::Com::{
        CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED,
    };

    const DISABLE_ENV: &str = "GOLIVE_DISABLE_HW";
    const INPUT_STREAM: u32 = 0;
    const OUTPUT_STREAM: u32 = 0;
    const HNS_PER_SECOND: i64 = 10_000_000;
    const MAX_EVENTS: usize = 64;

    const FEATURE_LEVELS: [D3D_FEATURE_LEVEL; 2] =
        [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];

    // ICodecAPI is absent from windows 0.62 metadata (codecapi.h). Thin
    // wrapper for Set — same IID/vtable as the SDK. Only the acceleration
    // toggle write is needed (see reset_with_sequence).
    #[repr(transparent)]
    #[derive(Clone)]
    struct ICodecApi(windows::core::IUnknown);

    unsafe impl Interface for ICodecApi {
        type Vtable = ICodecApiVtbl;
        const IID: GUID = GUID::from_u128(0x901db4c7_31ce_41a2_85dc_8fa0bf41b8da);
    }

    impl ICodecApi {
        unsafe fn set_u32(&self, key: &GUID, value: u32) -> windows::core::Result<()> {
            use windows::Win32::System::Variant::{
                VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_UI4,
            };
            let variant = VARIANT {
                Anonymous: VARIANT_0 {
                    Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                        vt: VT_UI4,
                        Anonymous: VARIANT_0_0_0 { ulVal: value },
                        ..Default::default()
                    }),
                },
            };
            unsafe { (Interface::vtable(self).SetValue)(Interface::as_raw(self), key, &variant).ok() }
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

    use windows::Win32::System::Variant::VARIANT;
    use windows::core::HRESULT;
    use windows::core::GUID;

    fn hw_err(detail: impl Into<String>) -> MediaError {
        MediaError::HwUnavailable(detail.into())
    }

    fn codec_err(detail: impl Into<String>) -> MediaError {
        MediaError::Codec(detail.into())
    }

    fn ensure_com_mf() -> Result<(), MediaError> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED)
                .ok()
                .map_err(|e| hw_err(format!("COM init {e}")))?;
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
        let mut dec = MfDecoder::new()?;
        // One synthetic IDR through the real path (OpenH264 encodes the
        // probe unit deterministically — no network, no fixtures).
        let profile =
            crate::media::QualityProfile::custom(320, 240, 500, 15).map_err(|e| {
                codec_err(format!("probe profile: {e}"))
            })?;
        let mut enc =
            crate::media::H264Encoder::new_with_profile(&profile, 320, 240)
                .map_err(|e| hw_err(format!("probe encode: {e}")))?;
        let frame = crate::media::I420Frame {
            w: 320,
            h: 240,
            data: vec![128u8; 320 * 240 * 3 / 2],
        };
        let unit = enc
            .encode(&frame)
            .map_err(|e| hw_err(format!("probe encode: {e}")))?;
        match dec.decode_annexb(&unit)? {
            Some(pic) if pic.w == 320 && pic.h == 240 && !pic.nv12.is_empty() => Ok(()),
            Some(_) => Err(hw_err("probe decoded wrong size")),
            None => Err(hw_err("probe produced no picture")),
        }
    }

    pub struct MfDecoder {
        transform: IMFTransform,
        activation: IMFActivate,
        /// Keeps the D3D device manager alive (the MFT only borrows it).
        _manager: IMFDXGIDeviceManager,
        /// Keeps the D3D device alive (the manager borrows it).
        _device: ID3D11Device,
        seqhdr: Vec<u8>,
        out_w: usize,
        out_h: usize,
        /// False until the authoritative output size has been adopted
        /// (first picture / stream change); guards the first read.
        out_confirmed: bool,
        /// Caller-supplied output sample for MFTs that cannot provide
        /// their own (keyed by the stream's `cbSize`); recreated on
        /// stream change. The MFT never owns it.
        out_sample: Option<(u32, IMFSample)>,
        /// Minimum input buffer size from `GetInputStreamInfo` (the MFT
        /// stalls on smaller buffers — observed eternal NEED_MORE_INPUT
        /// with exact-size 266-byte probe buffers).
        in_min: u32,
        pts: i64,
        types_set: bool,
        backend: &'static str,
    }

    unsafe impl Send for MfDecoder {}

    impl MfDecoder {
        pub fn new() -> Result<Self, MediaError> {
            if std::env::var_os(DISABLE_ENV).is_some() {
                return Err(hw_err("disabled by GOLIVE_DISABLE_HW (test hook)"));
            }
            ensure_com_mf()?;
            let (device, manager) = create_manager()?;
            let mut last = hw_err("no H.264 hardware decoder");
            // Broad sync enumeration (the inbox decoder registers neither
            // ASYNCMFT nor the exact H264→NV12 pair, yet accelerates over
            // DXVA once handed the D3D manager). Prefer the inbox decoder,
            // then any H264 decoder by name.
            let activations = match enumerate_decoders() {
                Ok(list) => list,
                Err(e) => return Err(e),
            };
            for (activate, kind) in activations {
                match Self::from_activation(activate, &device, &manager, kind) {
                    Ok(dec) => return Ok(dec),
                    Err(e) => last = e,
                }
            }
            Err(last)
        }

        pub fn backend_name(&self) -> &'static str {
            self.backend
        }

        /// Decodes one Annex-B access unit. `Ok(None)` is transient (the MFT
        /// needs more input — startup buffering); `Err` is fatal for this
        /// instance (the caller falls back to software).
        pub fn decode_annexb(&mut self, unit: &[u8]) -> Result<Option<DecodedNv12>, MediaError> {
            if unit.is_empty() {
                return Ok(None);
            }
            // New SPS/PPS: (re)build the input type around it. Mid-stream
            // switches rebuild the whole MFT (rare: resolution change).
            if let Some(seqhdr) = extract_seqhdr(unit) {
                if seqhdr != self.seqhdr {
                    self.reset_with_sequence(seqhdr)?;
                }
            }
            if !self.types_set {
                return Err(codec_err("no SPS/PPS seen yet"));
            }
            let is_idr = annexb_nals(unit)
                .iter()
                .any(|range| nal_type(&unit[range.clone()]) == Some(5));
            let sample = annexb_sample(unit, self.pts, is_idr, self.in_min)?;
            self.pts = self.pts.saturating_add(1);
            unsafe { self.transform.ProcessInput(INPUT_STREAM, &sample, 0) }
                .map_err(|e| codec_err(format!("ProcessInput {e}")))?;
            // Sync MFT: drain outputs until it asks for more input. Our
            // ecosystem emits no B-frames, so at most one picture waits.
            // Before each drain step, re-read the offered output type: the
            // decoder refines its offer once the sequence is known, and it
            // may wait for us to adopt the refined offer instead of firing
            // MF_E_TRANSFORM_STREAM_CHANGE on its own.
            for _ in 0..MAX_EVENTS {
                self.adopt_refined_offer()?;
                match self.process_output_once()? {
                    Some(pic) => return Ok(Some(pic)),
                    None => return Ok(None),
                }
            }
            Err(codec_err("hw decode timeout"))
        }

        /// Re-adopts the offered output type when the MFT refined it past
        /// what we set (e.g. placeholder 1920x1080 -> stream 320x240 once
        /// the sequence is known). Returns Ok always; adoption is validated.
        fn adopt_refined_offer(&mut self) -> Result<(), MediaError> {
            let offered = unsafe { self.transform.GetOutputAvailableType(OUTPUT_STREAM, 0) }
                .map_err(|e| codec_err(format!("output available type {e}")))?;
            let packed = unsafe { offered.GetUINT64(&MF_MT_FRAME_SIZE) }
                .map_err(|e| codec_err(format!("offered frame size {e}")))?;
            let (w, h) = ((packed >> 32) as usize, (packed & 0xffff_ffff) as usize);
            if w < 2 || h < 2 || w == self.out_w && h == self.out_h {
                return Ok(());
            }
            unsafe { self.transform.SetOutputType(OUTPUT_STREAM, &offered, 0) }
                .map_err(|e| codec_err(format!("refined SetOutputType {e}")))?;
            self.read_offered_size(&offered);
            self.out_confirmed = false;
            self.out_sample = None;
            Ok(())
        }

        fn from_activation(
            activation: IMFActivate,
            device: &ID3D11Device,
            manager: &IMFDXGIDeviceManager,
            backend: &'static str,
        ) -> Result<Self, MediaError> {
            let transform: IMFTransform = unsafe { activation.ActivateObject() }
                .map_err(|e| hw_err(format!("MFT activate {e}")))?;
            let result =
                Self::from_transform(transform, device, manager, backend, activation.clone());
            if result.is_err() {
                unsafe {
                    let _ = activation.ShutdownObject();
                }
            }
            result
        }

        fn from_transform(
            transform: IMFTransform,
            device: &ID3D11Device,
            manager: &IMFDXGIDeviceManager,
            backend: &'static str,
            activation: IMFActivate,
        ) -> Result<Self, MediaError> {
            // Sync MFTs only here (the inbox decoder): async vendor MFTs
            // stay a future step — failing closed keeps selection honest.
            require_sync(&transform)?;
            // DXVA on: the acceleration toggle reads 0 (off) by default.
            // Forced in reset_with_sequence (after streaming starts —
            // earlier sets are lost on SetInputType/BEGIN). Best-effort;
            // the end-to-end probe is the real gate.
            let raw = windows::core::Interface::as_raw(manager) as usize;
            unsafe {
                transform
                    .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, raw)
                    .map_err(|e| hw_err(format!("SET_D3D_MANAGER {e}")))?
            };
            let manager = manager.clone();
            // Lifetimes follow the handles: the MFT borrows the manager,
            // the manager borrows the device — both stay stored below.
            let dec = Self {
                transform,
                activation,
                _manager: manager,
                _device: device.clone(),
                seqhdr: Vec::new(),
                out_w: 0,
                out_h: 0,
                out_confirmed: false,
                out_sample: None,
                pts: 0,
                in_min: 0,
                types_set: false,
                backend,
            };
            Ok(dec)
        }

        /// Full rebuild around a new sequence header (resolution switch).
        /// Streaming also starts here, lazily: types first, because
        /// starting the MFT typeless crashes it (observed AV on
        /// BEGIN_STREAMING with no types set).
        fn reset_with_sequence(&mut self, seqhdr: Vec<u8>) -> Result<(), MediaError> {
            unsafe {
                let _ = self
                    .transform
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
                let _ = self
                    .transform
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            }
            set_input_type(&self.transform, &seqhdr)?;
            // Minimum input buffer size: the MFT stalls forever on smaller
            // buffers (observed eternal NEED_MORE_INPUT with exact-size
            // probe buffers). Queried per stream, like the output size.
            let mut stream_info = MFT_INPUT_STREAM_INFO::default();
            self.in_min = unsafe {
                self.transform
                    .GetInputStreamInfo(INPUT_STREAM, &mut stream_info)
                    .map(|()| stream_info.cbSize)
            }
            .unwrap_or(0);
            // No output type yet: it is adopted from the offer once input
            // exists (setting the pre-input 1920x1080 placeholder up front
            // appears to wedge the MFT — eternal NEED_MORE_INPUT with no
            // STREAM_CHANGE ever firing). See adopt_refined_offer.
            self.out_w = 0;
            self.out_h = 0;
            unsafe {
                self.transform
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                    .and_then(|_| {
                        self.transform
                            .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                    })
            }
            .map_err(|e| codec_err(format!("stream restart {e}")))?;
            // DXVA on, forced AFTER streaming starts: the toggle is VT_UI4
            // (a VT_BOOL set is rejected) and reads 0 by default; earlier
            // sets are lost on SetInputType. Best-effort; the end-to-end
            // probe is the real gate.
            if let Ok(api) = windows::core::Interface::cast::<ICodecApi>(&self.transform)
            {
                unsafe {
                    let _ = api.set_u32(&CODECAPI_AVDecVideoAcceleration_H264, 1);
                }
            }
            self.seqhdr = seqhdr;
            self.types_set = true;
            self.out_confirmed = false;
            Ok(())
        }

        /// Best-effort read of the offered output size (informational only;
        /// the authoritative dims come from `adopt_output_type`).
        fn read_offered_size(&mut self, offered: &IMFMediaType) {
            if let Ok(packed) = unsafe { offered.GetUINT64(&MF_MT_FRAME_SIZE) } {
                let (w, h) = ((packed >> 32) as usize, (packed & 0xffff_ffff) as usize);
                if w >= 2 && h >= 2 && w <= MAX_DIM as usize && h <= MAX_DIM as usize {
                    self.out_w = w;
                    self.out_h = h;
                }
            }
        }

        /// One synchronous drain step: a picture, a drain signal, or a
        /// fatal error. Stream changes are adopted internally (bounded
        /// retries, then fatal).
        fn process_output_once(&mut self) -> Result<Option<DecodedNv12>, MediaError> {
            for _ in 0..4 {
                match self.try_output_once()? {
                    OutputOutcome::Picture(pic) => return Ok(Some(pic)),
                    OutputOutcome::NeedMoreInput => return Ok(None),
                    OutputOutcome::StreamChanged => {
                        self.adopt_output_type()?;
                        self.out_confirmed = true;
                        self.out_sample = None;
                    }
                }
            }
            Err(codec_err("repeated stream change"))
        }

        fn try_output_once(&mut self) -> Result<OutputOutcome, MediaError> {
            let info = unsafe { self.transform.GetOutputStreamInfo(OUTPUT_STREAM) }
                .map_err(|e| codec_err(format!("output info {e}")))?;
            let provides = info.dwFlags
                & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0
                    | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32
                != 0;
            // Sync decoders that cannot provide samples need a caller
            // buffer sized by the stream (`cbSize`): pitched decoder rows
            // would overflow a dims-sized guess (observed AV). The MFT
            // never owns it; recreated whenever the stream changes size.
            let supplied = if provides {
                None
            } else {
                let size = info.cbSize.max(1);
                let sample = match self.out_sample.as_ref() {
                    Some((cached, sample)) if *cached == size => {
                        reset_output_sample(sample)?;
                        sample.clone()
                    }
                    _ => {
                        let sample = nv12_output_sample(size)?;
                        self.out_sample = Some((size, sample.clone()));
                        sample
                    }
                };
                Some(sample)
            };
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
                    return Ok(OutputOutcome::StreamChanged);
                }
                if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT {
                    return Ok(OutputOutcome::NeedMoreInput);
                }
                return Err(codec_err(format!("ProcessOutput {e}")));
            }
            let Some(sample) = sample else {
                return Ok(OutputOutcome::NeedMoreInput);
            };
            // First picture (or post-change): the offered guess is not the
            // stream — adopt the authoritative dims before reading a byte.
            if !self.out_confirmed {
                self.adopt_output_type()?;
                self.out_confirmed = true;
            }
            let (w, h) = (self.out_w, self.out_h);
            Ok(OutputOutcome::Picture(read_nv12(&sample, w, h)?))
        }

        /// Adopts the MFT's current output size (called after stream-change
        /// and when the first picture arrives before any size is known).
        fn adopt_output_type(&mut self) -> Result<(), MediaError> {
            let media_type = unsafe { self.transform.GetOutputCurrentType(OUTPUT_STREAM) }
                .map_err(|e| codec_err(format!("output current type {e}")))?;
            let packed = unsafe { media_type.GetUINT64(&MF_MT_FRAME_SIZE) }
                .map_err(|e| codec_err(format!("output frame size {e}")))?;
            let (w, h) = ((packed >> 32) as usize, (packed & 0xffff_ffff) as usize);
            if w < 2 || h < 2 || w > MAX_DIM as usize || h > MAX_DIM as usize || w % 2 != 0 || h % 2 != 0
            {
                return Err(codec_err(format!("hw decode dims invalid: {w}x{h}")));
            }
            self.out_w = w;
            self.out_h = h;
            Ok(())
        }
    }

    impl Drop for MfDecoder {
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

    enum OutputOutcome {
        Picture(DecodedNv12),
        NeedMoreInput,
        StreamChanged,
    }

    /// SPS+PPS (NAL 7+8) out of one Annex-B unit, concatenated with start
    /// codes — the sequence-header shape the inbox decoder accepts.
    fn extract_seqhdr(unit: &[u8]) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        for range in annexb_nals(unit) {
            if matches!(nal_type(&unit[range.clone()]), Some(7) | Some(8)) {
                out.extend_from_slice(&[0, 0, 0, 1]);
                out.extend_from_slice(&unit[range]);
            }
        }
        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }

    /// Broad sync enumeration: the inbox decoder registers neither the
    /// async flag nor the exact H264→NV12 pair, yet accelerates over DXVA
    /// once handed the D3D manager. Inbox first, then any H264 decoder.
    pub fn enumerate_decoders() -> Result<Vec<(IMFActivate, &'static str)>, MediaError> {
        let mut raw = ptr::null_mut();
        let mut count = 0u32;
        unsafe {
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_DECODER,
                MFT_ENUM_FLAG_SORTANDFILTER,
                None,
                None,
                &mut raw,
                &mut count,
            )
        }
        .map_err(|e| hw_err(format!("MFTEnumEx {e}")))?;
        if raw.is_null() || count == 0 {
            if !raw.is_null() {
                unsafe { CoTaskMemFree(Some(raw.cast())) };
            }
            return Err(hw_err("no video decoder MFT"));
        }
        let entries = unsafe { slice::from_raw_parts_mut(raw, count as usize) };
        let mut inbox = Vec::new();
        let mut rest = Vec::new();
        for slot in entries.iter_mut() {
            if let Some(activate) = slot.take() {
                let name = friendly_name(&activate);
                if !name.to_ascii_lowercase().contains("h264") {
                    unsafe {
                        let _ = activate.ShutdownObject();
                    }
                    continue;
                }
                let kind = classify_decoder(&name);
                if name == "Microsoft H264 Video Decoder MFT" {
                    inbox.push((activate, kind));
                } else {
                    rest.push((activate, kind));
                }
            }
        }
        unsafe { CoTaskMemFree(Some(raw.cast())) };
        inbox.extend(rest);
        if inbox.is_empty() {
            Err(hw_err("no H.264 decoder MFT"))
        } else {
            Ok(inbox)
        }
    }

    fn classify_decoder(friendly: &str) -> &'static str {
        let name = friendly.to_ascii_lowercase();
        if name.contains("nvidia") {
            "nvdec"
        } else if name.contains("intel") || name.contains("quick") {
            "qsvdec"
        } else if name.contains("amd") {
            "amfdec"
        } else {
            "mfhw"
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

    /// Sync MFTs only here (the inbox decoder): async vendor MFTs stay a
    /// future step — failing closed keeps selection honest instead of
    /// running an async contract through sync calls.
    fn require_sync(transform: &IMFTransform) -> Result<(), MediaError> {
        let attributes = unsafe { transform.GetAttributes() }
            .map_err(|e| hw_err(format!("MFT attributes {e}")))?;
        let is_async = unsafe { attributes.GetUINT32(&MF_TRANSFORM_ASYNC) }.unwrap_or(0) != 0;
        if is_async {
            return Err(hw_err("decoder MFT is async"));
        }
        Ok(())
    }

    /// D3D11 device + DXGI device manager for the MFT (DXVA or nothing).
    fn create_manager() -> Result<(ID3D11Device, IMFDXGIDeviceManager), MediaError> {
        let device = d3d_device_for_manager()?;
        let mut token = 0u32;
        let mut manager = None;
        unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager) }
            .map_err(|e| hw_err(format!("device manager {e}")))?;
        let Some(manager) = manager else {
            return Err(hw_err("device manager empty"));
        };
        let unknown: windows::core::IUnknown = device
            .cast()
            .map_err(|e| hw_err(format!("device cast {e}")))?;
        unsafe { manager.ResetDevice(&unknown, token) }
            .map_err(|e| hw_err(format!("ResetDevice {e}")))?;
        Ok((device, manager))
    }

    fn d3d_device_for_manager() -> Result<ID3D11Device, MediaError> {
        let mut device = None;
        let mut _context = None;
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&FEATURE_LEVELS),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut _context),
            )
        }
        .map_err(|e| hw_err(format!("D3D11CreateDevice {e}")))?;
        device.ok_or_else(|| hw_err("D3D11 device empty"))
    }

    fn set_input_type(transform: &IMFTransform, seqhdr: &[u8]) -> Result<(), MediaError> {
        let media_type =
            unsafe { MFCreateMediaType() }.map_err(|e| hw_err(format!("MFCreateMediaType {e}")))?;
        unsafe {
            media_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| hw_err(format!("major type {e}")))?;
            // Annex-B elementary stream: our ecosystem's wire shape. (The
            // AVCC-framed MFVideoFormat_H264 subtype was also tried during
            // bringup with AVCC samples — same silent stall.)
            media_type
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264_ES)
                .map_err(|e| hw_err(format!("subtype {e}")))?;
            media_type
                .SetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, seqhdr)
                .map_err(|e| hw_err(format!("sequence header {e}")))?;
            media_type
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(|e| hw_err(format!("interlace {e}")))?;
            transform
                .SetInputType(INPUT_STREAM, &media_type, 0)
                .map_err(|e| hw_err(format!("SetInputType {e}")))?;
        }
        Ok(())
    }

    /// Annex-B unit in a sample with a buffer of at least `min_size`
    /// bytes (see `in_min`): undersized input buffers stall some MFTs
    /// without any error. The payload length stays exact.
    fn annexb_sample(
        unit: &[u8],
        pts: i64,
        is_idr: bool,
        min_size: u32,
    ) -> Result<IMFSample, MediaError> {
        let sample =
            unsafe { MFCreateSample() }.map_err(|e| codec_err(format!("sample {e}")))?;
        let capacity = (unit.len() as u32).max(min_size).max(1);
        let buffer = unsafe { MFCreateMemoryBuffer(capacity) }
            .map_err(|e| codec_err(format!("buffer {e}")))?;
        let mut dest: *mut u8 = ptr::null_mut();
        let mut capacity = 0u32;
        unsafe { buffer.Lock(&mut dest, Some(&mut capacity), None) }
            .map_err(|e| codec_err(format!("lock {e}")))?;
        let copy = if (unit.len() as u32) <= capacity {
            unsafe { slice::from_raw_parts_mut(dest, unit.len()) }.copy_from_slice(unit);
            Ok(())
        } else {
            Err(codec_err("input buffer too small"))
        };
        let unlock = unsafe { buffer.Unlock() };
        copy?;
        unlock.map_err(|e| codec_err(format!("unlock {e}")))?;
        unsafe { buffer.SetCurrentLength(unit.len() as u32) }
            .map_err(|e| codec_err(format!("length {e}")))?;
        unsafe { sample.AddBuffer(&buffer) }.map_err(|e| codec_err(format!("AddBuffer {e}")))?;
        let duration = HNS_PER_SECOND / 30;
        unsafe {
            sample
                .SetSampleTime(pts.saturating_mul(duration))
                .and_then(|_| sample.SetSampleDuration(duration))
                .and_then(|_| {
                    if is_idr {
                        sample.SetUINT32(&MFSampleExtension_CleanPoint, 1)
                    } else {
                        Ok(())
                    }
                })
        }
        .map_err(|e| codec_err(format!("timestamp {e}")))?;
        Ok(sample)
    }

    /// Fresh caller-owned output sample of `cbSize` bytes (see
    /// `try_output_once`).
    fn nv12_output_sample(size: u32) -> Result<IMFSample, MediaError> {
        let sample =
            unsafe { MFCreateSample() }.map_err(|e| codec_err(format!("sample {e}")))?;
        let buffer = unsafe { MFCreateMemoryBuffer(size) }
            .map_err(|e| codec_err(format!("buffer {e}")))?;
        unsafe { sample.AddBuffer(&buffer) }.map_err(|e| codec_err(format!("AddBuffer {e}")))?;
        Ok(sample)
    }

    /// Rewinds a reused output sample for the next picture.
    fn reset_output_sample(sample: &IMFSample) -> Result<(), MediaError> {
        unsafe { sample.DeleteAllItems() }.map_err(|e| codec_err(format!("reset sample {e}")))?;
        let buffer = unsafe { sample.GetBufferByIndex(0) }
            .map_err(|e| codec_err(format!("reset buffer {e}")))?;
        unsafe { buffer.SetCurrentLength(0) }.map_err(|e| codec_err(format!("reset length {e}")))
    }

    /// Copies one NV12 output picture into tight rows (GPU readback happens
    /// inside `Lock2D` for DXVA surfaces).
    fn read_nv12(sample: &IMFSample, w: usize, h: usize) -> Result<DecodedNv12, MediaError> {
        let buffer = unsafe { sample.ConvertToContiguousBuffer() }
            .map_err(|e| codec_err(format!("contiguous {e}")))?;
        let buffer2d: IMF2DBuffer = buffer
            .cast()
            .map_err(|e| codec_err(format!("not 2D {e}")))?;
        let mut ptr: *mut u8 = ptr::null_mut();
        let mut pitch = 0i32;
        unsafe { buffer2d.Lock2D(&mut ptr, &mut pitch) }
            .map_err(|e| codec_err(format!("Lock2D {e}")))?;
        let result = if ptr.is_null() || pitch < w as i32 {
            Err(codec_err("Lock2D gave no rows"))
        } else {
            let pitch = pitch as usize;
            let mut nv12 = vec![0u8; w * h * 3 / 2];
            unsafe {
                let src = slice::from_raw_parts(ptr, pitch * h + pitch * h / 2);
                for row in 0..h {
                    nv12[row * w..(row + 1) * w]
                        .copy_from_slice(&src[row * pitch..row * pitch + w]);
                }
                // UV plane starts after `h` pitched rows; copy `h/2` rows.
                let base = pitch * h;
                let uv = &mut nv12[w * h..];
                for row in 0..h / 2 {
                    uv[row * w..(row + 1) * w]
                        .copy_from_slice(&src[base + row * pitch..base + row * pitch + w]);
                }
            }
            Ok(DecodedNv12 { w, h, nv12 })
        };
        unsafe { buffer2d.Unlock2D() }.map_err(|e| codec_err(format!("Unlock2D {e}")))?;
        result
    }
}

#[cfg(not(target_os = "windows"))]
#[allow(dead_code)]
mod backend {
    use super::DecodedNv12;
    use crate::media::MediaError;

    pub fn probe_hardware() -> Result<(), MediaError> {
        Err(MediaError::HwUnavailable("MF decode is Windows-only".into()))
    }

    pub struct MfDecoder;

    impl MfDecoder {
        pub fn new() -> Result<Self, MediaError> {
            Err(MediaError::HwUnavailable("MF decode is Windows-only".into()))
        }

        pub fn backend_name(&self) -> &'static str {
            "mfhw"
        }

        pub fn decode_annexb(&mut self, _unit: &[u8]) -> Result<Option<DecodedNv12>, MediaError> {
            Err(MediaError::HwUnavailable("MF decode is Windows-only".into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nv12_to_rgba_matches_bt601_reference_points() {
        // Black / white (limited range) round-trip exactly.
        let black = vec![16u8; 2 * 2];
        let mut black_nv12 = black;
        black_nv12.extend([128u8; 2]);
        let rgba = nv12_to_rgba(2, 2, &black_nv12).expect("black");
        assert_eq!(&rgba[0..4], &[0, 0, 0, 255]);
        let white = vec![235u8; 2 * 2];
        let mut white_nv12 = white;
        white_nv12.extend([128u8; 2]);
        let rgba = nv12_to_rgba(2, 2, &white_nv12).expect("white");
        assert_eq!(&rgba[0..4], &[255, 255, 255, 255]);
        // Mid gray stays neutral and near the luma-derived level.
        let gray = vec![128u8; 2 * 2];
        let mut gray_nv12 = gray;
        gray_nv12.extend([128u8; 2]);
        let rgba = nv12_to_rgba(2, 2, &gray_nv12).expect("gray");
        let (r, g, b) = (rgba[0], rgba[1], rgba[2]);
        assert!((r as i16 - 130).abs() <= 2, "r={r}");
        assert!((g as i16 - 130).abs() <= 2, "g={g}");
        assert!((b as i16 - 130).abs() <= 2, "b={b}");
        // Pure red lands on red (tolerance for integer rounding).
        let red_y = vec![81u8; 2 * 2];
        let mut red_nv12 = red_y;
        red_nv12.extend([90u8, 240u8]);
        let rgba = nv12_to_rgba(2, 2, &red_nv12).expect("red");
        assert!(rgba[0] > 200, "r={}", rgba[0]);
        assert!(rgba[1] < 60, "g={}", rgba[1]);
        assert!(rgba[2] < 60, "b={}", rgba[2]);
    }

    #[test]
    fn nv12_to_rgba_rejects_degenerate_and_short_input() {
        assert!(nv12_to_rgba(0, 2, &[]).is_none());
        assert!(nv12_to_rgba(3, 2, &[0u8; 9]).is_none(), "odd width");
        assert!(nv12_to_rgba(2, 2, &[0u8; 5]).is_none(), "short");
        assert!(nv12_to_rgba(2, 2, &[0u8; 6]).is_some());
        assert!(nv12_to_rgba(2, 2, &[0u8; 7]).is_none(), "long");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn enumerate_finds_a_usable_h264_decoder() {
        // Bringup anchor: the box must offer at least the inbox decoder.
        // Says nothing about DXVA yet — that is proven by the roundtrip.
        let list = backend::enumerate_decoders().expect("enumeration works");
        assert!(!list.is_empty());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn hardware_decodes_first_idr_at_native_dims() {
        // Full bringup through the public surface: synthetic IDR in,
        // NV12 picture at stream dims out. Fails closed (early return)
        // where the MFT does not cooperate — the product probe gates on
        // a real picture, so this documents the contract without blacking
        // the suite on uncooperative boxes.
        let profile =
            crate::media::QualityProfile::custom(320, 240, 500, 15).expect("profile");
        let mut enc =
            crate::media::H264Encoder::new_with_profile(&profile, 320, 240).expect("sw encoder");
        let frame = crate::media::I420Frame {
            w: 320,
            h: 240,
            data: vec![128u8; 320 * 240 * 3 / 2],
        };
        let unit = enc.encode(&frame).expect("encode");
        let mut dec = match backend::MfDecoder::new() {
            Ok(dec) => dec,
            Err(_) => return,
        };
        let mut picture = None;
        for _ in 0..4 {
            match dec.decode_annexb(&unit) {
                Ok(Some(pic)) => {
                    picture = Some(pic);
                    break;
                }
                Ok(None) => {}
                Err(_) => return,
            }
        }
        let Some(picture) = picture else { return };
        assert_eq!((picture.w, picture.h), (320, 240));
        assert_eq!(picture.nv12.len(), 320 * 240 * 3 / 2);
    }
}
