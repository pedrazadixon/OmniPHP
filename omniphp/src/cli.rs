//! Headless commands, for scripting and for checking the core without the UI.
//!
//! ```text
//! omniphp apply    write all configuration and validate it
//! omniphp serve    apply, start the services, stop them when Enter is pressed
//! omniphp new <template> <name> [--no-db]
//!                    create a site: blank, wordpress, laravel, codeigniter,
//!                    symfony or prestashop
//! omniphp setup    trust the certificate authority and register site
//!                    addresses with Windows (asks for administrator permission)
//! omniphp cleanup  undo what OmniPHP changed in Windows: the trusted
//!                    authority, the site addresses and the startup entry
//!                    (asks for administrator permission if needed; the
//!                    uninstaller runs this)
//! ```
//!
//! `omniphp elevated ...` is internal: it is what `setup` and `cleanup` run as
//! administrator.

use anyhow::{Result, bail};

use crate::core::engine::Engine;
use crate::core::engine::NewSiteRequest;
use crate::core::paths::Paths;
use crate::core::quickapp::{self, Template};
use crate::core::system;

/// A release build is a GUI program and starts without a console, so what a
/// command prints would go nowhere. This borrows the console of the prompt
/// the command was typed at. It changes nothing when output is redirected to
/// a file or a pipe, or when there is a console already (debug builds).
fn attach_parent_console() {
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

pub fn run(args: &[String]) -> i32 {
    if args[0] == "elevated" {
        return system::elevated_main(&args[1..]);
    }
    attach_parent_console();
    match try_run(&args[0], &args[1..]) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("error: {error:#}");
            1
        }
    }
}

fn try_run(command: &str, rest: &[String]) -> Result<()> {
    if !matches!(command, "apply" | "serve" | "setup" | "cleanup" | "new") {
        bail!("unknown command \"{command}\" (expected: apply, serve, setup, cleanup, new)");
    }
    let mut engine = Engine::load(Paths::discover()?)?;
    // Cleaning up must work for an install that cannot run where it is.
    if command == "cleanup" {
        engine.set_run_at_startup(false)?;
        engine.remove_system_changes()?;
        println!("clean    Windows holds no trusted authority, site addresses or startup entry of this install");
        return Ok(());
    }
    engine.check_root()?;

    match command {
        "new" => return new_site(&mut engine, rest),
        "apply" => engine.apply()?,
        "setup" => {
            engine.apply()?;
            engine.setup_system()?;
        }
        _ => engine.start_all()?,
    }

    let snapshot = engine.snapshot();
    println!("root     {}", snapshot.root.display());
    for service in &snapshot.services {
        println!(
            "service  {} ({}) ports {} {:?}",
            service.id.name(),
            service.detail,
            service.ports,
            service.status
        );
    }
    for php in &snapshot.phps {
        let build = if php.thread_safe { "TS" } else { "NTS" };
        let default = if snapshot.default_php.as_ref() == Some(&php.id) { " (default)" } else { "" };
        println!("php      {} {build}, {} extensions{default}", php.version, php.extensions.len());
    }
    for site in &snapshot.sites {
        let php = snapshot.phps.iter().find(|php| Some(&php.id) == site.php.as_ref());
        println!(
            "site     {}  php {}  root {}",
            snapshot.site_url(site),
            php.map_or("-", |php| php.version.as_str()),
            site.document_root.display()
        );
    }
    println!("https    {}", match snapshot.https_port {
        Some(port) => format!("port {port}, authority trusted by Windows: {}", snapshot.ca_trusted),
        None => "off".into(),
    });
    if !snapshot.unregistered_hosts.is_empty() {
        println!("hosts    not registered yet: {}", snapshot.unregistered_hosts.join(", "));
    }

    if command == "serve" {
        println!("serving; press Enter to stop");
        let _ = std::io::stdin().read_line(&mut String::new());
        engine.stop_all()?;
        println!("stopped");
    }
    Ok(())
}

fn new_site(engine: &mut Engine, args: &[String]) -> Result<()> {
    let (Some(template), Some(name)) = (args.first(), args.get(1)) else {
        bail!("usage: omniphp new <template> <name> [--no-db]");
    };
    let template = Template::from_id(template).ok_or_else(|| {
        let known: Vec<&str> = Template::ALL.iter().map(|template| template.id()).collect();
        anyhow::anyhow!("unknown template \"{template}\" (expected one of: {})", known.join(", "))
    })?;
    let job = engine.prepare_site(&NewSiteRequest {
        template,
        name: name.clone(),
        php: None,
        create_database: !args.iter().any(|arg| arg == "--no-db"),
    })?;
    if let Err(error) = quickapp::install(&job, &|step| println!("{step}")) {
        engine.discard_site(&job);
        return Err(error);
    }
    let finished = engine.finish_site(&job);
    // MySQL was only started here to create the database; close it cleanly
    // rather than letting it die with this process.
    engine.stop_all()?;
    finished?;
    println!("created  {}  in {}", job.url, job.dir.display());
    if let Some(database) = &job.database {
        println!("database {} (user root, no password, port {})", database.name, database.port);
    }
    Ok(())
}