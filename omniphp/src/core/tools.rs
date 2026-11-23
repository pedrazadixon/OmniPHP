//! Developer tools around the services: a terminal with the right PHP on its
//! path, and Adminer for browsing the databases.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::paths::Paths;
use super::proc::quiet_command;

/// Opens a command prompt in `dir` whose PATH starts with `path_dirs` (the
/// site's PHP, Composer, the MySQL client…). With `phprc` set, `php` on that
/// prompt reads the same php.ini as the site does in the browser.
pub fn open_terminal(dir: &Path, title: &str, path_dirs: &[PathBuf], phprc: Option<&Path>) -> Result<()> {
    let mut path = std::env::join_paths(path_dirs.iter().filter(|dir| dir.is_dir()))
        .context("a tool folder has a character PATH cannot hold")?;
    if let Some(existing) = std::env::var_os("PATH") {
        path.push(";");
        path.push(existing);
    }
    // `start` gives the new prompt its own window (Windows Terminal, when that
    // is the default terminal) and it inherits the environment set here.
    //
    // The command line is written by hand because `start` only takes its
    // first argument as the window title when it is quoted, and Rust quotes
    // only arguments with spaces: a bare `OmniPHP` was read as the program to
    // run, so no prompt opened.
    use std::os::windows::process::CommandExt;
    let mut command = quiet_command("cmd.exe");
    command.raw_arg(format!(
        r#"/C start "{}" /D "{}" cmd.exe /K "php -v""#,
        title.replace('"', ""),
        dir.display()
    ));
    command.env("PATH", path);
    if let Some(phprc) = phprc {
        command.env("PHPRC", phprc);
    }
    command.spawn().context("cannot open a terminal")?;
    Ok(())
}

/// Opens `url` in the default browser. Asks Windows directly rather than
/// going through `cmd /c start`, which misreads `&` in an address.
pub fn open_url(url: &str) -> Result<()> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let (verb, target) = (super::proc::wide("open"), super::proc::wide(url));
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecute reports success as a value above 32.
    if result as isize <= 32 {
        anyhow::bail!("Windows could not open {url}.");
    }
    Ok(())
}

/// The program and arguments that open `hosts` for editing: the bundled
/// Notepad++ when it is installed, otherwise Windows' own Notepad.
fn hosts_editor(notepad_plus_plus: Option<&Path>, hosts: &Path) -> (PathBuf, String) {
    match notepad_plus_plus {
        // Without `-multiInst` a Notepad++ that is already open takes the file
        // over, and that one is not running as administrator, so it could not
        // save. `-nosession` keeps the files of an earlier session out.
        Some(exe) => (exe.to_path_buf(), format!("-multiInst -nosession \"{}\"", hosts.display())),
        None => (PathBuf::from("notepad.exe"), format!("\"{}\"", hosts.display())),
    }
}

