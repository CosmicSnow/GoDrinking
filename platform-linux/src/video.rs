//! Portal PipeWire fd → latest-only BGRA frames.

use std::io::Cursor;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::Arc;
use std::time::Duration;

use golive_platform::{CaptureConfig, CapturePacket, PlatformError};
use pipewire as pw;
use pw::spa::pod::Pod;
use pw::stream::StreamFlags;

use crate::pixels::{bgra_from_packed, PackedKind};

struct VideoData {
    format: pw::spa::param::video::VideoInfoRaw,
    tx: SyncSender<CapturePacket>,
    ready: std::rc::Rc<std::cell::RefCell<Option<mpsc::Sender<Result<(), PlatformError>>>>>,
}

pub fn pump(
    fd: OwnedFd,
    node_id: u32,
    config: CaptureConfig,
    frame_tx: SyncSender<CapturePacket>,
    stop: Arc<AtomicBool>,
    ready_tx: mpsc::Sender<Result<(), PlatformError>>,
) {
    init_pipewire();
    let mainloop = match pw::main_loop::MainLoopRc::new(None) {
        Ok(loop_) => loop_,
        Err(_) => {
            let _ = ready_tx.send(Err(PlatformError::Internal("PipeWire não iniciou".into())));
            return;
        }
    };
    let context = match pw::context::ContextRc::new(&mainloop, None) {
        Ok(context) => context,
        Err(_) => {
            let _ = ready_tx.send(Err(PlatformError::Internal("PipeWire não iniciou".into())));
            return;
        }
    };
    let core = match context.connect_fd_rc(fd, None) {
        Ok(core) => core,
        Err(_) => {
            let _ = ready_tx.send(Err(PlatformError::Internal("PipeWire remoto falhou".into())));
            return;
        }
    };
    let ready = std::rc::Rc::new(std::cell::RefCell::new(Some(ready_tx)));
    let data = VideoData {
        format: pw::spa::param::video::VideoInfoRaw::new(),
        tx: frame_tx,
        ready: std::rc::Rc::clone(&ready),
    };
    let stream = match pw::stream::StreamBox::new(
        &core,
        "golive-screencast",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    ) {
        Ok(stream) => stream,
        Err(_) => {
            signal(&ready, Err(PlatformError::Internal("stream de vídeo falhou".into())));
            return;
        }
    };
    let _listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(|_, user, _old, new| {
            if matches!(new, pw::stream::StreamState::Error(_)) {
                signal(&user.ready, Err(PlatformError::Internal("stream de vídeo falhou".into())));
            } else if new == pw::stream::StreamState::Streaming {
                signal(&user.ready, Ok(()));
            }
        })
        .param_changed(|_, user, id, param| {
            let Some(param) = param else {
                return;
            };
            if id != pw::spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((media_type, media_subtype)) = pw::spa::param::format_utils::parse_format(param) else {
                return;
            };
            if media_type != pw::spa::param::format::MediaType::Video
                || media_subtype != pw::spa::param::format::MediaSubtype::Raw
            {
                return;
            }
            let _ = user.format.parse(param);
        })
        .process(|stream, user| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let data = &mut datas[0];
            if data.type_() == pw::spa::buffer::DataType::DmaBuf {
                return;
            }
            let Some(kind) = packed_kind(user.format.format()) else {
                return;
            };
            let size = user.format.size();
            let offset = data.chunk().offset() as usize;
            let chunk_size = data.chunk().size() as usize;
            let stride = data.chunk().stride();
            let Some(bytes) = data.data() else {
                return;
            };
            let Some(frame) = bgra_from_packed(
                bytes,
                offset,
                chunk_size,
                stride,
                size.width,
                size.height,
                kind,
            ) else {
                return;
            };
            let _ = user.tx.try_send(CapturePacket::Cpu(frame));
        })
        .register();
    let Some(_listener) = _listener.ok() else {
        signal(&ready, Err(PlatformError::Internal("stream de vídeo falhou".into())));
        return;
    };
    let values = video_pod(config.fps.max(1).min(60));
    let Some(pod) = Pod::from_bytes(&values) else {
        signal(&ready, Err(PlatformError::Internal("formato de vídeo inválido".into())));
        return;
    };
    let mut params = [pod];
    if stream
        .connect(pw::spa::utils::Direction::Input, Some(node_id), StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS, &mut params)
        .is_err()
    {
        signal(&ready, Err(PlatformError::Internal("não conectou o stream de vídeo".into())));
        return;
    }
    let loop_stop = mainloop.clone();
    let ready_stop = std::rc::Rc::clone(&ready);
    let stop_flag = Arc::clone(&stop);
    let timer = mainloop.loop_().add_timer(move |_| {
        if stop_flag.load(Ordering::Acquire) {
            signal(&ready_stop, Err(PlatformError::Internal("captura interrompida".into())));
            loop_stop.quit();
        }
    });
    let _ = timer.update_timer(Some(Duration::from_millis(200)), Some(Duration::from_millis(200)));
    mainloop.run();
    let _ = stream.disconnect();
}

fn signal(
    slot: &std::rc::Rc<std::cell::RefCell<Option<mpsc::Sender<Result<(), PlatformError>>>>>,
    result: Result<(), PlatformError>,
) {
    if let Some(tx) = slot.borrow_mut().take() {
        let _ = tx.send(result);
    }
}

fn packed_kind(format: pw::spa::param::video::VideoFormat) -> Option<PackedKind> {
    if format == pw::spa::param::video::VideoFormat::BGRx {
        Some(PackedKind::Bgrx)
    } else if format == pw::spa::param::video::VideoFormat::BGRA {
        Some(PackedKind::Bgra)
    } else if format == pw::spa::param::video::VideoFormat::RGBx {
        Some(PackedKind::Rgbx)
    } else if format == pw::spa::param::video::VideoFormat::RGBA {
        Some(PackedKind::Rgba)
    } else if format == pw::spa::param::video::VideoFormat::RGB {
        Some(PackedKind::Rgb)
    } else if format == pw::spa::param::video::VideoFormat::BGR {
        Some(PackedKind::Bgr)
    } else {
        None
    }
}

fn video_pod(fps: u32) -> Vec<u8> {
    let obj = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaType,
            Id,
            pw::spa::param::format::MediaType::Video
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaSubtype,
            Id,
            pw::spa::param::format::MediaSubtype::Raw
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            pw::spa::param::video::VideoFormat::BGRx,
            pw::spa::param::video::VideoFormat::BGRx,
            pw::spa::param::video::VideoFormat::BGRA,
            pw::spa::param::video::VideoFormat::RGBx,
            pw::spa::param::video::VideoFormat::RGBA
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            pw::spa::utils::Rectangle { width: 1920, height: 1080 },
            pw::spa::utils::Rectangle { width: 1, height: 1 },
            pw::spa::utils::Rectangle { width: 8192, height: 8192 }
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            pw::spa::utils::Fraction { num: fps, denom: 1 },
            pw::spa::utils::Fraction { num: 0, denom: 1 },
            pw::spa::utils::Fraction { num: 60, denom: 1 }
        ),
    );
    pw::spa::pod::serialize::PodSerializer::serialize(Cursor::new(Vec::new()), &pw::spa::pod::Value::Object(obj))
        .map(|value| value.0.into_inner())
        .unwrap_or_default()
}

pub(crate) fn init_pipewire() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        pw::init();
    });
}
