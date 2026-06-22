//! SOCKS5 tunnel probes through the routing proxy.
//!
//! Used by `jumphost test-tunnel` (and eventually supervisor warmup) to verify
//! end-to-end connectivity without external tools like curl or nc.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

// SOCKS5 constants (RFC 1928) — kept local so the probe stays a standalone client.
const SOCKS_VERSION: u8 = 0x05;
const AUTH_NONE: u8 = 0x00;
const CMD_CONNECT: u8 = 0x01;
const ATYP_DOMAIN: u8 = 0x03;
const ATYP_IPV4: u8 = 0x01;
const ATYP_IPV6: u8 = 0x04;
const REP_SUCCESS: u8 = 0x00;

/// A host:port target for a tunnel probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeTarget {
    pub host: String,
    pub port: u16,
}

impl ProbeTarget {
    pub fn display(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// Outcome of a single probe attempt.
#[derive(Debug, Clone)]
pub struct ProbeResult {
    pub target: ProbeTarget,
    pub ok: bool,
    pub latency: Duration,
    pub error: Option<String>,
}

/// Parse `host` or `host:port`. Missing port defaults to `default_port`.
pub fn parse_probe_target(input: &str, default_port: u16) -> Option<ProbeTarget> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }

    // IPv6 bracket form: [::1]:443
    if input.starts_with('[') {
        let end = input.find(']')?;
        let host = input[1..end].to_string();
        let port = input
            .get(end + 1..)
            .and_then(|rest| rest.strip_prefix(':'))
            .and_then(|p| p.parse().ok())
            .unwrap_or(default_port);
        return Some(ProbeTarget { host, port });
    }

    match input.rsplit_once(':') {
        Some((host, port_str)) if !host.is_empty() && port_str.chars().all(|c| c.is_ascii_digit()) => {
            let port: u16 = port_str.parse().ok()?;
            Some(ProbeTarget {
                host: host.to_string(),
                port,
            })
        }
        _ => Some(ProbeTarget {
            host: input.to_string(),
            port: default_port,
        }),
    }
}

/// SOCKS5 CONNECT through the routing proxy using domain-name ATYP (socks5h semantics).
pub async fn socks5_connect_via_routing(
    proxy_addr: SocketAddr,
    target: &ProbeTarget,
    connect_timeout: Duration,
) -> Result<Duration, String> {
    let started = Instant::now();

    let mut stream = timeout(connect_timeout, TcpStream::connect(proxy_addr))
        .await
        .map_err(|_| format!("timed out connecting to routing proxy at {proxy_addr}"))?
        .map_err(|e| format!("routing proxy unreachable at {proxy_addr}: {e}"))?;

    timeout(connect_timeout, stream.write_all(&[SOCKS_VERSION, 0x01, AUTH_NONE]))
        .await
        .map_err(|_| "timed out during SOCKS5 greeting".to_string())?
        .map_err(|e| format!("SOCKS5 greeting write failed: {e}"))?;

    let mut greeting_reply = [0u8; 2];
    timeout(connect_timeout, stream.read_exact(&mut greeting_reply))
        .await
        .map_err(|_| "timed out waiting for SOCKS5 greeting reply".to_string())?
        .map_err(|e| format!("SOCKS5 greeting read failed: {e}"))?;

    if greeting_reply[0] != SOCKS_VERSION || greeting_reply[1] != AUTH_NONE {
        return Err(format!(
            "SOCKS5 greeting failed: got {:02x} {:02x}",
            greeting_reply[0], greeting_reply[1]
        ));
    }

    let domain_bytes = target.host.as_bytes();
    if domain_bytes.len() > 255 {
        return Err("domain name exceeds 255 bytes".to_string());
    }

    let mut req = Vec::with_capacity(4 + 1 + domain_bytes.len() + 2);
    req.extend_from_slice(&[SOCKS_VERSION, CMD_CONNECT, 0x00, ATYP_DOMAIN]);
    req.push(domain_bytes.len() as u8);
    req.extend_from_slice(domain_bytes);
    req.extend_from_slice(&target.port.to_be_bytes());

    timeout(connect_timeout, stream.write_all(&req))
        .await
        .map_err(|_| "timed out sending SOCKS5 CONNECT".to_string())?
        .map_err(|e| format!("SOCKS5 CONNECT write failed: {e}"))?;

    let mut reply_header = [0u8; 4];
    timeout(connect_timeout, stream.read_exact(&mut reply_header))
        .await
        .map_err(|_| "timed out waiting for SOCKS5 CONNECT reply".to_string())?
        .map_err(|e| format!("SOCKS5 CONNECT read failed: {e}"))?;

    let reply_atyp = reply_header[3];
    match reply_atyp {
        ATYP_IPV4 => {
            let mut tail = [0u8; 6];
            timeout(connect_timeout, stream.read_exact(&mut tail))
                .await
                .map_err(|_| "timed out reading SOCKS5 reply bind address".to_string())?
                .map_err(|e| format!("SOCKS5 reply read failed: {e}"))?;
        }
        ATYP_IPV6 => {
            let mut tail = [0u8; 18];
            timeout(connect_timeout, stream.read_exact(&mut tail))
                .await
                .map_err(|_| "timed out reading SOCKS5 reply bind address".to_string())?
                .map_err(|e| format!("SOCKS5 reply read failed: {e}"))?;
        }
        ATYP_DOMAIN => {
            let name_len = timeout(connect_timeout, stream.read_u8())
                .await
                .map_err(|_| "timed out reading SOCKS5 reply domain length".to_string())?
                .map_err(|e| format!("SOCKS5 reply read failed: {e}"))? as usize;
            let mut tail = vec![0u8; name_len + 2];
            timeout(connect_timeout, stream.read_exact(&mut tail))
                .await
                .map_err(|_| "timed out reading SOCKS5 reply bind address".to_string())?
                .map_err(|e| format!("SOCKS5 reply read failed: {e}"))?;
        }
        other => {
            return Err(format!("SOCKS5 reply used unsupported ATYP: {other:#04x}"));
        }
    }

    let rep = reply_header[1];
    if rep != REP_SUCCESS {
        return Err(format!("SOCKS5 CONNECT failed with REP={rep:#04x}"));
    }

    Ok(started.elapsed())
}

