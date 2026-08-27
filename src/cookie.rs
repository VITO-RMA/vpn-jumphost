//! F5 `MRHSession` cookie management.
//!
//! - [`CookieStatus`] mirrors the tri-state exit codes of the old
//!   `scripts/validate-vpn-cookie.py` (0/1/2 → Valid/Invalid/NetworkError).
//! - [`validate_file`] reads a cookie from disk and probes the VPN
//!   endpoint with redirects disabled. A 3xx redirect from the F5 gateway
//!   means the session is expired — critically, we must **not** follow it
//!   (the SSO login page returns 200 and would look valid).
//! - [`fetch`] opens a Chromium browser (persistent user-data-dir)
//!   for SSO + MFA and polls for the `MRHSession` cookie. Replaces the old
//!   `scripts/fetch-vpn-cookie.py` + `playwright` flow with a pure-Rust
//!   implementation that talks the Chrome DevTools Protocol.
//!   When `headless` is set the browser launches without a visible window.
//!   Authenticator number matching remains headless; prompts requiring
//!   browser input relaunch in headed mode. Automated Authenticator pushes
//!   are capped at three across all refreshes in one supervisor process.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use chromiumoxide::browser::{Browser, BrowserConfig};
use futures_util::StreamExt;
use reqwest::StatusCode;
use reqwest::redirect::Policy as RedirectPolicy;
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::config;

/// Outcome of a cookie validation probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CookieStatus {
    /// VPN endpoint accepted the cookie (HTTP 200/2xx, not a 3xx, not 404).
    Valid,
    /// File is missing/empty, the endpoint returned 404, or a 3xx redirect
    /// (the F5 gateway redirects expired sessions to the SSO login page).
    Invalid,
    /// Could not reach the endpoint at all. Caller should normally **not**
    /// treat this as "invalid".
    NetworkError,
}

/// Read the cookie file and probe the VPN endpoint with it.
pub async fn validate_file(cookie_file: &Path) -> CookieStatus {
    let cookie = match fs::read_to_string(cookie_file) {
        Ok(s) => s.trim().to_string(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            debug!(path = %cookie_file.display(), "cookie file not found");
            return CookieStatus::Invalid;
        }
        Err(e) => {
            warn!(path = %cookie_file.display(), error = %e, "cannot read cookie file");
            return CookieStatus::Invalid;
        }
    };
    if cookie.is_empty() {
        debug!(path = %cookie_file.display(), "cookie file is empty");
        return CookieStatus::Invalid;
    }
    validate_cookie(&cookie).await
}

/// Probe the VPN endpoint with a single cookie value.
///
/// The probe deliberately disables HTTP redirects: a 302 from the F5
/// gateway redirecting to the SSO login page means the cookie is expired,
/// not valid.
pub async fn validate_cookie(cookie: &str) -> CookieStatus {
    let vpn_url = config::cfg_string("VPN_URL", config::DEFAULT_VPN_URL);
    let probe_url = format!("{}{}", vpn_url.trim_end_matches('/'), config::COOKIE_PROBE_PATH);

    let client = match reqwest::Client::builder()
        .no_proxy()
        .redirect(RedirectPolicy::none())
        .timeout(Duration::from_secs(10))
        .user_agent("vpn-jumphost-cookie-check")
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "could not build reqwest client");
            return CookieStatus::NetworkError;
        }
    };

    let resp = client
        .get(&probe_url)
        .header("Cookie", format!("{}={cookie}", config::COOKIE_NAME))
        .send()
        .await;

    match resp {
        Ok(r) => {
            let status = r.status();
            if status.is_redirection() {
                debug!(status = %status, "cookie-check: got redirect — cookie expired");
                return CookieStatus::Invalid;
            }
            if status == StatusCode::NOT_FOUND {
                debug!(status = %status, "cookie-check: 404 — cookie invalid");
                return CookieStatus::Invalid;
            }
            CookieStatus::Valid
        }
        Err(e) => {
            debug!(error = %e, "cookie-check: network error");
            CookieStatus::NetworkError
        }
    }
}

/// Write `value` to `path` with mode 600 (creates parent dirs).
pub fn write_cookie_file(path: &Path, value: &str) -> Result<()> {
    config::ensure_parent_dir(path).with_context(|| format!("creating parent dir for {}", path.display()))?;
    fs::write(path, value).with_context(|| format!("writing {}", path.display()))?;
    // chmod 600 (best effort on Unix).
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o600);
    fs::set_permissions(path, perms)?;
    Ok(())
}

// ── Cookie fetch (Chromium via CDP) ─────────────────────────────────────

/// Maximum Authenticator push notifications allowed before automated
/// authentication is paused for the lifetime of the supervisor.
pub const MAX_MFA_PUSH_ATTEMPTS: usize = 3;

