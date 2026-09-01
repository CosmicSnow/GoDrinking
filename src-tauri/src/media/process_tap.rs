//! macOS 14.2+ system-audio capture with per-app exclusion.

use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[cfg(target_os = "macos")]
use core_foundation::array::CFArray;
#[cfg(target_os = "macos")]
use core_foundation::base::{CFType, TCFType};
#[cfg(target_os = "macos")]
use core_foundation::boolean::CFBoolean;
#[cfg(target_os = "macos")]
use core_foundation::dictionary::CFDictionary;
#[cfg(target_os = "macos")]
use core_foundation::string::CFString;
#[cfg(target_os = "macos")]
use objc2::runtime::AnyObject;
#[cfg(target_os = "macos")]
use objc2::msg_send;
#[cfg(target_os = "macos")]
use objc2_foundation::{NSArray, NSNumber, NSString, NSUUID};

pub(crate) struct EncodedAudioPacket {
    pub(crate) data: Vec<u8>,
    pub(crate) duration: Duration,
}

pub(crate) struct ProcessTap {
    shutdown: Arc<AtomicBool>,
    _capture: Option<JoinHandle<()>>,
    #[cfg(target_os = "macos")]
    _native: Option<NativeTap>,
}

#[cfg(target_os = "macos")]
struct NativeTap {
    tap_id: u32,
    aggregate_id: u32,
    proc_id: *mut c_void,
}

#[cfg(target_os = "macos")]
unsafe impl Send for NativeTap {}

impl ProcessTap {
    /// Starts a process tap that writes encoded packets into `audio_tx`.
    /// The engine owns the Opus channel so a tap can be recreated against the
    /// same sender when exclusions change mid-session.
    pub(crate) fn start(
        excluded_bundle_ids: &[String],
        audio_tx: SyncSender<EncodedAudioPacket>,
    ) -> Result<Self, String> {
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (excluded_bundle_ids, audio_tx);
            return Err("process taps are unavailable on this platform".into());
        }
        #[cfg(target_os = "macos")]
        {
            start_macos(excluded_bundle_ids, audio_tx)
        }
    }
}

