//! TOML configuration file support.
//!
//! Reads the config file and exposes the parsed values as a global
//! singleton. The file location is resolved in order:
//!
//! 1. Explicit path set via [`set_path`] (i.e. the `-c / --config` CLI flag).
//! 2. `$XDG_CONFIG_HOME/vpn-jumphost/config.toml`.
//! 3. `~/.config/vpn-jumphost/config.toml`.
//!
//! The config file is optional — missing or unreadable files are silently
//! ignored.
//!
//! Precedence (highest → lowest): CLI flag > config file > compiled-in
//! default constant.

use std::path::PathBuf;
use std::sync::OnceLock;

use directories::BaseDirs;
use serde::Deserialize;
use tracing::{debug, info};

/// Global parsed config file (loaded once at first access).
static CONFIG: OnceLock<FileConfig> = OnceLock::new();

/// Optional CLI-provided config file path (`-c / --config`).  Must be set
/// via [`set_path`] **before** the first call to [`get`].
static CONFIG_PATH_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

/// Top-level TOML structure.
///
/// All fields are optional — a minimal (or empty) file is valid.
///
/// Example `config.toml`:
/// ```toml
/// vpn_url = "https://vpn.example.com"
/// vpn_protocol = "f5"
/// socks_port = 1080
/// ocproxy_keepalive = 60
/// check_interval = 300
/// no_headless = false
/// serve_pac = false
///
/// [routing_proxy]
/// bind = "127.0.0.1"
/// port = 1081
///
/// [pac_server]
/// bind = "127.0.0.1"
/// port = 8091
///
/// [domains]
/// proxy = ["example.com", "corp.local", "internal.example.com"]
/// direct = ["vpn.example.com"]
///
/// [credentials]
/// username = "user@example.com"
/// password_file = "/run/secrets/vpn_pass"
/// ```
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct FileConfig {
    /// F5 VPN endpoint URL.
    pub vpn_url: Option<String>,
    /// OpenConnect protocol.
    pub vpn_protocol: Option<String>,
    /// ocproxy SOCKS5 listen port.
    pub socks_port: Option<u16>,
    /// ocproxy keepalive seconds.
    pub ocproxy_keepalive: Option<u32>,
    /// Supervisor check interval in seconds.
    pub check_interval: Option<f64>,
    /// Never use headless mode.
    pub no_headless: Option<bool>,
    /// Start the in-process PAC HTTP server.
    pub serve_pac: Option<bool>,
    /// Chromium executable path.
    pub chromium_path: Option<PathBuf>,
    /// Enable debug-level (verbose) logging.
    pub verbose: Option<bool>,
    /// Routing proxy settings.
    pub routing_proxy: Option<RoutingProxyConfig>,
    /// PAC HTTP server settings.
    pub pac_server: Option<PacServerConfig>,
    /// Domain routing lists (overrides compiled-in constants).
    pub domains: Option<DomainsConfig>,
    /// VPN credentials.
    pub credentials: Option<CredentialsConfig>,
    /// Tunnel probe settings (`jumphost test-tunnel`).
    pub probe: Option<ProbeConfig>,
}

/// `[routing_proxy]` table.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct RoutingProxyConfig {
    /// Bind address.
    pub bind: Option<String>,
    /// Listen port.
    pub port: Option<u16>,
}

/// `[pac_server]` table.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct PacServerConfig {
    /// Bind address.
    pub bind: Option<String>,
    /// Listen port.
    pub port: Option<u16>,
}

/// `[domains]` table.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct DomainsConfig {
    /// Domains routed through the VPN (overrides `PROXY_DOMAINS` constant).
    pub proxy: Option<Vec<String>>,
    /// Domains always reached directly (overrides `DIRECT_DOMAINS` constant).
    pub direct: Option<Vec<String>>,
}

/// `[probe]` table.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct ProbeConfig {
    /// Probe targets as `host` or `host:port` (port defaults to 443 when omitted).
    pub hosts: Option<Vec<String>>,
    /// Per-probe connect timeout in seconds.
    pub timeout_secs: Option<u64>,
    /// Additional retries per failed probe.
    pub retries: Option<u32>,
}

/// `[credentials]` table.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct CredentialsConfig {
    /// VPN username (mirrors `VPN_USERNAME` env var).
    pub username: Option<String>,
    /// VPN password (mirrors `VPN_PASSWORD` env var).
    pub password: Option<String>,
    /// Path to username file.
    pub username_file: Option<PathBuf>,
    /// Path to password file.
    pub password_file: Option<PathBuf>,
}

/// Set an explicit config file path (from the `-c / --config` CLI flag).
///
/// Must be called **before** [`get`] is invoked for the first time;
/// later calls are silently ignored (the `OnceLock` is already set).
pub fn set_path(path: PathBuf) {
    let _ = CONFIG_PATH_OVERRIDE.set(path);
}

/// Return the effective config file path.
///
/// Resolution order:
/// 1. Explicit path from [`set_path`] (`-c / --config`).
/// 2. `$XDG_CONFIG_HOME/vpn-jumphost/config.toml`.
/// 3. `~/.config/vpn-jumphost/config.toml`.
pub fn config_file_path() -> PathBuf {
    if let Some(p) = CONFIG_PATH_OVERRIDE.get() {
        return p.clone();
    }
    if let Ok(raw) = std::env::var("XDG_CONFIG_HOME") {
        if !raw.is_empty() {
            return PathBuf::from(raw).join("vpn-jumphost").join("config.toml");
        }
    }
    BaseDirs::new()
        .map(|b| b.home_dir().join(".config"))
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join("vpn-jumphost")
        .join("config.toml")
}

/// Load and parse the config file. Returns `FileConfig::default()` if the
/// file doesn't exist or can't be parsed (with a warning logged for parse
/// errors).
///
/// When the path was set explicitly via [`set_path`] (`-c / --config`), a
/// missing or unreadable file is logged as a **warning** (the user asked
/// for it, so silence is misleading). For the XDG default path, a missing
/// file is expected and silently ignored.
fn load() -> FileConfig {
    let explicit = CONFIG_PATH_OVERRIDE.get().is_some();
    let path = config_file_path();
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            if explicit {
                // User explicitly asked for this file — always warn.
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "could not read config file (set via -c / --config)",
                );
            } else if e.kind() != std::io::ErrorKind::NotFound {
                // Only log if it's not a missing-file case (missing is normal).
                debug!(path = %path.display(), error = %e, "could not read config file");
            }
            return FileConfig::default();
        }
    };

    match toml::from_str::<FileConfig>(&contents) {
        Ok(cfg) => {
            info!(path = %path.display(), "loaded config file");
            cfg
        }
        Err(e) => {
            // Parse error is noteworthy — warn so the user knows their file
            // has a problem.
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "config file has syntax errors; ignoring"
            );
            FileConfig::default()
        }
    }
}

/// Get the global config (loaded once, lazily).
pub fn get() -> &'static FileConfig {
    CONFIG.get_or_init(load)
}
