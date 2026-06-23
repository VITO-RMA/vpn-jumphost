//! Tracing-subscriber bootstrap with TTY-aware coloring.
//!
//! Mirrors the behavior of `scripts/jumphost.py`'s `setup_logging`:
//!   - Plain timestamped log lines on stderr.
//!   - ANSI colors when stderr is a TTY (and `NO_COLOR` is unset).
//!   - `FORCE_COLOR` overrides the TTY check.
//!   - `RUST_LOG` (if set) overrides the verbosity flag.
//!
//! The default filter silences `chromiumoxide` below ERROR. The crate
//! emits `WS Invalid message: data did not match any variant of
//! untagged enum Message` warnings whenever Chromium sends a CDP event
//! whose schema is newer than the bundled protocol definitions —
//! harmless noise we can't act on. Users who actually want to see CDP
//! traffic can opt back in with `RUST_LOG=chromiumoxide=debug,...`.

use std::env;
use std::io::IsTerminal;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::time::ChronoLocal;

/// Initialize the global tracing subscriber. Safe to call exactly once at
/// process start. Subsequent calls are silently ignored.
pub fn init(verbose: bool) {
    let default_filter = if verbose {
        "debug,chromiumoxide=error"
    } else {
        "info,chromiumoxide=error"
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));

    let use_color = use_color();

    // ChronoLocal with a journald/console-friendly format.
    // Equivalent to Python's "%(asctime)s %(levelname)-7s %(name)s: %(message)s".
    let timer = ChronoLocal::new("%Y-%m-%d %H:%M:%S%.3f".to_string());

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(true)
        .with_ansi(use_color)
        .with_timer(timer)
        .try_init();
}

pub fn use_color() -> bool {
    if env::var_os("NO_COLOR").is_some() {
        return false;
    }
    match env::var("FORCE_COLOR") {
        Ok(v) if !v.is_empty() && v != "0" => return true,
        _ => {}
    }
    std::io::stderr().is_terminal()
}
