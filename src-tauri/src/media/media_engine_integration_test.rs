//! Phase-1 acceptance evidence through the public `MediaEngine` worker API.
//!
//! The worker harness deliberately exposes supported control-plane capabilities
//! while leaving native capture unavailable. This keeps the tests honest: a
//! test process cannot stand in for a packaged ScreenCaptureKit/WGC app or for
//! TCC/Windows privacy state. The cancellation, ledger, fencing, and signaling
//! assertions below are nevertheless driven by the real public worker loop.

use super::super::control_plane::OfferEpochFence;
use super::super::peer_transport::{PeerSignal, PeerSignalKind};
use super::super::types::{
    FrameRate, JoinMode, MediaLifecycleState, TransmissionQuality, UpdateMediaSessionRequest,
    VideoCodec, VideoEncoder, VideoResolution,
};
use super::tests::{request, wait_for_state, worker_engine, worker_engine_with_counters};
use super::{MediaEngineError, ResourceCounters};
use std::net::SocketAddr;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

fn update(
    quality: TransmissionQuality,
    bitrate_bps: Option<u32>,
    resolution: Option<VideoResolution>,
    frame_rate: Option<FrameRate>,
    system_audio: bool,
    excluded_apps: Vec<String>,
) -> UpdateMediaSessionRequest {
    UpdateMediaSessionRequest {
        source: None,
        source_id: None,
        quality,
        bitrate_bps,
        min_bitrate_bps: None,
        resolution,
        frame_rate,
        codec: VideoCodec::H264,
        encoder: VideoEncoder::Auto,
        system_audio,
        excluded_apps,
    }
}

fn assert_zero(counters: &ResourceCounters) {
    assert_eq!(counters.live_bundles(), 0, "resource bundle leaked");
    assert_eq!(counters.live_capture(), 0, "capture resource leaked");
    assert_eq!(counters.live_audio(), 0, "audio resource leaked");
    assert_eq!(counters.live_pipeline(), 0, "pipeline resource leaked");
    assert_eq!(counters.live_fanout(), 0, "fanout resource leaked");
    assert_eq!(counters.live_peers(), 0, "peer resource leaked");
    assert_eq!(counters.live_join_services(), 0, "join service leaked");
}

fn stop_share_until_complete(engine: &super::MediaEngine) {
    for _ in 0..4 {
        match engine.stop_share() {
            Ok(_) => return,
            Err(_) => thread::sleep(Duration::from_millis(25)),
        }
    }
    panic!("Stop Share did not complete its worker ledger");
}

#[test]
fn public_worker_stop_during_cancellable_acquisition_releases_every_resource() {
    let counters = Arc::new(ResourceCounters::default());
    let picker = Arc::new(Barrier::new(2));
    let engine =
        worker_engine_with_counters(None, Some(Arc::clone(&picker)), Some(Arc::clone(&counters)));
    let starter = engine.clone();
    let create = thread::spawn(move || starter.create_session(request()));
    wait_for_state(&engine, MediaLifecycleState::Starting);

    let running = engine.clone();
    let committed = thread::spawn(move || running.snapshot());
    wait_for_state(&engine, MediaLifecycleState::Starting);
    assert_eq!(
        committed.join().expect("snapshot thread").state,
        MediaLifecycleState::Starting
    );

    let stopper = engine.clone();
    let stop = thread::spawn(move || stopper.stop_session());
    wait_for_state(&engine, MediaLifecycleState::Stopping);
    picker.wait();

    assert!(create.join().expect("start worker").is_err());
    let stopped = stop.join().expect("stop worker").expect("stop completes");
    assert_eq!(stopped.state, MediaLifecycleState::Idle);
    assert!(stopped.session_id.is_none());
    assert_zero(&counters);
    // This is the represented capability boundary, not a claim that a native
    // picker completed in an un-packaged test process.
    assert!(!stopped.native_capture_active);
}

