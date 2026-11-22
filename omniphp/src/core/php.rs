//! Per-site `php.ini` generation.
//!
//! Each site gets its own folder with a `php.ini`; Apache passes that folder to
//! php-cgi as `PHPRC`. The file is the PHP build's pristine
//! `php.ini-development` followed by a managed block. PHP lets a later
//! directive override an earlier one, so the block wins without having to edit
//! the stock text.

use std::fmt::Write as _;
use std::path::PathBuf;

use anyhow::{Context, Result};

use super::paths::{Paths, slash};
use super::runtimes::PhpRuntime;
use super::sites::Site;

/// Extensions that load through `zend_extension=` rather than `extension=`.
const ZEND_EXTENSIONS: [&str; 2] = ["opcache", "xdebug"];

/// The stock ini shipped with a PHP build, or empty if it ships none.
pub fn base_ini(php: &PhpRuntime) -> String {
    ["php.ini-development", "php.ini-production"]
        .into_iter()
        .find_map(|name| std::fs::read_to_string(php.dir.join(name)).ok())
        .unwrap_or_default()
}

/// Where PHP's `mail()` should deliver: the local Mailpit, which catches every
/// message instead of sending it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailCatcher {
    pub exe: PathBuf,
    pub smtp_port: u16,
}

pub fn render_site_ini(
    base: &str,
    site: &Site,
    php: &PhpRuntime,
    mail: Option<&MailCatcher>,
    paths: &Paths,
) -> String {
    let mut ini = String::with_capacity(base.len() + 1024);
    ini.push_str(base.trim_end());
    ini.push_str("\n\n; ---- Managed by OmniPHP: everything below is regenerated ----\n[PHP]\n");
    let _ = writeln!(ini, "extension_dir = \"{}\"", slash(&php.dir.join("ext")));
    for ext in &site.extensions {
        let directive = if ZEND_EXTENSIONS.contains(&ext.as_str()) {
            "zend_extension"
        } else {
            "extension"
        };
        let _ = writeln!(ini, "{directive}={ext}");
    }
    let tmp = slash(&paths.tmp());
    let _ = writeln!(ini, "error_log = \"{}/php-{}.log\"", slash(&paths.logs()), site.name);
    ini.push_str("log_errors = On\ndisplay_errors = On\n");
    let _ = writeln!(ini, "session.save_path = \"{tmp}/sessions\"");
    let _ = writeln!(ini, "upload_tmp_dir = \"{tmp}\"");
    let _ = writeln!(ini, "sys_temp_dir = \"{tmp}\"");
    ini.push_str(
        "memory_limit = 512M\n\
         upload_max_filesize = 128M\n\
         post_max_size = 128M\n\
         max_execution_time = 120\n\
         cgi.fix_pathinfo = 1\n",
    );
    if let Some(mail) = mail {
        // PHP on Windows speaks SMTP itself. Going through Mailpit's sendmail
        // program instead (`sendmail_path`) fails under FastCGI: mail()
        // reports "Sendmail exited with non-zero exit code 2".
        let _ = writeln!(ini, "SMTP = 127.0.0.1\nsmtp_port = {}", mail.smtp_port);
        ini.push_str("sendmail_from = \"omniphp@localhost\"\n");
    }
    if !site.ini.is_empty() {
        // Last in the file, so these win over everything above.
        ini.push_str("\n; ---- Chosen for this site in OmniPHP ----\n");
        for (directive, value) in &site.ini {
            let _ = writeln!(ini, "{directive} = {}", quote_value(value));
        }
    }
    ini
}

/// A php.ini setting the site page offers a field for.
pub struct SiteDirective {
    pub name: &'static str,
    pub label: &'static str,
    /// What applies when the field is left empty.
    pub default: &'static str,
}

/// The settings people most often need to change per project. Any other
/// directive can still be set by adding it to the site's `ini` table in
/// `sites.toml`.
pub const SITE_DIRECTIVES: [SiteDirective; 8] = [
    SiteDirective { name: "date.timezone", label: "Time zone", default: "UTC" },
    SiteDirective { name: "memory_limit", label: "Memory limit", default: "512M" },
    SiteDirective { name: "upload_max_filesize", label: "Largest upload", default: "128M" },
    SiteDirective { name: "post_max_size", label: "Largest form post", default: "128M" },
    SiteDirective { name: "max_execution_time", label: "Time limit (seconds)", default: "120" },
    SiteDirective { name: "max_input_vars", label: "Most form fields", default: "1000" },
    SiteDirective { name: "display_errors", label: "Show errors on the page", default: "On" },
    SiteDirective { name: "error_reporting", label: "Error reporting", default: "E_ALL" },
];

/// Settings OmniPHP writes itself; overriding them would stop the site's
/// PHP from finding its extensions or its log.
pub const MANAGED_DIRECTIVES: [&str; 4] = ["extension_dir", "extension", "zend_extension", "error_log"];

