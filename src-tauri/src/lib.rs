mod media;

use media::logger;
use media::{
    CreateMediaSessionRequest, JoinMode, MediaEngine, MediaSessionSnapshot, MediaSessionStats,
    NativeCaptureSource, NativeRunningApp, PeerSignal, PreviewFrameEvent, ProbeReport,
    UpdateCredentialsRequest, UpdateMediaSessionRequest,
};
use tauri::State;

#[inline]
fn str_err<E: ToString>(error: E) -> String {
    error.to_string()
}

/// Returns the native media APIs known to be available on this platform.
///
/// This reports native capture and preview capability separately from native
/// encoding capability.
#[tauri::command]
fn get_media_capabilities(
    app: tauri::AppHandle,
    engine: State<'_, MediaEngine>,
) -> media::MediaCapabilities {
    engine.refresh_screen_recording_capabilities(&app)
}

/// Creates a native ScreenCaptureKit session on authorized macOS systems.
///
/// This command is async so Tauri does not run it on the main thread. The
/// system content picker must present and receive events on the AppKit run
/// loop; a sync command would deadlock waiting for that picker.
#[tauri::command]
async fn create_media_session(
    engine: State<'_, MediaEngine>,
    request: CreateMediaSessionRequest,
) -> Result<MediaSessionSnapshot, String> {
    let engine = engine.inner().clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        engine.create_session(request).map_err(str_err)
    })
    .await
    .map_err(str_err)?;
    result.map_err(|error| {
        logger::log("ERROR", "create session failed", &error);
        error
    })
}

/// Stops the active session and releases its native pipeline and stream handles.
/// Async: ScreenCaptureKit stop must not run on the UI thread (same deadlock
/// as start — the AppKit run loop has to keep turning).
#[tauri::command]
async fn stop_media_session(
    engine: State<'_, MediaEngine>,
) -> Result<MediaSessionSnapshot, String> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.stop_session().map_err(str_err))
        .await
        .map_err(str_err)?
}

/// Applies live settings (quality, system audio, exclusions) to the active
/// session without tearing down capture, the room, or the WebRTC peer.
#[tauri::command]
fn update_media_session(
    engine: State<'_, MediaEngine>,
    request: UpdateMediaSessionRequest,
) -> Result<MediaSessionSnapshot, String> {
    engine.update_session(request).map_err(str_err)
}

/// Reads the current native media state without moving frames over IPC.
#[tauri::command]
fn get_media_session_state(engine: State<'_, MediaEngine>) -> MediaSessionSnapshot {
    engine.snapshot()
}

/// Session-wide encoder + per-viewer link diagnostics for the Host popup.
#[tauri::command]
async fn get_media_session_stats(
    engine: State<'_, MediaEngine>,
) -> Result<MediaSessionStats, String> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.viewer_link_stats())
        .await
        .map_err(str_err)
}

/// Enumerates display and window metadata without exposing native handles or
/// frame buffers through IPC.
#[tauri::command]
fn get_media_capture_sources(
    engine: State<'_, MediaEngine>,
) -> Result<Vec<NativeCaptureSource>, String> {
    engine.enumerate_sources().map_err(str_err)
}

/// Requests Screen Recording access and returns the refreshed capability state.
#[tauri::command]
fn request_media_screen_recording_permission(
    app: tauri::AppHandle,
    engine: State<'_, MediaEngine>,
) -> media::MediaCapabilities {
    engine.request_screen_recording_permission(&app)
}

/// Returns the latest bounded, derived thumbnail, never the native source frame.
#[tauri::command]
fn get_media_preview(engine: State<'_, MediaEngine>) -> Option<PreviewFrameEvent> {
    engine.latest_preview()
}

#[tauri::command]
fn create_media_peer_offer(engine: State<'_, MediaEngine>) -> Result<PeerSignal, String> {
    engine.create_peer_offer().map_err(str_err)
}

#[tauri::command]
fn accept_media_peer_offer(
    engine: State<'_, MediaEngine>,
    offer: PeerSignal,
) -> Result<PeerSignal, String> {
    engine.accept_peer_offer(offer).map_err(str_err)
}

#[tauri::command]
fn set_media_peer_answer(engine: State<'_, MediaEngine>, answer: PeerSignal) -> Result<(), String> {
    engine.set_peer_answer(answer).map_err(str_err)
}

#[tauri::command]
fn close_media_peer_transport(engine: State<'_, MediaEngine>) -> Result<(), String> {
    engine.close_peer_transport().map_err(str_err)
}

#[tauri::command]
fn kick_media_viewer(
    engine: State<'_, MediaEngine>,
    id: String,
) -> Result<MediaSessionSnapshot, String> {
    engine.kick_viewer(&id).map_err(str_err)
}

#[tauri::command]
fn get_media_running_apps(engine: State<'_, MediaEngine>) -> Result<Vec<NativeRunningApp>, String> {
    engine.running_applications().map_err(str_err)
}

