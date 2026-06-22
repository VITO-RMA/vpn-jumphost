//! Shared configuration: defaults, config-file lookups, state-dir paths.
//!
//! All values mirror those used by the original Python `scripts/jumphost.py`
//! so existing configuration (cookie files, log files) continues to
//! work after the migration to Rust.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use directories::BaseDirs;

use crate::{config_file, credential_store};

/// Default F5 VPN endpoint URL (empty — must be configured via config file, env var, or CLI).
pub const DEFAULT_VPN_URL: &str = "";

/// Default openconnect `--protocol=` value.
pub const DEFAULT_VPN_PROTOCOL: &str = "f5";

/// Default ocproxy SOCKS5 port.
pub const DEFAULT_SOCKS_PORT: u16 = 1080;

/// Default routing-proxy listen port.
pub const DEFAULT_ROUTING_PROXY_PORT: u16 = 1081;

/// Default routing-proxy bind address.
pub const DEFAULT_ROUTING_PROXY_BIND: &str = "127.0.0.1";

/// Default ocproxy keepalive (seconds) for `-k`.
pub const DEFAULT_OCPROXY_KEEPALIVE: u32 = 60;

/// Default PAC HTTP server port.
pub const DEFAULT_PAC_PORT: u16 = 8091;

/// Default PAC HTTP server bind address.
pub const DEFAULT_PAC_BIND: &str = "127.0.0.1";

/// Default supervisor periodic cookie-check interval in seconds.
pub const DEFAULT_CHECK_INTERVAL_SECS: u64 = 300;

/// F5 cookie name set by the VPN gateway after a successful SSO login.
pub const COOKIE_NAME: &str = "MRHSession";

/// Endpoint probed by [`crate::cookie::validate_cookie`] to decide whether
/// the current cookie is still accepted by the VPN gateway.
pub const COOKIE_PROBE_PATH: &str = "/vdesk/vpn/index.php3?outform=xml";

/// Default TCP port when a probe target omits an explicit port.
pub const DEFAULT_PROBE_PORT: u16 = 443;

/// Default per-probe connect timeout.
pub const DEFAULT_PROBE_TIMEOUT_SECS: u64 = 10;

// ── Domain routing constants (single source of truth) ────────────────────────
//
// Both the PAC generator (`pac::generate`) and the routing SOCKS5 proxy
// (`routing::route_for`) consume these lists. Keeping them here ensures the
// two stay in sync — there is no longer a Python file to drift from.

/// Domains that should be tunneled through the VPN (compiled-in default; empty — configure via config file `[domains].proxy`).
const DEFAULT_PROXY_DOMAINS: &[&str] = &[];

/// Domains that must always be reached directly (compiled-in default;
/// empty — configure via config file `[domains].direct`). The VPN login
/// portal itself should be listed here so it stays reachable even when
/// the tunnel is down or misconfigured.
const DEFAULT_DIRECT_DOMAINS: &[&str] = &[];

/// Effective proxy domains: config file overrides compiled-in defaults.
///
/// Cached on first call — the result is stable for the lifetime of the
/// process. Both the PAC generator and the routing proxy call this.
pub fn proxy_domains() -> &'static [String] {
    static CACHED: OnceLock<Vec<String>> = OnceLock::new();
    CACHED.get_or_init(|| {
        let cfg = config_file::get();
        if let Some(ref domains) = cfg.domains {
            if let Some(ref list) = domains.proxy {
                return list.clone();
            }
        }
        DEFAULT_PROXY_DOMAINS.iter().map(|s| s.to_string()).collect()
    })
}

/// Effective direct domains: config file overrides compiled-in defaults.
///
/// Cached on first call — the result is stable for the lifetime of the
/// process.
pub fn direct_domains() -> &'static [String] {
    static CACHED: OnceLock<Vec<String>> = OnceLock::new();
    CACHED.get_or_init(|| {
        let cfg = config_file::get();
        if let Some(ref domains) = cfg.domains {
            if let Some(ref list) = domains.direct {
                return list.clone();
            }
        }
        DEFAULT_DIRECT_DOMAINS.iter().map(|s| s.to_string()).collect()
    })
}

