//! OpenH264 H.264 encoder wrapper for Windows native capture.
//!
//! The encoder is created lazily from the first `NativeFrame` dimensions and
//! re-initialized automatically by OpenH264 when the resolution changes. BGRA8
//! frames are converted to I420 with the crate's SIMD helpers, encoded to
//! Annex-B, normalized (SPS/PPS cached and injected before IDR), and pushed
//! into the same `AccessUnitQueue` the VideoToolbox path uses. A single encode
//! error is logged and a keyframe requested rather than failing the session.

use super::access_unit::{AccessUnitPushResult, AccessUnitQueue, EncodedAccessUnit};
use super::logger;
use super::pipeline::{EncoderControl, NativeFrame, PipelineState};
use super::types::VideoCodec;
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate as OpenH264FrameRate, FrameType, Profile,
    RateControlMode, UsageType, VuiConfig,
};
use openh264::formats::{BgraSliceU8, YUVBuffer};
use openh264::OpenH264API;
use openh264::Timestamp;
use openh264_sys2::ENCODER_OPTION_BITRATE;
use std::borrow::Cow;
use std::sync::Arc;

const ANNEX_B_START_CODE: &[u8] = &[0, 0, 0, 1];

// Defensive ceiling: capture already downscales to the session size, but a
// full-res leak must never reach OpenH264 (a 7MP software encode per frame
// pegs the CPU and the session falls behind forever).
fn fit_encode_size(width: u32, height: u32) -> (u32, u32) {
    crate::media::types::final_encode_size(width, height, 1920, 1080, false)
}

// Baseline-only decoder-safe ceiling: the pixel-budget fit above keeps
// 21:9 ultrawide WIDER than 1920 (e.g. 5120x1440 -> 2714x764), and that is
// exactly what black-screens Mac viewers while 1080p monitors work on the
// same network with the same SDP: anything wider than 1920 in a Baseline
// session must go. Keeps aspect (2714x764 -> 1920x528), macroblock-aligned.
fn fit_baseline_size(width: u32, height: u32) -> (u32, u32) {
    crate::media::types::final_encode_size(width, height, 1920, 1080, true)
}

// Nearest-neighbor BGRA downscale with fixed-point stepping: no division
// in the hot loop. Mirrors the capture-side resampler: the per-pixel
// division version cost tens of ms per frame and starved the encoder.
fn downscale_bgra_frame(src: &[u8], src_width: u32, src_height: u32, dst_width: u32, dst_height: u32) -> Vec<u8> {
    let src_w = src_width as usize;
    let src_h = src_height as usize;
    let dst_w = dst_width as usize;
    let dst_h = dst_height as usize;
    let mut dst = vec![0_u8; dst_w * dst_h * 4];
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return dst;
    }
    if src.len() < src_w * src_h * 4 {
        return dst;
    }
    let x_step = ((src_w as u64) << 16) / dst_w as u64;
    let y_step = ((src_h as u64) << 16) / dst_h as u64;
    let mut src_y_fp = 0u64;
    for y in 0..dst_h {
        let src_y = (src_y_fp >> 16) as usize;
        src_y_fp += y_step;
        let src_row_start = src_y * src_w * 4;
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
    dst
}

pub(crate) struct OpenH264Encoder {
    encoder: Encoder,
    high_profile: bool,
    converter: AnnexBConverter,
    output: AccessUnitQueue,
    control: Arc<EncoderControl>,
    frames_seen: u64,
    empty_bitstream: u64,
    converter_none: u64,
    pushed_units: u64,
}

