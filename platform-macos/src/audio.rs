//! macOS system-audio capture with per-app exclusion (process taps, 14.2+).
//! Video capture stays in `lib.rs`; this file is audio-only.

use core_foundation::array::CFArray;
use core_foundation::base::{CFType, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::CFString;
use golive_platform::{app_excluded_by_token, AudioApp, EncodedAudioPacket, PlatformError};
use objc2::msg_send;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSRunningApplication, NSWorkspace};
use objc2_foundation::{NSArray, NSNumber, NSString, NSUUID};
use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub struct AudioTap {
    shutdown: Arc<AtomicBool>,
    _capture: Option<JoinHandle<()>>,
    native: Option<NativeTap>,
}

struct NativeTap {
    tap_id: u32,
    aggregate_id: u32,
    proc_id: *mut c_void,
    context: *mut SyncSender<Vec<f32>>,
}

unsafe impl Send for NativeTap {}

impl Drop for AudioTap {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(thread) = self._capture.take() {
            let _ = thread.join();
        }
        if let Some(native) = self.native.take() {
            unsafe {
                if !native.proc_id.is_null() {
                    AudioDeviceStop(native.aggregate_id, native.proc_id);
                    AudioDeviceDestroyIOProcID(native.aggregate_id, native.proc_id);
                }
                AudioHardwareDestroyAggregateDevice(native.aggregate_id);
                AudioHardwareDestroyProcessTap(native.tap_id);
                if !native.context.is_null() {
                    drop(Box::from_raw(native.context));
                }
            }
        }
    }
}

pub fn start_audio_tap(
    excluded_tokens: &[String],
    opus_tx: SyncSender<EncodedAudioPacket>,
) -> Result<AudioTap, PlatformError> {
    if !process_tap_available() {
        return Err(PlatformError::OsVersionTooOld {
            have: os_version_label(),
            need: "macOS 14.2",
        });
    }
    let mut process_objects = process_objects_for_bundles(excluded_tokens);
    if let Ok(self_object) = translate_pid(std::process::id() as i32) {
        process_objects.push(self_object);
    }
    process_objects.sort_unstable();
    process_objects.dedup();

    let description = tap_description(&process_objects)?;
    let mut tap_id = 0_u32;
    let status = unsafe { AudioHardwareCreateProcessTap(description, &mut tap_id) };
    if status != 0 {
        return Err(PlatformError::Internal(format!(
            "AudioHardwareCreateProcessTap ({status})"
        )));
    }
    let tap_uid = match tap_uuid(description) {
        Ok(uid) => uid,
        Err(error) => {
            unsafe { AudioHardwareDestroyProcessTap(tap_id) };
            return Err(error);
        }
    };
    let aggregate = match aggregate_description(&tap_uid) {
        Ok(value) => value,
        Err(error) => {
            unsafe { AudioHardwareDestroyProcessTap(tap_id) };
            return Err(error);
        }
    };
    let mut aggregate_id = 0_u32;
    let status = unsafe {
        AudioHardwareCreateAggregateDevice(aggregate.as_concrete_TypeRef().cast(), &mut aggregate_id)
    };
    if status != 0 {
        unsafe { AudioHardwareDestroyProcessTap(tap_id) };
        return Err(PlatformError::Internal(format!(
            "AudioHardwareCreateAggregateDevice ({status})"
        )));
    }

    let (pcm_tx, pcm_rx) = sync_channel::<Vec<f32>>(8);
    let context = Box::into_raw(Box::new(pcm_tx));
    let mut proc_id: *mut c_void = ptr::null_mut();
    let status = unsafe {
        AudioDeviceCreateIOProcID(aggregate_id, audio_io_proc, context.cast(), &mut proc_id)
    };
    if status != 0 {
        unsafe {
            drop(Box::from_raw(context));
            AudioHardwareDestroyAggregateDevice(aggregate_id);
            AudioHardwareDestroyProcessTap(tap_id);
        }
        return Err(PlatformError::Internal(format!(
            "AudioDeviceCreateIOProcID ({status})"
        )));
    }
    let status = unsafe { AudioDeviceStart(aggregate_id, proc_id) };
    if status != 0 {
        unsafe {
            AudioDeviceDestroyIOProcID(aggregate_id, proc_id);
            drop(Box::from_raw(context));
            AudioHardwareDestroyAggregateDevice(aggregate_id);
            AudioHardwareDestroyProcessTap(tap_id);
        }
        return Err(PlatformError::Internal(format!(
            "AudioDeviceStart ({status})"
        )));
    }

    let shutdown = Arc::new(AtomicBool::new(false));
    let worker_shutdown = Arc::clone(&shutdown);
    let capture = thread::Builder::new()
        .name("golive-audio-opus".into())
        .spawn(move || opus_loop(pcm_rx, opus_tx, worker_shutdown))
        .map_err(|error| PlatformError::Internal(error.to_string()))?;

    Ok(AudioTap {
        shutdown,
        _capture: Some(capture),
        native: Some(NativeTap {
            tap_id,
            aggregate_id,
            proc_id,
            context,
        }),
    })
}