impl Drop for ProcessTap {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        #[cfg(target_os = "macos")]
        if let Some(native) = self._native.take() {
            unsafe {
                if !native.proc_id.is_null() {
                    AudioDeviceStop(native.aggregate_id, native.proc_id);
                    AudioDeviceDestroyIOProcID(native.aggregate_id, native.proc_id);
                }
                AudioHardwareDestroyAggregateDevice(native.aggregate_id);
                AudioHardwareDestroyProcessTap(native.tap_id);
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn start_macos(
    excluded_bundle_ids: &[String],
    opus_tx: SyncSender<EncodedAudioPacket>,
) -> Result<ProcessTap, String> {
    let mut process_objects = process_objects_for_bundles(excluded_bundle_ids);
    if let Ok(self_object) = translate_pid(std::process::id() as i32) {
        process_objects.push(self_object);
    }
    process_objects.sort_unstable();
    process_objects.dedup();

    eprintln!(
        "[goDrinking] creating process tap excluding {} Core Audio process object(s)",
        process_objects.len()
    );
    let description = tap_description(&process_objects)?;
    let mut tap_id = 0_u32;
    let status = unsafe { AudioHardwareCreateProcessTap(description, &mut tap_id) };
    if status != 0 {
        return Err(format!("AudioHardwareCreateProcessTap failed ({status})"));
    }
    let tap_uid = tap_uuid(description)?;
    let aggregate = aggregate_description(&tap_uid)?;
    let mut aggregate_id = 0_u32;
    let status = unsafe {
        AudioHardwareCreateAggregateDevice(aggregate.as_concrete_TypeRef().cast(), &mut aggregate_id)
    };
    if status != 0 {
        unsafe { AudioHardwareDestroyProcessTap(tap_id) };
        return Err(format!("AudioHardwareCreateAggregateDevice failed ({status})"));
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
        return Err(format!("AudioDeviceCreateIOProcID failed ({status})"));
    }
    let status = unsafe { AudioDeviceStart(aggregate_id, proc_id) };
    if status != 0 {
        unsafe {
            AudioDeviceDestroyIOProcID(aggregate_id, proc_id);
            drop(Box::from_raw(context));
            AudioHardwareDestroyAggregateDevice(aggregate_id);
            AudioHardwareDestroyProcessTap(tap_id);
        }
        return Err(format!("AudioDeviceStart failed ({status})"));
    }

    let shutdown = Arc::new(AtomicBool::new(false));
    let worker_shutdown = Arc::clone(&shutdown);
    let capture = thread::Builder::new()
        .name("godrinking-audio-opus".into())
        .spawn(move || opus_loop(pcm_rx, opus_tx, worker_shutdown))
        .map_err(|error| error.to_string())?;

    Ok(ProcessTap {
        shutdown,
        _capture: Some(capture),
        _native: Some(NativeTap {
            tap_id,
            aggregate_id,
            proc_id,
        }),
    })
}

#[cfg(target_os = "macos")]
fn opus_loop(
    pcm_rx: Receiver<Vec<f32>>,
    opus_tx: SyncSender<EncodedAudioPacket>,
    shutdown: Arc<AtomicBool>,
) {
    // Voip application mode keeps latency low for 20 ms stereo 48 kHz frames.
    // An init failure must not fail the session: the worker simply exits and
    // the tap keeps running without encoded output.
    let Ok(mut encoder) = opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Voip)
    else {
        eprintln!("[goDrinking] Opus encoder init failed; system audio continues without encoding");
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
                    if opus_tx
                        .try_send(EncodedAudioPacket {
                            data: output,
                            duration: Duration::from_millis(20),
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn tap_description(process_objects: &[u32]) -> Result<*mut AnyObject, String> {
    let cls = objc2::runtime::AnyClass::get(c"CATapDescription")
        .ok_or_else(|| "CATapDescription is unavailable (macOS 14.2+ required)".to_owned())?;
    let numbers: Vec<objc2::rc::Retained<NSNumber>> = process_objects
        .iter()
        .map(|id| NSNumber::new_u32(*id))
        .collect();
    let array = NSArray::from_retained_slice(&numbers);
    let allocated: *mut AnyObject = unsafe { msg_send![cls, alloc] };
    let description: *mut AnyObject =
        unsafe { msg_send![allocated, initStereoGlobalTapButExcludeProcesses: &*array] };
    if description.is_null() {
        return Err("failed to create CATapDescription".into());
    }
    let name = NSString::from_str("goDrinking System Audio Tap");
    unsafe {
        let _: () = msg_send![description, setName: &*name];
        let _: () = msg_send![description, setPrivate: true];
        let _: () = msg_send![description, setExclusive: true];
        let _: () = msg_send![description, setMixdown: true];
        // CATapUnmuted: excluded apps still play locally; they just stay out of the tap.
        let _: () = msg_send![description, setMuteBehavior: 0_isize];
    }
    Ok(description)
}

#[cfg(target_os = "macos")]
fn tap_uuid(description: *mut AnyObject) -> Result<CFString, String> {
    let uuid: *mut NSUUID = unsafe { msg_send![description, UUID] };
    if uuid.is_null() {
        return Err("process tap UUID is missing".into());
    }
    let uuid_string: *mut objc2_foundation::NSString = unsafe { msg_send![uuid, UUIDString] };
    if uuid_string.is_null() {
        return Err("process tap UUID string is missing".into());
    }
    Ok(CFString::new(&unsafe { &*uuid_string }.to_string()))
}

#[cfg(target_os = "macos")]
fn aggregate_description(tap_uid: &CFString) -> Result<CFDictionary<CFString, CFType>, String> {
    // Tap-only aggregate: do not list the default output as a subdevice.
    // Including it mixes the full hardware output (every app) into the IOProc,
    // so process exclusions on the tap have no effect.
    let name = CFString::new("goDrinking System Audio Tap");
    let uid = CFString::new(&format!("godrinking-tap-{}", std::process::id()));
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

/// Case-insensitive exclusion matcher. A selected token excludes an app when
/// the token equals or is contained in the app's name or bundle identifier.
/// This lets "Discord" or "com.hnc.Discord" exclude every Discord helper PID
/// (e.g. "Discord Helper (Renderer)" with bundle "com.hnc.Discord.helper").
pub(crate) fn app_excluded_by_token(name: &str, bundle_id: Option<&str>, token: &str) -> bool {
    let token = token.trim().to_ascii_lowercase();
    if token.is_empty() {
        return false;
    }
    if name.to_ascii_lowercase().contains(&token) {
        return true;
    }
    bundle_id
        .map(|bundle| bundle.to_ascii_lowercase().contains(&token))
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
fn process_objects_for_bundles(bundle_ids: &[String]) -> Vec<u32> {
    let matched: Vec<_> = running_applications()
        .into_iter()
        .filter(|app| {
            bundle_ids
                .iter()
                .any(|wanted| app_excluded_by_token(&app.name, app.bundle_id.as_deref(), wanted))
        })
        .collect();
    eprintln!(
        "[goDrinking] audio exclusion matched {} running app(s) for {} token(s)",
        matched.len(),
        bundle_ids.len()
    );
    let mut objects: Vec<u32> = matched
        .iter()
        .filter_map(|app| match translate_pid(app.pid) {
            Ok(object) => Some(object),
            Err(error) => {
                eprintln!(
                    "[goDrinking] audio exclusion matched \"{}\" (bundle {:?}, pid {}) but TranslatePID failed: {error}",
                    app.name, app.bundle_id, app.pid
                );
                None
            }
        })
        .collect();
    let wanted_pids: Vec<i32> = matched.iter().map(|app| app.pid).collect();
    for (object, pid) in audio_process_objects() {
        if objects.contains(&object) {
            continue;
        }
        let name = process_name(pid).unwrap_or_default();
        let listed = wanted_pids.contains(&pid);
        let named = bundle_ids
            .iter()
            .any(|wanted| app_excluded_by_token(&name, None, wanted));
        if listed || named {
            eprintln!(
                "[goDrinking] audio exclusion added process object {object} pid={pid} name={name:?}"
            );
            objects.push(object);
        }
    }
    objects
}

#[cfg(target_os = "macos")]
fn process_name(pid: i32) -> Option<String> {
    let mut buf = [0_u8; 256];
    let len = unsafe { proc_name(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if len <= 0 {
        return None;
    }
    Some(String::from_utf8_lossy(&buf[..len as usize]).into_owned())
}

#[cfg(target_os = "macos")]
fn audio_process_objects() -> Vec<(u32, i32)> {
    let mut size = 0_u32;
    let address = AudioObjectPropertyAddress {
        selector: fourcc(b"prs#"),
        scope: 0,
        element: 0,
    };
    let status = unsafe {
        AudioObjectGetPropertyDataSize(1, &address, 0, ptr::null(), &mut size)
    };
    if status != 0 || size == 0 {
        eprintln!("[goDrinking] ProcessObjectList size failed ({status}) size={size}");
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
        eprintln!("[goDrinking] ProcessObjectList read failed ({status})");
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

/// Best-effort check of whether a pid is currently running audio output.
///
/// Reads `kAudioProcessPropertyIsRunningOutput` (`'piro'`, UInt32, non-zero =
/// output running) from the pid's Core Audio process object(s). The pid is
/// translated with `id2p` first; the process-object list is scanned as a
/// fallback so a pid that owns several objects reports true if any of them is
/// running output. Never requires live audio to be present.
pub(crate) fn pid_is_emitting_output(pid: i32) -> bool {
    #[cfg(target_os = "macos")]
    {
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
    #[cfg(not(target_os = "macos"))]
    {
        let _ = pid;
        false
    }
}

#[cfg(target_os = "macos")]
fn translate_pid(pid: i32) -> Result<u32, String> {
    // kAudioHardwarePropertyTranslatePIDToProcessObject = 'id2p'
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
        return Err(format!("no Core Audio process object for pid {pid} ({status})"));
    }
    Ok(object_id)
}

#[cfg(target_os = "macos")]
fn audio_get<T>(object: u32, selector: u32, value: &mut T) -> Result<(), String> {
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
        return Err(format!("AudioObjectGetPropertyData {selector:#x} failed ({status})"));
    }
    Ok(())
}

pub fn running_applications() -> Vec<super::types::NativeRunningApp> {
    super::screen_capture_kit::ScreenCaptureKitAdapter::new()
        .enumerate_running_apps()
        .unwrap_or_default()
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct AudioObjectPropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct AudioBuffer {
    channels: u32,
    data_byte_size: u32,
    data: *mut c_void,
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct AudioBufferList {
    number_buffers: u32,
    buffers: [AudioBuffer; 8],
}

#[cfg(target_os = "macos")]
const fn fourcc(value: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*value)
}

#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
#[link(name = "proc", kind = "dylib")]
unsafe extern "C" {
    fn proc_name(pid: i32, buffer: *mut c_void, buffersize: u32) -> i32;
}

#[cfg(target_os = "macos")]
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

#[cfg(all(test, target_os = "macos"))]
fn collect_tap_pcm(excluded_tokens: &[String], duration: Duration) -> Result<Vec<f32>, String> {
    let mut process_objects = process_objects_for_bundles(excluded_tokens);
    if let Ok(self_object) = translate_pid(std::process::id() as i32) {
        process_objects.push(self_object);
    }
    process_objects.sort_unstable();
    process_objects.dedup();
    eprintln!(
        "[goDrinking] probe tap excluding {} object(s) tokens={excluded_tokens:?}",
        process_objects.len()
    );
    let description = tap_description(&process_objects)?;
    let mut tap_id = 0_u32;
    let status = unsafe { AudioHardwareCreateProcessTap(description, &mut tap_id) };
    if status != 0 {
        return Err(format!("AudioHardwareCreateProcessTap failed ({status})"));
    }
    let tap_uid = tap_uuid(description)?;
    let aggregate = aggregate_description(&tap_uid)?;
    let mut aggregate_id = 0_u32;
    let status = unsafe {
        AudioHardwareCreateAggregateDevice(aggregate.as_concrete_TypeRef().cast(), &mut aggregate_id)
    };
    if status != 0 {
        unsafe { AudioHardwareDestroyProcessTap(tap_id) };
        return Err(format!("AudioHardwareCreateAggregateDevice failed ({status})"));
    }
    let (pcm_tx, pcm_rx) = sync_channel::<Vec<f32>>(64);
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
        return Err(format!("AudioDeviceCreateIOProcID failed ({status})"));
    }
    let status = unsafe { AudioDeviceStart(aggregate_id, proc_id) };
    if status != 0 {
        unsafe {
            AudioDeviceDestroyIOProcID(aggregate_id, proc_id);
            drop(Box::from_raw(context));
            AudioHardwareDestroyAggregateDevice(aggregate_id);
            AudioHardwareDestroyProcessTap(tap_id);
        }
        return Err(format!("AudioDeviceStart failed ({status})"));
    }
    let deadline = std::time::Instant::now() + duration;
    let mut samples = Vec::new();
    while std::time::Instant::now() < deadline {
        if let Ok(chunk) = pcm_rx.recv_timeout(Duration::from_millis(20)) {
            samples.extend(chunk);
        }
    }
    unsafe {
        AudioDeviceStop(aggregate_id, proc_id);
        AudioDeviceDestroyIOProcID(aggregate_id, proc_id);
        AudioHardwareDestroyAggregateDevice(aggregate_id);
        AudioHardwareDestroyProcessTap(tap_id);
        drop(Box::from_raw(context));
    }
    Ok(samples)
}

#[cfg(all(test, target_os = "macos"))]
fn tone_energy(samples: &[f32], sample_rate: f32, freq: f32) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let omega = 2.0 * std::f32::consts::PI * freq / sample_rate;
    let coeff = 2.0 * omega.cos();
    let mut s0 = 0.0_f32;
    let mut s1 = 0.0_f32;
    let mut s2 = 0.0_f32;
    for sample in samples.iter().step_by(2) {
        s0 = sample + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
    (power / (samples.len() as f32 / 2.0).max(1.0)).sqrt()
}

#[cfg(all(test, target_os = "macos"))]
fn write_tone_wav(path: &std::path::Path, freq: f32, seconds: f32) -> std::io::Result<()> {
    let rate = 44_100_u32;
    let frames = (seconds * rate as f32) as usize;
    let mut data = Vec::with_capacity(frames * 2);
    for i in 0..frames {
        let t = i as f32 / rate as f32;
        let gate = if (t * 4.0).fract() < 0.5 { 1.0 } else { 0.0 };
        let sample = (t * freq * 2.0 * std::f32::consts::PI).sin() * 0.35 * gate;
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

#[cfg(test)]
mod tests {
    use super::app_excluded_by_token;

    #[test]
    fn discord_name_and_helper_bundles_all_match() {
        // Selected by display name.
        assert!(app_excluded_by_token("Discord", None, "Discord"));
        assert!(app_excluded_by_token("Discord Helper (Renderer)", None, "Discord"));
        // Selected by bundle id.
        assert!(app_excluded_by_token("Discord", Some("com.hnc.Discord"), "com.hnc.Discord"));
        assert!(app_excluded_by_token(
            "Discord Helper",
            Some("com.hnc.Discord.helper"),
            "com.hnc.Discord"
        ));
        // Case-insensitive on both sides.
        assert!(app_excluded_by_token("discord", Some("COM.HNC.DISCORD"), "Discord"));
        assert!(app_excluded_by_token("Discord Helper", Some("com.hnc.Discord.helper"), "discord"));
    }

    #[test]
    fn unrelated_apps_do_not_match() {
        assert!(!app_excluded_by_token("Safari", Some("com.apple.Safari"), "Discord"));
        assert!(!app_excluded_by_token("Google Chrome", Some("com.google.Chrome"), "discord"));
        assert!(!app_excluded_by_token("Slack", Some("com.tinyspeck.slackmacgap"), "Discord"));
        assert!(!app_excluded_by_token("Discord", Some("com.hnc.Discord"), ""));
        assert!(!app_excluded_by_token("Discord", Some("com.hnc.Discord"), "   "));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn translate_pid_uses_id2p_not_idtp() {
        assert_ne!(super::fourcc(b"idtp"), super::fourcc(b"id2p"));
        assert_eq!(super::fourcc(b"id2p"), u32::from_be_bytes(*b"id2p"));
        super::translate_pid(std::process::id() as i32)
            .expect("current process should map to a Core Audio process object via id2p");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn piro_is_k_audio_process_property_is_running_output() {
        // kAudioProcessPropertyIsRunningOutput = 'piro' (UInt32, non-zero = running).
        assert_eq!(super::fourcc(b"piro"), u32::from_be_bytes(*b"piro"));
        assert_ne!(super::fourcc(b"piro"), super::fourcc(b"ppid"));
        assert_ne!(super::fourcc(b"piro"), super::fourcc(b"prs#"));
        // Invalid pids never report output without touching Core Audio.
        assert!(!super::pid_is_emitting_output(-1));
        assert!(!super::pid_is_emitting_output(0));
        // The test binary never plays audio, so its own pid must not report
        // running output. No live audio is required for this assertion.
        assert!(!super::pid_is_emitting_output(std::process::id() as i32));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn excluding_afplay_resolves_core_audio_process_object() {
        use super::{process_objects_for_bundles, write_tone_wav};
        use std::process::{Command, Stdio};
        use std::time::Duration;

        let wav = std::env::temp_dir().join("godrinking-probe-tone.wav");
        write_tone_wav(&wav, 1234.0, 4.0).expect("write probe wav");
        let mut player = Command::new("afplay")
            .arg(&wav)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("afplay should start");
        let player_pid = player.id() as i32;
        std::thread::sleep(Duration::from_millis(300));
        let translated = super::translate_pid(player_pid);
        let objects = process_objects_for_bundles(&["afplay".into()]);
        let _ = player.kill();
        let _ = player.wait();
        eprintln!(
            "[goDrinking] afplay pid={player_pid} translate={translated:?} exclude_objects={objects:?}"
        );
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
