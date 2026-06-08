//! Shared configuration: defaults, environment variables, state-dir paths.
//!
//! All values mirror those used by the original Python `scripts/jumphost.py`
//! so existing configuration (env vars, cookie files, log files) continues to
//! work after the migration to Rust.

use std::env;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use directories::BaseDirs;

use crate::config_file;

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
        DEFAULT_PROXY_DOMAINS
            .iter()
            .map(|s| s.to_string())
            .collect()
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
        DEFAULT_DIRECT_DOMAINS
            .iter()
            .map(|s| s.to_string())
            .collect()
    })
}

/// State directory: `$XDG_STATE_HOME/vpn-jumphost/` (or
/// `~/.local/state/vpn-jumphost/` if XDG_STATE_HOME is unset).
pub fn state_dir() -> PathBuf {
    if let Some(raw) = env::var_os("XDG_STATE_HOME") {
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

/// Default cookie file path: `<state_dir>/cookie`.
///
/// Precedence: `VPN_COOKIE_FILE` env var > config file `cookie_file` >
/// `<state_dir>/cookie`.
///
/// Mirrors the `$HOME` / `$XDG_STATE_HOME` literal-variable defense from
/// `scripts/start-vpn.sh`: if `VPN_COOKIE_FILE` contains an unexpanded
/// `$HOME` or `$XDG_STATE_HOME`, we ignore it and fall back.
pub fn default_cookie_file() -> PathBuf {
    if let Ok(raw) = env::var("VPN_COOKIE_FILE") {
        if raw.contains("$HOME") || raw.contains("$XDG_STATE_HOME") {
            tracing::warn!(
                value = %raw,
                "Ignoring VPN_COOKIE_FILE with unexpanded variable",
            );
        } else if !raw.is_empty() {
            return PathBuf::from(raw);
        }
    }
    if let Some(ref path) = config_file::get().cookie_file {
        return path.clone();
    }
    state_dir().join("cookie")
}

/// Default persistent browser profile directory used by the cookie-fetch
/// flow (mirrors `VPN_BROWSER_PROFILE_DIR` from `fetch-vpn-cookie.py`).
///
/// Precedence: env var > config file > default.
pub fn default_browser_profile_dir() -> PathBuf {
    if let Ok(raw) = env::var("VPN_BROWSER_PROFILE_DIR") {
        if !raw.is_empty() && !raw.contains("$HOME") && !raw.contains("$XDG_STATE_HOME") {
            return PathBuf::from(raw);
        }
    }
    if let Some(ref path) = config_file::get().browser_profile_dir {
        return path.clone();
    }
    state_dir().join("chromium-profile")
}

/// Resolve a string env var with a fallback: env var > config file > default.
pub fn env_string(key: &str, default: &str) -> String {
    if let Ok(val) = env::var(key) {
        if !val.is_empty() {
            return val;
        }
    }
    // Check config file for a matching key.
    if let Some(val) = config_file_string(key) {
        return val;
    }
    default.to_string()
}

/// Resolve an integer env var with a fallback: env var > config file > default.
/// Invalid values log a warning and return the default.
pub fn env_u16(key: &str, default: u16) -> u16 {
    match env::var(key) {
        Ok(raw) => match raw.parse::<u16>() {
            Ok(v) => return v,
            Err(_) => {
                tracing::warn!(
                    key = %key,
                    value = %raw,
                    default = default,
                    "invalid integer; using default"
                );
                return default;
            }
        },
        Err(_) => {}
    }
    // Check config file.
    if let Some(v) = config_file_u16(key) {
        return v;
    }
    default
}

/// Resolve a u32 env var with a fallback: env var > config file > default.
pub fn env_u32(key: &str, default: u32) -> u32 {
    match env::var(key) {
        Ok(raw) => match raw.parse::<u32>() {
            Ok(v) => return v,
            Err(_) => {
                tracing::warn!(
                    key = %key,
                    value = %raw,
                    default = default,
                    "invalid integer; using default"
                );
                return default;
            }
        },
        Err(_) => {}
    }
    // Check config file.
    if let Some(v) = config_file_u32(key) {
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

/// Resolve VPN credentials with the following precedence:
///
/// 1. Environment variable (`VPN_USERNAME` / `VPN_PASSWORD`) — highest priority.
/// 2. File contents (`VPN_USERNAME_FILE` / `VPN_PASSWORD_FILE`) — read the
///    first line (trimmed) from the file at the given path.
/// 3. Config file `[credentials]` table.
///
/// Returns `Some(VpnCredentials)` only when **both** username and password
/// resolve to a non-empty value. Returns `None` otherwise.
pub fn vpn_credentials() -> Option<VpnCredentials> {
    let username = resolve_secret("VPN_USERNAME", "VPN_USERNAME_FILE", |c| {
        c.credentials
            .as_ref()
            .map(|cr| (&cr.username, &cr.username_file))
    });
    let password = resolve_secret("VPN_PASSWORD", "VPN_PASSWORD_FILE", |c| {
        c.credentials
            .as_ref()
            .map(|cr| (&cr.password, &cr.password_file))
    });

    match (username, password) {
        (Some(u), Some(p)) if !u.is_empty() && !p.is_empty() => Some(VpnCredentials {
            username: u,
            password: p,
        }),
        _ => None,
    }
}

/// Resolve a single secret value.
///
/// Precedence: env var > env var *_FILE > config file value > config file
/// *_file path.
fn resolve_secret<F>(env_key: &str, file_env_key: &str, cfg_accessor: F) -> Option<String>
where
    F: FnOnce(&config_file::FileConfig) -> Option<(&Option<String>, &Option<PathBuf>)>,
{
    // 1. Try the direct environment variable.
    if let Ok(val) = env::var(env_key) {
        if !val.is_empty() {
            return Some(val);
        }
    }

    // 2. Try reading from the file path specified by the `*_FILE` env var.
    if let Ok(path) = env::var(file_env_key) {
        if !path.is_empty() {
            if let Some(val) = read_secret_file(&path, file_env_key) {
                return Some(val);
            }
        }
    }

    // 3. Try the config file.
    let cfg = config_file::get();
    if let Some((direct_val, file_path)) = cfg_accessor(cfg) {
        // 3a. Direct value in config.
        if let Some(val) = direct_val {
            if !val.is_empty() {
                return Some(val.clone());
            }
        }
        // 3b. File path in config.
        if let Some(path) = file_path {
            let path_str = path.display().to_string();
            if let Some(val) = read_secret_file(&path_str, "config_file") {
                return Some(val);
            }
        }
    }

    None
}

/// Read a secret from a file path; returns None (with a warning) on error.
fn read_secret_file(path: &str, source: &str) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let trimmed = contents.trim().to_string();
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
            tracing::warn!(
                file = %path,
                source = %source,
                "secret file exists but is empty",
            );
        }
        Err(e) => {
            tracing::warn!(
                file = %path,
                source = %source,
                error = %e,
                "could not read secret file",
            );
        }
    }
    None
}

// ── Config-file lookup helpers ────────────────────────────────────────────
//
// These map well-known env-var names to their corresponding field in the
// config file. This keeps the `env_string` / `env_u16` / `env_u32` helpers
// generic while still honoring the TOML overrides.

/// Look up a string value from the config file, keyed by the env-var name.
fn config_file_string(key: &str) -> Option<String> {
    let cfg = config_file::get();
    match key {
        "VPN_URL" => cfg.vpn_url.clone(),
        "VPN_PROTOCOL" => cfg.vpn_protocol.clone(),
        "ROUTING_PROXY_BIND" => cfg.routing_proxy.as_ref().and_then(|r| r.bind.clone()),
        "PAC_SERVE_BIND" => cfg.pac_server.as_ref().and_then(|p| p.bind.clone()),
        "PAC_PROXY_HOST" => cfg.pac_generate.as_ref().and_then(|p| p.proxy_host.clone()),
        "PAC_SOCKS_PORT" => cfg.pac_generate.as_ref().and_then(|p| p.socks_port.clone()),
        "PAC_PROXY_CHAIN" => cfg
            .pac_generate
            .as_ref()
            .and_then(|p| p.proxy_chain.clone()),
        _ => None,
    }
}

/// Look up a u16 value from the config file, keyed by the env-var name.
fn config_file_u16(key: &str) -> Option<u16> {
    let cfg = config_file::get();
    match key {
        "SOCKS_PORT" => cfg.socks_port,
        "ROUTING_PROXY_PORT" => cfg.routing_proxy.as_ref().and_then(|r| r.port),
        "PAC_SERVE_PORT" => cfg.pac_server.as_ref().and_then(|p| p.port),
        _ => None,
    }
}

/// Look up a u32 value from the config file, keyed by the env-var name.
fn config_file_u32(key: &str) -> Option<u32> {
    let cfg = config_file::get();
    match key {
        "OCPROXY_KEEPALIVE" => cfg.ocproxy_keepalive,
        "JUMPHOST_CHECK_INTERVAL" => cfg.check_interval.map(|v| v as u32),
        _ => None,
    }
}

/// Return the `no_headless` setting from the config file (if set).
pub fn config_file_no_headless() -> Option<bool> {
    config_file::get().no_headless
}

/// Return the `check_interval` as f64 from the config file (if set).
pub fn config_file_check_interval() -> Option<f64> {
    config_file::get().check_interval
}

/// Return the `chromium_path` from the config file (if set).
pub fn config_file_chromium_path() -> Option<PathBuf> {
    config_file::get().chromium_path.clone()
}
