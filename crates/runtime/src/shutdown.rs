//! Graceful shutdown helpers.
//!
//! systemd sends SIGTERM, waits `TimeoutStopSec` (default 90s), then sends
//! SIGKILL. Services that finish in-flight work and exit cleanly within
//! that window get smooth restarts; services that ignore SIGTERM get killed.
//!
//! Use [`signal`] to await either SIGTERM or SIGINT (Ctrl-C) — useful as
//! the trigger for an axum server's `.with_graceful_shutdown`.

use tokio::signal::unix::{signal, SignalKind};

/// Resolve the future on the first SIGTERM or SIGINT received. Panics if
/// the signal handlers fail to install, which only happens on broken kernels.
///
/// ```ignore
/// axum::serve(listener, app)
///     .with_graceful_shutdown(shiku_runtime::shutdown::signal())
///     .await?;
/// ```
pub async fn signal_received() {
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");

    tokio::select! {
        _ = term.recv() => tracing::info!("SIGTERM received, shutting down"),
        _ = int.recv() => tracing::info!("SIGINT received, shutting down"),
    }
}
