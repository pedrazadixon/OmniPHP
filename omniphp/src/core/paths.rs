//! Where everything lives under the OmniPHP root.
//!
//! `bin\` holds what is downloaded: the runtimes, and the two things the app
//! can fetch itself (more PHP versions, Adminer). Everything the app generates
//! goes to `etc\`, runtime state to `data\`, `logs\` and `tmp\`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    root: PathBuf,
}

impl Paths {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// `OMNIPHP_ROOT` if set, otherwise the nearest ancestor of the executable
    /// that contains both `bin\` and `www\`.
    pub fn discover() -> Result<Self> {
        if let Some(root) = std::env::var_os("OMNIPHP_ROOT") {
            return Ok(Self::new(normalized(Path::new(&root))));
        }
        let exe = std::env::current_exe().context("cannot locate the executable")?;
        for dir in exe.ancestors().skip(1) {
            if dir.join("bin").is_dir() && dir.join("www").is_dir() {
                return Ok(Self::new(dir));
            }
        }
        bail!(
            "no OmniPHP root found above {}: expected a folder containing bin\\ and www\\ \
             (or set OMNIPHP_ROOT)",
            exe.display()
        )
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// A short, stable name for this install, safe to use in the name of a
    /// Windows kernel object or registry value (neither takes backslashes).
    /// Everything that must tell installs apart is named after it.
    pub fn install_key(&self) -> String {
        // FNV-1a over the lower-cased path: Windows paths are case-insensitive.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in self.root.to_string_lossy().to_lowercase().bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!("{hash:016x}")
    }

    pub fn bin(&self) -> PathBuf {
        self.root.join("bin")
    }
    pub fn www(&self) -> PathBuf {
        self.root.join("www")
    }
    pub fn etc(&self) -> PathBuf {
        self.root.join("etc")
    }
    pub fn data(&self) -> PathBuf {
        self.root.join("data")
    }
    pub fn logs(&self) -> PathBuf {
        self.root.join("logs")
    }
    pub fn tmp(&self) -> PathBuf {
        self.root.join("tmp")
    }

    pub fn config_file(&self) -> PathBuf {
        self.root.join("etc").join("omniphp").join("omniphp.toml")
    }
    pub fn sites_file(&self) -> PathBuf {
        self.root.join("etc").join("omniphp").join("sites.toml")
    }

    pub fn apache_etc(&self) -> PathBuf {
        self.root.join("etc").join("apache")
    }
    pub fn apache_conf(&self) -> PathBuf {
        self.apache_etc().join("httpd.conf")
    }
    /// Generated vhosts; rewritten on every apply.
    pub fn apache_sites(&self) -> PathBuf {
        self.apache_etc().join("sites")
    }
    /// Where a new configuration is written for Apache to check before it
    /// replaces the one in use.
    pub fn apache_staging(&self) -> PathBuf {
        self.apache_etc().join("staging")
    }
    /// Hand-written `*.conf` files the app includes but never touches.
    pub fn apache_custom(&self) -> PathBuf {
        self.apache_etc().join("custom")
    }
    /// Apache's FastCGI module. A download like the other runtimes, so it
    /// lives in `bin\`; it must be built for the same Apache build.
    pub fn fcgid_module(&self) -> PathBuf {
        self.bin().join("mod_fcgid").join("mod_fcgid.so")
    }

    /// Certificate authority files; site certificates live in `sites\` below it.
    pub fn ssl_dir(&self) -> PathBuf {
        self.root.join("etc").join("ssl")
    }

    /// Folder holding the generated `php.ini` for one site (used as `PHPRC`).
    pub fn php_site_dir(&self, site: &str) -> PathBuf {
        self.root.join("etc").join("php").join("sites").join(site)
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in [
            self.logs(),
            self.tmp(),
            self.tmp().join("sessions"),
            self.apache_sites(),
            self.apache_custom(),
            self.root.join("etc").join("omniphp"),
        ] {
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("cannot create {}", dir.display()))?;
        }
        Ok(())
    }
}

/// Path with forward slashes, as Apache and php.ini expect.
pub fn slash(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// `root` as an absolute path with backslashes and no trailing separator, so
/// that `C:/OmniPHP/` and `C:\OmniPHP` are written down the same way (the
/// hosts file and the generated configuration both quote the root). Links are
/// not resolved: an install reached through a junction keeps that path.
fn normalized(root: &Path) -> PathBuf {
    let absolute = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
    absolute.components().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_converts_backslashes() {
        assert_eq!(slash(Path::new(r"D:\OmniPHP\www\shop")), "D:/OmniPHP/www/shop");
    }

    #[test]
    fn root_is_written_the_same_way_however_it_was_given() {
        assert_eq!(normalized(Path::new("C:/OmniPHP/")), Path::new(r"C:\OmniPHP"));
        assert_eq!(normalized(Path::new(r"C:\OmniPHP\\")), Path::new(r"C:\OmniPHP"));
        assert_eq!(normalized(Path::new(r"C:\OmniPHP\www\..")), Path::new(r"C:\OmniPHP"));
        assert_eq!(normalized(Path::new(r"C:\OmniPHP")), Path::new(r"C:\OmniPHP"));
    }

    #[test]
    fn install_key_identifies_the_install_folder() {
        let key = Paths::new(r"C:\OmniPHP").install_key();
        assert_eq!(key, Paths::new(r"c:\omniphp").install_key(), "paths differing only in case are the same install");
        assert_ne!(key, Paths::new(r"D:\OmniPHP").install_key());
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()), "must be usable in a kernel object name");
    }

    #[test]
    fn derived_paths_stay_under_root() {
        let paths = Paths::new(r"D:\OmniPHP");
        assert_eq!(slash(&paths.apache_conf()), "D:/OmniPHP/etc/apache/httpd.conf");
        assert_eq!(slash(&paths.php_site_dir("shop")), "D:/OmniPHP/etc/php/sites/shop");
    }
}
