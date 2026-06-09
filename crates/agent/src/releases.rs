//! Release verification and persistence.
//!
//! When the CLI uploads a binary via rsync to
//! `~/apps/<app>/releases/<sha>/bin/<command>` and then sends
//! `Request::ReleaseUpload { app, sha, manifest }`, the agent:
//!
//!   1. Confirms the app is registered.
//!   2. Confirms the staged binary exists at the expected path.
//!   3. Recomputes SHA-256 of the staged binary and matches against
//!      `manifest.sha`.
//!   4. Writes `manifest.toml` next to the binary.
//!
//! Activation (Step 2.5) re-reads the manifest and uses it to drive the
//! systemd unit + env file generation.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use shiku_types::{AppName, ReleaseInfo, ReleaseManifest, ReleaseSha};
use tokio::process::Command as TokioCommand;

use crate::apps;

fn release_dir(app: &str, sha: &str) -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home)
        .join("apps")
        .join(app)
        .join("releases")
        .join(sha))
}

/// Validate a release sha is safe to use as a directory component. Hex of
/// SHA-256 is 64 lowercase chars; we accept that with a length window in
/// case we ever shorten or extend.
fn validate_sha(sha: &str) -> Result<()> {
    if sha.is_empty() {
        return Err(anyhow!("release sha is empty"));
    }
    if sha.len() > 128 {
        return Err(anyhow!("release sha too long"));
    }
    if !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(anyhow!("release sha must be hex (got '{sha}')"));
    }
    Ok(())
}

/// Verify and finalize a release that the CLI just rsync'd.
pub fn verify_and_finalize(
    app: &AppName,
    sha: &ReleaseSha,
    manifest: &ReleaseManifest,
) -> Result<()> {
    validate_sha(sha)?;
    if sha != &manifest.sha {
        return Err(anyhow!(
            "manifest sha mismatch: request says '{sha}', manifest says '{}'",
            manifest.sha
        ));
    }

    // App must be registered so we know its `command` (binary name) and
    // can later activate it.
    let app_config = apps::inspect(app).context("app not registered")?;

    let dir = release_dir(app, sha)?;
    if !dir.is_dir() {
        return Err(anyhow!(
            "release dir missing — was the binary rsync'd to {}?",
            dir.display()
        ));
    }

    let binary_path = dir.join("bin").join(&app_config.command);
    if !binary_path.is_file() {
        return Err(anyhow!(
            "binary missing at {} — rsync target mismatch?",
            binary_path.display()
        ));
    }

    // Recompute the hash. If it doesn't match, refuse to persist the
    // manifest; the caller's view of the binary disagrees with what's
    // actually on disk.
    let actual = hash_file_blocking(&binary_path)
        .with_context(|| format!("hashing {}", binary_path.display()))?;
    if actual != manifest.sha {
        // Remove the staged binary so a corrupted upload doesn't sit around
        // pretending to be a valid release.
        let _ = std::fs::remove_dir_all(&dir);
        bail!(
            "binary hash mismatch: manifest says '{}', actual is '{}'",
            manifest.sha,
            actual
        );
    }

    // Write manifest atomically.
    let manifest_path = dir.join("manifest.toml");
    let body = toml::to_string_pretty(manifest).context("serializing manifest")?;
    write_atomic(&manifest_path, body.as_bytes())
        .with_context(|| format!("writing {}", manifest_path.display()))?;

    tracing::info!(
        app = %app,
        sha = %sha,
        binary = %binary_path.display(),
        "release verified and finalized"
    );
    Ok(())
}

