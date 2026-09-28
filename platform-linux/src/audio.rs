//! PipeWire per-app capture. Copies each playback stream; nothing is moved
//! off the user's speakers.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::Cursor;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use golive_platform::{AudioApp, EncodedAudioPacket, PlatformError};
use pipewire as pw;
use pw::spa::pod::Pod;
use pw::stream::{StreamFlags, StreamListener, StreamRc};

use crate::mix::{
    declick, mix_ready, pace_next, select_node_ids, select_pid, trim_latency, AppNode, FRAME_SAMPLES,
};
use crate::video::init_pipewire;

pub struct AudioTap {
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for AudioTap {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub fn list_audio_apps() -> Vec<AudioApp> {
    let Ok(nodes) = snapshot_nodes() else {
        return Vec::new();
    };
    let mut apps: Vec<AudioApp> = Vec::new();
    for node in nodes {
        if let Some(existing) = apps.iter_mut().find(|app| app.pid == node.pid) {
            existing.emitting_audio = true;
            continue;
        }
        let id = std::path::Path::new(&node.binary)
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or(if node.node_name.is_empty() {
                node.binary.as_str()
            } else {
                node.node_name.as_str()
            })
            .to_owned();
        let name = if node.name.is_empty() { id.clone() } else { node.name.clone() };
        if name.is_empty() {
            continue;
        }
        apps.push(AudioApp {
            name,
            id,
            pid: node.pid,
            emitting_audio: true,
        });
    }
    apps.sort_by(|left, right| {
        left.name.to_ascii_lowercase().cmp(&right.name.to_ascii_lowercase())
    });
    apps
}

pub fn start_audio_tap(
    excluded_tokens: &[String],
    opus_tx: SyncSender<EncodedAudioPacket>,
) -> Result<AudioTap, PlatformError> {
    spawn_tap(excluded_tokens.to_vec(), None, opus_tx)
}

pub fn start_audio_tap_include(
    pid: u32,
    opus_tx: SyncSender<EncodedAudioPacket>,
) -> Result<AudioTap, PlatformError> {
    if pid == 0 {
        return Err(PlatformError::Internal("áudio do app indisponível".into()));
    }
    spawn_tap(Vec::new(), Some(pid as i32), opus_tx)
}

fn spawn_tap(
    tokens: Vec<String>,
    only_pid: Option<i32>,
    opus_tx: SyncSender<EncodedAudioPacket>,
) -> Result<AudioTap, PlatformError> {
    let (ready_tx, ready_rx) = mpsc::channel();
    let shutdown = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&shutdown);
    let thread = thread::Builder::new()
        .name("golive-audio-opus".into())
        .spawn(move || audio_thread(tokens, only_pid, opus_tx, worker_stop, ready_tx))
        .map_err(|error| PlatformError::Internal(error.to_string()))?;
    match ready_rx.recv_timeout(Duration::from_secs(4)) {
        Ok(Ok(())) => Ok(AudioTap { shutdown, thread: Some(thread) }),
        Ok(Err(error)) => {
            shutdown.store(true, Ordering::Release);
            let _ = thread.join();
            Err(error)
        }
        Err(_) => {
            shutdown.store(true, Ordering::Release);
            let _ = thread.join();
            Err(PlatformError::Internal("timeout ao abrir o áudio".into()))
        }
    }
}

struct PcmIn {
    pcm: Rc<RefCell<VecDeque<f32>>>,
    format: pw::spa::param::audio::AudioInfoRaw,
}

struct Live {
    node_id: u32,
    _listener: StreamListener<PcmIn>,
    stream: StreamRc,
    pcm: Rc<RefCell<VecDeque<f32>>>,
}

struct AudioState {
    nodes: Vec<AppNode>,
    lives: Vec<Live>,
    tokens: Vec<String>,
    only_pid: Option<i32>,
    self_pid: i32,
    encoder: opus::Encoder,
    opus_tx: SyncSender<EncodedAudioPacket>,
    next_emit: Instant,
    prev: [f32; 2],
}

fn audio_thread(
    tokens: Vec<String>,
    only_pid: Option<i32>,
    opus_tx: SyncSender<EncodedAudioPacket>,
    shutdown: Arc<AtomicBool>,
    ready_tx: mpsc::Sender<Result<(), PlatformError>>,
) {
    init_pipewire();
    let Ok(encoder) = opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Audio) else {
        let _ = ready_tx.send(Err(PlatformError::Internal("encoder opus falhou".into())));
        return;
    };
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
    let core = match context.connect_rc(None) {
        Ok(core) => core,
        Err(_) => {
            let _ = ready_tx.send(Err(PlatformError::Internal("PipeWire indisponível".into())));
            return;
        }
    };
    let registry = match core.get_registry_rc() {
        Ok(registry) => registry,
        Err(_) => {
            let _ = ready_tx.send(Err(PlatformError::Internal("PipeWire indisponível".into())));
            return;
        }
    };
    let state = Rc::new(RefCell::new(AudioState {
        nodes: Vec::new(),
        lives: Vec::new(),
        tokens,
        only_pid,
        self_pid: std::process::id() as i32,
        encoder,
        opus_tx,
        next_emit: Instant::now(),
        prev: [0.0; 2],
    }));
    let state_global = Rc::clone(&state);
    let state_remove = Rc::clone(&state);
    let pending = match core.sync(0) {
        Ok(pending) => pending,
        Err(_) => {
            let _ = ready_tx.send(Err(PlatformError::Internal("PipeWire indisponível".into())));
            return;
        }
    };
    let done = Rc::new(std::cell::Cell::new(false));
    let done_flag = Rc::clone(&done);
    let loop_done = mainloop.clone();
    let _core_listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == pw::core::PW_ID_CORE && seq == pending {
                done_flag.set(true);
                loop_done.quit();
            }
        })
        .register();
    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            if global.type_ != pw::types::ObjectType::Node {
                return;
            }
            let Some(props) = global.props else {
                return;
            };
            let Some(node) = app_node(global.id, props) else {
                return;
            };
            let mut state = state_global.borrow_mut();
            if let Some(existing) = state.nodes.iter_mut().find(|item| item.node_id == node.node_id) {
                *existing = node;
            } else {
                state.nodes.push(node);
            }
        })
        .global_remove(move |id| {
            let mut state = state_remove.borrow_mut();
            state.nodes.retain(|node| node.node_id != id);
        })
        .register();
    while !done.get() {
        mainloop.run();
    }
    let core_timer = core.clone();
    let state_timer = Rc::clone(&state);
    let loop_timer = mainloop.clone();
    let stop_timer = Arc::clone(&shutdown);
    let timer = mainloop.loop_().add_timer(move |_| {
        if stop_timer.load(Ordering::Acquire) {
            loop_timer.quit();
            return;
        }
        let mut state = state_timer.borrow_mut();
        state.rescan(&core_timer);
        if !state.mix_encode() {
            loop_timer.quit();
        }
    });
    let _ = timer.update_timer(Some(Duration::from_millis(20)), Some(Duration::from_millis(20)));
    let _ = ready_tx.send(Ok(()));
    mainloop.run();
}

