//! `shiku-runtime` — the service-side half of the Shiku deploy contract.
//!
//! Anything `shiku` deploys (and `shikud` activates) is expected to follow a
//! few conventions:
//!
//! - Read its listen address from `SHIKU_LISTEN_HTTP` (set by the agent from
//!   `[apps.X.listen.http]`).
//! - Read service dependencies from `SHIKU_BIND_<NAME>` env vars.
//! - Read secrets from env vars whose names match the app's allowlist.
//! - Expose a `/api/health` endpoint that returns 2xx when ready.
//! - Log structured JSON to stdout (captured by journald via systemd).
//! - Handle SIGTERM cleanly within ~10s.
//!
//! This crate provides helpers to make conforming cheap. A new service that
//! uses [`init`] for tracing and [`Bindings`] / [`Secrets`] / [`listen_addr`]
//! for env-var resolution gets the contract right by construction.
//!
//! ## Layering
//!
//! - [`bindings`] — typed lookup of `SHIKU_BIND_<NAME>` values.
//! - [`secrets`] — typed lookup of secret env vars (returns `SecretString`).
//! - [`listen`] — parse `SHIKU_LISTEN_HTTP` into a `SocketAddr`.
//! - [`tracing`] — opinionated `tracing-subscriber` setup.
//! - [`health`] (axum feature) — drop-in `Router` that exposes `/api/health`.
//! - [`shutdown`] (axum feature) — graceful shutdown helper around SIGTERM/SIGINT.
//!
//! Apps that don't use axum (CLI tools, gateway bots) can `default-features = false`
//! to drop the axum/tokio dependency.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod bindings;
pub mod listen;
pub mod manifest;
pub mod secrets;
pub mod tracing;

#[cfg(feature = "axum")]
pub mod health;

#[cfg(feature = "axum")]
pub mod shutdown;

pub use manifest::{Manifest, Need, SHIKU_NEEDS};

// Re-export hidden support module for macro expansions. The `shiku-macros`
// crate writes paths like `::shiku_runtime::__macro_support::linkme`.
#[doc(hidden)]
pub use manifest::__macro_support;

// Re-export the macros from `shiku-macros` so service code sees them under
// the same path as the runtime helpers (`shiku_runtime::secret!`, etc).
pub use shiku_macros::{binding, listen_http, secret};

/// Convenience re-export so apps don't need to depend on `shiku-types` directly
/// for the basic types.
pub use shiku_types::{AppName, ReleaseSha};

/// Top-level error type for runtime helpers. Each module typically wraps this
/// with a more specific kind via `thiserror`.
#[derive(Debug, ::thiserror::Error)]
pub enum RuntimeError {
    /// Required env var was not set or contained an invalid value.
    #[error("env var '{name}': {detail}")]
    Env {
        /// The env var name that was being read.
        name: String,
        /// Human-readable explanation.
        detail: String,
    },

    /// The agent didn't inject a binding the app expected.
    #[error("missing binding: SHIKU_BIND_{name} not set")]
    MissingBinding {
        /// The binding name (without the `SHIKU_BIND_` prefix).
        name: String,
    },

    /// A required secret was not present in the environment.
    #[error("missing secret: env var '{name}' not set")]
    MissingSecret {
        /// The secret name (matches the env var name).
        name: String,
    },
}

/// Convenience alias.
pub type RuntimeResult<T> = Result<T, RuntimeError>;

/// Initialise tracing, then return — the caller owns the runtime topology.
///
/// Equivalent to calling [`tracing::init_default`].
pub fn init_tracing() {
    tracing::init_default();
}

/// Top-of-`main` setup. Call this as the very first line of your `main`
/// before any logging or network setup:
///
/// ```ignore
/// #[tokio::main]
/// async fn main() -> anyhow::Result<()> {
///     shiku_runtime::init();
///     // ...
/// }
/// ```
///
/// What it does:
///   - If invoked with `--shiku-manifest`, prints the manifest as JSON and
///     exits 0. The agent calls this once per release upload to learn what
///     env vars / ports / bindings the binary needs.
///   - Otherwise, sets up tracing and returns. Your main continues normally.
pub fn init() {
    manifest::handle_manifest_flag();
    tracing::init_default();
}