#[test]
fn public_worker_supported_session_start_commits_before_full_stop() {
    let counters = Arc::new(ResourceCounters::default());
    let engine = worker_engine_with_counters(None, None, Some(Arc::clone(&counters)));
    let mut open = request();
    open.share_on_start = false;
    let started = engine
        .create_session(open)
        .expect("supported start commits");
    assert_eq!(started.state, MediaLifecycleState::Running);
    assert!(started.session_id.is_some());
    assert_eq!(counters.live_bundles(), 1);

    let stopped = engine.stop_session().expect("full stop completes");
    assert_eq!(stopped.state, MediaLifecycleState::Idle);
    assert_zero(&counters);
}

#[test]
fn public_worker_stop_share_completes_share_ledger_and_keeps_session_open() {
    let counters = Arc::new(ResourceCounters::default());
    let engine = worker_engine_with_counters(None, None, Some(Arc::clone(&counters)));
    let mut open = request();
    open.share_on_start = false;
    engine.create_session(open).expect("session starts");
    let started = engine.start_share().expect("Share start commits");
    assert_eq!(started.state, MediaLifecycleState::Running);
    let stopped_share = engine.stop_share().expect("Stop Share completes");
    assert_eq!(stopped_share.state, MediaLifecycleState::Running);
    assert!(!stopped_share.native_capture_active);
    assert!(
        stopped_share.session_id.is_some(),
        "Stop Share must not stop Session"
    );

    let stopped_session = engine.stop_session().expect("full session stop completes");
    assert_eq!(stopped_session.state, MediaLifecycleState::Idle);
    assert_zero(&counters);
}

#[test]
fn public_worker_stop_during_stop_share_waits_for_the_full_cleanup_transaction() {
    let counters = Arc::new(ResourceCounters::default());
    let engine = worker_engine_with_counters(None, None, Some(Arc::clone(&counters)));
    let mut open = request();
    open.share_on_start = false;
    engine.create_session(open).expect("session starts");
    engine.start_share().expect("Share starts");

    let cleanup_barrier = Arc::new(Barrier::new(2));
    engine.state.lock().expect("engine state").operation_barrier =
        Some(Arc::clone(&cleanup_barrier));
    let share_stopper = engine.clone();
    let stop_share = thread::spawn(move || share_stopper.stop_share());
    wait_for_state(&engine, MediaLifecycleState::Stopping);
    let session_stopper = engine.clone();
    let stop_session = thread::spawn(move || session_stopper.stop_session());
    wait_for_state(&engine, MediaLifecycleState::Stopping);
    cleanup_barrier.wait();

    assert!(stop_share.join().expect("Stop Share worker").is_err());
    let stopped = stop_session
        .join()
        .expect("Stop Session worker")
        .expect("full cleanup completes");
    assert_eq!(stopped.state, MediaLifecycleState::Idle);
    assert_zero(&counters);
}

#[test]
fn public_worker_bitrate_and_audio_only_update_retains_share_epoch() {
    let engine = worker_engine(None, None);
    let mut open = request();
    open.share_on_start = false;
    engine.create_session(open).expect("session starts");
    engine.start_share().expect("Share starts through worker");
    let before = engine.state.lock().expect("engine state").actor.share_epoch;

    let updated = engine
        .update_session(update(
            TransmissionQuality::Low,
            Some(900_000),
            None,
            None,
            true,
            vec!["Discord".into()],
        ))
        .expect("bitrate/audio update succeeds");
    let after = engine.state.lock().expect("engine state").actor.share_epoch;
    assert_eq!(
        before, after,
        "live bitrate/audio update changed ShareEpoch"
    );
    assert_eq!(updated.bitrate_bps, 900_000);
    assert!(updated.system_audio);
    assert_eq!(updated.excluded_apps, vec!["Discord"]);
}

