//! One OmniPHP per install folder and user session. A second launch of the
//! same install asks the first to show its window and exits. Different
//! installs (say `C:\OmniPHP` and a development copy) are independent and
//! can run side by side.
//!
//! An install is identified by its [`Paths::install_key`](crate::core::paths::Paths::install_key).

use std::ffi::c_void;

use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::{
    CreateEventW, CreateMutexW, INFINITE, SetEvent, WaitForSingleObject,
};

use crate::core::proc::wide;

fn show_event(key: &str) -> *mut c_void {
    // Auto-reset, initially unsignalled. Returns the existing event when another
    // instance already created it.
    let name = wide(&format!("Local\\OmniPHP.ShowWindow.{key}"));
    unsafe { CreateEventW(std::ptr::null(), 0, 0, name.as_ptr()) }
}

/// Returns `true` when this process is the first instance of the install
/// identified by `key`. Otherwise signals the running instance to show its
/// window and returns `false`.
///
/// The mutex handle is deliberately leaked: it must live as long as the process.
pub fn acquire(key: &str) -> bool {
    unsafe {
        let name = wide(&format!("Local\\OmniPHP.SingleInstance.{key}"));
        let mutex = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        if !mutex.is_null() && GetLastError() != ERROR_ALREADY_EXISTS {
            return true;
        }
        let event = show_event(key);
        if !event.is_null() {
            SetEvent(event);
        }
        false
    }
}

/// Calls `on_show` (on a background thread) each time another launch of the
/// same install asks this instance to show itself.
pub fn listen(key: &str, on_show: impl Fn() + Send + 'static) {
    let event = show_event(key) as isize;
    if event == 0 {
        return;
    }
    std::thread::Builder::new()
        .name("single-instance".into())
        .spawn(move || {
            while unsafe { WaitForSingleObject(event as *mut c_void, INFINITE) } == WAIT_OBJECT_0 {
                on_show();
            }
        })
        .ok();
}
