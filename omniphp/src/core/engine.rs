//! Ties the core together: loads settings, discovers runtimes and sites,
//! writes configuration and drives the services. The UI and the command line
//! both go through this type and render from its [`Snapshot`].

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use super::apache::{self, DEFAULT_SITE};
use super::config::{self, Config, SiteSettings, SitesFile, ThemeChoice};
use super::hosts;
use super::mysql;
use super::paths::Paths;
use super::php::{self, MailCatcher};
use super::quickapp::{self, Database, InstallJob, Template};
use super::runtimes::{self, ApacheRuntime, PhpRuntime, ToolRuntime};
use super::services::{
    ApacheService, MailpitService, MysqlService, RedisService, ServiceId, ServiceStatus,
};
use super::share::{self, Share};
use super::sites::{self, Site};
use super::ssl::{self, Authority};
use super::startup;
use super::system::{self, SystemChanges};
use super::tools;

/// What the new-site wizard asks for.
#[derive(Debug, Clone)]
pub struct NewSiteRequest {
    pub template: Template,
    pub name: String,
    /// PHP runtime id; the default PHP when unset.
    pub php: Option<String>,
    pub create_database: bool,
}

/// The configured version if it is installed, otherwise the newest.
fn pick<'a>(runtimes: &'a [ToolRuntime], wanted: Option<&String>) -> Option<&'a ToolRuntime> {
    wanted
        .and_then(|id| runtimes.iter().find(|runtime| &runtime.id == id))
        .or_else(|| runtimes.first())
}

/// Rejects settings that could not work, with a message for the Settings page.
pub fn validate_config(config: &Config) -> Result<()> {
    let ports = [
        ("Apache HTTP", config.apache.http_port),
        ("Apache HTTPS", config.apache.https_port),
        ("MySQL", config.mysql.port),
        ("Mailpit SMTP", config.mailpit.smtp_port),
        ("Mailpit inbox", config.mailpit.ui_port),
        ("Redis", config.redis.port),
    ];
    for (ix, (name, port)) in ports.iter().enumerate() {
        if *port == 0 {
            bail!("{name} needs a port between 1 and 65535.");
        }
        if let Some((other, _)) = ports[..ix].iter().find(|(_, taken)| taken == port) {
            bail!("{other} and {name} cannot both use port {port}.");
        }
    }
    if config.apache.listen.parse::<std::net::IpAddr>().is_err() {
        bail!("\"{}\" is not an IP address Apache can listen on.", config.apache.listen);
    }
    let pattern = &config.sites.hostname_pattern;
    if !pattern.contains("{name}") || !hosts::is_valid_hostname(&pattern.replace("{name}", "x")) {
        bail!("The address pattern must contain {{name}} and form a valid address, like {{name}}.test.");
    }
    Ok(())
}

pub struct Engine {
    paths: Paths,
    config: Config,
    site_settings: SitesFile,
    phps: Vec<PhpRuntime>,
    apaches: Vec<ApacheRuntime>,
    sites: Vec<Site>,
    mysqls: Vec<ToolRuntime>,
    mailpits: Vec<ToolRuntime>,
    redises: Vec<ToolRuntime>,
    apache: ApacheService,
    mysql: MysqlService,
    mailpit: MailpitService,
    redis: RedisService,
    /// Open Cloudflare tunnels, by site name.
    shares: HashMap<String, Share>,
    /// The local certificate authority, when HTTPS is enabled.
    authority: Option<Authority>,
}

/// A copy of everything the UI shows. Cheap to clone and free of handles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub root: PathBuf,
    pub www: PathBuf,
    pub phps: Vec<PhpRuntime>,
    pub default_php: Option<String>,
    pub sites: Vec<Site>,
    /// One entry per [`ServiceId`], in display order.
    pub services: Vec<ServiceInfo>,
    /// The saved settings, for the Settings page.
    pub config: Config,
    pub run_at_startup: bool,
    /// Public addresses of the sites being shared, by site name.
    pub shares: BTreeMap<String, String>,
    /// Whether cloudflared is installed.
    pub can_share: bool,
    pub mysql_installed: bool,
    pub http_port: u16,
    /// `Some` when HTTPS is enabled and this Apache build supports it.
    pub https_port: Option<u16>,
    pub secure_sites: HashSet<String>,
    /// Whether Windows trusts the local certificate authority. `true` when
    /// HTTPS is off, since there is then nothing to trust.
    pub ca_trusted: bool,
    /// Site addresses Windows cannot resolve yet (missing from the hosts file).
    pub unregistered_hosts: Vec<String>,
}

