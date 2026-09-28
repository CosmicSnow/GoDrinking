//! Windows system-audio capture. Video stays in the other modules.
//!
//! The shared mix is one continuous WASAPI stream (engine-exclude this
//! process when it should not be heard). Other ignored apps are cancelled
//! from that stream and never become the clock — stitching their loopbacks
//! together is what chopped the audio.

use golive_platform::{app_excluded_by_token, AudioApp, EncodedAudioPacket, PlatformError};
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const FRAME_SAMPLES: usize = 960 * 2;
const MAX_SUBTRACT: usize = 8;
const HISTORY_CAP: usize = 48_000 * 120 / 1000 * 2;

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
    let self_pid = std::process::id();
    let self_exe = current_exe_name();
    let procs = process_snapshot();
    let (engine_self, subtract) = subtract_plan(&procs, tokens, self_pid, &self_exe);
    let main_client = if engine_self {
        match wasapi::AudioClient::new_application_loopback_client(self_pid, false) {
            Ok(client) => client,
            Err(_) => device_client()?,
        }
    } else {
        device_client()?
    };
    let main = init_capture(main_client, "mix")?;
    let tokens = tokens.to_vec();
    let shutdown = Arc::new(AtomicBool::new(false));
    let worker_shutdown = Arc::clone(&shutdown);
    let thread = thread::Builder::new()
        .name("golive-audio-opus".into())
        .spawn(move || continuous_loop(main, tokens, subtract, opus_tx, worker_shutdown))
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

fn subtract_plan(procs: &[Proc], tokens: &[String], self_pid: u32, self_exe: &str) -> (bool, Vec<u32>) {
    let excluded = excluded_pids(procs, tokens, self_pid, self_exe);
    let engine_self = excluded.contains(&self_pid);
    let mut roots = Vec::new();
    for proc in procs {
        if proc.pid == 0 || !excluded.contains(&proc.pid) {
            continue;
        }
        if engine_self && (proc.pid == self_pid || is_descendant(procs, proc.pid, self_pid)) {
            continue;
        }
        if excluded.contains(&proc.ppid) {
            continue;
        }
        roots.push(proc.pid);
    }
    roots.sort_unstable();
    roots.dedup();
    if roots.len() > MAX_SUBTRACT {
        roots.truncate(MAX_SUBTRACT);
    }
    (engine_self, roots)
}

