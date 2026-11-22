//! The new-site wizard: creates a site folder and installs a popular PHP
//! application into it.
//!
//! Installing can take minutes (downloads, Composer), so it runs from an
//! [`InstallJob`], a plain description of the work that needs no access to the
//! engine while it runs.

use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::proc::quiet_command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Template {
    Blank,
    WordPress,
    Laravel,
    CodeIgniter,
    Symfony,
    PrestaShop,
}

impl Template {
    pub const ALL: [Template; 6] = [
        Template::Blank,
        Template::WordPress,
        Template::Laravel,
        Template::CodeIgniter,
        Template::Symfony,
        Template::PrestaShop,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Template::Blank => "Blank",
            Template::WordPress => "WordPress",
            Template::Laravel => "Laravel",
            Template::CodeIgniter => "CodeIgniter 4",
            Template::Symfony => "Symfony",
            Template::PrestaShop => "PrestaShop",
        }
    }

    /// How the template gets installed, shown under its name.
    pub fn note(self) -> &'static str {
        match self {
            Template::Blank => "Empty folder with index.php",
            Template::WordPress => "Downloads the latest release",
            Template::PrestaShop => "Downloads PrestaShop 8 (PHP 8.1 or older)",
            Template::Laravel | Template::CodeIgniter | Template::Symfony => "Installed with Composer",
        }
    }

    /// Name used on the command line.
    pub fn id(self) -> &'static str {
        match self {
            Template::Blank => "blank",
            Template::WordPress => "wordpress",
            Template::Laravel => "laravel",
            Template::CodeIgniter => "codeigniter",
            Template::Symfony => "symfony",
            Template::PrestaShop => "prestashop",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|template| template.id() == id)
    }

    /// The newest PHP series the template is known to run on, when that is
    /// older than current PHP. PrestaShop 9 is not published as a download on
    /// GitHub, so the wizard installs the 8.x line, which stops at PHP 8.1.
    pub fn max_php(self) -> Option<&'static str> {
        match self {
            Template::PrestaShop => Some("8.1"),
            _ => None,
        }
    }

    fn composer_package(self) -> Option<&'static str> {
        match self {
            Template::Laravel => Some("laravel/laravel"),
            Template::CodeIgniter => Some("codeigniter4/appstarter"),
            Template::Symfony => Some("symfony/skeleton"),
            _ => None,
        }
    }
}

/// The database created for a new site: user `root`, no password.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Database {
    pub name: String,
    pub port: u16,
}

/// Everything the installer needs, gathered up front.
#[derive(Debug, Clone)]
pub struct InstallJob {
    pub template: Template,
    pub name: String,
    /// The new, empty site folder.
    pub dir: PathBuf,
    /// The address the site will have, e.g. `https://shop.test`.
    pub url: String,
    pub php_exe: PathBuf,
    /// The site's generated php.ini, so Composer runs with its extensions.
    pub php_ini: PathBuf,
    pub composer: PathBuf,
    pub tmp: PathBuf,
    pub log: PathBuf,
    pub database: Option<Database>,
}

/// Folder names that work as a hostname label and a database name.
pub fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 50
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_'));
    if !ok {
        bail!("Use lower-case letters, digits, dashes and underscores for the name, starting with a letter or digit.");
    }
    Ok(())
}

pub fn database_name(site: &str) -> String {
    site.replace('-', "_")
}

