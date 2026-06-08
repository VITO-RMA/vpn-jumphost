//! Linux sleep/wake watcher via logind's `PrepareForSleep` D-Bus signal.
//!
//! Subscribes (via `zbus`) to
//! `org.freedesktop.login1.Manager.PrepareForSleep` on the system bus. The
//! signal arrives with `True` right before suspend and `False` after
//! resume; we fire `on_resume` on the resume edge.

use std::sync::Arc;

use futures_util::StreamExt;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use zbus::{message::Type as MessageType, Connection, MatchRule, MessageStream};

use super::SleepWakeHandle;

pub fn spawn() -> Option<SleepWakeHandle> {
    let on_resume = Arc::new(Notify::new());
    let shutdown = CancellationToken::new();

    let resume_clone = Arc::clone(&on_resume);
    let cancel_clone = shutdown.clone();

    tokio::spawn(async move {
        if let Err(e) = run(resume_clone, cancel_clone).await {
            warn!(error = %e, "logind sleep/wake watcher exited with error");
        }
    });

    Some(SleepWakeHandle {
        on_resume,
        shutdown,
    })
}

async fn run(on_resume: Arc<Notify>, shutdown: CancellationToken) -> zbus::Result<()> {
    let conn = match Connection::system().await {
        Ok(c) => c,
        Err(e) => {
            info!(
                error = %e,
                "logind: cannot open system D-Bus; falling back to wall-clock"
            );
            return Ok(());
        }
    };

    let rule = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .interface("org.freedesktop.login1.Manager")?
        .member("PrepareForSleep")?
        .path("/org/freedesktop/login1")?
        .build();

    info!("subscribed to logind PrepareForSleep on the system D-Bus");

    let mut stream = match MessageStream::for_match_rule(rule, &conn, None).await {
        Ok(s) => s,
        Err(e) => {
            info!(error = %e, "logind: AddMatch failed; falling back to wall-clock");
            return Ok(());
        }
    };

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                debug!("logind watcher: shutdown requested");
                return Ok(());
            }
            msg = stream.next() => {
                let Some(msg) = msg else {
                    debug!("logind watcher: message stream ended");
                    return Ok(());
                };
                let msg = match msg {
                    Ok(m) => m,
                    Err(e) => {
                        debug!(error = %e, "logind watcher: receive error");
                        continue;
                    }
                };
                let about_to_sleep: bool = match msg.body().deserialize() {
                    Ok(v) => v,
                    Err(e) => {
                        debug!(error = %e, "logind watcher: could not parse PrepareForSleep body");
                        continue;
                    }
                };
                if about_to_sleep {
                    info!("logind: system is about to sleep");
                } else {
                    info!("logind: system resumed from sleep");
                    on_resume.notify_one();
                }
            }
        }
    }
}
