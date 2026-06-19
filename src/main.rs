mod config;
mod config_file;
mod cookie;
mod credential_store;
mod doctor;
mod jumphost;
mod logging;
mod pac;
mod routing;
mod sleepwake;
mod vpn;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, CommandFactory, Parser, Subcommand};
use clap_complete::{Shell, generate};
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::cookie::{CookieStatus, FetchOptions};
use crate::jumphost::{Supervisor, SupervisorOptions};

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
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Store VPN credentials in the OS keyring and validate vpn login
    Authenticate(AuthenticateArgs),
    /// Remove stored credentials and delete the vpn cookie
    Deauthenticate,
    /// Run the VPN jumphost
    Run(RunArgs),
    /// Validate the current cookie file against the VPN endpoint. Requires authentication
    ValidateCookie,
    /// Generate a PAC file.
    GeneratePac(GeneratePacArgs),
    /// Send a test desktop notification to verify that the notification system is working
    TestNotification,
    /// Print a shell completion script for the given shell
    GenerateCompletions(GenerateCompletionsArgs),
    /// Run health checks for config, cookie, listeners, and proxychains
    Doctor,
}

#[derive(Args, Debug, Default, Clone)]
struct RunArgs {
    /// Disable headless cookie refresh. By default the supervisor uses
    /// headless mode when credentials are configured and shows an MFA
    /// desktop notification. This flag forces it to always open a visible
    /// browser window instead.
    #[arg(long)]
    no_headless: bool,
}

#[derive(Args, Debug, Clone)]
struct AuthenticateArgs {
    /// Read username and password from `VPN_USERNAME` and `VPN_PASSWORD`
    /// instead of prompting interactively, then store them in the OS keyring.
    #[arg(long)]
    from_env: bool,

    /// Disable headless cookie refresh. By default the supervisor uses
    /// headless mode when credentials are configured and shows an MFA
    /// desktop notification. This flag forces it to always open a visible
    /// browser window instead.
    #[arg(long)]
    no_headless: bool,
}

#[derive(Args, Debug, Clone)]
struct GenerateCompletionsArgs {
    /// Shell to generate completions for
    shell: Shell,
}

