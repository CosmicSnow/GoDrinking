//! Media timestamp conversion utilities.

/// Converts a rational media timestamp to the RTP clock used by video samples.
pub(crate) fn to_90khz(value: i64, timescale: i32) -> Option<u64> {
    if value < 0 || timescale <= 0 {
        return None;
    }
    let numerator = (value as u128).checked_mul(90_000)?;
    u64::try_from(numerator / timescale as u128).ok()
}

#[cfg(test)]
mod tests {
    use super::to_90khz;

    #[test]
    fn converts_common_video_timescales() {
        assert_eq!(to_90khz(1, 30), Some(3_000));
        assert_eq!(to_90khz(1_000_000, 1_000_000), Some(90_000));
        assert_eq!(to_90khz(90_000, 90_000), Some(90_000));
    }

    #[test]
    fn one_sixtieth_of_a_second_is_1500_ticks() {
        // 60fps frame duration in the 90kHz RTP clock: 90_000 / 60.
        assert_eq!(to_90khz(1, 60), Some(1_500));
        assert_eq!(to_90khz(1, 30), Some(3_000));
        // Monotonic source ticks stay monotonic after conversion.
        let a = to_90khz(100, 60).expect("a converts");
        let b = to_90khz(101, 60).expect("b converts");
        assert!(b > a);
        assert_eq!(b - a, 1_500);
    }

    #[test]
    fn rejects_invalid_or_overflowing_timestamps() {
        assert_eq!(to_90khz(-1, 90_000), None);
        assert_eq!(to_90khz(1, 0), None);
        assert_eq!(to_90khz(i64::MAX, 1), None);
    }
}