pub fn list_audio_apps() -> Vec<AudioApp> {
    let apps = running_apps_from_workspace();
    if apps.is_empty() {
        running_apps_from_window_list()
    } else {
        apps
    }
}

fn process_tap_available() -> bool {
    let version = objc2_foundation::NSProcessInfo::processInfo().operatingSystemVersion();
    version.majorVersion > 14 || (version.majorVersion == 14 && version.minorVersion >= 2)
}

fn os_version_label() -> String {
    let version = objc2_foundation::NSProcessInfo::processInfo().operatingSystemVersion();
    format!("{}.{}.{}", version.majorVersion, version.minorVersion, version.patchVersion)
}

fn tap_description(process_objects: &[u32]) -> Result<*mut AnyObject, PlatformError> {
    let cls = objc2::runtime::AnyClass::get(c"CATapDescription").ok_or_else(|| {
        PlatformError::OsVersionTooOld {
            have: os_version_label(),
            need: "macOS 14.2",
        }
    })?;
    let numbers: Vec<objc2::rc::Retained<NSNumber>> =
        process_objects.iter().map(|id| NSNumber::new_u32(*id)).collect();
    let array = NSArray::from_retained_slice(&numbers);
    let allocated: *mut AnyObject = unsafe { msg_send![cls, alloc] };
    let description: *mut AnyObject =
        unsafe { msg_send![allocated, initStereoGlobalTapButExcludeProcesses: &*array] };
    if description.is_null() {
        return Err(PlatformError::Internal("CATapDescription".into()));
    }
    let name = NSString::from_str("GoLive System Audio Tap");
    unsafe {
        let _: () = msg_send![description, setName: &*name];
        let _: () = msg_send![description, setPrivate: true];
        let _: () = msg_send![description, setExclusive: true];
        let _: () = msg_send![description, setMixdown: true];
        let _: () = msg_send![description, setMuteBehavior: 0_isize];
    }
    Ok(description)
}

fn tap_uuid(description: *mut AnyObject) -> Result<CFString, PlatformError> {
    let uuid: *mut NSUUID = unsafe { msg_send![description, UUID] };
    if uuid.is_null() {
        return Err(PlatformError::Internal("tap UUID".into()));
    }
    let uuid_string: *mut objc2_foundation::NSString = unsafe { msg_send![uuid, UUIDString] };
    if uuid_string.is_null() {
        return Err(PlatformError::Internal("tap UUID string".into()));
    }
    Ok(CFString::new(&unsafe { &*uuid_string }.to_string()))
}

fn aggregate_description(tap_uid: &CFString) -> Result<CFDictionary<CFString, CFType>, PlatformError> {
    let name = CFString::new("GoLive System Audio Tap");
    let uid = CFString::new(&format!("golive-tap-{}", std::process::id()));
    let tap_entry = CFDictionary::from_CFType_pairs(&[
        (CFString::new("uid"), tap_uid.as_CFType()),
        (CFString::new("drift"), CFBoolean::true_value().as_CFType()),
    ]);
    let taps = CFArray::from_CFTypes(&[tap_entry]);
    Ok(CFDictionary::from_CFType_pairs(&[
        (CFString::new("name"), name.as_CFType()),
        (CFString::new("uid"), uid.as_CFType()),
        (CFString::new("private"), CFBoolean::true_value().as_CFType()),
        (CFString::new("stacked"), CFBoolean::false_value().as_CFType()),
        (CFString::new("tapautostart"), CFBoolean::true_value().as_CFType()),
        (CFString::new("taps"), taps.as_CFType()),
    ]))
}

