//! Explicit, non-windowed acquisition fixture; never runs in the default suite.
use golive_platform::{CaptureConfig, VideoSource, NextError};
use golive_platform_macos::ScSource;
use std::time::{Duration, Instant};

#[test]
#[ignore = "requires explicit GOLIVE_CAPTURE_DISPLAY and screen recording permission"]
fn live_display_acquisition() {
    let id = std::env::var("GOLIVE_CAPTURE_DISPLAY").expect("explicit display id required");
    let source = ScSource::enumerate().unwrap().into_iter().find(|s|
        s.kind == golive_platform::SourceKind::Display && s.id == id).expect("display not found");
    let mut source = ScSource::open(&source).unwrap();
    let mut stream = source.start(&CaptureConfig { width: 1920, height: 1080, fps: 60 }).unwrap();
    let begin = Instant::now();
    let mut forwarded = 0u64;
    while begin.elapsed() < Duration::from_secs(30) {
        match stream.next_frame(Duration::from_millis(100)) {
            Ok(_) => forwarded += 1,
            Err(NextError::Timeout) => (),
            Err(error) => panic!("capture failed: {error:?}"),
        }
    }
    let elapsed_us = begin.elapsed().as_micros();
    let counts = stream.take_capture_counts().expect("SCK probe");
    stream.stop(Duration::from_secs(2)).unwrap();
    println!("CAPTURE_PROBE {{\"elapsed_us\":{elapsed_us},\"received\":{},\"forwarded\":{forwarded},\"gate_dropped\":{},\"queue_dropped\":{},\"invalid\":{},\"max_gap_us\":{}}}",
        counts.received, counts.gate_dropped, counts.queue_dropped, counts.invalid, counts.max_gap_us);
    assert!(forwarded > 0, "capture produced no frames");
}