impl OpenH264Encoder {
    pub(crate) fn new(
        _width: u32,
        _height: u32,
        bitrate: u32,
        fps: u32,
        video_codec: VideoCodec,
        output: AccessUnitQueue,
        _state: Arc<PipelineState>,
        control: Arc<EncoderControl>,
    ) -> Result<Self, String> {
        // The software encoder must match the session codec: the SDP offer
        // advertises Baseline (42e02a) for H.264 and High (64002a) for
        // H.264 High, and the transport drops samples whose SPS disagrees.
        let want_high = matches!(video_codec, VideoCodec::H264High);
        let make = |high: bool| {
            let config = EncoderConfig::new()
                .bitrate(BitRate::from_bps(bitrate))
                .max_frame_rate(OpenH264FrameRate::from_hz(fps as f32))
                .usage_type(UsageType::ScreenContentRealTime)
                .rate_control_mode(RateControlMode::Bitrate)
                .profile(if high { Profile::High } else { Profile::Baseline })
                .complexity(Complexity::Low)
                .intra_frame_period(openh264::encoder::IntraFramePeriod::from_num_frames(fps * 2))
                .vui(VuiConfig::bt709());
            Encoder::with_api_config(OpenH264API::from_source(), config)
        };
        let (encoder, high_profile) = match make(want_high) {
            Ok(encoder) => (encoder, want_high),
            Err(error) if want_high => {
                eprintln!(
                    "[goDrinking] OpenH264 High profile unavailable ({error}); falling back to Baseline"
                );
                (
                    make(false)
                        .map_err(|fallback| format!("OpenH264 initialization failed: {fallback}"))?,
                    false,
                )
            }
            Err(error) => return Err(format!("OpenH264 initialization failed: {error}")),
        };
        Ok(Self {
            encoder,
            high_profile,
            converter: AnnexBConverter::default(),
            output,
            control,
            frames_seen: 0,
            empty_bitstream: 0,
            converter_none: 0,
            pushed_units: 0,
        })
    }

    pub(crate) fn is_high_profile(&self) -> bool {
        self.high_profile
    }

    /// Encode size for this encoder instance: Baseline output additionally
    /// honors the decoder-safe 1920-wide ceiling (ultrawide 2714x764
    /// black-screens Mac viewers; 1920x528 does not). High keeps the
    /// pixel-budget fit.
    fn fit_for_encoder(&self, width: u32, height: u32) -> (u32, u32) {
        if self.high_profile {
            fit_encode_size(width, height)
        } else {
            fit_baseline_size(width, height)
        }
    }

    pub(crate) fn encode(&mut self, frame: &NativeFrame) -> Result<(), String> {
        if frame.width < 2 || frame.height < 2 {
            return Ok(());
        }
        if frame.storage.len() < (frame.width as usize) * (frame.height as usize) * 4 {
            return Err("OpenH264 frame storage is undersized".into());
        }
        // Apply the defensive ceiling first so an unexpected full-res frame
        // degrades to a downscale instead of a multi-second software encode.
        // Profile-aware: Baseline output stays within the decoder-safe
        // 1920-wide ceiling even when capture hands us a raw frame.
        let (fit_width, fit_height) = self.fit_for_encoder(frame.width, frame.height);
        let fitted: Cow<[u8]> = if (fit_width, fit_height) == (frame.width, frame.height) {
            Cow::Borrowed(frame.storage.as_ref())
        } else {
            Cow::Owned(downscale_bgra_frame(
                frame.storage.as_ref(),
                frame.width,
                frame.height,
                fit_width,
                fit_height,
            ))
        };
        let width = fit_width;
        let height = fit_height;
        // OpenH264 requires even dimensions. WGC frames are normally even; when
        // they are not, crop the last row/column into a temporary buffer.
        let (bgra, enc_width, enc_height): (Cow<[u8]>, u32, u32) =
            if width % 2 == 0 && height % 2 == 0 {
                (fitted, width, height)
            } else {
                let even_width = width & !1;
                let even_height = height & !1;
                let mut cropped = Vec::with_capacity((even_width * even_height * 4) as usize);
                let fitted_bytes: &[u8] = fitted.as_ref();
                for y in 0..even_height {
                    let row_start = (y * width * 4) as usize;
                    cropped.extend_from_slice(
                        &fitted_bytes[row_start..row_start + (even_width * 4) as usize],
                    );
                }
                (Cow::Owned(cropped), even_width, even_height)
            };
        let bgra = BgraSliceU8::new(&bgra, (enc_width as usize, enc_height as usize));
        let yuv = YUVBuffer::from_bgra8_source(bgra);
        let timestamp_ms = frame.timestamp_micros / 1000;
        let bitstream = self
            .encoder
            .encode_at(&yuv, Timestamp::from_millis(timestamp_ms))
            .map_err(|error| format!("OpenH264 encode failed: {error}"))?;
        let keyframe = bitstream.frame_type() == FrameType::IDR;
        let timestamp_90khz = frame.timestamp_micros.saturating_mul(9) / 100;
        let bytes = bitstream.to_vec();
        self.frames_seen += 1;
        if bytes.is_empty() {
            self.empty_bitstream += 1;
        }
        let Some(unit) = self.converter.convert(&bytes, timestamp_90khz, keyframe) else {
            self.converter_none += 1;
            if self.converter_none == 1 {
                logger::log(
                    "WARN",
                    "encoder",
                    &format!(
                        "converter dropped first frame ({} bytes, frame_type={:?}); no NALs parsed",
                        bytes.len(),
                        bitstream.frame_type()
                    ),
                );
            }
            return Ok(());
        };
        let unit_len = unit.data.len();
        let unit_keyframe = unit.keyframe;
        let unit_profile = unit.profile_level_id.clone();
        match self.output.try_push(unit) {
            AccessUnitPushResult::Enqueued => {
                self.pushed_units += 1;
            }
            AccessUnitPushResult::DroppedUntilKeyframe => {
                self.control.request_keyframe();
            }
            AccessUnitPushResult::Closed => {}
        }
        if self.frames_seen == 1 || self.frames_seen % 600 == 0 {
            logger::log(
                "INFO",
                "encoder",
                &format!(
                    "encode stats: frames={} empty_bs={} converter_none={} pushed={} (last {} bytes keyframe={} profile={:?})",
                    self.frames_seen,
                    self.empty_bitstream,
                    self.converter_none,
                    self.pushed_units,
                    unit_len,
                    unit_keyframe,
                    unit_profile
                ),
            );
        }
        Ok(())
    }

