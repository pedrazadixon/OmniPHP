//! Settings files: `etc\omniphp\omniphp.toml` (global) and
//! `etc\omniphp\sites.toml` (per-site overrides, keyed by folder name).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub general: GeneralConfig,
    pub sites: SitesConfig,
    pub apache: ApacheConfig,
    pub mysql: MysqlConfig,
    pub mailpit: MailpitConfig,
    pub redis: RedisConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    /// Follow the Windows light/dark setting.
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralConfig {
    /// Start every enabled service when the app opens.
    pub autostart_services: bool,
    /// Open in the tray without showing the window.
    pub start_minimized: bool,
    pub theme: ThemeChoice,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            autostart_services: false,
            start_minimized: false,
            theme: ThemeChoice::System,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MysqlConfig {
    pub enabled: bool,
    /// Runtime id (folder name under `bin\mysql`). Newest when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub port: u16,
}

impl Default for MysqlConfig {
    fn default() -> Self {
        Self { enabled: true, version: None, port: 3306 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MailpitConfig {
    pub enabled: bool,
    /// Runtime id (folder name under `bin\mailpit`). Newest when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Where PHP delivers outgoing mail.
    pub smtp_port: u16,
    /// The inbox web page.
    pub ui_port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RedisConfig {
    /// Off unless switched on: most PHP projects do not need Redis.
    pub enabled: bool,
    /// Runtime id (folder name under `bin\redis`). Newest when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub port: u16,
}

impl Default for RedisConfig {
    fn default() -> Self {
        Self { enabled: false, version: None, port: 6379 }
    }
}

impl Default for MailpitConfig {
    fn default() -> Self {
        Self { enabled: true, version: None, smtp_port: 1025, ui_port: 8025 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SitesConfig {
    /// Folder whose sub-folders become sites. Defaults to `<root>\www`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub www_dir: Option<PathBuf>,
    /// `{name}` is replaced by the site's folder name.
    pub hostname_pattern: String,
    /// Runtime id (folder name under `bin\php`) used by sites that pick none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_php: Option<String>,
}

impl Default for SitesConfig {
    fn default() -> Self {
        Self {
            www_dir: None,
            hostname_pattern: "{name}.test".into(),
            default_php: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ApacheConfig {
    pub enabled: bool,
    /// Runtime id (folder name under `bin\apache`). Newest when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Address Apache binds to. `127.0.0.1` keeps sites private to this PC;
    /// use `0.0.0.0` to reach them from other devices on the network.
    pub listen: String,
    pub http_port: u16,
    /// Serve every site over HTTPS as well, with locally issued certificates.
    pub ssl: bool,
    pub https_port: u16,
}

impl Default for ApacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            version: None,
            listen: "127.0.0.1".into(),
            http_port: 80,
            ssl: true,
            https_port: 443,
        }
    }
}

/// Per-site overrides. Anything left unset is detected or inherited.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SiteSettings {
    /// PHP runtime id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub php: Option<String>,
    /// Relative to the site folder; empty string means the folder itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document_root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    /// Enabled PHP extensions. Unset means the default set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<String>>,
    /// php.ini directives for this site only, such as `memory_limit = 1G`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub ini: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SitesFile {
    pub sites: BTreeMap<String, SiteSettings>,
}

/// Reads a TOML file; a missing file yields the defaults.
pub fn load<T: DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("invalid {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error).with_context(|| format!("cannot read {}", path.display())),
    }
}

pub fn save<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = toml::to_string_pretty(value)?;
    std::fs::write(path, text).with_context(|| format!("cannot write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_gives_defaults() {
        let config: Config = load(Path::new(r"Z:\does\not\exist.toml")).unwrap();
        assert_eq!(config, Config::default());
        assert_eq!(config.sites.hostname_pattern, "{name}.test");
        assert_eq!(config.apache.http_port, 80);
    }

    #[test]
    fn partial_file_keeps_other_defaults() {
        let config: Config = toml::from_str("[apache]\nhttp_port = 8080\n").unwrap();
        assert_eq!(config.apache.http_port, 8080);
        assert_eq!(config.apache.listen, "127.0.0.1");
        assert!(!config.general.autostart_services, "services start only when asked, unless switched on");
    }

    #[test]
    fn sites_file_round_trips() {
        let mut file = SitesFile::default();
        file.sites.insert(
            "shop".into(),
            SiteSettings {
                php: Some("php-8.3.37-Win32-vs16-x64".into()),
                document_root: Some("public".into()),
                extensions: Some(vec!["curl".into(), "gd".into()]),
                ..Default::default()
            },
        );
        let text = toml::to_string_pretty(&file).unwrap();
        assert_eq!(toml::from_str::<SitesFile>(&text).unwrap(), file);
    }
}
