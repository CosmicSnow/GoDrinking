//! Windows system-audio capture. Video stays in the other modules.
//!
//! Muted apps are never captured (process-loopback include of everyone else).
//! Subtracting them from the system mix leaked a choppy copy on Windows 10
//! and 11. Heard apps share one output clock so a late client cannot punch
//! silence into the others.

use golive_platform::{app_excluded_by_token, AudioApp, EncodedAudioPacket, PlatformError};
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const FRAME_SAMPLES: usize = 960 * 2;
const MAX_INCLUDE: usize = 16;

pub struct AudioTap {
    shutdown: Arc<AtomicBool>,
    native: Option<WindowsAudioTap>,
}

struct WindowsAudioTap {
    thread: Option<JoinHandle<()>>,
}

unsafe impl Send for WindowsAudioTap {}

impl Drop for AudioTap {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(native) = self.native.take() {
            if let Some(thread) = native.thread {
                let _ = thread.join();
            }
        }
    }
}

pub fn is_process_loopback_supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        let _ = wasapi::initialize_mta();
        wasapi::AudioClient::new_application_loopback_client(std::process::id(), true).is_ok()
    })
}

pub fn list_audio_apps() -> Vec<AudioApp> {
    let emitting = emitting_pids();
    let mut apps = Vec::new();
    for proc in process_snapshot() {
        if proc.pid == 0 || proc.exe.is_empty() {
            continue;
        }
        let name = proc
            .exe
            .strip_suffix(".exe")
            .or_else(|| proc.exe.strip_suffix(".EXE"))
            .unwrap_or(&proc.exe)
            .to_owned();
        apps.push(AudioApp {
            name,
            id: proc.exe,
            pid: proc.pid as i32,
            emitting_audio: emitting.contains(&proc.pid),
        });
    }
    apps.sort_by(|left, right| {
        left.name
            .to_ascii_lowercase()
            .cmp(&right.name.to_ascii_lowercase())
    });
    apps.dedup_by(|left, right| left.pid == right.pid);
    apps
}

pub fn start_audio_tap(
    excluded_tokens: &[String],
    opus_tx: SyncSender<EncodedAudioPacket>,
) -> Result<AudioTap, PlatformError> {
    let _ = wasapi::initialize_mta();
    if is_process_loopback_supported() {
        if let Ok(tap) = spawn_continuous(excluded_tokens, opus_tx.clone()) {
            return Ok(tap);
        }
    }
    spawn_device_loopback(opus_tx)
}

pub fn start_audio_tap_include(
    pid: u32,
    opus_tx: SyncSender<EncodedAudioPacket>,
) -> Result<AudioTap, PlatformError> {
    let _ = wasapi::initialize_mta();
    if pid == 0 || !is_process_loopback_supported() {
        return Err(PlatformError::Internal("áudio do app indisponível".into()));
    }
    let client = wasapi::AudioClient::new_application_loopback_client(pid, true)
        .map_err(|error| PlatformError::Internal(format!("WASAPI include: {error}")))?;
    spawn_mix(vec![client], opus_tx, "include")
}

fn spawn_continuous(
    tokens: &[String],
    opus_tx: SyncSender<EncodedAudioPacket>,
) -> Result<AudioTap, PlatformError> {
    let tokens = tokens.to_vec();
    let shutdown = Arc::new(AtomicBool::new(false));
    let worker_shutdown = Arc::clone(&shutdown);
    let thread = thread::Builder::new()
        .name("golive-audio-opus".into())
        .spawn(move || selective_loop(tokens, opus_tx, worker_shutdown))
        .map_err(|error| PlatformError::Internal(error.to_string()))?;
    Ok(AudioTap {
        shutdown,
        native: Some(WindowsAudioTap {
            thread: Some(thread),
        }),
    })
}

fn spawn_device_loopback(
    opus_tx: SyncSender<EncodedAudioPacket>,
) -> Result<AudioTap, PlatformError> {
    spawn_mix(vec![device_client()?], opus_tx, "device")
}

