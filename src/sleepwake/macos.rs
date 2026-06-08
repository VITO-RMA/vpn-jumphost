//! macOS sleep/wake watcher via `NSWorkspaceDidWakeNotification`.
//!
//! NSWorkspace's notification center delivers wake notifications only to
//! threads with a running `NSRunLoop`. We spawn a dedicated OS thread that
//! installs an observer and spins the runloop in short increments,
//! checking `shutdown` between iterations.
//!
//! Note: even if this watcher fails to start, the supervisor's wall-clock
//! skew fallback still catches suspend/resume — it's just a bit slower to
//! react (one short poll interval instead of immediately).

use std::ptr::NonNull;
use std::sync::Arc;
use std::time::Duration;

use objc2::rc::{autoreleasepool, Retained};
use objc2_app_kit::NSWorkspace;
use objc2_foundation::{
    NSDate, NSDefaultRunLoopMode, NSNotification, NSOperationQueue, NSRunLoop, NSString,
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use super::SleepWakeHandle;

pub fn spawn() -> Option<SleepWakeHandle> {
    let on_resume = Arc::new(Notify::new());
    let shutdown = CancellationToken::new();

    let resume_clone = Arc::clone(&on_resume);
    let cancel_clone = shutdown.clone();

    std::thread::Builder::new()
        .name("nsworkspace-sleepwake-watcher".to_string())
        .spawn(move || run(resume_clone, cancel_clone))
        .ok()?;

    info!("subscribed to NSWorkspaceDidWakeNotification (macOS)");

    Some(SleepWakeHandle {
        on_resume,
        shutdown,
    })
}

fn run(on_resume: Arc<Notify>, shutdown: CancellationToken) {
    autoreleasepool(|_| {
        // Capture a clone of `on_resume` inside the block so the observer
        // can fire it from arbitrary threads.
        let resume_for_block = Arc::clone(&on_resume);
        let block = block2::RcBlock::new(move |_notif: NonNull<NSNotification>| {
            debug!("NSWorkspaceDidWake received");
            resume_for_block.notify_one();
        });

        let workspace = unsafe { NSWorkspace::sharedWorkspace() };
        let nc = unsafe { workspace.notificationCenter() };
        let name = NSString::from_str("NSWorkspaceDidWakeNotification");
        let queue: Option<&NSOperationQueue> = None;

        // `addObserverForName:object:queue:usingBlock:` returns an opaque
        // token we must retain so the observer stays registered.
        let observer: Retained<objc2_foundation::NSObject> = unsafe {
            nc.addObserverForName_object_queue_usingBlock(Some(&name), None, queue, &block)
        };

        // Run the runloop in 500 ms slices so we can check shutdown.
        let run_loop = unsafe { NSRunLoop::currentRunLoop() };
        let mode = unsafe { NSDefaultRunLoopMode };
        while !shutdown.is_cancelled() {
            let until = unsafe { NSDate::dateWithTimeIntervalSinceNow(0.5) };
            unsafe {
                run_loop.runMode_beforeDate(mode, &until);
            }
        }

        // Best-effort cleanup before leaving the autorelease pool.
        unsafe {
            nc.removeObserver(&observer);
        }
        let _ = observer; // silence unused warning
        let _ = Duration::ZERO; // silence unused-import warning when feature flags shift
    });
}
