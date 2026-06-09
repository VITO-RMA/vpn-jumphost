//! Main supervisor: orchestrates the VPN, routing proxy, PAC server, and
//! periodic cookie management. This is the Rust port of the Python
//! `JumphostSupervisor` + `monitor_loop` from the old `scripts/jumphost.py`.
//!
//! - Validates the cookie on startup; refreshes it via the browser flow
//!   when expired/missing. Network errors at startup do not abort.
//! - Spawns `openconnect`; ocproxy is spawned by openconnect itself via
//!   `--script-tun --script "ocproxy …"`.
//! - Always starts the in-process routing SOCKS5 proxy on port 1081
//!   (default), forwarding VPN-domain traffic to ocproxy on port 1080.
//! - When `serve_pac` is true: starts the in-process PAC HTTP server.
//! - Monitor loop polls every `min(15s, check_interval/4)`:
//!     * Restarts VPN if it died.
//!     * At least every `check_interval`, re-validates the cookie.
//!     * On a forced check after resume, uses
//!       [`validate_cookie_with_retry`] with up to 60 s of exponential
//!       backoff (network often isn't ready right after wake).
//!     * On a forced check, restarts the VPN unconditionally even if the
//!       cookie is still valid — the F5 TCP/TLS session is usually dead
//!       after suspend anyway.
//!     * On periodic (non-forced) NetworkError, does NOT advance
//!       `last_check`, so the next short poll retries within
//!       `poll_interval` rather than waiting another full `check_interval`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::config::{
    self, DEFAULT_PAC_BIND, DEFAULT_PAC_PORT, DEFAULT_ROUTING_PROXY_BIND,
    DEFAULT_ROUTING_PROXY_PORT, DEFAULT_SOCKS_PORT,
};
use crate::cookie::{self, CookieStatus, FetchOptions};
use crate::pac;
use crate::routing;
use crate::sleepwake;
use crate::vpn::{self, VpnProcess};

/// Options for [`Supervisor::run`].
#[derive(Debug, Clone)]
pub struct SupervisorOptions {
    pub cookie_file: PathBuf,
    pub serve_pac: bool,
    pub check_interval: Duration,
    /// When `true`, never use headless mode for cookie refresh — always
    /// open a visible browser window.
    pub no_headless: bool,
}

/// Top-level orchestrator. Owns the VPN process and the in-process service
/// tasks (routing proxy, PAC server).
pub struct Supervisor {
    options: SupervisorOptions,
    vpn: Mutex<Option<VpnProcess>>,

    /// SOCKS5 port that ocproxy listens on (default 1080).
    socks_port: u16,

    /// Cancellation token for in-process service tasks (routing proxy + PAC
    /// server). Cancelled on shutdown.
    services_shutdown: CancellationToken,

    routing_task: Mutex<Option<JoinHandle<()>>>,
    pac_task: Mutex<Option<JoinHandle<()>>>,
}

impl Supervisor {
    pub fn new(options: SupervisorOptions) -> Self {
        let socks_port = config::cfg_u16("SOCKS_PORT", DEFAULT_SOCKS_PORT);

        Self {
            options,
            vpn: Mutex::new(None),
            socks_port,
            services_shutdown: CancellationToken::new(),
            routing_task: Mutex::new(None),
            pac_task: Mutex::new(None),
        }
    }