/// State directory: `$XDG_STATE_HOME/vpn-jumphost/` (or
/// `~/.local/state/vpn-jumphost/` if XDG_STATE_HOME is unset).
pub fn state_dir() -> PathBuf {
    if let Some(raw) = std::env::var_os("XDG_STATE_HOME") {
        let p = PathBuf::from(raw);
        if !p.as_os_str().is_empty() {
            return p.join("vpn-jumphost");
        }
    }
    BaseDirs::new()
        .map(|b| b.home_dir().join(".local").join("state"))
        .unwrap_or_else(|| PathBuf::from(".local").join("state"))
        .join("vpn-jumphost")
}

pub fn cookie_file_path() -> PathBuf {
    state_dir().join("cookie")
}

/// Persistent browser profile directory used by the cookie-fetch flow.
pub fn default_browser_profile_dir() -> PathBuf {
    state_dir().join("chromium-profile")
}

/// Resolve a string config value: config file > compiled-in default.
pub fn cfg_string(key: &str, default: &str) -> String {
    if let Some(val) = lookup_string(key) {
        return val;
    }
    default.to_string()
}

/// Resolve a u16 config value: config file > compiled-in default.
pub fn cfg_u16(key: &str, default: u16) -> u16 {
    if let Some(v) = lookup_u16(key) {
        return v;
    }
    default
}

/// Resolve a u32 config value: config file > compiled-in default.
pub fn cfg_u32(key: &str, default: u32) -> u32 {
    if let Some(v) = lookup_u32(key) {
        return v;
    }
    default
}

/// Ensure the parent directory of `path` exists. Returns Ok if creation
/// succeeded or the directory already existed.
pub fn ensure_parent_dir(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    Ok(())
}

// ── Credential resolution ────────────────────────────────────────────────

/// Resolved VPN credentials (username + password).
#[derive(Debug, Clone)]
pub struct VpnCredentials {
    pub username: String,
    pub password: String,
}

/// Resolve VPN credentials.
///
/// Each source must supply **both** username and password; sources are never
/// mixed. Precedence (highest → lowest):
///
/// 1. Environment variables (`VPN_USERNAME` + `VPN_PASSWORD`).
/// 2. OS keyring (macOS Keychain / Linux Secret Service).
/// 3. Config file `[credentials]` `username_file` / `password_file` paths.
///
/// Returns `Some(VpnCredentials)` only when both fields are non-empty from
/// the same source. Returns `None` otherwise.
pub fn vpn_credentials() -> Option<VpnCredentials> {
    // 1. Read from the environment
    if let Some(creds) = env_vpn_credentials() {
        return Some(creds);
    }

    // 2. OS keyring
    if let Some((u, p)) = credential_store::get_credentials() {
        if !u.is_empty() && !p.is_empty() {
            return Some(VpnCredentials { username: u, password: p });
        }
    }

    // 3. Config file *_file paths
    let cfg = config_file::get();
    if let Some(creds) = cfg.credentials.as_ref() {
        let username = creds.username_file.as_deref().and_then(read_secret_file);
        let password = creds.password_file.as_deref().and_then(read_secret_file);
        if let (Some(username), Some(password)) = (username, password) {
            return Some(VpnCredentials { username, password });
        }
    }

    None
}

/// Read VPN credentials from `VPN_USERNAME` and `VPN_PASSWORD` only.
///
/// Returns `Some` only when both variables are set and non-empty.
pub fn env_vpn_credentials() -> Option<VpnCredentials> {
    let username = std::env::var("VPN_USERNAME").ok().filter(|s| !s.is_empty())?;
    let password = std::env::var("VPN_PASSWORD").ok().filter(|s| !s.is_empty())?;
    Some(VpnCredentials { username, password })
}

