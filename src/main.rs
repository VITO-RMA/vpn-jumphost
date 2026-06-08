//! VPN jumphost — Rust supervisor + tooling.
//!
//! See [`crate::jumphost`] for the core orchestration logic. The CLI is
//! defined here.

mod config;
mod config_file;
mod cookie;
mod jumphost;
mod logging;
mod pac;
mod routing;
mod sleepwake;
mod vpn;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use tokio::signal::unix::{signal, SignalKind};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::config::{
    config_file_check_interval, config_file_chromium_path, config_file_no_headless,
    default_browser_profile_dir, default_cookie_file, env_u32, DEFAULT_CHECK_INTERVAL_SECS,
};
use crate::cookie::{CookieStatus, FetchOptions};
use crate::jumphost::{Supervisor, SupervisorOptions};

/// VPN jumphost: openconnect + ocproxy supervisor, routing SOCKS5 proxy,
/// PAC server, and F5 cookie management — all in one binary.
#[derive(Parser, Debug)]
#[command(name = "jumphost", version, about)]
struct Cli {
    /// Verbose (debug-level) logging.
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Path to a TOML config file. Overrides the default
    /// `$XDG_CONFIG_HOME/vpn-jumphost/config.toml`.
    #[arg(short = 'c', long = "config", global = true, value_name = "FILE")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,

    // ── `run` arguments at the top level for backwards-compatible UX ──
    // (`jumphost --serve-pac …` works just like the old
    // `python3 scripts/jumphost.py --serve-pac`.)
    #[command(flatten)]
    run_args: RunArgs,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the supervisor: openconnect + ocproxy + routing proxy +
    /// (optional) PAC server + periodic cookie check. (Default
    /// subcommand if none is given.)
    Run(RunArgs),
    /// Fetch the F5 MRHSession cookie via a browser login window and
    /// write it to the cookie file.
    FetchCookie(FetchArgs),
    /// Validate the current cookie file against the VPN endpoint.
    /// Exit codes: 0 = valid, 1 = invalid, 2 = network error.
    ValidateCookie(ValidateArgs),
    /// Print the generated PAC file to stdout (or to a file).
    GeneratePac(GenerateArgs),
    /// Run only the loopback PAC HTTP server (no VPN, no routing proxy).
    /// Useful for serving the PAC URL even when the VPN is down — set
    /// this as a separate systemd user unit if you want the URL to
    /// survive jumphost restarts.
    ServePac(ServePacArgs),
}

#[derive(Args, Debug, Default, Clone)]
struct RunArgs {
    /// Also start the loopback PAC HTTP server.
    #[arg(long)]
    serve_pac: bool,

    /// Seconds between periodic cookie validity checks.
    #[arg(long, env = "JUMPHOST_CHECK_INTERVAL", value_name = "SECONDS")]
    check_interval: Option<f64>,

    /// Path to the F5 MRHSession cookie file. Defaults to
    /// `$VPN_COOKIE_FILE` or `$XDG_STATE_HOME/vpn-jumphost/cookie`.
    #[arg(long, value_name = "PATH")]
    cookie_file: Option<PathBuf>,

    /// Disable headless cookie refresh. By default the supervisor uses
    /// headless mode when `VPN_USERNAME` + `VPN_PASSWORD` are set. This
    /// flag forces it to always open a visible browser window instead.
    /// Use as an escape hatch if the headless flow is unstable.
    #[arg(long, env = "JUMPHOST_NO_HEADLESS")]
    no_headless: bool,
}

#[derive(Args, Debug, Clone)]
struct FetchArgs {
    /// File to write the cookie to (mode 600). Defaults to the same path
    /// used by `run`.
    #[arg(short, long, value_name = "FILE")]
    output: Option<PathBuf>,

    /// Persistent Chromium user-data-dir for SSO/session reuse.
    #[arg(long, value_name = "DIR")]
    profile_dir: Option<PathBuf>,

    /// Path to a Chromium executable. Defaults to `$CHROMIUM_PATH`,
    /// then `$CHROME`, then chromiumoxide's auto-detection.
    #[arg(long, value_name = "PATH")]
    chromium: Option<PathBuf>,