#[derive(serde::Deserialize)]
struct JoinRoomRequest {
    #[serde(default)]
    code: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    nickname: String,
    #[serde(default)]
    join_mode: JoinMode,
    /// Direct mode: IP literal + porta tudo junto (ex. 192.168.1.40:41234 ou
    /// [2001:db8::1]:41234). DNS rejeitado — o Viewer cola o que o Host mostrou.
    #[serde(default)]
    host: Option<String>,
    /// Campo legado: alguns clientes antigos separavam host/porta. Mantido para
    /// compatibilidade mas ignorado se `host` já contiver `:`.
    #[serde(default)]
    port: Option<u16>,
    /// Stunar mode: the Rendezvous base URL.
    #[serde(default)]
    rendezvous_url: Option<String>,
}

#[tauri::command]
async fn discover_media_room(
    engine: State<'_, MediaEngine>,
    request: JoinRoomRequest,
) -> Result<(String, PeerSignal, String), String> {
    let result: Result<(String, PeerSignal, String), String> = match request.join_mode {
        JoinMode::Lan | JoinMode::Direct => {
            tauri::async_runtime::spawn_blocking(move || match request.join_mode {
                JoinMode::Lan => {
                    let (host, offer, host_nickname) =
                        media::discover_room(&request.code, &request.password, &request.nickname)?;
                    Ok((host.to_string(), offer, host_nickname))
                }
                JoinMode::Direct => {
                    let raw_host = request
                        .host
                        .as_deref()
                        .ok_or_else(|| "Could not reach that address.".to_owned())?
                        .trim();
                    if raw_host.is_empty() {
                        return Err("Could not reach that address.".to_owned());
                    }
                    // O Viewer manda IP:porta tudo junto em `host`
                    // (ex. 192.168.1.10:41234 ou [2001:db8::1]:41234).
                    // `port` é compat com clientes antigos que separavam host/port.
                    let addr: std::net::SocketAddr = if let Ok(addr) = raw_host.parse() {
                        addr
                    } else if let Some(port) = request.port {
                        format!("{raw_host}:{port}")
                            .parse()
                            .map_err(|_| "Could not reach that address.".to_owned())?
                    } else {
                        return Err("Could not reach that address.".to_owned());
                    };
                    let (offer, host_nickname) =
                        media::discover_direct(addr, &request.password, &request.nickname)?;
                    Ok((addr.to_string(), offer, host_nickname))
                }
                JoinMode::Stunar => unreachable!("stunar handled outside spawn_blocking"),
            })
            .await
            .map_err(str_err)?
        }
        JoinMode::Stunar => {
            let engine = engine.inner().clone();
            let base = request
                .rendezvous_url
                .clone()
                .ok_or_else(|| "Set the Stunar URL in settings.".to_owned())?;
            tauri::async_runtime::spawn_blocking(move || {
                engine
                    .discover_stunar(&base, &request.code, &request.password, &request.nickname)
                    .map(|(token, offer)| (token, offer, String::new()))
                    .map_err(str_err)
            })
            .await
            .map_err(str_err)?
        }
    };
    result.map_err(|error| {
        logger::log("ERROR", "join failed", &error);
        error
    })
}

#[derive(serde::Deserialize)]
struct SubmitAnswerRequest {
    host: String,
    answer: PeerSignal,
    #[serde(default)]
    join_mode: JoinMode,
}

#[tauri::command]
async fn submit_media_room_answer(
    engine: State<'_, MediaEngine>,
    request: SubmitAnswerRequest,
) -> Result<(), String> {
    match request.join_mode {
        JoinMode::Stunar => {
            let engine = engine.inner().clone();
            tauri::async_runtime::spawn_blocking(move || {
                engine.submit_stunar_answer(request.answer).map_err(str_err)
            })
            .await
            .map_err(str_err)?
        }
        JoinMode::Lan | JoinMode::Direct => tauri::async_runtime::spawn_blocking(move || {
            let host = request
                .host
                .parse()
                .map_err(|_| "invalid room host address".to_owned())?;
            media::submit_room_answer(host, &request.answer)
        })
        .await
        .map_err(str_err)?,
    }
}

/// Viewer-side Stunar cleanup: drops the stored WS (called on Disconnect).
#[tauri::command]
fn stunar_viewer_close(engine: State<'_, MediaEngine>) {
    engine.close_stunar_viewer();
}

#[tauri::command]
fn poll_stunar_offers(engine: State<'_, MediaEngine>) -> Vec<media::StunarIncomingOffer> {
    engine.poll_incoming_offers()
}

#[derive(serde::Deserialize)]
struct RoomSignalRequest {
    to: String,
    answer: PeerSignal,
}

#[tauri::command]
fn send_stunar_room_answer(
    engine: State<'_, MediaEngine>,
    request: RoomSignalRequest,
) -> Result<(), String> {
    let mut signal = request.answer;
    signal.id = Some(request.to.clone());
    engine
        .send_stunar_signal(&request.to, signal)
        .map_err(str_err)
}

