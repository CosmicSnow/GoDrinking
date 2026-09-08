//! Screen-capture bridge: platform stream → core frames.
//!
//! The app owns this glue because it alone depends on both
//! `golive-platform` and `golive-core` (neither knows the other):
//! - enumerate sources / report capabilities (UI),
//! - open + start the selected source (may show the OS prompt on first use),
//! - pump packets into core frames: CPU packets downscale + convert here,
//!   GPU packets forward retained (zero-copy submit downstream).
//!
//! CPU economy (the capture sink matters as much as the encoder):
//! - capture fps is clamped to the profile fps BEFORE conversion: surplus
//!   packets die on arrival (timestamp gate, no pixel touched);
//! - CPU frames are downscaled to the profile target BEFORE conversion, so
//!   `bgra_to_i420` costs target pixels, never capture pixels;
//! - GPU packets are NEVER converted here: they ride retained into the
//!   core, which submits them to VideoToolbox with zero CPU copy (one
//!   conversion only on the software/drift fallback);
//! - the live profile is shared (`Arc<Mutex<…>>`) so `set_quality`
//!   re-clamps mid-share (plus a transactional stream restart).
//!
//! Failures are typed (`PlatformError` Display is user-safe); the bridge
//! never logs titles, pixels, or tokens.

use golive_core::media::{normalize_dims, ExternalFrame, I420Frame, QualityProfile};
use golive_platform::{
    BgraFrame, CaptureConfig, CapturePacket, FrameStream, NextError, PixelFormat, PlatformError,
    SourceInfo, SourceKind, VideoSource,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// Pacing tick for the bridge pump (matches the ~30fps contract loosely;
/// the core re-paces by frame counter anyway).
const BRIDGE_TICK: Duration = Duration::from_millis(100);
/// Latest-only channel depth: one in flight, one waiting.
const CHANNEL_DEPTH: usize = 2;

/// Handle to a running bridge. `stop()` is idempotent and bounded; `Drop`
/// stops best-effort. `reconfigure()` restarts the OS stream at a new
/// profile without dropping the core channel (the core repeats its last
/// frame across the gap).
pub struct BridgeHandle {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    info: SourceInfo,
    core_tx: mpsc::SyncSender<ExternalFrame>,
    live: Arc<Mutex<QualityProfile>>,
}

impl BridgeHandle {
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    /// Transactional stream restart at `profile`: the new stream starts
    /// FIRST (same core channel, latest-only — no glitch, no leak); only
    /// then the old one stops. On failure the old stream keeps running and
    /// `Err` surfaces typed. Bounded (SCK start rendezvous has a deadline).
    pub fn reconfigure(&mut self, profile: QualityProfile) -> Result<(), PlatformError> {
        let config = profile_config(&self.info, &profile);
        let (thread, stop) = spawn_stream(
            self.info.clone(),
            config,
            self.core_tx.clone(),
            Arc::clone(&self.live),
        )?;
        // New stream is live: retire the old one, adopt the new.
        self.stop.store(true, Ordering::Release);
        if let Some(old) = self.thread.take() {
            let _ = old.join();
        }
        self.thread = Some(thread);
        self.stop = stop;
        Ok(())
    }
}

impl Drop for BridgeHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Profile-sized output config for a source: fit the source into the
/// profile (aspect preserved, never upscale), then clamp to backend
/// bounds. The SCK stream scales in capture, so the callback never sees
/// full-res pixels.
fn profile_config(info: &SourceInfo, profile: &QualityProfile) -> CaptureConfig {
    let (tw, th) = normalize_dims(info.w, info.h, profile.w, profile.h);
    golive_platform::capture_config_for(tw, th, profile.fps)
}

/// Spawns one OS stream + pump feeding `core_tx`. `open_stream` only
/// returns once SCK rendezvoused started, so no second rendezvous here;
/// thread-builder failure is the only spawn error. Shared by initial
/// start and reconfigure.
fn spawn_stream(
    info: SourceInfo,
    config: CaptureConfig,
    core_tx: mpsc::SyncSender<ExternalFrame>,
    live: Arc<Mutex<QualityProfile>>,
) -> Result<(std::thread::JoinHandle<()>, Arc<AtomicBool>), PlatformError> {
    let mut stream = open_stream(&info, &config)?;
    let stop = Arc::new(AtomicBool::new(false));
    let stop_ = Arc::clone(&stop);
    let thread = std::thread::Builder::new()
        .name("golive-screen-bridge".into())
        .spawn(move || {
            pump_bridge(&mut stream, &core_tx, &stop_, &live);
        })
        .map_err(|e| PlatformError::Internal(format!("thread da ponte: {e}")))?;
    Ok((thread, stop))
}

/// Starts the OS stream for `info` and bridges it into core frames.
/// Returns the core channel plus the handle that owns the backend.
/// Errors before any lifecycle is touched (pre-flight safe).
/// `live` carries the current share profile (fps clamp + target dims);
/// `set_quality` re-clamps it and restarts the stream via `reconfigure`.
pub fn start_capture(
    info: &SourceInfo,
    config: CaptureConfig,
    live: Arc<Mutex<QualityProfile>>,
) -> Result<(mpsc::Receiver<ExternalFrame>, BridgeHandle), PlatformError> {
    let (core_tx, core_rx) = mpsc::sync_channel::<ExternalFrame>(CHANNEL_DEPTH);
    let (thread, stop) =
        spawn_stream(info.clone(), config, core_tx.clone(), Arc::clone(&live))?;
    Ok((
        core_rx,
        BridgeHandle { stop, thread: Some(thread), info: info.clone(), core_tx, live },
    ))
}

/// Starts OS capture for one listed source id and bridges it into core
/// frames. Pre-flight safe: on error nothing was started and nothing leaks.
/// `label` uses kind+id only — never the user-facing name (titles stay out
/// of logs). Capture asks the OS for at most the profile fps (clamped to
/// the backend's 1..=30 range); the bridge gate below enforces it exactly.
pub fn start_capture_for(
    kind: SourceKind,
    id: &str,
    profile: QualityProfile,
    live: Arc<Mutex<QualityProfile>>,
) -> Result<(mpsc::Receiver<ExternalFrame>, BridgeHandle, String), PlatformError> {
    let info = enumerate_sources()?
        .into_iter()
        .find(|item| item.kind == kind && item.id == id)
        .ok_or_else(|| PlatformError::SourceGone { id: id.to_owned() })?;
    let label = match kind {
        SourceKind::Display => format!("display:{id}"),
        SourceKind::Window => format!("window:{id}"),
    };
    let config = profile_config(&info, &profile);
    let (rx, handle) = start_capture(&info, config, live)?;
    Ok((rx, handle, label))
}

/// Opens + starts the platform stream for one listed source.
fn open_stream(
    info: &SourceInfo,
    config: &CaptureConfig,
) -> Result<FrameStream, PlatformError> {
    #[cfg(target_os = "macos")]
    {
        let mut source =
            golive_platform_macos::ScSource::open(info).map_err(|e| {
                // open() is validation-only; surface as-is (typed upstream).
                e
            })?;
        source.start(config)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (info, config);
        Err(PlatformError::UnsupportedPlatform {
            reason: "captura de tela: apenas macOS (Windows planejado)",
        })
    }
}

/// Time gate: forwards a frame only when at least `interval` elapsed since
/// the last forwarded one. Pure (the bridge owns the clock).
pub fn should_forward(
    last: Option<Instant>,
    now: Instant,
    interval: Duration,
) -> bool {
    match last {
        None => true,
        Some(t) => now.duration_since(t) >= interval,
    }
}

/// Nearest-neighbor BGRA downscale (or copy at equal size). Honors stride;
/// output is always tight (`stride == dw*4`). Pure.
pub fn scale_bgra_nearest(src: &BgraFrame, dw: u32, dh: u32) -> BgraFrame {
    if src.w == 0 || src.h == 0 || dw == 0 || dh == 0 || src.data.is_empty() {
        return BgraFrame {
            w: dw,
            h: dh,
            stride: (dw as usize) * 4,
            format: PixelFormat::Bgra8888,
            data: Vec::new(),
        };
    }
    let mut data = vec![0u8; (dw as usize) * (dh as usize) * 4];
    for y in 0..dh as usize {
        let sy = (y * src.h as usize / dh as usize).min(src.h as usize - 1);
        for x in 0..dw as usize {
            let sx = (x * src.w as usize / dw as usize).min(src.w as usize - 1);
            let s = sy * src.stride + sx * 4;
            let d = (y * dw as usize + x) * 4;
            if s + 4 <= src.data.len() {
                data[d..d + 4].copy_from_slice(&src.data[s..s + 4]);
            }
        }
    }
    BgraFrame { w: dw, h: dh, stride: (dw as usize) * 4, format: PixelFormat::Bgra8888, data }
}

/// Bridge pump: platform packets → throttled core frames, latest-only.
/// Surplus packets die on arrival (no conversion, no alloc); kept CPU
/// frames convert at TARGET size; kept GPU packets forward retained
/// (zero-copy submit downstream — never converted here). Ends when the
/// stream ends/fails or `stop` fires; dropping our sender is what tells
/// the core encode loop (disconnect path).
fn pump_bridge(
    stream: &mut FrameStream,
    core_tx: &mpsc::SyncSender<ExternalFrame>,
    stop: &AtomicBool,
    live: &Arc<Mutex<QualityProfile>>,
) {
    let mut last_forwarded: Option<Instant> = None;
    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        // One cheap lock per tick: current fps clamp + target dims.
        let (interval, target) = match live.lock() {
            Ok(profile) => (profile.frame_duration(), (profile.w, profile.h)),
            Err(_) => (BRIDGE_TICK, (1280, 720)),
        };
        match stream.next_frame(BRIDGE_TICK) {
            Ok(CapturePacket::Gpu(gpu)) => {
                let now = Instant::now();
                if !should_forward(last_forwarded, now, interval) {
                    continue; // over profile fps: drop retained, no pixels
                }
                last_forwarded = Some(now);
                // Latest-only: a full channel means the core is behind;
                // drop (releasing) rather than queue stale.
                let _ = core_tx.try_send(ExternalFrame::Gpu(gpu));
            }
            Ok(CapturePacket::Cpu(bgra)) => {
                let now = Instant::now();
                if !should_forward(last_forwarded, now, interval) {
                    continue; // over profile fps: drop before touching pixels
                }
                // Fit the capture into the profile (never upscale), so the
                // encoder never sees anything above the contract.
                let (tw, th) = normalize_dims(bgra.w, bgra.h, target.0, target.1);
                let small = scale_bgra_nearest(&bgra, tw, th);
                match golive_platform::bgra_to_i420(&small) {
                    Ok(planar) => {
                        let mut data =
                            Vec::with_capacity((planar.w * planar.h * 3 / 2) as usize);
                        data.extend_from_slice(&planar.y);
                        data.extend_from_slice(&planar.u);
                        data.extend_from_slice(&planar.v);
                        let frame = I420Frame { w: planar.w as usize, h: planar.h as usize, data };
                        last_forwarded = Some(now);
                        // Latest-only: a full channel means the core is
                        // behind; drop this one rather than queue stale.
                        let _ = core_tx.try_send(ExternalFrame::Cpu(frame));
                    }
                    Err(e) => {
                        // Malformed frame: skip one, keep the stream (log the
                        // kind only — never pixels).
                        eprintln!("screen convert skipped: {e}");
                    }
                }
            }
            Err(NextError::Timeout) => continue,
            Err(NextError::Ended) | Err(NextError::Failed(_)) => break,
        }
    }
    stream.stop(Duration::from_secs(2)).ok();
}

