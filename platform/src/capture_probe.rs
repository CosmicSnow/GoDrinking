//! Numeric acquisition counters. No OS types, pixels, identifiers or logging.
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

#[derive(Default)]
pub struct CaptureProbe {
    pub received: AtomicU64,
    pub gate_dropped: AtomicU64,
    pub queue_dropped: AtomicU64,
    pub invalid: AtomicU64,
    pub idle: AtomicU64,
    pub blank: AtomicU64,
    last_ns: AtomicU64,
    max_gap_ns: AtomicU64,
}

#[derive(Default)]
pub struct CaptureCounts {
    pub received: u64,
    pub gate_dropped: u64,
    pub queue_dropped: u64,
    pub invalid: u64,
    pub idle: u64,
    pub blank: u64,
    pub max_gap_us: u64,
}

impl CaptureProbe {
    pub fn arrival(&self, now_ns: u64) {
        self.received.fetch_add(1, Relaxed);
        let previous = self.last_ns.swap(now_ns, Relaxed);
        if previous != 0 { self.max_gap_ns.fetch_max(now_ns.saturating_sub(previous), Relaxed); }
    }

    // Single observer. Counts may straddle one observation at the callback
    // boundary, but are never duplicated or lost over successive snapshots.
    pub fn take(&self) -> CaptureCounts {
        CaptureCounts {
            received: self.received.swap(0, Relaxed),
            gate_dropped: self.gate_dropped.swap(0, Relaxed),
            queue_dropped: self.queue_dropped.swap(0, Relaxed),
            invalid: self.invalid.swap(0, Relaxed),
            idle: self.idle.swap(0, Relaxed),
            blank: self.blank.swap(0, Relaxed),
            max_gap_us: self.max_gap_ns.swap(0, Relaxed) / 1000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn snapshots_preserve_drop_reasons_and_gaps_across_windows() {
        let probe = CaptureProbe::default();
        probe.arrival(1_000_000);
        probe.arrival(21_000_000);
        probe.gate_dropped.fetch_add(1, Relaxed);
        probe.idle.fetch_add(3, Relaxed);
        probe.blank.fetch_add(1, Relaxed);
        let first = probe.take();
        assert_eq!((first.received, first.gate_dropped, first.max_gap_us), (2, 1, 20_000));
        assert_eq!((first.idle, first.blank), (3, 1));
        let empty = probe.take();
        assert_eq!((empty.received, empty.idle, empty.blank), (0, 0, 0));
        probe.arrival(61_000_000);
        probe.queue_dropped.fetch_add(1, Relaxed);
        let next = probe.take();
        assert_eq!((next.received, next.gate_dropped, next.queue_dropped, next.max_gap_us), (1, 0, 1, 40_000));
    }
}
