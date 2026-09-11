//! Windows system-audio capture: WASAPI device loopback, with optional
//! process-loopback exclusion of one process tree. Video stays in the other
//! modules; this file is audio-only.

use golive_platform::{AudioApp, EncodedAudioPacket, PlatformError};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

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
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    let Ok(snapshot) = snapshot else {
        return Vec::new();
    };
    if snapshot.is_invalid() {
        return Vec::new();
    }
    let mut apps = Vec::new();
    let mut entry = PROCESSENTRY32W::default();
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut more = unsafe { Process32FirstW(snapshot, &mut entry) }.is_ok();
    let own_pid = std::process::id();
    while more {
        let len = entry
            .szExeFile
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(entry.szExeFile.len());
        let exe = String::from_utf16_lossy(&entry.szExeFile[..len]);
        let pid = entry.th32ProcessID;
        if pid != 0 && pid != own_pid && !exe.is_empty() {
            let name = exe
                .strip_suffix(".exe")
                .or_else(|| exe.strip_suffix(".EXE"))
                .unwrap_or(&exe)
                .to_owned();
            apps.push(AudioApp {
                name: name.clone(),
                id: exe,
                pid: pid as i32,
                emitting_audio: false,
            });
        }
        more = unsafe { Process32NextW(snapshot, &mut entry) }.is_ok();
    }
    let _ = unsafe { CloseHandle(snapshot) };
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
    if !excluded_tokens.is_empty() {
        if is_process_loopback_supported() {
            if let Some((name, pid)) = resolve_exclusion_root(excluded_tokens) {
                match wasapi::AudioClient::new_application_loopback_client(pid, false) {
                    Ok(client) => match spawn_loopback(client, opus_tx.clone(), "process") {
                        Ok(tap) => return Ok(tap),
                        Err(_) => {}
                    },
                    Err(_) => {}
                }
                let _ = name;
            }
        }
    }
    let _ = wasapi::initialize_mta();
    let enumerator = wasapi::DeviceEnumerator::new()
        .map_err(|error| PlatformError::Internal(format!("WASAPI enumerator: {error}")))?;
    let device = enumerator
        .get_default_device(&wasapi::Direction::Render)
        .map_err(|error| PlatformError::Internal(format!("WASAPI render device: {error}")))?;
    let client = device
        .get_iaudioclient()
        .map_err(|error| PlatformError::Internal(format!("WASAPI client: {error}")))?;
    spawn_loopback(client, opus_tx, "device")
}

fn resolve_exclusion_root(excluded_apps: &[String]) -> Option<(String, u32)> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    let snapshot: Vec<(u32, u32, String)> = unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return None;
        };
        if snapshot.is_invalid() {
            return None;
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
            out.push((
                entry.th32ProcessID,
                entry.th32ParentProcessID,
                String::from_utf16_lossy(&entry.szExeFile[..len]),
            ));
            more = Process32NextW(snapshot, &mut entry).is_ok();
        }
        let _ = CloseHandle(snapshot);
        out
    };
    let own_pid = std::process::id();
    let matches_token = |exe: &str, token: &str| {
        exe.eq_ignore_ascii_case(token)
            || exe.eq_ignore_ascii_case(&format!("{token}.exe"))
            || token.eq_ignore_ascii_case(exe.strip_suffix(".exe").unwrap_or(exe))
    };
    for token in excluded_apps
        .iter()
        .map(|item| item.trim())
        .filter(|item| !item.is_empty())
    {
        let mut pids: Vec<(u32, u32)> = snapshot
            .iter()
            .filter(|(_, _, exe)| matches_token(exe, token))
            .map(|(pid, ppid, _)| (*pid, *ppid))
            .filter(|(pid, _)| *pid != own_pid)
            .collect();
        if pids.is_empty() {
            continue;
        }
        pids.sort_unstable();
        pids.dedup();
        let roots: Vec<u32> = pids
            .iter()
            .filter(|(_, ppid)| !pids.iter().any(|(other, _)| other == ppid))
            .map(|(pid, _)| *pid)
            .collect();
        let root = roots.into_iter().next().unwrap_or(pids[0].0);
        return Some((token.to_owned(), root));
    }
    None
}

fn spawn_loopback(
    mut client: wasapi::AudioClient,
    opus_tx: SyncSender<EncodedAudioPacket>,
    kind: &str,
) -> Result<AudioTap, PlatformError> {
    let desired = wasapi::WaveFormat::new(32, 32, &wasapi::SampleType::Float, 48_000, 2, None);
    let mode = wasapi::StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: 200_000,
    };
    client
        .initialize_client(&desired, &wasapi::Direction::Capture, &mode)
        .map_err(|error| PlatformError::Internal(format!("WASAPI {kind} init: {error}")))?;
    let event = client
        .set_get_eventhandle()
        .map_err(|error| PlatformError::Internal(format!("WASAPI event: {error}")))?;
    let capture = client
        .get_audiocaptureclient()
        .map_err(|error| PlatformError::Internal(format!("WASAPI capture: {error}")))?;
    client
        .start_stream()
        .map_err(|error| PlatformError::Internal(format!("WASAPI start: {error}")))?;

    let shutdown = Arc::new(AtomicBool::new(false));
    let worker_shutdown = Arc::clone(&shutdown);
    let loopback = WasapiLoopback {
        _client: client,
        capture,
        event,
    };
    let thread = thread::Builder::new()
        .name("golive-audio-opus".into())
        .spawn(move || {
            let _ = wasapi_loop(loopback, opus_tx, worker_shutdown);
        })
        .map_err(|error| PlatformError::Internal(error.to_string()))?;

    Ok(AudioTap {
        shutdown,
        native: Some(WindowsAudioTap {
            thread: Some(thread),
        }),
    })
}

struct WasapiLoopback {
    _client: wasapi::AudioClient,
    capture: wasapi::AudioCaptureClient,
    event: wasapi::Handle,
}

unsafe impl Send for WasapiLoopback {}

fn wasapi_loop(
    loopback: WasapiLoopback,
    opus_tx: SyncSender<EncodedAudioPacket>,
    shutdown: Arc<AtomicBool>,
) {
    let Ok(mut encoder) = opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Voip)
    else {
        return;
    };
    let mut pcm = Vec::<f32>::new();
    let mut bytes = VecDeque::<u8>::new();
    while !shutdown.load(Ordering::Acquire) {
        if loopback.event.wait_for_event(100).is_err() {
            continue;
        }
        loop {
            let Ok(Some(frames)) = loopback.capture.get_next_packet_size() else {
                break;
            };
            if frames == 0 {
                break;
            }
            let Ok(_info) = loopback.capture.read_from_device_to_deque(&mut bytes) else {
                break;
            };
            while bytes.len() >= 8 {
                let mut frame = [0_u8; 8];
                for byte in frame.iter_mut() {
                    *byte = bytes.pop_front().expect("length checked above");
                }
                pcm.push(f32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]));
                pcm.push(f32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]]));
            }
            while pcm.len() >= 960 * 2 {
                let frame: Vec<f32> = pcm.drain(..960 * 2).collect();
                let mut output = vec![0_u8; 4000];
                match encoder.encode_float(&frame, &mut output) {
                    Ok(size) if size > 0 => {
                        output.truncate(size);
                        match opus_tx.try_send(EncodedAudioPacket {
                            data: output,
                            duration: Duration::from_millis(20),
                        }) {
                            Ok(()) | Err(TrySendError::Full(_)) => {}
                            Err(TrySendError::Disconnected(_)) => return,
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}