fn process_objects_for_bundles(tokens: &[String]) -> Vec<u32> {
    let matched: Vec<_> = list_audio_apps()
        .into_iter()
        .filter(|app| {
            tokens
                .iter()
                .any(|wanted| app_excluded_by_token(&app.name, Some(app.id.as_str()), wanted))
        })
        .collect();
    let mut objects: Vec<u32> = matched
        .iter()
        .filter_map(|app| translate_pid(app.pid).ok())
        .collect();
    let wanted_pids: Vec<i32> = matched.iter().map(|app| app.pid).collect();
    for (object, pid) in audio_process_objects() {
        if objects.contains(&object) {
            continue;
        }
        let name = process_name(pid).unwrap_or_default();
        let listed = wanted_pids.contains(&pid);
        let named = tokens
            .iter()
            .any(|wanted| app_excluded_by_token(&name, None, wanted));
        if listed || named {
            objects.push(object);
        }
    }
    objects
}

fn process_name(pid: i32) -> Option<String> {
    let mut buf = [0_u8; 256];
    let len = unsafe { proc_name(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if len <= 0 {
        return None;
    }
    Some(String::from_utf8_lossy(&buf[..len as usize]).into_owned())
}

fn audio_process_objects() -> Vec<(u32, i32)> {
    let mut size = 0_u32;
    let address = AudioObjectPropertyAddress {
        selector: fourcc(b"prs#"),
        scope: 0,
        element: 0,
    };
    let status = unsafe { AudioObjectGetPropertyDataSize(1, &address, 0, ptr::null(), &mut size) };
    if status != 0 || size == 0 {
        return Vec::new();
    }
    let count = (size as usize) / std::mem::size_of::<u32>();
    let mut ids = vec![0_u32; count];
    let mut actual = size;
    let status = unsafe {
        AudioObjectGetPropertyData(
            1,
            &address,
            0,
            ptr::null(),
            &mut actual,
            ids.as_mut_ptr().cast(),
        )
    };
    if status != 0 {
        return Vec::new();
    }
    ids.into_iter()
        .filter_map(|object| {
            let mut pid = 0_i32;
            audio_get(object, fourcc(b"ppid"), &mut pid)
                .ok()
                .filter(|_| pid > 0)
                .map(|_| (object, pid))
        })
        .collect()
}

fn pid_is_emitting_output(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    let mut objects = Vec::new();
    if let Ok(object) = translate_pid(pid) {
        objects.push(object);
    }
    objects.extend(
        audio_process_objects()
            .into_iter()
            .filter(|(_, object_pid)| *object_pid == pid)
            .map(|(object, _)| object),
    );
    objects.sort_unstable();
    objects.dedup();
    objects.into_iter().any(|object| {
        let mut running = 0_u32;
        audio_get(object, fourcc(b"piro"), &mut running).is_ok() && running != 0
    })
}

fn translate_pid(pid: i32) -> Result<u32, PlatformError> {
    let address = AudioObjectPropertyAddress {
        selector: fourcc(b"id2p"),
        scope: 0,
        element: 0,
    };
    let mut object_id = 0_u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    let pid = pid;
    let status = unsafe {
        AudioObjectGetPropertyData(
            1,
            &address,
            std::mem::size_of::<i32>() as u32,
            (&pid as *const i32).cast(),
            &mut size,
            (&mut object_id as *mut u32).cast(),
        )
    };
    if status != 0 || object_id == 0 {
        return Err(PlatformError::Internal(format!(
            "TranslatePID ({status})"
        )));
    }
    Ok(object_id)
}

fn audio_get<T>(object: u32, selector: u32, value: &mut T) -> Result<(), PlatformError> {
    let address = AudioObjectPropertyAddress {
        selector,
        scope: 0,
        element: 0,
    };
    let mut size = std::mem::size_of::<T>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            &address,
            0,
            ptr::null(),
            &mut size,
            (value as *mut T).cast(),
        )
    };
    if status != 0 {
        return Err(PlatformError::Internal(format!(
            "AudioObjectGetPropertyData ({status})"
        )));
    }
    Ok(())
}

fn running_apps_from_workspace() -> Vec<AudioApp> {
    let workspace = NSWorkspace::sharedWorkspace();
    let apps: &NSArray<NSRunningApplication> = &workspace.runningApplications();
    let mut result = Vec::with_capacity(apps.count() as usize);
    for index in 0..apps.count() {
        let app = apps.objectAtIndex(index);
        let pid = app.processIdentifier();
        if pid <= 0 {
            continue;
        }
        let name = app
            .localizedName()
            .map(|name| name.to_string())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| format!("pid {pid}"));
        let bundle_id = app.bundleIdentifier().map(|bundle| bundle.to_string());
        let id = bundle_id.clone().filter(|value| !value.is_empty()).unwrap_or_else(|| name.clone());
        result.push(AudioApp {
            name,
            id,
            pid,
            emitting_audio: pid_is_emitting_output(pid),
        });
    }
    dedupe_apps_by_pid(result)
}