/// A push-attempt budget shared by every cookie refresh in one supervisor.
///
/// Sharing this across browser sessions is important: a service manager may
/// keep the supervisor running indefinitely, and each timed-out browser
/// session must not receive a fresh set of MFA attempts.
#[derive(Debug, Clone, Default)]
pub struct MfaAttemptBudget {
    attempts: Arc<AtomicUsize>,
}

impl MfaAttemptBudget {
    pub fn attempts(&self) -> usize {
        self.attempts.load(Ordering::Relaxed)
    }

    pub fn is_exhausted(&self) -> bool {
        self.attempts() >= MAX_MFA_PUSH_ATTEMPTS
    }

    fn try_record_attempt(&self) -> std::result::Result<usize, MfaAttemptLimitReached> {
        self.attempts
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |attempts| {
                (attempts < MAX_MFA_PUSH_ATTEMPTS).then_some(attempts + 1)
            })
            .map(|previous| previous + 1)
            .map_err(|attempts| MfaAttemptLimitReached { attempts })
    }

    pub fn reset(&self) {
        self.attempts.store(0, Ordering::Relaxed);
    }
}

/// Returned when another automated Authenticator push would exceed the
/// process-wide safety limit.
#[derive(Debug, Error)]
#[error(
    "MFA was not completed after {attempts} Authenticator notifications; automatic authentication is paused. Run `jumphost authenticate` when you are present, or restart the service to allow another three attempts"
)]
pub struct MfaAttemptLimitReached {
    attempts: usize,
}

fn mfa_attempt_limit_error(budget: &MfaAttemptBudget) -> anyhow::Error {
    MfaAttemptLimitReached {
        attempts: budget.attempts(),
    }
    .into()
}

/// Options for [`fetch`].
pub struct FetchOptions {
    /// Persistent user-data-dir (browser profile). When `None`, an
    /// ephemeral temp dir is used.
    pub profile_dir: Option<PathBuf>,
    /// Maximum time to wait for the user to complete SSO + MFA.
    pub max_wait: Duration,
    /// Optional path to a Chromium executable. If `None`, chromiumoxide
    /// auto-detects (`$CHROME`, then platform defaults).
    pub chromium_path: Option<PathBuf>,
    /// Launch Chromium without a visible window. When an MFA prompt is
    /// detected, the browser is closed and relaunched in headed mode
    /// automatically.
    pub headless: bool,
    /// Cancellation token — when cancelled, the fetch loop exits early
    /// and the browser is closed. Allows SIGTERM / SIGINT to interrupt
    /// the (potentially long-running) SSO + MFA flow.
    pub stop: CancellationToken,
    /// Authenticator push-attempt budget. The supervisor supplies one shared
    /// instance across refreshes so retries cannot exceed the safety limit.
    pub mfa_attempt_budget: MfaAttemptBudget,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            profile_dir: Some(config::default_browser_profile_dir()),
            max_wait: Duration::from_secs(300),
            chromium_path: config::chromium_path(),
            headless: false,
            stop: CancellationToken::new(),
            mfa_attempt_budget: MfaAttemptBudget::default(),
        }
    }
}

/// Outcome of the inner fetch loop.
enum FetchOutcome {
    /// Successfully captured a valid MRHSession cookie.
    Cookie(String),
    /// An MFA screen was detected that requires interactive input in the
    /// browser (e.g. TOTP code entry). The caller should relaunch in
    /// headed mode.
    InteractionRequired,
}

/// Open a Chromium browser, wait for SSO to complete, and return the
/// captured `MRHSession` cookie. On success the cookie is also written
/// to `config::cookie_file_path()`.
///
/// When `options.headless` is `true` the browser starts without a visible
/// window. For Authenticator push MFA the number to match is extracted
/// from the page and shown as a desktop notification so the user can
/// approve on their phone without a browser window. If a TOTP code entry
/// or other interactive prompt is detected, the headless browser is
/// closed and relaunched in headed mode.
pub async fn fetch(options: FetchOptions) -> Result<String> {
    if options.mfa_attempt_budget.is_exhausted() {
        return Err(mfa_attempt_limit_error(&options.mfa_attempt_budget));
    }

    let vpn_url = config::cfg_string("VPN_URL", config::DEFAULT_VPN_URL);
    if vpn_url.is_empty() {
        return Err(anyhow!(
            "VPN_URL is not configured — set vpn_url in the config file or export VPN_URL"
        ));
    }

    let cookie = if options.headless {
        info!("attempting headless cookie fetch");
        match launch_and_fetch(&vpn_url, &options, true).await? {
            FetchOutcome::Cookie(c) => c,
            FetchOutcome::InteractionRequired => {
                if options.stop.is_cancelled() {
                    return Err(anyhow!("interrupted"));
                }
                info!("interactive MFA prompt detected — relaunching browser with visible window");
                match launch_and_fetch(&vpn_url, &options, false).await? {
                    FetchOutcome::Cookie(c) => c,
                    FetchOutcome::InteractionRequired => {
                        return Err(anyhow!("interactive MFA required but headed browser also failed"));
                    }
                }
            }
        }
    } else {
        match launch_and_fetch(&vpn_url, &options, false).await? {
            FetchOutcome::Cookie(c) => c,
            FetchOutcome::InteractionRequired => {
                unreachable!("InteractionRequired is never returned in headed mode")
            }
        }
    };

    let out = config::cookie_file_path();
    write_cookie_file(&out, &cookie)?;
    info!(path = %out.display(), "cookie saved");

    Ok(cookie)
}