/// Extract the Shiku manifest from the release's binary by running
/// `<binary> --shiku-manifest`, persisting the JSON output next to the
/// binary as `shiku-manifest.json`.
///
/// Called by the agent after `verify_and_finalize` succeeds. Failure is
/// non-fatal — a binary that doesn't speak `--shiku-manifest` (e.g. one
/// not built on `shiku-runtime`) just gets an empty manifest and falls
/// back to "no platform needs."
pub async fn extract_manifest(app: &AppName, sha: &ReleaseSha) -> Result<()> {
    let app_config = apps::inspect(app).context("app not registered")?;
    let dir = release_dir(app, sha)?;
    let binary_path = dir.join("bin").join(&app_config.command);
    let manifest_path = dir.join("shiku-manifest.json");

    let output = TokioCommand::new(&binary_path)
        .arg("--shiku-manifest")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| format!("running {} --shiku-manifest", binary_path.display()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "binary returned non-zero from --shiku-manifest (status: {}): {}",
            output.status,
            stderr.trim()
        );
    }

    // Validate the JSON parses as a Manifest (catches binaries that print
    // unrelated stuff to stdout — better to fail at upload than at activate).
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str::<serde_json::Value>(stdout.trim())
        .context("parsing --shiku-manifest output as JSON")?;

    write_atomic(&manifest_path, stdout.as_bytes())
        .with_context(|| format!("writing {}", manifest_path.display()))?;

    tracing::info!(
        app = %app,
        sha = %sha,
        manifest = %manifest_path.display(),
        "extracted shiku manifest"
    );
    Ok(())
}

/// Read the cached `shiku-manifest.json` for a release. Returns `None` if
/// the file doesn't exist (release was uploaded by an old agent before
/// manifest extraction landed, or the binary doesn't support `--shiku-manifest`).
pub fn read_manifest(app: &AppName, sha: &ReleaseSha) -> Result<Option<ShikuManifest>> {
    let dir = release_dir(app, sha)?;
    let path = dir.join("shiku-manifest.json");
    if !path.is_file() {
        return Ok(None);
    }
    let body =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let parsed: ShikuManifest =
        serde_json::from_str(&body).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(parsed))
}

/// Mirror of `shiku_runtime::Manifest` — duplicated here so the agent
/// doesn't take a runtime-crate dependency (which would pull in axum and
/// other things the agent doesn't need).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ShikuManifest {
    #[allow(dead_code)] // version field reserved for forward-compat
    pub version: u32,
    #[serde(default)]
    pub secrets: Vec<String>,
    #[serde(default)]
    pub bindings: Vec<String>,
    #[serde(default)]
    pub listen_http: bool,
}

