use super::screen_capture_kit::{ScreenCaptureKitAdapter, ScreenRecordingAuthorization};
use serde::Serialize;

/// How accurately the native platform can honor per-application audio
/// exclusions. Browser/WebView capture cannot enforce these rules.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AppAudioExclusionSupport {
    Enhanced,
    BestEffort,
    Unsupported,
}

/// Platform media capability information. macOS native capture is implemented
/// for ScreenCaptureKit; Windows remains an explicit future adapter.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MediaCapabilities {
    pub platform: String,
    pub supported: bool,
    pub screen_capture_kit: bool,
    pub screen_recording_authorization: ScreenRecordingAuthorization,
    pub source_enumeration_available: bool,
    pub windows_graphics_capture: bool,
    pub wasapi: bool,
    pub process_loopback: bool,
    pub app_audio_exclusion: AppAudioExclusionSupport,
    pub native_capture_implemented: bool,
    pub native_encoder_implemented: bool,
    pub native_peer_transport_implemented: bool,
    /// Host can encode AV1 (VideoToolbox M3+ on macOS; false elsewhere for now).
    pub av1_encode_supported: bool,
    pub detail: String,
}

pub fn detect() -> MediaCapabilities {
    #[cfg(target_os = "macos")]
    {
        let adapter = ScreenCaptureKitAdapter::new();
        let availability = adapter.availability();
        return MediaCapabilities {
            platform: "macos".into(),
            supported: true,
            screen_capture_kit: availability.framework_available,
            screen_recording_authorization: availability.authorization,
            source_enumeration_available: availability.source_enumeration_available,
            windows_graphics_capture: false,
            wasapi: false,
            process_loopback: false,
            app_audio_exclusion: if macos_process_tap_available() {
                AppAudioExclusionSupport::Enhanced
            } else {
                AppAudioExclusionSupport::Unsupported
            },
            native_capture_implemented: true,
            native_encoder_implemented: true,
            native_peer_transport_implemented: true,
            av1_encode_supported: super::video_toolbox::av1_encode_supported(),
            detail: format!(
                "{} Local-only native WebRTC is available. On macOS 14.2+, system audio can exclude selected apps.",
                availability.detail
            ),
        };
    }

    #[cfg(target_os = "windows")]
    {
        return MediaCapabilities {
            platform: "windows".into(),
            supported: true,
            screen_capture_kit: false,
            screen_recording_authorization: ScreenRecordingAuthorization::Unsupported,
            source_enumeration_available: true,
            windows_graphics_capture: true,
            wasapi: true,
            process_loopback: false,
            app_audio_exclusion: AppAudioExclusionSupport::Unsupported,
            native_capture_implemented: true,
            native_encoder_implemented: true,
            native_peer_transport_implemented: true,
            av1_encode_supported: false,
            detail: "Windows Graphics Capture and OpenH264 native capture are available. System audio is a full device loopback; per-app audio exclusion is unsupported (no mix-minus tap).".into(),
        };
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        MediaCapabilities {
            platform: std::env::consts::OS.into(),
            supported: false,
            screen_capture_kit: false,
            screen_recording_authorization: ScreenRecordingAuthorization::Unsupported,
            source_enumeration_available: false,
            windows_graphics_capture: false,
            wasapi: false,
            process_loopback: false,
            app_audio_exclusion: AppAudioExclusionSupport::Unsupported,
            native_capture_implemented: false,
            native_encoder_implemented: false,
            native_peer_transport_implemented: false,
            av1_encode_supported: false,
            detail: "Native media capture is unsupported on this platform; the WebView path remains the active implementation.".into(),
        }
    }
}

#[cfg(target_os = "macos")]
fn macos_process_tap_available() -> bool {
    let version = objc2_foundation::NSProcessInfo::processInfo().operatingSystemVersion();
    version.majorVersion > 14 || (version.majorVersion == 14 && version.minorVersion >= 2)
}

#[cfg(test)]
mod tests {
    use super::{detect, AppAudioExclusionSupport};

    #[test]
    fn capabilities_are_explicit_about_native_implementation() {
        let capabilities = detect();
        #[cfg(target_os = "macos")]
        assert!(capabilities.native_capture_implemented);
        #[cfg(not(target_os = "macos"))]
        assert!(!capabilities.native_capture_implemented);
        #[cfg(not(target_os = "macos"))]
        assert!(!capabilities.native_encoder_implemented);
        #[cfg(target_os = "macos")]
        assert!(capabilities.native_encoder_implemented);
        #[cfg(target_os = "macos")]
        assert!(capabilities.native_peer_transport_implemented);
        assert!(!capabilities.detail.is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_reports_screen_capture_kit_and_process_tap_audio_exclusion() {
        let capabilities = detect();
        assert!(capabilities.screen_capture_kit);
        assert_ne!(
            capabilities.app_audio_exclusion,
            AppAudioExclusionSupport::BestEffort
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_reports_native_media_as_implemented() {
        let capabilities = detect();
        assert!(capabilities.supported);
        assert!(capabilities.windows_graphics_capture);
        assert!(capabilities.wasapi);
        assert!(capabilities.native_capture_implemented);
        assert!(capabilities.native_encoder_implemented);
        assert!(capabilities.native_peer_transport_implemented);
        assert!(capabilities.source_enumeration_available);
        assert_eq!(
            capabilities.app_audio_exclusion,
            AppAudioExclusionSupport::Unsupported
        );
        assert!(!capabilities.detail.is_empty());
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn other_platforms_are_unsupported() {
        let capabilities = detect();
        assert!(!capabilities.supported);
        assert_eq!(
            capabilities.app_audio_exclusion,
            AppAudioExclusionSupport::Unsupported
        );
    }
}
