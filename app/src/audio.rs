//! Share-session system audio: one OS tap, Opus fanout to publishers,
//! optional viewer playback. OS selection happens only here.

use golive_platform::{AudioApp, EncodedAudioPacket};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

#[cfg(target_os = "macos")]
use golive_platform_macos::{list_audio_apps as os_list, start_audio_tap as os_start, AudioTap};
#[cfg(target_os = "windows")]
use golive_platform_windows::{list_audio_apps as os_list, start_audio_tap as os_start, AudioTap};

pub fn list_apps() -> Vec<AudioApp> {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        os_list()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Vec::new()
    }
}

pub struct ShareAudio {
    tap: Option<OsTap>,
    excluded: Vec<String>,
    hub_tx: SyncSender<EncodedAudioPacket>,
    subscribers: Arc<Mutex<Vec<SyncSender<EncodedAudioPacket>>>>,
    _hub: JoinHandle<()>,
}

struct OsTap {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    _inner: AudioTap,
}

impl ShareAudio {
    pub fn start(excluded: Vec<String>) -> Result<Self, String> {
        let (hub_tx, hub_rx) = mpsc::sync_channel::<EncodedAudioPacket>(16);
        let subscribers: Arc<Mutex<Vec<SyncSender<EncodedAudioPacket>>>> =
            Arc::new(Mutex::new(Vec::new()));
        let hub_subs = Arc::clone(&subscribers);
        let hub = std::thread::Builder::new()
            .name("golive-audio-hub".into())
            .spawn(move || audio_hub(hub_rx, hub_subs))
            .map_err(|error| error.to_string())?;
        let tap = match start_tap(&excluded, hub_tx.clone()) {
            Ok(tap) => tap,
            Err(error) => {
                drop(hub_tx);
                let _ = hub.join();
                return Err(error);
            }
        };
        Ok(Self {
            tap: Some(tap),
            excluded,
            hub_tx,
            subscribers,
            _hub: hub,
        })
    }

    pub fn start_for_window(window_id: &str) -> Result<Self, String> {
        let (hub_tx, hub_rx) = mpsc::sync_channel::<EncodedAudioPacket>(16);
        let subscribers: Arc<Mutex<Vec<SyncSender<EncodedAudioPacket>>>> =
            Arc::new(Mutex::new(Vec::new()));
        let hub_subs = Arc::clone(&subscribers);
        let hub = std::thread::Builder::new()
            .name("golive-audio-hub".into())
            .spawn(move || audio_hub(hub_rx, hub_subs))
            .map_err(|error| error.to_string())?;
        let tap = match start_window_tap(window_id, hub_tx.clone()) {
            Ok(tap) => tap,
            Err(error) => {
                drop(hub_tx);
                let _ = hub.join();
                return Err(error);
            }
        };
        Ok(Self {
            tap: Some(tap),
            excluded: Vec::new(),
            hub_tx,
            subscribers,
            _hub: hub,
        })
    }

    pub fn subscribe(&self) -> Receiver<EncodedAudioPacket> {
        let (tx, rx) = mpsc::sync_channel(16);
        if let Ok(mut list) = self.subscribers.lock() {
            list.push(tx);
        }
        rx
    }

    pub fn set_exclusions(&mut self, excluded: Vec<String>) -> Result<(), String> {
        self.tap = None;
        match start_tap(&excluded, self.hub_tx.clone()) {
            Ok(tap) => {
                self.tap = Some(tap);
                self.excluded = excluded;
                Ok(())
            }
            Err(error) => {
                self.excluded = excluded;
                Err(error)
            }
        }
    }

    pub fn excluded(&self) -> &[String] {
        &self.excluded
    }

    pub fn live(&self) -> bool {
        self.tap.is_some()
    }
}

fn start_window_tap(window_id: &str, tx: SyncSender<EncodedAudioPacket>) -> Result<OsTap, String> {
    #[cfg(target_os = "windows")]
    {
        if let Some(pid) = golive_platform_windows::window_pid(window_id) {
            return golive_platform_windows::start_audio_tap_include(pid, tx)
                .map(|inner| OsTap { _inner: inner })
                .map_err(|error| error.to_string());
        }
    }
    #[cfg(not(target_os = "windows"))]
    let _ = window_id;
    start_tap(&[], tx)
}

fn start_tap(
    excluded: &[String],
    tx: SyncSender<EncodedAudioPacket>,
) -> Result<OsTap, String> {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        os_start(excluded, tx)
            .map(|inner| OsTap { _inner: inner })
            .map_err(|error| error.to_string())
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = (excluded, tx);
        Err("áudio do sistema: apenas macOS/Windows".into())
    }
}

fn audio_hub(
    rx: Receiver<EncodedAudioPacket>,
    subscribers: Arc<Mutex<Vec<SyncSender<EncodedAudioPacket>>>>,
) {
    while let Ok(packet) = rx.recv() {
        let Ok(mut list) = subscribers.lock() else {
            break;
        };
        list.retain(|tx| match tx.try_send(packet.clone()) {
            Ok(()) | Err(TrySendError::Full(_)) => true,
            Err(TrySendError::Disconnected(_)) => false,
        });
    }
}

pub struct ViewerPlayback {
    stop: Arc<AtomicBool>,
    _thread: Option<JoinHandle<()>>,
}