/// Installs the template into `job.dir`, reporting each step through `progress`.
pub fn install(job: &InstallJob, progress: &dyn Fn(&str)) -> Result<()> {
    match job.template {
        Template::Blank => {
            let page = format!(
                "<?php\n// {} — created by OmniPHP.\necho '<h1>{}</h1><p>PHP ' . PHP_VERSION . '</p>';\n",
                job.url, job.name
            );
            std::fs::write(job.dir.join("index.php"), page)?;
        }
        Template::WordPress => {
            let zip = download("https://wordpress.org/latest.zip", "wordpress.zip", job, progress)?;
            progress("Unpacking WordPress…");
            extract_unwrapped(&zip, &job.dir)?;
            if let Some(database) = &job.database {
                configure_wordpress(&job.dir, database)?;
            }
        }
        Template::PrestaShop => {
            progress("Looking up the latest PrestaShop download…");
            // Newest first; not every release carries a zip.
            let release = http_get("https://api.github.com/repos/PrestaShop/PrestaShop/releases?per_page=40")?
                .body_mut()
                .read_to_string()
                .context("GitHub did not answer")?;
            let url = release_zip_url(&release)
                .context("No recent PrestaShop release offers a zip download.")?;
            let zip = download(&url, "prestashop.zip", job, progress)?;
            progress("Unpacking PrestaShop…");
            extract_unwrapped(&zip, &job.dir)?;
            // The release zip wraps the real files in a second zip.
            let inner = job.dir.join("prestashop.zip");
            if inner.is_file() {
                extract_unwrapped(&inner, &job.dir)?;
                let _ = std::fs::remove_file(&inner);
            }
        }
        Template::Laravel | Template::CodeIgniter | Template::Symfony => {
            composer_create_project(job, progress)?;
            configure_framework(job)?;
        }
    }
    Ok(())
}

/// Steps that need the site's database to exist and MySQL to be running.
pub fn after_database(job: &InstallJob) -> Result<()> {
    if job.template != Template::Laravel || job.database.is_none() {
        return Ok(());
    }
    // Laravel keeps sessions, cache and jobs in the database by default, so a
    // fresh project errors on its first page until its tables exist.
    let output = quiet_command(&job.php_exe)
        .arg("-c")
        .arg(&job.php_ini)
        .args(["artisan", "migrate", "--force", "--no-interaction"])
        .current_dir(&job.dir)
        .output()
        .context("cannot run artisan")?;
    if !output.status.success() {
        bail!(
            "Laravel was installed, but creating its tables failed. Run \"php artisan migrate\" in {}.\n{}",
            job.dir.display(),
            String::from_utf8_lossy(&output.stdout).trim()
        );
    }
    Ok(())
}

// ---- downloads and archives -------------------------------------------------

pub(super) fn http_get(url: &str) -> Result<ureq::http::Response<ureq::Body>> {
    ureq::get(url)
        // GitHub's API refuses requests without a client name.
        .header("User-Agent", "omniphp-local-dev")
        .call()
        .with_context(|| format!("cannot download {url}"))
}

fn download(url: &str, file_name: &str, job: &InstallJob, progress: &dyn Fn(&str)) -> Result<PathBuf> {
    progress(&format!("Downloading {}…", job.template.label()));
    let dir = job.tmp.join("downloads");
    std::fs::create_dir_all(&dir)?;
    let target = dir.join(file_name);
    let mut response = http_get(url)?;
    let mut file = File::create(&target).with_context(|| format!("cannot write {}", target.display()))?;
    std::io::copy(&mut response.body_mut().as_reader(), &mut file)
        .with_context(|| format!("the download of {url} was interrupted"))?;
    Ok(target)
}

/// First `.zip` download in a GitHub release (or list of releases).
fn release_zip_url(release_json: &str) -> Option<String> {
    release_json
        .split("\"browser_download_url\"")
        .skip(1)
        .filter_map(|rest| {
            let start = rest.find("\"")? + 1;
            let end = start + rest[start..].find('"')?;
            Some(rest[start..end].to_string())
        })
        .find(|url| url.ends_with(".zip"))
}

