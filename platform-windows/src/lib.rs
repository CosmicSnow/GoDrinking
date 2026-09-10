//! Windows capture backend: DXGI Desktop Duplication (displays) +
//! Windows.Graphics.Capture (windows).
//!
//! Mapping to the pure contract (`golive-platform`):
//! - `enumerate()` lists DXGI outputs (displays) and top-level windows.
//!   Empty on a real desktop maps to [`PlatformError::PermissionDenied`].
//! - `open()` validates id/kind against a fresh enumeration (gone →
//!   `SourceGone`).
//! - `start()` creates a D3D11 device, acquires duplication or a WGC
//!   session, and spawns a pump thread feeding a bounded latest-only
//!   channel of [`BgraFrame`]. Startup is rendezvous-bounded.
//! - `thumbnail()` is a one-shot still (same BGRA shape as macOS).
//!
//! Frames out, errors typed. No RTP/WebRTC/lifecycle here. Titles, pixels,
//! tokens, and SDP never reach logs (aggregate counts + kind only).

use golive_platform::{
    BgraFrame, CaptureConfig, CapturePacket, FrameStream, PlatformError, RestartOrder, SourceInfo,
    SourceKind, VideoSource,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

mod copy;
mod d3d;
mod dxgi;
mod map;
mod wgc;

pub use copy::{copy_tight_bgra, gate_open, initial_last_ns, interval_ns};

/// Copy shown when Windows denies capture. Points at Settings, never at a title.
pub const PERMISSION_HINT: &str =
    "Sem permissão de captura de tela — autorize em Configurações → Privacidade e segurança e tente de novo.";

const START_DEADLINE: Duration = Duration::from_secs(8);
const CHANNEL_DEPTH: usize = 2;

/// A Windows display or window selected from [`enumerate`].
#[derive(Clone, Debug)]
pub struct WindowsSource {
    info: SourceInfo,
}

impl WindowsSource {
    fn validated(info: &SourceInfo) -> Result<Self, PlatformError> {
        if info.id.trim().is_empty() {
            return Err(PlatformError::InvalidSource { reason: "id vazio" });
        }
        match info.kind {
            SourceKind::Display | SourceKind::Window => Ok(Self { info: info.clone() }),
        }
    }
}

/// List capture targets on this PC. Empty on a real desktop means denial
/// hid the content, so empty maps to `PermissionDenied`, never a silent UI.
pub fn enumerate() -> Result<Vec<SourceInfo>, PlatformError> {
    init_com();
    let mut out = dxgi::enumerate_displays()?;
    match wgc::enumerate_windows() {
        Ok(mut windows) => out.append(&mut windows),
        Err(error) => {
            if out.is_empty() {
                return Err(error);
            }
        }
    }
    dxgi::empty_is_denied(&out)
}

/// One-shot still for the share-modal preview. Failures are typed; titles
/// and pixels never reach logs.
pub fn thumbnail(kind: SourceKind, id: &str) -> Result<BgraFrame, PlatformError> {
    init_com();
    let id = id.trim();
    if id.is_empty() {
        return Err(PlatformError::InvalidSource { reason: "id vazio" });
    }
    match kind {
        SourceKind::Display => dxgi::thumbnail_display(id),
        SourceKind::Window => wgc::thumbnail_window(id),
    }
}

impl VideoSource for WindowsSource {
    fn enumerate() -> Result<Vec<SourceInfo>, PlatformError> {
        enumerate()
    }

    fn open(info: &SourceInfo) -> Result<Self, PlatformError> {
        let source = Self::validated(info)?;
        let list = enumerate()?;
        if copy::find_source(&list, info.kind, info.id.trim()).is_none() {
            return Err(PlatformError::SourceGone { id: info.id.clone() });
        }
        Ok(source)
    }

    fn start(&mut self, config: &CaptureConfig) -> Result<FrameStream, PlatformError> {
        let list = enumerate()?;
        if copy::find_source(&list, self.info.kind, self.info.id.trim()).is_none() {
            return Err(PlatformError::SourceGone {
                id: self.info.id.clone(),
            });
        }
        let info = self.info.clone();
        let config = *config;
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), PlatformError>>();
        let (frame_tx, frame_rx) = mpsc::sync_channel::<CapturePacket>(CHANNEL_DEPTH);
        let error: Arc<Mutex<Option<PlatformError>>> = Arc::new(Mutex::new(None));
        let stop_flag = Arc::new(AtomicBool::new(false));
        let error_ = Arc::clone(&error);
        let stop_ = Arc::clone(&stop_flag);
        let worker = std::thread::Builder::new()
            .name("golive-wgc".into())
            .spawn(move || {
                init_com();
                match info.kind {
                    SourceKind::Display => dxgi::run_display(
                        info.id, config, frame_tx, stop_, error_, ready_tx,
                    ),
                    SourceKind::Window => wgc::run_window(
                        info.id, config, frame_tx, stop_, error_, ready_tx,
                    ),
                }
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

    fn restart_order(info: &SourceInfo) -> RestartOrder {
        match info.kind {
            // DXGI allows a single duplication per process per output: a
            // second DuplicateOutput while the old stream is alive fails
            // E_INVALIDARG, so the old stream must stop + join first
            // (brief blackout gap during the switch).
            SourceKind::Display => RestartOrder::StopFirst,
            // WGC sessions are independent per (item, pool) objects and the
            // DWM composes for concurrent sessions, so windows keep the
            // glitch-free new-first restart.
            SourceKind::Window => RestartOrder::NewFirst,
        }
    }
}

fn init_com() {
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
    use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use golive_platform::SourceKind;

    fn display(id: &str) -> SourceInfo {
        SourceInfo {
            kind: SourceKind::Display,
            id: id.into(),
            name: "Display 1 · 1920x1080".into(),
            w: 1920,
            h: 1080,
        }
    }

    #[test]
    fn open_validates_without_os() {        let bad = SourceInfo {
            kind: SourceKind::Display,
            id: "  ".into(),
            name: String::new(),
            w: 0,
            h: 0,
        };
        assert!(matches!(
            WindowsSource::validated(&bad).unwrap_err(),
            PlatformError::InvalidSource { .. }
        ));
        assert!(WindowsSource::validated(&display("\\\\.\\DISPLAY1")).is_ok());
    }

    /// Restart ordering per kind (no OS: pure mapping). DXGI displays must
    /// stop first (one duplication per process per output — a concurrent
    /// DuplicateOutput fails E_INVALIDARG); WGC windows keep new-first.
    /// Windows-only build: runs on Windows CI, not on macOS/Linux hosts.
    #[test]
    fn restart_order_is_stop_first_for_dxgi_displays() {
        assert_eq!(
            WindowsSource::restart_order(&display("\\\\.\\DISPLAY1")),
            RestartOrder::StopFirst
        );
        let window = SourceInfo {
            kind: SourceKind::Window,
            id: "12345".into(),
            name: "Janela".into(),
            w: 0,
            h: 0,
        };
        assert_eq!(WindowsSource::restart_order(&window), RestartOrder::NewFirst);
    }

    #[test]
    fn empty_list_is_denied_never_silent() {
        let error = dxgi::empty_is_denied(&[]).unwrap_err();
        assert!(matches!(error, PlatformError::PermissionDenied { .. }));
        assert!(error.to_string().contains("Privacidade"));
        assert!(dxgi::empty_is_denied(&[display("1")]).is_ok());
    }

    #[test]
    fn gone_match_is_by_kind_and_id() {
        let list = vec![display("\\\\.\\DISPLAY1")];
        assert!(copy::find_source(&list, SourceKind::Display, "\\\\.\\DISPLAY1").is_some());
        assert!(copy::find_source(&list, SourceKind::Window, "\\\\.\\DISPLAY1").is_none());
        assert!(copy::find_source(&list, SourceKind::Display, "gone").is_none());
    }

    #[test]
    fn denial_hresult_maps_typed() {
        assert!(matches!(
            map::map_hresult(map::hresult_i32(windows::Win32::Foundation::E_ACCESSDENIED)),
            PlatformError::PermissionDenied { .. }
        ));
        assert!(matches!(
            map::map_hresult(map::hresult_i32(
                windows::Win32::Graphics::Dxgi::DXGI_ERROR_ACCESS_DENIED
            )),
            PlatformError::PermissionDenied { .. }
        ));
        assert!(matches!(
            map::map_hresult(map::hresult_i32(
                windows::Win32::Graphics::Dxgi::DXGI_ERROR_NOT_CURRENTLY_AVAILABLE
            )),
            PlatformError::PermissionDenied { .. }
        ));
        match map::map_hresult(42) {
            PlatformError::Internal(detail) => assert!(detail.contains("42")),
            other => panic!("unexpected {other:?}"),
        }
        assert!(map::is_wait_timeout(map::hresult_i32(
            windows::Win32::Graphics::Dxgi::DXGI_ERROR_WAIT_TIMEOUT
        )));
        assert!(map::is_access_lost(map::hresult_i32(
            windows::Win32::Graphics::Dxgi::DXGI_ERROR_ACCESS_LOST
        )));
    }

    #[test]
    fn gate_opens_exactly_on_cadence() {
        let interval = interval_ns(15);
        assert!(gate_open(initial_last_ns(0, interval), 0, interval));
        assert!(!gate_open(0, interval - 1, interval));
        assert!(gate_open(0, interval, interval));
    }

    #[test]
    fn copy_tight_bgra_drops_stride_padding() {
        let mut src = vec![0u8; 8 * 2];
        src[0..4].copy_from_slice(&[1, 2, 3, 4]);
        src[8..12].copy_from_slice(&[5, 6, 7, 8]);
        let frame = copy_tight_bgra(&src, 1, 2, 8).unwrap();
        assert_eq!(frame.stride, 4);
        assert_eq!(frame.data, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(copy_tight_bgra(&src, 0, 2, 8).is_none());
        assert!(copy_tight_bgra(&src, 3, 2, 8).is_none());
    }

    #[test]
    fn stream_interval_math_matches_profile_fps() {
        assert_eq!(interval_ns(30), 33_333_333);
        assert_eq!(interval_ns(15), 66_666_666);
        assert_eq!(interval_ns(0), 1_000_000_000);
    }
}
