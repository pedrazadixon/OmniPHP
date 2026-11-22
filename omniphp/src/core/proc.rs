//! Child-process helpers for Windows.

use std::ffi::OsStr;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::process::{Child, Command};
use std::sync::OnceLock;

use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows_sys::Win32::System::Threading::{EVENT_MODIFY_STATE, OpenEventW, SetEvent};
use windows_sys::Win32::Foundation::CloseHandle;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A command that never flashes a console window.
pub fn quiet_command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Puts `child` (and everything it spawns: Apache workers, php-cgi) into a job
/// that Windows kills when this process ends, however it ends. Services can
/// therefore never outlive the app as orphans.
pub fn kill_with_app(child: &Child) {
    static JOB: OnceLock<isize> = OnceLock::new();
    let job = *JOB.get_or_init(|| unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return 0;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        // The handle is kept open for the life of the process on purpose.
        job as isize
    });
    if job != 0 {
        unsafe {
            AssignProcessToJobObject(job as *mut core::ffi::c_void, child.as_raw_handle() as _);
        }
    }
}

/// Signals a named Win32 event. Returns `false` when no such event exists.
pub fn signal_event(name: &str) -> bool {
    unsafe {
        let event = OpenEventW(EVENT_MODIFY_STATE, 0, wide(name).as_ptr());
        if event.is_null() {
            return false;
        }
        let ok = SetEvent(event) != 0;
        CloseHandle(event);
        ok
    }
}
