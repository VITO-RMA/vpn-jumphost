//! OpenConnect + ocproxy process management.
//!
//! [`start`] spawns `openconnect --protocol=f5 --cookie-on-stdin
//! --script-tun --script "ocproxy -D <socks_port> -k <keepalive>"
//! <vpn_url>` with stdin redirected from the cookie file. openconnect
//! spawns ocproxy as its `--script-tun` peer (lwIP userspace stack),
//! and ocproxy serves SOCKS5 on `127.0.0.1:<socks_port>`.
//!
//! Termination: [`VpnProcess::stop`] sends SIGTERM and waits up to
//! `timeout`; on timeout the process is SIGKILL'd. openconnect cleans up
//! ocproxy via the `VPNFD` socketpair, so we only need to manage the
//! openconnect PID.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tokio::process::{Child, Command};
use tokio::time::timeout;
use tracing::{info, warn};

use crate::config;

/// Parameters captured at spawn time so [`VpnProcess`] can log them later.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct VpnSpawnInfo {
    pub vpn_url: String,
    pub socks_port: u16,
    pub cookie_file: PathBuf,
}

/// A running openconnect process (with ocproxy as its `--script-tun` child).
#[allow(dead_code)]
pub struct VpnProcess {
    child: Child,
    info: VpnSpawnInfo,
}

impl VpnProcess {
    /// PID of the openconnect process, if it has not yet been reaped.
    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    /// True if the underlying process is still alive.
    pub fn is_alive(&mut self) -> bool {
        match self.child.try_wait() {
            Ok(Some(_)) => false,
            Ok(None) => true,
            Err(_) => false,
        }
    }

    /// Information about how the process was spawned.
    #[allow(dead_code)]
    pub fn info(&self) -> &VpnSpawnInfo {
        &self.info
    }

    /// Wait for the child to exit on its own. Returns the exit status.
    #[allow(dead_code)]
    pub async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait().await
    }

    /// Send SIGTERM, wait up to `timeout`; on timeout escalate to SIGKILL.
    /// Returns once the process has actually been reaped.
    pub async fn stop(&mut self, term_timeout: Duration) -> Result<()> {
        let Some(pid) = self.child.id() else {
            // Already reaped.
            return Ok(());
        };
        info!(pid, "stopping VPN (openconnect)");
        // SIGTERM via nix so openconnect can run its normal teardown
        // (which closes VPNFD and lets ocproxy exit).
        if let Err(e) = kill(Pid::from_raw(pid as i32), Signal::SIGTERM) {
            // ESRCH = process already gone.
            warn!(error = %e, "SIGTERM failed (process may already be gone)");
        }

        match timeout(term_timeout, self.child.wait()).await {
            Ok(Ok(status)) => {
                info!(?status, "VPN stopped");
                Ok(())
            }
            Ok(Err(e)) => Err(anyhow!("waiting for openconnect failed: {e}")),
            Err(_) => {
                warn!(
                    "openconnect did not exit within {:?}; sending SIGKILL",
                    term_timeout
                );
                let _ = self.child.kill().await;
                let _ = self.child.wait().await;
                info!("VPN stopped (forced)");
                Ok(())
            }
        }
    }
}

/// Spawn openconnect with stdin redirected from `cookie_file`.
///
/// `socks_port` controls the ocproxy SOCKS5 listener (default 1080). The
/// routing proxy sits in front on port 1081.
pub fn start(cookie_file: &Path, socks_port: u16) -> Result<VpnProcess> {
    if !cookie_file.exists() {
        bail!("cookie file does not exist: {}", cookie_file.display());
    }
    let metadata = std::fs::metadata(cookie_file)
        .with_context(|| format!("stat {}", cookie_file.display()))?;
    if metadata.len() == 0 {
        bail!("cookie file is empty: {}", cookie_file.display());
    }

    for tool in &["openconnect", "ocproxy"] {
        if which(tool).is_none() {
            bail!("missing required executable: {tool}");
        }
    }

    let vpn_url = config::cfg_string("VPN_URL", config::DEFAULT_VPN_URL);
    let vpn_protocol = config::cfg_string("VPN_PROTOCOL", config::DEFAULT_VPN_PROTOCOL);
    let keepalive = config::cfg_u32("OCPROXY_KEEPALIVE", config::DEFAULT_OCPROXY_KEEPALIVE);

    let script = format!("ocproxy -D {socks_port} -k {keepalive}");
    info!(
        vpn_url = %vpn_url,
        protocol = %vpn_protocol,
        socks_port,
        keepalive,
        "starting openconnect → ocproxy"
    );

    let cookie_fh = File::open(cookie_file)
        .with_context(|| format!("opening cookie file {}", cookie_file.display()))?;
    let stdin = Stdio::from(cookie_fh);

    // We intentionally do NOT use `process_group(0)` / `start_new_session`:
    // keeping openconnect in our process group means SIGINT (Ctrl-C) reaches
    // it directly when running in the foreground.
    let child = Command::new("openconnect")
        .arg(format!("--protocol={vpn_protocol}"))
        .arg("--cookie-on-stdin")
        .arg("--script-tun")
        .arg("--script")
        .arg(&script)
        .arg(&vpn_url)
        .stdin(stdin)
        .spawn()
        .with_context(|| "failed to spawn openconnect")?;

    let pid = child.id().unwrap_or(0);
    info!(pid, "openconnect spawned");

    Ok(VpnProcess {
        child,
        info: VpnSpawnInfo {
            vpn_url,
            socks_port,
            cookie_file: cookie_file.to_path_buf(),
        },
    })
}

/// Resolve an executable name via `$PATH` lookup. Returns the absolute path
/// when found.
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}
