//! VideoToolbox decoding. Native objects never leave this module.
fn nal_units(bytes: &[u8]) -> Result<Vec<&[u8]>, String> {
    if bytes.len() > 16 * 1024 * 1024 {
        return Err("access unit too large".into());
    }
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let size = if bytes[i..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if bytes[i..].starts_with(&[0, 0, 1]) {
            3
        } else {
            i += 1;
            continue;
        };
        starts.push((i, i + size));
        i += size;
    }
    if starts.is_empty() || bytes[..starts[0].0].iter().any(|b| *b != 0) {
        return Err("invalid Annex B prefix".into());
    }
    let mut result = Vec::with_capacity(starts.len());
    for (n, &(_, begin)) in starts.iter().enumerate() {
        let mut end = starts.get(n + 1).map_or(bytes.len(), |s| s.0);
        while end > begin && bytes[end - 1] == 0 {
            end -= 1;
        }
        if end == begin || bytes[begin] & 0x80 != 0 {
            return Err("invalid NAL unit".into());
        }
        result.push(&bytes[begin..end]);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn annex_b_preserves_payload_and_rejects_empty_units() {
        let bytes = [
            0, 0, 0, 1, 9, 16, 0, 0, 1, 0x67, 0x42, 0, 0, 3, 1, 0x80, 0, 0, 0, 1, 0x68, 0x80,
        ];
        assert_eq!(
            nal_units(&bytes).unwrap(),
            vec![&bytes[4..6], &bytes[9..16], &bytes[20..22]]
        );
        for bad in [&[][..], &[1, 2, 3], &[0, 0, 1], &[0, 0, 1, 0x67, 0, 0, 1]] {
            assert!(nal_units(bad).is_err());
        }
    }
}

use golive_platform::decode::{Nv12Picture, VideoDecoder};
use objc2_core_foundation::{
    kCFBooleanTrue, kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFDictionary,
    CFNumber, CFRetained,
};
use objc2_core_media::*;
use objc2_core_video::*;
use objc2_video_toolbox::*;
use std::{
    ffi::c_void,
    ptr::{null, null_mut, NonNull},
    sync::Mutex,
};

pub fn new_decoder() -> Result<Box<dyn VideoDecoder>, String> {
    Ok(Box::new(VtDecoder {
        session: None,
        format: None,
        sps: Vec::new(),
        pps: Vec::new(),
        recovery_sets: Vec::new(),
    }))
}

struct VtDecoder {
    session: Option<CFRetained<VTDecompressionSession>>,
    format: Option<CFRetained<CMFormatDescription>>,
    sps: Vec<u8>,
    pps: Vec<u8>,
    recovery_sets: Vec<u8>,
}

impl VtDecoder {
    fn close(&mut self) {
        if let Some(session) = self.session.take() {
            // Decode always uses synchronous flags. No callbacks survive a call.
            unsafe {
                session.invalidate();
            }
        }
        self.format = None;
    }

    fn open(&mut self) -> Result<(), String> {
        self.close();
        unsafe {
            let mut pointers = [
                NonNull::new(self.sps.as_mut_ptr()).unwrap(),
                NonNull::new(self.pps.as_mut_ptr()).unwrap(),
            ];
            let mut sizes = [self.sps.len(), self.pps.len()];
            let mut raw_format = null();
            check(
                CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    None,
                    2,
                    NonNull::new(pointers.as_mut_ptr()).unwrap(),
                    NonNull::new(sizes.as_mut_ptr()).unwrap(),
                    4,
                    NonNull::from(&mut raw_format),
                ),
                "format",
            )?;
            let format = CFRetained::from_raw(
                NonNull::new(raw_format as *mut CMFormatDescription).ok_or("missing format")?,
            );
            let dims = CMVideoFormatDescriptionGetDimensions(&format);
            if dims.width <= 0
                || dims.height <= 0
                || dims.width > 8192
                || dims.height > 8192
                || dims.width % 2 != 0
                || dims.height % 2 != 0
            {
                return Err("unsupported decode dimensions".into());
            }
            // Only expose parameter sets after CoreMedia validated them. They
            // remain useful even if the hardware-only session cannot be created.
            self.recovery_sets.clear();
            for nal in [&self.sps, &self.pps] {
                self.recovery_sets.extend_from_slice(&[0, 0, 0, 1]);
                self.recovery_sets.extend_from_slice(nal);
            }
            let hardware = kCFBooleanTrue.ok_or("missing CFBoolean")?;
            let spec = dictionary(
                kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder as *const _
                    as *const c_void,
                hardware as *const _ as *const c_void,
            )?;
            let pixel_format =
                CFNumber::new_i32(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange as i32);
            let attrs = dictionary(
                kCVPixelBufferPixelFormatTypeKey as *const _ as *const c_void,
                &*pixel_format as *const _ as *const c_void,
            )?;
            let callback = VTDecompressionOutputCallbackRecord {
                decompressionOutputCallback: Some(output),
                decompressionOutputRefCon: null_mut(),
            };
            let mut raw_session = null_mut();
            check(
                VTDecompressionSession::create(
                    None,
                    &format,
                    Some(&spec),
                    Some(&attrs),
                    &callback,
                    NonNull::from(&mut raw_session),
                ),
                "session",
            )?;
            self.session = Some(CFRetained::from_raw(
                NonNull::new(raw_session).ok_or("missing session")?,
            ));
            self.format = Some(format);
            eprintln!("golive: decode backend=videotoolbox");
            Ok(())
        }
    }
}

