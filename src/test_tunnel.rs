//! `jumphost test-tunnel` — end-to-end SOCKS5 probes through the routing proxy.

use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::Duration;

use clap::Args;
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::config::{self, DEFAULT_ROUTING_PROXY_BIND, DEFAULT_ROUTING_PROXY_PORT};
use crate::probe::{self, ProbeTarget};

#[derive(Args, Debug, Clone)]
pub struct TestTunnelArgs {
    /// Probe target as `host` or `host:port` (repeatable). Port defaults to 443.
    #[arg(short = 'H', long = "host", value_name = "HOST[:PORT]")]
    hosts: Vec<String>,

    /// Per-probe connect timeout in seconds.
    #[arg(long, default_value_t = 0)]
    timeout: u64,

    /// Retry failed probes this many additional times.
    #[arg(long, default_value_t = 0)]
    retries: u32,

    /// Pass when at least one probe succeeds (default: all must succeed).
    #[arg(long, conflicts_with = "require_all")]
    require_any: bool,

    /// Pass only when every probe succeeds (default).
    #[arg(long, conflicts_with = "require_any")]
    require_all: bool,

    /// Only print failures and the summary line.
    #[arg(short, long)]
    quiet: bool,
}

pub async fn run(args: TestTunnelArgs) -> ExitCode {
    let proxy_bind = config::cfg_string("ROUTING_PROXY_BIND", DEFAULT_ROUTING_PROXY_BIND);
    let proxy_port = config::cfg_u16("ROUTING_PROXY_PORT", DEFAULT_ROUTING_PROXY_PORT);
    let proxy_addr: SocketAddr = match format!("{proxy_bind}:{proxy_port}").parse() {
        Ok(addr) => addr,
        Err(e) => {
            eprintln!("test-tunnel: invalid routing proxy address {proxy_bind}:{proxy_port}: {e}");
            return ExitCode::from(2);
        }
    };

    if !routing_proxy_up(proxy_addr).await {
        eprintln!(
            "test-tunnel: routing proxy not listening on {proxy_addr} — start `jumphost run` first"
        );
        return ExitCode::from(2);
    }

    let connect_timeout = if args.timeout > 0 {
        Duration::from_secs(args.timeout)
    } else {
        config::probe_timeout()
    };
    let retries = if args.retries > 0 {
        args.retries
    } else {
        config::probe_retries()
    };

    let targets = resolve_targets(&args.hosts);
    if targets.is_empty() {
        eprintln!(
            "test-tunnel: no probe targets — set [probe].hosts in config.toml or pass -H host[:port]"
        );
        return ExitCode::FAILURE;
    }

    let results = probe::run_probes(proxy_addr, &targets, connect_timeout, retries).await;

    let require_all = !args.require_any;
    let passed = results.iter().filter(|r| r.ok).count();
    let total = results.len();
    let success = if require_all {
        passed == total
    } else {
        passed > 0
    };

    if !args.quiet {
        eprintln!("jumphost test-tunnel\n");
        for result in &results {
            let tag = if result.ok { "ok  " } else { "FAIL" };
            let route = config::route_label_for_host(&result.target.host);
            if result.ok {
                eprintln!(
                    "  [{tag}]  {:<40} {route:<7} {}ms",
                    result.target.display(),
                    result.latency.as_millis()
                );
            } else {
                let detail = result.error.as_deref().unwrap_or("unknown error");
                eprintln!(
                    "  [{tag}]  {:<40} {route:<7} {detail}",
                    result.target.display(),
                );
            }
        }
        eprintln!();
    }

    if success {
        if args.quiet {
            eprintln!("test-tunnel: ok ({passed}/{total})");
        } else {
            eprintln!("test-tunnel: {passed}/{total} probes passed");
        }
        ExitCode::SUCCESS
    } else {
        if args.quiet {
            eprintln!("test-tunnel: FAIL ({passed}/{total})");
        } else {
            eprintln!("test-tunnel: {passed}/{total} probes passed");
        }
        ExitCode::FAILURE
    }
}

fn resolve_targets(cli_hosts: &[String]) -> Vec<ProbeTarget> {
    if !cli_hosts.is_empty() {
        return cli_hosts
            .iter()
            .filter_map(|h| probe::parse_probe_target(h, config::DEFAULT_PROBE_PORT))
            .collect();
    }
    probe::probe_targets()
}

async fn routing_proxy_up(addr: SocketAddr) -> bool {
    timeout(Duration::from_secs(2), TcpStream::connect(addr))
        .await
        .is_ok_and(|r| r.is_ok())
}
