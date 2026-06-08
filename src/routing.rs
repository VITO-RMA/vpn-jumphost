//! Routing SOCKS5 proxy: per-domain VPN routing.
//!
//! Listens on `127.0.0.1:1081` (configurable) and routes SOCKS5 CONNECT
//! requests:
//!
//! - VPN domains (matching [`crate::config::PROXY_DOMAINS`]) → upstream
//!   ocproxy SOCKS5 on port 1080, using ATYP 0x03 (domain name) so ocproxy
//!   resolves DNS through the VPN.
//! - Always-DIRECT domains (matching [`crate::config::DIRECT_DOMAINS`],
//!   checked first) → direct.
//! - Raw IP addresses (ATYP 0x01/0x04) → direct.
//! - Everything else → direct.
//!
//! Ported from the standalone `routing-proxy` crate (now removed). Behavior
//! is preserved 1:1; tests at the bottom of this file cover the routing
//! decision matrix.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::config::{direct_domains, proxy_domains};

// ── SOCKS5 constants (RFC 1928) ─────────────────────────────────────────

const SOCKS_VERSION: u8 = 0x05;
const AUTH_NONE: u8 = 0x00;
const CMD_CONNECT: u8 = 0x01;
const ATYP_IPV4: u8 = 0x01;
const ATYP_DOMAIN: u8 = 0x03;
const ATYP_IPV6: u8 = 0x04;

// Reply codes.
const REP_SUCCESS: u8 = 0x00;
const REP_GENERAL_FAILURE: u8 = 0x01;
const REP_NET_UNREACHABLE: u8 = 0x03;
const REP_HOST_UNREACHABLE: u8 = 0x04;
const REP_CONN_REFUSED: u8 = 0x05;
const REP_CMD_NOT_SUPPORTED: u8 = 0x07;
const REP_ATYP_NOT_SUPPORTED: u8 = 0x08;

// ── Destination target ──────────────────────────────────────────────────

/// Parsed destination from a SOCKS5 CONNECT request.
enum Target {
    Ipv4(Ipv4Addr, u16),
    Ipv6(Ipv6Addr, u16),
    Domain(String, u16),
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Target::Ipv4(ip, port) => write!(f, "{ip}:{port}"),
            Target::Ipv6(ip, port) => write!(f, "[{ip}]:{port}"),
            Target::Domain(name, port) => write!(f, "{name}:{port}"),
        }
    }
}

// ── Routing decision ────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Direct,
    Upstream,
}

/// Check whether `hostname` matches a domain pattern (case-insensitive
/// exact match, or subdomain match).
fn domain_matches(hostname: &str, pattern: &str) -> bool {
    let h = hostname.to_ascii_lowercase();
    let p = pattern.to_ascii_lowercase();
    if h == p {
        return true;
    }
    h.ends_with(&format!(".{p}"))
}

/// Decide how to route a target.
fn route_for(target: &Target) -> Route {
    match target {
        Target::Ipv4(..) | Target::Ipv6(..) => {
            debug!(target = %target, route = "direct", reason = "raw IP address");
            Route::Direct
        }
        Target::Domain(hostname, _) => {
            for pat in direct_domains() {
                if domain_matches(hostname, pat) {
                    debug!(target = %target, route = "direct", reason = "DIRECT_DOMAINS match");
                    return Route::Direct;
                }
            }
            for pat in proxy_domains() {
                if domain_matches(hostname, pat) {
                    debug!(target = %target, route = "upstream", reason = "PROXY_DOMAINS match");
                    return Route::Upstream;
                }
            }
            debug!(target = %target, route = "direct", reason = "no domain match");
            Route::Direct
        }
    }
}

// ── SOCKS5 reply helpers ────────────────────────────────────────────────

fn socks5_reply(rep: u8) -> [u8; 10] {
    [
        SOCKS_VERSION,
        rep,
        0x00,
        ATYP_IPV4,
        0,
        0,
        0,
        0, // BND.ADDR (0.0.0.0)
        0,
        0, // BND.PORT (0)
    ]
}

fn socks5_reply_with_bind(rep: u8, addr: SocketAddr) -> Vec<u8> {
    let mut buf = vec![SOCKS_VERSION, rep, 0x00];
    match addr {
        SocketAddr::V4(v4) => {
            buf.push(ATYP_IPV4);
            buf.extend_from_slice(&v4.ip().octets());
            buf.extend_from_slice(&v4.port().to_be_bytes());
        }
        SocketAddr::V6(v6) => {
            buf.push(ATYP_IPV6);
            buf.extend_from_slice(&v6.ip().octets());
            buf.extend_from_slice(&v6.port().to_be_bytes());
        }
    }
    buf
}

