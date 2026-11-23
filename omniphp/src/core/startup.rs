//! "Run OmniPHP when Windows starts": an entry in the current user's Run
//! key. It needs no administrator rights and shows up in Windows' own
//! Startup apps list, where it can also be switched off.
//!
//! Each install has its own entry, named after its
//! [`Paths::install_key`], so two installs neither overwrite each other's
//! entry nor mistake it for their own.

use std::path::Path;

use anyhow::{Context, Result, bail};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ, RRF_RT_REG_SZ, RegCloseKey, RegDeleteValueW,
    RegGetValueW, RegOpenKeyExW, RegSetValueExW,
};

use super::paths::Paths;
use super::proc::wide;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// Passed on the command line by the startup entry so the app opens in the tray.
pub const STARTUP_FLAG: &str = "--startup";

fn value_name(paths: &Paths) -> String {
    format!("OmniPHP.{}", paths.install_key())
}

/// The command stored in the Run key to start `exe`.
fn command_for(exe: &Path) -> String {
    format!("\"{}\" {STARTUP_FLAG}", exe.display())
}

/// The text of a value of the Run key, if the value exists.
fn read(value: &str) -> Option<String> {
    let (key, value) = (wide(RUN_KEY), wide(value));
    let get = |data: *mut core::ffi::c_void, bytes: &mut u32| unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            data,
            bytes,
        )
    };
    // Asked without a buffer, Windows answers with the size it needs.
    let mut bytes = 0u32;
    if get(std::ptr::null_mut(), &mut bytes) != 0 {
        return None;
    }
    let mut text = vec![0u16; bytes as usize / 2 + 1];
    let mut bytes = (text.len() * 2) as u32;
    if get(text.as_mut_ptr().cast(), &mut bytes) != 0 {
        return None;
    }
    let len = text.iter().position(|&unit| unit == 0).unwrap_or(text.len());
    Some(String::from_utf16_lossy(&text[..len]))
}

/// The Run key, open for writing; closed when dropped.
struct RunKey(HKEY);

impl RunKey {
    fn open() -> Option<Self> {
        let mut key: HKEY = std::ptr::null_mut();
        let ok = unsafe {
            RegOpenKeyExW(HKEY_CURRENT_USER, wide(RUN_KEY).as_ptr(), 0, KEY_SET_VALUE, &mut key)
        } == 0;
        ok.then_some(Self(key))
    }

    fn set(&self, value: &str, text: &str) -> Result<()> {
        let data = wide(text);
        let status = unsafe {
            RegSetValueExW(
                self.0,
                wide(value).as_ptr(),
                0,
                REG_SZ,
                data.as_ptr() as *const u8,
                (data.len() * 2) as u32,
            )
        };
        check(status)
    }

    fn delete(&self, value: &str) -> Result<()> {
        match unsafe { RegDeleteValueW(self.0, wide(value).as_ptr()) } {
            2 => Ok(()), // ERROR_FILE_NOT_FOUND: already off.
            status => check(status),
        }
    }
}

impl Drop for RunKey {
    fn drop(&mut self) {
        unsafe { RegCloseKey(self.0) };
    }
}

fn check(status: u32) -> Result<()> {
    if status != 0 {
        bail!("Windows refused the startup setting (error {status}).");
    }
    Ok(())
}

pub fn is_enabled(paths: &Paths) -> bool {
    read(&value_name(paths)).is_some()
}

pub fn set_enabled(enabled: bool, paths: &Paths) -> Result<()> {
    let exe = std::env::current_exe().context("cannot locate the executable")?;
    let key = RunKey::open().context("cannot open the Windows startup settings")?;
    let value = value_name(paths);
    if enabled { key.set(&value, &command_for(&exe)) } else { key.delete(&value) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_command_quotes_the_executable_and_passes_the_flag() {
        let command = command_for(Path::new(r"C:\My Tools\OmniPHP.exe"));
        assert_eq!(command, r#""C:\My Tools\OmniPHP.exe" --startup"#);
    }

    #[test]
    fn each_install_has_its_own_entry() {
        let name = value_name(&Paths::new(r"C:\OmniPHP"));
        assert_eq!(name, value_name(&Paths::new(r"c:\omniphp")));
        assert_ne!(name, value_name(&Paths::new(r"D:\OmniPHP")));
    }
}
