//! "Run OmniPHP when Windows starts": an entry in the current user's Run
//! key. It needs no administrator rights and shows up in Windows' own
//! Startup apps list, where it can also be switched off.

use anyhow::{Context, Result, bail};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_SZ, RegCloseKey, RegDeleteValueW,
    RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};

use super::proc::wide;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE: &str = "OmniPHP";
/// Passed on the command line by the startup entry so the app opens in the tray.
pub const STARTUP_FLAG: &str = "--startup";

fn open(access: u32) -> Option<HKEY> {
    let mut key: HKEY = std::ptr::null_mut();
    let ok = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, wide(RUN_KEY).as_ptr(), 0, access, &mut key) } == 0;
    ok.then_some(key)
}

pub fn is_enabled() -> bool {
    let Some(key) = open(KEY_READ) else {
        return false;
    };
    let found = unsafe {
        RegQueryValueExW(
            key,
            wide(VALUE).as_ptr(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        ) == 0
    };
    unsafe { RegCloseKey(key) };
    found
}

pub fn set_enabled(enabled: bool) -> Result<()> {
    let key = open(KEY_SET_VALUE).context("cannot open the Windows startup settings")?;
    let status = unsafe {
        if enabled {
            let exe = std::env::current_exe().context("cannot locate the executable")?;
            let command = wide(&format!("\"{}\" {STARTUP_FLAG}", exe.display()));
            RegSetValueExW(
                key,
                wide(VALUE).as_ptr(),
                0,
                REG_SZ,
                command.as_ptr() as *const u8,
                (command.len() * 2) as u32,
            )
        } else {
            match RegDeleteValueW(key, wide(VALUE).as_ptr()) {
                2 => 0, // ERROR_FILE_NOT_FOUND: already off.
                other => other,
            }
        }
    };
    unsafe { RegCloseKey(key) };
    if status != 0 {
        bail!("Windows refused the startup setting (error {status}).");
    }
    Ok(())
}