fn device_client() -> Result<wasapi::AudioClient, PlatformError> {
    let enumerator = wasapi::DeviceEnumerator::new()
        .map_err(|error| PlatformError::Internal(format!("WASAPI enumerator: {error}")))?;
    let device = enumerator
        .get_default_device(&wasapi::Direction::Render)
        .map_err(|error| PlatformError::Internal(format!("WASAPI render device: {error}")))?;
    device
        .get_iaudioclient()
        .map_err(|error| PlatformError::Internal(format!("WASAPI client: {error}")))
}

fn emitting_pids() -> HashSet<u32> {
    active_session_pids()
        .unwrap_or_default()
        .into_iter()
        .collect()
}

fn active_session_pids() -> Result<Vec<u32>, ()> {
    let mut pids = Vec::new();
    let _ = wasapi::initialize_mta();
    let enumerator = wasapi::DeviceEnumerator::new().map_err(|_| ())?;
    let device = enumerator
        .get_default_device(&wasapi::Direction::Render)
        .map_err(|_| ())?;
    let manager = device.get_iaudiosessionmanager().map_err(|_| ())?;
    let sessions = manager.get_audiosessionenumerator().map_err(|_| ())?;
    let count = sessions.get_count().map_err(|_| ())?;
    for index in 0..count {
        let Ok(session) = sessions.get_session(index) else {
            continue;
        };
        let Ok(pid) = session.get_process_id() else {
            continue;
        };
        if pid == 0 {
            continue;
        }
        let active = matches!(session.get_state(), Ok(wasapi::SessionState::Active));
        let peak = session
            .get_audiometerinformation()
            .and_then(|meter| meter.get_peak_value())
            .unwrap_or(0.0);
        if active || peak > 0.0005 {
            pids.push(pid);
        }
    }
    pids.sort_unstable();
    pids.dedup();
    Ok(pids)
}

#[derive(Clone, Debug)]
struct Proc {
    pid: u32,
    ppid: u32,
    exe: String,
}

fn current_exe_name() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_default()
}

fn exe_matches(exe: &str, token: &str) -> bool {
    let token = token.trim();
    !token.is_empty() && app_excluded_by_token(exe, Some(exe), token)
}

fn process_snapshot() -> Vec<Proc> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return Vec::new();
        };
        if snapshot.is_invalid() {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut entry = PROCESSENTRY32W::default();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut more = Process32FirstW(snapshot, &mut entry).is_ok();
        while more {
            let len = entry
                .szExeFile
                .iter()
                .position(|unit| *unit == 0)
                .unwrap_or(entry.szExeFile.len());
            out.push(Proc {
                pid: entry.th32ProcessID,
                ppid: entry.th32ParentProcessID,
                exe: String::from_utf16_lossy(&entry.szExeFile[..len]),
            });
            more = Process32NextW(snapshot, &mut entry).is_ok();
        }
        let _ = CloseHandle(snapshot);
        out
    }
}

