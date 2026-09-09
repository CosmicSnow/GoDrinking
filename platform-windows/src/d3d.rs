//! Shared D3D11 device + staging copy. Frames out as tight BGRA.

use golive_platform::{BgraFrame, PlatformError};
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_10_0,
    D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIAdapter1};

use crate::copy::copy_tight_bgra;
use crate::map::map_windows;

const FEATURE_LEVELS: [windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL; 4] = [
    D3D_FEATURE_LEVEL_11_1,
    D3D_FEATURE_LEVEL_11_0,
    D3D_FEATURE_LEVEL_10_1,
    D3D_FEATURE_LEVEL_10_0,
];

pub fn create_device(
    adapter: Option<&IDXGIAdapter1>,
) -> Result<(ID3D11Device, ID3D11DeviceContext), PlatformError> {
    let mut device = None;
    let mut context = None;
    unsafe {
        if let Some(adapter) = adapter {
            let adapter: IDXGIAdapter = adapter.cast().map_err(|e| map_windows(&e))?;
            D3D11CreateDevice(
                Some(&adapter),
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&FEATURE_LEVELS),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .map_err(|e| map_windows(&e))?;
        } else {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&FEATURE_LEVELS),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .map_err(|e| map_windows(&e))?;
        }
    }
    match (device, context) {
        (Some(device), Some(context)) => Ok((device, context)),
        _ => Err(PlatformError::Internal("dispositivo D3D11 vazio".into())),
    }
}

pub fn texture_to_bgra(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    tex: &ID3D11Texture2D,
) -> Result<BgraFrame, PlatformError> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { tex.GetDesc(&mut desc) };
    desc.Usage = D3D11_USAGE_STAGING;
    desc.BindFlags = 0;
    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
    desc.MiscFlags = 0;
    let mut staging = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut staging)) }
        .map_err(|e| map_windows(&e))?;
    let staging = staging.ok_or_else(|| PlatformError::Internal("staging vazio".into()))?;
    unsafe { context.CopyResource(&staging, tex) };
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe { context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }
        .map_err(|e| map_windows(&e))?;
    let w = desc.Width;
    let h = desc.Height;
    let stride = mapped.RowPitch as usize;
    let need = stride
        .saturating_mul(h.saturating_sub(1) as usize)
        .saturating_add((w as usize).saturating_mul(4));
    let frame = if mapped.pData.is_null() {
        None
    } else {
        let src = unsafe { std::slice::from_raw_parts(mapped.pData as *const u8, need) };
        copy_tight_bgra(src, w, h, stride)
    };
    unsafe { context.Unmap(&staging, 0) };
    frame.ok_or_else(|| PlatformError::Internal("frame BGRA vazio".into()))
}