    /// Run the full supervisor lifecycle:
    ///   1. Ensure the cookie is valid (refresh if needed).
    ///   2. Start in-process services (routing proxy, PAC).
    ///   3. Start the VPN.
    ///   4. Run the monitor loop until `stop` is cancelled.
    ///   5. Shut everything down cleanly.
    pub async fn run(self: Arc<Self>, stop: CancellationToken) -> Result<()> {
        // Step 1: cookie must be present before we ever exec openconnect.
        if !ensure_valid_cookie(&self.options.cookie_file, &stop, self.options.no_headless).await {
            anyhow::bail!("could not obtain a valid VPN cookie at startup");
        }

        // Step 2: in-process services.
        self.start_services().await?;

        // Step 3: VPN.
        self.start_vpn().await?;

        // Step 4: sleep/wake watcher + monitor loop.
        let watcher = sleepwake::spawn();
        let wake_notify = watcher
            .as_ref()
            .map(|w| Arc::clone(&w.on_resume))
            .unwrap_or_else(|| Arc::new(Notify::new()));

        let monitor_self = Arc::clone(&self);
        let monitor_stop = stop.clone();
        let monitor_wake = Arc::clone(&wake_notify);
        let monitor = tokio::spawn(async move {
            monitor_self.monitor_loop(monitor_stop, monitor_wake).await;
        });

        // Wait for shutdown.
        stop.cancelled().await;

        // Step 5: shut down everything.
        info!("supervisor: shutdown requested");
        if let Some(w) = watcher.as_ref() {
            w.cancel();
        }
        let _ = monitor.await;
        self.shutdown().await;
        Ok(())
    }

    async fn start_services(&self) -> Result<()> {
        {
            let bind = config::cfg_string("ROUTING_PROXY_BIND", DEFAULT_ROUTING_PROXY_BIND);
            let port = config::cfg_u16("ROUTING_PROXY_PORT", DEFAULT_ROUTING_PROXY_PORT);
            let upstream = self.socks_port;
            let shutdown = self.services_shutdown.clone();
            let handle = tokio::spawn(async move {
                if let Err(e) = routing::run(&bind, port, upstream, shutdown).await {
                    error!(error = %e, "routing proxy task exited with error");
                }
            });
            *self.routing_task.lock().await = Some(handle);
        }

        if self.options.serve_pac {
            let bind = config::cfg_string("PAC_SERVE_BIND", DEFAULT_PAC_BIND);
            let port = config::cfg_u16("PAC_SERVE_PORT", DEFAULT_PAC_PORT);
            let shutdown = self.services_shutdown.clone();
            let handle = tokio::spawn(async move {
                if let Err(e) = pac::serve(&bind, port, shutdown).await {
                    error!(error = %e, "PAC server task exited with error");
                }
            });
            *self.pac_task.lock().await = Some(handle);
        }

        Ok(())
    }

    async fn start_vpn(&self) -> Result<()> {
        let mut slot = self.vpn.lock().await;
        if let Some(existing) = slot.as_mut() {
            if existing.is_alive() {
                debug!(pid = existing.pid(), "VPN already running");
                return Ok(());
            }
        }
        let proc = vpn::start(&self.options.cookie_file, self.socks_port)?;
        *slot = Some(proc);
        Ok(())
    }

    async fn stop_vpn(&self, term_timeout: Duration) {
        let mut slot = self.vpn.lock().await;
        if let Some(mut proc) = slot.take() {
            if let Err(e) = proc.stop(term_timeout).await {
                warn!(error = %e, "error stopping VPN");
            }
        }
    }

    async fn restart_vpn(&self) {
        self.stop_vpn(Duration::from_secs(10)).await;
        if let Err(e) = self.start_vpn().await {
            error!(error = %e, "failed to (re)start VPN");
        }
    }

    async fn vpn_alive(&self) -> bool {
        let mut slot = self.vpn.lock().await;
        slot.as_mut().map(|p| p.is_alive()).unwrap_or(false)
    }

    async fn shutdown(&self) {
        // Stop services first so listeners are closed.
        self.services_shutdown.cancel();
        if let Some(h) = self.routing_task.lock().await.take() {
            let _ = h.await;
        }
        if let Some(h) = self.pac_task.lock().await.take() {
            let _ = h.await;
        }
        self.stop_vpn(Duration::from_secs(10)).await;
        info!("supervisor: shutdown complete");
    }

    // ── Monitor loop ──────────────────────────────────────────────────

