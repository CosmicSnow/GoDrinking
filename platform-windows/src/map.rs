//! HRESULT → typed [`PlatformError`]. Codes only — never message text.

use golive_platform::PlatformError;
use windows::core::HRESULT;
use windows::Win32::Foundation::E_ACCESSDENIED;
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_ACCESS_DENIED, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_NOT_CURRENTLY_AVAILABLE,
    DXGI_ERROR_WAIT_TIMEOUT,
};

use crate::PERMISSION_HINT;

#[cfg(test)]
pub fn hresult_i32(hr: HRESULT) -> i32 {
    hr.0
}

pub fn is_wait_timeout(code: i32) -> bool {
    code == DXGI_ERROR_WAIT_TIMEOUT.0
}

pub fn is_access_lost(code: i32) -> bool {
    code == DXGI_ERROR_ACCESS_LOST.0
}

pub fn denied() -> PlatformError {
    PlatformError::PermissionDenied { hint: PERMISSION_HINT }
}

/// Maps a capture HRESULT. Timeout and access-lost are NOT mapped here —
/// the pump treats those as idle / recreate.
pub fn map_hresult(code: i32) -> PlatformError {
    let hr = HRESULT(code);
    if hr == E_ACCESSDENIED || hr == DXGI_ERROR_ACCESS_DENIED || hr == DXGI_ERROR_NOT_CURRENTLY_AVAILABLE
    {
        return denied();
    }
    PlatformError::Internal(format!("captura falhou (#{code})"))
}

pub fn map_windows(err: &windows::core::Error) -> PlatformError {
    map_hresult(err.code().0)
}