/// What the Services page shows for one service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceInfo {
    pub id: ServiceId,
    /// Version and a short note, or how to install it when missing.
    pub detail: String,
    pub ports: String,
    pub status: ServiceStatus,
    pub installed: bool,
    /// Whether "Start all" and autostart include it.
    pub enabled: bool,
}

impl Snapshot {
    pub fn is_running(&self, id: ServiceId) -> bool {
        self.services
            .iter()
            .any(|service| service.id == id && service.status != ServiceStatus::Stopped)
    }

    pub fn running_count(&self) -> usize {
        self.services.iter().filter(|service| service.status != ServiceStatus::Stopped).count()
    }

    /// `https://shop.test`, or `http://shop.test:8080` and the like when the
    /// site has no certificate or the ports are not the standard ones.
    pub fn site_url(&self, site: &Site) -> String {
        match self.https_port {
            Some(443) if self.secure_sites.contains(&site.name) => format!("https://{}", site.hostname),
            Some(port) if self.secure_sites.contains(&site.name) => {
                format!("https://{}:{port}", site.hostname)
            }
            _ if self.http_port == 80 => format!("http://{}", site.hostname),
            _ => format!("http://{}:{}", site.hostname, self.http_port),
        }
    }

    /// Whether Windows itself still needs a change that requires administrator
    /// permission (trusting the authority or registering site addresses).
    pub fn needs_system_setup(&self) -> bool {
        !self.ca_trusted || !self.unregistered_hosts.is_empty()
    }
}

impl Engine {
    pub fn load(paths: Paths) -> Result<Self> {
        let config: Config = config::load(&paths.config_file())?;
        let site_settings: SitesFile = config::load(&paths.sites_file())?;
        let mut engine = Self {
            phps: runtimes::scan_php(&paths.bin().join("php")),
            apaches: runtimes::scan_apache(&paths.bin().join("apache")),
            sites: Vec::new(),
            mysqls: runtimes::scan_tool(&paths.bin().join("mysql"), r"bin\mysqld.exe"),
            mailpits: runtimes::scan_tool(&paths.bin().join("mailpit"), "mailpit.exe"),
            redises: runtimes::scan_tool(&paths.bin().join("redis"), "redis-server.exe"),
            apache: ApacheService::default(),
            mysql: MysqlService::default(),
            mailpit: MailpitService::default(),
            redis: RedisService::default(),
            shares: HashMap::new(),
            authority: None,
            paths,
            config,
            site_settings,
        };
        engine.rescan_sites();
        // Best effort here; `apply` reports the error if this cannot work.
        if engine.https_port().is_some() {
            engine.authority = Authority::load_or_create(&engine.paths.ssl_dir()).ok();
        }
        Ok(engine)
    }

    pub fn www(&self) -> PathBuf {
        self.config.sites.www_dir.clone().unwrap_or_else(|| self.paths.www())
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.paths.logs()
    }

    pub fn autostart(&self) -> bool {
        self.config.general.autostart_services
    }

    pub fn start_minimized(&self) -> bool {
        self.config.general.start_minimized
    }

    pub fn theme(&self) -> ThemeChoice {
        self.config.general.theme
    }

    fn mysql_runtime(&self) -> Result<&ToolRuntime> {
        pick(&self.mysqls, self.config.mysql.version.as_ref())
            .with_context(|| format!("no MySQL found in {}", self.paths.bin().join("mysql").display()))
    }

    fn mailpit_runtime(&self) -> Result<&ToolRuntime> {
        pick(&self.mailpits, self.config.mailpit.version.as_ref())
            .with_context(|| format!("no Mailpit found in {}", self.paths.bin().join("mailpit").display()))
    }

    fn redis_runtime(&self) -> Result<&ToolRuntime> {
        pick(&self.redises, self.config.redis.version.as_ref())
            .with_context(|| format!("no Redis found in {}", self.paths.bin().join("redis").display()))
    }