fn mark_descendants(procs: &[Proc], excluded: &mut HashSet<u32>) {
    for _ in 0..procs.len().saturating_add(1) {
        let mut grew = false;
        for proc in procs {
            if proc.pid != 0 && excluded.contains(&proc.ppid) && excluded.insert(proc.pid) {
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
}

fn is_descendant(procs: &[Proc], pid: u32, ancestor: u32) -> bool {
    let mut current = pid;
    for _ in 0..procs.len().saturating_add(1) {
        let Some(proc) = procs.iter().find(|item| item.pid == current) else {
            return false;
        };
        if proc.ppid == ancestor {
            return true;
        }
        if proc.ppid == 0 || proc.ppid == current {
            return false;
        }
        current = proc.ppid;
    }
    false
}

fn excluded_pids(procs: &[Proc], tokens: &[String], self_pid: u32, self_exe: &str) -> HashSet<u32> {
    let meaningful: Vec<&str> = tokens
        .iter()
        .map(|token| token.trim())
        .filter(|token| !token.is_empty())
        .collect();
    let mut excluded = HashSet::new();
    if meaningful.is_empty() {
        excluded.insert(self_pid);
    } else {
        for proc in procs {
            if meaningful.iter().any(|token| exe_matches(&proc.exe, token)) {
                excluded.insert(proc.pid);
            }
        }
        if meaningful.iter().any(|token| exe_matches(self_exe, token)) {
            excluded.insert(self_pid);
        }
    }
    mark_descendants(procs, &mut excluded);
    excluded
}

fn include_roots(
    procs: &[Proc],
    session_pids: &[u32],
    tokens: &[String],
    self_pid: u32,
    self_exe: &str,
) -> Vec<u32> {
    let excluded = excluded_pids(procs, tokens, self_pid, self_exe);
    let mut wanted = HashSet::new();
    for pid in session_pids {
        if *pid == 0 || excluded.contains(pid) {
            continue;
        }
        let Some(proc) = procs.iter().find(|proc| proc.pid == *pid) else {
            continue;
        };
        if is_engine_exe(&proc.exe) {
            continue;
        }
        if procs
            .iter()
            .any(|other| excluded.contains(&other.pid) && is_descendant(procs, *pid, other.pid))
        {
            continue;
        }
        wanted.insert(*pid);
    }
    let mut roots: Vec<u32> = wanted
        .iter()
        .copied()
        .filter(|pid| {
            !wanted
                .iter()
                .any(|other| *other != *pid && is_descendant(procs, *pid, *other))
        })
        .collect();
    roots.sort_unstable();
    roots.dedup();
    if roots.len() > MAX_INCLUDE {
        roots.truncate(MAX_INCLUDE);
    }
    roots
}

fn is_engine_exe(exe: &str) -> bool {
    exe.eq_ignore_ascii_case("audiodg.exe") || exe.eq_ignore_ascii_case("audiodg")
}

struct PcmCapture {
    client: wasapi::AudioClient,
    capture: wasapi::AudioCaptureClient,
    pending: VecDeque<u8>,
    pcm: VecDeque<f32>,
    discontinuity: bool,
}

struct LiveInclude {
    pid: u32,
    capture: PcmCapture,
}

struct MixCapture {
    streams: Vec<PcmCapture>,
}

unsafe impl Send for MixCapture {}
unsafe impl Send for LiveInclude {}
unsafe impl Send for PcmCapture {}

impl Drop for PcmCapture {
    fn drop(&mut self) {
        let _ = self.client.stop_stream();
    }
}

fn init_capture(mut client: wasapi::AudioClient, kind: &str) -> Result<PcmCapture, PlatformError> {
    let desired = wasapi::WaveFormat::new(32, 32, &wasapi::SampleType::Float, 48_000, 2, None);
    let mode = wasapi::StreamMode::PollingShared {
        autoconvert: true,
        buffer_duration_hns: 200_000,
    };
    client
        .initialize_client(&desired, &wasapi::Direction::Capture, &mode)
        .map_err(|error| PlatformError::Internal(format!("WASAPI {kind} init: {error}")))?;
    let capture = client
        .get_audiocaptureclient()
        .map_err(|error| PlatformError::Internal(format!("WASAPI {kind} capture: {error}")))?;
    client
        .start_stream()
        .map_err(|error| PlatformError::Internal(format!("WASAPI {kind} start: {error}")))?;
    Ok(PcmCapture {
        client,
        capture,
        pending: VecDeque::new(),
        pcm: VecDeque::new(),
        discontinuity: false,
    })
}

fn spawn_mix(
    clients: Vec<wasapi::AudioClient>,
    opus_tx: SyncSender<EncodedAudioPacket>,
    kind: &str,
) -> Result<AudioTap, PlatformError> {
    let mut streams = Vec::new();
    for client in clients {
        streams.push(init_capture(client, kind)?);
    }
    let shutdown = Arc::new(AtomicBool::new(false));
    let worker_shutdown = Arc::clone(&shutdown);
    let mix = MixCapture { streams };
    let thread = thread::Builder::new()
        .name("golive-audio-opus".into())
        .spawn(move || wasapi_mix(mix, opus_tx, worker_shutdown))
        .map_err(|error| PlatformError::Internal(error.to_string()))?;
    Ok(AudioTap {
        shutdown,
        native: Some(WindowsAudioTap {
            thread: Some(thread),
        }),
    })
}

fn pull_pcm(stream: &mut PcmCapture) -> bool {
    let mut got = false;
    loop {
        let Ok(Some(frames)) = stream.capture.get_next_packet_size() else {
            break;
        };
        if frames == 0 {
            break;
        }
        let Ok(info) = stream
            .capture
            .read_from_device_to_deque(&mut stream.pending)
        else {
            break;
        };
        if info.flags.data_discontinuity {
            stream.discontinuity = true;
            stream.pending.clear();
        }
        got = true;
        while stream.pending.len() >= 8 {
            let mut frame = [0_u8; 8];
            for byte in frame.iter_mut() {
                *byte = stream.pending.pop_front().expect("length checked above");
            }
            stream
                .pcm
                .push_back(f32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]));
            stream
                .pcm
                .push_back(f32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]]));
        }
    }
    got
}