/// List all releases for an app, with metadata.
///
/// Reads each `~/apps/<app>/releases/<sha>/manifest.toml` for build info,
/// computes directory size on disk, and flags which release is currently
/// active (`current` symlink target) and which is the rollback target
/// (`previous` symlink target).
pub fn list(app: &AppName) -> Result<Vec<ReleaseInfo>> {
    let _ = apps::inspect(app).context("app not registered")?;

    let home = std::env::var("HOME").context("HOME not set")?;
    let app_root = PathBuf::from(home).join("apps").join(app);
    let releases_dir = app_root.join("releases");

    if !releases_dir.is_dir() {
        return Ok(Vec::new());
    }

    let current_sha = read_link_basename(&app_root.join("current"));
    let previous_sha = read_link_basename(&app_root.join("previous"));

    let mut out = Vec::new();
    for entry in std::fs::read_dir(&releases_dir)
        .with_context(|| format!("reading {}", releases_dir.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let sha = match entry.file_name().to_str() {
            Some(s) => s.to_string(),
            None => continue,
        };
        let manifest_path = entry.path().join("manifest.toml");
        let built_at = read_manifest_built_at(&manifest_path).unwrap_or_default();
        let size_bytes = dir_size_bytes(&entry.path()).unwrap_or(0);
        out.push(ReleaseInfo {
            sha: sha.clone(),
            built_at,
            is_current: current_sha.as_deref() == Some(sha.as_str()),
            is_previous: previous_sha.as_deref() == Some(sha.as_str()),
            size_bytes,
        });
    }

    // Newest first by built_at (RFC3339 strings compare lexicographically),
    // unbuilt-at falling to the bottom.
    out.sort_by(|a, b| b.built_at.cmp(&a.built_at));
    Ok(out)
}

/// Prune old releases, keeping the most recent `keep_latest` plus whatever
/// the `current` and `previous` symlinks point at (those are always retained
/// so rollback works).
///
/// Returns the number of release directories removed.
pub fn prune(app: &AppName, keep_latest: usize) -> Result<usize> {
    let _ = apps::inspect(app).context("app not registered")?;

    let home = std::env::var("HOME").context("HOME not set")?;
    let app_root = PathBuf::from(home).join("apps").join(app);
    let releases_dir = app_root.join("releases");
    if !releases_dir.is_dir() {
        return Ok(0);
    }

    let pinned_current = read_link_basename(&app_root.join("current"));
    let pinned_previous = read_link_basename(&app_root.join("previous"));

    // Collect all releases sorted newest-first by built_at (manifest), then by mtime as tiebreaker.
    let mut entries: Vec<(String, PathBuf, String)> = Vec::new();
    for entry in std::fs::read_dir(&releases_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let sha = match entry.file_name().to_str() {
            Some(s) => s.to_string(),
            None => continue,
        };
        let built_at = read_manifest_built_at(&entry.path().join("manifest.toml"))
            .unwrap_or_else(|| entry_mtime_rfc3339(&entry.path()));
        entries.push((sha, entry.path(), built_at));
    }
    entries.sort_by(|a, b| b.2.cmp(&a.2));

    let mut keep: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Some(s) = &pinned_current {
        keep.insert(s.clone());
    }
    if let Some(s) = &pinned_previous {
        keep.insert(s.clone());
    }

    // Among the rest (newest-first), keep the first `keep_latest`.
    let mut held = 0usize;
    for (sha, _, _) in &entries {
        if keep.contains(sha) {
            continue;
        }
        if held < keep_latest {
            keep.insert(sha.clone());
            held += 1;
        }
    }

    let mut removed = 0usize;
    for (sha, path, _) in &entries {
        if keep.contains(sha) {
            continue;
        }
        if let Err(e) = std::fs::remove_dir_all(path) {
            tracing::warn!(release = %sha, error = %e, "failed to prune release");
            continue;
        }
        removed += 1;
        tracing::info!(release = %sha, "pruned old release");
    }
    Ok(removed)
}

fn entry_mtime_rfc3339(path: &Path) -> String {
    let mtime = path
        .metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
    humantime::format_rfc3339_seconds(mtime).to_string()
}

fn read_link_basename(path: &Path) -> Option<String> {
    if !path.is_symlink() {
        return None;
    }
    std::fs::read_link(path)
        .ok()
        .and_then(|t| t.file_name().map(|n| n.to_os_string()))
        .and_then(|n| n.into_string().ok())
}

fn read_manifest_built_at(manifest_path: &Path) -> Option<String> {
    let body = std::fs::read_to_string(manifest_path).ok()?;
    let manifest: ReleaseManifest = toml::from_str(&body).ok()?;
    Some(manifest.built_at)
}

/// Directory size in bytes (recursive, follows no symlinks).
fn dir_size_bytes(path: &Path) -> Result<u64> {
    let mut total = 0u64;
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            total += dir_size_bytes(&entry.path())?;
        } else if ft.is_file() {
            total += entry.metadata()?.len();
        }
    }
    Ok(total)
}

fn hash_file_blocking(path: &Path) -> Result<String> {
    use std::io::Read;

    let mut file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).context("reading file")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn write_atomic(path: &Path, body: &[u8]) -> Result<()> {
    use std::io::Write;

    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("path has no parent: {}", path.display()))?;
    let tmp_name = format!(
        ".{}.tmp.{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("manifest"),
        std::process::id()
    );
    let tmp_path = parent.join(tmp_name);

    {
        let mut f = std::fs::File::create(&tmp_path)
            .with_context(|| format!("creating tempfile {}", tmp_path.display()))?;
        f.write_all(body).context("writing tempfile")?;
        f.sync_all().context("fsync")?;
    }

    std::fs::rename(&tmp_path, path)
        .with_context(|| format!("renaming {} -> {}", tmp_path.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_sha() {
        assert!(validate_sha("a1b2c3").is_ok());
        assert!(validate_sha(&"a".repeat(64)).is_ok());
        assert!(validate_sha("").is_err());
        assert!(validate_sha("not-hex").is_err());
        assert!(validate_sha("../../etc/passwd").is_err());
    }
}
