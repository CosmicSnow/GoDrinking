//! BGRA → I420 conversion (pure, heavily tested).
//!
//! Colorspace chain (documented assumptions):
//! - The macOS backend requests `sRGB` from ScreenCaptureKit, so incoming
//!   bytes are sRGB-encoded (gamma ~2.2, sRGB primaries). Where the system
//!   cannot honor the request (e.g. a Display-P3 panel), values stay
//!   display-encoded and color will be approximately right — exact P3 gamut
//!   mapping needs GPU and is out of scope.
//! - Luma/chroma use the BT.709 matrix in LIMITED range (16–235 / 16–240),
//!   the broadcast standard the H.264 pipeline expects. This matches the
//!   existing synthetic/movie path, which also feeds limited-range YUV.
//!
//! Integer math (matches `libyuv`/`ffmpeg` BT.709 limited constants):
//! ```text
//! Y = 16  + ( 47*R + 157*G + 16*B + 128) >> 8
//! U = 128 + (-26*R -  87*G +113*B + 128) >> 8   (from 2x2 average)
//! V = 128 + (112*R - 102*G - 10*B + 128) >> 8   (from 2x2 average)
//! ```
//! (47/157/16 = 0.1826/0.6142/0.0620 × 256; U/V are the standard
//! -0.1016/-0.3390/0.4392 and 0.4392/-0.3986/-0.0407 × 256, rounded.)

use crate::error::PlatformError;
use crate::types::{BgraFrame, PixelFormat, PlanarYuv};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConvertError {
    WrongFormat,
    ZeroSize,
    StrideTooSmall,
    BufferTooShort,
}

impl std::fmt::Display for ConvertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongFormat => write!(f, "só BGRA8888 é suportado"),
            Self::ZeroSize => write!(f, "dimensões zeradas"),
            Self::StrideTooSmall => write!(f, "stride menor que a linha"),
            Self::BufferTooShort => write!(f, "buffer menor que o stride exige"),
        }
    }
}

impl std::error::Error for ConvertError {}

/// Converts one BGRA frame to planar I420 (4:2:0, limited-range BT.709).
/// Dimensions must be even (chroma subsampling); stride-aware.
pub fn bgra_to_i420(frame: &BgraFrame) -> Result<PlanarYuv, ConvertError> {
    if frame.format != PixelFormat::Bgra8888 {
        return Err(ConvertError::WrongFormat);
    }
    let w = frame.w as usize;
    let h = frame.h as usize;
    if w == 0 || h == 0 || w % 2 != 0 || h % 2 != 0 {
        return Err(ConvertError::ZeroSize);
    }
    if frame.stride < w * 4 {
        return Err(ConvertError::StrideTooSmall);
    }
    if frame.data.len() < frame.stride * (h - 1) + w * 4 {
        return Err(ConvertError::BufferTooShort);
    }
    let mut y = vec![0u8; w * h];
    let mut u = vec![0u8; w * h / 4];
    let mut v = vec![0u8; w * h / 4];
    let row = |yy: usize, xx: usize| -> (i32, i32, i32) {
        let base = yy * frame.stride + xx * 4;
        (
            frame.data[base + 2] as i32, // R
            frame.data[base + 1] as i32, // G
            frame.data[base] as i32,     // B
        )
    };
    for yy in 0..h {
        for xx in 0..w {
            let (r, g, b) = row(yy, xx);
            y[yy * w + xx] = clamp8(16 + ((47 * r + 157 * g + 16 * b + 128) >> 8));
        }
    }
    for yy in (0..h).step_by(2) {
        for xx in (0..w).step_by(2) {
            let mut sr = 0i32;
            let mut sg = 0i32;
            let mut sb = 0i32;
            for (dy, dx) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                let (r, g, b) = row(yy + dy, xx + dx);
                sr += r;
                sg += g;
                sb += b;
            }
            // Average over 4, then matrix (>>10 keeps the /4 exact).
            let i = (yy / 2) * (w / 2) + (xx / 2);
            u[i] = clamp8(128 + ((-26 * sr - 87 * sg + 113 * sb + 512) >> 10));
            v[i] = clamp8(128 + ((112 * sr - 102 * sg - 10 * sb + 512) >> 10));
        }
    }
    Ok(PlanarYuv { w: frame.w, h: frame.h, y, u, v })
}

