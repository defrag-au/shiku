//! App registry: per-app configuration persisted under `~/apps/<name>/`.
//!
//! On-disk layout (per the deploy doc):
//!
//! ```text
//! ~/apps/
//!   <app>/
//!     config.toml      <-- AppConfig in TOML form
//!     releases/        <-- created lazily; populated by Step 2.4
//! ```
//!
//! The agent treats `~/apps/<name>/` as the source of truth. `config.toml` is
//! written atomically (tempfile + rename) so a crash mid-write can't leave a
//! half-written file.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use shiku_types::{AppConfig, AppName};

/// Path to the directory holding all apps for this agent's user.
fn apps_root() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join("apps"))
}

fn app_dir(name: &str) -> Result<PathBuf> {
    Ok(apps_root()?.join(name))
}

fn config_path(name: &str) -> Result<PathBuf> {
    Ok(app_dir(name)?.join("config.toml"))
}

/// Validate that an app name is safe to use as a directory component.
///
/// We persist app configs under `~/apps/<name>/`, so a name containing path
/// separators or relative components could escape the apps directory. Reject
/// anything other than ASCII alphanumeric, dashes, and underscores.
fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(anyhow!("app name is empty"));
    }
    if name.len() > 64 {
        return Err(anyhow!("app name too long (max 64 chars)"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(anyhow!(
            "app name '{name}' contains invalid characters (allowed: a-z A-Z 0-9 - _)"
        ));
    }
    Ok(())
}

/// Register or update an app. If the app already exists, the config is
/// overwritten in place — re-registration is the way to update config without
/// a full app removal.
pub fn register(name: &AppName, config: &AppConfig) -> Result<()> {
    validate_name(name)?;
    if name != &config.name {
        return Err(anyhow!(
            "app name mismatch: registry key '{}' vs config.name '{}'",
            name,
            config.name
        ));
    }

    let dir = app_dir(name)?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::create_dir_all(dir.join("releases"))
        .with_context(|| format!("creating {}", dir.join("releases").display()))?;

    let path = config_path(name)?;
    let body = toml::to_string_pretty(config).context("serializing AppConfig as TOML")?;
    write_atomic(&path, body.as_bytes()).with_context(|| format!("writing {}", path.display()))?;

    tracing::info!(app = %name, dir = %dir.display(), "registered app");
    Ok(())
}

/// Remove an app entirely: deletes `~/apps/<name>/` and all releases.
///
/// Returns `Ok(false)` if the app wasn't registered (idempotent).
pub fn remove(name: &AppName) -> Result<bool> {
    validate_name(name)?;
    let dir = app_dir(name)?;
    if !dir.exists() {
        return Ok(false);
    }
    std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
    tracing::info!(app = %name, "removed app");
    Ok(true)
}

/// List registered app names. Empty `~/apps/` returns an empty Vec.
///
/// Includes the `__shiku__` pseudo-app — internal callers that need to
/// iterate every registered config (e.g. ingress rendering) want to see
/// it for filtering. The CLI-facing `Apps` response filters it out at
/// the dispatch layer so users don't see internal apps.
pub fn list() -> Result<Vec<AppName>> {
    let root = apps_root()?;
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in std::fs::read_dir(&root).with_context(|| format!("reading {}", root.display()))? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry
            .file_name()
            .to_str()
            .ok_or_else(|| anyhow!("non-UTF8 entry in apps dir"))?
            .to_string();
        // Only include directories that actually have a config.toml — guards
        // against stray dirs that aren't fully-registered apps.
        if entry.path().join("config.toml").is_file() {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

/// User-facing app list — same as [`list`] but with the `__shiku__`
/// pseudo-app filtered out. Used by `Request::AppList` and `Status`.
pub fn list_user_visible() -> Result<Vec<AppName>> {
    Ok(list()?.into_iter().filter(|n| !is_pseudo(n)).collect())
}

/// Load an app's config.
pub fn inspect(name: &AppName) -> Result<AppConfig> {
    validate_name(name)?;
    let path = config_path(name)?;
    if !path.is_file() {
        return Err(anyhow!("app '{name}' not registered"));
    }
    let body =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let config: AppConfig =
        toml::from_str(&body).with_context(|| format!("parsing {}", path.display()))?;
    Ok(config)
}

/// Whether an app is registered.
#[allow(dead_code)] // used by Step 2.4 (release upload)
pub fn is_registered(name: &AppName) -> Result<bool> {
    validate_name(name)?;
    Ok(config_path(name)?.is_file())
}

/// Reserved name for Shiku's internal pseudo-app. The pseudo-app holds
/// platform-level secrets (CF API token, cloudflared tunnel token) that
/// aren't tied to any user-visible service. It reuses the apps registry
/// so the existing secrets/env-file machinery applies, but is filtered
/// from `shiku app list`, `shiku status`, and ingress rendering.
pub const PSEUDO_APP_NAME: &str = "__shiku__";

/// Whether `name` is the reserved pseudo-app.
pub fn is_pseudo(name: &str) -> bool {
    name == PSEUDO_APP_NAME
}

/// Atomic write: write to a sibling tempfile, then rename. The rename is
/// atomic on Linux for paths within the same filesystem.
fn write_atomic(path: &Path, body: &[u8]) -> Result<()> {
    use std::io::Write;

    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("path has no parent: {}", path.display()))?;

    // tempfile crate would be nicer, but a manual implementation keeps the
    // dependency surface honest for this small piece. The agent already
    // depends on tempfile for the secrets layer (Step 3.3); switch then if it
    // simplifies things.
    let tmp_name = format!(
        ".{}.tmp.{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("config"),
        std::process::id()
    );
    let tmp_path = parent.join(tmp_name);

    {
        let mut f = std::fs::File::create(&tmp_path)
            .with_context(|| format!("creating tempfile {}", tmp_path.display()))?;
        f.write_all(body)
            .with_context(|| format!("writing tempfile {}", tmp_path.display()))?;
        f.sync_all().context("fsync tempfile")?;
    }

    std::fs::rename(&tmp_path, path)
        .with_context(|| format!("renaming {} -> {}", tmp_path.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_name() {
        assert!(validate_name("eternal-seas").is_ok());
        assert!(validate_name("norn-server").is_ok());
        assert!(validate_name("App_1").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("a/b").is_err());
        assert!(validate_name("..").is_err());
        assert!(validate_name(".hidden").is_err());
        assert!(validate_name("with space").is_err());
    }
}