    /// PHP sends mail to Mailpit whenever Mailpit is installed and enabled.
    fn mail_catcher(&self) -> Option<MailCatcher> {
        let runtime = self.mailpit_runtime().ok()?;
        self.config.mailpit.enabled.then(|| MailCatcher {
            exe: runtime.exe.clone(),
            smtp_port: self.config.mailpit.smtp_port,
        })
    }

    /// Re-reads the sites folder. Returns `true` when the site list changed.
    pub fn rescan_sites(&mut self) -> bool {
        let folders = sites::scan_folders(&self.www());
        let sites = sites::resolve(&folders, &self.site_settings, &self.config, &self.phps);
        let changed = sites != self.sites;
        self.sites = sites;
        changed
    }

    fn apache_runtime(&self) -> Result<&ApacheRuntime> {
        let wanted = self.config.apache.version.as_ref();
        wanted
            .and_then(|id| self.apaches.iter().find(|apache| &apache.id == id))
            .or_else(|| self.apaches.first())
            .with_context(|| format!("no Apache found in {}", self.paths.bin().join("apache").display()))
    }

    fn https_port(&self) -> Option<u16> {
        let apache = self.apache_runtime().ok()?;
        (self.config.apache.ssl && apache::ssl_available(apache)).then_some(self.config.apache.https_port)
    }

    /// The fallback vhost: `http://localhost/` lists the sites folder.
    fn default_site(&self) -> Site {
        let php = sites::default_php(&self.config, &self.phps);
        Site {
            name: DEFAULT_SITE.into(),
            dir: self.www(),
            hostname: "localhost".into(),
            aliases: Vec::new(),
            document_root: self.www(),
            php: php.map(|php| php.id.clone()),
            extensions: php.map(sites::default_extensions).unwrap_or_default(),
            ini: Default::default(),
        }
    }

    /// Writes all configuration for the current sites (issuing any missing
    /// certificates) and checks that Apache accepts it.
    pub fn apply(&mut self) -> Result<()> {
        let apache = self.apache_runtime()?.clone();
        let https_port = self.https_port();
        let default_site = self.default_site();

        let mut certificates = HashMap::new();
        if https_port.is_some() {
            let authority = Authority::load_or_create(&self.paths.ssl_dir())?;
            let dir = self.paths.ssl_dir().join("sites");
            for site in std::iter::once(&default_site).chain(&self.sites) {
                if let Some(certificate) = authority.ensure_site(&site.hostname, &site.aliases, &dir)? {
                    certificates.insert(site.name.clone(), certificate);
                }
            }
            self.authority = Some(authority);
        }

        apache::write_all(
            &apache,
            &self.config.apache,
            https_port,
            &certificates,
            self.mail_catcher().as_ref(),
            &default_site,
            &self.sites,
            &self.phps,
            &self.paths,
        )?;
        apache::validate(&apache, &self.paths)
    }

    // ---- services ----------------------------------------------------------

    pub fn start(&mut self, id: ServiceId) -> Result<()> {
        match id {
            ServiceId::Apache => {
                self.apply()?;
                let apache = self.apache_runtime()?.clone();
                let https_port = self.https_port();
                self.apache.start(&apache, &self.config.apache, https_port, &self.paths)
            }
            ServiceId::Mysql => {
                let runtime = self.mysql_runtime()?.clone();
                self.mysql.start(&runtime, &self.config.mysql, &self.paths)
            }
            ServiceId::Mailpit => {
                let runtime = self.mailpit_runtime()?.clone();
                self.mailpit.start(&runtime, &self.config.mailpit, &self.paths)
            }
            ServiceId::Redis => {
                let runtime = self.redis_runtime()?.clone();
                self.redis.start(&runtime, &self.config.redis, &self.paths)
            }
        }
    }

    pub fn stop(&mut self, id: ServiceId) -> Result<()> {
        match id {
            // Open tunnels are left alone: they reconnect to Apache by
            // themselves, so a shared link survives a restart. Closing them
            // here would kill the link for good (Cloudflare error 1033).
            ServiceId::Apache => self.apache.stop(),
            ServiceId::Mysql => self.mysql.stop(),
            ServiceId::Mailpit => self.mailpit.stop(),
            ServiceId::Redis => self.redis.stop(),
        }
    }

    fn status(&mut self, id: ServiceId) -> ServiceStatus {
        match id {
            ServiceId::Apache => self.apache.status(),
            ServiceId::Mysql => self.mysql.status(),
            ServiceId::Mailpit => self.mailpit.status(),
            ServiceId::Redis => self.redis.status(),
        }
    }

