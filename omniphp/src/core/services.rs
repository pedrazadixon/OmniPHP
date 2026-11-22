//! Service supervision: Apache, MySQL and Mailpit as child processes.
//!
//! Each service starts from configuration generated under `etc\`, is checked
//! to be really accepting connections before it counts as running, and is
//! stopped the way that service wants to be stopped.

use std::fs::File;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use super::config::{ApacheConfig, MailpitConfig, MysqlConfig, RedisConfig};
use super::paths::slash;
use super::mysql;
use super::paths::Paths;
use super::proc::{kill_with_app, quiet_command, signal_event};
use super::runtimes::{ApacheRuntime, ToolRuntime};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ServiceId {
    Apache,
    Mysql,
    Mailpit,
    Redis,
}

impl ServiceId {
    pub const ALL: [ServiceId; 4] =
        [ServiceId::Apache, ServiceId::Mysql, ServiceId::Mailpit, ServiceId::Redis];

    pub fn name(self) -> &'static str {
        match self {
            ServiceId::Apache => "Apache",
            ServiceId::Mysql => "MySQL",
            ServiceId::Mailpit => "Mailpit",
            ServiceId::Redis => "Redis",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceStatus {
    Stopped,
    Running { pid: u32 },
}

/// A supervised child process.
#[derive(Default)]
struct Process {
    child: Option<Child>,
}

impl Process {
    fn status(&mut self) -> ServiceStatus {
        match &mut self.child {
            Some(child) => match child.try_wait() {
                Ok(None) => ServiceStatus::Running { pid: child.id() },
                // Exited (or unknowable): forget it.
                _ => {
                    self.child = None;
                    ServiceStatus::Stopped
                }
            },
            None => ServiceStatus::Stopped,
        }
    }

    fn is_running(&mut self) -> bool {
        self.status() != ServiceStatus::Stopped
    }

    /// Spawns `command` with its output captured in `startup_log`, tied to the
    /// app's lifetime.
    fn spawn(&mut self, mut command: Command, startup_log: &Path) -> Result<()> {
        let log = File::create(startup_log)
            .with_context(|| format!("cannot write {}", startup_log.display()))?;
        let program = command.get_program().to_string_lossy().into_owned();
        let child = command
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()
            .with_context(|| format!("cannot start {program}"))?;
        kill_with_app(&child);
        self.child = Some(child);
        Ok(())
    }

    /// Waits until the process accepts connections on `address`. Fails if it
    /// exits first (quoting `startup_log`) or stays silent past `timeout`.
    fn wait_until_listening(
        &mut self,
        name: &str,
        address: SocketAddr,
        timeout: Duration,
        startup_log: &Path,
    ) -> Result<()> {
        let probe = connectable(address);
        let deadline = Instant::now() + timeout;
        loop {
            if !self.is_running() {
                let detail = std::fs::read_to_string(startup_log).unwrap_or_default();
                bail!("{name} stopped right after starting.\n{}", last_lines(&detail, 8));
            }
            if TcpStream::connect_timeout(&probe, Duration::from_millis(200)).is_ok() {
                return Ok(());
            }
            if Instant::now() > deadline {
                bail!("{name} started but is not answering on port {}.", address.port());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Signals the named shutdown event (when the service has one), waits up
    /// to `grace` for a clean exit, then ends the whole process tree.
    fn stop(&mut self, shutdown_event: Option<&dyn Fn(u32) -> String>, grace: Duration) -> Result<()> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        let exited = |child: &mut Child| child.try_wait().ok().flatten().is_some();
        if exited(&mut child) {
            return Ok(());
        }
        if let Some(event) = shutdown_event
            && signal_event(&event(child.id()))
        {
            let deadline = Instant::now() + grace;
            while Instant::now() < deadline {
                if exited(&mut child) {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        // /T takes child processes (Apache's worker, php-cgi) along.
        let _ = quiet_command("taskkill")
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .output();
        if !exited(&mut child) {
            child.kill().context("cannot stop the process")?;
        }
        let _ = child.wait();
        Ok(())
    }
}

fn socket(listen: &str, port: u16) -> Result<SocketAddr> {
    format!("{listen}:{port}")
        .parse()
        .with_context(|| format!("invalid listen address {listen}:{port}"))
}

/// Fails with a readable message when something else already holds `port`.
fn require_free_port(name: &str, listen: &str, port: u16) -> Result<()> {
    if TcpListener::bind(socket(listen, port)?).is_err() {
        bail!(
            "Port {port} is already in use, so {name} cannot start. Stop the other program \
             using it (for example Laragon) or change the port in Settings."
        );
    }
    Ok(())
}

/// `0.0.0.0` is not a destination; probe loopback instead.
fn connectable(address: SocketAddr) -> SocketAddr {
    if address.ip().is_unspecified() {
        SocketAddr::from(([127, 0, 0, 1], address.port()))
    } else {
        address
    }
}

fn last_lines(text: &str, count: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|line| !line.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(count)..].join("\n")
}

// ---- Apache ----------------------------------------------------------------

#[derive(Default)]
pub struct ApacheService {
    process: Process,
}

impl ApacheService {
    pub fn status(&mut self) -> ServiceStatus {
        self.process.status()
    }

    /// Starts Apache with the generated configuration.
    pub fn start(
        &mut self,
        apache: &ApacheRuntime,
        config: &ApacheConfig,
        https_port: Option<u16>,
        paths: &Paths,
    ) -> Result<()> {
        if self.process.is_running() {
            return Ok(());
        }
        for port in std::iter::once(config.http_port).chain(https_port) {
            require_free_port("Apache", &config.listen, port)?;
        }
        // Errors raised before Apache opens its own error log land here.
        let startup_log = paths.logs().join("apache-startup.log");
        let mut command = quiet_command(apache.httpd());
        command.arg("-f").arg(paths.apache_conf());
        self.process.spawn(command, &startup_log)?;
        self.process.wait_until_listening(
            "Apache",
            socket(&config.listen, config.http_port)?,
            Duration::from_secs(10),
            &startup_log,
        )
    }

    /// A clean exit takes about 4 s idle and up to 13 s once mod_fcgid has
    /// php-cgi processes to wind down, so Apache gets a short grace period
    /// before its process tree is ended. It keeps no state that could be lost.
    pub fn stop(&mut self) -> Result<()> {
        // The same event `httpd -k stop` would signal.
        self.process.stop(Some(&|pid| format!("ap{pid}_shutdown")), Duration::from_secs(5))
    }

    /// Graceful restart: Apache re-reads its configuration and replaces its
    /// worker (and with it every php-cgi process) without dropping the port.
    /// Returns `false` when Apache is not running.
    pub fn reload(&mut self) -> Result<bool> {
        let ServiceStatus::Running { pid } = self.status() else {
            return Ok(false);
        };
        if !signal_event(&format!("ap{pid}_restart")) {
            bail!("Apache did not accept the reload signal.");
        }
        Ok(true)
    }
}

// ---- MySQL -----------------------------------------------------------------

#[derive(Default)]
pub struct MysqlService {
    process: Process,
}

impl MysqlService {
    pub fn status(&mut self) -> ServiceStatus {
        self.process.status()
    }

    /// Starts MySQL, creating its data directory on first use (root has no
    /// password and only this PC can connect).
    pub fn start(&mut self, runtime: &ToolRuntime, config: &MysqlConfig, paths: &Paths) -> Result<()> {
        if self.process.is_running() {
            return Ok(());
        }
        require_free_port("MySQL", mysql::LISTEN, config.port)?;
        let my_ini = mysql::write_config(runtime, config, paths)?;
        mysql::initialize_if_needed(runtime, &my_ini, paths)?;

        let startup_log = paths.logs().join("mysql-startup.log");
        let mut command = quiet_command(&runtime.exe);
        command.arg(format!("--defaults-file={}", my_ini.display()));
        self.process.spawn(command, &startup_log)?;
        // Crash recovery after an unclean stop can take a while.
        let result = self.process.wait_until_listening(
            "MySQL",
            socket(mysql::LISTEN, config.port)?,
            Duration::from_secs(60),
            &startup_log,
        );
        result.with_context(|| format!("See {} for details", mysql::error_log(paths).display()))
    }

    /// MySQL flushes its data on a clean shutdown, so it gets a generous
    /// grace period before being forced.
    pub fn stop(&mut self) -> Result<()> {
        // mysqld listens for this event on Windows (checked against 8.4);
        // unlike `mysqladmin shutdown` it needs no password.
        self.process.stop(Some(&|pid| format!("mysqld{pid}_shutdown")), Duration::from_secs(30))
    }
}

// ---- Mailpit ---------------------------------------------------------------

/// Mailpit binds loopback only: it would otherwise accept mail from the network.
pub const MAILPIT_LISTEN: &str = "127.0.0.1";

#[derive(Default)]
pub struct MailpitService {
    process: Process,
}

impl MailpitService {
    pub fn status(&mut self) -> ServiceStatus {
        self.process.status()
    }

    pub fn start(&mut self, runtime: &ToolRuntime, config: &MailpitConfig, paths: &Paths) -> Result<()> {
        if self.process.is_running() {
            return Ok(());
        }
        require_free_port("Mailpit", MAILPIT_LISTEN, config.smtp_port)?;
        require_free_port("Mailpit", MAILPIT_LISTEN, config.ui_port)?;
        let data = paths.data().join("mailpit");
        std::fs::create_dir_all(&data).with_context(|| format!("cannot create {}", data.display()))?;

        let startup_log = paths.logs().join("mailpit-startup.log");
        let mut command = quiet_command(&runtime.exe);
        command
            .arg("--smtp")
            .arg(format!("{MAILPIT_LISTEN}:{}", config.smtp_port))
            .arg("--listen")
            .arg(format!("{MAILPIT_LISTEN}:{}", config.ui_port))
            .arg("--database")
            .arg(data.join("mailpit.db"))
            .arg("--log-file")
            .arg(paths.logs().join("mailpit.log"));
        self.process.spawn(command, &startup_log)?;
        self.process.wait_until_listening(
            "Mailpit",
            socket(MAILPIT_LISTEN, config.ui_port)?,
            Duration::from_secs(10),
            &startup_log,
        )
    }

    pub fn stop(&mut self) -> Result<()> {
        self.process.stop(None, Duration::ZERO)
    }
}

// ---- Redis -----------------------------------------------------------------

/// Redis has no password here, so it only listens on this PC.
pub const REDIS_LISTEN: &str = "127.0.0.1";

#[derive(Default)]
pub struct RedisService {
    process: Process,
    port: u16,
    cli: Option<std::path::PathBuf>,
}

impl RedisService {
    pub fn status(&mut self) -> ServiceStatus {
        self.process.status()
    }

    pub fn start(&mut self, runtime: &ToolRuntime, config: &RedisConfig, paths: &Paths) -> Result<()> {
        if self.process.is_running() {
            return Ok(());
        }
        require_free_port("Redis", REDIS_LISTEN, config.port)?;
        let data = paths.data().join("redis");
        let etc = paths.etc().join("redis");
        for dir in [&data, &etc] {
            std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        }
        let conf = etc.join("redis.conf");
        std::fs::write(
            &conf,
            format!(
                "# Generated by OmniPHP. Do not edit: this file is overwritten.\n\
                 bind {REDIS_LISTEN}\nport {}\ndir \"{}\"\nlogfile \"{}\"\n",
                config.port,
                slash(&data),
                slash(&paths.logs().join("redis.log")),
            ),
        )?;

        let startup_log = paths.logs().join("redis-startup.log");
        let mut command = quiet_command(&runtime.exe);
        command.arg(&conf);
        self.process.spawn(command, &startup_log)?;
        self.port = config.port;
        self.cli = Some(runtime.dir.join("redis-cli.exe"));
        self.process.wait_until_listening(
            "Redis",
            socket(REDIS_LISTEN, config.port)?,
            Duration::from_secs(10),
            &startup_log,
        )
    }

    /// Asks Redis to save and exit; ends it if that takes too long.
    pub fn stop(&mut self) -> Result<()> {
        if self.process.is_running()
            && let Some(cli) = &self.cli
        {
            let _ = quiet_command(cli)
                .args(["-h", REDIS_LISTEN, "-p", &self.port.to_string(), "shutdown"])
                .output();
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.process.is_running() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        self.process.stop(None, Duration::ZERO)
    }
}