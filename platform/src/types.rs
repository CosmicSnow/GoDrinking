//! Plain data types. No OS calls, no threads — safe to construct anywhere,
//! including tests and the UI layer.

use serde::{Deserialize, Serialize};

/// What can be captured.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    Display,
    Window,
}

/// One capturable source, as listed for the UI. `id` is the OS handle
/// rendered opaque (decimal display/window id); `name` is user-facing text
/// (window titles may be sensitive — display it, never log it).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceInfo {
    pub kind: SourceKind,
    pub id: String,
    pub name: String,
    pub w: u32,
    pub h: u32,
}

/// Requested output geometry + rate. Backends scale to fit; the core scales
/// again to the contract size, so this is a quality hint, not a promise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self { width: 1920, height: 1080, fps: 30 }
    }
}

/// Declared pixel layout of [`BgraFrame`]. Only BGRA exists today; the enum
/// keeps future formats (P010, NV12) from becoming silent reinterpretations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PixelFormat {
    Bgra8888,
}

/// One captured frame: packed BGRA rows. `stride >= w*4`; `data.len()` must
/// cover `stride*(h-1) + w*4`. No timestamps here — pacing belongs to the
/// consumer (the core paces by frame counter, never wall-clock).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BgraFrame {
    pub w: u32,
    pub h: u32,
    pub stride: usize,
    pub format: PixelFormat,
    pub data: Vec<u8>,
}

/// Planar 4:2:0 result: contiguous Y then U then V, no padding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanarYuv {
    pub w: u32,
    pub h: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_info_roundtrips_for_tauri() {
        let info = SourceInfo {
            kind: SourceKind::Display,
            id: "1".into(),
            name: "Display 1 · 2560x1440".into(),
            w: 2560,
            h: 1440,
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("\"display\""));
        let back: SourceInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(info, back);
    }
}
