mod media;

use media::{
    CreateMediaSessionRequest, MediaEngine, MediaSessionSnapshot, NativeCaptureSource,
    NativeRunningApp, PeerSignal, PreviewFrameEvent, UpdateMediaSessionRequest,
};
use tauri::State;

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
    tauri::async_runtime::spawn_blocking(move || {
        engine
            .create_session(request)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

/// Stops the active session and releases its native pipeline and stream handles.
#[tauri::command]
fn stop_media_session(engine: State<'_, MediaEngine>) -> Result<MediaSessionSnapshot, String> {
    engine.stop_session().map_err(|error| error.to_string())
}

/// Applies live settings (quality, system audio, exclusions) to the active
/// session without tearing down capture, the room, or the WebRTC peer.
#[tauri::command]
fn update_media_session(
    engine: State<'_, MediaEngine>,
    request: UpdateMediaSessionRequest,
) -> Result<MediaSessionSnapshot, String> {
    engine
        .update_session(request)
        .map_err(|error| error.to_string())
}

/// Reads the current native media state without moving frames over IPC.
#[tauri::command]
fn get_media_session_state(engine: State<'_, MediaEngine>) -> MediaSessionSnapshot {
    engine.snapshot()
}

/// Enumerates display and window metadata without exposing native handles or
/// frame buffers through IPC.
#[tauri::command]
fn get_media_capture_sources(
    engine: State<'_, MediaEngine>,
) -> Result<Vec<NativeCaptureSource>, String> {
    engine
        .enumerate_sources()
        .map_err(|error| error.to_string())
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
    engine
        .create_peer_offer()
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn accept_media_peer_offer(
    engine: State<'_, MediaEngine>,
    offer: PeerSignal,
) -> Result<PeerSignal, String> {
    engine
        .accept_peer_offer(offer)
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn set_media_peer_answer(engine: State<'_, MediaEngine>, answer: PeerSignal) -> Result<(), String> {
    engine
        .set_peer_answer(answer)
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn close_media_peer_transport(engine: State<'_, MediaEngine>) -> Result<(), String> {
    engine
        .close_peer_transport()
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn get_media_running_apps(
    engine: State<'_, MediaEngine>,
) -> Result<Vec<NativeRunningApp>, String> {
    engine
        .running_applications()
        .map_err(|error| error.to_string())
}

#[derive(serde::Deserialize)]
struct JoinRoomRequest {
    code: String,
}

#[tauri::command]
fn discover_media_room(request: JoinRoomRequest) -> Result<(String, PeerSignal), String> {
    let (host, offer) = media::discover_room(&request.code)?;
    Ok((host.to_string(), offer))
}

#[derive(serde::Deserialize)]
struct SubmitAnswerRequest {
    host: String,
    answer: PeerSignal,
}

#[tauri::command]
fn submit_media_room_answer(request: SubmitAnswerRequest) -> Result<(), String> {
    let host = request
        .host
        .parse()
        .map_err(|_| "invalid room host address".to_owned())?;
    media::submit_room_answer(host, &request.answer)
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
            get_media_capture_sources,
            request_media_screen_recording_permission,
            get_media_preview,
            create_media_peer_offer,
            accept_media_peer_offer,
            set_media_peer_answer,
            close_media_peer_transport,
            get_media_running_apps,
            discover_media_room,
            submit_media_room_answer
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