    /// Maximum seconds to wait for the user to complete SSO + MFA.
    #[arg(long, default_value_t = 300, value_name = "SECONDS")]
    max_wait: u64,

    /// Launch the browser in headless mode (no visible window). If an
    /// MFA/2FA prompt is detected, the browser is automatically
    /// relaunched with a visible window for user interaction.
    #[arg(long, env = "JUMPHOST_HEADLESS")]
    headless: bool,
}

#[derive(Args, Debug, Clone)]
struct ValidateArgs {
    /// Path to the cookie file. Defaults to the same path used by `run`.
    #[arg(value_name = "PATH")]
    cookie_file: Option<PathBuf>,
}

#[derive(Args, Debug, Clone)]
struct ServePacArgs {
    /// Bind address.
    #[arg(long, env = "PAC_SERVE_BIND", default_value = "127.0.0.1")]
    bind: String,

    /// Listen port.
    #[arg(long, env = "PAC_SERVE_PORT", default_value_t = 8091)]
    port: u16,
}

#[derive(Args, Debug, Clone)]
struct GenerateArgs {
    /// Output path. When omitted, the PAC is written to stdout.
    #[arg(value_name = "PATH")]
    output: Option<PathBuf>,
}

fn main() -> ExitCode {
    // Load .env before CLI parsing so that env-backed clap defaults and
    // config::vpn_credentials() see the values regardless of whether the
    // binary is invoked via `just` (which has its own dotenv-load) or
    // directly.
    let _ = dotenvy::dotenv();

    let cli = Cli::parse();

    // Wire the explicit config path before anything reads the config.
    if let Some(ref path) = cli.config {
        config_file::set_path(path.clone());
    }

    logging::init(cli.verbose);

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("jumphost: could not start tokio runtime: {e}");
            return ExitCode::from(2);
        }
    };

    let code = rt.block_on(async move {
        match cli.command.unwrap_or(Command::Run(cli.run_args)) {
            Command::Run(args) => cmd_run(args).await,
            Command::FetchCookie(args) => cmd_fetch(args).await,
            Command::ValidateCookie(args) => cmd_validate(args).await,
            Command::GeneratePac(args) => cmd_generate(args).await,
            Command::ServePac(args) => cmd_serve_pac(args).await,
        }
    });

    code
}

// ── Subcommands ───────────────────────────────────────────────────────────

async fn cmd_run(args: RunArgs) -> ExitCode {
    let cookie_file = args.cookie_file.unwrap_or_else(default_cookie_file);
    if let Some(parent) = cookie_file.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            warn!(path = %parent.display(), error = %e, "could not create cookie parent dir");
        }
    }

    let check_interval_s = args.check_interval.unwrap_or_else(|| {
        // CLI flag not set — try env var, then config file, then default.
        let from_env = env_u32(
            "JUMPHOST_CHECK_INTERVAL",
            DEFAULT_CHECK_INTERVAL_SECS as u32,
        ) as f64;
        // env_u32 already consults the config file, so `from_env` already
        // reflects the config file value if no env var is set. But the
        // config file stores this as an f64, so prefer the precise value
        // when the env var is absent.
        if std::env::var("JUMPHOST_CHECK_INTERVAL").is_err() {
            config_file_check_interval().unwrap_or(from_env)
        } else {
            from_env
        }
    });
    let check_interval = Duration::from_secs_f64(check_interval_s.max(1.0));

    // Resolve no_headless: CLI flag > env var > config file.
    let no_headless = if args.no_headless {
        true
    } else if std::env::var("JUMPHOST_NO_HEADLESS").is_ok() {
        // clap already parsed the env var into args.no_headless, but it
        // only sees truthy string values. Re-check.
        args.no_headless
    } else {
        config_file_no_headless().unwrap_or(false)
    };

    let options = SupervisorOptions {
        cookie_file,
        serve_pac: args.serve_pac,
        check_interval,
        no_headless,
    };

    info!(
        cookie = %options.cookie_file.display(),
        check_interval_s = options.check_interval.as_secs_f64(),
        serve_pac = options.serve_pac,
        "VPN jumphost starting"
    );

    let supervisor = Arc::new(Supervisor::new(options));
    let stop = CancellationToken::new();
    install_signal_handlers(stop.clone());

    match supervisor.clone().run(stop).await {
        Ok(()) => {
            info!("jumphost: stopped cleanly");
            ExitCode::SUCCESS
        }
        Err(e) => {
            error!(error = %e, "jumphost: fatal error");
            ExitCode::FAILURE
        }
    }
}

