//! Sites: every top-level folder of the sites directory, combined with its
//! saved overrides and sensible detection for everything left unset.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::config::{Config, SitesFile};
use super::runtimes::PhpRuntime;

/// A fully resolved site, ready to turn into a vhost and a php.ini.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Site {
    /// Folder name; also the key in `sites.toml`.
    pub name: String,
    pub dir: PathBuf,
    pub hostname: String,
    pub aliases: Vec<String>,
    pub document_root: PathBuf,
    /// PHP runtime id, or `None` when no PHP is installed.
    pub php: Option<String>,
    pub extensions: Vec<String>,
    /// php.ini directives set for this site only.
    pub ini: BTreeMap<String, String>,
}

/// Sub-folders that frameworks use as their public directory, in priority order.
pub const PUBLIC_DIRS: [&str; 4] = ["public", "public_html", "webroot", "web"];

/// Extensions enabled for a site that has not chosen its own, in load order.
/// Names missing from a given PHP build are skipped (`gd2` is PHP 7's `gd`).
const DEFAULT_EXTENSIONS: [&str; 16] = [
    "curl", "fileinfo", "gd", "gd2", "intl", "mbstring", "exif", "mysqli", "openssl",
    "pdo_mysql", "pdo_sqlite", "sodium", "sqlite3", "xsl", "zip", "opcache",
];

/// Site folders in `www`, sorted by name. Hidden folders are skipped.
pub fn scan_folders(www: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(www) else {
        return Vec::new();
    };
    let mut folders: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| (entry.file_name().to_string_lossy().into_owned(), entry.path()))
        .filter(|(name, _)| !name.starts_with('.'))
        .collect();
    folders.sort_by_key(|(name, _)| name.to_lowercase());
    folders
}

/// The public sub-folder of a project, if it has one that holds an index file.
/// A bare `public\` folder without an index is ignored, as Laragon does.
pub fn detect_document_root(dir: &Path) -> Option<&'static str> {
    PUBLIC_DIRS.into_iter().find(|sub| {
        let candidate = dir.join(sub);
        candidate.join("index.php").is_file() || candidate.join("index.html").is_file()
    })
}

/// Applies the hostname pattern to a folder name: lower-case, with anything a
/// hostname cannot hold turned into `-`. Underscores are kept, as browsers and
/// Apache accept them and existing Laragon projects rely on them.
pub fn hostname_for(name: &str, pattern: &str) -> String {
    let mut label = String::with_capacity(name.len());
    for c in name.to_lowercase().chars() {
        let keep = c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
        match (keep, label.ends_with('-')) {
            (true, _) => label.push(c),
            (false, false) => label.push('-'),
            (false, true) => {}
        }
    }
    let label = label.trim_matches(['-', '.']);
    pattern.replace("{name}", if label.is_empty() { "site" } else { label })
}

pub fn default_extensions(php: &PhpRuntime) -> Vec<String> {
    DEFAULT_EXTENSIONS
        .into_iter()
        .filter(|ext| php.extensions.iter().any(|have| have == ext))
        .map(str::to_string)
        .collect()
}

/// The name under which `php` ships a chosen extension, if it ships it. GD is
/// `gd2` in PHP 7 and `gd` in PHP 8, so a site keeps it when switching versions.
fn available_as(extension: &str, php: &PhpRuntime) -> Option<String> {
    let renamed = match extension {
        "gd" => "gd2",
        "gd2" => "gd",
        _ => extension,
    };
    [extension, renamed]
        .into_iter()
        .find(|name| php.extensions.iter().any(|have| have == name))
        .map(str::to_string)
}

/// Sorts extensions so dependencies load first (`exif` needs `mbstring`), in
/// the order of [`DEFAULT_EXTENSIONS`], with everything else after, by name.
fn in_load_order(mut extensions: Vec<String>) -> Vec<String> {
    let rank = |ext: &String| {
        DEFAULT_EXTENSIONS.iter().position(|known| known == ext).unwrap_or(usize::MAX)
    };
    extensions.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.cmp(b)));
    extensions.dedup();
    extensions
}

/// The runtime used when a site names none: the configured default if it is
/// installed, otherwise the newest (`phps` is sorted newest first).
pub fn default_php<'a>(config: &Config, phps: &'a [PhpRuntime]) -> Option<&'a PhpRuntime> {
    config
        .sites
        .default_php
        .as_ref()
        .and_then(|id| phps.iter().find(|php| &php.id == id))
        .or_else(|| phps.first())
}

