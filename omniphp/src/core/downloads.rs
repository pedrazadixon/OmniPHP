//! Installing more PHP versions from the official Windows builds at
//! windows.php.net. Each download is checked against the SHA-256 the site
//! publishes before it is unpacked.

use std::fs::File;
use std::io::Read;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use super::paths::Paths;
use super::quickapp::{extract_unwrapped, http_get};
use super::runtimes::compare_versions;

const RELEASES: &str = "https://windows.php.net/downloads/releases/";

/// One downloadable PHP build (64-bit, non-thread-safe: the kind FastCGI wants).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhpRelease {
    pub version: String,
    /// File name on the download server, e.g. `php-8.4.28-nts-Win32-vs17-x64.zip`.
    pub file: String,
    pub sha256: String,
    /// Human-readable size as published, e.g. `33.74MB`.
    pub size: String,
}

impl PhpRelease {
    /// The folder the build is installed into under `bin\php`.
    pub fn folder(&self) -> &str {
        self.file.strip_suffix(".zip").unwrap_or(&self.file)
    }
}

/// The current release of every supported PHP series, newest first.
pub fn available_php() -> Result<Vec<PhpRelease>> {
    let json = http_get(&format!("{RELEASES}releases.json"))?
        .body_mut()
        .read_to_string()
        .context("windows.php.net did not answer")?;
    parse_releases(&json)
}

pub fn parse_releases(json: &str) -> Result<Vec<PhpRelease>> {
    let root: serde_json::Value = serde_json::from_str(json).context("unexpected release list format")?;
    let series = root.as_object().context("unexpected release list format")?;
    let mut releases: Vec<PhpRelease> = series
        .values()
        .filter_map(|entry| {
            let entry = entry.as_object()?;
            let version = entry.get("version")?.as_str()?;
            // Keys look like `nts-vs17-x64`; the compiler part varies by series.
            let (_, build) = entry
                .iter()
                .find(|(key, _)| key.starts_with("nts-") && key.ends_with("-x64"))?;
            let zip = build.get("zip")?;
            let file = zip.get("path")?.as_str()?;
            // The name ends up in a URL and a folder name: accept only the expected shape.
            let safe = file.starts_with("php-")
                && file.ends_with(".zip")
                && file.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'));
            safe.then(|| PhpRelease {
                version: version.to_string(),
                file: file.to_string(),
                sha256: zip.get("sha256").and_then(|v| v.as_str()).unwrap_or_default().to_lowercase(),
                size: zip.get("size").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            })
        })
        .collect();
    releases.sort_by(|a, b| compare_versions(&b.version, &a.version));
    Ok(releases)
}

/// Downloads, verifies and unpacks a PHP build into `bin\php`.
pub fn install_php(release: &PhpRelease, paths: &Paths, progress: &dyn Fn(&str)) -> Result<PathBuf> {
    let target = paths.bin().join("php").join(release.folder());
    if target.exists() {
        bail!("PHP {} is already installed.", release.version);
    }
    if release.sha256.len() != 64 {
        bail!("windows.php.net published no checksum for PHP {}, so it was not downloaded.", release.version);
    }

    progress(&format!("Downloading PHP {} ({})…", release.version, release.size));
    let downloads = paths.tmp().join("downloads");
    std::fs::create_dir_all(&downloads)?;
    let zip = downloads.join(&release.file);
    {
        let mut response = http_get(&format!("{RELEASES}{}", release.file))?;
        let mut file = File::create(&zip).with_context(|| format!("cannot write {}", zip.display()))?;
        std::io::copy(&mut response.body_mut().as_reader(), &mut file)
            .context("the PHP download was interrupted")?;
    }

    progress("Checking the download…");
    let actual = sha256_of(&zip)?;
    if actual != release.sha256 {
        let _ = std::fs::remove_file(&zip);
        bail!("The PHP {} download does not match its published checksum and was discarded.", release.version);
    }

    progress(&format!("Unpacking PHP {}…", release.version));
    std::fs::create_dir_all(&target)?;
    if let Err(error) = extract_unwrapped(&zip, &target) {
        let _ = std::fs::remove_dir_all(&target);
        return Err(error);
    }
    let _ = std::fs::remove_file(&zip);
    Ok(target)
}

fn sha256_of(file: &std::path::Path) -> Result<String> {
    let mut input = File::open(file)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 16];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_64_bit_nts_build_of_each_series() {
        let json = r#"{
          "8.3": {"version": "8.3.37",
                  "ts-vs16-x64":  {"zip": {"path": "php-8.3.37-Win32-vs16-x64.zip", "size": "31MB", "sha256": "AA"}},
                  "nts-vs16-x86": {"zip": {"path": "php-8.3.37-nts-Win32-vs16-x86.zip", "size": "28MB", "sha256": "BB"}},
                  "nts-vs16-x64": {"zip": {"path": "php-8.3.37-nts-Win32-vs16-x64.zip", "size": "31.5MB", "sha256": "CC"}}},
          "8.10": {"version": "8.10.1",
                  "nts-vs18-x64": {"zip": {"path": "php-8.10.1-nts-Win32-vs18-x64.zip", "size": "35MB", "sha256": "dd"}}},
          "9.0": {"version": "9.0.0", "source": {"path": "php-9.0.0-src.zip"}},
          "bad": {"version": "1.0.0",
                  "nts-vs18-x64": {"zip": {"path": "../../evil.zip", "size": "1MB", "sha256": "ee"}}}
        }"#;
        let releases = parse_releases(json).unwrap();
        let versions: Vec<&str> = releases.iter().map(|release| release.version.as_str()).collect();
        assert_eq!(versions, ["8.10.1", "8.3.37"], "newest first; series without a usable build are skipped");
        assert_eq!(releases[1].file, "php-8.3.37-nts-Win32-vs16-x64.zip");
        assert_eq!(releases[1].sha256, "cc");
        assert_eq!(releases[1].folder(), "php-8.3.37-nts-Win32-vs16-x64");
    }

    #[test]
    fn hashes_files_like_sha256sum() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("abc.txt");
        std::fs::write(&file, "abc").unwrap();
        assert_eq!(
            sha256_of(&file).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