#[derive(serde::Deserialize)]
struct RoomOfferSendRequest {
    to: String,
    offer: PeerSignal,
}

#[tauri::command]
fn send_stunar_room_offer(
    engine: State<'_, MediaEngine>,
    request: RoomOfferSendRequest,
) -> Result<(), String> {
    engine
        .send_stunar_signal(&request.to, request.offer)
        .map_err(str_err)
}

#[derive(serde::Deserialize)]
struct MemberOfferRequest {
    id: String,
    nickname: String,
}

#[tauri::command]
fn create_member_offer(
    engine: State<'_, MediaEngine>,
    request: MemberOfferRequest,
) -> Result<PeerSignal, String> {
    engine
        .offer_for_member(&request.id, &request.nickname)
        .map_err(str_err)
}

#[tauri::command]
fn announce_media_share(engine: State<'_, MediaEngine>, start: bool) -> Result<(), String> {
    engine.announce_share(start).map_err(str_err)
}

#[tauri::command]
fn stunar_watch(engine: State<'_, MediaEngine>, to: String, start: bool) -> Result<(), String> {
    engine.request_watch(&to, start).map_err(str_err)
}

/// Async like `create_media_session`: ScreenCaptureKit's picker must run on
/// the AppKit loop. A sync command on the UI thread deadlocks the window.
#[tauri::command]
async fn start_media_share(engine: State<'_, MediaEngine>) -> Result<MediaSessionSnapshot, String> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.start_share().map_err(str_err))
        .await
        .map_err(str_err)?
}

#[tauri::command]
async fn stop_media_share(engine: State<'_, MediaEngine>) -> Result<MediaSessionSnapshot, String> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.stop_share().map_err(str_err))
        .await
        .map_err(str_err)?
}

/// Returns the last 30 session log files (newest first) for the View logs UI.
#[tauri::command]
async fn get_app_logs() -> Vec<media::LogSession> {
    tauri::async_runtime::spawn_blocking(media::read_sessions)
        .await
        .unwrap_or_default()
}

/// Absolute path of the session logs directory, for the "open folder" action.
#[tauri::command]
fn get_logs_dir() -> Option<String> {
    media::logs_dir_string()
}

/// Deletes every session log file.
#[tauri::command]
fn clear_app_logs() {
    media::clear();
}

#[tauri::command]
fn reset_firewall_rules() -> Result<String, String> {
    crate::media::firewall::reset_firewall_rules()
}

#[tauri::command]
fn get_firewall_status() -> String {
    crate::media::firewall::check_firewall_status()
}

#[tauri::command]
fn admit_media_viewer(
    engine: State<'_, MediaEngine>,
    id: String,
) -> Result<MediaSessionSnapshot, String> {
    engine.admit_viewer(&id).map_err(str_err)
}

#[tauri::command]
fn reject_media_viewer(
    engine: State<'_, MediaEngine>,
    id: String,
) -> Result<MediaSessionSnapshot, String> {
    engine.reject_viewer(&id).map_err(str_err)
}

#[tauri::command]
fn update_media_session_credentials(
    engine: State<'_, MediaEngine>,
    request: UpdateCredentialsRequest,
) -> Result<MediaSessionSnapshot, String> {
    engine.update_session_credentials(request).map_err(str_err)
}

/// Local encoder probe. No Session, no Rendezvous, no Media on the wire.
#[tauri::command]
fn run_media_benchmark(engine: State<'_, MediaEngine>) -> Result<ProbeReport, String> {
    let snapshot = engine.snapshot();
    if snapshot.state == media::MediaLifecycleState::Running || snapshot.native_capture_active {
        return Err("Stop the session before measuring this PC.".into());
    }
    let caps = engine.capabilities();
    Ok(media::run_local_probe(
        caps.native_encoder_implemented,
        caps.av1_encode_supported,
        cfg!(target_os = "macos"),
    ))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(MediaEngine::new())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            get_media_capabilities,
            create_media_session,
            update_media_session,
            stop_media_session,
            get_media_session_state,
            get_media_session_stats,
            get_media_capture_sources,
            request_media_screen_recording_permission,
            get_media_preview,
            create_media_peer_offer,
            accept_media_peer_offer,
            set_media_peer_answer,
            close_media_peer_transport,
            kick_media_viewer,
            admit_media_viewer,
            reject_media_viewer,
            update_media_session_credentials,
            get_media_running_apps,
            discover_media_room,
            submit_media_room_answer,
            stunar_viewer_close,
            get_app_logs,
            clear_app_logs,
            get_logs_dir,
            reset_firewall_rules,
            get_firewall_status,
            run_media_benchmark,
            poll_stunar_offers,
            send_stunar_room_answer,
            send_stunar_room_offer,
            create_member_offer,
            announce_media_share,
            stunar_watch,
            start_media_share,
            stop_media_share
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