/// Launch Chromium (headed or headless), run the SSO flow, and return
/// either a captured cookie or an [`FetchOutcome::InteractionRequired`] signal.
async fn launch_and_fetch(vpn_url: &str, options: &FetchOptions, headless: bool) -> Result<FetchOutcome> {
    // Authentication must not depend on a tunnel that cannot start until this
    // flow produces a cookie. Override desktop/PAC/environment proxy settings
    // for the dedicated Chromium process, regardless of domain routing rules.
    let mut builder = BrowserConfig::builder().window_size(1280, 900).arg("--no-proxy-server");

    if !headless {
        builder = builder.with_head();
    }

    if let Some(profile) = &options.profile_dir {
        config::ensure_parent_dir(profile).with_context(|| format!("creating profile dir parent for {}", profile.display()))?;
        builder = builder.user_data_dir(profile);
        info!(profile = %profile.display(), headless, "using persistent Chromium profile");
    } else {
        info!(headless, "using ephemeral Chromium profile");
    }

    if let Some(exe) = &options.chromium_path {
        if !exe.exists() {
            return Err(anyhow!("configured Chromium executable not found: {}", exe.display()));
        }
        builder = builder.chrome_executable(exe);
        info!(path = %exe.display(), "using configured Chromium executable");
    }

    let config = builder.build().map_err(|e| anyhow!("could not build Chromium config: {e}"))?;
    let (mut browser, mut handler) = Browser::launch(config)
        .await
        .map_err(|e| anyhow!("could not launch Chromium: {e}. Is `chromium` installed?"))?;

    let handler_task = tokio::spawn(async move {
        while let Some(h) = handler.next().await {
            if let Err(e) = h {
                debug!(error = %e, "chromiumoxide handler event");
            }
        }
    });

    let result = fetch_inner(
        &mut browser,
        vpn_url,
        options.max_wait,
        headless,
        &options.stop,
        &options.mfa_attempt_budget,
    )
    .await;

    // Clean up the browser with a timeout so a broken CDP connection or a
    // slow Chromium shutdown cannot hang the process (e.g. after Ctrl-C).
    let cleanup = async {
        if let Err(e) = browser.close().await {
            debug!(error = %e, "browser.close() failed");
        }
        let _ = browser.wait().await;
    };

    if tokio::time::timeout(Duration::from_secs(5), cleanup).await.is_err() {
        warn!("browser cleanup timed out after 5 s; abandoning");
    }
    handler_task.abort();

    result
}