    fn is_installed(&self, id: ServiceId) -> bool {
        match id {
            ServiceId::Apache => self.apache_runtime().is_ok(),
            ServiceId::Mysql => self.mysql_runtime().is_ok(),
            ServiceId::Mailpit => self.mailpit_runtime().is_ok(),
            ServiceId::Redis => self.redis_runtime().is_ok(),
        }
    }

    fn is_enabled(&self, id: ServiceId) -> bool {
        match id {
            ServiceId::Apache => self.config.apache.enabled,
            ServiceId::Mysql => self.config.mysql.enabled,
            ServiceId::Mailpit => self.config.mailpit.enabled,
            ServiceId::Redis => self.config.redis.enabled,
        }
    }

    /// The switch on the Services page: starts or stops the service and
    /// remembers the choice for "Start all" and the next launch.
    pub fn set_service(&mut self, id: ServiceId, on: bool) -> Result<()> {
        if self.is_enabled(id) != on {
            match id {
                ServiceId::Apache => self.config.apache.enabled = on,
                ServiceId::Mysql => self.config.mysql.enabled = on,
                ServiceId::Mailpit => self.config.mailpit.enabled = on,
                ServiceId::Redis => self.config.redis.enabled = on,
            }
            config::save(&self.paths.config_file(), &self.config)?;
        }
        let result = if on { self.start(id) } else { self.stop(id) };
        // Whether PHP mails Mailpit depends on Mailpit being enabled.
        if id == ServiceId::Mailpit && self.apache.status() != ServiceStatus::Stopped {
            self.reload_apache()?;
        }
        result
    }

    /// Starts every enabled, installed service. All are attempted; the errors
    /// of those that failed are reported together.
    pub fn start_all(&mut self) -> Result<()> {
        let mut errors = Vec::new();
        for id in ServiceId::ALL {
            if self.is_enabled(id) && self.is_installed(id)
                && let Err(error) = self.start(id)
            {
                errors.push(format!("{error:#}"));
            }
        }
        if !errors.is_empty() {
            bail!("{}", errors.join("\n"));
        }
        Ok(())
    }

    pub fn stop_all(&mut self) -> Result<()> {
        let mut errors = Vec::new();
        for id in ServiceId::ALL {
            if let Err(error) = self.stop(id) {
                errors.push(format!("{}: {error:#}", id.name()));
            }
        }
        if !errors.is_empty() {
            bail!("{}", errors.join("\n"));
        }
        Ok(())
    }

    fn service_info(&mut self, id: ServiceId) -> ServiceInfo {
        let (version, note, ports, folder) = match id {
            ServiceId::Apache => (
                self.apache_runtime().ok().map(|runtime| runtime.version.clone()),
                "mod_fcgid",
                match self.https_port() {
                    Some(https) => format!("{} / {https}", self.config.apache.http_port),
                    None => self.config.apache.http_port.to_string(),
                },
                "apache",
            ),
            ServiceId::Mysql => (
                self.mysql_runtime().ok().map(|runtime| runtime.version.clone()),
                "user root, no password",
                self.config.mysql.port.to_string(),
                "mysql",
            ),
            ServiceId::Mailpit => (
                self.mailpit_runtime().ok().map(|runtime| runtime.version.clone()),
                "catches mail sent by PHP",
                format!("{} / {}", self.config.mailpit.smtp_port, self.config.mailpit.ui_port),
                "mailpit",
            ),
            ServiceId::Redis => (
                self.redis_runtime().ok().map(|runtime| runtime.version.clone()),
                "cache and queues",
                self.config.redis.port.to_string(),
                "redis",
            ),
        };
        ServiceInfo {
            id,
            installed: version.is_some(),
            detail: match version {
                Some(version) => format!("{version} · {note}"),
                None => format!("Not installed: unzip it into bin\\{folder}"),
            },
            ports,
            status: self.status(id),
            enabled: self.is_enabled(id),
        }
    }

    // ---- settings ----------------------------------------------------------

