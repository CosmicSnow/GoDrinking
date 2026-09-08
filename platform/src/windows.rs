//! Windows backend stub: compiles the contract today, captures tomorrow.
//!
//! Plug point for the real implementation (WGC/DXGI):
//! - file: this one (`platform/src/windows.rs`), gated
//!   `#[cfg(target_os = "windows")]`;
//! - trait methods to implement: [`crate::VideoSource::enumerate`],
//!   [`crate::VideoSource::open`], [`crate::VideoSource::start`], plus
//!   [`crate::FrameStream::stop`] semantics (deadline-bounded, indicators
//!   released);
//! - expected dependencies: `windows` crate (Windows::Graphics::Capture,
//!   Windows::Win32::Graphics::Dxgi / Direct3D11) for frame acquisition,
//!   reusing [`crate::BgraFrame`] + [`crate::convert::bgra_to_i420`]
//!   unchanged;
//! - keep every adapter small: no RTP/WebRTC/lifecycle in here — frames out,
//!   errors typed, nothing else.

use crate::error::PlatformError;
use crate::traits::{FrameStream, VideoSource};
use crate::types::{CaptureConfig, SourceInfo};

/// Placeholder source: every entry point explains itself instead of failing
/// mute. `#[cfg(target_os = "windows")]` only.
pub struct WindowsSource {
    // Held for shape parity with real backends (constructed at plug time).
    #[allow(dead_code)]
    info: SourceInfo,
}

impl VideoSource for WindowsSource {
    fn enumerate() -> Result<Vec<SourceInfo>, PlatformError> {
        Err(PlatformError::UnsupportedPlatform {
            reason: "planejado (WGC/DXGI) — veja platform/src/windows.rs",
        })
    }

    fn open(info: &SourceInfo) -> Result<Self, PlatformError> {
        if info.id.trim().is_empty() {
            return Err(PlatformError::InvalidSource { reason: "id vazio" });
        }
        Err(PlatformError::UnsupportedPlatform {
            reason: "planejado (WGC/DXGI) — veja platform/src/windows.rs",
        })
    }

    fn start(&mut self, _config: &CaptureConfig) -> Result<FrameStream, PlatformError> {
        Err(PlatformError::UnsupportedPlatform {
            reason: "planejado (WGC/DXGI) — veja platform/src/windows.rs",
        })
    }
}
