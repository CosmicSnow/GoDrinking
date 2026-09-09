//! Capture cadence independent of callback arrival jitter. No OS clock calls.

/// Advance after accepting a frame. Preserve the scheduled cadence instead
/// of restarting the interval at a late callback. Carry at most half an
/// interval of lateness: after a stall the next frame must still wait at
/// least half an interval, rather than replaying the missed ticks in a burst.
/// Caller checks that `now.wrapping_sub(last) >= interval` before accepting.
pub fn advance_capture_clock(last: u64, now: u64, interval: u64) -> u64 {
    let late = now.wrapping_sub(last).saturating_sub(interval);
    now.wrapping_sub(late.min(interval / 2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_preserves_cadence_at_capture_rates() {
        for fps in [15, 30, 60] {
            let interval = 1_000_000_000 / fps;
            let mut last = 0u64.wrapping_sub(interval);
            let mut accepted = 0;
            for n in 0..300 {
                let now = n * interval + (n % 2) * 1_000_000;
                if now.wrapping_sub(last) >= interval {
                    last = advance_capture_clock(last, now, interval);
                    accepted += 1;
                }
            }
            assert_eq!(accepted, 300, "{fps}fps with 1ms jitter");
        }
    }

    #[test]
    fn stall_cannot_accumulate_a_catchup_burst() {
        let interval = 100;
        let now = 10_000;
        let last = advance_capture_clock(0, now, interval);
        assert!(now - last < interval);
        assert!(now + interval / 2 - 1 - last < interval);
        assert_eq!(now + interval / 2 - last, interval);
    }

    #[test]
    fn fast_source_stays_within_rate_budget() {
        let interval = 1_000_000_000 / 30;
        let mut last = 0u64.wrapping_sub(interval);
        let mut accepted = 0;
        for now in (0..10_000_000_000u64).step_by(1_000_000) {
            if now.wrapping_sub(last) >= interval {
                last = advance_capture_clock(last, now, interval);
                accepted += 1;
            }
        }
        assert_eq!(accepted, 300);
    }
}