struct PcmCapture {
    client: wasapi::AudioClient,
    capture: wasapi::AudioCaptureClient,
    pending: VecDeque<u8>,
    pcm: VecDeque<f32>,
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
        let Ok(_info) = stream
            .capture
            .read_from_device_to_deque(&mut stream.pending)
        else {
            break;
        };
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
            match opus_tx.try_send(EncodedAudioPacket {
                data: output,
                duration: Duration::from_millis(20),
            }) {
                Ok(()) | Err(TrySendError::Full(_)) => true,
                Err(TrySendError::Disconnected(_)) => false,
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
    while !shutdown.load(Ordering::Acquire) {
        let mut got = false;
        for stream in &mut mix.streams {
            if pull_pcm(stream) {
                got = true;
            }
        }
        while let Some(frame) = take_frame(&mut mix.streams) {
            if !encode_frame(&mut encoder, &frame, &opus_tx) {
                return;
            }
        }
        if !got {
            thread::sleep(Duration::from_millis(5));
        }
    }
}

fn open_include(pid: u32) -> Option<PcmCapture> {
    let client = wasapi::AudioClient::new_application_loopback_client(pid, true).ok()?;
    init_capture(client, "include").ok()
}

fn continuous_loop(
    mut main: PcmCapture,
    tokens: Vec<String>,
    initial_subtract: Vec<u32>,
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
    let watcher = thread::Builder::new()
        .name("golive-audio-exclude".into())
        .spawn(move || watch_subtract(tokens, initial_subtract, sub_tx, watch_shutdown))
        .ok();
    let mut subs: Vec<LiveInclude> = Vec::new();
    let mut history: Vec<VecDeque<f32>> = Vec::new();
    while !shutdown.load(Ordering::Acquire) {
        while let Ok(sub) = sub_rx.try_recv() {
            if subs.iter().any(|existing| existing.pid == sub.pid) {
                continue;
            }
            history.push(VecDeque::new());
            subs.push(sub);
        }
        let mut got = pull_pcm(&mut main);
        for (index, sub) in subs.iter_mut().enumerate() {
            if pull_pcm(&mut sub.capture) {
                got = true;
            }
            while let Some(sample) = sub.capture.pcm.pop_front() {
                history[index].push_back(sample);
            }
            while history[index].len() > HISTORY_CAP {
                history[index].pop_front();
            }
        }
        while main.pcm.len() >= FRAME_SAMPLES {
            let mut frame: Vec<f32> = main.pcm.drain(..FRAME_SAMPLES).collect();
            for hist in &history {
                let samples: Vec<f32> = hist.iter().copied().collect();
                cancel_block(&mut frame, &samples);
            }
            if !encode_frame(&mut encoder, &frame, &opus_tx) {
                let _ = watcher.and_then(|thread| thread.join().ok());
                return;
            }
        }
        if !got {
            thread::sleep(Duration::from_millis(5));
        }
    }
    let _ = watcher.and_then(|thread| thread.join().ok());
}

fn watch_subtract(
    tokens: Vec<String>,
    initial_subtract: Vec<u32>,
    sub_tx: mpsc::Sender<LiveInclude>,
    shutdown: Arc<AtomicBool>,
) {
    let _ = wasapi::initialize_mta();
    let mut opened: HashSet<u32> = HashSet::new();
    for pid in initial_subtract {
        if shutdown.load(Ordering::Acquire) {
            return;
        }
        if let Some(capture) = open_include(pid) {
            opened.insert(pid);
            if sub_tx.send(LiveInclude { pid, capture }).is_err() {
                return;
            }
        }
    }
    while !shutdown.load(Ordering::Acquire) {
        for _ in 0..20 {
            if shutdown.load(Ordering::Acquire) {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
        let self_pid = std::process::id();
        let self_exe = current_exe_name();
        let (_, wanted) = subtract_plan(&process_snapshot(), &tokens, self_pid, &self_exe);
        for pid in wanted {
            if opened.contains(&pid) {
                continue;
            }
            if let Some(capture) = open_include(pid) {
                opened.insert(pid);
                if sub_tx.send(LiveInclude { pid, capture }).is_err() {
                    return;
                }
            }
        }
    }
}

fn cancel_block(frame: &mut [f32], history: &[f32]) -> bool {
    let width = frame.len();
    if width < 2 || width % 2 != 0 || history.len() < width {
        return false;
    }
    let exclude_energy: f32 = history.iter().map(|sample| sample * sample).sum();
    if exclude_energy < 1e-4 {
        return false;
    }
    let before: f32 = frame.iter().map(|sample| sample * sample).sum();
    if before < 1e-5 {
        return false;
    }
    let max_off = (history.len() - width) / 2;
    let step = 8usize;
    let mut best_off = 0usize;
    let mut best_after = f32::MAX;
    let mut best_gain = 0.0f32;
    let mut offset = 0usize;
    while offset <= max_off {
        let (after, gain) = residual_gain(frame, history, offset * 2);
        if after < best_after {
            best_after = after;
            best_off = offset;
            best_gain = gain;
        }
        if step == 0 || offset > max_off.saturating_sub(step) {
            break;
        }
        offset += step;
    }
    let fine_lo = best_off.saturating_sub(step);
    let fine_hi = (best_off + step).min(max_off);
    for offset in fine_lo..=fine_hi {
        let (after, gain) = residual_gain(frame, history, offset * 2);
        if after < best_after {
            best_after = after;
            best_off = offset;
            best_gain = gain;
        }
    }
    if best_gain <= 0.0 || best_after >= before * 0.8 {
        return false;
    }
    let start = best_off * 2;
    for index in 0..width {
        frame[index] = (frame[index] - best_gain * history[start + index]).clamp(-1.0, 1.0);
    }
    true
}

fn residual_gain(main: &[f32], exclude: &[f32], offset: usize) -> (f32, f32) {
    let mut dot = 0.0f32;
    let mut exclude_energy = 0.0f32;
    let mut main_energy = 0.0f32;
    for index in 0..main.len() {
        let sample = exclude[offset + index];
        let mixed = main[index];
        dot += mixed * sample;
        exclude_energy += sample * sample;
        main_energy += mixed * mixed;
    }
    if exclude_energy < 1e-8 || dot <= 0.0 {
        return (main_energy, 0.0);
    }
    let gain = (dot / exclude_energy).clamp(0.0, 1.25);
    let mut after = 0.0f32;
    for index in 0..main.len() {
        let delta = main[index] - gain * exclude[offset + index];
        after += delta * delta;
    }
    (after, gain)
}

#[cfg(test)]
mod tests {
    use super::{cancel_block, subtract_plan, Proc};

    fn proc(pid: u32, ppid: u32, exe: &str) -> Proc {
        Proc {
            pid,
            ppid,
            exe: exe.into(),
        }
    }

    #[test]
    fn continuous_mix_excludes_self_and_cancels_discord_only() {
        let procs = vec![
            proc(10, 1, "Discord.exe"),
            proc(11, 10, "Discord.exe"),
            proc(12, 1, "DiscordPTB.exe"),
            proc(20, 1, "goDrinking.exe"),
            proc(21, 20, "golive-video.exe"),
            proc(30, 1, "chrome.exe"),
        ];
        let tokens = vec!["Discord".into(), "goDrinking".into(), "golive-video.exe".into()];
        let (engine_self, subtract) = subtract_plan(&procs, &tokens, 20, "goDrinking.exe");
        assert!(engine_self);
        assert_eq!(subtract, vec![10, 12]);
    }

    #[test]
    fn empty_tokens_exclude_only_self() {
        let procs = vec![proc(20, 1, "goDrinking.exe"), proc(10, 1, "Discord.exe")];
        let (engine_self, subtract) = subtract_plan(&procs, &[], 20, "goDrinking.exe");
        assert!(engine_self);
        assert!(subtract.is_empty());
    }

    #[test]
    fn cancel_block_removes_a_delayed_copy_without_touching_silence() {
        let frame = vec![0.5, -0.25, 0.5, -0.25];
        let mut history = vec![0.0; 8];
        history[4] = 0.5;
        history[5] = -0.25;
        history[6] = 0.5;
        history[7] = -0.25;
        let mut out = frame.clone();
        assert!(cancel_block(&mut out, &history));
        assert!(out.iter().all(|sample| sample.abs() < 0.01), "{out:?}");
        let mut quiet = vec![0.0; 4];
        assert!(!cancel_block(&mut quiet, &history));
    }
}
