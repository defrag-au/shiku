//! Secret-env-var lookup that returns [`SecretString`] rather than `String`.
//!
//! Why bother with a wrapper:
//!
//! - `SecretString` doesn't implement `Display` / `Debug` for the value, so
//!   accidentally logging it is a compile error rather than a runtime leak.
//! - Drop-zeroizes, so plaintext doesn't linger in memory.
//! - Forces apps to call `.expose_secret()` deliberately when they actually
//!   need the bytes — a small but real ergonomic nudge towards careful
//!   handling.
//!
//! Apps that take a `SecretString` parameter rather than `String` make their
//! "this is a secret" intent visible in the type signature.

use secrecy::SecretString;

use crate::{RuntimeError, RuntimeResult};

/// Look up a secret by name. Returns `None` if the env var isn't set.
pub fn lookup(name: &str) -> Option<SecretString> {
    std::env::var(name)
        .ok()
        .map(|s| SecretString::new(s.into()))
}

/// Like [`lookup`], but errors when missing. Use for required secrets — the
/// service should fail fast on startup rather than try to operate without
/// credentials.
pub fn require(name: &str) -> RuntimeResult<SecretString> {
    lookup(name).ok_or_else(|| RuntimeError::MissingSecret {
        name: name.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn lookup_unset_returns_none() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::remove_var("SHIKU_RUNTIME_SECRET_TEST_UNSET");
        assert!(lookup("SHIKU_RUNTIME_SECRET_TEST_UNSET").is_none());
    }

    #[test]
    fn lookup_returns_secret() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var("SHIKU_RUNTIME_SECRET_TEST_SET", "abc123");
        let s = lookup("SHIKU_RUNTIME_SECRET_TEST_SET").expect("present");
        assert_eq!(s.expose_secret(), "abc123");
        std::env::remove_var("SHIKU_RUNTIME_SECRET_TEST_SET");
    }

    #[test]
    fn require_errors_on_missing() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::remove_var("SHIKU_RUNTIME_SECRET_REQUIRE_MISSING");
        let err = require("SHIKU_RUNTIME_SECRET_REQUIRE_MISSING").unwrap_err();
        assert!(matches!(err, RuntimeError::MissingSecret { .. }));
    }
}