    /// Changes the saved settings and restarts whatever they affect.
    pub fn update_config(&mut self, change: impl FnOnce(&mut Config)) -> Result<()> {
        let old = self.config.clone();
        let mut new = old.clone();
        change(&mut new);
        validate_config(&new)?;
        if new == old {
            return Ok(());
        }
        config::save(&self.paths.config_file(), &new)?;
        self.config = new.clone();
        if new.sites != old.sites {
            self.rescan_sites();
        }

        let restart = |engine: &mut Self, id: ServiceId| -> Result<()> {
            if engine.status(id) != ServiceStatus::Stopped {
                engine.stop(id)?;
                engine.start(id)?;
            }
            Ok(())
        };
        if new.mysql != old.mysql {
            restart(self, ServiceId::Mysql)?;
        }
        if new.mailpit != old.mailpit {
            restart(self, ServiceId::Mailpit)?;
        }
        if new.redis != old.redis {
            restart(self, ServiceId::Redis)?;
        }
        if new.apache.http_port != old.apache.http_port {
            // Tunnels point at the old port; they cannot follow it.
            self.stop_sharing();
        }
        if new.apache != old.apache {
            // Ports and addresses only change with a full restart.
            restart(self, ServiceId::Apache)?;
        } else if (new.sites != old.sites || new.mailpit != old.mailpit)
            && self.apache.status() != ServiceStatus::Stopped
        {
            self.reload_apache()?;
        }
        Ok(())
    }

    pub fn set_run_at_startup(&mut self, enabled: bool) -> Result<()> {
        startup::set_enabled(enabled)
    }

    /// Regenerates configuration and, if Apache is running, makes it pick the
    /// changes up. A configuration Apache rejects is never loaded.
    pub fn reload_apache(&mut self) -> Result<()> {
        self.apply()?;
        self.apache.reload()?;
        Ok(())
    }

    /// Picks up added or removed site folders. Returns `true` if anything changed.
    pub fn sync_sites(&mut self) -> Result<bool> {
        if !self.rescan_sites() {
            return Ok(false);
        }
        if self.apache.status() != ServiceStatus::Stopped {
            self.reload_apache()?;
        }
        Ok(true)
    }

    /// Makes `id` the PHP used by every site that has not picked its own.
    pub fn set_default_php(&mut self, id: &str) -> Result<()> {
        if !self.phps.iter().any(|php| php.id == id) {
            bail!("PHP runtime \"{id}\" is not installed");
        }
        self.config.sites.default_php = Some(id.to_string());
        config::save(&self.paths.config_file(), &self.config)?;
        self.rescan_sites();
        if self.apache.status() != ServiceStatus::Stopped {
            self.reload_apache()?;
        }
        Ok(())
    }

    // ---- per-site settings -------------------------------------------------

    /// The overrides saved for a site (not the resolved values).
    pub fn site_settings(&self, name: &str) -> SiteSettings {
        self.site_settings.sites.get(name).cloned().unwrap_or_default()
    }

    /// Changes a site's saved settings, then applies them. If the result is
    /// not usable, the change is undone and the reason returned.
    pub fn update_site(&mut self, name: &str, change: impl FnOnce(&mut SiteSettings)) -> Result<()> {
        let before = self.site_settings.clone();
        let mut settings = self.site_settings(name);
        change(&mut settings);
        if settings == SiteSettings::default() {
            self.site_settings.sites.remove(name);
        } else {
            self.site_settings.sites.insert(name.to_string(), settings);
        }
        self.rescan_sites();

        if let Err(error) = self.validate_site(name) {
            self.site_settings = before;
            self.rescan_sites();
            return Err(error);
        }
        config::save(&self.paths.sites_file(), &self.site_settings)?;
        if self.apache.status() != ServiceStatus::Stopped {
            self.reload_apache()?;
        }
        Ok(())
    }

    /// Turns one PHP extension on or off for a site.
    pub fn set_site_extension(&mut self, name: &str, extension: &str, enabled: bool) -> Result<()> {
        let mut extensions = self.site(name)?.extensions.clone();
        extensions.retain(|ext| ext != extension);
        if enabled {
            extensions.push(extension.to_string());
        }
        self.update_site(name, |settings| settings.extensions = Some(extensions))
    }

    // ---- tools --------------------------------------------------------------

    pub fn paths(&self) -> Paths {
        self.paths.clone()
    }

    /// Picks up PHP versions added to `bin\php` since the app started.
    pub fn rescan_runtimes(&mut self) {
        self.phps = runtimes::scan_php(&self.paths.bin().join("php"));
        self.rescan_sites();
    }

