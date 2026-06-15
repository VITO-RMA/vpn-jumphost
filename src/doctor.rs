//! `jumphost doctor` — quick health check for common setup problems.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use tokio::net::TcpStream;

use crate::config::{self, DEFAULT_VPN_URL};
use crate::config_file;
use crate::cookie::{self, CookieStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok,
    Warn,
    Fail,
}

struct Line {
    label: &'static str,
    status: Status,
    detail: String,
}

pub async fn run() -> ExitCode {
    let mut lines = Vec::new();
    check_config(&mut lines);
    check_credentials(&mut lines);
    check_cookie(&mut lines).await;
    check_listeners(&mut lines).await;
    check_proxychains(&mut lines);

    eprintln!("jumphost doctor\n");
    let mut failed = false;
    for line in &lines {
        let tag = match line.status {
            Status::Ok => "ok  ",
            Status::Warn => "warn",
            Status::Fail => "FAIL",
        };
        eprintln!("  [{tag}]  {:<22} {}", line.label, line.detail);
        if line.status == Status::Fail {
            failed = true;
        }
    }
    eprintln!();

    if failed {
        eprintln!("doctor: one or more checks failed");
        ExitCode::FAILURE
    } else {
        eprintln!("doctor: all critical checks passed");
        ExitCode::SUCCESS
    }
}

fn push(lines: &mut Vec<Line>, label: &'static str, status: Status, detail: impl Into<String>) {
    lines.push(Line {
        label,
        status,
        detail: detail.into(),
    });
}

fn check_config(lines: &mut Vec<Line>) {
    let path = config_file::config_file_path();
    if !path.is_file() {
        push(
            lines,
            "config file",
            Status::Fail,
            format!("missing: {}", path.display()),
        );
    } else {
        push(
            lines,
            "config file",
            Status::Ok,
            path.display().to_string(),
        );
    }

    let vpn_url = config::cfg_string("VPN_URL", DEFAULT_VPN_URL);
    if vpn_url.is_empty() {
        push(lines, "vpn_url", Status::Fail, "not set in config");
    } else {
        push(lines, "vpn_url", Status::Ok, vpn_url);
    }

    let proxy = config::proxy_domains();
    if proxy.is_empty() {
        push(
            lines,
            "domains.proxy",
            Status::Fail,
            "empty — add VPN domain suffixes (e.g. vito.local)",
        );
    } else {
        push(
            lines,
            "domains.proxy",
            Status::Ok,
            proxy.join(", "),
        );
    }
}

fn check_credentials(lines: &mut Vec<Line>) {
    match config::vpn_credentials() {
        Some(_) => push(lines, "credentials", Status::Ok, "configured"),
        None => push(
            lines,
            "credentials",
            Status::Warn,
            "not found — run: jumphost authenticate",
        ),
    }
}

async fn check_cookie(lines: &mut Vec<Line>) {
    let cookie_file = config::cookie_file_path();
    if !cookie_file.is_file() {
        push(
            lines,
            "cookie",
            Status::Warn,
            format!("missing: {}", cookie_file.display()),
        );
        return;
    }

    match cookie::validate_file(&cookie_file).await {
        CookieStatus::Valid => {
            push(lines, "cookie", Status::Ok, cookie_file.display().to_string());
        }
        CookieStatus::Invalid => {
            push(
                lines,
                "cookie",
                Status::Fail,
                format!("invalid or expired ({})", cookie_file.display()),
            );
        }
        CookieStatus::NetworkError => {
            push(
                lines,
                "cookie",
                Status::Warn,
                "could not reach VPN endpoint to validate",
            );
        }
    }
}

async fn check_listeners(lines: &mut Vec<Line>) {
    let rp_bind = config::cfg_string("ROUTING_PROXY_BIND", config::DEFAULT_ROUTING_PROXY_BIND);
    let rp_port = config::cfg_u16("ROUTING_PROXY_PORT", config::DEFAULT_ROUTING_PROXY_PORT);
    let socks_port = config::cfg_u16("SOCKS_PORT", config::DEFAULT_SOCKS_PORT);
    let pac_bind = config::cfg_string("PAC_SERVE_BIND", config::DEFAULT_PAC_BIND);
    let pac_port = config::cfg_u16("PAC_SERVE_PORT", config::DEFAULT_PAC_PORT);

    if listener_up(&rp_bind, rp_port).await {
        push(
            lines,
            "routing proxy",
            Status::Ok,
            format!("{rp_bind}:{rp_port}"),
        );
    } else {
        push(
            lines,
            "routing proxy",
            Status::Fail,
            format!("not listening on {rp_bind}:{rp_port} — start jumphost"),
        );
    }

    if listener_up(&rp_bind, socks_port).await {
        push(
            lines,
            "ocproxy (VPN)",
            Status::Ok,
            format!("{rp_bind}:{socks_port}"),
        );
    } else {
        push(
            lines,
            "ocproxy (VPN)",
            Status::Warn,
            format!("not listening on {rp_bind}:{socks_port} — tunnel may still be starting"),
        );
    }

    if config::serve_pac() {
        if listener_up(&pac_bind, pac_port).await {
            push(
                lines,
                "PAC server",
                Status::Ok,
                format!("http://{pac_bind}:{pac_port}"),
            );
        } else {
            push(
                lines,
                "PAC server",
                Status::Warn,
                format!("serve_pac = true but nothing on {pac_bind}:{pac_port}"),
            );
        }
    }
}

