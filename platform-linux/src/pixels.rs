//! Packed raw video → tight BGRA. No OS calls.

use golive_platform::{BgraFrame, PixelFormat};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackedKind {
    Bgra,
    Bgrx,
    Rgba,
    Rgbx,
    Rgb,
    Bgr,
}

impl PackedKind {
    fn bpp(self) -> usize {
        match self {
            Self::Rgb | Self::Bgr => 3,
            _ => 4,
        }
    }
}

/// Copy one frame into tight BGRA. `offset`/`chunk_size` bound the visible
/// bytes inside `src` (a mapped PipeWire chunk). Returns `None` when the
/// geometry does not fit.
pub fn bgra_from_packed(
    src: &[u8],
    offset: usize,
    chunk_size: usize,
    stride: i32,
    width: u32,
    height: u32,
    kind: PackedKind,
) -> Option<BgraFrame> {
    if width == 0 || height == 0 {
        return None;
    }
    let bpp = kind.bpp();
    let row_bytes = (width as usize).checked_mul(bpp)?;
    let stride = if stride > 0 {
        stride as usize
    } else {
        row_bytes
    };
    if stride < row_bytes {
        return None;
    }
    let need = stride.checked_mul(height as usize - 1)?.checked_add(row_bytes)?;
    let end = offset.checked_add(chunk_size.max(need))?;
    if end > src.len() || offset + need > src.len() {
        return None;
    }
    let mut data = vec![0u8; (width as usize) * (height as usize) * 4];
    for y in 0..height as usize {
        let row = &src[offset + y * stride..offset + y * stride + row_bytes];
        let dst = &mut data[y * width as usize * 4..(y + 1) * width as usize * 4];
        pack_row(row, dst, kind);
    }
    Some(BgraFrame {
        w: width,
        h: height,
        stride: (width as usize) * 4,
        format: PixelFormat::Bgra8888,
        data,
    })
}

fn pack_row(row: &[u8], dst: &mut [u8], kind: PackedKind) {
    match kind {
        PackedKind::Bgra => dst.copy_from_slice(row),
        PackedKind::Bgrx => {
            for (pix, out) in row.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
                out[0] = pix[0];
                out[1] = pix[1];
                out[2] = pix[2];
                out[3] = 255;
            }
        }
        PackedKind::Rgba => {
            for (pix, out) in row.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
                out[0] = pix[2];
                out[1] = pix[1];
                out[2] = pix[0];
                out[3] = pix[3];
            }
        }
        PackedKind::Rgbx => {
            for (pix, out) in row.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
                out[0] = pix[2];
                out[1] = pix[1];
                out[2] = pix[0];
                out[3] = 255;
            }
        }
        PackedKind::Rgb => {
            for (pix, out) in row.chunks_exact(3).zip(dst.chunks_exact_mut(4)) {
                out[0] = pix[2];
                out[1] = pix[1];
                out[2] = pix[0];
                out[3] = 255;
            }
        }
        PackedKind::Bgr => {
            for (pix, out) in row.chunks_exact(3).zip(dst.chunks_exact_mut(4)) {
                out[0] = pix[0];
                out[1] = pix[1];
                out[2] = pix[2];
                out[3] = 255;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bgrx_becomes_opaque_bgra_and_drops_stride_padding() {
        let src = vec![1, 2, 3, 9, 0, 0, 0, 0, 4, 5, 6, 9];
        let frame = bgra_from_packed(&src, 0, src.len(), 8, 1, 2, PackedKind::Bgrx).unwrap();
        assert_eq!(frame.stride, 4);
        assert_eq!(frame.data, vec![1, 2, 3, 255, 4, 5, 6, 255]);
    }

    #[test]
    fn rgba_swaps_channels() {
        let src = [10, 20, 30, 40];
        let frame = bgra_from_packed(&src, 0, 4, 4, 1, 1, PackedKind::Rgba).unwrap();
        assert_eq!(frame.data, vec![30, 20, 10, 40]);
    }

    #[test]
    fn short_buffer_is_rejected() {
        assert!(bgra_from_packed(&[1, 2, 3], 0, 3, 4, 1, 1, PackedKind::Bgra).is_none());
        assert!(bgra_from_packed(&[0; 8], 0, 8, 4, 0, 1, PackedKind::Bgra).is_none());
    }
}