#[derive(Args, Debug, Clone)]
struct GeneratePacArgs {
    /// Output path. When omitted, the PAC is written to stdout.
    #[arg(value_name = "PATH")]
    output: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> ExitCode {
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

    logging::init(cli.verbose || config_file::get().verbose.unwrap_or(false));

    // On macOS, CLI binaries have no bundle identifier, so
    // NSUserNotificationCenter silently drops notifications.  Register
    // one so MFA desktop notifications actually appear.
    #[cfg(target_os = "macos")]
    {
        if let Err(e) = notify_rust::set_application("sas.vpn-jumphost") {
            warn!(error = %e, "could not set macOS notification bundle id");
        }
    }

    match cli.command {
        Command::Run(args) => cmd_run(args).await,
        Command::ValidateCookie => cmd_validate().await,
        Command::GeneratePac(args) => cmd_generate_pac(args).await,
        Command::Authenticate(args) => cmd_authenticate(args).await,
        Command::Deauthenticate => cmd_deauthenticate().await,
        Command::TestNotification => cmd_test_notification().await,
        Command::GenerateCompletions(args) => cmd_generate_completions(args),
        Command::Doctor => doctor::run().await,
    }
}

// ── Subcommands ───────────────────────────────────────────────────────────

fn cmd_generate_completions(args: GenerateCompletionsArgs) -> ExitCode {
    let mut cmd = Cli::command();
    generate(args.shell, &mut cmd, "jumphost", &mut std::io::stdout());
    ExitCode::SUCCESS
}

async fn cmd_run(args: RunArgs) -> ExitCode {
    let options = SupervisorOptions {
        no_headless: args.no_headless || config::no_headless(),
    };

    info!(no_headless = options.no_headless, "VPN jumphost starting");

    let supervisor = Supervisor::new(options);
    let stop = CancellationToken::new();
    install_signal_handlers(stop.clone());

    match supervisor.run(stop).await {
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

async fn cmd_validate() -> ExitCode {
    let cookie_file = config::cookie_file_path();
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

async fn cmd_generate_pac(args: GeneratePacArgs) -> ExitCode {
    let body = pac::generate();
    match args.output {
        Some(path) => match std::fs::write(&path, &body) {
            Ok(()) => {
                info!(path = %path.display(), "PAC written to {}", path.display());
                ExitCode::SUCCESS
            }
            Err(e) => {
                error!(error = %e, path = %path.display(), "failed to write PAC");
                ExitCode::FAILURE
            }
        },
        None => {
            print!("{body}");
            ExitCode::SUCCESS
        }
    }
}

async fn cmd_test_notification() -> ExitCode {
    eprintln!("sending test notification...");
    match tokio::task::spawn_blocking(|| {
        let mut n = notify_rust::Notification::new();
        n.summary("VPN Jumphost")
            .body("Test notification — if you see this, notifications are working!")
            .appname("jumphost");
        #[cfg(target_os = "linux")]
        {
            n.icon("dialog-information").urgency(notify_rust::Urgency::Normal);
        }
        n.show()
    })
    .await
    {
        Ok(Ok(_)) => {
            eprintln!("notification sent successfully");
            ExitCode::SUCCESS
        }
        Ok(Err(e)) => {
            error!(error = %e, "failed to send notification");
            ExitCode::FAILURE
        }
        Err(e) => {
            error!(error = %e, "notification task panicked");
            ExitCode::FAILURE
        }
    }
}

// ── Signal handling ──────────────────────────────────────────────────────

async fn cmd_authenticate(args: AuthenticateArgs) -> ExitCode {
    let (username, password) = if args.from_env {
        match config::env_vpn_credentials() {
            Some(creds) => (creds.username, creds.password),
            None => {
                error!("--from-env requires non-empty VPN_USERNAME and VPN_PASSWORD");
                return ExitCode::FAILURE;
            }
        }
    } else {
        eprint!("VPN username: ");
        let mut username = String::new();
        if let Err(e) = std::io::stdin().read_line(&mut username) {
            error!(error = %e, "failed to read username");
            return ExitCode::FAILURE;
        }
        let username = username.trim().to_string();
        if username.is_empty() {
            error!("username must not be empty");
            return ExitCode::FAILURE;
        }

        let password = match rpassword::prompt_password("VPN password: ") {
            Ok(p) => p,
            Err(e) => {
                error!(error = %e, "failed to read password");
                return ExitCode::FAILURE;
            }
        };
        if password.is_empty() {
            error!("password must not be empty");
            return ExitCode::FAILURE;
        }
        (username, password)
    };

    if let Err(e) = credential_store::store_credentials(&username, &password) {
        error!(error = %e, "failed to store credentials");
        return ExitCode::FAILURE;
    }
    eprintln!("credentials stored in OS keyring");

    let stop = CancellationToken::new();
    install_signal_handlers(stop.clone());

    let opts = FetchOptions {
        headless: !args.no_headless,
        stop,
        ..FetchOptions::default()
    };

    match cookie::fetch(opts).await {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            error!(error = %e, "cookie fetch failed");
            ExitCode::FAILURE
        }
    }
}

async fn cmd_deauthenticate() -> ExitCode {
    let mut ok = true;

    match credential_store::delete_credentials() {
        Ok(()) => eprintln!("credentials deleted from OS keyring"),
        Err(e) => {
            error!(error = %e, "failed to delete credentials");
            ok = false;
        }
    }

    let cookie_file = config::cookie_file_path();
    match std::fs::remove_file(&cookie_file) {
        Ok(()) => eprintln!("cookie file deleted"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            error!(error = %e, path = %cookie_file.display(), "failed to delete cookie file");
            ok = false;
        }
    }

    let profile_dir = config::default_browser_profile_dir();
    match std::fs::remove_dir_all(&profile_dir) {
        Ok(()) => eprintln!("browser profile directory deleted"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            error!(error = %e, path = %profile_dir.display(), "failed to delete browser profile directory");
            ok = false;
        }
    }

    if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

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