/// Read a secret from a file; returns `None` (with a warning) on error or empty content.
fn read_secret_file(path: &Path) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let trimmed = contents.trim().to_string();
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
            tracing::warn!(file = %path.display(), "secret file exists but is empty");
        }
        Err(e) => {
            tracing::warn!(file = %path.display(), error = %e, "could not read secret file");
        }
    }
    None
}

// ── Config-file lookup helpers ────────────────────────────────────────────
//
// These map well-known config keys to their corresponding field in the
// config file. This keeps the `cfg_string` / `cfg_u16` / `cfg_u32` helpers
// generic while still honoring the TOML overrides.

/// Look up a string value from the config file, keyed by the env-var name.
fn lookup_string(key: &str) -> Option<String> {
    let cfg = config_file::get();
    match key {
        "VPN_URL" => cfg.vpn_url.clone(),
        "VPN_PROTOCOL" => cfg.vpn_protocol.clone(),
        "ROUTING_PROXY_BIND" => cfg.routing_proxy.as_ref().and_then(|r| r.bind.clone()),
        "PAC_SERVE_BIND" => cfg.pac_server.as_ref().and_then(|p| p.bind.clone()),
        _ => None,
    }
}

/// Look up a u16 value from the config file, keyed by the env-var name.
fn lookup_u16(key: &str) -> Option<u16> {
    let cfg = config_file::get();
    match key {
        "SOCKS_PORT" => cfg.socks_port,
        "ROUTING_PROXY_PORT" => cfg.routing_proxy.as_ref().and_then(|r| r.port),
        "PAC_SERVE_PORT" => cfg.pac_server.as_ref().and_then(|p| p.port),
        _ => None,
    }
}

/// Look up a u32 value from the config file, keyed by the env-var name.
fn lookup_u32(key: &str) -> Option<u32> {
    let cfg = config_file::get();
    match key {
        "OCPROXY_KEEPALIVE" => cfg.ocproxy_keepalive,
        _ => None,
    }
}

pub fn no_headless() -> bool {
    config_file::get().no_headless.unwrap_or(false)
}

pub fn serve_pac() -> bool {
    config_file::get().serve_pac.unwrap_or(false)
}

pub fn cookie_check_interval() -> Duration {
    let secs = config_file::get().check_interval.unwrap_or(DEFAULT_CHECK_INTERVAL_SECS as f64);
    Duration::from_secs_f64(secs.max(1.0))
}

pub fn chromium_path() -> Option<PathBuf> {
    config_file::get().chromium_path.clone()
}

/// Configured `[probe].hosts` entries, if any.
pub fn probe_hosts_from_config() -> Option<Vec<String>> {
    config_file::get()
        .probe
        .as_ref()
        .and_then(|p| p.hosts.clone())
}

pub fn probe_timeout() -> Duration {
    let secs = config_file::get()
        .probe
        .as_ref()
        .and_then(|p| p.timeout_secs)
        .unwrap_or(DEFAULT_PROBE_TIMEOUT_SECS);
    Duration::from_secs(secs.max(1))
}

pub fn probe_retries() -> u32 {
    config_file::get()
        .probe
        .as_ref()
        .and_then(|p| p.retries)
        .unwrap_or(0)
}

/// Routing label for probe output (`direct` vs `tunnel`).
pub fn route_label_for_host(hostname: &str) -> &'static str {
    for pat in direct_domains() {
        if domain_matches(hostname, pat) {
            return "direct";
        }
    }
    for pat in proxy_domains() {
        if domain_matches(hostname, pat) {
            return "tunnel";
        }
    }
    "direct"
}

fn domain_matches(hostname: &str, pattern: &str) -> bool {
    let h = hostname.to_ascii_lowercase();
    let p = pattern.to_ascii_lowercase();
    if h == p {
        return true;
    }
    h.ends_with(&format!(".{p}"))
}
