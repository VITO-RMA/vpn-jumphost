use std::ffi::OsString;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{ChildStdout, Command as ProcessCommand, ExitCode, ExitStatus, Stdio};

use clap::{Args, ValueEnum};

use crate::config;

const SYSTEMD_UNIT: &str = "vpn-jumphost.service";
const LAUNCHD_LOG_PATH: &str = "/tmp/vpn-jumphost.log";

#[derive(Args, Debug, Clone)]
pub struct LogsArgs {
    /// Keep streaming new log lines.
    #[arg(short, long)]
    pub(crate) follow: bool,

    /// Number of recent lines to show before following.
    #[arg(short = 'n', long, default_value_t = 100)]
    pub(crate) lines: u32,

    /// Log source to read. Auto prefers a known systemd user service,
    /// then the detached nohup log, then the macOS launchd log.
    #[arg(long, value_enum, default_value_t = LogSource::Auto)]
    source: LogSource,

    /// Show authentication-related log lines.
    #[arg(long)]
    auth: bool,

    /// Show MFA-related log lines, including Authenticator number-match codes.
    #[arg(long)]
    mfa: bool,

    /// Show warning and error log lines.
    #[arg(long)]
    errors: bool,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
enum LogSource {
    Auto,
    Systemd,
    Detached,
    Launchd,
}

pub fn run(args: LogsArgs) -> ExitCode {
    let source = match args.source {
        LogSource::Auto => match select_auto_source() {
            Some(source) => source,
            None => {
                eprintln!("jumphost logs: no log source found");
                eprintln!("  systemd:  journalctl --user -u {SYSTEMD_UNIT} -f");
                eprintln!("  detached: {}", detached_log_path().display());
                eprintln!("  macOS:    {LAUNCHD_LOG_PATH}");
                return ExitCode::FAILURE;
            }
        },
        source => source,
    };

    match source {
        LogSource::Auto => unreachable!("auto source must be resolved before execution"),
        LogSource::Systemd => run_systemd_logs(&args),
        LogSource::Detached => run_file_logs(&args, &detached_log_path()),
        LogSource::Launchd => run_file_logs(&args, Path::new(LAUNCHD_LOG_PATH)),
    }
}

fn select_auto_source() -> Option<LogSource> {
    select_auto_source_from_state(SourceState {
        systemd_service_known: systemd_user_service_known(),
        journalctl_available: command_exists("journalctl"),
        detached_log_exists: detached_log_path().is_file(),
        launchd_log_exists: Path::new(LAUNCHD_LOG_PATH).is_file(),
    })
}

#[derive(Debug, Clone, Copy)]
struct SourceState {
    systemd_service_known: bool,
    journalctl_available: bool,
    detached_log_exists: bool,
    launchd_log_exists: bool,
}

fn select_auto_source_from_state(state: SourceState) -> Option<LogSource> {
    if state.systemd_service_known {
        return Some(LogSource::Systemd);
    }
    if state.detached_log_exists {
        return Some(LogSource::Detached);
    }
    if state.launchd_log_exists {
        return Some(LogSource::Launchd);
    }
    if state.journalctl_available {
        return Some(LogSource::Systemd);
    }
    None
}

fn run_systemd_logs(args: &LogsArgs) -> ExitCode {
    if !command_exists("journalctl") {
        eprintln!("jumphost logs: journalctl not found on PATH");
        return ExitCode::FAILURE;
    }

    let mut command_args = vec![
        OsString::from("--user"),
        OsString::from("-u"),
        OsString::from(SYSTEMD_UNIT),
        OsString::from("-n"),
        OsString::from(args.lines.to_string()),
        OsString::from("--no-pager"),
    ];
    if args.follow {
        command_args.push(OsString::from("-f"));
    }

    run_command("journalctl", command_args, LogFilters::from_args(args))
}

fn run_file_logs(args: &LogsArgs, path: &Path) -> ExitCode {
    if !path.is_file() {
        eprintln!("jumphost logs: log file does not exist: {}", path.display());
        return ExitCode::FAILURE;
    }
    if !command_exists("tail") {
        eprintln!("jumphost logs: tail not found on PATH");
        return ExitCode::FAILURE;
    }

    let mut command_args = vec![OsString::from("-n"), OsString::from(args.lines.to_string())];
    if args.follow {
        command_args.push(OsString::from("-f"));
    }
    command_args.push(path.as_os_str().to_os_string());

    run_command("tail", command_args, LogFilters::from_args(args))
}

fn run_command(program: &str, args: Vec<OsString>, filters: LogFilters) -> ExitCode {
    if !filters.any() {
        return match ProcessCommand::new(program).args(args).status() {
            Ok(status) => status_to_exit_code(status),
            Err(e) => {
                eprintln!("jumphost logs: failed to run {program}: {e}");
                ExitCode::FAILURE
            }
        };
    }

    let child = ProcessCommand::new(program).args(args).stdout(Stdio::piped()).spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(e) => {
            eprintln!("jumphost logs: failed to run {program}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let Some(stdout) = child.stdout.take() else {
        eprintln!("jumphost logs: failed to capture {program} output");
        let _ = child.kill();
        let _ = child.wait();
        return ExitCode::FAILURE;
    };

    if let Err(e) = filter_output(stdout, filters) {
        eprintln!("jumphost logs: failed to filter {program} output: {e}");
        let _ = child.kill();
        let _ = child.wait();
        return ExitCode::FAILURE;
    }

    match child.wait() {
        Ok(status) => status_to_exit_code(status),
        Err(e) => {
            eprintln!("jumphost logs: failed to wait for {program}: {e}");
            ExitCode::FAILURE
        }
    }
}

fn filter_output(stdout: ChildStdout, filters: LogFilters) -> std::io::Result<()> {
    let mut reader = std::io::BufReader::new(stdout);
    let mut out = std::io::stdout().lock();
    let mut line = Vec::new();
    while reader.read_until(b'\n', &mut line)? != 0 {
        let decoded = decode_log_line(&line);
        if filters.matches(&decoded) {
            write!(out, "{decoded}")?;
            if !decoded.ends_with('\n') {
                writeln!(out)?;
            }
            out.flush()?;
        }
        line.clear();
    }
    Ok(())
}

fn decode_log_line(line: &[u8]) -> std::borrow::Cow<'_, str> {
    String::from_utf8_lossy(line)
}

fn status_to_exit_code(status: ExitStatus) -> ExitCode {
    if status.success() {
        return ExitCode::SUCCESS;
    }
    let code = status.code().unwrap_or(1).clamp(1, u8::MAX as i32) as u8;
    ExitCode::from(code)
}

fn detached_log_path() -> PathBuf {
    config::state_dir().join("jumphost.log")
}

fn systemd_user_service_known() -> bool {
    command_exists("systemctl") && (systemd_user_service_active() || systemd_user_service_loaded())
}

fn systemd_user_service_active() -> bool {
    systemctl_status(["--user", "is-active", "--quiet", SYSTEMD_UNIT]).is_ok_and(|status| status.success())
}

fn systemd_user_service_loaded() -> bool {
    match ProcessCommand::new("systemctl")
        .args(["--user", "show", "--property=LoadState", "--value", SYSTEMD_UNIT])
        .output()
    {
        Ok(output) if output.status.success() => {
            let load_state = String::from_utf8_lossy(&output.stdout);
            let load_state = load_state.trim();
            !load_state.is_empty() && load_state != "not-found"
        }
        _ => false,
    }
}

fn systemctl_status<const N: usize>(args: [&str; N]) -> std::io::Result<ExitStatus> {
    ProcessCommand::new("systemctl").args(args).status()
}

fn command_exists(program: &str) -> bool {
    std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join(program))
                .any(|candidate| candidate.is_file())
        })
        .unwrap_or(false)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LogFilters {
    auth: bool,
    mfa: bool,
    errors: bool,
}

