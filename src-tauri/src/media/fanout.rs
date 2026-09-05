//! Copies encoded video/audio to every Viewer PeerTransport.

use super::access_unit::{AccessUnitQueue, AccessUnitReceiver, EncodedAccessUnit};
use super::pipeline::EncoderControl;
use super::process_tap::EncodedAudioPacket;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub(crate) const VIDEO_QUEUE_CAPACITY: usize = 16;
const AUDIO_QUEUE: usize = 16;

pub(crate) struct MediaFanout {
    video: Arc<Mutex<HashMap<String, AccessUnitQueue>>>,
    audio: Arc<Mutex<HashMap<String, SyncSender<EncodedAudioPacket>>>>,
    /// Coalesced encoder keyframe flag shared with the pipeline. Set by the
    /// session owner after `start`; when present, a new subscription
    /// (viewer join / reconnect) and any per-viewer queue overflow force an
    /// IDR, which the encoder consumes in its Video arm. PLI/FIR already
    /// arrive through this same flag from the transport layer.
    keyframe_control: Arc<Mutex<Option<Arc<EncoderControl>>>>,
    has_audio: bool,
    shutdown: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    worker_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShutdownStatus {
    pub(crate) quiesced: bool,
    pub(crate) pending: Vec<&'static str>,
    pub(crate) errors: Vec<String>,
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
        let keyframe_control = Arc::new(Mutex::new(None));
        let worker_keyframes = Arc::clone(&keyframe_control);
        let worker_result = thread::Builder::new()
            .name("godrinking-media-fanout".into())
            .spawn(move || {
                fanout_loop(
                    video,
                    audio,
                    worker_video,
                    worker_audio,
                    worker_shutdown,
                    worker_keyframes,
                )
            });
        let (worker, worker_error) = match worker_result {
            Ok(worker) => (Some(worker), None),
            Err(error) => (
                None,
                Some(format!("fanout worker failed to start: {error}")),
            ),
        };
        Self {
            video: video_subs,
            audio: audio_subs,
            keyframe_control,
            has_audio,
            shutdown,
            worker,
            worker_error,
        }
    }

    /// Attaches the pipeline's coalesced keyframe flag. Until set, the
    /// fanout still drops on overflow per queue (GOP integrity is preserved
    /// by each `AccessUnitQueue`); with the flag set, overflow and new
    /// subscriptions additionally force the next IDR.
    pub(crate) fn set_keyframe_control(&self, control: Arc<EncoderControl>) {
        if let Ok(mut slot) = self.keyframe_control.lock() {
            *slot = Some(control);
        }
    }

    fn request_keyframe(&self) {
        if let Ok(slot) = self.keyframe_control.lock() {
            if let Some(control) = slot.as_ref() {
                control.request_keyframe();
            }
        }
    }

    pub(crate) fn subscribe(
        &self,
        id: &str,
    ) -> (AccessUnitReceiver, Option<Receiver<EncodedAudioPacket>>) {
        let (video_tx, video_rx) = AccessUnitQueue::bounded(VIDEO_QUEUE_CAPACITY);
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
        // A (re)joining viewer needs SPS/PPS + IDR immediately: on static
        // screens the encoder emits mostly SKIP frames, so without a forced
        // keyframe the viewer would wait up to the intra period. Coalesced
        // with any in-flight request; a no-op until `set_keyframe_control`.
        self.request_keyframe();
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

    /// Stops the fanout loop and joins it within `timeout`. A timeout leaves
    /// the JoinHandle owned by this object for a later retry.
    pub(crate) fn shutdown_and_join(&mut self, timeout: Duration) -> ShutdownStatus {
        self.shutdown.store(true, Ordering::Release);
        let mut status = ShutdownStatus {
            quiesced: self.worker_error.is_none(),
            pending: Vec::new(),
            errors: self.worker_error.clone().into_iter().collect(),
        };
        let Some(worker) = self.worker.as_ref() else {
            return status;
        };
        let deadline = std::time::Instant::now() + timeout;
        while !worker.is_finished() {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                status.quiesced = false;
                status.pending.push("fanout worker");
                return status;
            }
            thread::sleep(remaining.min(Duration::from_millis(2)));
        }
        let worker = self
            .worker
            .take()
            .expect("fanout worker handle still present");
        if worker.join().is_err() {
            status.quiesced = false;
            status.errors.push("fanout worker panicked".into());
        }
        status
    }
}

impl Drop for MediaFanout {
    fn drop(&mut self) {
        let status = self.shutdown_and_join(Duration::from_secs(3));
        if !status.quiesced {
            eprintln!("[goDrinking] fanout cleanup incomplete: {status:?}");
        }
    }
}

