//! System-audio types shared by OS backends. No OS calls.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// One running app the host can exclude from system-audio capture.
/// `id` is the exclusion token (bundle id or exe name); `name` is UI copy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioApp {
    pub name: String,
    pub id: String,
    pub pid: i32,
    #[serde(default)]
    pub emitting_audio: bool,
}

/// One Opus frame ready for the WebRTC audio track. 20 ms, 48 kHz stereo.
#[derive(Clone, Debug)]
pub struct EncodedAudioPacket {
    pub data: Vec<u8>,
    pub duration: Duration,
}

/// Case-insensitive exclusion matcher. A selected token excludes an app when
/// the token equals or is contained in the app's name or bundle identifier.
pub fn app_excluded_by_token(name: &str, bundle_id: Option<&str>, token: &str) -> bool {
    let token = token.trim().to_ascii_lowercase();
    if token.is_empty() {
        return false;
    }
    if name.to_ascii_lowercase().contains(&token) {
        return true;
    }
    bundle_id
        .map(|bundle| bundle.to_ascii_lowercase().contains(&token))
        .unwrap_or(false)
}

/// Default exclusion tokens for display-share system audio: Discord (call
/// echo leaks into the room) plus our own processes (app + golive-video
/// helper). Matched with [`app_excluded_by_token`] (case-insensitive
/// contains on name/bundle id/exe), so one entry covers renames and helper
/// processes. The frontend mirrors this list (`DEFAULT_AUDIO_EXCLUSION_TOKENS`
/// in app/web/src/views.tsx) — keep the two in sync.
pub const DEFAULT_EXCLUDED_TOKENS: &[&str] = &[
    "Discord",
    "com.hnc.Discord",
    "Discord.exe",
    "dev.golive.sala",
    "goDrinking",
    "goDrinking.exe",
    "golive-video",
    "golive-video.exe",
];

/// Owned copy of [`DEFAULT_EXCLUDED_TOKENS`] for share-start defaults.
pub fn default_excluded_tokens() -> Vec<String> {
    DEFAULT_EXCLUDED_TOKENS
        .iter()
        .map(|token| token.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{app_excluded_by_token, DEFAULT_EXCLUDED_TOKENS};

    #[test]
    fn discord_name_and_helper_bundles_all_match() {
        assert!(app_excluded_by_token("Discord", None, "Discord"));
        assert!(app_excluded_by_token("Discord Helper (Renderer)", None, "Discord"));
        assert!(app_excluded_by_token(
            "Discord",
            Some("com.hnc.Discord"),
            "com.hnc.Discord"
        ));
        assert!(app_excluded_by_token(
            "Discord Helper",
            Some("com.hnc.Discord.helper"),
            "com.hnc.Discord"
        ));
        assert!(app_excluded_by_token(
            "discord",
            Some("COM.HNC.DISCORD"),
            "Discord"
        ));
        assert!(app_excluded_by_token(
            "Discord Helper",
            Some("com.hnc.Discord.helper"),
            "discord"
        ));
    }

    #[test]
    fn unrelated_apps_do_not_match() {
        assert!(!app_excluded_by_token(
            "Safari",
            Some("com.apple.Safari"),
            "Discord"
        ));
        assert!(!app_excluded_by_token(
            "Google Chrome",
            Some("com.google.Chrome"),
            "discord"
        ));
        assert!(!app_excluded_by_token(
            "Slack",
            Some("com.tinyspeck.slackmacgap"),
            "Discord"
        ));
        assert!(!app_excluded_by_token(
            "Discord",
            Some("com.hnc.Discord"),
            ""
        ));
        assert!(!app_excluded_by_token(
            "Discord",
            Some("com.hnc.Discord"),
            "   "
        ));
    }

    #[test]
    fn default_tokens_block_discord_and_self_but_not_unrelated() {
        let matches = |name: &str, bundle: Option<&str>| {
            DEFAULT_EXCLUDED_TOKENS
                .iter()
                .any(|token| app_excluded_by_token(name, bundle, token))
        };
        // Discord (app, helper, exe spellings).
        assert!(matches("Discord", Some("com.hnc.Discord")));
        assert!(matches("Discord Helper (Renderer)", None));
        assert!(matches("Discord", Some("com.hnc.Discord.helper")));
        assert!(matches("Discord", None));
        // Own app (Tauri bundle id) + helper (exe/process names).
        assert!(matches("goDrinking", Some("dev.golive.sala")));
        assert!(matches("goDrinking", None));
        assert!(matches("golive-video", None));
        // Unrelated apps stay audible.
        assert!(!matches("Safari", Some("com.apple.Safari")));
        assert!(!matches("Slack", Some("com.tinyspeck.slackmacgap")));
    }
}