impl AudioState {
    fn wanted(&self) -> Vec<u32> {
        if let Some(pid) = self.only_pid {
            select_pid(&self.nodes, pid)
        } else {
            select_node_ids(&self.nodes, &self.tokens, self.self_pid)
        }
    }

    fn rescan(&mut self, core: &pw::core::CoreRc) {
        let wanted = self.wanted();
        let mut kept = Vec::new();
        for live in self.lives.drain(..) {
            if wanted.contains(&live.node_id) {
                kept.push(live);
            } else {
                let _ = live.stream.disconnect();
            }
        }
        self.lives = kept;
        for node_id in wanted {
            if self.lives.iter().any(|live| live.node_id == node_id) {
                continue;
            }
            if let Some(live) = open_live(core, node_id) {
                self.lives.push(live);
            }
        }
    }

    fn mix_encode(&mut self) -> bool {
        let mut queues = Vec::with_capacity(self.lives.len());
        let mut force = false;
        for live in &self.lives {
            let mut pcm = live.pcm.borrow_mut();
            if trim_latency(&mut pcm) {
                force = true;
            }
            queues.push(std::mem::take(&mut *pcm));
        }
        let now = Instant::now();
        let keep_going = if now >= self.next_emit {
            if let Some(mut frame) = mix_ready(&mut queues) {
                declick(&mut self.prev, &mut frame, force);
                self.next_emit = pace_next(self.next_emit, now);
                encode_frame(&mut self.encoder, &frame, &self.opus_tx)
            } else {
                true
            }
        } else {
            true
        };
        for (live, mut queue) in self.lives.iter().zip(queues) {
            let mut pcm = live.pcm.borrow_mut();
            if pcm.is_empty() {
                *pcm = queue;
            } else {
                pcm.append(&mut queue);
            }
        }
        keep_going
    }
}

