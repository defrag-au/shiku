//! Service-binding lookup.
//!
//! At activation time, `shikud` injects `SHIKU_BIND_<NAME>=<addr>` env vars
//! for every entry in the app's `bindings` list. This module provides typed
//! lookup helpers so application code reads:
//!
//! ```ignore
//! let narrator = shiku_runtime::bindings::require("narrator")?;
//! // narrator: String, e.g. "http://127.0.0.1:9090"
//! ```
//!
//! …rather than `std::env::var("SHIKU_BIND_NARRATOR")` directly. The
//! difference is small but pays off when bindings get more elaborate
//! (Unix sockets, cross-platform routes, resource bindings) — only this
//! module changes when the resolution mechanism evolves.

use crate::{RuntimeError, RuntimeResult};

/// Prefix the agent uses for binding env vars. Public so apps can grep for
/// it consistently.
pub const ENV_PREFIX: &str = "SHIKU_BIND_";

/// Look up a binding by its short name (e.g. `"narrator"` → reads
/// `SHIKU_BIND_NARRATOR`). Returns `None` if the env var isn't set.
///
/// Names are normalised to uppercase with `-` → `_`, matching the agent's
/// `bindings::to_env_suffix`.
pub fn lookup(name: &str) -> Option<String> {
    let env_name = format!("{}{}", ENV_PREFIX, normalise(name));
    std::env::var(env_name).ok()
}

/// Like [`lookup`], but returns an error if the binding wasn't injected.
/// Use this for required dependencies — if the agent didn't set it, the
/// app's deploy config is wrong.
pub fn require(name: &str) -> RuntimeResult<String> {
    lookup(name).ok_or_else(|| RuntimeError::MissingBinding {
        name: name.to_string(),
    })
}

/// Normalise a binding name to its env-var suffix form. Mirrors the agent's
/// transformation.
fn normalise(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '-' => '_',
            c => c.to_ascii_uppercase(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn normalises_names() {
        assert_eq!(normalise("narrator"), "NARRATOR");
        assert_eq!(normalise("norn-narrator"), "NORN_NARRATOR");
        assert_eq!(normalise("Eternal_Seas"), "ETERNAL_SEAS");
    }

    #[test]
    fn lookup_returns_none_when_unset() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::remove_var("SHIKU_BIND_DEFINITELY_UNSET_TEST");
        assert!(lookup("definitely-unset-test").is_none());
    }

    #[test]
    fn lookup_finds_set_value() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var("SHIKU_BIND_TEST_NARRATOR", "http://127.0.0.1:9090");
        assert_eq!(
            lookup("test-narrator"),
            Some("http://127.0.0.1:9090".to_string())
        );
        std::env::remove_var("SHIKU_BIND_TEST_NARRATOR");
    }

    #[test]
    fn require_errors_on_missing() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::remove_var("SHIKU_BIND_TEST_MISSING");
        let err = require("test-missing").unwrap_err();
        assert!(matches!(err, RuntimeError::MissingBinding { .. }));
    }
}
