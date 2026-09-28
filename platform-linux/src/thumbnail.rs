//! One-shot still via KWin ScreenShot2 when the compositor allows it.
//! A normal app is usually refused; the share modal keeps its gradient.

use std::collections::HashMap;
use std::io::Read;
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, Ordering};

use golive_platform::{BgraFrame, PlatformError, SourceKind};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{Fd, OwnedValue, Value};

use crate::pixels::{bgra_from_packed, PackedKind};

static SCREENSHOT_DENIED: AtomicBool = AtomicBool::new(false);

pub fn thumbnail(kind: SourceKind, id: &str) -> Result<BgraFrame, PlatformError> {
    if kind != SourceKind::Display || id.is_empty() || id == crate::PORTAL_DISPLAY_ID || id == crate::PORTAL_WINDOW_ID {
        return Err(PlatformError::Internal("miniatura indisponível".into()));
    }
    if SCREENSHOT_DENIED.load(Ordering::Acquire) {
        return Err(PlatformError::Internal("miniatura indisponível neste compositor".into()));
    }
    let connection = Connection::session().map_err(|_| {
        PlatformError::Internal("miniatura indisponível".into())
    })?;
    let proxy = Proxy::new(
        &connection,
        "org.kde.KWin.ScreenShot2",
        "/org/kde/KWin/ScreenShot2",
        "org.kde.KWin.ScreenShot2",
    )
    .map_err(|_| PlatformError::Internal("miniatura indisponível".into()))?;
    let (mut reader, writer) = std::io::pipe().map_err(|_| {
        PlatformError::Internal("miniatura indisponível".into())
    })?;
    let mut options: HashMap<&str, Value<'_>> = HashMap::new();
    options.insert("native-resolution", Value::Bool(true));
    let fd = Fd::from(writer.as_fd());
    let reply = proxy.call_method("CaptureScreen", &(id, &options, &fd));
    drop(writer);
    let message = match reply {
        Ok(message) => message,
        Err(error) => {
            let text = error.to_string();
            if text.contains("NoAuthorized") || text.contains("NotAuthorized") || text.contains("authorized") {
                SCREENSHOT_DENIED.store(true, Ordering::Release);
            }
            return Err(PlatformError::Internal("miniatura indisponível neste compositor".into()));
        }
    };
    let results: HashMap<String, OwnedValue> = message
        .body()
        .deserialize()
        .map_err(|_| PlatformError::Internal("miniatura indisponível".into()))?;
    let width = dict_u32(&results, "width").unwrap_or(0);
    let height = dict_u32(&results, "height").unwrap_or(0);
    let stride = dict_u32(&results, "stride").unwrap_or(0) as i32;
    let format = dict_u32(&results, "format").unwrap_or(0);
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).map_err(|_| {
        PlatformError::Internal("miniatura indisponível".into())
    })?;
    let kind = match format {
        4 | 5 | 6 => PackedKind::Bgra,
        _ if stride > 0 && width > 0 && stride as u32 >= width * 4 => PackedKind::Bgra,
        _ => return Err(PlatformError::Internal("miniatura em formato desconhecido".into())),
    };
    bgra_from_packed(&bytes, 0, bytes.len(), stride, width, height, kind)
        .ok_or_else(|| PlatformError::Internal("miniatura vazia".into()))
}

fn dict_u32(map: &HashMap<String, OwnedValue>, key: &str) -> Option<u32> {
    let value = map.get(key)?;
    if let Ok(number) = u32::try_from(value) {
        return Some(number);
    }
    i32::try_from(value).ok().filter(|number| *number > 0).map(|number| number as u32)
}

