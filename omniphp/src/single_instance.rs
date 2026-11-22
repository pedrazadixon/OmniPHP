//! One OmniPHP per install folder and user session. A second launch of the
//! same install asks the first to show its window and exits. Different
//! installs (say `C:\OmniPHP` and a development copy) are independent and
//! can run side by side.

use std::ffi::c_void;
use std::path::Path;

use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::{
    CreateEventW, CreateMutexW, INFINITE, SetEvent, WaitForSingleObject,
};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A short, stable name for the install at `root`, safe to use in the name of
/// a Windows kernel object (which cannot contain backslashes).
pub fn key_for(root: &Path) -> String {
    // FNV-1a over the lower-cased path: Windows paths are case-insensitive.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in root.to_string_lossy().to_lowercase().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_identifies_the_install_folder() {
        let key = key_for(Path::new(r"C:\OmniPHP"));
        assert_eq!(key, key_for(Path::new(r"c:\omniphp")), "paths differing only in case are the same install");
        assert_ne!(key, key_for(Path::new(r"D:\OmniPHP")));
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()), "must be usable in a kernel object name");
    }
}