/// Opens the Windows hosts file in an editor running as administrator, which
/// is what saving that file takes. Windows asks for permission first;
/// declining is not an error.
pub fn edit_hosts_file(paths: &Paths) -> Result<()> {
    use windows_sys::Win32::Foundation::{ERROR_CANCELLED, GetLastError};
    use windows_sys::Win32::UI::Shell::{SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let bundled = paths.bin().join("notepad++").join("notepad++.exe");
    let (editor, parameters) =
        hosts_editor(bundled.is_file().then_some(bundled.as_path()), &super::hosts::hosts_path());
    let (verb, file, parameters) = (
        super::proc::wide("runas"),
        super::proc::wide(&editor.to_string_lossy()),
        super::proc::wide(&parameters),
    );
    unsafe {
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.lpVerb = verb.as_ptr();
        info.lpFile = file.as_ptr();
        info.lpParameters = parameters.as_ptr();
        info.nShow = SW_SHOWNORMAL;
        if ShellExecuteExW(&mut info) == 0 {
            let error = GetLastError();
            if error != ERROR_CANCELLED {
                anyhow::bail!("Windows could not open the hosts file in {} (error {error}).", editor.display());
            }
        }
    }
    Ok(())
}

/// The folder Apache serves at `/adminer`. It holds only the generated
/// wrapper page; Adminer itself is a download and lives in `bin\`.
pub fn adminer_dir(paths: &Paths) -> PathBuf {
    paths.etc().join("apps").join("adminer")
}

pub fn adminer_script(paths: &Paths) -> PathBuf {
    paths.bin().join("adminer").join("adminer.php")
}

/// The manifest `fetch-runtimes.bat` reads, built into the app so that both
/// install the same pinned versions.
const RUNTIMES_MANIFEST: &str = include_str!("../../../runtimes.json");

/// Where the pinned Adminer release can be downloaded, and its SHA-256.
fn pinned_adminer(manifest: &str) -> Result<(Vec<String>, String)> {
    let manifest: serde_json::Value = serde_json::from_str(manifest).context("runtimes.json is not valid JSON")?;
    let entry = manifest["runtimes"]
        .as_array()
        .and_then(|entries| entries.iter().find(|entry| entry["name"] == "adminer"))
        .context("runtimes.json lists no adminer")?;
    let urls: Vec<String> = entry["urls"]
        .as_array()
        .map(|urls| urls.iter().filter_map(|url| url.as_str()).map(str::to_string).collect())
        .unwrap_or_default();
    let sha256 = entry["sha256"].as_str().unwrap_or_default().to_lowercase();
    if urls.is_empty() || sha256.len() != 64 {
        anyhow::bail!("runtimes.json gives no address or no SHA-256 for adminer");
    }
    Ok((urls, sha256))
}

/// Downloads the pinned Adminer release to `script`. Nothing is written
/// unless the download matches its checksum.
fn download_adminer(script: &Path) -> Result<()> {
    use sha2::{Digest, Sha256};

    let (urls, expected) = pinned_adminer(RUNTIMES_MANIFEST)?;
    let mut failure = None;
    for url in &urls {
        let code = super::quickapp::http_get(url).and_then(|mut response| {
            response
                .body_mut()
                .with_config()
                .limit(20 * 1024 * 1024)
                .read_to_vec()
                .context("the Adminer download was interrupted")
        });
        match code {
            Ok(code) => {
                let actual: String = Sha256::digest(&code).iter().map(|byte| format!("{byte:02x}")).collect();
                if actual != expected {
                    anyhow::bail!("The Adminer download does not match its checksum and was discarded.");
                }
                if let Some(dir) = script.parent() {
                    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
                }
                return std::fs::write(script, code).with_context(|| format!("cannot write {}", script.display()));
            }
            Err(error) => failure = Some(error),
        }
    }
    Err(failure.unwrap_or_else(|| anyhow::anyhow!("runtimes.json gives no address for adminer")))
}

/// Downloads Adminer if `bin\adminer` does not hold it yet. Apart from
/// [`ensure_adminer`] so that the window can do it without holding the engine.
pub fn fetch_adminer(paths: &Paths) -> Result<()> {
    let script = adminer_script(paths);
    if !script.is_file() {
        download_adminer(&script)?;
    }
    Ok(())
}

/// Makes sure Adminer is in place (downloading it if `bin\adminer` is empty)
/// and writes the wrapper page Apache serves.
pub fn ensure_adminer(paths: &Paths, mysql_port: u16) -> Result<()> {
    fetch_adminer(paths)?;
    let script = adminer_script(paths);
    let dir = adminer_dir(paths);
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    // Adminer used to be downloaded next to the wrapper; that copy is unused now.
    let _ = std::fs::remove_file(dir.join("adminer.php"));
    let index = dir.join("index.php");
    std::fs::write(&index, adminer_index(&script, mysql_port))
        .with_context(|| format!("cannot write {}", index.display()))
}

fn adminer_index(script: &Path, mysql_port: u16) -> String {
    // The path ends up inside a single-quoted PHP string.
    let script = super::paths::slash(script).replace('\'', "\\'");
    ADMINER_INDEX.replace("{port}", &mysql_port.to_string()).replace("{script}", &script)
}

/// Adminer refuses accounts without a password unless told otherwise, and the
/// local root account has none. This wrapper allows it; the page itself is
/// only reachable from this PC (`Require local` in the vhost).
const ADMINER_INDEX: &str = r#"<?php
// Generated by OmniPHP. Do not edit: this file is overwritten.
function adminer_object() {
    if (class_exists('Adminer\Adminer')) {
        return new class extends Adminer\Adminer {
            function login($login, $password) { return true; }
            // Adminer 6 wants its form token on a login; the automatic
            // sign-in below has none.
            function verifyLoginToken() { return false; }
        };
    }
    return new class extends Adminer {
        function login($login, $password) { return true; }
    };
}
// Opened without an address of its own (the Database button), sign in to the
// local server straight away instead of showing the login form.
if (!$_GET && !$_POST) {
    $_POST['auth'] = ['driver' => 'server', 'server' => '127.0.0.1:{port}', 'username' => 'root', 'password' => '', 'db' => ''];
}
include '{script}';
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_open_in_notepad_plus_plus_when_it_is_installed() {
        let hosts = Path::new(r"C:\Windows\System32\drivers\etc\hosts");
        let (editor, parameters) = hosts_editor(Some(Path::new(r"D:\OmniPHP\bin\notepad++\notepad++.exe")), hosts);
        assert_eq!(editor, Path::new(r"D:\OmniPHP\bin\notepad++\notepad++.exe"));
        assert_eq!(parameters, r#"-multiInst -nosession "C:\Windows\System32\drivers\etc\hosts""#);
    }

    #[test]
    fn manifest_pins_every_runtime() {
        let manifest: serde_json::Value = serde_json::from_str(RUNTIMES_MANIFEST).unwrap();
        let entries = manifest["runtimes"].as_array().unwrap();
        assert!(!entries.is_empty());
        for entry in entries {
            let name = entry["name"].as_str().expect("every runtime has a name");
            let sha256 = entry["sha256"].as_str().unwrap_or_default();
            assert!(
                sha256.len() == 64 && sha256.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')),
                "{name}: sha256 must be 64 lower-case hex digits"
            );
            let urls = entry["urls"].as_array().unwrap_or_else(|| panic!("{name}: no urls"));
            assert!(!urls.is_empty(), "{name}: no urls");
            assert!(
                urls.iter().all(|url| url.as_str().is_some_and(|url| url.starts_with("https://"))),
                "{name}: urls must be https"
            );
            let dir = entry["dir"].as_str().unwrap_or_default();
            assert!(dir.starts_with("bin/") && !dir.contains(".."), "{name}: dir must be inside bin/");
            assert!(entry["check"].as_str().is_some_and(|check| !check.is_empty()), "{name}: no check file");
        }
    }

    #[test]
    fn adminer_comes_from_the_manifest() {
        let (urls, sha256) = pinned_adminer(RUNTIMES_MANIFEST).unwrap();
        assert!(urls[0].ends_with(".php"));
        assert_eq!(sha256.len(), 64);
        assert!(pinned_adminer(r#"{"runtimes": []}"#).is_err());
        let unpinned = r#"{"runtimes": [{"name": "adminer", "urls": ["https://x/a.php"], "sha256": "abc"}]}"#;
        assert!(pinned_adminer(unpinned).is_err());
    }

    #[test]
    fn adminer_wrapper_includes_the_copy_in_bin() {
        let page = adminer_index(Path::new(r"D:\OmniPHP\bin\adminer\adminer.php"), 3307);
        assert!(page.contains("include 'D:/OmniPHP/bin/adminer/adminer.php';"));
        assert!(page.contains("'server' => '127.0.0.1:3307'"));
        assert!(!page.contains("{script}") && !page.contains("{port}"));
    }

    #[test]
    fn hosts_fall_back_to_windows_notepad() {
        let (editor, parameters) = hosts_editor(None, Path::new(r"C:\Windows\System32\drivers\etc\hosts"));
        assert_eq!(editor, Path::new("notepad.exe"));
        assert_eq!(parameters, r#""C:\Windows\System32\drivers\etc\hosts""#);
    }
}