/// Extracts `zip` into `dest`. When the archive holds a single top-level
/// folder (`wordpress/…`), its contents go straight into `dest`.
///
/// Files are written directly to their final place. Unpacking elsewhere and
/// moving the result fails on Windows when antivirus is still scanning the
/// new folders ("Access is denied").
pub(super) fn extract_unwrapped(zip: &Path, dest: &Path) -> Result<()> {
    let unusable = || format!("{} is not a usable zip file", zip.display());
    let file = File::open(zip).with_context(|| format!("cannot open {}", zip.display()))?;
    let mut archive = zip::ZipArchive::new(file).with_context(unusable)?;

    // `enclosed_name` refuses entries that would escape the destination.
    let mut names = Vec::with_capacity(archive.len());
    for index in 0..archive.len() {
        let entry = archive.by_index(index).with_context(unusable)?;
        names.push(entry.enclosed_name());
    }
    let wrapper = common_root(names.iter().flatten().map(PathBuf::as_path));

    for (index, name) in names.iter().enumerate() {
        let Some(name) = name else { continue };
        let relative = match &wrapper {
            Some(root) => name.strip_prefix(root).unwrap_or(name),
            None => name,
        };
        if relative.as_os_str().is_empty() {
            continue;
        }
        let target = dest.join(relative);
        let mut entry = archive.by_index(index).with_context(unusable)?;
        if entry.is_dir() {
            std::fs::create_dir_all(&target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = File::create(&target).with_context(|| format!("cannot write {}", target.display()))?;
        std::io::copy(&mut entry, &mut out).with_context(unusable)?;
    }
    Ok(())
}

/// The single top-level folder every path sits under, if there is one.
fn common_root<'a>(mut paths: impl Iterator<Item = &'a Path>) -> Option<PathBuf> {
    let first = paths.next()?;
    let root = PathBuf::from(first.components().next()?.as_os_str());
    // A file directly at the top level means there is no wrapping folder.
    let wrapped = |path: &Path| path.starts_with(&root) && path != root.as_path();
    let first_ok = wrapped(first) || first.components().count() == 1 && first == root.as_path();
    (first_ok && paths.all(|path| wrapped(path) || path == root.as_path())).then_some(root)
}

// ---- Composer ---------------------------------------------------------------

fn composer_create_project(job: &InstallJob, progress: &dyn Fn(&str)) -> Result<()> {
    let package = job.template.composer_package().expect("composer template");
    if !job.composer.is_file() {
        bail!("Composer is missing: expected {}", job.composer.display());
    }
    progress(&format!("Installing {} with Composer. This can take a few minutes…", job.template.label()));
    let log = File::create(&job.log).with_context(|| format!("cannot write {}", job.log.display()))?;
    let status = quiet_command(&job.php_exe)
        .arg("-c")
        .arg(&job.php_ini)
        .arg(&job.composer)
        .args(["create-project", package])
        .arg(&job.dir)
        .args(["--no-interaction", "--prefer-dist", "--no-progress"])
        .current_dir(job.dir.parent().unwrap_or(&job.dir))
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .status()
        .context("cannot run Composer")?;
    if !status.success() {
        let output = std::fs::read_to_string(&job.log).unwrap_or_default();
        let lines: Vec<&str> = output.lines().filter(|line| !line.trim().is_empty()).collect();
        bail!(
            "Composer could not install {}.\n{}\nFull output: {}",
            job.template.label(),
            lines[lines.len().saturating_sub(6)..].join("\n"),
            job.log.display()
        );
    }
    Ok(())
}

// ---- application configuration ---------------------------------------------

fn configure_framework(job: &InstallJob) -> Result<()> {
    let database = job.database.as_ref();
    match job.template {
        Template::Laravel => edit_file(&job.dir.join(".env"), |env| {
            let mut env = set_env(env, "APP_URL", &job.url);
            if let Some(database) = database {
                for (key, value) in [
                    ("DB_CONNECTION", "mysql".to_string()),
                    ("DB_HOST", "127.0.0.1".to_string()),
                    ("DB_PORT", database.port.to_string()),
                    ("DB_DATABASE", database.name.clone()),
                    ("DB_USERNAME", "root".to_string()),
                    ("DB_PASSWORD", String::new()),
                ] {
                    env = set_env(&env, key, &value);
                }
            }
            env
        }),
        Template::CodeIgniter => {
            // CodeIgniter ships its settings as `env`; `.env` is the live copy.
            let sample = std::fs::read_to_string(job.dir.join("env")).unwrap_or_default();
            let mut env = set_env(&sample, "CI_ENVIRONMENT", "development");
            env = set_env(&env, "app.baseURL", &format!("'{}/'", job.url));
            if let Some(database) = database {
                for (key, value) in [
                    ("database.default.hostname", "127.0.0.1".to_string()),
                    ("database.default.database", database.name.clone()),
                    ("database.default.username", "root".to_string()),
                    ("database.default.password", String::new()),
                    ("database.default.DBDriver", "MySQLi".to_string()),
                    ("database.default.port", database.port.to_string()),
                ] {
                    env = set_env(&env, key, &value);
                }
            }
            std::fs::write(job.dir.join(".env"), env)?;
            Ok(())
        }
        Template::Symfony => {
            if let Some(database) = database {
                // Read by Doctrine once it is installed; harmless before that.
                let line = format!(
                    "DATABASE_URL=\"mysql://root:@127.0.0.1:{}/{}?charset=utf8mb4\"\n",
                    database.port, database.name
                );
                std::fs::write(job.dir.join(".env.local"), line)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn edit_file(file: &Path, change: impl FnOnce(&str) -> String) -> Result<()> {
    let text = std::fs::read_to_string(file).with_context(|| format!("cannot read {}", file.display()))?;
    std::fs::write(file, change(&text)).with_context(|| format!("cannot write {}", file.display()))
}

/// Sets `key` in dotenv-style text: replaces the existing line (also when it
/// is commented out), or appends one. Keeps the file's own `=` spacing.
pub fn set_env(text: &str, key: &str, value: &str) -> String {
    let mut found = false;
    let mut lines: Vec<String> = text
        .lines()
        .map(|line| {
            let bare = line.trim_start().trim_start_matches('#').trim_start();
            let is_key = bare
                .strip_prefix(key)
                .is_some_and(|rest| rest.trim_start().starts_with('='));
            if !is_key || found {
                return line.to_string();
            }
            found = true;
            let spaced = bare[key.len()..].starts_with(' ');
            if spaced { format!("{key} = {value}") } else { format!("{key}={value}") }
        })
        .collect();
    if !found {
        lines.push(format!("{key}={value}"));
    }
    lines.join("\n") + "\n"
}

fn configure_wordpress(dir: &Path, database: &Database) -> Result<()> {
    let sample = std::fs::read_to_string(dir.join("wp-config-sample.php"))
        .context("the WordPress download has no wp-config-sample.php")?;
    std::fs::write(dir.join("wp-config.php"), render_wp_config(&sample, database, &random_token))?;
    Ok(())
}

pub fn render_wp_config(sample: &str, database: &Database, token: &dyn Fn() -> String) -> String {
    let config = sample
        .replace("database_name_here", &database.name)
        .replace("username_here", "root")
        .replace("password_here", "")
        .replace("'localhost'", &format!("'127.0.0.1:{}'", database.port));
    // Each secret key gets its own random value.
    let mut out = String::with_capacity(config.len() + 512);
    let mut rest = config.as_str();
    const PLACEHOLDER: &str = "put your unique phrase here";
    while let Some(at) = rest.find(PLACEHOLDER) {
        out.push_str(&rest[..at]);
        out.push_str(&token());
        rest = &rest[at + PLACEHOLDER.len()..];
    }
    out.push_str(rest);
    out
}

/// 64 unpredictable characters. `RandomState` is seeded by the operating
/// system's random generator, which is plenty for a development site's salts.
fn random_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789!#%&()*+,-./:;<=>?@[]^_{|}~";
    let mut token = String::with_capacity(64);
    while token.len() < 64 {
        let mut bits = std::collections::hash_map::RandomState::new().build_hasher().finish();
        for _ in 0..8 {
            token.push(ALPHABET[(bits % ALPHABET.len() as u64) as usize] as char);
            bits /= ALPHABET.len() as u64;
        }
    }
    token
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_must_work_as_folder_host_and_database() {
        for good in ["shop", "my-store", "api_v2", "2fast"] {
            assert!(validate_name(good).is_ok(), "{good}");
        }
        for bad in ["", "My Shop", "-shop", "shop.test", "tienda/2", "ñandú"] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
        assert_eq!(database_name("my-store"), "my_store");
    }

    #[test]
    fn set_env_replaces_uncomments_or_appends() {
        let laravel = "APP_URL=http://localhost\nDB_CONNECTION=sqlite\n# DB_HOST=127.0.0.1\n";
        let env = set_env(laravel, "DB_CONNECTION", "mysql");
        let env = set_env(&env, "DB_HOST", "127.0.0.1");
        let env = set_env(&env, "DB_DATABASE", "shop");
        assert_eq!(env, "APP_URL=http://localhost\nDB_CONNECTION=mysql\nDB_HOST=127.0.0.1\nDB_DATABASE=shop\n");

        let codeigniter = "# CI_ENVIRONMENT = production\n# database.default.database = ci4\n";
        let env = set_env(codeigniter, "CI_ENVIRONMENT", "development");
        let env = set_env(&env, "database.default.database", "shop");
        assert_eq!(env, "CI_ENVIRONMENT = development\ndatabase.default.database = shop\n");

        // A key that is only a prefix of another must not be mistaken for it.
        assert_eq!(set_env("DB_HOST_READ=x\n", "DB_HOST", "y"), "DB_HOST_READ=x\nDB_HOST=y\n");
    }

    #[test]
    fn wp_config_gets_database_and_distinct_salts() {
        let sample = "define( 'DB_NAME', 'database_name_here' );\ndefine( 'DB_USER', 'username_here' );\n\
                      define( 'DB_PASSWORD', 'password_here' );\ndefine( 'DB_HOST', 'localhost' );\n\
                      define( 'AUTH_KEY', 'put your unique phrase here' );\ndefine( 'NONCE_KEY', 'put your unique phrase here' );\n";
        let counter = std::cell::Cell::new(0);
        let config = render_wp_config(sample, &Database { name: "blog".into(), port: 3307 }, &|| {
            counter.set(counter.get() + 1);
            format!("salt{}", counter.get())
        });
        assert!(config.contains("'DB_NAME', 'blog'"));
        assert!(config.contains("'DB_USER', 'root'"));
        assert!(config.contains("'DB_PASSWORD', ''"));
        assert!(config.contains("'DB_HOST', '127.0.0.1:3307'"));
        assert!(config.contains("'AUTH_KEY', 'salt1'") && config.contains("'NONCE_KEY', 'salt2'"));
    }

    #[test]
    fn random_tokens_differ_and_avoid_quotes() {
        let (a, b) = (random_token(), random_token());
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
        assert!(!a.contains(['\'', '"', '\\', '$']));
    }

    #[test]
    fn detects_a_wrapping_folder_in_an_archive() {
        let paths = |list: &[&str]| -> Vec<PathBuf> { list.iter().map(PathBuf::from).collect() };
        let wrapped = paths(&["wordpress/", "wordpress/index.php", "wordpress/wp-admin/x.php"]);
        assert_eq!(common_root(wrapped.iter().map(PathBuf::as_path)), Some(PathBuf::from("wordpress")));
        let flat = paths(&["index.php", "prestashop.zip", "docs/readme.txt"]);
        assert_eq!(common_root(flat.iter().map(PathBuf::as_path)), None);
        let mixed = paths(&["app/index.php", "readme.txt"]);
        assert_eq!(common_root(mixed.iter().map(PathBuf::as_path)), None);
    }

    #[test]
    fn finds_the_zip_in_a_github_release() {
        let json = r#"{"assets":[{"browser_download_url":"https://x/notes.xml"},{"browser_download_url":"https://x/prestashop_9.zip"}]}"#;
        assert_eq!(release_zip_url(json).as_deref(), Some("https://x/prestashop_9.zip"));
        assert_eq!(release_zip_url("{}"), None);
    }
}