/// Every setting this PHP build knows, with its current value for the site
/// whose php.ini is `site_ini` (so settings of the site's extensions are
/// included). Sorted by name.
pub fn list_directives(php: &PhpRuntime, site_ini: &std::path::Path) -> Result<Vec<(String, String)>> {
    const SCRIPT: &str = "foreach (ini_get_all(null, false) as $k => $v) { \
                          echo $k, chr(9), str_replace([chr(13), chr(10)], ' ', (string) $v), chr(10); }";
    let output = super::proc::quiet_command(php.dir.join("php.exe"))
        .arg("-c")
        .arg(site_ini)
        .args(["-r", SCRIPT])
        .output()
        .with_context(|| format!("cannot run PHP {}", php.version))?;
    let mut directives: Vec<(String, String)> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .filter(|(name, _)| !MANAGED_DIRECTIVES.contains(name))
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
    if directives.is_empty() {
        anyhow::bail!(
            "PHP {} did not list its settings: {}",
            php.version,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    directives.sort();
    Ok(directives)
}

/// Refuses anything that could add a second line to php.ini or break out of a
/// quoted value.
pub fn validate_directive(name: &str, value: &str) -> Result<()> {
    let name_ok = !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.'));
    if !name_ok {
        anyhow::bail!("\"{name}\" is not a php.ini setting name.");
    }
    if MANAGED_DIRECTIVES.contains(&name) {
        anyhow::bail!("{name} is managed by OmniPHP and cannot be changed per site.");
    }
    if value.trim().is_empty() {
        anyhow::bail!("{name} has no value.");
    }
    if value.chars().any(|c| c.is_control() || c == '"') {
        anyhow::bail!("The value of {name} cannot contain quotes or line breaks.");
    }
    Ok(())
}

/// Plain words, numbers and constant expressions (`512M`, `On`,
/// `E_ALL & ~E_NOTICE`) go in as they are; anything else is quoted so PHP
/// reads it as one string (`America/Bogota`).
fn quote_value(value: &str) -> String {
    let value = value.trim();
    let bare = value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | ' ' | '&' | '|' | '~' | '^'));
    if bare { value.to_string() } else { format!("\"{value}\"") }
}

pub fn write_site_ini(
    site: &Site,
    php: &PhpRuntime,
    mail: Option<&MailCatcher>,
    paths: &Paths,
) -> Result<()> {
    let dir = paths.php_site_dir(&site.name);
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let ini = render_site_ini(&base_ini(php), site, php, mail, paths);
    let file = dir.join("php.ini");
    std::fs::write(&file, ini).with_context(|| format!("cannot write {}", file.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directives_cannot_smuggle_extra_lines() {
        assert!(validate_directive("memory_limit", "1G").is_ok());
        assert!(validate_directive("date.timezone", "America/Bogota").is_ok());
        assert!(validate_directive("memory_limit", "1G\nextension=evil").is_err());
        assert!(validate_directive("memory_limit", "1G\" ; x").is_err());
        assert!(validate_directive("memory limit", "1G").is_err());
        assert!(validate_directive("memory_limit = 1\nx", "1").is_err());
        assert!(validate_directive("memory_limit", "  ").is_err());
        assert!(validate_directive("extension_dir", "C:/elsewhere").is_err());
        assert!(validate_directive("opcache.enable", "0").is_ok());
    }

    #[test]
    fn managed_block_follows_the_stock_ini() {
        let php = PhpRuntime {
            id: "php-8.4".into(),
            version: "8.4.28".into(),
            thread_safe: false,
            dir: PathBuf::from(r"D:\OmniPHP\bin\php\php-8.4"),
            extensions: vec![],
        };
        let site = Site {
            name: "shop".into(),
            dir: PathBuf::from(r"D:\OmniPHP\www\shop"),
            hostname: "shop.test".into(),
            aliases: vec![],
            document_root: PathBuf::from(r"D:\OmniPHP\www\shop\public"),
            php: Some(php.id.clone()),
            extensions: vec!["curl".into(), "opcache".into(), "xdebug".into()],
            ini: [
                ("memory_limit".to_string(), "1G".to_string()),
                ("date.timezone".to_string(), "America/Bogota".to_string()),
                ("error_reporting".to_string(), "E_ALL & ~E_DEPRECATED".to_string()),
            ]
            .into(),
        };
        let paths = Paths::new(r"D:\OmniPHP");
        let base = "[PHP]\nmemory_limit = 128M\n";
        assert!(!render_site_ini(base, &site, &php, None, &paths).contains("sendmail_from"));

        let mail = MailCatcher {
            exe: PathBuf::from(r"D:\OmniPHP\bin\mailpit\1.22.3\mailpit.exe"),
            smtp_port: 1025,
        };
        let ini = render_site_ini(base, &site, &php, Some(&mail), &paths);
        assert!(ini.contains("\nSMTP = 127.0.0.1\nsmtp_port = 1025\n"));
        assert!(ini.contains("sendmail_from = \"omniphp@localhost\""));

        let stock = ini.find("memory_limit = 128M").unwrap();
        let managed = ini.find("memory_limit = 512M").unwrap();
        let chosen = ini.find("memory_limit = 1G").unwrap();
        assert!(stock < managed, "managed values must override the stock ini");
        assert!(managed < chosen, "the site's own values must come last to win");
        assert!(ini.contains("\ndate.timezone = \"America/Bogota\"\n"));
        assert!(ini.contains("\nerror_reporting = E_ALL & ~E_DEPRECATED\n"));
        assert!(ini.contains("extension_dir = \"D:/OmniPHP/bin/php/php-8.4/ext\""));
        assert!(ini.contains("\nextension=curl\n"));
        assert!(ini.contains("\nzend_extension=opcache\n"));
        assert!(ini.contains("\nzend_extension=xdebug\n"));
        assert!(ini.contains("error_log = \"D:/OmniPHP/logs/php-shop.log\""));
    }
}