async fn send_error(stream: &mut TcpStream, rep: u8, msg: &str) -> io::Error {
    let _ = stream.write_all(&socks5_reply(rep)).await;
    io::Error::new(io::ErrorKind::Other, msg)
}

fn rep_for_io_error(e: &io::Error) -> u8 {
    match e.kind() {
        io::ErrorKind::ConnectionRefused => REP_CONN_REFUSED,
        io::ErrorKind::AddrNotAvailable
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::TimedOut => REP_HOST_UNREACHABLE,
        _ => {
            if let Some(raw) = e.raw_os_error() {
                // ENETUNREACH = 101 on Linux, 51 on macOS — accept either.
                if raw == 101 || raw == 51 {
                    return REP_NET_UNREACHABLE;
                }
            }
            REP_GENERAL_FAILURE
        }
    }
}

// ── SOCKS5 client handshake ─────────────────────────────────────────────

async fn client_greeting(stream: &mut TcpStream) -> io::Result<()> {
    let ver = stream.read_u8().await?;
    if ver != SOCKS_VERSION {
        return Err(send_error(stream, REP_GENERAL_FAILURE, "unsupported SOCKS version").await);
    }
    let nmethods = stream.read_u8().await? as usize;
    let mut methods = vec![0u8; nmethods];
    stream.read_exact(&mut methods).await?;

    if !methods.contains(&AUTH_NONE) {
        let _ = stream.write_all(&[SOCKS_VERSION, 0xFF]).await;
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "no acceptable auth method",
        ));
    }

    stream.write_all(&[SOCKS_VERSION, AUTH_NONE]).await?;
    Ok(())
}

async fn client_request(stream: &mut TcpStream) -> io::Result<Target> {
    let ver = stream.read_u8().await?;
    if ver != SOCKS_VERSION {
        return Err(send_error(stream, REP_GENERAL_FAILURE, "bad SOCKS version in request").await);
    }

    let cmd = stream.read_u8().await?;
    let _rsv = stream.read_u8().await?;
    let atyp = stream.read_u8().await?;

    if cmd != CMD_CONNECT {
        return Err(send_error(stream, REP_CMD_NOT_SUPPORTED, "only CONNECT is supported").await);
    }

    let target = match atyp {
        ATYP_IPV4 => {
            let mut addr = [0u8; 4];
            stream.read_exact(&mut addr).await?;
            let port = stream.read_u16().await?;
            Target::Ipv4(Ipv4Addr::from(addr), port)
        }
        ATYP_DOMAIN => {
            let len = stream.read_u8().await? as usize;
            if len == 0 {
                return Err(send_error(stream, REP_GENERAL_FAILURE, "empty domain name").await);
            }
            let mut name = vec![0u8; len];
            stream.read_exact(&mut name).await?;
            let domain = String::from_utf8(name).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "domain name is not valid UTF-8")
            })?;
            let port = stream.read_u16().await?;
            Target::Domain(domain, port)
        }
        ATYP_IPV6 => {
            let mut addr = [0u8; 16];
            stream.read_exact(&mut addr).await?;
            let port = stream.read_u16().await?;
            Target::Ipv6(Ipv6Addr::from(addr), port)
        }
        _ => {
            return Err(
                send_error(stream, REP_ATYP_NOT_SUPPORTED, "unsupported address type").await,
            );
        }
    };

    Ok(target)
}

// ── Direct connection ───────────────────────────────────────────────────

async fn connect_direct(target: &Target) -> io::Result<TcpStream> {
    match target {
        Target::Ipv4(ip, port) => TcpStream::connect(SocketAddr::from((*ip, *port))).await,
        Target::Ipv6(ip, port) => TcpStream::connect(SocketAddr::from((*ip, *port))).await,
        Target::Domain(name, port) => TcpStream::connect((name.as_str(), *port)).await,
    }
}

// ── Upstream SOCKS5 handshake (ocproxy) ─────────────────────────────────