impl Drop for VtDecoder {
    fn drop(&mut self) {
        self.close();
    }
}

impl VideoDecoder for VtDecoder {
    fn parameter_sets(&self) -> Vec<u8> {
        self.recovery_sets.clone()
    }

    fn decode(&mut self, annex_b: &[u8]) -> Result<Option<Nv12Picture>, String> {
        let units = nal_units(annex_b)?;
        let mut changed = false;
        let mut idr = false;
        let mut vcl = false;
        let mut avcc = Vec::with_capacity(annex_b.len() + units.len());
        for nal in units {
            match nal[0] & 31 {
                7 => {
                    if self.sps != nal {
                        self.sps = nal.to_vec();
                        changed = true;
                    }
                }
                8 => {
                    if self.pps != nal {
                        self.pps = nal.to_vec();
                        changed = true;
                    }
                }
                5 => {
                    idr = true;
                    vcl = true;
                }
                1..=4 => vcl = true,
                _ => (),
            }
            avcc.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            avcc.extend_from_slice(nal);
        }
        if changed {
            self.close();
        }
        if !vcl {
            return Ok(None);
        }
        if self.session.is_none() {
            if !idr || self.sps.is_empty() || self.pps.is_empty() {
                return Ok(None);
            }
            self.open()?;
        }
        unsafe {
            // CM owns the compressed copy, including while its sample is alive.
            let mut raw_block = null_mut();
            check(
                CMBlockBuffer::create_with_memory_block(
                    None,
                    null_mut(),
                    avcc.len(),
                    None,
                    null(),
                    0,
                    avcc.len(),
                    0,
                    NonNull::from(&mut raw_block),
                ),
                "block",
            )?;
            let block = CFRetained::from_raw(NonNull::new(raw_block).ok_or("missing block")?);
            check(
                CMBlockBuffer::replace_data_bytes(
                    NonNull::new(avcc.as_mut_ptr().cast()).unwrap(),
                    &block,
                    0,
                    avcc.len(),
                ),
                "copy",
            )?;
            let mut raw_sample = null_mut();
            let size = avcc.len();
            check(
                CMSampleBuffer::create_ready(
                    None,
                    Some(&block),
                    self.format.as_deref(),
                    1,
                    0,
                    null(),
                    1,
                    &size,
                    NonNull::from(&mut raw_sample),
                ),
                "sample",
            )?;
            let sample = CFRetained::from_raw(NonNull::new(raw_sample).ok_or("missing sample")?);
            let sink: Mutex<Option<Result<Nv12Picture, String>>> = Mutex::new(None);
            // Both flags are zero: Apple's contract guarantees the callback has
            // finished before decode_frame returns. The stack sink is alive then.
            check(
                self.session.as_ref().unwrap().decode_frame(
                    &sample,
                    VTDecodeFrameFlags(0),
                    &sink as *const _ as *mut c_void,
                    null_mut(),
                ),
                "decode",
            )?;
            sink.into_inner()
                .map_err(|_| "decode callback lock")?
                .transpose()
        }
    }
}

