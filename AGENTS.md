# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

OmniPHP is a Laragon-style local PHP development environment for Windows, written in Rust with gpui-kit. Its distinguishing feature is per-site PHP version, extensions, php.ini settings and document root. Windows only.

See also `AGENTS.md` (gpui-kit docs: https://gpui-kit.com/docs/ and https://gpui-kit.com/component/).

## Layout

The folder that contains this file is both the repository root and a live OmniPHP install root (the app finds its root as the nearest ancestor of the exe containing `bin\` and `www\`, or from `OMNIPHP_ROOT`).

- `omniphp\` — the Rust crate (the only source code).
- `installer\omniphp.iss` — Inno Setup script.
- `bin\` — everything downloaded (Apache and its `mod_fcgid`, PHP versions, MySQL, Mailpit, Redis, cloudflared, Composer, Adminer, Notepad++). Git-ignored: `runtimes.json` pins every version with its download address and SHA-256, and `fetch-runtimes.bat` (`scripts\fetch-runtimes.ps1`) downloads them. The app itself adds to `bin\` in two cases only, both checksum-verified: "Add PHP version" (`core\downloads.rs`) and Adminer when it is missing (`core\tools.rs`, which reads the pin from `runtimes.json`, compiled in with `include_str!`).
- `etc\` — everything the app generates (Apache/PHP/MySQL config, certificates) plus its settings in `etc\omniphp\omniphp.toml` and `sites.toml`. `etc\ssl\ca.key` is a private key.
- `www\` (sites), `data\` (databases), `logs\`, `tmp\`, `dist\` (installer output). All of these except source are git-ignored.

The root path must not contain spaces (PHP's FastCGI launcher fails).

## Commands

Run from `omniphp\`:

```
cargo build                    # debug build (console attached)
cargo build --release          # GUI build, no console window
cargo test                     # unit tests (core only)
cargo test hosts::             # tests of one module
cargo test merging_is_stable   # a single test by name
```

`cargo test` does not rebuild the app executable; run `cargo build` before launching `target\debug\omniphp.exe`.

Headless commands on the same exe (useful for testing without the UI): `omniphp apply` (write and validate all config), `omniphp serve` (apply, start services, stop on Enter), `omniphp setup` (trust CA + hosts, UAC prompt), `omniphp cleanup` (undo setup and the startup entry; the uninstaller runs it), `omniphp new <template> <name> [--no-db]`.

Installer (needs a release build first; bundles `bin\` by default, ~200 MB):

```
ISCC.exe installer\omniphp.iss                     # complete
ISCC.exe /DBundleRuntimes=0 installer\omniphp.iss  # app only
```

The installed Inno Setup is an older 6.x: use `ArchitecturesAllowed=x64`, not `x64compatible`.

## Architecture

`src\core\` has no gpui imports and holds all logic and all unit tests; `src\ui\` is the gpui-kit front end; `src\cli.rs` drives the same core headlessly.

**Engine** (`core\engine.rs`) is the single facade. It owns config, scanned runtimes, sites and running services, and exposes actions (`start`, `apply`, `update_site`, `share_site`, …) plus a `Snapshot` for rendering. The UI holds it as `Arc<Mutex<Engine>>`; `AppView::run` (`ui\app.rs`) executes an action on the background executor and then refreshes the snapshot. A 2 s poller calls `sync_sites` to pick up changes in `www\`.

**Per-site PHP** works through Apache `mod_fcgid` configured inside each generated vhost (`core\apache.rs`): `FcgidWrapper` points at the chosen version's `php-cgi.exe`, and `FcgidInitialEnv PHPRC` points at `etc\php\sites\<site>\`, where `core\php.rs` writes a php.ini made of PHP's `php.ini-development`, a managed block, then the site's own overrides last. The app manages no PHP processes itself. `mod_fcgid.so` lives in `bin\mod_fcgid\` and must match Apache's VS18 build.

**Apply cycle**: any change to sites or settings regenerates everything under `etc\`. The new configuration is first written to `etc\apache\staging\` and checked there with `httpd -t`; only when Apache accepts it are the live files replaced (in place, and only those that changed) and Apache reloaded. A rejected configuration stays in `staging\` for inspection and the live one is untouched. Generated files are overwritten; hand-written Apache config goes in `etc\apache\custom\`.

**Sites** (`core\sites.rs`) are the top-level folders of `www\`. Hostname defaults to `{name}.test`; document root is auto-detected (`public`, `public_html`, `webroot`, `web`). Overrides are stored centrally in `sites.toml`, keyed by folder name, never inside the project folder.

**HTTPS** (`core\ssl.rs`): a local name-constrained CA plus one leaf certificate per site, generated with rcgen. The CA is trusted once; new sites need no new trust prompt.

**Elevation** (`core\system.rs`): the app runs unelevated. The only privileged operations are trusting the CA and writing the hosts file; both happen by relaunching the exe as `omniphp elevated …` through UAC, and only when the user clicks "Set up". The helper is told its install with `--root`, never left to discover it. The same helper undoes both ("Undo changes to Windows" in Settings, `omniphp cleanup`). The Hosts button on the Services page (`core\tools.rs`) opens the hosts file as administrator in `bin\notepad++`, falling back to Windows Notepad; that is the user editing by hand, not the app. The hosts file gets one block per install, tagged with the install root (`core\hosts.rs`), so several installs coexist. Anything else that must tell installs apart (single-instance mutex, startup entry) is named after `Paths::install_key()`.

**Services** (`core\services.rs`, `core\proc.rs`): Apache, MySQL, Mailpit and Redis run as child processes inside a kill-on-close Job Object, with no console window. Graceful stop uses named Windows events (`ap<pid>_shutdown`, `ap<pid>_restart`, `mysqld<pid>_shutdown`). Cloudflare quick tunnels (`core\share.rs`) are separate and deliberately survive Apache restarts.

**Desktop shell**: tray icon via `tray-icon` on GPUI's main thread (`ui\tray.rs`); closing the window hides it (`ui\window.rs`), and the app quits only from the tray. One instance per install root (`single_instance.rs`).

## Things that are easy to get wrong

- `gpui-kit` is pinned to `=0.10.0` (it releases often and breaks APIs). `.when(..)` needs `gpui_kit::prelude::FluentBuilder`. Only the default `IconName` set is available.
- Mail: PHP is configured with native SMTP settings for Mailpit. `sendmail_path` does not work under FastCGI.
- The app icon is embedded by `build.rs` as resource id `1`; Explorer, the GPUI window and the tray all read that id. `assets\icon.ico`, `icon.png` and `icon-64.png` must be regenerated together.
- Default ports are 80/443/3306; Laragon or another stack running on the machine will collide.
- When a runtime in `bin\` is upgraded, update `runtimes.json` too (version, folder, address, SHA-256). `scripts\fetch-runtimes.ps1` must keep running under Windows PowerShell 5.1: no PowerShell 7 syntax, ASCII only.
- Quick-app installers (`core\quickapp.rs`) extract zips directly into the destination (moving staged files fails with "Access is denied" on Windows).
