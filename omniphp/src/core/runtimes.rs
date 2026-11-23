//! Discovers the runtimes dropped into `bin\`: one folder per version.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use super::proc::quiet_command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhpRuntime {
    /// Folder name under `bin\php`; this is what config files refer to.
    pub id: String,
    /// Full version, e.g. `8.4.28`.
    pub version: String,
    pub thread_safe: bool,
    pub dir: PathBuf,
    /// Loadable extensions found in `ext\`, without the `php_` prefix.
    pub extensions: Vec<String>,
}

impl PhpRuntime {
    pub fn cgi(&self) -> PathBuf {
        self.dir.join("php-cgi.exe")
    }

    /// `8.4` for `8.4.28`.
    pub fn short_version(&self) -> String {
        self.version.split('.').take(2).collect::<Vec<_>>().join(".")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApacheRuntime {
    pub id: String,
    pub version: String,
    pub dir: PathBuf,
}

impl ApacheRuntime {
    pub fn httpd(&self) -> PathBuf {
        self.dir.join("bin").join("httpd.exe")
    }
}

/// A runtime that is just one executable: MySQL, Mailpit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRuntime {
    pub id: String,
    pub version: String,
    pub dir: PathBuf,
    pub exe: PathBuf,
}

/// Version folders under `bin_dir` that contain `exe` (a path relative to the
/// version folder), newest first.
pub fn scan_tool(bin_dir: &Path, exe: &str) -> Vec<ToolRuntime> {
    let mut runtimes: Vec<ToolRuntime> = subdirs(bin_dir)
        .into_iter()
        .filter(|dir| dir.join(exe).is_file())
        .map(|dir| {
            let id = folder_name(&dir);
            ToolRuntime {
                version: version_in(&id).unwrap_or_else(|| id.clone()),
                exe: dir.join(exe),
                id,
                dir,
            }
        })
        .collect();
    runtimes.sort_by(|a, b| compare_versions(&b.version, &a.version));
    runtimes
}

/// PHP runtimes that can serve FastCGI, newest first.
pub fn scan_php(bin_php: &Path) -> Vec<PhpRuntime> {
    let mut runtimes: Vec<PhpRuntime> = subdirs(bin_php)
        .into_iter()
        .filter(|dir| dir.join("php-cgi.exe").is_file())
        .map(|dir| {
            let id = folder_name(&dir);
            // The binary is asked only when the folder does not say: asking
            // starts one process per PHP version, every time the app opens.
            let version = exact_version_in(&id)
                .or_else(|| probe_php_version(&dir))
                .or_else(|| version_in(&id))
                .unwrap_or_else(|| id.clone());
            PhpRuntime {
                thread_safe: is_thread_safe(&dir),
                extensions: scan_extensions(&dir.join("ext")),
                id,
                version,
                dir,
            }
        })
        .collect();
    runtimes.sort_by(|a, b| compare_versions(&b.version, &a.version));
    runtimes
}

/// Apache runtimes, newest first.
pub fn scan_apache(bin_apache: &Path) -> Vec<ApacheRuntime> {
    let mut runtimes: Vec<ApacheRuntime> = subdirs(bin_apache)
        .into_iter()
        .filter(|dir| dir.join("bin").join("httpd.exe").is_file())
        .map(|dir| {
            let id = folder_name(&dir);
            ApacheRuntime {
                version: version_in(&id).unwrap_or_else(|| id.clone()),
                id,
                dir,
            }
        })
        .collect();
    runtimes.sort_by(|a, b| compare_versions(&b.version, &a.version));
    runtimes
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect()
}

fn folder_name(dir: &Path) -> String {
    dir.file_name().unwrap_or_default().to_string_lossy().into_owned()
}

/// Thread-safe builds ship `php8ts.dll` / `php7ts.dll`.
fn is_thread_safe(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|entries| {
        entries.flatten().any(|entry| {
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            name.starts_with("php") && name.ends_with("ts.dll")
        })
    })
}

