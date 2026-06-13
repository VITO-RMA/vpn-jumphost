//! PAC (Proxy Auto-Configuration) generation and serving.
//!
//! - [`generate`] produces the PAC JavaScript text from the routing
//!   constants in [`crate::config`]. The proxy chain is always
//!   `SOCKS5 <routing_proxy.bind>:<routing_proxy.port>; DIRECT`.
//! - [`serve`] runs a tiny HTTP/1.1 server on `bind:port` that serves the
//!   generated PAC on every path with `Content-Type: application/x-ns-proxy-autoconfig`.
//!   Replaces `miniserve`; the body is regenerated from the current
//!   configuration on every request, so changes to `routing_proxy.bind` /
//!   `routing_proxy.port` in the config file are picked up without
//!   restarting the supervisor.

use std::net::SocketAddr;

use axum::{Router, http::StatusCode, response::IntoResponse, routing::get};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::config;

/// PAC content-type that browsers and most proxy clients accept.
const PAC_CONTENT_TYPE: &str = "application/x-ns-proxy-autoconfig";

/// Build the PAC JS for one domain match.
fn domain_condition(domain: &str) -> String {
    if let Some(stripped) = domain.strip_prefix("*.") {
        format!("shExpMatch(host, \"*.{stripped}\")")
    } else {
        format!("host === \"{domain}\" || dnsDomainIs(host, \".{domain}\")")
    }
}

fn build_condition_block(conditions: &[String]) -> String {
    if conditions.is_empty() {
        return "    false".to_string();
    }
    conditions
        .iter()
        .map(|c| format!("    {c}"))
        .collect::<Vec<_>>()
        .join(" ||\n")
}

fn build_direct_rules(domains: &[&str]) -> String {
    let mut blocks = Vec::with_capacity(domains.len());
    for d in domains {
        blocks.push(format!(
            "  if ({cond}) {{\n    return \"DIRECT\";\n  }}",
            cond = domain_condition(d),
        ));
    }
    blocks.join("\n")
}

/// Generate the PAC file text using the current configuration.
///
/// The proxy chain is always `SOCKS5 <routing_proxy.bind>:<routing_proxy.port>; DIRECT`,
/// using the same bind/port settings as the routing proxy itself.
pub fn generate() -> String {
    let proxy_host = config::cfg_string("ROUTING_PROXY_BIND", config::DEFAULT_ROUTING_PROXY_BIND);
    let socks_port = config::cfg_u16("ROUTING_PROXY_PORT", config::DEFAULT_ROUTING_PROXY_PORT);
    let proxy_chain = format!("SOCKS5 {proxy_host}:{socks_port}; DIRECT");

    let direct = config::direct_domains();
    let proxy = config::proxy_domains();

    let direct_rules = build_direct_rules(&direct.iter().map(|s| s.as_str()).collect::<Vec<_>>());
    let conditions: Vec<String> = proxy.iter().map(|d| domain_condition(d)).collect();
    let match_block = build_condition_block(&conditions);
    let direct_list = direct.join(", ");

    format!(
        "function FindProxyForURL(url, host) {{\n  \
         var proxy_chain = \"{proxy_chain}\";\n\n  \
         // Always-DIRECT domains (e.g. VPN portal): checked before dnsResolve so a\n  \
         // broken VPN DNS cannot force these through the proxy.\n  \
         // Always-DIRECT: {direct_list}\n\
{direct_rules}\n\n  \
         if (\n\
{match_block}\n  \
         ) {{\n    return proxy_chain;\n  }}\n\n  \
         return \"DIRECT\";\n\
         }}\n",
    )
}

// ── HTTP server ────────────────────────────────────────────────────────

/// Handler for GET (and HEAD — axum strips the body automatically for HEAD).
/// Returns 405 automatically for any other method.
async fn pac_handler() -> impl IntoResponse {
    let body = generate();
    (
        StatusCode::OK,
        [
            ("Content-Type", PAC_CONTENT_TYPE),
            ("Cache-Control", "no-store"),
            // Hint the filename when the PAC URL is opened directly in a browser.
            ("Content-Disposition", "inline; filename=\"proxy.pac\""),
        ],
        body,
    )
}

/// Run the PAC HTTP server until `shutdown` is cancelled.
pub async fn serve(bind: &str, port: u16, shutdown: CancellationToken) -> anyhow::Result<()> {
    let addr: SocketAddr = format!("{bind}:{port}")
        .parse()
        .map_err(|e| anyhow::anyhow!("pac: invalid bind address {bind}:{port}: {e}"))?;
    let listener = TcpListener::bind(addr).await?;
    info!(bind = %addr, "PAC HTTP server listening");

    let app = Router::new().fallback(get(pac_handler));

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown.cancelled().await;
            info!("PAC server: shutdown requested");
        })
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pac_default_chain_format() {
        let pac = generate();
        assert!(pac.contains("SOCKS5 127.0.0.1:1081; DIRECT"));
    }

    #[test]
    fn pac_mentions_each_proxy_domain() {
        let pac = generate();
        for d in config::proxy_domains() {
            assert!(pac.contains(d.as_str()), "PAC should reference {d}");
        }
    }

    #[test]
    fn pac_direct_domains_no_url_indexof() {
        let pac = generate();
        // Direct-domain rules must use host-based checks only; a url.indexOf
        // check is redundant and can produce false positives when the direct
        // domain appears in the path or query of a proxied URL.
        assert!(
            !pac.contains("url.indexOf"),
            "PAC should not contain url.indexOf checks"
        );
    }
}