fn take_frame(streams: &mut [PcmCapture]) -> Option<Vec<f32>> {
    if !streams
        .iter()
        .any(|stream| stream.pcm.len() >= FRAME_SAMPLES)
    {
        return None;
    }
    let mut frame = vec![0.0f32; FRAME_SAMPLES];
    let mut mixed = false;
    for stream in streams {
        if stream.pcm.len() < FRAME_SAMPLES {
            continue;
        }
        mixed = true;
        for sample in &mut frame {
            *sample += stream.pcm.pop_front().unwrap_or(0.0);
        }
    }
    if mixed {
        for sample in &mut frame {
            *sample = sample.clamp(-1.0, 1.0);
        }
    }
    mixed.then_some(frame)
}

fn encode_frame(
    encoder: &mut opus::Encoder,
    frame: &[f32],
    opus_tx: &SyncSender<EncodedAudioPacket>,
) -> bool {
    let mut output = vec![0_u8; 4000];
    match encoder.encode_float(frame, &mut output) {
        Ok(size) if size > 0 => {
            output.truncate(size);
            let packet = EncodedAudioPacket {
                data: output,
                duration: Duration::from_millis(20),
            };
            let deadline = Instant::now() + Duration::from_millis(40);
            loop {
                match opus_tx.try_send(packet.clone()) {
                    Ok(()) => return true,
                    Err(TrySendError::Disconnected(_)) => return false,
                    Err(TrySendError::Full(_)) if Instant::now() >= deadline => return true,
                    Err(TrySendError::Full(_)) => thread::sleep(Duration::from_millis(2)),
                }
            }
        }
        _ => true,
    }
}

fn wasapi_mix(
    mut mix: MixCapture,
    opus_tx: SyncSender<EncodedAudioPacket>,
    shutdown: Arc<AtomicBool>,
) {
    let _ = wasapi::initialize_mta();
    let Ok(mut encoder) =
        opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Audio)
    else {
        return;
    };
    let mut next_emit = Instant::now();
    let mut prev = [0.0f32; 2];
    while !shutdown.load(Ordering::Acquire) {
        let mut force = false;
        for stream in &mut mix.streams {
            if pull_pcm(stream) && stream.discontinuity {
                force = true;
                stream.discontinuity = false;
            }
            if trim_latency(&mut stream.pcm) {
                force = true;
            }
        }
        let now = Instant::now();
        if now >= next_emit {
            if let Some(mut frame) = take_frame(&mut mix.streams) {
                declick(&mut prev, &mut frame, force);
                if !encode_frame(&mut encoder, &frame, &opus_tx) {
                    return;
                }
                next_emit = pace_next(next_emit, now);
            }
        }
        thread::sleep(Duration::from_millis(2));
    }
}