fn open_live(core: &pw::core::CoreRc, node_id: u32) -> Option<Live> {
    let mut props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Music",
    };
    props.insert("target.object", node_id.to_string());
    let stream = StreamRc::new(core.clone(), "golive-audio", props).ok()?;
    let pcm = Rc::new(RefCell::new(VecDeque::new()));
    let slot = PcmIn {
        pcm: Rc::clone(&pcm),
        format: pw::spa::param::audio::AudioInfoRaw::new(),
    };
    let listener = stream
        .add_local_listener_with_user_data(slot)
        .param_changed(|_, user, id, param| {
            let Some(param) = param else {
                return;
            };
            if id != pw::spa::param::ParamType::Format.as_raw() {
                return;
            }
            let _ = user.format.parse(param);
        })
        .process(|stream, user| {
            if user.format.rate() != 0 && user.format.format() != pw::spa::param::audio::AudioFormat::F32LE {
                return;
            }
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let data = &mut datas[0];
            let channels = user.format.channels().max(1) as usize;
            let start = data.chunk().offset() as usize;
            let size = data.chunk().size() as usize;
            let Some(bytes) = data.data() else {
                return;
            };
            let end = start.saturating_add(size).min(bytes.len());
            if start >= end {
                return;
            }
            let samples = &bytes[start..end];
            let mut pcm = user.pcm.borrow_mut();
            let mut index = 0;
            while index + 4 <= samples.len() {
                if channels == 1 {
                    let sample = f32::from_le_bytes(samples[index..index + 4].try_into().unwrap_or([0; 4]));
                    pcm.push_back(sample);
                    pcm.push_back(sample);
                    index += 4;
                } else {
                    if index + channels * 4 > samples.len() {
                        break;
                    }
                    let left = f32::from_le_bytes(samples[index..index + 4].try_into().unwrap_or([0; 4]));
                    let right = f32::from_le_bytes(samples[index + 4..index + 8].try_into().unwrap_or([0; 4]));
                    pcm.push_back(left);
                    pcm.push_back(right);
                    index += channels * 4;
                }
            }
            let _ = FRAME_SAMPLES;
        })
        .register()
        .ok()?;
    let bytes = audio_pod();
    let pod = Pod::from_bytes(&bytes)?;
    let mut params = [pod];
    stream
        .connect(
            pw::spa::utils::Direction::Input,
            Some(node_id),
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .ok()?;
    Some(Live { node_id, stream, _listener: listener, pcm })
}

fn audio_pod() -> Vec<u8> {
    let mut info = pw::spa::param::audio::AudioInfoRaw::new();
    info.set_format(pw::spa::param::audio::AudioFormat::F32LE);
    info.set_rate(48_000);
    info.set_channels(2);
    let mut position = [0; pw::spa::param::audio::MAX_CHANNELS];
    position[0] = pw::spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = pw::spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);
    let obj = pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pw::spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    pw::spa::pod::serialize::PodSerializer::serialize(Cursor::new(Vec::new()), &pw::spa::pod::Value::Object(obj))
        .map(|value| value.0.into_inner())
        .unwrap_or_default()
}

fn encode_frame(
    encoder: &mut opus::Encoder,
    frame: &[f32],
    opus_tx: &SyncSender<EncodedAudioPacket>,
) -> bool {
    let mut output = vec![0u8; 4000];
    match encoder.encode_float(frame, &mut output) {
        Ok(size) if size > 0 => {
            output.truncate(size);
            let packet = EncodedAudioPacket {
                data: output,
                duration: Duration::from_millis(20),
            };
            match opus_tx.try_send(packet) {
                Ok(()) | Err(TrySendError::Full(_)) => true,
                Err(TrySendError::Disconnected(_)) => false,
            }
        }
        _ => true,
    }
}

fn app_node(id: u32, props: &pw::spa::utils::dict::DictRef) -> Option<AppNode> {
    if props.get("media.class")? != "Stream/Output/Audio" {
        return None;
    }
    let pid: i32 = props.get("application.process.id")?.parse().ok()?;
    if pid <= 0 {
        return None;
    }
    let name = props.get("application.name").unwrap_or("").to_owned();
    let binary_raw = props.get("application.process.binary").unwrap_or("");
    let binary = std::path::Path::new(binary_raw)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(binary_raw)
        .to_owned();
    let node_name = props.get("node.name").unwrap_or("").to_owned();
    if name.is_empty() && binary.is_empty() && node_name.is_empty() {
        return None;
    }
    Some(AppNode { node_id: id, name, binary, node_name, pid })
}

fn snapshot_nodes() -> Result<Vec<AppNode>, PlatformError> {
    init_pipewire();
    let mainloop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|_| PlatformError::Internal("PipeWire não iniciou".into()))?;
    let context = pw::context::ContextRc::new(&mainloop, None)
        .map_err(|_| PlatformError::Internal("PipeWire não iniciou".into()))?;
    let core = context
        .connect_rc(None)
        .map_err(|_| PlatformError::Internal("PipeWire indisponível".into()))?;
    let registry = core
        .get_registry_rc()
        .map_err(|_| PlatformError::Internal("PipeWire indisponível".into()))?;
    let nodes = Rc::new(RefCell::new(Vec::new()));
    let nodes_global = Rc::clone(&nodes);
    let pending = core.sync(0).map_err(|_| PlatformError::Internal("PipeWire indisponível".into()))?;
    let done = Rc::new(std::cell::Cell::new(false));
    let done_flag = Rc::clone(&done);
    let loop_done = mainloop.clone();
    let _core_listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == pw::core::PW_ID_CORE && seq == pending {
                done_flag.set(true);
                loop_done.quit();
            }
        })
        .register();
    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            if global.type_ != pw::types::ObjectType::Node {
                return;
            }
            let Some(props) = global.props else {
                return;
            };
            if let Some(node) = app_node(global.id, props) {
                nodes_global.borrow_mut().push(node);
            }
        })
        .register();
    while !done.get() {
        mainloop.run();
    }
    let snapshot = nodes.borrow().clone();
    Ok(snapshot)
}