    async fn monitor_loop(&self, stop: CancellationToken, wake: Arc<Notify>) {
        let check_interval = self.options.check_interval;
        let poll_interval =
            Duration::from_secs_f64((check_interval.as_secs_f64() / 4.0).clamp(1.0, 15.0));
        info!(
            poll_interval_s = poll_interval.as_secs_f64(),
            check_interval_s = check_interval.as_secs_f64(),
            "monitor started"
        );

        let mut last_check = Instant::now();
        let mut last_wall = SystemTime::now();

        loop {
            let wait_result = wait_for_stop_or_wake(&stop, &wake, poll_interval).await;
            if matches!(wait_result, WaitResult::Stop) {
                break;
            }
            let wake_signaled = matches!(wait_result, WaitResult::Wake);

            let now_mono = Instant::now();
            let now_wall = SystemTime::now();
            let wall_delta = now_wall.duration_since(last_wall).unwrap_or(Duration::ZERO);
            last_wall = now_wall;

            let suspend_threshold =
                Duration::from_secs_f64((poll_interval.as_secs_f64() * 4.0).max(30.0));
            let suspended = wall_delta > suspend_threshold;
            if wake_signaled {
                info!("OS sleep/wake watcher reported resume; re-validating now");
            } else if suspended {
                info!(
                    wall_delta_s = wall_delta.as_secs_f64(),
                    "wall-clock jump detected (suspend/resume likely); re-validating now"
                );
            }
            let force_check = wake_signaled || suspended;

            // PAC / routing-proxy liveness: the tasks are tokio-spawned in
            // this process, so death is unrecoverable mid-run. Log and
            // continue — the supervisor will shut down on the next stop
            // signal.
            if let Some(handle) = self.pac_task.lock().await.as_ref() {
                if handle.is_finished() {
                    warn!("PAC server task has finished unexpectedly");
                }
            }
            if let Some(handle) = self.routing_task.lock().await.as_ref() {
                if handle.is_finished() {
                    warn!("routing proxy task has finished unexpectedly");
                }
            }

            // VPN liveness — restart from scratch if openconnect died.
            if !self.vpn_alive().await {
                warn!("VPN process is not running; (re)starting");
                if ensure_valid_cookie(&self.options.cookie_file, &stop, self.options.no_headless)
                    .await
                {
                    if let Err(e) = self.start_vpn().await {
                        error!(error = %e, "failed to (re)start VPN");
                    }
                } else {
                    error!("no valid cookie available; will retry on next cycle");
                }
                last_check = now_mono;
                continue;
            }

            // Periodic cookie validation (or forced after a suspend/resume).
            let elapsed = now_mono.duration_since(last_check);
            if force_check || elapsed >= check_interval {
                let rc = if force_check {
                    validate_cookie_with_retry(
                        &self.options.cookie_file,
                        &stop,
                        Duration::from_secs(60),
                    )
                    .await
                } else {
                    cookie::validate_file(&self.options.cookie_file).await
                };

                match rc {
                    CookieStatus::Valid => {
                        last_check = now_mono;
                        if force_check {
                            info!(
                                "cookie still valid; restarting VPN for a fresh tunnel \
                                 after suspend/resume"
                            );
                            self.restart_vpn().await;
                        } else {
                            debug!("periodic check: cookie still valid");
                        }
                    }
                    CookieStatus::NetworkError => {
                        // Do NOT advance last_check — retry next short poll.
                        info!("periodic check: network error talking to VPN; will retry shortly");
                    }
                    CookieStatus::Invalid => {
                        last_check = now_mono;
                        info!("periodic check: cookie expired/invalid — refreshing and restarting VPN");
                        if refresh_cookie(
                            &self.options.cookie_file,
                            &stop,
                            self.options.no_headless,
                        )
                        .await
                        {
                            self.restart_vpn().await;
                        } else {
                            error!("cookie refresh failed; will retry on next cycle");
                        }
                    }
                }
            }
        }

        info!("monitor loop exiting");
    }
}

// ── Cookie helpers ────────────────────────────────────────────────────────