fn scan_extensions(ext_dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(ext_dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            Some(name.strip_prefix("php_")?.strip_suffix(".dll")?.to_string())
        })
        .collect();
    names.sort();
    names
}

/// The version in a folder name when it is exact, down to the patch release:
/// `php-8.4.28-nts-Win32-vs17-x64` gives `8.4.28`, as the official downloads
/// are named. `php-8.4-nts-...-latest` gives nothing.
fn exact_version_in(name: &str) -> Option<String> {
    version_in(name).filter(|version| version.split('.').filter(|part| !part.is_empty()).count() >= 3)
}

/// Folder names such as `php-8.4-nts-...-latest` only carry `8.4`, so ask the
/// binary. `-n` skips php.ini so a broken ini cannot get in the way.
fn probe_php_version(dir: &Path) -> Option<String> {
    let output = quiet_command(dir.join("php.exe")).args(["-n", "-v"]).output().ok()?;
    parse_php_version(&String::from_utf8_lossy(&output.stdout))
}

/// Extracts `8.4.28` from `PHP 8.4.28 (cli) (built: ...)`.
pub fn parse_php_version(output: &str) -> Option<String> {
    let rest = output.trim_start().strip_prefix("PHP ")?;
    let version: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    (version.matches('.').count() >= 1).then_some(version)
}

/// First dotted number in a folder name: `httpd-2.4.71-261112-Win64-VS18` → `2.4.71`.
pub fn version_in(name: &str) -> Option<String> {
    name.split(['-', '_'])
        .find(|part| {
            part.contains('.')
                && part.chars().all(|c| c.is_ascii_digit() || c == '.')
                && part.starts_with(|c: char| c.is_ascii_digit())
        })
        .map(str::to_string)
}

pub fn compare_versions(a: &str, b: &str) -> Ordering {
    let parse = |v: &str| -> Vec<u32> { v.split('.').map(|p| p.parse().unwrap_or(0)).collect() };
    parse(a).cmp(&parse(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_php_v_output() {
        let out = "PHP 8.4.28 (cli) (built: Oct  1 2026 10:00:00) (NTS Visual C++ 2022 x64)\nCopyright";
        assert_eq!(parse_php_version(out).as_deref(), Some("8.4.28"));
        assert_eq!(parse_php_version("not php"), None);
    }

    #[test]
    fn finds_version_in_folder_names() {
        assert_eq!(version_in("httpd-2.4.71-261112-Win64-VS18").as_deref(), Some("2.4.71"));
        assert_eq!(version_in("php-8.4-nts-Win32-vs17-x64-latest").as_deref(), Some("8.4"));
        assert_eq!(version_in("php-7.4.33-Win32-vc15-x64").as_deref(), Some("7.4.33"));
        assert_eq!(version_in("mailpit"), None);
        assert_eq!(version_in("1.22.3").as_deref(), Some("1.22.3"));
        assert_eq!(version_in("mysql-8.4.3-winx64").as_deref(), Some("8.4.3"));
    }

    #[test]
    fn only_an_exact_version_in_the_name_spares_asking_the_binary() {
        assert_eq!(exact_version_in("php-8.4.28-nts-Win32-vs17-x64").as_deref(), Some("8.4.28"));
        assert_eq!(exact_version_in("php-7.4.33-Win32-vc15-x64").as_deref(), Some("7.4.33"));
        assert_eq!(exact_version_in("php-8.4-nts-Win32-vs17-x64-latest"), None);
        assert_eq!(exact_version_in("php-8.4.-nts"), None);
        assert_eq!(exact_version_in("my-php"), None);
    }

    #[test]
    fn compares_versions_numerically() {
        assert_eq!(compare_versions("8.10.0", "8.9.3"), Ordering::Greater);
        assert_eq!(compare_versions("7.4.33", "8.0.0"), Ordering::Less);
        assert_eq!(compare_versions("8.4", "8.4"), Ordering::Equal);
    }
}