    pub(crate) fn force_keyframe(&mut self) {
        self.encoder.force_intra_frame();
    }

    pub(crate) fn set_bitrate(&mut self, bitrate: u32) -> Result<(), String> {
        unsafe {
            self.encoder
                .raw_api()
                .set_option(ENCODER_OPTION_BITRATE, (&bitrate as *const u32).cast_mut().cast());
        }
        Ok(())
    }
}

/// Normalizes OpenH264's Annex-B output: caches SPS/PPS and injects them before
/// every IDR so the access-unit queue always starts a decodable GOP. Mirrors
/// the `AvccAnnexBConverter` behavior used by the VideoToolbox path.
#[derive(Default)]
pub(crate) struct AnnexBConverter {
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

impl AnnexBConverter {
    pub(crate) fn convert(
        &mut self,
        data: &[u8],
        timestamp_90khz: u64,
        keyframe: bool,
    ) -> Option<EncodedAccessUnit> {
        let nals = annex_b_nals(data);
        if nals.is_empty() {
            return None;
        }
        for nal in &nals {
            if nal.is_empty() {
                continue;
            }
            match nal[0] & 0x1f {
                7 => self.sps = Some(nal.to_vec()),
                8 => self.pps = Some(nal.to_vec()),
                _ => {}
            }
        }
        let contains_idr = nals
            .iter()
            .any(|nal| !nal.is_empty() && nal[0] & 0x1f == 5);
        let is_keyframe = keyframe || contains_idr;
        let profile_level_id = self.sps.as_deref().and_then(sps_profile_level_id);
        let mut out = Vec::with_capacity(data.len() + 64);
        if is_keyframe {
            if let Some(sps) = &self.sps {
                out.extend_from_slice(ANNEX_B_START_CODE);
                out.extend_from_slice(sps);
            }
            if let Some(pps) = &self.pps {
                out.extend_from_slice(ANNEX_B_START_CODE);
                out.extend_from_slice(pps);
            }
        }
        for nal in nals {
            if nal.is_empty() {
                continue;
            }
            if is_keyframe && matches!(nal[0] & 0x1f, 7 | 8) {
                continue;
            }
            out.extend_from_slice(ANNEX_B_START_CODE);
            out.extend_from_slice(nal);
        }
        Some(EncodedAccessUnit {
            data: out,
            timestamp_90khz,
            keyframe: is_keyframe,
            profile_level_id,
        })
    }
}

fn sps_profile_level_id(sps: &[u8]) -> Option<String> {
    (sps.len() >= 4).then(|| format!("{:02x}{:02x}{:02x}", sps[1], sps[2], sps[3]))
}

/// Splits an Annex-B byte stream into NAL units, handling both 3- and 4-byte
/// start codes.
fn annex_b_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut nals = Vec::new();
    let len = data.len();
    let mut pos = 0;
    while let Some((start, code_len)) = next_start_code(data, pos) {
        let nal_start = start + code_len;
        let end = next_start_code(data, nal_start)
            .map(|(next_start, _)| next_start)
            .unwrap_or(len);
        if end > nal_start {
            nals.push(&data[nal_start..end]);
        }
        pos = end;
    }
    nals
}

