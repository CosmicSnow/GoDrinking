//! Isolated production encoder probe, no WebRTC or WebView. Run from app/:
//! cargo run --release --example encode_probe -- 30
//! Optional GOLIVE_TRACE_DIR enables the same stage traces as desktop.
use golive_core::media::{EngineKind, I420Frame, QualityProfile, VideoEncoder};
use std::time::{Duration, Instant};

fn main() {
    let seconds: u64 = std::env::args().nth(1).unwrap_or_else(|| "30".into()).parse().expect("seconds");
    assert!((1..=300).contains(&seconds));
    #[cfg(target_os = "macos")]
    golive_core::media::install_media_cpu_clock(golive_platform_macos::decode::thread_cpu_us);
    let profile = QualityProfile { w: 1920, h: 1080, fps: 60, bitrate_kbps: 6000 };
    let mut encoder = VideoEncoder::new(&profile, 1920, 1080, EngineKind::Hardware).expect("hardware required");
    let mut frame = I420Frame { w: 1920, h: 1080, data: vec![128; 1920 * 1080 * 3 / 2] };
    let period = Duration::from_nanos(1_000_000_000 / 60);
    let started = Instant::now();
    let mut next = started;
    let mut frames = 0u64;
    let mut max_us = 0u64;
    let mut slow = 0u64;
    while started.elapsed() < Duration::from_secs(seconds) {
        // Moving bars; source generation intentionally outside encode cost.
        for (row, bytes) in frame.data[..1920*1080].chunks_mut(1920).enumerate() {
            for (x, value) in bytes.iter_mut().enumerate() {
                *value = 16 + (((x + row + frames as usize * 7) / 16) % 220) as u8;
            }
        }
        let before = Instant::now();
        let output = encoder.encode_frame(&frame).expect("encode");
        let us = before.elapsed().as_micros() as u64;
        max_us = max_us.max(us);
        slow += (us > 50_000) as u64;
        frames += output.is_some() as u64;
        next += period;
        if next < Instant::now() { next = Instant::now() + period; }
        std::thread::sleep(next.saturating_duration_since(Instant::now()));
    }
    println!("{}", serde_json::json!({"frames":frames,"seconds":started.elapsed().as_secs_f64(),
        "max_encode_us":max_us,"encode_over_50ms":slow}));
}