async fn fetch_inner(
    browser: &mut Browser,
    vpn_url: &str,
    max_wait: Duration,
    headless: bool,
    stop: &CancellationToken,
    mfa_attempt_budget: &MfaAttemptBudget,
) -> Result<FetchOutcome> {
    info!(url = %vpn_url, headless, "opening browser for VPN login");
    let page = browser
        .new_page(vpn_url)
        .await
        .map_err(|e| anyhow!("could not open new page: {e}"))?;

    // Best-effort credential auto-fill.
    let creds = config::vpn_credentials();
    if let Some(creds) = &creds {
        try_microsoft_login(&page, &creds.username, &creds.password).await;
    } else {
        debug!("no credentials configured; skipping auto-fill");
    }

    // Poll the browser's cookie jar for an accepted `MRHSession`.
    let deadline = Instant::now() + max_wait;
    let mut warned_invalid = false;
    let mut notified_number: Option<String> = None;
    let mut mfa_attempt_state = MfaAttemptState::Idle;
    let mut mfa_notification = NotificationGuard::new();
    let mut stuck_url_count: u32 = 0;
    let mut last_url = String::new();
    loop {
        // Fast-path: catch cancellations that arrived during a short CDP call in the previous iteration.
        if stop.is_cancelled() {
            info!("cookie fetch interrupted by shutdown signal");
            return Err(anyhow!("interrupted"));
        }

        // Poll the browser cookie jar.  Wrapped in `select!` because the
        // CDP `Storage.getCookies` request can hang for minutes after the
        // browser process is killed by a terminal signal (Ctrl-C).
        let maybe_value = tokio::select! {
            v = current_mrhsession(browser) => v,
            _ = stop.cancelled() => {
                info!("cookie fetch interrupted by shutdown signal");
                return Err(anyhow!("interrupted"));
            }
        };

        if let Some(value) = maybe_value {
            if matches!(validate_cookie(&value).await, CookieStatus::Valid) {
                info!("captured valid MRHSession cookie");
                // Dismiss the MFA number-match notification now that
                // login has succeeded.
                mfa_notification.close();
                return Ok(FetchOutcome::Cookie(value));
            }
            if !warned_invalid {
                warn!(
                    "found {} cookie but VPN endpoint rejected it; waiting for fresh login",
                    config::COOKIE_NAME
                );
                warned_invalid = true;
            }
        }

        // Track MFA pushes in both headed and headless sessions so the
        // process-wide safety limit also covers visible-browser refreshes.
        // In headless mode we additionally select the Authenticator method,
        // display the number, and fall back to a headed browser for prompts
        // that require browser input.
        {
            // Detect stuck transitional pages (e.g. DeviceAuthTls/reprocess)
            // that auto-redirect in headed mode but hang in headless.
            let current_url = page
                .evaluate("location.href")
                .await
                .ok()
                .and_then(|v| v.into_value::<String>().ok())
                .unwrap_or_default();
            if current_url == last_url {
                stuck_url_count += 1;
            } else {
                stuck_url_count = 0;
                last_url = current_url.clone();
            }
            if headless && stuck_url_count >= 5 && current_url.contains("/DeviceAuthTls") {
                warn!(url = %current_url, "stuck on device-auth page; reloading VPN URL to retry");
                let _ = page.evaluate(format!("location.href = {:?}", vpn_url)).await;
                stuck_url_count = 0;
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }

            match detect_mfa_phase(&page).await {
                MfaPhase::None => {}
                MfaPhase::MethodPicker if headless => {
                    // A picker can remain in the DOM for several polls after
                    // it is clicked. Only click once until a number screen has
                    // appeared; otherwise one login can generate rapid pushes.
                    if matches!(&mfa_attempt_state, MfaAttemptState::Idle | MfaAttemptState::Number(_)) {
                        if mfa_attempt_budget.is_exhausted() {
                            return Err(mfa_attempt_limit_error(mfa_attempt_budget));
                        }
                        info!("MFA method-picker detected — selecting Authenticator app");
                        if click_authenticator_option(&page).await {
                            let attempt = mfa_attempt_budget.try_record_attempt()?;
                            info!(attempt, max_attempts = MAX_MFA_PUSH_ATTEMPTS, "triggered Authenticator push");
                            mfa_attempt_state = MfaAttemptState::Triggered;
                        }
                        // Give the page a moment to transition to the
                        // approval screen before the next poll.
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                }
                MfaPhase::MethodPicker => {}
                MfaPhase::NumberMatch(ref num) => {
                    let is_new_attempt = match &mfa_attempt_state {
                        MfaAttemptState::Idle => true,
                        MfaAttemptState::Triggered => false,
                        MfaAttemptState::Number(previous) => previous != num,
                    };
                    if is_new_attempt {
                        let attempt = mfa_attempt_budget.try_record_attempt()?;
                        info!(attempt, max_attempts = MAX_MFA_PUSH_ATTEMPTS, "detected Authenticator push");
                    }
                    mfa_attempt_state = MfaAttemptState::Number(num.clone());

                    // Show the number via desktop notification once in
                    // headless mode. A headed browser already displays it.
                    if headless && notified_number.as_deref() != Some(num) {
                        eprintln!();
                        eprintln!("  ╔═══════════════════════════════════════╗");
                        eprintln!("  ║  MFA: approve this number  ->  {num:>3}    ║");
                        eprintln!("  ╚═══════════════════════════════════════╝");
                        eprintln!();
                        info!(number = %num, "MFA: approve sign-in request with this number");
                        mfa_notification.set(send_mfa_notification(num).await);
                        notified_number = Some(num.clone());
                    }
                }
                MfaPhase::ApprovalPending => {
                    if matches!(&mfa_attempt_state, MfaAttemptState::Idle) {
                        let attempt = mfa_attempt_budget.try_record_attempt()?;
                        info!(attempt, max_attempts = MAX_MFA_PUSH_ATTEMPTS, "detected pending Authenticator push");
                        mfa_attempt_state = MfaAttemptState::Triggered;
                    }
                    if headless && notified_number.is_none() {
                        // Dump visible text once so we can find the right selector.
                        let dump = page
                            .evaluate("(document.body && document.body.innerText || '').substring(0, 500)")
                            .await
                            .ok()
                            .and_then(|v| v.into_value::<String>().ok())
                            .unwrap_or_default();
                        warn!(page_text = %dump, "MFA approval screen detected but could not extract the number — check your phone");
                    }
                }
                MfaPhase::InteractivePrompt if headless => {
                    info!("interactive MFA prompt detected (e.g. TOTP) — need visible browser");
                    return Ok(FetchOutcome::InteractionRequired);
                }
                MfaPhase::InteractivePrompt => {}
            }
        }

        // MFA approval screen — tick "Don't ask again for 14 days"
        // (#idChkBx_SAOTCAS_TD) before the user approves on their phone.
        // Runs unconditionally so it works in both headed and headless flows.
        try_mfa_remember_device(&page).await;

        // Re-attempt credential fill in case Microsoft moved to a new
        // form between polls (e.g. account picker → password).
        if let Some(creds) = &creds {
            try_microsoft_login(&page, &creds.username, &creds.password).await;
        } else {
            debug!("no credentials configured; skipping auto-fill (loop)");
        }

        if Instant::now() >= deadline {
            if mfa_attempt_budget.is_exhausted() {
                return Err(mfa_attempt_limit_error(mfa_attempt_budget));
            }
            return Err(anyhow!("did not find a valid {} cookie within {:?}", config::COOKIE_NAME, max_wait));
        }
        tokio::select! {
            _ = stop.cancelled() => {
                info!("cookie fetch interrupted by shutdown signal");
                return Err(anyhow!("interrupted"));
            }
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
    }
}

/// State used to avoid counting the number screen twice after this process
/// triggered the corresponding push from the method picker.
enum MfaAttemptState {
    Idle,
    Triggered,
    Number(String),
}

/// Which MFA phase the page is currently showing.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MfaPhase {
    /// No MFA-related UI detected.
    None,
    /// The method-picker screen ("Verify your identity") where the user
    /// must choose between Authenticator push, TOTP code, phone call, etc.
    /// We can advance past this automatically by clicking the
    /// Authenticator option.
    MethodPicker,
    /// The Authenticator push approval screen showing the number to match
    /// on the phone. We can stay headless — the number is extracted from
    /// the DOM and printed to the terminal.
    NumberMatch(String),
    /// The Authenticator push approval screen is visible but the number
    /// element has not rendered (yet) or uses an unknown selector.
    ApprovalPending,
    /// A screen that requires interactive input in the browser (TOTP code
    /// entry, phone-call verification, or an unrecognised MFA prompt).
    /// Must relaunch headed.
    InteractivePrompt,
}

/// Determine which MFA phase (if any) the current page is showing.
///
/// The detection is ordered so that the more-specific approval-screen
/// selectors are checked first — once the Authenticator option has been
/// clicked the method picker DOM is often still present underneath.
///
/// The JS returns a plain string: `"none"`, `"picker"`, `"interactive"`,
/// or `"number:XX"` when the Authenticator number-match screen is
/// visible (XX is the number to tap on the phone).
async fn detect_mfa_phase(page: &chromiumoxide::Page) -> MfaPhase {
    let script = r#"(() => {
        // ── Authenticator number-match screen ────────────────────────
        // The element #idRichContext_DisplaySign contains the number
        // the user must tap on their phone.
        const numEl = document.querySelector('#idRichContext_DisplaySign');
        if (numEl) {
            const num = (numEl.textContent || '').trim();
            if (num) return 'number:' + num;
        }

        // Broader fallback: look for a standalone 2-digit number in
        // elements that Microsoft may use for the display code.
        const altSelectors = [
            '.display-sign-container', '.displaySign',
            '[data-bind*="DisplaySign"]', '.sign-in-number'
        ];
        for (const sel of altSelectors) {
            const el = document.querySelector(sel);
            if (el) {
                const num = (el.textContent || '').trim();
                if (/^\d{1,3}$/.test(num)) return 'number:' + num;
            }
        }

        // ── Interactive prompts (need a visible browser) ────────────
        // TOTP / verification-code input box.
        if (document.querySelector('#idTxtBx_SAOTCC_OTC'))       return 'interactive';
        // Phone call verification.
        if (document.querySelector('#idDiv_SAOTCC_Description')) return 'interactive';

        // Text fallbacks for interactive screens.
        const body = (document.body && document.body.innerText) || '';
        const lower = body.toLowerCase();
        if (lower.includes('enter the code shown'))              return 'interactive';

        // ── Approval screen (push sent, waiting for user) ───────────
        // Require the actual approval-screen DOM element to avoid
        // false positives on transitional pages (e.g. DeviceAuthTls).
        if (document.querySelector('#idDiv_SAOTCAS_Title')) {
            // Last-ditch: scan the page text for a standalone 2-digit
            // number that looks like a match code.
            const m = body.match(/(?:^|\n)\s*(\d{1,3})\s*(?:\n|$)/);
            if (m) return 'number:' + m[1];
            return 'pending';
        }

        // ── Method picker ───────────────────────────────────────────
        // Require the actual picker DOM container — text-only checks
        // like "verify your identity" match too broadly (e.g. the
        // DeviceAuthTls/reprocess page) and cause false positives.
        if (document.querySelector('#idDiv_SAASDS_Title') ||
            document.querySelector('#idDiv_SAASDS_DEFAULT'))     return 'picker';

        return 'none';
    })()"#;

    match page.evaluate(script).await {
        Ok(val) => {
            let raw = match val.into_value::<String>() {
                Ok(s) => s,
                _ => return MfaPhase::None,
            };
            if let Some(num) = raw.strip_prefix("number:") {
                MfaPhase::NumberMatch(num.to_string())
            } else {
                match raw.as_str() {
                    "picker" => MfaPhase::MethodPicker,
                    "pending" => MfaPhase::ApprovalPending,
                    "interactive" => MfaPhase::InteractivePrompt,
                    _ => MfaPhase::None,
                }
            }
        }
        Err(e) => {
            debug!(error = %e, "MFA phase detection script failed");
            MfaPhase::None
        }
    }
}

