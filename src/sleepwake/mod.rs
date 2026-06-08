//! OS-native sleep/wake event sources.
//!
//! Replaces the Python `jeepney` (Linux) and `PyObjC` (macOS) watchers in
//! the old `scripts/jumphost.py`. The supervisor uses these to react to
//! suspend/resume immediately instead of waiting for the next wall-clock
//! poll iteration. The wall-clock skew heuristic in
//! [`crate::jumphost`] still runs as a portable fallback.
//!
//! Each platform implementation provides an async [`spawn`] function that
//! returns a [`SleepWakeHandle`]. Cancelling the handle (drop or `cancel`)
//! stops the watcher.

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "macos")]
mod macos;

/// Handle for a running sleep/wake watcher.
pub struct SleepWakeHandle {
    /// Fired once each time the OS reports a resume from suspend.
    pub on_resume: std::sync::Arc<Notify>,
    /// Cancel the underlying watcher task.
    pub shutdown: CancellationToken,
}

impl SleepWakeHandle {
    pub fn cancel(&self) {
        self.shutdown.cancel();
    }
}

/// Try to spawn a platform-native sleep/wake watcher.
///
/// Returns `None` on unsupported platforms or when the OS event source is
/// not available (e.g. no D-Bus session bus on Linux). The caller should
/// rely on the wall-clock skew fallback in that case.
pub fn spawn() -> Option<SleepWakeHandle> {
    #[cfg(target_os = "linux")]
    {
        linux::spawn()
    }
    #[cfg(target_os = "macos")]
    {
        macos::spawn()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        tracing::info!(
            "no native sleep/wake watcher implemented for this platform; \
             relying on wall-clock fallback"
        );
        None
    }
}