fn open_include(pid: u32) -> Option<PcmCapture> {
    let client = wasapi::AudioClient::new_application_loopback_client(pid, true).ok()?;
    init_capture(client, "include").ok()
}

fn selective_loop(
    tokens: Vec<String>,
    opus_tx: SyncSender<EncodedAudioPacket>,
    shutdown: Arc<AtomicBool>,
) {
    let _ = wasapi::initialize_mta();
    let Ok(mut encoder) =
        opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Audio)
    else {
        return;
    };
    let (sub_tx, sub_rx) = mpsc::channel::<LiveInclude>();
    let watch_shutdown = Arc::clone(&shutdown);
    let watch_tokens = tokens.clone();
    let watcher = thread::Builder::new()
        .name("golive-audio-exclude".into())
        .spawn(move || watch_includes(watch_tokens, sub_tx, watch_shutdown))
        .ok();
    let mut clients: Vec<LiveInclude> = Vec::new();
    let mut next_emit = Instant::now();
    let mut prev = [0.0f32; 2];
    while !shutdown.load(Ordering::Acquire) {
        while let Ok(client) = sub_rx.try_recv() {
            if clients.iter().any(|existing| existing.pid == client.pid) {
                continue;
            }
            clients.push(client);
        }
        let mut force = false;
        let now = Instant::now();
        for client in &mut clients {
            if pull_pcm(&mut client.capture) && client.capture.discontinuity {
                force = true;
                client.capture.discontinuity = false;
            }
            if trim_latency(&mut client.capture.pcm) {
                force = true;
            }
        }
        if now >= next_emit {
            if let Some(mut frame) = mix_ready(&mut clients) {
                declick(&mut prev, &mut frame, force);
                if !encode_frame(&mut encoder, &frame, &opus_tx) {
                    let _ = watcher.and_then(|thread| thread.join().ok());
                    return;
                }
                next_emit = pace_next(next_emit, now);
            }
        }
        thread::sleep(Duration::from_millis(2));
    }
    let _ = watcher.and_then(|thread| thread.join().ok());
}

