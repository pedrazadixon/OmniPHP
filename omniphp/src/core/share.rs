//! Sharing a site on the internet through a Cloudflare quick tunnel: a
//! temporary `https://<random>.trycloudflare.com` address that forwards to the
//! local Apache. No Cloudflare account is involved, and the address stops
//! working when the tunnel is closed.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use super::paths::Paths;
use super::proc::{kill_with_app, quiet_command};

pub struct Share {
    child: Child,
    pub url: String,
}

impl Share {
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    pub fn stop(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The 64-bit cloudflared in `bin\cloudflared`, or any other build found there.
pub fn find_cloudflared(paths: &Paths) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(paths.bin().join("cloudflared"))
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
                && path.file_name().is_some_and(|name| name.to_string_lossy().starts_with("cloudflared"))
        })
        .collect();
    candidates.sort_by_key(|path| !path.to_string_lossy().contains("amd64"));
    candidates.into_iter().next()
}

/// Opens a tunnel to `hostname` on the local Apache and waits for its public
/// address.
pub fn start(cloudflared: &Path, site: &str, hostname: &str, http_port: u16, paths: &Paths) -> Result<Share> {
    let dir = paths.etc().join("cloudflared");
    std::fs::create_dir_all(&dir)?;
    // Quick tunnels refuse to start when the user's own `.cloudflared\config.yml`
    // exists, so point cloudflared at an empty configuration of ours instead.
    let config = dir.join("quick-tunnel.yml");
    // (It holds one harmless setting, because cloudflared logs an error for an empty file.)
    std::fs::write(&config, "# Used by OmniPHP for quick tunnels.\nno-autoupdate: true\n")?;

    let log_file = paths.logs().join(format!("share-{site}.log"));
    let log = File::create(&log_file).with_context(|| format!("cannot write {}", log_file.display()))?;
    let child = quiet_command(cloudflared)
        .arg("--config")
        .arg(&config)
        .args(["tunnel", "--no-autoupdate", "--url"])
        .arg(format!("http://127.0.0.1:{http_port}"))
        // Apache picks the site by this name.
        .args(["--http-host-header", hostname])
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()
        .with_context(|| format!("cannot start {}", cloudflared.display()))?;
    kill_with_app(&child);
    let mut share = Share { child, url: String::new() };

    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        let output = std::fs::read_to_string(&log_file).unwrap_or_default();
        if let Some(url) = find_tunnel_url(&output) {
            share.url = url;
            return Ok(share);
        }
        if !share.is_alive() || Instant::now() > deadline {
            share.stop();
            let lines: Vec<&str> = output.lines().filter(|line| !line.trim().is_empty()).collect();
            bail!(
                "Cloudflare did not open a tunnel.\n{}",
                lines[lines.len().saturating_sub(4)..].join("\n")
            );
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// The `https://….trycloudflare.com` address in cloudflared's output.
pub fn find_tunnel_url(output: &str) -> Option<String> {
    const SUFFIX: &str = ".trycloudflare.com";
    let end = output.find(SUFFIX)? + SUFFIX.len();
    let start = output[..end].rfind("https://")?;
    let url = &output[start..end];
    url["https://".len()..]
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.'))
        .then(|| url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_public_address_in_cloudflared_output() {
        let output = "2026-11-21T22:00:00Z INF Requesting new quick Tunnel on trycloudflare.com...\n\
                      2026-11-21T22:00:02Z INF |  https://shop-demo-blue-river.trycloudflare.com   |\n";
        assert_eq!(
            find_tunnel_url(output).as_deref(),
            Some("https://shop-demo-blue-river.trycloudflare.com")
        );
        assert_eq!(find_tunnel_url("INF Requesting new quick Tunnel on trycloudflare.com..."), None);
    }
}