    /// Opens a command prompt in a site's folder (or the sites folder) with
    /// that site's PHP, Composer and the MySQL client on its PATH.
    pub fn open_terminal(&mut self, site: Option<&str>) -> Result<()> {
        let (dir, title, php_id, ini_name) = match site {
            Some(name) => {
                let site = self.site(name)?.clone();
                (site.dir.clone(), format!("OmniPHP - {name}"), site.php.clone(), name.to_string())
            }
            None => {
                let php = sites::default_php(&self.config, &self.phps).map(|php| php.id.clone());
                (self.www(), "OmniPHP".to_string(), php, apache::DEFAULT_SITE.to_string())
            }
        };
        let php = php_id.and_then(|id| self.phps.iter().find(|php| php.id == id));
        let bin = self.paths.bin();
        let mut path_dirs: Vec<PathBuf> = php.iter().map(|php| php.dir.clone()).collect();
        path_dirs.push(bin.join("composer"));
        path_dirs.extend(self.mysql_runtime().ok().map(|runtime| runtime.dir.join("bin")));
        path_dirs.extend(self.redis_runtime().ok().map(|runtime| runtime.dir.clone()));
        path_dirs.push(bin.join("git").join("cmd"));

        // Same php.ini as the site (or `localhost`) uses in the browser. It is
        // written afresh each time: one left from before the install folder
        // was moved or renamed still points at the old PHP folders.
        let phprc = match php {
            Some(php) => {
                let site = match site {
                    Some(name) => self.site(name)?.clone(),
                    None => self.default_site(),
                };
                php::write_site_ini(&site, php, self.mail_catcher().as_ref(), &self.paths)?;
                Some(self.paths.php_site_dir(&ini_name))
            }
            None => None,
        };
        tools::open_terminal(&dir, &title, &path_dirs, phprc.as_deref())
    }

    /// Opens the Windows hosts file in an editor running as administrator, to
    /// check what is registered or to try an address by hand.
    pub fn edit_hosts(&self) -> Result<()> {
        tools::edit_hosts_file(&self.paths)
    }

    /// Opens Adminer on the local MySQL in the browser, installing Adminer
    /// and starting MySQL first if needed.
    pub fn open_database(&mut self) -> Result<()> {
        if self.apache.status() == ServiceStatus::Stopped {
            bail!("Start Apache first: the database tool is a web page it serves.");
        }
        tools::ensure_adminer(&self.paths, self.config.mysql.port)?;
        self.start(ServiceId::Mysql)?;
        // Make sure the running Apache knows the /adminer address.
        self.reload_apache()?;
        let port = match self.config.apache.http_port {
            80 => String::new(),
            port => format!(":{port}"),
        };
        tools::open_url(&format!("http://localhost{port}/adminer/"))
    }

    // ---- sharing ------------------------------------------------------------

    /// Publishes a site at a temporary Cloudflare address.
    pub fn share_site(&mut self, name: &str) -> Result<()> {
        if self.shares.get_mut(name).is_some_and(|share| share.is_alive()) {
            return Ok(());
        }
        let site = self.site(name)?.clone();
        let cloudflared = share::find_cloudflared(&self.paths).with_context(|| {
            format!("cloudflared is missing: put cloudflared.exe in {}", self.paths.bin().join("cloudflared").display())
        })?;
        if self.apache.status() == ServiceStatus::Stopped {
            bail!("Start Apache before sharing a site.");
        }
        let share = share::start(&cloudflared, name, &site.hostname, self.config.apache.http_port, &self.paths)?;
        self.shares.insert(name.to_string(), share);
        Ok(())
    }

    pub fn unshare_site(&mut self, name: &str) -> Result<()> {
        if let Some(share) = self.shares.remove(name) {
            share.stop();
        }
        Ok(())
    }

    fn stop_sharing(&mut self) {
        for (_, share) in self.shares.drain() {
            share.stop();
        }
    }

    // ---- new sites -----------------------------------------------------------