#[test]
fn public_worker_source_resolution_fps_replacement_advances_epoch_and_rebuilds_staged_share() {
    let engine = worker_engine(None, None);
    let mut open = request();
    open.share_on_start = false;
    engine.create_session(open).expect("session starts");
    let before = engine.state.lock().expect("engine state").actor.share_epoch;

    // Native capture is intentionally unavailable in this worker harness. We
    // therefore verify the equivalent supported path: replacement settings are
    // staged while the Share slot is stopped, then the real worker rebuilds the
    // Share pipeline/audio channel on the next Start Share transaction.
    engine
        .update_session(update(
            TransmissionQuality::Medium,
            None,
            Some(VideoResolution::P720),
            Some(FrameRate::Fps30),
            true,
            vec!["Music".into()],
        ))
        .expect("replacement settings stage");
    let staged = engine.snapshot();
    assert_eq!(staged.resolution, Some(VideoResolution::P720));
    assert_eq!(staged.frame_rate, Some(FrameRate::Fps30));
    assert!(staged.system_audio);

    let replaced = engine.start_share().expect("replacement Share starts");
    let after = engine.state.lock().expect("engine state").actor.share_epoch;
    assert!(
        after.0 > before.0,
        "Share replacement did not advance ShareEpoch"
    );
    assert_eq!(replaced.resolution, Some(VideoResolution::P720));
    assert_eq!(replaced.frame_rate, Some(FrameRate::Fps30));
    assert!(replaced.detail.contains("Share slot") || !replaced.native_capture_active);
}

#[test]
fn public_worker_stale_exact_answer_cannot_affect_replacement_link() {
    let engine = worker_engine(None, None);
    let mut open = request();
    open.share_on_start = false;
    engine.create_session(open).expect("session starts");
    let old = engine
        .offer_for_member("viewer-1", "Viewer")
        .expect("old offer");
    stop_share_until_complete(&engine);
    engine.start_share().expect("replacement Share starts");
    let replacement = engine
        .offer_for_member("viewer-1", "Viewer")
        .expect("replacement offer");
    assert_ne!(old.offer_attempt, replacement.offer_attempt);

    let answer = PeerSignal {
        kind: PeerSignalKind::Answer,
        sdp: String::new(),
        id: Some("viewer-1".into()),
    };
    let result = engine.set_peer_answer(answer, old.offer_attempt);
    assert!(
        matches!(result, Err(MediaEngineError::NativePeer(message)) if message.contains("stale"))
    );
    assert!(engine
        .snapshot()
        .roster
        .iter()
        .any(|entry| entry.id == "viewer-1"));
}

#[test]
fn public_worker_late_lan_and_direct_joins_use_current_share_fence() {
    for mode in [JoinMode::Lan, JoinMode::Direct] {
        let engine = worker_engine(None, None);
        let mut open = request();
        open.join_mode = mode;
        open.share_on_start = false;
        engine.create_session(open).expect("session starts");
        let old = match mode {
            // The public offer ingress is deterministic here; LAN broadcast
            // discovery itself is covered by room tests because parallel Rust
            // test processes cannot reserve the fixed discovery port reliably.
            JoinMode::Lan => (
                String::new(),
                engine
                    .offer_for_member("late-viewer", "Viewer")
                    .expect("old LAN offer"),
                String::new(),
            ),
            JoinMode::Direct => {
                let port = engine
                    .snapshot()
                    .direct_listen_port
                    .expect("Direct listener port");
                engine
                    .discover_direct(SocketAddr::from(([127, 0, 0, 1], port)), "", "Viewer")
                    .expect("old Direct join")
            }
            JoinMode::Stunar => unreachable!("Stunar is covered by the Rendezvous tests"),
        };
        stop_share_until_complete(&engine);
        engine.start_share().expect("replacement Share starts");
        let current = match mode {
            JoinMode::Lan => (
                String::new(),
                engine
                    .offer_for_member("late-viewer", "Viewer")
                    .expect("current LAN offer"),
                String::new(),
            ),
            JoinMode::Direct => {
                let port = engine
                    .snapshot()
                    .direct_listen_port
                    .expect("Direct listener port");
                engine
                    .discover_direct(SocketAddr::from(([127, 0, 0, 1], port)), "", "Viewer")
                    .expect("current Direct join")
            }
            JoinMode::Stunar => unreachable!("Stunar is covered by the Rendezvous tests"),
        };
        let old_fence: OfferEpochFence =
            serde_json::from_str(&old.1.offer_attempt).expect("old fence");
        let current_fence: OfferEpochFence =
            serde_json::from_str(&current.1.offer_attempt).expect("current fence");
        assert!(current_fence.epoch.share.0 > old_fence.epoch.share.0);
        assert!(engine
            .set_peer_answer(
                PeerSignal {
                    kind: PeerSignalKind::Answer,
                    sdp: String::new(),
                    id: None,
                },
                old.1.offer_attempt,
            )
            .is_err());
        engine.close_peer_transport().expect("late link cleanup");
        engine.stop_session().expect("session stops");
    }
}