async fn connect_upstream(target: &Target, upstream_port: u16) -> io::Result<(TcpStream, Vec<u8>)> {
    let upstream_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, upstream_port));
    let mut upstream = TcpStream::connect(upstream_addr).await.map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("failed to connect to upstream proxy at {upstream_addr}: {e}"),
        )
    })?;

    // Greeting.
    upstream
        .write_all(&[SOCKS_VERSION, 0x01, AUTH_NONE])
        .await?;
    let mut greeting_reply = [0u8; 2];
    upstream.read_exact(&mut greeting_reply).await?;
    if greeting_reply[0] != SOCKS_VERSION || greeting_reply[1] != AUTH_NONE {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!(
                "upstream SOCKS5 greeting failed: got {:02x} {:02x}",
                greeting_reply[0], greeting_reply[1]
            ),
        ));
    }

    // CONNECT request with ATYP 0x03 (domain name) so ocproxy resolves DNS.
    let (domain, port) = match target {
        Target::Domain(name, port) => (name.as_str(), *port),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "upstream routing requires a domain target",
            ));
        }
    };

    let domain_bytes = domain.as_bytes();
    let domain_len = domain_bytes.len();
    if domain_len > 255 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "domain name exceeds 255 bytes",
        ));
    }

    let mut req = Vec::with_capacity(4 + 1 + domain_len + 2);
    req.extend_from_slice(&[SOCKS_VERSION, CMD_CONNECT, 0x00, ATYP_DOMAIN]);
    req.push(domain_len as u8);
    req.extend_from_slice(domain_bytes);
    req.extend_from_slice(&port.to_be_bytes());
    upstream.write_all(&req).await?;

    // Read reply header.
    let mut reply_header = [0u8; 4];
    upstream.read_exact(&mut reply_header).await?;

    let reply_atyp = reply_header[3];
    let mut reply = Vec::from(&reply_header[..]);

    match reply_atyp {
        ATYP_IPV4 => {
            let mut tail = [0u8; 6];
            upstream.read_exact(&mut tail).await?;
            reply.extend_from_slice(&tail);
        }
        ATYP_IPV6 => {
            let mut tail = [0u8; 18];
            upstream.read_exact(&mut tail).await?;
            reply.extend_from_slice(&tail);
        }
        ATYP_DOMAIN => {
            let name_len = upstream.read_u8().await? as usize;
            reply.push(name_len as u8);
            let mut tail = vec![0u8; name_len + 2];
            upstream.read_exact(&mut tail).await?;
            reply.extend_from_slice(&tail);
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("upstream returned unsupported ATYP: {reply_atyp:#04x}"),
            ));
        }
    }

    let rep = reply_header[1];
    if rep != REP_SUCCESS {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("upstream SOCKS5 CONNECT failed with REP={rep:#04x}"),
        ));
    }

    Ok((upstream, reply))
}

// ── Per-connection handler ──────────────────────────────────────────────

async fn handle_client(mut client: TcpStream, peer: SocketAddr, upstream_port: u16) {
    if let Err(e) = handle_client_inner(&mut client, peer, upstream_port).await {
        debug!(peer = %peer, error = %e, "connection closed");
    }
}

async fn handle_client_inner(
    client: &mut TcpStream,
    peer: SocketAddr,
    upstream_port: u16,
) -> io::Result<()> {
    client_greeting(client).await?;
    let target = client_request(client).await?;
    let route = route_for(&target);
    info!(peer = %peer, target = %target, route = ?route, "CONNECT");

    match route {
        Route::Direct => match connect_direct(&target).await {
            Ok(outbound) => {
                let bind_addr = outbound
                    .local_addr()
                    .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 0)));
                client
                    .write_all(&socks5_reply_with_bind(REP_SUCCESS, bind_addr))
                    .await?;
                relay(client, outbound).await;
                Ok(())
            }
            Err(e) => {
                let rep = rep_for_io_error(&e);
                warn!(peer = %peer, target = %target, error = %e, "direct connect failed");
                Err(send_error(client, rep, &e.to_string()).await)
            }
        },
        Route::Upstream => match connect_upstream(&target, upstream_port).await {
            Ok((outbound, reply)) => {
                client.write_all(&reply).await?;
                relay(client, outbound).await;
                Ok(())
            }
            Err(e) => {
                let rep = rep_for_io_error(&e);
                warn!(peer = %peer, target = %target, error = %e, "upstream connect failed");
                Err(send_error(client, rep, &e.to_string()).await)
            }
        },
    }
}