fn check_proxychains(lines: &mut Vec<Line>) {
    let Some(bin) = find_proxychains_binary() else {
        push(
            lines,
            "proxychains",
            Status::Warn,
            "not on PATH (optional — needed for DBeaver/psql)",
        );
        return;
    };
    push(lines, "proxychains", Status::Ok, bin);

    let rp_port = config::cfg_u16("ROUTING_PROXY_PORT", config::DEFAULT_ROUTING_PROXY_PORT);
    match find_proxychains_config() {
        None => push(
            lines,
            "proxychains config",
            Status::Warn,
            "not found — run: just proxychains-setup",
        ),
        Some(path) => match validate_proxychains_config(&path, rp_port) {
            Ok(()) => push(
                lines,
                "proxychains config",
                Status::Ok,
                path.display().to_string(),
            ),
            Err(msg) => push(
                lines,
                "proxychains config",
                Status::Warn,
                format!("{} ({})", msg, path.display()),
            ),
        },
    }
}

async fn listener_up(host: &str, port: u16) -> bool {
    let addr: SocketAddr = match format!("{host}:{port}").parse() {
        Ok(a) => a,
        Err(_) => return false,
    };
    tokio::time::timeout(Duration::from_secs(2), TcpStream::connect(addr))
        .await
        .is_ok_and(|r| r.is_ok())
}

fn find_proxychains_binary() -> Option<String> {
    for name in ["proxychains4", "proxychains", "proxychains-ng"] {
        if let Some(path) = find_in_path(name) {
            return Some(path.display().to_string());
        }
    }
    None
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn find_proxychains_config() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("PROXYCHAINS_CONF") {
        let p = PathBuf::from(path);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Some(home) = directories::BaseDirs::new() {
        let p = home.home_dir().join(".proxychains").join("proxychains.conf");
        if p.is_file() {
            return Some(p);
        }
    }
    let system = PathBuf::from("/etc/proxychains4.conf");
    if system.is_file() {
        return Some(system);
    }
    None
}

/// Validate that a proxychains config enables remote DNS and points at the routing proxy.
fn validate_proxychains_config(path: &Path, routing_port: u16) -> Result<(), &'static str> {
    let contents = std::fs::read_to_string(path).map_err(|_| "unreadable")?;
    if !config_has_proxy_dns(&contents) {
        return Err("missing proxy_dns");
    }
    if !config_has_socks_upstream(&contents, routing_port) {
        return Err("missing socks5 127.0.0.1 routing proxy port");
    }
    Ok(())
}

fn config_has_proxy_dns(contents: &str) -> bool {
    contents.lines().any(|line| {
        let trimmed = line.split('#').next().unwrap_or("").trim();
        trimmed.eq_ignore_ascii_case("proxy_dns")
    })
}

fn config_has_socks_upstream(contents: &str, port: u16) -> bool {
    let port_str = port.to_string();
    contents.lines().any(|line| {
        let trimmed = line.split('#').next().unwrap_or("").trim();
        if !trimmed.to_ascii_lowercase().starts_with("socks5") {
            return false;
        }
        trimmed.contains("127.0.0.1") && trimmed.contains(&port_str)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_proxy_dns() {
        let conf = "strict_chain\nproxy_dns\n[ProxyList]\nsocks5 127.0.0.1 1081\n";
        assert!(config_has_proxy_dns(conf));
    }

    #[test]
    fn ignores_commented_proxy_dns() {
        let conf = "# proxy_dns\nstrict_chain\n";
        assert!(!config_has_proxy_dns(conf));
    }

    #[test]
    fn detects_socks_upstream() {
        let conf = "[ProxyList]\nsocks5 127.0.0.1 1081\n";
        assert!(config_has_socks_upstream(conf, 1081));
        assert!(!config_has_socks_upstream(conf, 1080));
    }
}