fn running_apps_from_window_list() -> Vec<AudioApp> {
    use core_foundation::number::CFNumber;
    const ON_SCREEN_ONLY: u32 = 1;
    let raw = unsafe { CGWindowListCopyWindowInfo(ON_SCREEN_ONLY, 0) };
    if raw.is_null() {
        return Vec::new();
    }
    let windows: CFArray<CFDictionary<CFString, CFType>> =
        unsafe { CFArray::wrap_under_create_rule(raw) };
    let owner_key = CFString::new("kCGWindowOwnerName");
    let pid_key = CFString::new("kCGWindowOwnerPID");
    let mut apps = Vec::new();
    for window in &windows {
        let pid = window
            .find(&pid_key)
            .and_then(|value| value.downcast::<CFNumber>())
            .and_then(|number| number.to_i64())
            .unwrap_or(0);
        if pid <= 0 {
            continue;
        }
        let name = window
            .find(&owner_key)
            .and_then(|value| value.downcast::<CFString>())
            .map(|value| value.to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| format!("pid {pid}"));
        apps.push(AudioApp {
            name: name.clone(),
            id: name,
            pid: pid as i32,
            emitting_audio: pid_is_emitting_output(pid as i32),
        });
    }
    dedupe_apps_by_pid(apps)
}

fn dedupe_apps_by_pid(apps: Vec<AudioApp>) -> Vec<AudioApp> {
    let mut by_pid: HashMap<i32, AudioApp> = HashMap::new();
    for app in apps {
        match by_pid.get_mut(&app.pid) {
            Some(existing) => {
                existing.emitting_audio = existing.emitting_audio || app.emitting_audio;
                if existing.id == existing.name && app.id != app.name {
                    existing.id = app.id;
                }
            }
            None => {
                by_pid.insert(app.pid, app);
            }
        }
    }
    let mut result: Vec<_> = by_pid.into_values().collect();
    result.sort_by(|left, right| {
        left.name
            .to_ascii_lowercase()
            .cmp(&right.name.to_ascii_lowercase())
    });
    result
}

fn opus_loop(
    pcm_rx: Receiver<Vec<f32>>,
    opus_tx: SyncSender<EncodedAudioPacket>,
    shutdown: Arc<AtomicBool>,
) {
    let Ok(mut encoder) = opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Voip)
    else {
        return;
    };
    let mut pending = Vec::<f32>::new();
    while !shutdown.load(Ordering::Acquire) {
        let Ok(samples) = pcm_rx.recv_timeout(Duration::from_millis(20)) else {
            continue;
        };
        pending.extend(samples);
        while pending.len() >= 960 * 2 {
            let frame: Vec<f32> = pending.drain(..960 * 2).collect();
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

#[repr(C)]
struct AudioObjectPropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

#[repr(C)]
struct AudioBuffer {
    channels: u32,
    data_byte_size: u32,
    data: *mut c_void,
}

#[repr(C)]
struct AudioBufferList {
    number_buffers: u32,
    buffers: [AudioBuffer; 8],
}

const fn fourcc(value: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*value)
}

#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {
    fn AudioHardwareCreateProcessTap(description: *mut AnyObject, out_tap_id: *mut u32) -> i32;
    fn AudioHardwareDestroyProcessTap(tap_id: u32) -> i32;
    fn AudioHardwareCreateAggregateDevice(
        description: core_foundation::dictionary::CFDictionaryRef,
        out_device_id: *mut u32,
    ) -> i32;
    fn AudioHardwareDestroyAggregateDevice(device_id: u32) -> i32;
    fn AudioDeviceCreateIOProcID(
        device_id: u32,
        proc: unsafe extern "C" fn(
            u32,
            *const c_void,
            *const AudioBufferList,
            *const c_void,
            *mut AudioBufferList,
            *const c_void,
            *mut c_void,
        ) -> i32,
        client_data: *mut c_void,
        out_proc_id: *mut *mut c_void,
    ) -> i32;
    fn AudioDeviceDestroyIOProcID(device_id: u32, proc_id: *mut c_void) -> i32;
    fn AudioDeviceStart(device_id: u32, proc_id: *mut c_void) -> i32;
    fn AudioDeviceStop(device_id: u32, proc_id: *mut c_void) -> i32;
    fn AudioObjectGetPropertyData(
        object_id: u32,
        address: *const AudioObjectPropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        data_size: *mut u32,
        data: *mut c_void,
    ) -> i32;
    fn AudioObjectGetPropertyDataSize(
        object_id: u32,
        address: *const AudioObjectPropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        data_size: *mut u32,
    ) -> i32;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGWindowListCopyWindowInfo(
        option: u32,
        relative_to_window: u32,
    ) -> core_foundation::array::CFArrayRef;
}

