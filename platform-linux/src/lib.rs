//! Linux capture backend: xdg-desktop-portal for pixels, PipeWire for audio.
//!
//! The compositor does not let a client pick a monitor by itself, so the
//! first `start` confirms the source in the desktop dialog. A restore token
//! skips that dialog afterwards. Per-app audio is a copy of each playback
//! node — sinks are never moved.

mod audio;
mod mix;
mod outputs;
mod pixels;
mod portal;
mod thumbnail;
mod video;

pub use audio::{list_audio_apps, start_audio_tap, start_audio_tap_include, AudioTap};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use golive_platform::{
    BgraFrame, CaptureConfig, CapturePacket, FrameStream, PlatformError, RestartOrder, SourceInfo,
    SourceKind, VideoSource,
};

pub const PORTAL_WINDOW_ID: &str = "portal-window";
pub const PORTAL_DISPLAY_ID: &str = "portal-display";

const START_DEADLINE: Duration = Duration::from_secs(125);
const CHANNEL_DEPTH: usize = 2;

#[derive(Clone, Debug)]
pub struct LinuxSource {
    info: SourceInfo,
}

impl LinuxSource {
    fn validated(info: &SourceInfo) -> Result<Self, PlatformError> {
        if info.id.trim().is_empty() {
            return Err(PlatformError::InvalidSource { reason: "id vazio" });
        }
        match info.kind {
            SourceKind::Display | SourceKind::Window => Ok(Self { info: info.clone() }),
        }
    }
}

pub fn enumerate() -> Result<Vec<SourceInfo>, PlatformError> {
    let mut out = Vec::new();
    match outputs::list_displays() {
        Ok(list) => out.extend(list),
        Err(_) => {
            if std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some() {
                out.push(SourceInfo {
                    kind: SourceKind::Display,
                    id: PORTAL_DISPLAY_ID.into(),
                    name: "Escolher tela no diálogo do sistema".into(),
                    w: 0,
                    h: 0,
                });
            } else {
                return Err(PlatformError::Internal("sem sessão gráfica".into()));
            }
        }
    }
    out.push(SourceInfo {
        kind: SourceKind::Window,
        id: PORTAL_WINDOW_ID.into(),
        name: "Escolher janela no diálogo do sistema".into(),
        w: 0,
        h: 0,
    });
    Ok(out)
}

pub fn thumbnail(kind: SourceKind, id: &str) -> Result<BgraFrame, PlatformError> {
    thumbnail::thumbnail(kind, id.trim())
}

impl VideoSource for LinuxSource {
    fn enumerate() -> Result<Vec<SourceInfo>, PlatformError> {
        enumerate()
    }

    fn open(info: &SourceInfo) -> Result<Self, PlatformError> {
        let source = Self::validated(info)?;
        let list = enumerate()?;
        if !list.iter().any(|item| item.kind == info.kind && item.id == info.id.trim()) {
            return Err(PlatformError::SourceGone { id: info.id.clone() });
        }
        Ok(source)
    }

    fn start(&mut self, config: &CaptureConfig) -> Result<FrameStream, PlatformError> {
        let list = enumerate()?;
        if !list.iter().any(|item| item.kind == self.info.kind && item.id == self.info.id) {
            return Err(PlatformError::SourceGone { id: self.info.id.clone() });
        }
        let info = self.info.clone();
        let config = *config;
        let (ready_tx, ready_rx) = mpsc::channel();
        let (frame_tx, frame_rx) = mpsc::sync_channel::<CapturePacket>(CHANNEL_DEPTH);
        let error: Arc<Mutex<Option<PlatformError>>> = Arc::new(Mutex::new(None));
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stop_flag);
        let worker = std::thread::Builder::new()
            .name("golive-linux-cap".into())
            .spawn(move || {
                let cast = match portal::open_cast(info.kind, &info.id) {
                    Ok(cast) => cast,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                };
                let (fd, node_id, closer) = cast.into_parts();
                video::pump(fd, node_id, config, frame_tx, stop, ready_tx);
                drop(closer);
            })
            .map_err(|error| PlatformError::Internal(format!("thread de captura: {error}")))?;
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

    fn restart_order(_info: &SourceInfo) -> RestartOrder {
        RestartOrder::StopFirst
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_id_is_rejected_without_a_dialog() {
        let info = SourceInfo {
            kind: SourceKind::Display,
            id: "  ".into(),
            name: String::new(),
            w: 0,
            h: 0,
        };
        assert!(matches!(
            LinuxSource::open(&info),
            Err(PlatformError::InvalidSource { .. })
        ));
    }

    #[test]
    fn restart_is_stop_first() {
        let info = SourceInfo {
            kind: SourceKind::Display,
            id: "eDP-1".into(),
            name: String::new(),
            w: 1920,
            h: 1080,
        };
        assert_eq!(LinuxSource::restart_order(&info), RestartOrder::StopFirst);
    }

    #[test]
    fn wayland_list_does_not_open_the_portal() {
        if std::env::var_os("WAYLAND_DISPLAY").is_none() {
            return;
        }
        let list = outputs::list_displays().expect("lista wayland");
        assert!(!list.is_empty());
        assert!(list.iter().all(|item| item.kind == SourceKind::Display));
        assert!(list.iter().all(|item| item.id != PORTAL_DISPLAY_ID));
    }

    #[test]
    fn portal_window_thumbnail_does_not_touch_the_bus() {
        let error = thumbnail(SourceKind::Window, PORTAL_WINDOW_ID).unwrap_err();
        assert!(matches!(error, PlatformError::Internal(_)));
    }
}
