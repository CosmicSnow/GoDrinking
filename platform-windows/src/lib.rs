//! Windows capture backend: honest stub today, DXGI/WGC tomorrow.
//!
//! API mirrors the macOS backend crate (`golive-platform-macos`) on purpose
//! (`WindowsSource` + free [`enumerate`] + free [`thumbnail`]) so the app
//! glue (`golive-app/src/screen.rs`) delegates per OS with no UI changes:
//! same [`golive_platform::SourceInfo`] shapes in, same typed
//! [`golive_platform::PlatformError`] out.
//!
//! Current behavior (PRESERVED from the pre-crate stub): capture is
//! unavailable — capabilities report unsupported, `enumerate` is denied
//! with an honest reason, `open` validates without touching the OS,
//! `start`/`thumbnail` refuse typed. No silence, no empty lists, no panics.
//!
//! Where the real DXGI/WGC capture plugs in (all three are TODOs below):
//! - `enumerate()`: list adapters via `IDXGIAdapter1` + outputs via
//!   `IDXGIOutput1::DuplicateOutput` (displays), windows via the
//!   GraphicsCapturePicker (WGC). Map each to `SourceInfo`.
//! - `WindowsSource::open()`: validate the id/kind against a fresh
//!   enumeration (gone → `SourceGone`), hold the adapter/output handles.
//! - `WindowsSource::start()`: create the D3D11 device, acquire
//!   `IDXGIOutputDuplication`, spawn the pump thread feeding a bounded
//!   latest-only channel of `BgraFrame` (reuse
//!   [`golive_platform::bgra_to_i420`] downstream unchanged), rendezvous-
//!   bounded startup, deadline-bounded `stop` (indicators released).
//! Expected dependency then: the `windows` crate
//! (`Windows::Graphics::Capture`, `Windows::Win32::Graphics::{Dxgi,
//! Direct3D11}`). Permission model differs from macOS TCC (capture picker,
//! no equivalent): surface denial as `PermissionDenied` with its own hint.
//! Keep the adapter small: frames out, errors typed — no RTP/WebRTC/
//! lifecycle in here (same rule as every other backend).

use golive_platform::{
    BgraFrame, CaptureConfig, FrameStream, PlatformError, SourceInfo, SourceKind, VideoSource,
};

/// A Windows display or window selected from [`enumerate`].
#[derive(Clone, Debug)]
pub struct WindowsSource {
    // Held for shape parity with real backends (populated at plug time:
    // adapter LUID + output index, or WGC item token).
    #[allow(dead_code)]
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

/// List capture targets on this PC.
///
/// TODO(DXGI): enumerate adapters/outputs (displays) + picker items
/// (windows) and return one `SourceInfo` per target. Until then this is
/// denied with an honest reason — never an empty list on a real desktop
/// (same rule as every backend: empty-on-desktop means denial, not "none").
pub fn enumerate() -> Result<Vec<SourceInfo>, PlatformError> {
    Err(PlatformError::UnsupportedPlatform {
        reason: "captura de tela: apenas macOS (Windows planejado)",
    })
}

/// One-shot still for the share-modal preview (single synchronous grab).
///
/// TODO(DXGI): implement via `IDXGIOutputDuplication::AcquireNextFrame`
/// (display) / WGC one-shot (window), returning tight BGRA pixels like the
/// macOS `thumbnail`. Until then: typed refusal, same as capture.
pub fn thumbnail(kind: SourceKind, id: &str) -> Result<BgraFrame, PlatformError> {
    let _ = (kind, id);
    Err(PlatformError::UnsupportedPlatform {
        reason: "miniaturas: apenas macOS (Windows planejado)",
    })
}

impl VideoSource for WindowsSource {
    fn enumerate() -> Result<Vec<SourceInfo>, PlatformError> {
        enumerate()
    }

    fn open(info: &SourceInfo) -> Result<Self, PlatformError> {
        // TODO(DXGI): validate id/kind against a fresh `enumerate()` here
        // (gone → `SourceGone`) and retain the adapter/output handles.
        // Validation-only today: fast, no OS contact.
        Self::validated(info)
    }

    fn start(&mut self, config: &CaptureConfig) -> Result<FrameStream, PlatformError> {
        // TODO(DXGI): D3D11 device + DuplicateOutput + pump thread (bounded
        // latest-only channel, rendezvous-bounded startup, deadline-bounded
        // stop). First OS contact happens here — this is where the capture
        // picker/consent surfaces.
        let _ = config;
        Err(PlatformError::UnsupportedPlatform {
            reason: "captura de tela: apenas macOS (Windows planejado)",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn enumerate_is_denied_never_silent() {
        // A real desktop with zero sources would be denial hiding content;
        // the stub denies honestly instead of returning an empty list.
        let error = enumerate().unwrap_err();
        assert!(matches!(error, PlatformError::UnsupportedPlatform { .. }));
        assert!(WindowsSource::enumerate().is_err());
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
        assert!(matches!(
            WindowsSource::open(&bad).unwrap_err(),
            PlatformError::InvalidSource { .. }
        ));
        assert!(WindowsSource::open(&display("1")).is_ok());
    }

    #[test]
    fn start_and_thumbnail_refuse_typed() {
        let mut source = WindowsSource::open(&display("1")).unwrap();
        assert!(matches!(
            source.start(&CaptureConfig::default()),
            Err(PlatformError::UnsupportedPlatform { .. })
        ));
        assert!(thumbnail(SourceKind::Display, "1").is_err());
    }
}