/// Click the "Approve a request on my Microsoft Authenticator app" option
/// on the MFA method-picker page. This triggers the push notification so
/// the approval screen (with the number to match) appears next.
async fn click_authenticator_option(page: &chromiumoxide::Page) -> bool {
    let script = r#"(() => {
        // The method-picker renders each option as a clickable div. Find
        // the one whose text mentions "Microsoft Authenticator".
        const divs = document.querySelectorAll(
            '[data-value], [role="button"], .tile-img, div[class*="option"], div'
        );
        for (const d of divs) {
            const txt = (d.textContent || '').toLowerCase();
            if (txt.includes('approve a request') && txt.includes('authenticator')) {
                d.click();
                return true;
            }
        }
        // Fallback: click the first tile in the selection list.
        const first = document.querySelector(
            '#idDiv_SAASDS_DEFAULT div[role="button"], #idDiv_SAASDS_DEFAULT > div'
        );
        if (first) { first.click(); return true; }
        return false;
    })()"#;

    match page.evaluate(script).await {
        Ok(value) => {
            let clicked = value.into_value::<bool>().unwrap_or(false);
            if clicked {
                debug!("clicked Authenticator option on MFA method picker");
            } else {
                debug!("Authenticator option was not clickable on MFA method picker");
            }
            clicked
        }
        Err(e) => {
            debug!(error = %e, "could not click Authenticator option");
            false
        }
    }
}