fn fanout_loop(
    video: AccessUnitReceiver,
    audio: Option<Receiver<EncodedAudioPacket>>,
    video_subs: Arc<Mutex<HashMap<String, AccessUnitQueue>>>,
    audio_subs: Arc<Mutex<HashMap<String, SyncSender<EncodedAudioPacket>>>>,
    shutdown: Arc<AtomicBool>,
    keyframe_control: Arc<Mutex<Option<Arc<EncoderControl>>>>,
) {
    loop {
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        if let Some(unit) = video.recv_timeout(Duration::from_millis(10)) {
            push_video(&video_subs, &keyframe_control, &unit);
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

fn push_video(
    subs: &Mutex<HashMap<String, AccessUnitQueue>>,
    keyframe_control: &Mutex<Option<Arc<EncoderControl>>>,
    unit: &EncodedAccessUnit,
) {
    use super::access_unit::AccessUnitPushResult;
    let Ok(subs) = subs.lock() else {
        return;
    };
    // try_send full => the per-viewer queue drops its partial GOP and waits
    // for the next IDR; force one so the affected link recovers instead of
    // stalling until the next periodic intra. One viewer overflowing never
    // affects other viewers or the host capture.
    let mut overflowed = false;
    for queue in subs.values() {
        if queue.try_push(unit.clone()) == AccessUnitPushResult::DroppedUntilKeyframe {
            overflowed = true;
        }
    }
    if overflowed {
        if let Ok(slot) = keyframe_control.lock() {
            if let Some(control) = slot.as_ref() {
                control.request_keyframe();
            }
        }
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

#[cfg(test)]
mod tests {
    use super::{MediaFanout, VIDEO_QUEUE_CAPACITY};
    use crate::media::access_unit::{AccessUnitQueue, EncodedAccessUnit};
    use crate::media::pipeline::EncoderControl;
    use std::sync::Arc;
    use std::time::Duration;

    fn unit(sequence: u8, keyframe: bool) -> EncodedAccessUnit {
        EncodedAccessUnit {
            data: vec![sequence],
            timestamp_90khz: u64::from(sequence) * 3000,
            keyframe,
            profile_level_id: None,
        }
    }

    #[test]
    fn per_viewer_queue_stays_at_sixteen() {
        assert_eq!(VIDEO_QUEUE_CAPACITY, 16);
    }

    #[test]
    fn overflow_forces_an_idr_without_touching_other_viewers() {
        use super::push_video;
        use std::collections::HashMap;
        use std::sync::Mutex;
        // One full (16) and one healthy per-viewer queue behind the same map.
        let subs = Mutex::new(HashMap::new());
        let (full_tx, full_rx) = AccessUnitQueue::bounded(VIDEO_QUEUE_CAPACITY);
        for sequence in 0..VIDEO_QUEUE_CAPACITY {
            assert_eq!(
                full_tx.try_push(unit(sequence as u8, false)),
                crate::media::access_unit::AccessUnitPushResult::Enqueued
            );
        }
        let (healthy_tx, healthy_rx) = AccessUnitQueue::bounded(VIDEO_QUEUE_CAPACITY);
        subs.lock()
            .expect("subs")
            .insert("slow".to_owned(), full_tx);
        subs.lock()
            .expect("subs")
            .insert("fast".to_owned(), healthy_tx);
        let control = EncoderControl::new(8_000_000, 2_000_000);
        let keyframes = Mutex::new(Some(Arc::clone(&control)));
        // try_send full => drop the partial GOP and force an IDR for the
        // affected link; the healthy viewer keeps every unit.
        push_video(&subs, &keyframes, &unit(200, false));
        assert!(
            control.take_keyframe_for_test(),
            "overflow must force an IDR"
        );
        // The healthy queue still delivers the unit; the overflowed one waits
        // for the next keyframe.
        assert_eq!(
            healthy_rx.recv_timeout(Duration::from_millis(10)),
            Some(unit(200, false))
        );
        // The IDR heals the overflowed queue: no new flag is raised, and the
        // slow viewer receives decodable data again.
        push_video(&subs, &keyframes, &unit(201, true));
        assert!(!control.take_keyframe_for_test());
        assert_eq!(
            full_rx.recv_timeout(Duration::from_millis(10)),
            Some(unit(201, true))
        );
        // Steady state raises no flags.
        push_video(&subs, &keyframes, &unit(202, false));
        assert!(!control.take_keyframe_for_test());
    }

    #[test]
    fn subscribe_requests_a_keyframe_for_the_joining_viewer() {
        let (_tx, rx) = AccessUnitQueue::bounded(8);
        let mut fanout = MediaFanout::start(rx, None);
        let control = EncoderControl::new(8_000_000, 2_000_000);
        // No flag before the control is attached.
        let (_rx, _audio) = fanout.subscribe("early");
        fanout.set_keyframe_control(Arc::clone(&control));
        // (Re)join forces an IDR so the newcomer gets SPS/PPS immediately.
        let (_rx, _audio) = fanout.subscribe("late");
        assert!(control.take_keyframe_for_test());
        let status = fanout.shutdown_and_join(Duration::from_secs(1));
        assert!(status.quiesced, "fanout shutdown status: {status:?}");
    }

    #[test]
    fn shutdown_and_join_proves_fanout_worker_quiescence() {
        let (_tx, rx) = AccessUnitQueue::bounded(1);
        let mut fanout = MediaFanout::start(rx, None);
        let status = fanout.shutdown_and_join(Duration::from_secs(1));
        assert!(status.quiesced, "fanout shutdown status: {status:?}");
        assert!(status.pending.is_empty());
        assert!(status.errors.is_empty());
    }
}