    /// First step of creating a site: makes the folder and gathers what the
    /// installer needs. The installer itself ([`quickapp::install`]) then runs
    /// without the engine, and [`Self::finish_site`] completes the job.
    pub fn prepare_site(&mut self, request: &NewSiteRequest) -> Result<InstallJob> {
        let name = request.name.trim();
        quickapp::validate_name(name)?;
        let dir = self.www().join(name);
        if dir.exists() {
            bail!("A folder named \"{name}\" already exists in {}.", self.www().display());
        }
        if let Some(id) = &request.php
            && !self.phps.iter().any(|php| &php.id == id)
        {
            bail!("PHP runtime \"{id}\" is not installed.");
        }
        // A template that cannot run on current PHP gets the newest installed
        // version it supports, unless one was chosen explicitly.
        let php_choice = match (&request.php, request.template.max_php()) {
            (Some(id), _) => Some(id.clone()),
            (None, Some(max)) => Some(
                self.phps
                    .iter()
                    .find(|php| runtimes::compare_versions(&php.short_version(), max).is_le())
                    .with_context(|| format!("{} needs PHP {max} or older, and none is installed.", request.template.label()))?
                    .id
                    .clone(),
            ),
            (None, None) => None,
        };
        std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
        if let Some(id) = &php_choice {
            self.site_settings.sites.entry(name.to_string()).or_default().php = Some(id.clone());
            config::save(&self.paths.sites_file(), &self.site_settings)?;
        }
        self.rescan_sites();

        let site = self.site(name)?.clone();
        let php = site
            .php
            .as_ref()
            .and_then(|id| self.phps.iter().find(|php| &php.id == id))
            .context("No PHP is installed: unzip a PHP build into bin\\php.")?;
        php::write_site_ini(&site, php, self.mail_catcher().as_ref(), &self.paths)?;
        let database = (request.create_database && self.mysql_runtime().is_ok()).then(|| Database {
            name: quickapp::database_name(name),
            port: self.config.mysql.port,
        });
        let php_exe = php.dir.join("php.exe");
        Ok(InstallJob {
            template: request.template,
            name: name.to_string(),
            url: self.snapshot().site_url(&site),
            php_exe,
            php_ini: self.paths.php_site_dir(name).join("php.ini"),
            composer: self.paths.bin().join("composer").join("composer.phar"),
            tmp: self.paths.tmp(),
            log: self.paths.logs().join(format!("install-{name}.log")),
            database,
            dir,
        })
    }

    /// Last step of creating a site: creates its database and brings the new
    /// site online.
    pub fn finish_site(&mut self, job: &InstallJob) -> Result<()> {
        // The installer may have added a `public` folder; detect it now.
        self.rescan_sites();
        if let Some(database) = &job.database {
            self.start(ServiceId::Mysql)?;
            mysql::create_database(self.mysql_runtime()?, database.port, &database.name)?;
            quickapp::after_database(job)?;
        }
        if self.apache.status() != ServiceStatus::Stopped {
            self.reload_apache()?;
        }
        Ok(())
    }

    /// Cleans up after a failed install: an untouched (empty) folder is
    /// removed; anything the installer already wrote is left for inspection.
    pub fn discard_site(&mut self, job: &InstallJob) {
        let empty = std::fs::read_dir(&job.dir).is_ok_and(|mut entries| entries.next().is_none());
        if empty {
            let _ = std::fs::remove_dir(&job.dir);
            if self.site_settings.sites.remove(&job.name).is_some() {
                let _ = config::save(&self.paths.sites_file(), &self.site_settings);
            }
        }
        self.rescan_sites();
    }

    /// Every php.ini setting available to a site, with its current value.
    pub fn php_directives(&mut self, name: &str) -> Result<Vec<(String, String)>> {
        let site = self.site(name)?.clone();
        let php = site
            .php
            .as_ref()
            .and_then(|id| self.phps.iter().find(|php| &php.id == id))
            .context("No PHP is installed.")?;
        // Make sure the file PHP is asked about reflects the saved settings.
        php::write_site_ini(&site, php, self.mail_catcher().as_ref(), &self.paths)?;
        php::list_directives(php, &self.paths.php_site_dir(name).join("php.ini"))
    }

    fn site(&self, name: &str) -> Result<&Site> {
        self.sites
            .iter()
            .find(|site| site.name == name)
            .with_context(|| format!("site \"{name}\" no longer exists"))
    }

