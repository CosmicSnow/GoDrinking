//! Copies encoded video/audio to every Viewer PeerTransport.

use super::access_unit::{AccessUnitQueue, AccessUnitReceiver, EncodedAccessUnit};
use super::process_tap::EncodedAudioPacket;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const VIDEO_QUEUE: usize = 16;
const AUDIO_QUEUE: usize = 16;

pub(crate) struct MediaFanout {
    video: Arc<Mutex<HashMap<String, AccessUnitQueue>>>,
    audio: Arc<Mutex<HashMap<String, SyncSender<EncodedAudioPacket>>>>,
    has_audio: bool,
    shutdown: Arc<AtomicBool>,
    _worker: Option<JoinHandle<()>>,
}

impl MediaFanout {
    pub(crate) fn start(
        video: AccessUnitReceiver,
        audio: Option<Receiver<EncodedAudioPacket>>,
    ) -> Self {
        let video_subs = Arc::new(Mutex::new(HashMap::new()));
        let audio_subs = Arc::new(Mutex::new(HashMap::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let has_audio = audio.is_some();
        let worker_video = Arc::clone(&video_subs);
        let worker_audio = Arc::clone(&audio_subs);
        let worker_shutdown = Arc::clone(&shutdown);
        let worker = thread::Builder::new()
            .name("godrinking-media-fanout".into())
            .spawn(move || fanout_loop(video, audio, worker_video, worker_audio, worker_shutdown))
            .ok();
        Self {
            video: video_subs,
            audio: audio_subs,
            has_audio,
            shutdown,
            _worker: worker,
        }
    }

    pub(crate) fn subscribe(
        &self,
        id: &str,
    ) -> (AccessUnitReceiver, Option<Receiver<EncodedAudioPacket>>) {
        let (video_tx, video_rx) = AccessUnitQueue::bounded(VIDEO_QUEUE);
        if let Ok(mut video) = self.video.lock() {
            video.insert(id.to_owned(), video_tx);
        }
        let audio_rx = if self.has_audio {
            let (tx, rx) = sync_channel(AUDIO_QUEUE);
            if let Ok(mut audio) = self.audio.lock() {
                audio.insert(id.to_owned(), tx);
            }
            Some(rx)
        } else {
            None
        };
        (video_rx, audio_rx)
    }

    pub(crate) fn unsubscribe(&self, id: &str) {
        if let Ok(mut video) = self.video.lock() {
            video.remove(id);
        }
        if let Ok(mut audio) = self.audio.lock() {
            audio.remove(id);
        }
    }
}

impl Drop for MediaFanout {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
    }
}

fn fanout_loop(
    video: AccessUnitReceiver,
    audio: Option<Receiver<EncodedAudioPacket>>,
    video_subs: Arc<Mutex<HashMap<String, AccessUnitQueue>>>,
    audio_subs: Arc<Mutex<HashMap<String, SyncSender<EncodedAudioPacket>>>>,
    shutdown: Arc<AtomicBool>,
) {
    loop {
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        if let Some(unit) = video.recv_timeout(Duration::from_millis(10)) {
            push_video(&video_subs, &unit);
        } else if video.is_closed() {
            break;
        }
        if let Some(audio) = audio.as_ref() {
            match audio.recv_timeout(Duration::from_millis(0)) {
                Ok(packet) => push_audio(&audio_subs, &packet),
                Err(RecvTimeoutError::Disconnected) => {}
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }
}

fn push_video(subs: &Mutex<HashMap<String, AccessUnitQueue>>, unit: &EncodedAccessUnit) {
    let Ok(subs) = subs.lock() else {
        return;
    };
    for queue in subs.values() {
        let _ = queue.try_push(unit.clone());
    }
}

fn push_audio(
    subs: &Mutex<HashMap<String, SyncSender<EncodedAudioPacket>>>,
    packet: &EncodedAudioPacket,
) {
    let Ok(subs) = subs.lock() else {
        return;
    };
    for tx in subs.values() {
        let _ = tx.try_send(packet.clone());
    }
}