pub fn resolve(
    folders: &[(String, PathBuf)],
    settings: &SitesFile,
    config: &Config,
    phps: &[PhpRuntime],
) -> Vec<Site> {
    let fallback = default_php(config, phps);
    folders
        .iter()
        .map(|(name, dir)| {
            let saved = settings.sites.get(name).cloned().unwrap_or_default();
            let php = saved
                .php
                .as_ref()
                .and_then(|id| phps.iter().find(|php| &php.id == id))
                .or(fallback);
            let relative_root = match &saved.document_root {
                Some(relative) => relative.trim_matches(['/', '\\']).to_string(),
                None => detect_document_root(dir).unwrap_or_default().to_string(),
            };
            let document_root = if relative_root.is_empty() {
                dir.clone()
            } else {
                dir.join(relative_root)
            };
            let extensions = match (&saved.extensions, php) {
                // Keep only what this PHP build can actually load.
                (Some(chosen), Some(php)) => in_load_order(
                    chosen.iter().filter_map(|ext| available_as(ext, php)).collect(),
                ),
                (None, Some(php)) => default_extensions(php),
                (_, None) => Vec::new(),
            };
            Site {
                hostname: saved
                    .hostname
                    .filter(|h| !h.trim().is_empty())
                    .unwrap_or_else(|| hostname_for(name, &config.sites.hostname_pattern)),
                aliases: saved.aliases,
                document_root,
                php: php.map(|php| php.id.clone()),
                extensions,
                ini: saved.ini,
                name: name.clone(),
                dir: dir.clone(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::SiteSettings;

    fn php(id: &str, version: &str, extensions: &[&str]) -> PhpRuntime {
        PhpRuntime {
            id: id.into(),
            version: version.into(),
            thread_safe: false,
            dir: PathBuf::from(format!(r"D:\OmniPHP\bin\php\{id}")),
            extensions: extensions.iter().map(|e| e.to_string()).collect(),
        }
    }

    #[test]
    fn hostnames_are_lowercase_and_safe() {
        assert_eq!(hostname_for("shop", "{name}.test"), "shop.test");
        assert_eq!(hostname_for("Dev_SAC", "{name}.test"), "dev_sac.test");
        assert_eq!(hostname_for("My  Shop!", "{name}.test"), "my-shop.test");
        assert_eq!(hostname_for("portal.whatsper", "{name}.test"), "portal.whatsper.test");
        assert_eq!(hostname_for("café", "{name}.localhost"), "caf.localhost");
        assert_eq!(hostname_for("!!!", "{name}.test"), "site.test");
    }

    #[test]
    fn detects_public_folder_only_with_an_index() {
        let tmp = tempfile::tempdir().unwrap();
        let site = tmp.path().join("app");
        std::fs::create_dir_all(site.join("public")).unwrap();
        assert_eq!(detect_document_root(&site), None);
        std::fs::write(site.join("public").join("index.php"), "<?php").unwrap();
        assert_eq!(detect_document_root(&site), Some("public"));
    }

    #[test]
    fn scan_skips_files_and_hidden_folders() {
        let tmp = tempfile::tempdir().unwrap();
        for dir in ["beta", "Alpha", ".git"] {
            std::fs::create_dir(tmp.path().join(dir)).unwrap();
        }
        std::fs::write(tmp.path().join("readme.txt"), "").unwrap();
        let names: Vec<String> = scan_folders(tmp.path()).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["Alpha", "beta"]);
    }

    #[test]
    fn resolve_applies_overrides_and_defaults() {
        let phps = [
            php("php-8.4", "8.4.28", &["curl", "gd", "mbstring", "redis"]),
            php("php-7.4", "7.4.33", &["curl", "gd2", "mbstring"]),
        ];
        let folders = vec![
            ("shop".to_string(), PathBuf::from(r"D:\www\shop")),
            ("legacy".to_string(), PathBuf::from(r"D:\www\legacy")),
        ];
        let mut settings = SitesFile::default();
        settings.sites.insert(
            "legacy".into(),
            SiteSettings {
                php: Some("php-7.4".into()),
                document_root: Some("htdocs".into()),
                hostname: Some("old.test".into()),
                extensions: Some(vec!["gd2".into(), "redis".into()]),
                ..Default::default()
            },
        );
        let sites = resolve(&folders, &settings, &Config::default(), &phps);

        assert_eq!(sites[0].hostname, "shop.test");
        assert_eq!(sites[0].php.as_deref(), Some("php-8.4"));
        assert_eq!(sites[0].extensions, ["curl", "gd", "mbstring"]);
        assert_eq!(sites[0].document_root, PathBuf::from(r"D:\www\shop"));

        assert_eq!(sites[1].hostname, "old.test");
        assert_eq!(sites[1].php.as_deref(), Some("php-7.4"));
        // redis is not built for 7.4 here, so it is dropped rather than breaking PHP.
        assert_eq!(sites[1].extensions, ["gd2"]);
        assert_eq!(sites[1].document_root, PathBuf::from(r"D:\www\legacy\htdocs"));
    }

    #[test]
    fn gd_follows_the_site_across_php_versions() {
        let phps = [php("php-8.4", "8.4.28", &["gd", "mbstring"])];
        let folders = vec![("old".to_string(), PathBuf::from(r"D:\www\old"))];
        let mut settings = SitesFile::default();
        settings.sites.insert(
            "old".into(),
            SiteSettings { extensions: Some(vec!["gd2".into(), "mbstring".into()]), ..Default::default() },
        );
        let sites = resolve(&folders, &settings, &Config::default(), &phps);
        assert_eq!(sites[0].extensions, ["gd", "mbstring"]);
    }

    #[test]
    fn configured_default_php_wins_over_newest() {
        let phps = [php("php-8.6", "8.6.0", &[]), php("php-8.4", "8.4.28", &[])];
        let mut config = Config::default();
        assert_eq!(default_php(&config, &phps).unwrap().id, "php-8.6");
        config.sites.default_php = Some("php-8.4".into());
        assert_eq!(default_php(&config, &phps).unwrap().id, "php-8.4");
        config.sites.default_php = Some("php-gone".into());
        assert_eq!(default_php(&config, &phps).unwrap().id, "php-8.6");
    }
}