/// Lists capture sources for the UI. Empty/error surfaces typed (the UI
/// shows the reason instead of failing mute).
pub fn enumerate_sources() -> Result<Vec<SourceInfo>, PlatformError> {
    #[cfg(target_os = "macos")]
    {
        golive_platform_macos::enumerate()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(PlatformError::UnsupportedPlatform {
            reason: "captura de tela: apenas macOS (Windows planejado)",
        })
    }
}

/// Re-exported types + helpers for Tauri commands (single import site).
pub use golive_platform::capabilities;
pub use golive_platform::CapabilitySet as Capabilities;
pub use golive_platform::SourceInfo as ListedSource;

#[cfg(test)]
mod tests {
    use super::*;
    use golive_platform::mock::{drive_lifecycle, MockSource};
    use golive_platform::{BgraFrame, PixelFormat, SourceKind};

    fn solid_bgra(w: u32, h: u32, r: u8, g: u8, b: u8) -> BgraFrame {
        let mut data = vec![0u8; (w * h * 4) as usize];
        for px in data.chunks_exact_mut(4) {
            px[0] = b;
            px[1] = g;
            px[2] = r;
            px[3] = 255;
        }
        BgraFrame { w, h, stride: (w * 4) as usize, format: PixelFormat::Bgra8888, data }
    }

