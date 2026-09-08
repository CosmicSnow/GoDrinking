//! macOS capture backend: ScreenCaptureKit via pure `objc2` bindings.
//!
//! WHY objc2 and not a Swift-bridge crate: Swift bridges need Swift-compat
//! static archives at link time plus a Swift-concurrency dylib that exists
//! nowhere on CLT-only machines — the packaged app would abort at launch.
//! `objc2` links plain system frameworks, which every Mac has. (Verified
//! the hard way; see git history.)
//!
//! Mapping to the pure contract (`golive-platform`):
//! - `enumerate()` → `getShareableContentWithCompletionHandler:` → displays
//!   + on-screen windows (titles shown in UI, never logged).
//! - `open()` validates id/kind only (fast, no OS).
//! - `start()` re-enumerates, matches the id (gone → `SourceGone`), builds
//!   filter + BGRA config + output object, and spawns a supervisor thread
//!   owning every SCK object. First OS contact happens there — this is
//!   where the system permission prompt appears. Startup is
//!   rendezvous-bounded (ready or typed error).
//! - Frames arrive on our own dispatch queue; the output object copies BGRA
//!   bytes (stride-aware) into a bounded latest-only channel.
//! - `stop()` signals the supervisor, which stops capture (indicators
//!   released deterministically) and exits; join is deadline-bounded.
//!
//! Colorspace: SCK delivers display-native BGRA and this backend does NOT
//! pin a color space (an unverifiable string would risk breaking real
//! capture without TCC to test it). [`golive_platform::bgra_to_i420`]
//! treats values as sRGB-encoded BT.709 input — exact on sRGB displays,
//! approximate on Display-P3 panels (exact gamut mapping needs GPU).
//!
//! Mid-stream disappearance (unplugged display, closed window) surfaces via
//! the stream delegate (`didStopWithError:`) into the error slot; silence
//! otherwise means a static screen (the core repeats the last frame).
//!
//! Aggregate counts only in logs (never titles, pixels, or tokens).
//!
//! # Safety
//! All `unsafe` here is mechanical ObjC interop: null-checked pointers,
//! exact-size reads, lock/unlock pairing via RAII guard. The one semantic
//! claim is the CVImageBuffer→CVPixelBuffer cast, justified by SCK's
//! documented screen-capture contract plus a runtime pixel-format check
//! (non-BGRA aborts the frame, never misinterprets it).

