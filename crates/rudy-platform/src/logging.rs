//! Debug logging for both binaries.
//!
//! Rudy has two output channels and they are not the same thing. *Output* is
//! what the user reads — the CLI's tables, the GUI's banner. *Logs* are what a
//! maintainer reads off a bug report. This module is only the second one.
//!
//! **Logs always go to stderr, in every binary.** The CLI's stdout carries
//! tables and `verify --json`, and a caller parsing those would read a log line
//! as data, so the writer is not configurable.

use tracing::Level;
use tracing_subscriber::EnvFilter;

/// Installs the process-wide subscriber, writing to stderr.
///
/// `RUST_LOG` wins when it is set and parses; [`default_filter`] applies when it
/// is not. Per-target filtering is the reason this uses `EnvFilter` rather than a
/// plain level — `RUST_LOG=rudy_platform::authorized_target=debug` is what makes a
/// log useful when the question is about one seam.
///
/// Never fatal and safe to call twice: the second call is a no-op. A process
/// part-way through writing a partition table must not die because logging could
/// not be set up.
pub fn init(default: Level) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| default_filter(default));

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

/// The filter a binary runs with when `RUST_LOG` is not set: `default` for
/// Rudy's own crates, `warn` for everything else.
///
/// Public so a test can ask what a user's log actually keeps at a binary's
/// default level. A bare level would let through a line this drops — one logged
/// under a target outside Rudy's crates, say — and pass while the journal stayed
/// empty (AR-13).
pub fn default_filter(default: Level) -> EnvFilter {
    // Scoped to Rudy's own crates, with everything else held at `warn`.
    // `tracing-subscriber` installs `LogTracer`, so a bare level would capture
    // every `log::` record in the dependency graph — winit, wayland, calloop and
    // slint all emit them, and `rudy-gui`'s journal (the thing a bug report is
    // made of) would be mostly compositor chatter with the run buried in it.
    let level = default.to_string().to_lowercase();
    EnvFilter::new(format!(
        "warn,rudy={level},rudy_cli={level},rudy_core={level},\
         rudy_gui={level},rudy_platform={level}"
    ))
}