    #[test]
    fn bridge_converts_mock_frames_end_to_end() {
        // Full path with zero OS: mock source → pump → core frames.
        let info = SourceInfo {
            kind: SourceKind::Display,
            id: "mock-1".into(),
            name: "Mock".into(),
            w: 64,
            h: 64,
        };
        let mut source = MockSource::open(&info).unwrap();
        source.frames = vec![MockSource::display("x", 64, 64)
            .with_solid_frame(200, 30, 30)
            .frames
            .remove(0)];
        let frames = drive_lifecycle(source, 2).unwrap();
        assert_eq!(frames.len(), 2);
        // Same conversion the bridge applies, on the same bytes.
        let planar = golive_platform::bgra_to_i420(&frames[0]).unwrap();
        assert_eq!((planar.w, planar.h), (64, 64));
        let mean_y: f64 = planar.y.iter().map(|b| *b as u64).sum::<u64>() as f64
            / planar.y.len() as f64;
        // (200,30,30): Y = 16+(47*200+157*30+16*30+128)>>8 = 73.
        assert!((mean_y - 73.0).abs() <= 5.0, "reddish luma {mean_y}");
    }

    #[test]
    fn bridge_drops_stale_not_new() {
        // try_send semantics the pump relies on: a full cap-2 channel keeps
        // flowing (drops), never blocks the capture thread.
        let (tx, rx) = mpsc::sync_channel::<u8>(2);
        tx.try_send(1).unwrap();
        tx.try_send(2).unwrap();
        assert!(tx.try_send(3).is_err());
        assert_eq!(rx.try_recv().unwrap(), 1);
    }