fn clamp8(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

/// Builds the user-facing error for a failed conversion (no pixel data).
pub fn convert_error(e: ConvertError) -> PlatformError {
    PlatformError::Internal(format!("conversão de pixel: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, r: u8, g: u8, b: u8) -> BgraFrame {
        let mut data = vec![0u8; (w * h * 4) as usize];
        for px in data.chunks_exact_mut(4) {
            px[0] = b;
            px[1] = g;
            px[2] = r;
            px[3] = 255;
        }
        BgraFrame { w, h, stride: (w * 4) as usize, format: PixelFormat::Bgra8888, data }
    }

    fn mean(bytes: &[u8]) -> f64 {
        bytes.iter().map(|b| *b as u64).sum::<u64>() as f64 / bytes.len() as f64
    }

    #[test]
    fn black_and_white_hit_limited_range() {
        let black = bgra_to_i420(&solid(4, 4, 0, 0, 0)).unwrap();
        assert_eq!(mean(&black.y).round(), 16.0);
        assert_eq!(mean(&black.u).round(), 128.0);
        assert_eq!(mean(&black.v).round(), 128.0);
        let white = bgra_to_i420(&solid(4, 4, 255, 255, 255)).unwrap();
        assert_eq!(mean(&white.y).round(), 235.0);
        assert!((mean(&white.u) - 128.0).abs() <= 1.0);
        assert!((mean(&white.v) - 128.0).abs() <= 1.0);
    }

    #[test]
    fn primaries_land_near_reference() {
        // BT.709 limited references (±3 tolerates integer rounding).
        let red = bgra_to_i420(&solid(4, 4, 255, 0, 0)).unwrap();
        assert!((mean(&red.y) - 63.0).abs() <= 3.0, "Y={}", mean(&red.y));
        assert!((mean(&red.u) - 102.0).abs() <= 3.0, "U={}", mean(&red.u));
        assert!((mean(&red.v) - 240.0).abs() <= 3.0, "V={}", mean(&red.v));
        let green = bgra_to_i420(&solid(4, 4, 0, 255, 0)).unwrap();
        assert!((mean(&green.y) - 173.0).abs() <= 3.0, "Y={}", mean(&green.y));
        let blue = bgra_to_i420(&solid(4, 4, 0, 0, 255)).unwrap();
        assert!((mean(&blue.y) - 32.0).abs() <= 3.0, "Y={}", mean(&blue.y));
        let gray = bgra_to_i420(&solid(4, 4, 128, 128, 128)).unwrap();
        assert!((mean(&gray.y) - 126.0).abs() <= 3.0, "Y={}", mean(&gray.y));
        assert!((mean(&gray.u) - 128.0).abs() <= 1.0);
        assert!((mean(&gray.v) - 128.0).abs() <= 1.0);
    }

    #[test]
    fn stride_padding_is_skipped_not_smeared() {
        // 2px wide rows with 8 bytes of padding after each row.
        let w = 2u32;
        let h = 2u32;
        let stride = 16usize;
        let mut data = vec![0xAAu8; stride * h as usize];
        for yy in 0..h as usize {
            for xx in 0..w as usize {
                let base = yy * stride + xx * 4;
                data[base] = 0;
                data[base + 1] = 0;
                data[base + 2] = 255; // red pixels
                data[base + 3] = 255;
            }
        }
        let frame = BgraFrame { w, h, stride, format: PixelFormat::Bgra8888, data };
        let yuv = bgra_to_i420(&frame).unwrap();
        // All luma is red (~63); padding 0xAA must not leak in.
        assert!(yuv.y.iter().all(|&v| (v as i32 - 63).abs() <= 3));
    }

    #[test]
    fn malformed_inputs_rejected() {
        let base = solid(4, 4, 0, 0, 0);
        assert_eq!(
            bgra_to_i420(&BgraFrame { w: 0, h: 4, ..base.clone() }),
            Err(ConvertError::ZeroSize)
        );
        assert_eq!(
            bgra_to_i420(&BgraFrame { w: 3, h: 4, ..base.clone() }),
            Err(ConvertError::ZeroSize)
        );
        assert_eq!(
            bgra_to_i420(&BgraFrame { stride: 8, ..base.clone() }),
            Err(ConvertError::StrideTooSmall)
        );
        assert_eq!(
            bgra_to_i420(&BgraFrame { data: vec![0u8; 8], ..base.clone() }),
            Err(ConvertError::BufferTooShort)
        );
    }
}