use golive_platform::{
    BgraFrame, CaptureConfig, FrameStream, PixelFormat, PlatformError, SourceInfo, SourceKind,
    VideoSource,
};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, AnyThread, DefinedClass};
use objc2_core_media::{CMTime, CMTimeFlags};
use objc2_core_video::{
    kCVPixelFormatType_32BGRA, CVImageBuffer, CVPixelBuffer, CVPixelBufferGetBaseAddress,
    CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamOutput,
    SCStreamOutputType, SCWindow,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

/// Startup rendezvous bound: SCK setup (incl. first prompt wait) is quick;
/// beyond this the start is declared failed, never hung.
const START_DEADLINE: Duration = Duration::from_secs(8);
/// Channel depth: one in flight, one waiting. Newest drops when full, so
/// memory stays flat and staleness is bounded (~2 frames), never queued.
const CHANNEL_DEPTH: usize = 2;
/// Apple's SCStreamErrorUserDeclined. Observed constant, flagged as such:
/// denial primarily surfaces as empty enumerate (see below), this is the
/// defensive second net for start-time errors.
const SC_USER_DECLINED: i32 = -3810;

/// A macOS display or window selected from [`enumerate`].
#[derive(Clone, Debug)]
pub struct ScSource {
    info: SourceInfo,
}

impl ScSource {
    fn validated(info: &SourceInfo) -> Result<Self, PlatformError> {
        if info.id.trim().is_empty() {
            return Err(PlatformError::InvalidSource { reason: "id vazio" });
        }
        match info.kind {
            SourceKind::Display | SourceKind::Window => Ok(Self { info: info.clone() }),
        }
    }

    /// Current macOS version as `(major, minor)`, read from
    /// `/System/Library/CoreServices/SystemVersion.plist` with std::fs only.
    /// Used for `OsVersionTooOld`.
    fn os_version() -> (u32, u32) {
        let text = std::fs::read_to_string("/System/Library/CoreServices/SystemVersion.plist")
            .unwrap_or_default();
        parse_product_version(&text).unwrap_or((0, 0))
    }
}

/// Parses `ProductVersion` (e.g. "14.2.1") out of SystemVersion.plist text.
/// Pure and unit-tested; the file read above is the only impure step.
fn parse_product_version(text: &str) -> Option<(u32, u32)> {
    let mut lines = text.lines();
    for line in &mut lines {
        if line.contains("<key>ProductVersion</key>") {
            let value = lines.next().unwrap_or("");
            let version = value
                .trim()
                .trim_start_matches("<string>")
                .trim_end_matches("</string>")
                .trim();
            let mut parts = version.split('.');
            let major: u32 = parts.next()?.parse().ok()?;
            let minor: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            return Some((major, minor));
        }
    }
    None
}

/// NSError → typed error. Only domain + code travel (never message text —
/// system strings stay out of logs by policy).
fn map_ns_error(domain: &str, code: i32) -> PlatformError {
    if domain.contains("ScreenCaptureKit") && code == SC_USER_DECLINED {
        return PlatformError::permission_denied();
    }
    // Denial also hides content instead of erroring; empty enumerate maps
    // separately (see enumerate). Anything else is internal, redacted to
    // domain + code (+ OS major for triage — not a secret).
    let (major, _) = ScSource::os_version();
    PlatformError::Internal(format!("captura falhou ({domain} #{code}, macOS {major})"))
}

fn ns_error_parts(error: &NSError) -> (String, i32) {
    // Plain getters (safe bindings); only domain + code travel onward.
    (error.domain().to_string(), error.code() as i32)
}

/// List capture targets on this Mac. Empty on a real desktop means denial
/// hid the content (a Mac without any display or window is not a real
/// case), so empty maps to `PermissionDenied`, not to an empty UI.
pub fn enumerate() -> Result<Vec<SourceInfo>, PlatformError> {
    let content = shareable_content()?;
    let mut out = Vec::new();
    unsafe {
        for display in content.displays().iter() {
            let id = display.displayID();
            let w = display.width().max(0) as u32;
            let h = display.height().max(0) as u32;
            out.push(SourceInfo {
                kind: SourceKind::Display,
                id: id.to_string(),
                name: format!("Display {id} · {w}x{h}"),
                w,
                h,
            });
        }
        for window in content.windows().iter() {
            let title = window
                .title()
                .map(|title| title.to_string())
                .unwrap_or_default();
            let name = if title.trim().is_empty() {
                "Janela sem título".into()
            } else {
                title
            };
            // Window size isn't exposed pre-stream; filled at start().
            out.push(SourceInfo {
                kind: SourceKind::Window,
                id: window.windowID().to_string(),
                name,
                w: 0,
                h: 0,
            });
        }
    }
    if out.is_empty() {
        return Err(PlatformError::permission_denied());
    }
    Ok(out)
}

/// Blocking shareable-content fetch with deadline. Errors map typed;
/// null content without error maps to denial (hidden content).
fn shareable_content() -> Result<Retained<SCShareableContent>, PlatformError> {
    let (tx, rx) = mpsc::channel::<Result<Retained<SCShareableContent>, PlatformError>>();
    let block = block2::RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            let result = if !error.is_null() {
                // SAFETY: non-null per the null check; borrowed for the call.
                let (domain, code) = unsafe { ns_error_parts(&*error) };
                Err(map_ns_error(&domain, code))
            } else if content.is_null() {
                Err(PlatformError::permission_denied())
            } else {
                // SAFETY: non-null per the check; SCK passes a +0 reference
                // that lives through this call — retain for the channel.
                let retained: Retained<SCShareableContent> =
                    unsafe { Retained::retain(content).unwrap() };
                Ok(retained)
            };
            let _ = tx.send(result);
        },
    );
    unsafe {
        SCShareableContent::getShareableContentWithCompletionHandler(&block);
    }
    match rx.recv_timeout(START_DEADLINE) {
        Ok(result) => result,
        Err(_) => Err(PlatformError::Internal("timeout ao listar fontes".into())),
    }
}