#[link(name = "proc", kind = "dylib")]
unsafe extern "C" {
    fn proc_name(pid: i32, buffer: *mut c_void, buffersize: u32) -> i32;
}

unsafe extern "C" fn audio_io_proc(
    _device: u32,
    _now: *const c_void,
    input: *const AudioBufferList,
    _input_time: *const c_void,
    _output: *mut AudioBufferList,
    _output_time: *const c_void,
    client: *mut c_void,
) -> i32 {
    if input.is_null() || client.is_null() {
        return 0;
    }
    let sender = unsafe { &*(client as *const SyncSender<Vec<f32>>) };
    let list = unsafe { &*input };
    if list.number_buffers == 0 {
        return 0;
    }
    let buffer = &list.buffers[0];
    if buffer.data.is_null() || buffer.data_byte_size == 0 {
        return 0;
    }
    let samples = buffer.data_byte_size as usize / std::mem::size_of::<f32>();
    let slice = unsafe { std::slice::from_raw_parts(buffer.data as *const f32, samples) };
    let _ = sender.try_send(slice.to_vec());
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translate_pid_uses_id2p_not_idtp() {
        assert_ne!(fourcc(b"idtp"), fourcc(b"id2p"));
        assert_eq!(fourcc(b"id2p"), u32::from_be_bytes(*b"id2p"));
        translate_pid(std::process::id() as i32)
            .expect("current process should map to a Core Audio process object via id2p");
    }

    #[test]
    fn piro_is_k_audio_process_property_is_running_output() {
        assert_eq!(fourcc(b"piro"), u32::from_be_bytes(*b"piro"));
        assert!(!pid_is_emitting_output(-1));
        assert!(!pid_is_emitting_output(0));
        assert!(!pid_is_emitting_output(std::process::id() as i32));
    }

    #[test]
    fn list_audio_apps_returns_named_processes() {
        let apps = list_audio_apps();
        assert!(
            apps.iter().any(|app| app.pid > 0 && !app.name.is_empty()),
            "workspace/window list should yield at least one named app"
        );
    }

    fn write_tone_wav(path: &std::path::Path, freq: f32, seconds: f32) -> std::io::Result<()> {
        let rate = 44_100_u32;
        let frames = (seconds * rate as f32) as usize;
        let mut data = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            let t = i as f32 / rate as f32;
            let sample = (t * freq * 2.0 * std::f32::consts::PI).sin() * 0.35;
            let int = (sample * i16::MAX as f32) as i16;
            data.extend_from_slice(&int.to_le_bytes());
        }
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        let data_len = data.len() as u32;
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&rate.to_le_bytes());
        bytes.extend_from_slice(&(rate * 2).to_le_bytes());
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&16_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        bytes.extend_from_slice(&data);
        std::fs::write(path, bytes)
    }

    #[test]
    fn excluding_afplay_resolves_core_audio_process_object() {
        let wav = std::env::temp_dir().join("golive-probe-tone.wav");
        write_tone_wav(&wav, 440.0, 3.0).expect("write probe wav");
        let mut player = std::process::Command::new("afplay")
            .arg(&wav)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("afplay should start");
        let player_pid = player.id() as i32;
        std::thread::sleep(Duration::from_millis(300));
        let translated = translate_pid(player_pid);
        let objects = process_objects_for_bundles(&["afplay".into()]);
        let _ = player.kill();
        let _ = player.wait();
        assert!(
            translated.is_ok(),
            "afplay pid should translate with id2p: {translated:?}"
        );
        assert!(
            !objects.is_empty(),
            "excluding token \"afplay\" should resolve at least one Core Audio process object"
        );
        assert!(
            objects.contains(&translated.unwrap()),
            "exclude list should include the afplay process object"
        );
    }
}