fn watch_includes(tokens: Vec<String>, sub_tx: mpsc::Sender<LiveInclude>, shutdown: Arc<AtomicBool>) {
    let _ = wasapi::initialize_mta();
    let mut opened: HashSet<u32> = HashSet::new();
    while !shutdown.load(Ordering::Acquire) {
        let self_pid = std::process::id();
        let self_exe = current_exe_name();
        let sessions = active_session_pids().unwrap_or_default();
        let wanted = include_roots(
            &process_snapshot(),
            &sessions,
            &tokens,
            self_pid,
            &self_exe,
        );
        for pid in wanted {
            if shutdown.load(Ordering::Acquire) {
                return;
            }
            if opened.contains(&pid) {
                continue;
            }
            if let Some(capture) = open_include(pid) {
                opened.insert(pid);
                if sub_tx
                    .send(LiveInclude { pid, capture })
                    .is_err()
                {
                    return;
                }
            }
        }
        for _ in 0..10 {
            if shutdown.load(Ordering::Acquire) {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

fn mix_ready(clients: &mut [LiveInclude]) -> Option<Vec<f32>> {
    if clients.is_empty() {
        return None;
    }
    // Only clients holding a full frame take part. Waiting on a client that
    // just went quiet stalls the shared 20 ms clock and drops audio.
    let gating: Vec<usize> = clients
        .iter()
        .enumerate()
        .filter(|(_, client)| client.capture.pcm.len() >= FRAME_SAMPLES)
        .map(|(index, _)| index)
        .collect();
    if gating.is_empty() {
        return None;
    }
    let min_len = gating
        .iter()
        .map(|index| clients[*index].capture.pcm.len())
        .min()
        .unwrap_or(0);
    if min_len < FRAME_SAMPLES {
        return None;
    }
    let mut frame = vec![0.0f32; FRAME_SAMPLES];
    let mut mixed = false;
    for index in gating {
        mixed = true;
        for sample in &mut frame {
            *sample += clients[index].capture.pcm.pop_front().unwrap_or(0.0);
        }
    }
    if !mixed {
        return None;
    }
    for sample in &mut frame {
        *sample = sample.clamp(-1.0, 1.0);
    }
    Some(frame)
}

fn pace_next(next_emit: Instant, now: Instant) -> Instant {
    let next = next_emit + Duration::from_millis(20);
    if next + Duration::from_millis(40) < now {
        now + Duration::from_millis(20)
    } else {
        next
    }
}

fn trim_latency(pcm: &mut VecDeque<f32>) -> bool {
    let cap = FRAME_SAMPLES * 15;
    if pcm.len() <= cap {
        return false;
    }
    let keep = FRAME_SAMPLES * 5;
    let drop = pcm.len() - keep;
    pcm.drain(..drop);
    true
}

fn declick(prev: &mut [f32; 2], frame: &mut [f32], force: bool) {
    if frame.len() < 4 {
        return;
    }
    let jump = (frame[0] - prev[0]).abs().max((frame[1] - prev[1]).abs());
    if force || jump > 0.2 {
        let frames = (frame.len() / 2).min(48);
        for index in 0..frames {
            let gain = (index as f32 + 1.0) / frames as f32;
            let base = index * 2;
            frame[base] = prev[0] + (frame[base] - prev[0]) * gain;
            frame[base + 1] = prev[1] + (frame[base + 1] - prev[1]) * gain;
        }
    }
    let last = frame.len() - 2;
    prev[0] = frame[last];
    prev[1] = frame[last + 1];
}

#[cfg(test)]
mod tests {
    use super::{declick, include_roots, Proc};

    fn proc(pid: u32, ppid: u32, exe: &str) -> Proc {
        Proc {
            pid,
            ppid,
            exe: exe.into(),
        }
    }

    #[test]
    fn muted_apps_are_absent_and_the_rest_is_kept() {
        let procs = vec![
            proc(10, 1, "Discord.exe"),
            proc(11, 10, "Discord.exe"),
            proc(12, 1, "DiscordPTB.exe"),
            proc(20, 1, "goDrinking.exe"),
            proc(21, 20, "golive-video.exe"),
            proc(30, 1, "chrome.exe"),
            proc(31, 30, "chrome.exe"),
        ];
        let tokens = vec!["Discord".into(), "goDrinking".into()];
        let sessions = vec![10, 11, 12, 20, 21, 30, 31];
        assert_eq!(
            include_roots(&procs, &sessions, &tokens, 20, "goDrinking.exe"),
            vec![30]
        );
    }

    #[test]
    fn untoggled_discord_is_heard_and_self_stays_out() {
        let procs = vec![proc(10, 1, "Discord.exe"), proc(20, 1, "goDrinking.exe"), proc(30, 1, "chrome.exe")];
        let roots = include_roots(&procs, &[10, 20, 30], &["goDrinking.exe".into()], 20, "goDrinking.exe");
        assert_eq!(roots, vec![10, 30]);
    }

    #[test]
    fn declick_ramps_a_pop_and_leaves_a_smooth_join() {
        let mut prev = [0.0f32; 2];
        let mut popped = vec![0.8f32, -0.8, 0.8, -0.8, 0.8, -0.8, 0.8, -0.8];
        declick(&mut prev, &mut popped, false);
        assert!(popped[0].abs() < 0.25, "{}", popped[0]);
        assert!((popped[6] - 0.8).abs() < 0.05, "{}", popped[6]);
        prev = [0.02, -0.01];
        let mut smooth = vec![0.02f32, -0.01, 0.03, -0.02];
        let before = smooth.clone();
        declick(&mut prev, &mut smooth, false);
        assert_eq!(smooth, before);
    }
}