fn check(status: i32, stage: &str) -> Result<(), String> {
    if status == 0 {
        Ok(())
    } else {
        Err(format!("VT {stage} status {status}"))
    }
}

unsafe fn dictionary(
    key: *const c_void,
    value: *const c_void,
) -> Result<CFRetained<CFDictionary>, String> {
    let mut keys = [key];
    let mut values = [value];
    CFDictionary::new(
        None,
        keys.as_mut_ptr(),
        values.as_mut_ptr(),
        1,
        &kCFTypeDictionaryKeyCallBacks,
        &kCFTypeDictionaryValueCallBacks,
    )
    .ok_or("dictionary allocation".into())
}

unsafe extern "C-unwind" fn output(
    _: *mut c_void,
    frame: *mut c_void,
    status: i32,
    _: VTDecodeInfoFlags,
    image: *mut CVImageBuffer,
    _: CMTime,
    _: CMTime,
) {
    if frame.is_null() {
        return;
    }
    let sink = &*(frame as *const Mutex<Option<Result<Nv12Picture, String>>>);
    let result = if status != 0 {
        Err(format!("VT callback status {status}"))
    } else if image.is_null() {
        Err("VT callback missing image".into())
    } else {
        copy_nv12(&*(image as *const CVPixelBuffer))
    };
    if let Ok(mut slot) = sink.lock() {
        *slot = Some(result);
    }
}

unsafe fn copy_nv12(pixel: &CVPixelBuffer) -> Result<Nv12Picture, String> {
    if CVPixelBufferGetPixelFormatType(pixel) != kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
        || CVPixelBufferGetPlaneCount(pixel) != 2
    {
        return Err("unsupported VT pixel layout".into());
    }
    let (width, height) = (CVPixelBufferGetWidth(pixel), CVPixelBufferGetHeight(pixel));
    if width == 0
        || height == 0
        || width > 8192
        || height > 8192
        || width % 2 != 0
        || height % 2 != 0
    {
        return Err("invalid VT pixel dimensions".into());
    }
    check(
        CVPixelBufferLockBaseAddress(pixel, CVPixelBufferLockFlags::ReadOnly),
        "pixel lock",
    )?;
    struct Unlock<'a>(&'a CVPixelBuffer);
    impl Drop for Unlock<'_> {
        fn drop(&mut self) {
            unsafe {
                CVPixelBufferUnlockBaseAddress(self.0, CVPixelBufferLockFlags::ReadOnly);
            }
        }
    }
    let _unlock = Unlock(pixel);
    let mut data = Vec::with_capacity(width * height * 3 / 2);
    for (plane, rows) in [(0, height), (1, height / 2)] {
        let stride = CVPixelBufferGetBytesPerRowOfPlane(pixel, plane);
        let base = CVPixelBufferGetBaseAddressOfPlane(pixel, plane) as *const u8;
        if base.is_null() || stride < width || CVPixelBufferGetHeightOfPlane(pixel, plane) < rows {
            return Err("invalid VT pixel stride".into());
        }
        for row in 0..rows {
            data.extend_from_slice(std::slice::from_raw_parts(base.add(row * stride), width));
        }
    }
    Ok(Nv12Picture {
        width,
        height,
        data,
    })
}