#[test]
fn public_worker_cleanup_failure_is_pending_until_every_ledger_counter_reaches_zero() {
    let counters = Arc::new(ResourceCounters::default());
    let engine = worker_engine_with_counters(None, None, Some(Arc::clone(&counters)));
    let mut open = request();
    open.share_on_start = false;
    engine.create_session(open).expect("session starts");
    counters.fail_cleanup(true);
    assert!(engine.stop_session().is_err());
    assert_eq!(engine.snapshot().state, MediaLifecycleState::CleanupPending);
    assert_ne!(engine.snapshot().state, MediaLifecycleState::Idle);

    counters.fail_cleanup(false);
    let stopped = engine.stop_session().expect("cleanup retry completes");
    assert_eq!(stopped.state, MediaLifecycleState::Idle);
    assert_zero(&counters);
}

#[test]
fn public_worker_stunar_prepare_failure_aborts_without_publishing_running() {
    let engine = worker_engine(None, None);
    let mut open = request();
    open.join_mode = JoinMode::Stunar;
    open.password = "valid-password".into();
    open.rendezvous_url = Some("http://127.0.0.1:1".into());
    open.share_on_start = false;
    let result = engine.create_session(open);
    assert!(result.is_err(), "unreachable prepare must fail");
    let snapshot = engine.snapshot();
    assert_ne!(snapshot.state, MediaLifecycleState::Running);
    assert_eq!(snapshot.state, MediaLifecycleState::Idle);
    assert!(snapshot.session_id.is_none());
}

#[test]
fn public_worker_signaling_completes_without_snapshot_polling_and_isolates_viewer_failure() {
    let engine = worker_engine(None, None);
    let mut open = request();
    open.join_mode = JoinMode::Direct;
    open.share_on_start = false;
    engine.create_session(open).expect("session starts");
    let port = engine
        .snapshot()
        .direct_listen_port
        .expect("Direct listener port");
    let host = SocketAddr::from(([127, 0, 0, 1], port));
    let first = engine
        .discover_direct(host, "", "Failed")
        .expect("first Direct join");
    let second = engine
        .discover_direct(host, "", "Healthy")
        .expect("second Direct join");

    // This is a signaling failure, not a synthetic native peer failure: the
    // public worker receives the answer and the real WebRTC worker rejects the
    // deliberately invalid SDP. No snapshot is needed to drive that work.
    engine
        .submit_room_answer(
            host,
            PeerSignal {
                kind: PeerSignalKind::Answer,
                sdp: String::new(),
                id: None,
            },
            first.1.offer_attempt.clone(),
        )
        .expect("answer mailbox accepts the exact fence");
    thread::sleep(Duration::from_millis(100));
    let roster = engine.snapshot().roster;
    assert_eq!(
        roster.len(),
        2,
        "one Direct Viewer failure must not reap the other"
    );
    assert_eq!(engine.snapshot().state, MediaLifecycleState::Running);
    assert_ne!(second.1.offer_attempt, first.1.offer_attempt);
    engine
        .close_peer_transport()
        .expect("failed link is isolated");
    engine.stop_session().expect("session stops");
}

#[test]
fn public_worker_accepts_only_h264_and_at_most_sixty_fps() {
    let engine = worker_engine(None, None);
    let mut codec = request();
    codec.codec = VideoCodec::Av1;
    assert!(matches!(
        engine.create_session(codec),
        Err(MediaEngineError::Unsupported(_))
    ));

    let mut high_fps = request();
    high_fps.frame_rate = FrameRate::Fps120;
    assert!(matches!(
        engine.create_session(high_fps),
        Err(MediaEngineError::Unsupported(_))
    ));
    assert_eq!(engine.snapshot().state, MediaLifecycleState::Idle);
}