    fn validate_site(&self, name: &str) -> Result<()> {
        let site = self.site(name)?;
        if !hosts::is_valid_hostname(&site.hostname) {
            bail!("\"{}\" is not a valid address. Use letters, digits, dots and dashes.", site.hostname);
        }
        for alias in &site.aliases {
            if !hosts::is_valid_hostname(alias.strip_prefix("*.").unwrap_or(alias)) {
                bail!("\"{alias}\" is not a valid alias.");
            }
        }
        let taken = self.sites.iter().any(|other| {
            other.name != site.name
                && (other.hostname == site.hostname || other.aliases.contains(&site.hostname))
        });
        if taken {
            bail!("Another site already uses the address {}.", site.hostname);
        }
        for (directive, value) in &site.ini {
            php::validate_directive(directive, value)?;
        }
        if !site.document_root.is_dir() {
            bail!("The document root {} does not exist.", site.document_root.display());
        }
        if !site.document_root.starts_with(&site.dir) {
            bail!("The document root must be inside the site folder.");
        }
        Ok(())
    }

    // ---- Windows integration -----------------------------------------------

    /// Every address Windows should resolve to this PC.
    fn desired_hosts(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for site in &self.sites {
            for name in std::iter::once(&site.hostname).chain(&site.aliases) {
                // Wildcards cannot be expressed in a hosts file.
                if hosts::is_valid_hostname(name) && !names.contains(name) {
                    names.push(name.clone());
                }
            }
        }
        names
    }

    fn ca_trusted(&self) -> bool {
        match (&self.authority, self.https_port()) {
            (Some(authority), Some(_)) => system::ca_trusted(authority.thumbprint()),
            (None, Some(_)) => false,
            (_, None) => true,
        }
    }

    fn pending_system_changes(&self) -> SystemChanges {
        let desired = self.desired_hosts();
        SystemChanges {
            trust_ca: match &self.authority {
                Some(authority) if !self.ca_trusted() => Some(authority.thumbprint().to_string()),
                _ => None,
            },
            hosts: (system::registered_hosts(&self.paths) != desired).then_some(desired),
        }
    }

    /// Trusts the certificate authority and registers the site addresses, as
    /// needed. Shows one Windows administrator prompt; does nothing (and shows
    /// no prompt) when everything is already in place.
    pub fn setup_system(&mut self) -> Result<()> {
        if self.https_port().is_some() && self.authority.is_none() {
            self.authority = Some(Authority::load_or_create(&self.paths.ssl_dir())?);
        }
        system::run_elevated(&self.pending_system_changes(), &self.paths)
    }

    pub fn snapshot(&mut self) -> Snapshot {
        let registered = system::registered_hosts(&self.paths);
        let services = ServiceId::ALL.into_iter().map(|id| self.service_info(id)).collect();
        // Forget tunnels that have died on their own.
        self.shares.retain(|_, share| share.is_alive());
        let shares = self.shares.iter().map(|(site, share)| (site.clone(), share.url.clone())).collect();
        Snapshot {
            root: self.paths.root().to_path_buf(),
            www: self.www(),
            phps: self.phps.clone(),
            default_php: sites::default_php(&self.config, &self.phps).map(|php| php.id.clone()),
            sites: self.sites.clone(),
            services,
            config: self.config.clone(),
            run_at_startup: startup::is_enabled(),
            shares,
            can_share: share::find_cloudflared(&self.paths).is_some(),
            mysql_installed: self.mysql_runtime().is_ok(),
            http_port: self.config.apache.http_port,
            https_port: self.https_port(),
            // Known before Apache has ever been started: a site gets a
            // certificate exactly when the authority may sign its address.
            secure_sites: self
                .sites
                .iter()
                .filter(|site| ssl::covers(&site.hostname))
                .map(|site| site.name.clone())
                .collect(),
            ca_trusted: self.ca_trusted(),
            unregistered_hosts: self
                .desired_hosts()
                .into_iter()
                .filter(|name| !registered.contains(name))
                .collect(),
        }
    }

    /// Fails early with a clear message when the root cannot work at all.
    pub fn check_root(&self) -> Result<()> {
        // mod_fcgid splits its wrapper command on spaces, so php-cgi's path
        // cannot contain one.
        if self.paths.root().to_string_lossy().contains(' ') {
            bail!(
                "The OmniPHP folder ({}) has a space in its path, which PHP's FastCGI \
                 launcher cannot handle. Move it to a path without spaces.",
                self.paths.root().display()
            );
        }
        Ok(())
    }
}
