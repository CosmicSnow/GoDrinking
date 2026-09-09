//! Window enumerate + Windows.Graphics.Capture pump.

use golive_platform::{
    BgraFrame, CaptureConfig, CapturePacket, PlatformError, SourceInfo, SourceKind,
};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;
use std::time::{Duration, Instant};
use windows::core::Interface;
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetAncestor, GetClassNameW, GetWindowLongW, GetWindowRect, GetWindowTextW,
    IsIconic, IsWindowVisible, GA_ROOT, GWL_EXSTYLE, GWL_STYLE, WS_CHILD, WS_EX_TOOLWINDOW,
};

use crate::copy::{gate_open, initial_last_ns, interval_ns, now_ns};
use crate::d3d::{create_device, texture_to_bgra};
use crate::map::map_windows;

pub fn enumerate_windows() -> Result<Vec<SourceInfo>, PlatformError> {
    let mut out: Vec<SourceInfo> = Vec::new();
    unsafe {
        EnumWindows(
            Some(enum_windows_proc),
            LPARAM(&mut out as *mut Vec<SourceInfo> as isize),
        )
        .map_err(|e| map_windows(&e))?;
    }
    Ok(out)
}

unsafe extern "system" fn enum_windows_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let out = unsafe { &mut *(lparam.0 as *mut Vec<SourceInfo>) };
    if let Some(info) = window_info(hwnd) {
        out.push(info);
    }
    true.into()
}

fn window_info(hwnd: HWND) -> Option<SourceInfo> {
    unsafe {
        if hwnd.0.is_null() || !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            return None;
        }
        if GetAncestor(hwnd, GA_ROOT) != hwnd {
            return None;
        }
        let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        if style & WS_CHILD.0 != 0 {
            return None;
        }
        let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        if ex & WS_EX_TOOLWINDOW.0 != 0 {
            return None;
        }
        if is_cloaked(hwnd) {
            return None;
        }
        let mut class = [0u16; 64];
        let class_len = GetClassNameW(hwnd, &mut class);
        if class_len > 0 {
            let class_name = String::from_utf16_lossy(&class[..class_len as usize]);
            if class_name == "Progman" || class_name == "WorkerW" || class_name == "Shell_TrayWnd"
            {
                return None;
            }
        }
        let mut rect = RECT::default();
        GetWindowRect(hwnd, &mut rect).ok()?;
        let w = rect.right.saturating_sub(rect.left).max(0) as u32;
        let h = rect.bottom.saturating_sub(rect.top).max(0) as u32;
        if w < 2 || h < 2 {
            return None;
        }
        let mut title = [0u16; 256];
        let n = GetWindowTextW(hwnd, &mut title);
        let name = if n > 0 {
            let text = String::from_utf16_lossy(&title[..n as usize]);
            if text.trim().is_empty() {
                "Janela sem título".into()
            } else {
                text
            }
        } else {
            "Janela sem título".into()
        };
        Some(SourceInfo {
            kind: SourceKind::Window,
            id: hwnd_id(hwnd),
            name,
            w: 0,
            h: 0,
        })
    }
}

fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked: u32 = 0;
    let result = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut core::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
        )
    };
    result.is_ok() && cloaked != 0
}

fn hwnd_id(hwnd: HWND) -> String {
    (hwnd.0 as usize).to_string()
}

fn parse_hwnd(id: &str) -> Result<HWND, PlatformError> {
    let n: usize = id
        .trim()
        .parse()
        .map_err(|_| PlatformError::InvalidSource {
            reason: "id de fonte inválido",
        })?;
    Ok(HWND(n as *mut core::ffi::c_void))
}

fn winrt_device(device: &ID3D11Device) -> Result<IDirect3DDevice, PlatformError> {
    let dxgi: IDXGIDevice = device.cast().map_err(|e| map_windows(&e))?;
    let inspectable =
        unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }.map_err(|e| map_windows(&e))?;
    inspectable.cast().map_err(|e| map_windows(&e))
}

fn capture_item(hwnd: HWND) -> Result<GraphicsCaptureItem, PlatformError> {
    let interop =
        windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
            .map_err(|e| map_windows(&e))?;
    unsafe { interop.CreateForWindow(hwnd) }.map_err(|e| {
        if e.code() == windows::Win32::Foundation::E_ACCESSDENIED {
            crate::map::denied()
        } else {
            PlatformError::SourceGone {
                id: hwnd_id(hwnd),
            }
        }
    })
}