/// Handle type for a revocable desktop notification.
///
/// On Linux, `notify_rust::NotificationHandle::close()` lets us revoke
/// the notification over D-Bus. On macOS the legacy notify-rust backend
/// delivers only when the handle is dropped, so we must not retain it.
type NotifHandle = notify_rust::NotificationHandle;

/// Guard that closes a desktop notification when dropped or explicitly
/// closed.  Ensures the MFA number-match notification is dismissed once
/// login succeeds (or the fetch is abandoned for any reason).
struct NotificationGuard(Option<NotifHandle>);

impl NotificationGuard {
    fn new() -> Self {
        Self(None)
    }

    /// Replace the stored handle, closing the previous notification (if
    /// any) first.  Passing `None` just closes without storing a new one.
    fn set(&mut self, handle: Option<NotifHandle>) {
        self.close();
        self.0 = handle;
    }

    /// Explicitly close the notification (idempotent).
    #[cfg(target_os = "linux")]
    fn close(&mut self) {
        if let Some(handle) = self.0.take() {
            // On Linux, `NotificationHandle::close()` sends a D-Bus
            // `CloseNotification` message. It uses zbus's blocking
            // `block_on` internally, which panics when called from
            // within a tokio runtime, so spawn a short-lived thread.
            #[cfg(target_os = "linux")]
            {
                let _ = std::thread::Builder::new().name("close-mfa-notif".into()).spawn(move || {
                    handle.close();
                });
            }
            info!("closed MFA desktop notification");
        }
    }

    /// Explicitly close the notification (idempotent).
    #[cfg(not(target_os = "linux"))]
    fn close(&mut self) {
        self.0 = None;
    }
}

impl Drop for NotificationGuard {
    fn drop(&mut self) {
        self.close();
    }
}

/// Show the MFA number-match code as a desktop notification so the user
/// can approve on their phone even when the browser is headless.
///
/// Uses [`notify_rust`] on all platforms (D-Bus on Linux,
/// `mac-notification-sys` on macOS).  Returns a [`NotifHandle`] so the
/// caller can close (revoke) the notification once login succeeds
/// (revocation is only effective on Linux).
///
/// Failures are logged but not fatal; the number is also emitted via
/// `tracing::info` for journal/log consumers.
async fn send_mfa_notification(number: &str) -> Option<NotifHandle> {
    let number = number.to_owned();
    tokio::task::spawn_blocking(move || {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let notification = build_mfa_notification(&number);

            match notification.show() {
                Ok(handle) => {
                    // notify-rust's legacy macOS backend is lazy: `show()`
                    // returns a handle and the handle's Drop implementation
                    // performs the actual delivery. Holding that handle for
                    // later "close" support delays the MFA banner until after
                    // authentication, leaving the user to see an older
                    // Notification Center item. Deliver immediately on macOS;
                    // there is no programmatic close support there anyway.
                    #[cfg(target_os = "macos")]
                    {
                        drop(handle);
                        info!(number = %number, "sent MFA desktop notification");
                        None
                    }

                    #[cfg(target_os = "linux")]
                    {
                        info!(number = %number, "sent MFA desktop notification");
                        Some(handle)
                    }
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        number = %number,
                        "desktop notification failed; approve the number shown in the log"
                    );
                    None
                }
            }
        }

        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            warn!(
                number = %number,
                "no notification backend for this platform; approve the number shown in the log"
            );
            None
        }
    })
    .await
    .ok()
    .flatten()
}

