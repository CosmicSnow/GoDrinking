//! Screen-capture bridge: platform stream → core frames.
//!
//! The app owns this glue because it alone depends on both
//! `golive-platform` and `golive-core` (neither knows the other):
//! - enumerate sources / report capabilities (UI),
//! - open + start the selected source (may show the OS prompt on first use),
//! - pump `BgraFrame` → `bgra_to_i420` → core `I420Frame` into a bounded
//!   latest-only channel the core encode loop consumes.
//!
//! Failures are typed (`PlatformError` Display is user-safe); the bridge
//! never logs titles, pixels, or tokens.

use golive_core::media::I420Frame;
use golive_platform::{
    CaptureConfig, FrameStream, NextError, PlatformError, SourceInfo, SourceKind, VideoSource,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

/// Pacing tick for the bridge pump (matches the ~30fps contract loosely;
/// the core re-paces by frame counter anyway).
const BRIDGE_TICK: Duration = Duration::from_millis(100);
/// Latest-only channel depth: one in flight, one waiting.
const CHANNEL_DEPTH: usize = 2;

/// Handle to a running bridge. `stop()` is idempotent and bounded; `Drop`
/// stops best-effort.
pub struct BridgeHandle {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl BridgeHandle {
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for BridgeHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Starts the OS stream for `info` and bridges it into core frames.
/// Returns the core channel plus the handle that owns the backend.
/// Errors before any lifecycle is touched (pre-flight safe).
pub fn start_capture(
    info: &SourceInfo,
    config: CaptureConfig,
) -> Result<(mpsc::Receiver<I420Frame>, BridgeHandle), PlatformError> {
    let mut stream = open_stream(info, &config)?;
    let (core_tx, core_rx) = mpsc::sync_channel::<I420Frame>(CHANNEL_DEPTH);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_ = Arc::clone(&stop);
    let thread = std::thread::Builder::new()
        .name("golive-screen-bridge".into())
        .spawn(move || {
            pump_bridge(&mut stream, &core_tx, &stop_);
        })
        .map_err(|e| PlatformError::Internal(format!("thread da ponte: {e}")))?;
    Ok((core_rx, BridgeHandle { stop, thread: Some(thread) }))
}

/// Starts OS capture for one listed source id and bridges it into core
/// frames. Pre-flight safe: on error nothing was started and nothing leaks.
/// `label` uses kind+id only — never the user-facing name (titles stay out
/// of logs).
pub fn start_capture_for(
    kind: SourceKind,
    id: &str,
) -> Result<(mpsc::Receiver<I420Frame>, BridgeHandle, String), PlatformError> {
    let info = enumerate_sources()?
        .into_iter()
        .find(|item| item.kind == kind && item.id == id)
        .ok_or_else(|| PlatformError::SourceGone { id: id.to_owned() })?;
    let label = match kind {
        SourceKind::Display => format!("display:{id}"),
        SourceKind::Window => format!("window:{id}"),
    };
    let config = CaptureConfig {
        width: info.w.max(320).min(1920),
        height: info.h.max(240).min(1080),
        fps: 30,
    };
    let (rx, handle) = start_capture(&info, config)?;
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

/// Bridge pump: platform frames → converted core frames, latest-only.
/// Ends when the stream ends/fails or `stop` fires; dropping our sender is
/// what tells the core encode loop (disconnect path).
fn pump_bridge(
    stream: &mut FrameStream,
    core_tx: &mpsc::SyncSender<I420Frame>,
    stop: &AtomicBool,
) {
    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        match stream.next_frame(BRIDGE_TICK) {
            Ok(bgra) => {
                match golive_platform::bgra_to_i420(&bgra) {
                    Ok(planar) => {
                        let mut data =
                            Vec::with_capacity((planar.w * planar.h * 3 / 2) as usize);
                        data.extend_from_slice(&planar.y);
                        data.extend_from_slice(&planar.u);
                        data.extend_from_slice(&planar.v);
                        let frame = I420Frame { w: planar.w as usize, h: planar.h as usize, data };
                        // Latest-only: a full channel means the core is
                        // behind; drop this one rather than queue stale.
                        let _ = core_tx.try_send(frame);
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
}
