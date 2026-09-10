//! Display enumerate + DXGI Desktop Duplication pump.

use golive_platform::{
    BgraFrame, CaptureConfig, CapturePacket, PlatformError, SourceInfo, SourceKind,
};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication,
    IDXGIResource, DXGI_OUTPUT_DESC,
};

use crate::copy::{gate_open, initial_last_ns, interval_ns, now_ns};
use crate::d3d::{create_device, texture_to_bgra, Readback};
use crate::map::{denied, is_access_lost, is_wait_timeout, map_windows};

pub fn enumerate_displays() -> Result<Vec<SourceInfo>, PlatformError> {
    let factory: IDXGIFactory1 =
        unsafe { CreateDXGIFactory1() }.map_err(|e| map_windows(&e))?;
    let mut out = Vec::new();
    let mut adapter_index = 0u32;
    loop {
        let adapter: IDXGIAdapter1 = match unsafe { factory.EnumAdapters1(adapter_index) } {
            Ok(adapter) => adapter,
            Err(_) => break,
        };
        adapter_index += 1;
        let mut output_index = 0u32;
        loop {
            let output = match unsafe { adapter.EnumOutputs(output_index) } {
                Ok(output) => output,
                Err(_) => break,
            };
            output_index += 1;
            let desc = match unsafe { output.GetDesc() } {
                Ok(desc) => desc,
                Err(_) => continue,
            };
            if !desc.AttachedToDesktop.as_bool() {
                continue;
            }
            let id = device_name(&desc);
            if id.is_empty() {
                continue;
            }
            let rect = desc.DesktopCoordinates;
            let w = rect.right.saturating_sub(rect.left).max(0) as u32;
            let h = rect.bottom.saturating_sub(rect.top).max(0) as u32;
            out.push(SourceInfo {
                kind: SourceKind::Display,
                id,
                name: format!("Display {} · {w}x{h}", out.len() + 1),
                w,
                h,
            });
        }
    }
    Ok(out)
}

fn device_name(desc: &DXGI_OUTPUT_DESC) -> String {
    let end = desc
        .DeviceName
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(desc.DeviceName.len());
    String::from_utf16_lossy(&desc.DeviceName[..end])
}

fn find_output(id: &str) -> Result<(IDXGIAdapter1, IDXGIOutput1), PlatformError> {
    let factory: IDXGIFactory1 =
        unsafe { CreateDXGIFactory1() }.map_err(|e| map_windows(&e))?;
    let mut adapter_index = 0u32;
    loop {
        let adapter: IDXGIAdapter1 = match unsafe { factory.EnumAdapters1(adapter_index) } {
            Ok(adapter) => adapter,
            Err(_) => break,
        };
        adapter_index += 1;
        let mut output_index = 0u32;
        loop {
            let output = match unsafe { adapter.EnumOutputs(output_index) } {
                Ok(output) => output,
                Err(_) => break,
            };
            output_index += 1;
            let desc = match unsafe { output.GetDesc() } {
                Ok(desc) => desc,
                Err(_) => continue,
            };
            if device_name(&desc) == id {
                let output1: IDXGIOutput1 = output.cast().map_err(|e| map_windows(&e))?;
                return Ok((adapter, output1));
            }
        }
    }
    Err(PlatformError::SourceGone { id: id.to_owned() })
}

fn open_duplication(
    adapter: &IDXGIAdapter1,
    output: &IDXGIOutput1,
) -> Result<(ID3D11Device, ID3D11DeviceContext, IDXGIOutputDuplication), PlatformError> {
    let (device, context) = create_device(Some(adapter))?;
    let dup = unsafe { output.DuplicateOutput(&device) }.map_err(|e| map_windows(&e))?;
    Ok((device, context, dup))
}

pub fn thumbnail_display(id: &str) -> Result<BgraFrame, PlatformError> {
    let (adapter, output) = find_output(id)?;
    let (device, context, dup) = open_duplication(&adapter, &output)?;
    match grab_one(&device, &context, &dup, 2000) {
        Ok(frame) => {
            let _ = unsafe { dup.ReleaseFrame() };
            Ok(frame)
        }
        Err(error) => {
            let _ = unsafe { dup.ReleaseFrame() };
            Err(error)
        }
    }
}