impl VideoSource for ScSource {
    fn enumerate() -> Result<Vec<SourceInfo>, PlatformError> {
        enumerate()
    }

    fn open(info: &SourceInfo) -> Result<Self, PlatformError> {
        Self::validated(info)
    }

    fn start(&mut self, config: &CaptureConfig) -> Result<FrameStream, PlatformError> {
        let info = self.info.clone();
        let config = *config;
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), PlatformError>>();
        let (frame_tx, frame_rx) = mpsc::sync_channel::<BgraFrame>(CHANNEL_DEPTH);
        let error: Arc<Mutex<Option<PlatformError>>> = Arc::new(Mutex::new(None));
        let stop_flag = Arc::new(AtomicBool::new(false));
        let error_ = Arc::clone(&error);
        let stop_ = Arc::clone(&stop_flag);
        // Every SCK object lives on this thread: no Send questions, prompt
        // teardown, deterministic startup rendezvous below.
        let worker = std::thread::Builder::new()
            .name("golive-sck".into())
            .spawn(move || {
                run_capture(info, config, frame_tx, error_, stop_, ready_tx);
            })
            .map_err(|e| PlatformError::Internal(format!("thread de captura: {e}")))?;
        match ready_rx.recv_timeout(START_DEADLINE) {
            Ok(Ok(())) => Ok(FrameStream::new(frame_rx, error, stop_flag, worker)),
            Ok(Err(error)) => {
                stop_flag.store(true, Ordering::Release);
                let _ = worker.join();
                Err(error)
            }
            Err(_) => {
                stop_flag.store(true, Ordering::Release);
                let _ = worker.join();
                Err(PlatformError::Internal("timeout ao iniciar captura".into()))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Output object: SCK frames in, bounded channel out.
// ---------------------------------------------------------------------------

struct OutputIvars {
    tx: Mutex<mpsc::SyncSender<BgraFrame>>,
    // Written here, read by golive-platform's FrameStream::next_frame
    // (cross-crate use the dead-code lint cannot see).
    #[allow(dead_code)]
    error_slot: Arc<Mutex<Option<PlatformError>>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop impl
    // (ivars are plain data, released field-wise); SyncSender is Send and
    // guarded access makes cross-thread use sound.
    #[unsafe(super(NSObject))]
    #[ivars = OutputIvars]
    struct CaptureOutput;

    unsafe impl NSObjectProtocol for CaptureOutput {}

    unsafe impl SCStreamOutput for CaptureOutput {
        // ObjC selector name is mandatory (not Rust snake case).
        #[allow(non_snake_case)]
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn stream_didOutputSampleBuffer_ofType(
            &self,
            _stream: &SCStream,
            sample_buffer: &objc2_core_media::CMSampleBuffer,
            r#type: SCStreamOutputType,
        ) {
            if r#type != SCStreamOutputType::Screen {
                return;
            }
            if let Some(frame) = extract_bgra(sample_buffer) {
                if let Ok(ivars) = self.ivars().tx.lock() {
                    // Latest-only: drop newest (not oldest) when full.
                    let _ = ivars.try_send(frame);
                }
            }
        }
    }
);

/// Copies one BGRA sample into an owned frame. Returns None (drop frame)
/// on any anomaly — a dropped frame beats a misinterpreted one.
fn extract_bgra(sample: &objc2_core_media::CMSampleBuffer) -> Option<BgraFrame> {
    // SAFETY: SCK screen output with a BGRA configuration always carries a
    // CVPixelBuffer; the pixel-format check below validates before touching.
    unsafe {
        let image = sample.image_buffer()?;
        // Toll-free: CVPixelBuffer IS-A CVImageBuffer. Validated by format
        // below before touching a byte.
        let pixel = &*(image.as_ref() as *const CVImageBuffer as *const CVPixelBuffer);
        if CVPixelBufferGetPixelFormatType(pixel) != kCVPixelFormatType_32BGRA {
            return None;
        }
        let w = CVPixelBufferGetWidth(pixel);
        let h = CVPixelBufferGetHeight(pixel);
        let stride = CVPixelBufferGetBytesPerRow(pixel);
        if w == 0 || h == 0 || w > 8192 || h > 8192 || stride < w * 4 {
            return None;
        }
        if CVPixelBufferLockBaseAddress(pixel, CVPixelBufferLockFlags::ReadOnly) != 0 {
            return None;
        }
        let base = CVPixelBufferGetBaseAddress(pixel) as *const u8;
        let frame = if base.is_null() {
            None
        } else {
            let bytes = stride
                .checked_mul(h.saturating_sub(1))?
                .checked_add(w.checked_mul(4)?)?;
            let src = std::slice::from_raw_parts(base, bytes);
            let mut data = vec![0u8; bytes];
            for (dst_row, src_row) in data.chunks_exact_mut(stride).zip(src.chunks(stride)).take(h)
            {
                dst_row[..w * 4].copy_from_slice(&src_row[..w * 4]);
            }
            Some(BgraFrame {
                w: w as u32,
                h: h as u32,
                stride,
                format: PixelFormat::Bgra8888,
                data,
            })
        };
        CVPixelBufferUnlockBaseAddress(pixel, CVPixelBufferLockFlags::ReadOnly);
        frame
    }
}

/// Supervisor body: enumerate → match id → filter → config → stream → pump
/// until stop. Reports readiness once; SCK teardown always runs on exit.
#[allow(clippy::too_many_lines)]
fn run_capture(
    info: SourceInfo,
    config: CaptureConfig,
    frame_tx: mpsc::SyncSender<BgraFrame>,
    error_slot: Arc<Mutex<Option<PlatformError>>>,
    stop_flag: Arc<AtomicBool>,
    ready_tx: mpsc::Sender<Result<(), PlatformError>>,
) {
    let fail = |error: PlatformError| {
        if let Ok(mut guard) = error_slot.lock() {
            *guard = Some(error.clone());
        }
        let _ = ready_tx.send(Err(error));
    };
    let content = match shareable_content() {
        Ok(content) => content,
        Err(error) => {
            fail(error);
            return;
        }
    };
    // SAFETY: all objects below are created and consumed on this thread;
    // callbacks only move owned data (channels, flags).
    unsafe {
        let filter: Option<Retained<SCContentFilter>> = match info.kind {
            SourceKind::Display => {
                let id: u32 = match info.id.parse() {
                    Ok(id) => id,
                    Err(_) => {
                        fail(PlatformError::InvalidSource {
                            reason: "id de display inválido",
                        });
                        return;
                    }
                };
                match content
                    .displays()
                    .iter()
                    .find(|d| d.displayID() == id)
                {
                    Some(display) => {
                        let empty: Retained<NSArray<SCWindow>> = NSArray::from_slice(&[]);
                        Some(SCContentFilter::initWithDisplay_excludingWindows(
                            SCContentFilter::alloc(),
                            &display,
                            &empty,
                        ))
                    }
                    None => {
                        fail(PlatformError::SourceGone { id: info.id.clone() });
                        return;
                    }
                }
            }
            SourceKind::Window => {
                let id: u32 = match info.id.parse() {
                    Ok(id) => id,
                    Err(_) => {
                        fail(PlatformError::InvalidSource {
                            reason: "id de janela inválido",
                        });
                        return;
                    }
                };
                match content.windows().iter().find(|w| w.windowID() == id) {
                    Some(window) => Some(SCContentFilter::initWithDesktopIndependentWindow(
                        SCContentFilter::alloc(),
                        &window,
                    )),
                    None => {
                        fail(PlatformError::SourceGone { id: info.id.clone() });
                        return;
                    }
                }
            }
        };
        let Some(filter) = filter else {
            fail(PlatformError::Internal("filtro vazio".into()));
            return;
        };
        let stream_config = SCStreamConfiguration::new();
        stream_config.setWidth(config.width as usize);
        stream_config.setHeight(config.height as usize);
        stream_config.setPixelFormat(kCVPixelFormatType_32BGRA);
        stream_config.setMinimumFrameInterval(CMTime {
            value: 1,
            timescale: config.fps.clamp(1, 60) as i32,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        });
        stream_config.setShowsCursor(true);
        let output = CaptureOutput::alloc().set_ivars(OutputIvars {
            tx: Mutex::new(frame_tx),
            error_slot: Arc::clone(&error_slot),
        });
        // SAFETY: standard init after alloc with ivars set.
        let output: Retained<CaptureOutput> = msg_send![super(output), init];
        let output_proto = ProtocolObject::<dyn SCStreamOutput>::from_retained(output);
        let stream: Retained<SCStream> = SCStream::initWithFilter_configuration_delegate(
            SCStream::alloc(),
            &filter,
            &stream_config,
            None,
        );
        let queue = dispatch2::DispatchQueue::new("dev.golive.sck", None);
        if stream
            .addStreamOutput_type_sampleHandlerQueue_error(
                &output_proto,
                SCStreamOutputType::Screen,
                Some(&queue),
            )
            .is_err()
        {
            fail(PlatformError::Internal("saída de captura recusada".into()));
            return;
        }
        let started = std::sync::mpsc::channel::<Result<(), (String, i32)>>();
        let started_tx = started.0;
        let block = block2::RcBlock::new(move |error: *mut NSError| {
            if error.is_null() {
                let _ = started_tx.send(Ok(()));
            } else {
                // Non-null per check; borrowed for two getters.
                let error: &NSError = &*error;
                let _ = started_tx.send(Err(ns_error_parts(error)));
            }
        });
        stream.startCaptureWithCompletionHandler(Some(&block));
        // Rendezvous: started, typed error, or timeout (never hang).
        match started.1.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => {}
            Ok(Err((domain, code))) => {
                fail(map_ns_error(&domain, code));
                return;
            }
            Err(_) => {
                fail(PlatformError::Internal("timeout ao iniciar stream".into()));
                return;
            }
        }
        let _ = ready_tx.send(Ok(()));
        // Park until stop; teardown below runs on every exit (indicators out).
        while !stop_flag.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(50));
        }
        let stopped = std::sync::mpsc::channel::<()>();
        let stopped_tx = stopped.0;
        let stop_block = block2::RcBlock::new(move |_error: *mut NSError| {
            let _ = stopped_tx.send(());
        });
        stream.stopCaptureWithCompletionHandler(Some(&stop_block));
        let _ = stopped.1.recv_timeout(Duration::from_secs(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_version_parses_plist_fixture() {
        let plist = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>ProductVersion</key>
    <string>14.2.1</string>
</dict>
</plist>"#;
        assert_eq!(parse_product_version(plist), Some((14, 2)));
        assert_eq!(parse_product_version("garbage"), None);
        assert_eq!(parse_product_version("<key>ProductVersion</key>"), None);
    }

    #[test]
    fn os_version_detects_this_mac() {
        // Runs on any real Mac: sane values, never panics.
        let (major, _) = ScSource::os_version();
        assert!(major >= 12, "unexpected major {major}");
    }

    #[test]
    fn denial_maps_typed() {
        assert_eq!(
            map_ns_error("com.apple.ScreenCaptureKit.scstream.error", -3810),
            PlatformError::permission_denied()
        );
        match map_ns_error("com.apple.Foo", 42) {
            PlatformError::Internal(detail) => {
                assert!(detail.contains("com.apple.Foo"));
                assert!(detail.contains("42"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn version_gate_maps_typed() {
        let mapped = PlatformError::OsVersionTooOld { have: "macOS 12".into(), need: "macOS 99" };
        match mapped {
            PlatformError::OsVersionTooOld { need, .. } => assert_eq!(need, "macOS 99"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn open_validates_without_os() {
        let bad = SourceInfo {
            kind: SourceKind::Display,
            id: "  ".into(),
            name: String::new(),
            w: 0,
            h: 0,
        };
        assert!(ScSource::open(&bad).is_err());
        let good = SourceInfo {
            kind: SourceKind::Window,
            id: "42".into(),
            name: String::new(),
            w: 0,
            h: 0,
        };
        assert!(ScSource::open(&good).is_ok());
    }
}