fn texture_from_frame(
    frame: &windows::Graphics::Capture::Direct3D11CaptureFrame,
) -> Result<ID3D11Texture2D, PlatformError> {
    let surface = frame.Surface().map_err(|e| map_windows(&e))?;
    let access: IDirect3DDxgiInterfaceAccess = surface.cast().map_err(|e| map_windows(&e))?;
    unsafe { access.GetInterface::<ID3D11Texture2D>() }.map_err(|e| map_windows(&e))
}

fn grab_wgc_frame(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    pool: &Direct3D11CaptureFramePool,
) -> Option<BgraFrame> {
    let frame = pool.TryGetNextFrame().ok()?;
    let tex = texture_from_frame(&frame).ok()?;
    texture_to_bgra(device, context, &tex).ok()
}

pub fn thumbnail_window(id: &str) -> Result<BgraFrame, PlatformError> {
    let hwnd = parse_hwnd(id)?;
    let (device, context) = create_device(None)?;
    let winrt = winrt_device(&device)?;
    let item = capture_item(hwnd)?;
    let size = item.Size().map_err(|e| map_windows(&e))?;
    if size.Width <= 0 || size.Height <= 0 {
        return Err(PlatformError::SourceGone { id: id.to_owned() });
    }
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &winrt,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        2,
        size,
    )
    .map_err(|e| map_windows(&e))?;
    let session = pool.CreateCaptureSession(&item).map_err(|e| map_windows(&e))?;
    session.StartCapture().map_err(|e| map_windows(&e))?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut grabbed = None;
    while Instant::now() < deadline {
        if let Some(frame) = grab_wgc_frame(&device, &context, &pool) {
            grabbed = Some(frame);
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = session.Close();
    let _ = pool.Close();
    grabbed.ok_or_else(|| PlatformError::Internal("thumbnail vazio".into()))
}

pub fn run_window(
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
    let hwnd = match parse_hwnd(&id) {
        Ok(hwnd) => hwnd,
        Err(error) => {
            fail(error);
            return;
        }
    };
    let (device, context) = match create_device(None) {
        Ok(pair) => pair,
        Err(error) => {
            fail(error);
            return;
        }
    };
    let winrt = match winrt_device(&device) {
        Ok(dev) => dev,
        Err(error) => {
            fail(error);
            return;
        }
    };
    let item = match capture_item(hwnd) {
        Ok(item) => item,
        Err(error) => {
            fail(error);
            return;
        }
    };
    let size = match item.Size() {
        Ok(size) if size.Width > 0 && size.Height > 0 => size,
        Ok(_) => {
            fail(PlatformError::SourceGone { id: id.clone() });
            return;
        }
        Err(e) => {
            fail(map_windows(&e));
            return;
        }
    };
    let pool = match Direct3D11CaptureFramePool::CreateFreeThreaded(
        &winrt,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        2,
        SizeInt32 { Width: size.Width, Height: size.Height },
    ) {
        Ok(pool) => pool,
        Err(e) => {
            fail(map_windows(&e));
            return;
        }
    };
    let session = match pool.CreateCaptureSession(&item) {
        Ok(session) => session,
        Err(e) => {
            fail(map_windows(&e));
            return;
        }
    };
    if let Err(e) = session.SetIsCursorCaptureEnabled(true) {
        let _ = e;
    }
    if let Err(e) = session.StartCapture() {
        fail(map_windows(&e));
        return;
    }
    let applied = golive_platform::capture_config_for(config.width, config.height, config.fps);
    eprintln!(
        "golive: capture {}x{}@{}fps (window)",
        applied.width, applied.height, applied.fps
    );
    let interval = interval_ns(applied.fps);
    let last_ns = AtomicU64::new(initial_last_ns(now_ns(), interval));
    let _ = ready_tx.send(Ok(()));
    while !stop_flag.load(Ordering::Acquire) {
        match grab_wgc_frame(&device, &context, &pool) {
            Some(frame) => {
                let now = now_ns();
                if gate_open(last_ns.load(Ordering::Relaxed), now, interval) {
                    let _ = frame_tx.try_send(CapturePacket::Cpu(frame));
                    last_ns.store(
                        golive_platform::cadence::advance_capture_clock(
                            last_ns.load(Ordering::Relaxed), now, interval,
                        ),
                        Ordering::Relaxed,
                    );
                }
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    let _ = session.Close();
    let _ = pool.Close();
}