fn grab_one(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    dup: &IDXGIOutputDuplication,
    timeout_ms: u32,
) -> Result<BgraFrame, PlatformError> {
    let mut info = windows::Win32::Graphics::Dxgi::DXGI_OUTDUPL_FRAME_INFO::default();
    let mut resource: Option<IDXGIResource> = None;
    unsafe { dup.AcquireNextFrame(timeout_ms, &mut info, &mut resource) }.map_err(|e| {
        if is_wait_timeout(e.code().0) {
            PlatformError::Internal("thumbnail vazio".into())
        } else {
            map_windows(&e)
        }
    })?;
    let resource = resource.ok_or_else(|| PlatformError::Internal("thumbnail vazio".into()))?;
    let tex: ID3D11Texture2D = resource.cast().map_err(|e| map_windows(&e))?;
    texture_to_bgra(device, context, &tex)
}

pub fn run_display(
    id: String,
    config: CaptureConfig,
    frame_tx: SyncSender<CapturePacket>,
    stop_flag: std::sync::Arc<AtomicBool>,
    error_slot: std::sync::Arc<std::sync::Mutex<Option<PlatformError>>>,
    ready_tx: std::sync::mpsc::Sender<Result<(), PlatformError>>,
) {
    let fail = |error: PlatformError| {
        if let Ok(mut guard) = error_slot.lock() {
            *guard = Some(error.clone());
        }
        let _ = ready_tx.send(Err(error));
    };
    let (adapter, output) = match find_output(&id) {
        Ok(found) => found,
        Err(error) => {
            fail(error);
            return;
        }
    };
    let opened = match open_duplication(&adapter, &output) {
        Ok(opened) => opened,
        Err(error) => {
            fail(error);
            return;
        }
    };
    let (device, context, dup) = opened;
    let mut dup = Some(dup);
    let mut readback = Readback::new(device.clone(), context);
    let applied = golive_platform::capture_config_for(config.width, config.height, config.fps);
    eprintln!(
        "golive: capture {}x{}@{}fps (display)",
        applied.width, applied.height, applied.fps
    );
    let interval = interval_ns(applied.fps);
    let last_ns = AtomicU64::new(initial_last_ns(now_ns(), interval));
    let _ = ready_tx.send(Ok(()));
    while !stop_flag.load(Ordering::Acquire) {
        let mut info = windows::Win32::Graphics::Dxgi::DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        match unsafe { dup.as_ref().unwrap().AcquireNextFrame(100, &mut info, &mut resource) } {
            Ok(()) => {
                let now = now_ns();
                if gate_open(last_ns.load(Ordering::Relaxed), now, interval) {
                    if let Some(resource) = resource {
                        if let Ok(tex) = resource.cast::<ID3D11Texture2D>() {
                            if let Ok(frame) = readback.texture_to_bgra(&tex) {
                                let _ = frame_tx.try_send(CapturePacket::Cpu(frame));
                                last_ns.store(
                                    golive_platform::cadence::advance_capture_clock(
                                        last_ns.load(Ordering::Relaxed), now, interval,
                                    ),
                                    Ordering::Relaxed,
                                );
                            }
                        }
                    }
                }
                let _ = unsafe { dup.as_ref().unwrap().ReleaseFrame() };
            }
            Err(e) if is_wait_timeout(e.code().0) => continue,
            Err(e) if is_access_lost(e.code().0) => {
                match crate::resource::replace_after_drop(&mut dup, || {
                    unsafe { output.DuplicateOutput(&device) }.map_err(|e| map_windows(&e))
                }) {
                    Ok(()) => {},
                    Err(error) => {
                        if let Ok(mut guard) = error_slot.lock() {
                            *guard = Some(error);
                        }
                        return;
                    }
                }
            }
            Err(e) => {
                if let Ok(mut guard) = error_slot.lock() {
                    *guard = Some(map_windows(&e));
                }
                return;
            }
        }
    }
}

pub fn empty_is_denied(list: &[SourceInfo]) -> Result<Vec<SourceInfo>, PlatformError> {
    if list.is_empty() {
        Err(denied())
    } else {
        Ok(list.to_vec())
    }
}
