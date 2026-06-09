//! Opinionated tracing setup for Shiku-deployed services.
//!
//! Defaults:
//! - JSON output to stdout (journald captures it via systemd's
//!   `StandardOutput=journal`).
//! - Log level controlled by `<APP>_LOG` env var, falling back to `info`.
//!
//! Apps that want different formatting (e.g. compact for local dev) can
//! skip [`init_default`] and configure their own subscriber. The runtime
//! crate doesn't enforce a particular subscriber — these are conveniences.

use tracing_subscriber::EnvFilter;

/// Initialise the default tracing subscriber: JSON to stdout, env-filter
/// based on `RUST_LOG` (or fallback to `info`).
///
/// Idempotent in the sense of: if a subscriber is already installed, this
/// will silently return. Set `RUST_LOG=trace` for debugging.
pub fn init_default() {
    init_with_filter_env("RUST_LOG");
}

/// Initialise tracing using a specific env var as the level filter.
///
/// Useful for apps that want their own namespaced log knob (e.g. `NORN_LOG`
/// instead of `RUST_LOG`).
pub fn init_with_filter_env(env_var: &str) {
    let filter = EnvFilter::try_from_env(env_var).unwrap_or_else(|_| EnvFilter::new("info"));

    // Use JSON formatter when running under systemd / shiku; pretty when
    // attached to a terminal (cargo run during development). The detection
    // is intentionally simple — we honour `SHIKU_LOG_FORMAT=pretty|json`
    // first, then fall back to "json if stdout isn't a tty".
    let format = std::env::var("SHIKU_LOG_FORMAT").ok();
    let want_json = match format.as_deref() {
        Some("pretty") => false,
        Some("json") => true,
        _ => !is_terminal_stdout(),
    };

    if want_json {
        let _ = tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .with_target(true)
            .try_init();
    } else {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .compact()
            .try_init();
    }
}

/// Detect whether stdout is connected to a terminal. Returns `true` when run
/// under `cargo run` (TTY), `false` under systemd (pipe to journald).
fn is_terminal_stdout() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal()
}