/// Finds the next Annex-B start code at or after `from`, returning its byte
/// offset and length (3 for `00 00 01`, 4 for `00 00 00 01`).
fn next_start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let len = data.len();
    let mut i = from;
    while i + 3 <= len {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if i >= 1 && data[i - 1] == 0 {
                return Some((i - 1, 4));
            }
            return Some((i, 3));
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{annex_b_nals, fit_baseline_size, AnnexBConverter};

    #[test]
    fn baseline_cap_keeps_ultrawide_within_1920() {
        // The incident frame (3620x1018) budget-fits to 2714x764, which
        // black-screens Mac viewers; the Baseline cap lands on 1920x528.
        assert_eq!(fit_baseline_size(3620, 1018), (1920, 528));
        assert_eq!(fit_baseline_size(5120, 1440), (1920, 528));
        // Normal sizes pass through untouched.
        assert_eq!(fit_baseline_size(1920, 1080), (1920, 1080));
        assert_eq!(fit_baseline_size(1280, 720), (1280, 720));
        assert_eq!(fit_baseline_size(640, 360), (640, 360));
        // Second monitor sizes never trigger the cap.
        let (w, h) = fit_baseline_size(2714, 764);
        assert!(w <= 1920 && h % 2 == 0);
    }

    #[test]
    fn splits_annex_b_with_three_and_four_byte_start_codes() {
        let data = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3, 0, 0, 1, 0x65, 4,
        ];
        let nals = annex_b_nals(&data);
        assert_eq!(nals.len(), 3);
        assert_eq!(nals[0], &[0x67, 1, 2]);
        assert_eq!(nals[1], &[0x68, 3]);
        assert_eq!(nals[2], &[0x65, 4]);
    }

    #[test]
    fn injects_cached_sps_pps_before_an_idr() {
        let mut converter = AnnexBConverter::default();
        let sps_pps = [0, 0, 0, 1, 0x67, 0x42, 0xe0, 0x2a, 1, 0, 0, 0, 1, 0x68, 2];
        converter
            .convert(&sps_pps, 0, false)
            .expect("parameter sets should convert");
        let unit = converter
            .convert(&[0, 0, 0, 1, 0x65, 3], 3_000, true)
            .expect("IDR should convert");
        assert!(unit.keyframe);
        assert_eq!(
            unit.data,
            vec![
                0, 0, 0, 1, 0x67, 0x42, 0xe0, 0x2a, 1, 0, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65, 3,
            ]
        );
        assert_eq!(unit.profile_level_id.as_deref(), Some("42e02a"));
    }

    #[test]
    fn non_keyframes_do_not_repeat_parameter_sets() {
        let mut converter = AnnexBConverter::default();
        converter
            .convert(&[0, 0, 0, 1, 0x67, 0x42, 0xe0, 0x2a, 1, 0, 0, 0, 1, 0x68, 2], 0, false)
            .expect("parameter sets");
        let unit = converter
            .convert(&[0, 0, 0, 1, 0x41, 9], 3_000, false)
            .expect("P frame should convert");
        assert!(!unit.keyframe);
        assert_eq!(unit.data, vec![0, 0, 0, 1, 0x41, 9]);
    }
}
