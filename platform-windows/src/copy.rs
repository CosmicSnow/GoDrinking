//! Pure pixel/cadence helpers. No OS calls — unit-tested on any host.

use golive_platform::{BgraFrame, PixelFormat};

pub fn gate_open(last_ns: u64, now_ns: u64, interval_ns: u64) -> bool {
    now_ns.wrapping_sub(last_ns) >= interval_ns
}

/// Gate the expensive GPU readback itself, not the already-copied pixels.
/// Failed readbacks leave the clock unchanged so the next frame can retry.
pub fn readback_if_due<T>(
    last: &mut u64,
    now: u64,
    interval: u64,
    readback: impl FnOnce() -> Option<T>,
) -> Option<T> {
    if !gate_open(*last, now, interval) {
        return None;
    }
    let frame = readback()?;
    *last = golive_platform::cadence::advance_capture_clock(*last, now, interval);
    Some(frame)
}

pub fn initial_last_ns(now: u64, interval_ns: u64) -> u64 {
    now.wrapping_sub(interval_ns)
}

pub fn interval_ns(fps: u32) -> u64 {
    1_000_000_000u64 / fps.max(1) as u64
}

pub fn now_ns() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static T0: OnceLock<Instant> = OnceLock::new();
    T0.get_or_init(Instant::now)
        .elapsed()
        .as_nanos()
        .min(u64::MAX as u128) as u64
}

/// Tight BGRA copy honoring `stride` (row padding dropped). `None` on any
/// anomaly — a dropped frame beats a misinterpreted one.
pub fn copy_tight_bgra(src: &[u8], w: u32, h: u32, stride: usize) -> Option<BgraFrame> {
    let wu = w as usize;
    let hu = h as usize;
    if wu == 0 || hu == 0 || wu > 8192 || hu > 8192 || stride < wu.saturating_mul(4) {
        return None;
    }
    let need = stride
        .checked_mul(hu.saturating_sub(1))?
        .checked_add(wu * 4)?;
    if src.len() < need {
        return None;
    }
    let mut data = vec![0u8; wu * hu * 4];
    for y in 0..hu {
        let s = y * stride;
        let d = y * wu * 4;
        data[d..d + wu * 4].copy_from_slice(&src[s..s + wu * 4]);
    }
    Some(BgraFrame {
        w,
        h,
        stride: wu * 4,
        format: PixelFormat::Bgra8888,
        data,
    })
}

pub const POINTER_MONOCHROME: u32 = 1;
pub const POINTER_COLOR: u32 = 2;
pub const POINTER_MASKED_COLOR: u32 = 4;

pub struct PointerShape {
    pub kind: u32,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub hotspot_x: i32,
    pub hotspot_y: i32,
    pub pixels: Vec<u8>,
}

pub fn composite_pointer(frame: &mut BgraFrame, shape: &PointerShape, hot_x: i32, hot_y: i32) {
    if shape.width == 0 || shape.pitch == 0 || shape.pixels.is_empty() {
        return;
    }
    let left = hot_x - shape.hotspot_x;
    let top = hot_y - shape.hotspot_y;
    match shape.kind {
        POINTER_MONOCHROME => composite_mono(frame, shape, left, top),
        POINTER_COLOR => composite_color(frame, shape, left, top, false),
        POINTER_MASKED_COLOR => composite_color(frame, shape, left, top, true),
        _ => {}
    }
}

fn composite_mono(frame: &mut BgraFrame, shape: &PointerShape, left: i32, top: i32) {
    let visible_h = shape.height / 2;
    if visible_h == 0 {
        return;
    }
    let pitch = shape.pitch as usize;
    let and_bytes = pitch.saturating_mul(visible_h as usize);
    if shape.pixels.len() < and_bytes.saturating_mul(2) {
        return;
    }
    for y in 0..visible_h {
        let and_row = y as usize * pitch;
        let xor_row = and_row + and_bytes;
        for x in 0..shape.width {
            let mask = 0x80u8 >> (x % 8);
            let byte = (x / 8) as usize;
            let and_bit = shape.pixels[and_row + byte] & mask != 0;
            let xor_bit = shape.pixels[xor_row + byte] & mask != 0;
            let Some(dst) = pixel_mut(frame, left + x as i32, top + y as i32) else {
                continue;
            };
            if !and_bit && !xor_bit {
                dst[..4].copy_from_slice(&[0, 0, 0, 255]);
            } else if !and_bit && xor_bit {
                dst[..4].copy_from_slice(&[255, 255, 255, 255]);
            } else if and_bit && xor_bit {
                dst[0] = 255 - dst[0];
                dst[1] = 255 - dst[1];
                dst[2] = 255 - dst[2];
                dst[3] = 255;
            }
        }
    }
}