#[cfg(test)]
mod native_tests {
    use super::*;
    use openh264::{
        encoder::Encoder,
        formats::{YUVBuffer, YUVSource},
    };

    #[test]
    fn hardware_decodes_deltas_and_dimension_changes() {
        let mut native = new_decoder().unwrap();
        let mut software = openh264::decoder::Decoder::new().unwrap();
        for (w, h) in [(320, 180), (480, 270), (1920, 1080)] {
            let mut encoder = Encoder::new().unwrap();
            for n in 0..4 {
                let mut rgb = vec![0u8; w * h * 3];
                for (i, pixel) in rgb.chunks_exact_mut(3).enumerate() {
                    pixel.copy_from_slice(&[
                        ((i % w + n * 7) % 240) as u8,
                        ((i / w + n * 3) % 240) as u8,
                        90,
                    ]);
                }
                let yuv =
                    YUVBuffer::from_rgb_source(openh264::formats::RgbSliceU8::new(&rgb, (w, h)));
                let unit = encoder.encode(&yuv).unwrap().to_vec();
                let reference = software.decode(&unit).unwrap().unwrap();
                // Exercise parameter sets arriving separately from the IDR.
                let picture = if n == 0 {
                    let nals = nal_units(&unit).unwrap();
                    let mut payload = Vec::new();
                    for nal in nals {
                        let mut part = vec![0, 0, 0, 1];
                        part.extend_from_slice(nal);
                        if matches!(nal[0] & 31, 7 | 8) {
                            assert!(native.decode(&part).unwrap().is_none());
                        } else {
                            payload.extend_from_slice(&part);
                        }
                    }
                    native.decode(&payload)
                } else {
                    native.decode(&unit)
                }
                .expect("real VT hardware decode")
                .expect("synchronous output");
                assert_eq!(
                    (picture.width, picture.height, picture.data.len()),
                    (w, h, w * h * 3 / 2)
                );
                let stride = reference.strides().0;
                let error: usize = (0..h)
                    .flat_map(|row| (0..w).map(move |col| (row, col)))
                    .map(|(row, col)| {
                        picture.data[row * w + col].abs_diff(reference.y()[row * stride + col])
                            as usize
                    })
                    .sum();
                assert!(
                    error as f64 / ((w * h) as f64) < 2.0,
                    "luma must agree with software: {error}"
                );
                let (_, us, vs) = reference.strides();
                let chroma_error: usize = (0..h / 2)
                    .flat_map(|row| (0..w / 2).map(move |col| (row, col)))
                    .map(|(row, col)| {
                        let uv = w * h + row * w + col * 2;
                        picture.data[uv].abs_diff(reference.u()[row * us + col]) as usize
                            + picture.data[uv + 1].abs_diff(reference.v()[row * vs + col]) as usize
                    })
                    .sum();
                assert!(
                    chroma_error as f64 / ((w * h / 2) as f64) < 2.0,
                    "NV12 chroma must agree: {chroma_error}"
                );
            }
        }
    }
}

/// Diagnostic clock for the calling worker only; excludes GPU/other threads.
pub fn thread_cpu_us() -> Option<u64> {
    unsafe {
        let mut value: libc::timespec = std::mem::zeroed();
        if libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut value) != 0 {
            return None;
        }
        Some((value.tv_sec as u64).checked_mul(1_000_000)? + value.tv_nsec as u64 / 1000)
    }
}

#[cfg(test)]
mod cpu_clock_tests {
    #[test]
    fn thread_cpu_clock_excludes_sleep() {
        let before = super::thread_cpu_us().expect("thread CPU clock available");
        std::thread::sleep(std::time::Duration::from_millis(40));
        let after = super::thread_cpu_us().unwrap();
        assert!(after >= before);
        assert!(
            after - before < 20_000,
            "sleep must not look like busy codec CPU"
        );
    }
}
