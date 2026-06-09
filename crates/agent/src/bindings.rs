//! Service-binding resolution.
//!
//! When an app declares `bindings = ["narrator", "ownership"]` in
//! `shiku.toml`, the agent injects `SHIKU_BIND_NARRATOR=http://127.0.0.1:9090`
//! and `SHIKU_BIND_OWNERSHIP=...` env vars at activation time. Each binding
//! resolves to the listen address of another registered app (per-user, per-box).
//!
//! Application code reads these via `std::env::var("SHIKU_BIND_NARRATOR")`,
//! or via the `shiku-runtime` helper trait once that crate exists (Step 4.1).
//!
//! v1 only supports localhost-port bindings (apps with `[listen.http]`).
//! Future:
//!   - Unix-socket bindings for same-box service-to-service traffic.
//!   - Cross-platform bindings (CF Tunnel back-routes to Workers).
//!   - Resource bindings (R2, Postgres) via opaque connection strings.

use anyhow::{anyhow, Result};
use shiku_types::AppName;

use crate::apps;

/// Resolve all of an app's bindings into `(env_name, value)` pairs ready to
/// inject into the env file. Errors fast on any unresolved binding rather
/// than silently dropping it — a missing dependency at activation time is a
/// deploy bug worth surfacing.
pub fn resolve_for_app(app_bindings: &[AppName]) -> Result<Vec<(String, String)>> {
    let mut out = Vec::with_capacity(app_bindings.len());
    for target_name in app_bindings {
        let env_name = format!("SHIKU_BIND_{}", to_env_suffix(target_name));
        let value = resolve_one(target_name)?;
        out.push((env_name, value));
    }
    Ok(out)
}

/// Look up one bound app's listen address.
///
/// The dependency must be registered with an HTTP port (which the agent
/// allocates on registration). Whether it's currently *running* isn't our
/// concern — bindings give an address; whether the address answers is the
/// consumer's problem.
fn resolve_one(target: &AppName) -> Result<String> {
    let target_config = apps::inspect(target)
        .map_err(|e| anyhow!("binding target '{target}' not registered: {e}"))?;
    let port = target_config
        .listen
        .as_ref()
        .filter(|l| l.http)
        .and_then(|l| l.http_port)
        .ok_or_else(|| {
            anyhow!(
                "binding target '{target}' has no allocated http port; \
                 only HTTP-listening apps can be referenced via bindings"
            )
        })?;
    Ok(format!("http://127.0.0.1:{port}"))
}

/// Convert a binding's app name into the `SHIKU_BIND_<NAME>` env-var suffix.
///
/// App names allow `[a-zA-Z0-9_-]`; env vars don't allow `-`, so we transform
/// dashes to underscores and uppercase. `norn-narrator` → `NORN_NARRATOR`.
fn to_env_suffix(app: &str) -> String {
    app.chars()
        .map(|c| match c {
            '-' => '_',
            c => c.to_ascii_uppercase(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_suffix_uppercases_and_replaces_dashes() {
        assert_eq!(to_env_suffix("narrator"), "NARRATOR");
        assert_eq!(to_env_suffix("norn-narrator"), "NORN_NARRATOR");
        assert_eq!(to_env_suffix("Eternal_Seas"), "ETERNAL_SEAS");
    }
}