/// Validate the cookie; refresh if expired/invalid. Returns true iff usable
/// (a valid cookie, or NetworkError which we treat as "keep going and let
/// openconnect decide").
pub async fn ensure_valid_cookie(
    cookie_file: &std::path::Path,
    stop: &CancellationToken,
    no_headless: bool,
) -> bool {
    match cookie::validate_file(cookie_file).await {
        CookieStatus::Valid => {
            info!("VPN cookie is valid");
            true
        }
        CookieStatus::NetworkError => {
            warn!(
                "cookie validation hit a network error; keeping existing cookie and \
                 letting openconnect decide"
            );
            true
        }
        CookieStatus::Invalid => {
            if cookie_exists_nonempty(cookie_file) {
                info!("existing cookie is expired or invalid; refreshing");
            } else {
                info!(
                    path = %cookie_file.display(),
                    "no cookie file; fetching a fresh one"
                );
            }
            refresh_cookie(cookie_file, stop, no_headless).await
        }
    }
}

fn cookie_exists_nonempty(path: &std::path::Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.len() > 0)
        .unwrap_or(false)
}

async fn refresh_cookie(
    cookie_file: &std::path::Path,
    stop: &CancellationToken,
    #[cfg_attr(target_os = "macos", allow(unused))] no_headless: bool,
) -> bool {
    #[cfg_attr(target_os = "macos", allow(unused))]
    let has_credentials = config::vpn_credentials().is_some();

    // On macOS, always use headed mode — headless Chrome has issues with
    // DeviceAuthTls flows and the MFA approval screen is easier to handle
    // when the user can see the browser window.
    #[cfg(target_os = "macos")]
    let headless = false;
    #[cfg(not(target_os = "macos"))]
    let headless = has_credentials && !no_headless;

    if headless {
        info!("refreshing VPN cookie via headless browser (credentials available)");
    } else {
        info!("refreshing VPN cookie via browser login (this opens Chromium)");
    }

    let mut opts = FetchOptions::default();
    opts.output = Some(cookie_file.to_path_buf());
    opts.headless = headless;
    opts.stop = stop.clone();
    match cookie::fetch(opts).await {
        Ok(_) => {
            info!(path = %cookie_file.display(), "cookie refreshed successfully");
            true
        }
        Err(e) => {
            error!(error = %e, "cookie refresh failed");
            false
        }
    }
}

async fn validate_cookie_with_retry(
    cookie_file: &std::path::Path,
    stop: &CancellationToken,
    max_wait: Duration,
) -> CookieStatus {
    let deadline = Instant::now() + max_wait;
    let mut delay = Duration::from_secs(1);
    let mut attempts = 0u32;
    let mut last = CookieStatus::NetworkError;
    while !stop.is_cancelled() {
        attempts += 1;
        last = cookie::validate_file(cookie_file).await;
        if last != CookieStatus::NetworkError {
            if attempts > 1 {
                info!(attempts, "cookie validation succeeded after retry");
            }
            return last;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            info!(
                attempts,
                max_wait_s = max_wait.as_secs_f64(),
                "cookie validation: still no network; giving up for now"
            );
            return last;
        }
        let sleep_for = std::cmp::min(delay, remaining);
        if attempts == 1 {
            info!(
                max_wait_s = max_wait.as_secs_f64(),
                "cookie validation: network error; retrying with exponential backoff"
            );
        } else {
            debug!(attempts, sleep_for_s = sleep_for.as_secs_f64(), "retry");
        }
        tokio::select! {
            _ = stop.cancelled() => return last,
            _ = tokio::time::sleep(sleep_for) => {}
        }
        delay = Duration::from_secs_f64((delay.as_secs_f64() * 1.5).min(10.0));
    }
    last
}

// ── Wait helpers ─────────────────────────────────────────────────────────

enum WaitResult {
    Stop,
    Wake,
    Timeout,
}

async fn wait_for_stop_or_wake(
    stop: &CancellationToken,
    wake: &Notify,
    timeout: Duration,
) -> WaitResult {
    tokio::select! {
        _ = stop.cancelled() => WaitResult::Stop,
        _ = wake.notified() => WaitResult::Wake,
        _ = tokio::time::sleep(timeout) => WaitResult::Timeout,
    }
}