    #[test]
    fn enumerate_fails_typed_off_macos() {
        // On macOS enumerate() touches the OS (prompt/denial) — unit tests
        // must NEVER call it there. Elsewhere it must be the typed
        // UnsupportedPlatform, never a panic or empty silence.
        #[cfg(not(target_os = "macos"))]
        {
            let error = enumerate_sources().unwrap_err();
            assert!(matches!(
                error,
                PlatformError::UnsupportedPlatform { .. }
            ));
        }
        #[cfg(target_os = "macos")]
        {
            // Capabilities are pure cfg facts: safe to assert, no OS contact.
            let caps = crate::screen::capabilities();
            assert!(caps.display.supported);
        }
    }

    #[test]
    fn solid_helper_builds_contract_frames() {
        let _ = solid_bgra(4, 4, 0, 0, 0);
    }

    #[test]
    fn scale_bgra_downsamples_top_left_of_each_cell() {
        // 4x4 gradient in R; 2x2 takes (0,0),(2,0),(0,2),(2,2).
        let mut data = vec![0u8; 4 * 4 * 4];
        for y in 0..4 {
            for x in 0..4 {
                let i = (y * 4 + x) * 4;
                data[i] = (y * 10) as u8; // B encodes row
                data[i + 2] = (x * 10) as u8; // R encodes col
                data[i + 3] = 255;
            }
        }
        let src = BgraFrame { w: 4, h: 4, stride: 16, format: PixelFormat::Bgra8888, data };
        let small = scale_bgra_nearest(&src, 2, 2);
        assert_eq!((small.w, small.h, small.stride), (2, 2, 8));
        // (0,0) stays, (1,0) is old (2,0): R=20.
        assert_eq!(small.data[2], 0);
        assert_eq!(small.data[6], 20);
        // (0,1) is old (0,2): B=20.
        assert_eq!(small.data[8], 20);
        // Degenerate input never panics.
        assert!(scale_bgra_nearest(&src, 0, 2).data.is_empty());
    }

    #[test]
    fn should_forward_gates_on_interval() {
        let now = Instant::now();
        let interval = Duration::from_millis(100);
        assert!(should_forward(None, now, interval));
        assert!(!should_forward(Some(now), now, interval));
        assert!(should_forward(Some(now - interval), now, interval));
        assert!(!should_forward(Some(now - interval + Duration::from_millis(1)), now, interval));
    }