impl ViewerPlayback {
    pub fn start() -> Option<Self> {
        let queue: Arc<Mutex<VecDeque<f32>>> = Arc::new(Mutex::new(VecDeque::with_capacity(48_000)));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker_queue = Arc::clone(&queue);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<bool>();
        let thread = std::thread::Builder::new()
            .name("golive-audio-play".into())
            .spawn(move || {
                use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
                let Some(device) = cpal::default_host().default_output_device() else {
                    let _ = ready_tx.send(false);
                    return;
                };
                let Ok(config) = device.default_output_config() else {
                    let _ = ready_tx.send(false);
                    return;
                };
                let sample_rate = config.sample_rate().0.max(1);
                let channels = config.channels() as usize;
                let q = Arc::clone(&worker_queue);
                let err_fn = |_err| {};
                let stream_config = config.config();
                let step = 48_000.0 / f64::from(sample_rate);
                let stream = match config.sample_format() {
                    cpal::SampleFormat::F32 => device.build_output_stream(
                        &stream_config,
                        {
                            let mut acc = 1.0 - step;
                            let mut hold = [0.0f32; 2];
                            move |data: &mut [f32], _| {
                                fill_output(data, channels, step, &mut acc, &mut hold, &q)
                            }
                        },
                        err_fn,
                        None,
                    ),
                    cpal::SampleFormat::I16 => device.build_output_stream(
                        &stream_config,
                        {
                            let mut acc = 1.0 - step;
                            let mut hold = [0.0f32; 2];
                            move |data: &mut [i16], _| {
                                let mut tmp = vec![0f32; data.len()];
                                fill_output(&mut tmp, channels, step, &mut acc, &mut hold, &q);
                                for (dst, src) in data.iter_mut().zip(tmp.iter()) {
                                    *dst = (src.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                                }
                            }
                        },
                        err_fn,
                        None,
                    ),
                    _ => {
                        let _ = ready_tx.send(false);
                        return;
                    }
                };
                let Ok(stream) = stream else {
                    let _ = ready_tx.send(false);
                    return;
                };
                if stream.play().is_err() {
                    let _ = ready_tx.send(false);
                    return;
                }
                let _ = ready_tx.send(true);
                while !worker_stop.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(50));
                }
                drop(stream);
            })
            .ok()?;
        if !ready_rx.recv_timeout(Duration::from_secs(2)).unwrap_or(false) {
            stop.store(true, Ordering::Release);
            let _ = thread.join();
            return None;
        }
        PLAYBACK_QUEUE.store(Some(queue));
        Some(Self {
            stop,
            _thread: Some(thread),
        })
    }

    pub fn push(samples: &[f32]) {
        if let Some(queue) = PLAYBACK_QUEUE.load() {
            if let Ok(mut buf) = queue.lock() {
                if buf.len() > 48_000 * 2 {
                    buf.clear();
                }
                buf.extend(samples.iter().copied());
            }
        }
    }
}

impl Drop for ViewerPlayback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        PLAYBACK_QUEUE.store(None);
        if let Some(thread) = self._thread.take() {
            let _ = thread.join();
        }
    }
}

struct QueueSlot(std::sync::Mutex<Option<Arc<Mutex<VecDeque<f32>>>>>);

impl QueueSlot {
    const fn new() -> Self {
        Self(std::sync::Mutex::new(None))
    }
    fn store(&self, value: Option<Arc<Mutex<VecDeque<f32>>>>) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = value;
        }
    }
    fn load(&self) -> Option<Arc<Mutex<VecDeque<f32>>>> {
        self.0.lock().ok().and_then(|slot| slot.clone())
    }
}

static PLAYBACK_QUEUE: QueueSlot = QueueSlot::new();

fn fill_output(
    data: &mut [f32],
    out_ch: usize,
    step: f64,
    acc: &mut f64,
    hold: &mut [f32; 2],
    queue: &Mutex<VecDeque<f32>>,
) {
    let Ok(mut buf) = queue.lock() else {
        data.fill(0.0);
        return;
    };
    for frame in data.chunks_mut(out_ch.max(1)) {
        *acc += step;
        while *acc >= 1.0 {
            *acc -= 1.0;
            if buf.len() >= 2 {
                hold[0] = buf.pop_front().unwrap_or(0.0);
                hold[1] = buf.pop_front().unwrap_or(0.0);
            } else {
                hold[0] = 0.0;
                hold[1] = 0.0;
            }
        }
        if out_ch == 1 {
            frame[0] = (hold[0] + hold[1]) * 0.5;
        } else {
            frame[0] = hold[0];
            frame[1] = hold[1];
            for sample in frame.iter_mut().skip(2) {
                *sample = 0.0;
            }
        }
    }
}

pub fn playback_callback() -> Arc<dyn Fn(&[f32]) + Send + Sync> {
    Arc::new(|samples: &[f32]| ViewerPlayback::push(samples))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_output_keeps_48k_stereo_1_to_1() {
        let queue = Mutex::new(VecDeque::from(vec![0.5, -0.5, 0.25, -0.25]));
        let mut acc = 0.0;
        let mut hold = [0.0f32; 2];
        let mut out = [0.0f32; 4];
        fill_output(&mut out, 2, 1.0, &mut acc, &mut hold, &queue);
        assert_eq!(out, [0.5, -0.5, 0.25, -0.25]);
        assert!(queue.lock().unwrap().is_empty());
    }

    #[test]
    fn fill_output_holds_sample_when_device_is_faster() {
        let queue = Mutex::new(VecDeque::from(vec![1.0, 0.0]));
        let mut acc = 0.5;
        let mut hold = [0.0f32; 2];
        let mut out = [0.0f32; 4];
        fill_output(&mut out, 2, 0.5, &mut acc, &mut hold, &queue);
        assert_eq!(out, [1.0, 0.0, 1.0, 0.0]);
    }
}
