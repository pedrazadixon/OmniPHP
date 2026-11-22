//! Window helpers GPUI does not offer on Windows: hiding to the tray and
//! bringing the window back.

use gpui_kit::Window;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    IsIconic, MB_ICONERROR, MB_OK, MessageBoxW, SW_HIDE, SW_RESTORE, SW_SHOW,
    SetForegroundWindow, ShowWindow,
};

/// Blocking error dialog for failures that happen before any window exists.
pub fn fatal(message: &str) {
    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            wide(message).as_ptr(),
            wide("OmniPHP").as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}

fn hwnd(window: &Window) -> Option<*mut core::ffi::c_void> {
    let handle = HasWindowHandle::window_handle(window).ok()?;
    match handle.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get() as *mut core::ffi::c_void),
        _ => None,
    }
}

/// Hides the window without closing it; the app keeps running in the tray.
pub fn hide(window: &Window) {
    if let Some(hwnd) = hwnd(window) {
        unsafe { ShowWindow(hwnd, SW_HIDE) };
    }
}

/// Shows the window again (restoring it if minimized) and brings it forward.
pub fn show(window: &Window) {
    if let Some(hwnd) = hwnd(window) {
        unsafe {
            let cmd = if IsIconic(hwnd) != 0 { SW_RESTORE } else { SW_SHOW };
            ShowWindow(hwnd, cmd);
            SetForegroundWindow(hwnd);
        }
    }
}