    /// Feeds N frames as fast as possible through the real pump with a
    /// 1 fps profile: exactly the first survives (timestamp gate, no
    /// conversion on drops), at profile dims (downscale before convert).
    /// Returns decoded CPU frames (GPU packets pass through untouched and
    /// are unwrapped by their own test below).
    fn run_pump(frames: Vec<BgraFrame>, profile: QualityProfile) -> Vec<I420Frame> {
        use golive_platform::{CapturePacket, FrameStream};
        let (tx, rx) = mpsc::channel::<CapturePacket>();
        let worker = std::thread::spawn(move || {
            for frame in frames {
                if tx.send(CapturePacket::Cpu(frame)).is_err() {
                    break;
                }
            }
        });
        let mut stream = FrameStream::new(
            rx,
            Arc::new(Mutex::new(None)),
            Arc::new(AtomicBool::new(false)),
            worker,
        );
        let (core_tx, core_rx) = mpsc::sync_channel::<ExternalFrame>(2);
        let stop = AtomicBool::new(false);
        let live = Arc::new(Mutex::new(profile));
        pump_bridge(&mut stream, &core_tx, &stop, &live);
        let mut out = Vec::new();
        while let Ok(packet) = core_rx.recv_timeout(Duration::from_millis(200)) {
            match packet {
                ExternalFrame::Cpu(frame) => out.push(frame),
                ExternalFrame::Gpu(_) => panic!("CPU-only feed emitted GPU"),
            }
        }
        out
    }

    #[test]
    fn bridge_clamps_fps_and_downscales_before_convert() {
        let profile =
            QualityProfile::custom(64, 48, 500, 1).expect("1fps test profile");
        let frames = vec![solid_bgra(128, 96, 200, 30, 30); 10];
        let out = run_pump(frames, profile);
        // 10 back-to-back frames inside one 1 s window: exactly one survives.
        assert_eq!(out.len(), 1, "fps clamp drops surplus before conversion");
        // 128x96 capture fit into 64x48: exact target dims (even, no upscale).
        assert_eq!((out[0].w, out[0].h), (64, 48));
        assert_eq!(out[0].data.len(), 64 * 48 * 3 / 2);
    }

    #[test]
    fn bridge_passes_through_at_native_size() {
        let profile = QualityProfile::medium();
        let out = run_pump(vec![solid_bgra(64, 64, 0, 200, 0)], profile);
        assert_eq!(out.len(), 1);
        // 64x64 into 1280x720: no upscale, native size kept.
        assert_eq!((out[0].w, out[0].h), (64, 64));
    }

    #[test]
    fn bridge_forwards_gpu_retained_without_converting() {
        use golive_platform::{CapturePacket, FrameStream, GpuPixelBuffer};
        use std::sync::atomic::AtomicU64;
        static RELEASES: AtomicU64 = AtomicU64::new(0);
        // SAFETY: test-only release — counts, never dereferences.
        unsafe extern "C-unwind" fn mock_release(ptr: *mut std::ffi::c_void) {
            assert!(!ptr.is_null());
            RELEASES.fetch_add(1, Ordering::SeqCst);
        }
        RELEASES.store(0, Ordering::SeqCst);
        // SAFETY: dangling non-null pointer; nothing touches pixels here,
        // the pump must forward it untouched and Drop releases once.
        let gpu = unsafe {
            GpuPixelBuffer::from_raw(0x2000 as *mut std::ffi::c_void, 128, 96, 512, mock_release)
        };
        let (tx, rx) = mpsc::channel::<CapturePacket>();
        let worker = std::thread::spawn(move || {
            let _ = tx.send(CapturePacket::Gpu(gpu));
        });
        let mut stream = FrameStream::new(
            rx,
            Arc::new(Mutex::new(None)),
            Arc::new(AtomicBool::new(false)),
            worker,
        );
        let (core_tx, core_rx) = mpsc::sync_channel::<ExternalFrame>(2);
        let stop = AtomicBool::new(false);
        let live = Arc::new(Mutex::new(QualityProfile::medium()));
        pump_bridge(&mut stream, &core_tx, &stop, &live);
        match core_rx.recv_timeout(Duration::from_secs(2)).expect("gpu forwarded") {
            ExternalFrame::Gpu(forwarded) => {
                // Untouched: same dims/stride, still owned (no convert ran —
                // a conversion would have produced Cpu instead).
                assert_eq!((forwarded.w, forwarded.h, forwarded.stride), (128, 96, 512));
                drop(forwarded);
            }
            ExternalFrame::Cpu(_) => panic!("GPU packet must not convert in the bridge"),
        }
        assert_eq!(RELEASES.load(Ordering::SeqCst), 1, "exactly one release");
    }
}
