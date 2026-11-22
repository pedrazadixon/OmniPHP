//! The two changes OmniPHP makes to Windows itself, both of which need
//! administrator rights: trusting the local certificate authority and
//! registering site names in the hosts file.
//!
//! The app runs unelevated. When either change is needed it relaunches itself
//! with the `elevated` command through a UAC prompt ([`run_elevated`]), and
//! that short-lived process does the work ([`elevated_main`]). What to do is
//! passed on the command line, which the prompt shows and nothing can alter
//! afterwards.

use anyhow::{Context, Result, bail};
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, GetLastError};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, RegCloseKey, RegOpenKeyExW,
};
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
use windows_sys::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

use super::hosts;
use super::paths::Paths;
use super::proc::{quiet_command, wide};
use super::ssl;

const ELEVATED_LOG: &str = "elevated.log";

/// Whether Windows trusts the certificate with this SHA-1 thumbprint as a root
/// authority (for the whole machine or for the current user).
pub fn ca_trusted(thumbprint: &str) -> bool {
    let key = format!(r"SOFTWARE\Microsoft\SystemCertificates\ROOT\Certificates\{thumbprint}");
    [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER].into_iter().any(|hive| unsafe {
        let mut handle: HKEY = std::ptr::null_mut();
        let found = RegOpenKeyExW(hive, wide(&key).as_ptr(), 0, KEY_READ, &mut handle) == 0;
        if found {
            RegCloseKey(handle);
        }
        found
    })
}

/// Hostnames this install has registered in the hosts file.
pub fn registered_hosts(paths: &Paths) -> Vec<String> {
    std::fs::read(hosts::hosts_path())
        .map(|bytes| hosts::managed_names(&String::from_utf8_lossy(&bytes), paths.root()))
        .unwrap_or_default()
}

/// What the elevated helper should do.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SystemChanges {
    /// Thumbprint of the authority to trust, if it is not trusted yet.
    pub trust_ca: Option<String>,
    /// The complete list of hostnames the hosts block should contain, if it
    /// differs from what is there now.
    pub hosts: Option<Vec<String>>,
}

impl SystemChanges {
    pub fn is_empty(&self) -> bool {
        self.trust_ca.is_none() && self.hosts.is_none()
    }

    fn to_args(&self) -> Vec<String> {
        let mut args = vec!["elevated".to_string()];
        if let Some(thumbprint) = &self.trust_ca {
            args.extend(["--trust-ca".to_string(), thumbprint.clone()]);
        }
        if let Some(names) = &self.hosts {
            // "-" stands for an empty list, since an empty argument is fragile.
            let list = if names.is_empty() { "-".to_string() } else { names.join(",") };
            args.extend(["--hosts".to_string(), list]);
        }
        args
    }

    fn from_args(args: &[String]) -> Result<Self> {
        let mut changes = Self::default();
        let mut args = args.iter();
        while let Some(flag) = args.next() {
            let value = args.next().with_context(|| format!("{flag} needs a value"))?;
            match flag.as_str() {
                "--trust-ca" => changes.trust_ca = Some(value.clone()),
                "--hosts" => {
                    changes.hosts = Some(match value.as_str() {
                        "-" => Vec::new(),
                        list => list.split(',').map(str::to_string).collect(),
                    })
                }
                other => bail!("unknown option {other}"),
            }
        }
        Ok(changes)
    }
}

/// Applies `changes` through a UAC prompt and waits for the result.
pub fn run_elevated(changes: &SystemChanges, paths: &Paths) -> Result<()> {
    if changes.is_empty() {
        return Ok(());
    }
    let exe = std::env::current_exe().context("cannot locate the executable")?;
    let parameters = changes.to_args().join(" ");
    let log = paths.logs().join(ELEVATED_LOG);
    let _ = std::fs::remove_file(&log);

    let exe_w = wide(&exe.to_string_lossy());
    let parameters_w = wide(&parameters);
    let verb_w = wide("runas");
    let exit_code = unsafe {
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS;
        info.lpVerb = verb_w.as_ptr();
        info.lpFile = exe_w.as_ptr();
        info.lpParameters = parameters_w.as_ptr();
        info.nShow = SW_HIDE;
        if ShellExecuteExW(&mut info) == 0 {
            if GetLastError() == ERROR_CANCELLED {
                bail!("Administrator permission was declined, so nothing was changed.");
            }
            bail!("Windows could not start the administrator helper (error {}).", GetLastError());
        }
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 1u32;
        GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
        code
    };
    if exit_code != 0 {
        let detail = std::fs::read_to_string(&log).unwrap_or_default();
        bail!("The administrator helper failed. {}", detail.trim());
    }
    Ok(())
}

/// Entry point of the elevated process. `args` are the arguments after `elevated`.
pub fn elevated_main(args: &[String]) -> i32 {
    let paths = match Paths::discover() {
        Ok(paths) => paths,
        Err(_) => return 2,
    };
    match apply(args, &paths) {
        Ok(()) => 0,
        Err(error) => {
            let _ = std::fs::create_dir_all(paths.logs());
            let _ = std::fs::write(paths.logs().join(ELEVATED_LOG), format!("{error:#}"));
            1
        }
    }
}

fn apply(args: &[String], paths: &Paths) -> Result<()> {
    let changes = SystemChanges::from_args(args)?;

    if let Some(expected) = &changes.trust_ca {
        let cert_file = paths.ssl_dir().join("ca.crt");
        let pem = std::fs::read_to_string(&cert_file)
            .with_context(|| format!("cannot read {}", cert_file.display()))?;
        // Trust exactly the certificate the user was asked about, not whatever
        // file happens to be there now.
        if &ssl::thumbprint_of_pem(&pem)? != expected {
            bail!("The certificate authority file changed while asking for permission.");
        }
        let output = quiet_command("certutil")
            .args(["-addstore", "-f", "Root"])
            .arg(&cert_file)
            .output()
            .context("cannot run certutil")?;
        if !output.status.success() {
            bail!("certutil could not trust the certificate: {}", String::from_utf8_lossy(&output.stdout).trim());
        }
    }

    if let Some(names) = &changes.hosts {
        if let Some(bad) = names.iter().find(|name| !hosts::is_valid_hostname(name)) {
            bail!("\"{bad}\" is not a valid hostname.");
        }
        let file = hosts::hosts_path();
        let current = std::fs::read(&file).with_context(|| format!("cannot read {}", file.display()))?;
        let merged = hosts::merge(&String::from_utf8_lossy(&current), paths.root(), names);
        std::fs::write(&file, merged).with_context(|| format!("cannot write {}", file.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_survive_the_command_line() {
        for changes in [
            SystemChanges { trust_ca: Some("ABCDEF".into()), hosts: Some(vec!["a.test".into(), "b.test".into()]) },
            SystemChanges { trust_ca: None, hosts: Some(Vec::new()) },
            SystemChanges { trust_ca: Some("ABCDEF".into()), hosts: None },
        ] {
            let args = changes.to_args();
            assert_eq!(args[0], "elevated");
            assert!(args.iter().all(|arg| !arg.contains(' ')), "arguments must not need quoting");
            assert_eq!(SystemChanges::from_args(&args[1..]).unwrap(), changes);
        }
    }

    #[test]
    fn unknown_options_are_refused() {
        assert!(SystemChanges::from_args(&["--delete".into(), "x".into()]).is_err());
    }
}