fn composite_color(frame: &mut BgraFrame, shape: &PointerShape, left: i32, top: i32, masked: bool) {
    let pitch = shape.pitch as usize;
    let row_bytes = (shape.width as usize).saturating_mul(4);
    if pitch < row_bytes {
        return;
    }
    let need = pitch
        .saturating_mul(shape.height.saturating_sub(1) as usize)
        .saturating_add(row_bytes);
    if shape.pixels.len() < need {
        return;
    }
    for y in 0..shape.height {
        let row = y as usize * pitch;
        for x in 0..shape.width {
            let src_i = row + x as usize * 4;
            let src = &shape.pixels[src_i..src_i + 4];
            let Some(dst) = pixel_mut(frame, left + x as i32, top + y as i32) else {
                continue;
            };
            if masked {
                if src[3] == 0 {
                    dst[0] ^= src[0];
                    dst[1] ^= src[1];
                    dst[2] ^= src[2];
                } else {
                    dst[0] = src[0];
                    dst[1] = src[1];
                    dst[2] = src[2];
                    dst[3] = 255;
                }
                continue;
            }
            let alpha = src[3] as u32;
            if alpha == 0 {
                continue;
            }
            if alpha == 255 {
                dst[..4].copy_from_slice(&[src[0], src[1], src[2], 255]);
                continue;
            }
            for channel in 0..3 {
                dst[channel] = ((src[channel] as u32 * alpha + dst[channel] as u32 * (255 - alpha))
                    / 255) as u8;
            }
            dst[3] = 255;
        }
    }
}

fn pixel_mut(frame: &mut BgraFrame, x: i32, y: i32) -> Option<&mut [u8]> {
    if x < 0 || y < 0 {
        return None;
    }
    let x = x as usize;
    let y = y as usize;
    if x >= frame.w as usize || y >= frame.h as usize {
        return None;
    }
    let index = y
        .checked_mul(frame.stride)?
        .checked_add(x.checked_mul(4)?)?;
    frame.data.get_mut(index..index + 4)
}

pub fn find_source<'a>(
    list: &'a [golive_platform::SourceInfo],
    kind: golive_platform::SourceKind,
    id: &str,
) -> Option<&'a golive_platform::SourceInfo> {
    list.iter().find(|item| item.kind == kind && item.id == id)
}

#[cfg(test)]
mod tests {
    use super::{
        composite_pointer, PointerShape, POINTER_COLOR, POINTER_MASKED_COLOR, POINTER_MONOCHROME,
    };
    use golive_platform::{BgraFrame, PixelFormat};

    fn frame(w: u32, h: u32, fill: [u8; 4]) -> BgraFrame {
        BgraFrame {
            w,
            h,
            stride: w as usize * 4,
            format: PixelFormat::Bgra8888,
            data: fill.to_vec().repeat((w * h) as usize),
        }
    }

    #[test]
    fn color_cursor_blends_and_clips() {
        let mut image = frame(2, 2, [0, 0, 0, 255]);
        let shape = PointerShape {
            kind: POINTER_COLOR,
            width: 2,
            height: 1,
            pitch: 8,
            hotspot_x: 1,
            hotspot_y: 0,
            pixels: vec![0, 0, 255, 255, 10, 20, 30, 128],
        };
        composite_pointer(&mut image, &shape, 0, 0);
        assert_eq!(&image.data[0..4], &[5, 10, 15, 255]);
        assert_eq!(&image.data[4..8], &[0, 0, 0, 255]);
        composite_pointer(&mut image, &shape, 1, 5);
        assert_eq!(image.data.len(), 16);
    }

    #[test]
    fn mono_cursor_draws_black_white_and_skips_transparent() {
        let mut image = frame(2, 1, [10, 20, 30, 255]);
        let shape = PointerShape {
            kind: POINTER_MONOCHROME,
            width: 2,
            height: 2,
            pitch: 1,
            hotspot_x: 0,
            hotspot_y: 0,
            pixels: vec![0b0100_0000, 0b1000_0000],
        };
        composite_pointer(&mut image, &shape, 0, 0);
        assert_eq!(&image.data[0..4], &[255, 255, 255, 255]);
        assert_eq!(&image.data[4..8], &[10, 20, 30, 255]);
    }

    #[test]
    fn masked_color_xors_when_alpha_is_clear() {
        let mut image = frame(1, 1, [0b1111_0000, 0, 0, 255]);
        let shape = PointerShape {
            kind: POINTER_MASKED_COLOR,
            width: 1,
            height: 1,
            pitch: 4,
            hotspot_x: 0,
            hotspot_y: 0,
            pixels: vec![0b0000_1111, 0, 0, 0],
        };
        composite_pointer(&mut image, &shape, 0, 0);
        assert_eq!(image.data[0], 0b1111_1111);
    }
}