impl LogFilters {
    fn from_args(args: &LogsArgs) -> Self {
        Self {
            auth: args.auth,
            mfa: args.mfa,
            errors: args.errors,
        }
    }

    fn any(self) -> bool {
        self.auth || self.mfa || self.errors
    }

    fn matches(self, line: &str) -> bool {
        if !self.any() {
            return true;
        }
        let lower = line.to_ascii_lowercase();
        (self.auth && contains_any(&lower, AUTH_PATTERNS))
            || (self.mfa && contains_any(&lower, MFA_PATTERNS))
            || (self.errors && contains_any(&lower, ERROR_PATTERNS))
    }
}

const AUTH_PATTERNS: &[&str] = &[
    "auth",
    "browser login",
    "credential",
    "cookie",
    "login",
    "mrhsession",
    "sso",
    "token",
];

const MFA_PATTERNS: &[&str] = &["authenticator", "mfa", "number-match", "number="];

const ERROR_PATTERNS: &[&str] = &["error ", "error=", "warn ", "warn=", "failed", "failure", "fatal", "panic"];

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_prefers_known_systemd_service_over_detached_log() {
        let source = select_auto_source_from_state(SourceState {
            systemd_service_known: true,
            journalctl_available: true,
            detached_log_exists: true,
            launchd_log_exists: true,
        });

        assert_eq!(source, Some(LogSource::Systemd));
    }

    #[test]
    fn auto_uses_detached_log_before_launchd_log() {
        let source = select_auto_source_from_state(SourceState {
            systemd_service_known: false,
            journalctl_available: true,
            detached_log_exists: true,
            launchd_log_exists: true,
        });

        assert_eq!(source, Some(LogSource::Detached));
    }

    #[test]
    fn auto_falls_back_to_journalctl_without_known_logs() {
        let source = select_auto_source_from_state(SourceState {
            systemd_service_known: false,
            journalctl_available: true,
            detached_log_exists: false,
            launchd_log_exists: false,
        });

        assert_eq!(source, Some(LogSource::Systemd));
    }

    #[test]
    fn auto_returns_none_without_available_sources() {
        let source = select_auto_source_from_state(SourceState {
            systemd_service_known: false,
            journalctl_available: false,
            detached_log_exists: false,
            launchd_log_exists: false,
        });

        assert_eq!(source, None);
    }

    #[test]
    fn mfa_filter_matches_number_and_authenticator_lines() {
        let filters = LogFilters {
            auth: false,
            mfa: true,
            errors: false,
        };

        assert!(filters.matches("INFO jumphost::cookie: MFA: approve sign-in request with this number number=123"));
        assert!(filters.matches("INFO jumphost::cookie: triggered Authenticator push"));
        assert!(!filters.matches("INFO jumphost::vpn: VPN stopped"));
    }

    #[test]
    fn auth_filter_matches_cookie_and_login_lines() {
        let filters = LogFilters {
            auth: true,
            mfa: false,
            errors: false,
        };

        assert!(filters.matches("INFO jumphost::cookie: captured valid MRHSession cookie"));
        assert!(filters.matches("INFO jumphost::cookie: refreshing VPN cookie via browser login"));
        assert!(!filters.matches("INFO jumphost::routing: routing proxy listening"));
    }

    #[test]
    fn errors_filter_matches_warning_and_error_lines() {
        let filters = LogFilters {
            auth: false,
            mfa: false,
            errors: true,
        };

        assert!(filters.matches("2026-09-01 12:00:00.000  WARN jumphost::vpn: retrying"));
        assert!(filters.matches("2026-09-01 12:00:00.000 ERROR jumphost: fatal error"));
        assert!(filters.matches("cookie refresh failed; will retry"));
        assert!(!filters.matches("INFO jumphost::vpn: VPN stopped"));
    }

    #[test]
    fn combined_filters_match_any_selected_category() {
        let filters = LogFilters {
            auth: true,
            mfa: false,
            errors: true,
        };

        assert!(filters.matches("INFO jumphost::cookie: cookie saved"));
        assert!(filters.matches("ERROR jumphost::vpn: openconnect failed"));
        assert!(!filters.matches("INFO jumphost::pac: PAC server listening"));
    }

    #[test]
    fn log_line_decoding_tolerates_non_utf8_bytes() {
        assert_eq!(decode_log_line(b"INFO ok\n"), "INFO ok\n");
        assert_eq!(decode_log_line(b"WARN bad byte: \xff\n"), "WARN bad byte: \u{fffd}\n");
    }
}
