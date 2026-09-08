//! What the current platform can capture. Compile-time facts with a
//! human-readable `reason` — the UI disables options with the explanation
//! instead of failing silently. Runtime denial (TCC) still surfaces as
//! [`crate::PlatformError::PermissionDenied`] at enumerate/start time.

use serde::Serialize;

/// One capability verdict. `Serialize`-only by design: capabilities flow
/// one way (backend → UI); borrowed `&'static` reasons stay zero-copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Support {
    pub supported: bool,
    pub reason: &'static str,
}

// `yes` is unused on targets where every capability is negative today
// (Windows stub); kept so each cfg arm reads identically.
#[allow(dead_code)]
const fn yes(reason: &'static str) -> Support {
    Support { supported: true, reason }
}

const fn no(reason: &'static str) -> Support {
    Support { supported: false, reason }
}

/// The four axes the UI cares about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct CapabilitySet {
    pub display: Support,
    pub window: Support,
    pub app_audio: Support,
    pub exclusion: Support,
}

/// Compile-time capabilities for this build. Audio capture is a later lane
/// everywhere, so it is honestly unsupported on all platforms for now.
pub const fn capabilities() -> CapabilitySet {
    #[cfg(target_os = "macos")]
    {
        CapabilitySet {
            display: yes("ScreenCaptureKit (pode pedir permissão no primeiro uso)"),
            window: yes("ScreenCaptureKit (pode pedir permissão no primeiro uso)"),
            app_audio: no("planejado (lane de áudio)"),
            exclusion: yes("SCContentFilter suporta excluir janelas"),
        }
    }
    #[cfg(target_os = "windows")]
    {
        CapabilitySet {
            display: no("planejado (WGC/DXGI)"),
            window: no("planejado (WGC/DXGI)"),
            app_audio: no("planejado (lane de áudio)"),
            exclusion: no("planejado (WGC/DXGI)"),
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        CapabilitySet {
            display: no("apenas macOS/Windows"),
            window: no("apenas macOS/Windows"),
            app_audio: no("planejado (lane de áudio)"),
            exclusion: no("apenas macOS/Windows"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_are_never_empty() {
        let caps = capabilities();
        for support in [caps.display, caps.window, caps.app_audio, caps.exclusion] {
            assert!(!support.reason.is_empty());
        }
    }

    #[test]
    fn audio_is_honestly_unsupported_everywhere() {
        assert!(!capabilities().app_audio.supported);
    }
}