async fn relay(client: &mut TcpStream, mut target: TcpStream) {
    let (mut cr, mut cw) = client.split();
    let (mut tr, mut tw) = target.split();

    let c2t = tokio::io::copy(&mut cr, &mut tw);
    let t2c = tokio::io::copy(&mut tr, &mut cw);

    tokio::select! {
        r = c2t => if let Err(e) = r { debug!(error = %e, "client→target relay ended"); },
        r = t2c => if let Err(e) = r { debug!(error = %e, "target→client relay ended"); },
    }
}

// ── Public API ──────────────────────────────────────────────────────────

/// Run the routing SOCKS5 proxy until `shutdown` is cancelled.
///
/// `bind` and `port` control the listener; `upstream_port` is the loopback
/// ocproxy SOCKS5 port that VPN-bound traffic is forwarded to.
pub async fn run(
    bind: &str,
    port: u16,
    upstream_port: u16,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let bind_addr: SocketAddr = format!("{bind}:{port}")
        .parse()
        .map_err(|e| anyhow::anyhow!("routing-proxy: invalid bind address {bind}:{port}: {e}"))?;
    let listener = TcpListener::bind(bind_addr).await?;

    info!(
        bind = %bind_addr,
        upstream_port,
        proxy_domains = ?proxy_domains(),
        direct_domains = ?direct_domains(),
        "routing proxy listening"
    );

    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                info!("routing proxy: shutdown requested");
                break;
            }
            result = listener.accept() => {
                match result {
                    Ok((stream, peer)) => {
                        debug!(peer = %peer, "accepted connection");
                        tokio::spawn(handle_client(stream, peer, upstream_port));
                    }
                    Err(e) => {
                        warn!(error = %e, "failed to accept connection");
                    }
                }
            }
        }
    }

    info!("routing proxy: stopped accepting new connections");
    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_domain_matches_exact() {
        assert!(domain_matches("example.com", "example.com"));
        assert!(domain_matches("EXAMPLE.COM", "example.com"));
        assert!(domain_matches("example.com", "EXAMPLE.COM"));
    }

    #[test]
    fn test_domain_matches_subdomain() {
        assert!(domain_matches("foo.example.com", "example.com"));
        assert!(domain_matches("a.b.example.com", "example.com"));
        assert!(domain_matches("FOO.EXAMPLE.COM", "example.com"));
    }

    #[test]
    fn test_domain_no_match() {
        assert!(!domain_matches("notexample.com", "example.com"));
        assert!(!domain_matches("xexample.com", "example.com"));
        assert!(!domain_matches("other.com", "example.com"));
    }

    #[test]
    fn test_route_ipv4_always_direct() {
        let target = Target::Ipv4(Ipv4Addr::new(10, 0, 0, 1), 22);
        assert_eq!(route_for(&target), Route::Direct);
    }

    #[test]
    fn test_route_ipv6_always_direct() {
        let target = Target::Ipv6(Ipv6Addr::LOCALHOST, 22);
        assert_eq!(route_for(&target), Route::Direct);
    }

    #[test]
    fn test_route_unknown_domain_direct() {
        let target = Target::Domain("google.com".to_string(), 443);
        assert_eq!(route_for(&target), Route::Direct);
    }

    #[test]
    fn test_socks5_reply_structure() {
        let reply = socks5_reply(REP_SUCCESS);
        assert_eq!(reply[0], SOCKS_VERSION);
        assert_eq!(reply[1], REP_SUCCESS);
        assert_eq!(reply[2], 0x00);
        assert_eq!(reply[3], ATYP_IPV4);
        assert_eq!(reply.len(), 10);
    }

    #[test]
    fn test_socks5_reply_with_bind_v4() {
        let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let reply = socks5_reply_with_bind(REP_SUCCESS, addr);
        assert_eq!(reply[0], SOCKS_VERSION);
        assert_eq!(reply[1], REP_SUCCESS);
        assert_eq!(reply[3], ATYP_IPV4);
        assert_eq!(&reply[4..8], &[127, 0, 0, 1]);
        assert_eq!(&reply[8..10], &8080u16.to_be_bytes());
    }

    #[test]
    fn test_target_display() {
        let t = Target::Domain("foo.example.com".to_string(), 443);
        assert_eq!(t.to_string(), "foo.example.com:443");

        let t = Target::Ipv4(Ipv4Addr::new(10, 0, 0, 1), 22);
        assert_eq!(t.to_string(), "10.0.0.1:22");

        let t = Target::Ipv6(Ipv6Addr::LOCALHOST, 80);
        assert_eq!(t.to_string(), "[::1]:80");
    }
}