fn build_mfa_notification(number: &str) -> notify_rust::Notification {
    let mut notification = notify_rust::Notification::new();
    notification
        .summary(&format!("VPN sign-in code {number}"))
        .body(&format!("Approve {number} in Microsoft Authenticator."))
        .icon("dialog-password")
        .appname("jumphost");

    #[cfg(target_os = "linux")]
    {
        notification.urgency(notify_rust::Urgency::Critical);
    }

    notification
}

/// Read `MRHSession` from the browser-wide cookie jar (covers all open
/// tabs and the persistent profile).
///
/// Uses [`Browser::get_cookies`], which under the hood issues
/// `Storage.getCookies` against the browser target — the only CDP call
/// that returns cookies across every page/origin without requiring a
/// page session. Earlier versions of this code issued
/// `Network.getCookies` directly against the browser, which silently
/// returned an empty list because `Network` is a per-target domain.
async fn current_mrhsession(browser: &Browser) -> Option<String> {
    match browser.get_cookies().await {
        Ok(cookies) => cookies
            .into_iter()
            .find(|c| c.name == config::COOKIE_NAME && !c.value.is_empty())
            .map(|c| c.value),
        Err(e) => {
            debug!(error = %e, "Storage.getCookies failed");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mfa_notification_payload_contains_the_current_number() {
        let notification = build_mfa_notification("24");

        assert_eq!(notification.summary, "VPN sign-in code 24");
        assert_eq!(notification.body, "Approve 24 in Microsoft Authenticator.");
        assert_eq!(notification.appname, "jumphost");
    }

    #[test]
    fn mfa_notification_payload_does_not_reuse_a_previous_number() {
        let first = build_mfa_notification("24");
        let second = build_mfa_notification("91");

        assert!(first.summary.contains("24"));
        assert!(!first.summary.contains("91"));
        assert!(second.summary.contains("91"));
        assert!(!second.summary.contains("24"));
    }

    #[test]
    fn mfa_attempt_budget_allows_exactly_three_attempts() {
        let budget = MfaAttemptBudget::default();

        for expected in 1..=MAX_MFA_PUSH_ATTEMPTS {
            assert_eq!(budget.try_record_attempt().unwrap(), expected);
        }

        assert!(budget.is_exhausted());
        assert!(budget.try_record_attempt().is_err());
        assert_eq!(budget.attempts(), MAX_MFA_PUSH_ATTEMPTS);
    }

    #[test]
    fn mfa_attempt_budget_is_shared_and_can_be_reset() {
        let budget = MfaAttemptBudget::default();
        let shared = budget.clone();

        budget.try_record_attempt().unwrap();
        assert_eq!(shared.attempts(), 1);

        shared.reset();
        assert_eq!(budget.attempts(), 0);
        assert!(!budget.is_exhausted());
    }
}

/// Check the "Don't ask again for 14 days" checkbox on the Azure AD MFA
/// approval screen, if it is visible and not yet checked.
///
/// The element `#idChkBx_SAOTCAS_TD` (name `rememberMFA`) appears on the
/// Authenticator push-approval page alongside the number-match display.
/// Ticking it suppresses the MFA prompt for 14 days on this browser profile.
/// The checkbox is Knockout.js-bound; `.click()` dispatches a native click
/// event which KO's `checked` binding handles correctly.
///
/// Returns `true` when the element was found (regardless of prior state).
async fn try_mfa_remember_device(page: &chromiumoxide::Page) -> bool {
    let script = r#"(() => {
        const cb = document.querySelector('#idChkBx_SAOTCAS_TD');
        if (!cb) return 'none';
        if (cb.checked) return 'already-checked';
        if (cb.disabled) return 'disabled';
        cb.click();
        return 'clicked';
    })()"#;
    match page.evaluate(script).await {
        Ok(val) => {
            let result = val.into_value::<String>().unwrap_or_else(|_| "<parse-error>".into());
            if result == "clicked" {
                info!("MFA: checked \"don't ask again for 14 days\" on approval screen");
            }
            result != "none"
        }
        Err(e) => {
            debug!(error = %e, "try_mfa_remember_device evaluate failed");
            false
        }
    }
}

