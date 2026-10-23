//! Read-only access to a release's bundled assets.
//!
//! When a service declares `assets = [...]` in `shiku.toml`, the CLI ships
//! those files inside the release (`releases/<sha>/assets/…`) and the agent
//! exports their location as `SHIKU_ASSETS_DIR` (= `~/apps/<app>/current/assets`).
//! Read them through [`asset_path`] rather than a hardcoded path, so the
//! service always reads from the *active* release.
//!
//! The base directory is resolved **once** at first use — following the
//! `current` symlink to its concrete `releases/<sha>/assets` — so a long-running
//! process stays pinned to its own release's assets even if `current` later
//! swaps under it during a deploy.

use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use crate::{RuntimeError, RuntimeResult};

/// Env var the agent sets to the active release's `assets/` directory.
pub const ASSETS_DIR_ENV: &str = "SHIKU_ASSETS_DIR";

/// Resolve and cache the assets base directory from `$SHIKU_ASSETS_DIR`.
/// `None` when the var is unset (no assets declared / not under shikud).
fn cached_base() -> Option<&'static Path> {
    static BASE: OnceLock<Option<PathBuf>> = OnceLock::new();
    BASE.get_or_init(|| {
        let raw = PathBuf::from(std::env::var_os(ASSETS_DIR_ENV)?);
        // Resolve `current -> releases/<sha>/assets` once. Fall back to the raw
        // path if it doesn't exist yet (asset_path then fails on the file).
        Some(std::fs::canonicalize(&raw).unwrap_or(raw))
    })
    .as_deref()
}

/// The active release's assets directory, resolved once at first use.
pub fn asset_dir() -> RuntimeResult<&'static Path> {
    cached_base().ok_or_else(|| RuntimeError::Env {
        name: ASSETS_DIR_ENV.to_string(),
        detail: "not set; is this running under shikud with `assets` declared in shiku.toml?"
            .to_string(),
    })
}

/// Resolve a release-relative asset path, verifying it exists and stays within
/// the assets directory.
///
/// `rel` must be a relative path with no `..` or root component. An empty `rel`
/// (or `"."`) returns the assets directory itself. A missing file yields
/// [`RuntimeError::MissingAsset`] — whose message points at the likely cause,
/// an asset not declared under `assets` in `shiku.toml`.
pub fn asset_path(rel: &str) -> RuntimeResult<PathBuf> {
    asset_path_in(asset_dir()?, rel)
}

/// The path-confinement + existence logic, factored out so it's testable
/// without the process-global `$SHIKU_ASSETS_DIR` / `OnceLock`.
fn asset_path_in(base: &Path, rel: &str) -> RuntimeResult<PathBuf> {
    let rel_path = Path::new(rel);
    let confined = rel_path
        .components()
        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    if rel_path.is_absolute() || !confined {
        return Err(RuntimeError::Asset {
            path: rel.to_string(),
            detail: "must be a relative path with no `..` or root".to_string(),
        });
    }
    let resolved = std::fs::canonicalize(base.join(rel_path)).map_err(|_| {
        RuntimeError::MissingAsset {
            path: rel.to_string(),
        }
    })?;
    // Defence in depth against symlinks inside the asset tree pointing out.
    if !resolved.starts_with(base) {
        return Err(RuntimeError::Asset {
            path: rel.to_string(),
            detail: "resolves outside the asset directory".to_string(),
        });
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "shiku-assets-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("projects")).unwrap();
        std::fs::write(dir.join("projects/config.toml"), b"x").unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    #[test]
    fn resolves_declared_file() {
        let base = scratch();
        let p = asset_path_in(&base, "projects/config.toml").expect("present");
        assert!(p.ends_with("config.toml"));
        assert!(p.starts_with(&base));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn empty_returns_base() {
        let base = scratch();
        assert_eq!(asset_path_in(&base, "").expect("base"), base);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn missing_file_errors() {
        let base = scratch();
        let err = asset_path_in(&base, "projects/nope.toml").unwrap_err();
        assert!(matches!(err, RuntimeError::MissingAsset { .. }));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn parent_escape_rejected() {
        let base = scratch();
        let err = asset_path_in(&base, "../secrets").unwrap_err();
        assert!(matches!(err, RuntimeError::Asset { .. }));
        let _ = std::fs::remove_dir_all(&base);
    }
}