async fn cmd_fetch(args: FetchArgs) -> ExitCode {
    let output = args.output.unwrap_or_else(default_cookie_file);
    let profile = args.profile_dir.unwrap_or_else(default_browser_profile_dir);

    let stop = CancellationToken::new();
    install_signal_handlers(stop.clone());

    let opts = FetchOptions {
        output: Some(output.clone()),
        profile_dir: Some(profile),
        max_wait: Duration::from_secs(args.max_wait),
        chromium_path: args.chromium.or_else(|| {
            std::env::var("CHROMIUM_PATH")
                .ok()
                .or_else(|| std::env::var("CHROME").ok())
                .map(PathBuf::from)
                .or_else(config_file_chromium_path)
        }),
        headless: args.headless,
        stop,
    };

    match cookie::fetch(opts).await {
        Ok(_) => {
            info!(path = %output.display(), "cookie saved");
            ExitCode::SUCCESS
        }
        Err(e) => {
            error!(error = %e, "fetch failed");
            ExitCode::FAILURE
        }
    }
}

async fn cmd_validate(args: ValidateArgs) -> ExitCode {
    let cookie_file = args.cookie_file.unwrap_or_else(default_cookie_file);
    match cookie::validate_file(&cookie_file).await {
        CookieStatus::Valid => {
            info!(path = %cookie_file.display(), "cookie is valid");
            ExitCode::SUCCESS
        }
        CookieStatus::Invalid => {
            warn!(path = %cookie_file.display(), "cookie is invalid or expired");
            ExitCode::from(1)
        }
        CookieStatus::NetworkError => {
            warn!(path = %cookie_file.display(), "cookie validity unknown (network error)");
            ExitCode::from(2)
        }
    }
}

async fn cmd_generate(args: GenerateArgs) -> ExitCode {
    let body = pac::generate();
    match args.output {
        Some(path) => match std::fs::write(&path, &body) {
            Ok(()) => {
                info!(path = %path.display(), "PAC written");
                ExitCode::SUCCESS
            }
            Err(e) => {
                error!(error = %e, path = %path.display(), "failed to write PAC");
                ExitCode::FAILURE
            }
        },
        None => {
            // Write to stdout.
            use std::io::Write;
            let stdout = std::io::stdout();
            let mut h = stdout.lock();
            if let Err(e) = h.write_all(body.as_bytes()) {
                error!(error = %e, "failed to write PAC to stdout");
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
    }
}

async fn cmd_serve_pac(args: ServePacArgs) -> ExitCode {
    let stop = CancellationToken::new();
    install_signal_handlers(stop.clone());
    info!(bind = %args.bind, port = args.port, "starting PAC-only HTTP server");
    match pac::serve(&args.bind, args.port, stop).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!(error = %e, "PAC server exited with error");
            ExitCode::FAILURE
        }
    }
}

// ── Signal handling ──────────────────────────────────────────────────────

fn install_signal_handlers(stop: CancellationToken) {
    // SIGTERM / SIGINT / SIGHUP — all trigger a clean shutdown.
    spawn_signal(stop.clone(), SignalKind::terminate(), "SIGTERM");
    spawn_signal(stop.clone(), SignalKind::interrupt(), "SIGINT");
    spawn_signal(stop, SignalKind::hangup(), "SIGHUP");
}

fn spawn_signal(stop: CancellationToken, kind: SignalKind, name: &'static str) {
    tokio::spawn(async move {
        let mut sig = match signal(kind) {
            Ok(s) => s,
            Err(e) => {
                warn!(signal = name, error = %e, "could not install signal handler");
                return;
            }
        };
        if sig.recv().await.is_some() {
            info!(signal = name, "received signal; shutting down");
            stop.cancel();
        }
        // A second signal of the same kind means the graceful shutdown is
        // stuck (e.g. browser cleanup hanging). Force-exit immediately.
        if sig.recv().await.is_some() {
            eprintln!("jumphost: received second {name}; forcing exit");
            std::process::exit(1);
        }
    });
}