/// Best-effort port of `_try_microsoft_login_steps` from the old Python
/// fetcher. Selectors target the standard Azure AD login form (`#i0116` =
/// username/email, `#i0118` = password, `#idSIButton9` = primary submit)
/// and the account-picker screen that Azure AD shows when it already
/// remembers the user (`Pick an account`).
async fn try_microsoft_login(page: &chromiumoxide::Page, username: &str, password: &str) {
    // Detect which page we're on to help diagnose auto-fill issues.
    let page_state = detect_login_page(page).await;
    debug!(state = %page_state, username, "try_microsoft_login");

    // Account picker ("Pick an account"). Shown when SSO remembers the
    // user from a previous session in the persistent profile. The tile
    // for `username` must be clicked before the password form appears.
    if try_account_picker(page, username).await {
        debug!("clicked account-picker tile; waiting for next page");
        return;
    }

    // Username field — only submit if we actually typed the username.
    // Return early so the page has time to navigate to the password
    // step before the next poll iteration.
    if type_into(page, "#i0116", username, false).await.is_ok() {
        debug!("typed username into login form");
        let _ = click_once(page, "#idSIButton9").await;
        return;
    }

    // Password field — only reached once the username step is done.
    if type_into(page, "#i0118", password, true).await.is_ok() {
        debug!("typed password into login form");
        let _ = click_once(page, "#idSIButton9").await;
    }
}

/// Click the account-picker tile matching `username`, if the picker is
/// currently visible. No-op on any other page.
///
/// Azure AD renders each remembered account as a tile under
/// `#tilesHolder`. The tile's `data-test-id` is set to the account's
/// user-principal-name (e.g. `user@example.com`); we also fall back to
/// matching any tile whose visible text contains the username, in case
/// Microsoft changes the attribute name again.
/// Detect which login page we're currently on (for diagnostics).
async fn detect_login_page(page: &chromiumoxide::Page) -> String {
    let script = r#"(() => {
        const url = location.href;
        if (document.querySelector('#tilesHolder'))  return 'account-picker (url=' + url + ')';
        if (document.querySelector('#i0116'))         return 'username (url=' + url + ')';
        if (document.querySelector('#i0118'))         return 'password (url=' + url + ')';
        if (document.querySelector('#idDiv_SAASDS_Title')) return 'mfa-picker (url=' + url + ')';
        if (document.querySelector('#idRichContext_DisplaySign')) return 'mfa-number (url=' + url + ')';
        return 'unknown (url=' + url + ')';
    })()"#;
    match page.evaluate(script).await {
        Ok(val) => val.into_value::<String>().unwrap_or_else(|_| "<eval-parse-error>".into()),
        Err(e) => format!("<eval-error: {e}>"),
    }
}

/// Returns `true` if a tile was clicked, `false` otherwise.
async fn try_account_picker(page: &chromiumoxide::Page, username: &str) -> bool {
    // JSON-encode the username so embedded quotes / backslashes can't
    // break out of the JS string literal.
    let user_json = serde_json_encode(username);
    let script = format!(
        r#"(() => {{
            const user = {user_json};
            const lower = user.toLowerCase();
            const direct = document.querySelector(
                `[data-test-id="${{user}}"]`
            );
            if (direct) {{ direct.click(); return 'data-test-id'; }}
            const tiles = document.querySelectorAll(
                '#tilesHolder [role="button"]'
            );
            for (const t of tiles) {{
                const txt = (t.textContent || '').toLowerCase();
                const aria = (t.getAttribute('aria-label') || '').toLowerCase();
                if (txt.includes(lower) || aria.includes(lower)) {{
                    t.click();
                    return 'text-match';
                }}
            }}
            return 'none (tiles=' + tiles.length + ')';
        }})()"#
    );
    match page.evaluate(script).await {
        Ok(val) => {
            let result = val.into_value::<String>().unwrap_or_else(|_| "<parse-error>".into());
            let clicked = !result.starts_with("none");
            debug!(result, clicked, "try_account_picker");
            clicked
        }
        Err(e) => {
            debug!(error = %e, "try_account_picker evaluate failed");
            false
        }
    }
}

/// Minimal JSON string encoder for embedding `s` inside a JS literal.
/// Avoids pulling in `serde_json` just for this one call site.
fn serde_json_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Type `value` into the first element matching `selector` if visible.
/// When `clear_first` is true, the field is emptied before typing.
async fn type_into(page: &chromiumoxide::Page, selector: &str, value: &str, clear_first: bool) -> Result<()> {
    let element = page
        .find_element(selector)
        .await
        .map_err(|e| anyhow!("selector {selector} not found: {e}"))?;
    if clear_first {
        let _ = element.click().await;
        // Select-all + delete is the most reliable cross-browser clear.
        let _ = page
            .evaluate(format!("document.querySelector({sel:?}).value = ''", sel = selector))
            .await;
    }
    let _ = element.click().await;
    element
        .type_str(value)
        .await
        .map_err(|e| anyhow!("typing into {selector} failed: {e}"))?;
    Ok(())
}

async fn click_once(page: &chromiumoxide::Page, selector: &str) -> Result<()> {
    let element = page
        .find_element(selector)
        .await
        .map_err(|e| anyhow!("selector {selector} not found: {e}"))?;
    element.click().await.map_err(|e| anyhow!("clicking {selector} failed: {e}"))?;
    Ok(())
}
