//! Pure pixel/cadence helpers. No OS calls — unit-tested on any host.

use golive_platform::{BgraFrame, PixelFormat};

pub fn gate_open(last_ns: u64, now_ns: u64, interval_ns: u64) -> bool {
    now_ns.wrapping_sub(last_ns) >= interval_ns
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
    T0.get_or_init(Instant::now).elapsed().as_nanos().min(u64::MAX as u128) as u64
}

/// Tight BGRA copy honoring `stride` (row padding dropped). `None` on any
/// anomaly — a dropped frame beats a misinterpreted one.
pub fn copy_tight_bgra(src: &[u8], w: u32, h: u32, stride: usize) -> Option<BgraFrame> {
    let wu = w as usize;
    let hu = h as usize;
    if wu == 0 || hu == 0 || wu > 8192 || hu > 8192 || stride < wu.saturating_mul(4) {
        return None;
    }
    let need = stride.checked_mul(hu.saturating_sub(1))?.checked_add(wu * 4)?;
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

pub fn find_source<'a>(
    list: &'a [golive_platform::SourceInfo],
    kind: golive_platform::SourceKind,
    id: &str,
) -> Option<&'a golive_platform::SourceInfo> {
    list.iter().find(|item| item.kind == kind && item.id == id)
}