/// Resolve probe targets from `[probe].hosts` in the config file.
pub fn probe_targets() -> Vec<ProbeTarget> {
    crate::config::probe_hosts_from_config()
        .unwrap_or_default()
        .iter()
        .filter_map(|h| parse_probe_target(h, crate::config::DEFAULT_PROBE_PORT))
        .collect()
}

/// Run probes for each target, retrying failures up to `retries` additional times.
pub async fn run_probes(
    proxy_addr: SocketAddr,
    targets: &[ProbeTarget],
    connect_timeout: Duration,
    retries: u32,
) -> Vec<ProbeResult> {
    let mut results = Vec::with_capacity(targets.len());
    for target in targets {
        let mut last_error = None;
        let mut latency = Duration::ZERO;
        let mut ok = false;

        for attempt in 0..=retries {
            match socks5_connect_via_routing(proxy_addr, target, connect_timeout).await {
                Ok(elapsed) => {
                    ok = true;
                    latency = elapsed;
                    last_error = None;
                    break;
                }
                Err(e) => {
                    last_error = Some(if attempt < retries {
                        format!("{e} (retrying)")
                    } else {
                        e
                    });
                }
            }
        }

        results.push(ProbeResult {
            target: target.clone(),
            ok,
            latency,
            error: last_error,
        });
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_host_only_defaults_port() {
        let t = parse_probe_target("example.com", 443).unwrap();
        assert_eq!(t.host, "example.com");
        assert_eq!(t.port, 443);
    }

    #[test]
    fn parse_host_with_port() {
        let t = parse_probe_target("example.com:8443", 443).unwrap();
        assert_eq!(t.host, "example.com");
        assert_eq!(t.port, 8443);
    }

    #[test]
    fn parse_ipv6_with_port() {
        let t = parse_probe_target("[::1]:443", 443).unwrap();
        assert_eq!(t.host, "::1");
        assert_eq!(t.port, 443);
    }

    #[test]
    fn parse_empty_returns_none() {
        assert!(parse_probe_target("", 443).is_none());
        assert!(parse_probe_target("  ", 443).is_none());
    }

    #[test]
    fn probe_targets_empty_without_config() {
        assert!(probe_targets().is_empty());
    }
}
