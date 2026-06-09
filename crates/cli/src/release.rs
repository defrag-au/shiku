//! Release-pipeline helpers.
//!
//! A "release" is one (binary + manifest) pair, content-addressed by the
//! SHA-256 of the binary. The CLI:
//!
//!   1. Builds the binary via `cargo zigbuild --target <triple> -p <package>`.
//!   2. Hashes the binary to derive the release sha.
//!   3. Builds a [`shiku_types::ReleaseManifest`] with metadata.
//!   4. Rsyncs the binary to `<host>:~/apps/<app>/releases/<sha>/bin/<command>`.
//!   5. Sends `Request::ReleaseUpload { app, sha, manifest }` to the agent so
//!      it can verify the staged binary matches the manifest.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use shiku_types::ReleaseManifest;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::config::ResolvedApp;

/// Result of a successful build: the absolute path to the binary that should
/// be uploaded.
pub struct BuiltBinary {
    pub path: PathBuf,
    pub package: String,
    pub target: String,
}

/// Run the configured build command (default: `cargo zigbuild`) for the
/// app's target triple, returning the path to the produced binary.
///
/// Streams stdout+stderr to the user so they see compile progress in real time.
pub async fn build(resolved: &ResolvedApp) -> Result<BuiltBinary> {
    let workspace_root = workspace_root().context("locating workspace root")?;
    let build_cmd = parse_build_command(&resolved.build)?;

    tracing::info!(
        package = %resolved.package,
        target = %resolved.target,
        "building"
    );

    let mut cmd = Command::new(&build_cmd[0]);
    cmd.args(&build_cmd[1..])
        .arg("--release")
        .arg("--target")
        .arg(&resolved.target)
        .arg("-p")
        .arg(&resolved.package)
        .current_dir(&workspace_root)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let status = cmd
        .spawn()
        .with_context(|| format!("spawning {}", resolved.build))?
        .wait()
        .await
        .context("waiting for build")?;
    if !status.success() {
        bail!(
            "build failed: `{} --release --target {} -p {}` exited with {}",
            resolved.build,
            resolved.target,
            resolved.package,
            status
        );
    }

    let path = workspace_root
        .join("target")
        .join(&resolved.target)
        .join("release")
        .join(&resolved.package);

    if !path.is_file() {
        bail!(
            "build succeeded but binary not found at expected path: {}",
            path.display()
        );
    }

    Ok(BuiltBinary {
        path,
        package: resolved.package.clone(),
        target: resolved.target.clone(),
    })
}

/// Compute SHA-256 of a file, returning a 64-char lowercase hex string.
pub async fn hash_file(path: &Path) -> Result<String> {
    let mut file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).await.context("reading file")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Build the release manifest.
pub fn build_manifest(
    sha: &str,
    binary: &BuiltBinary,
    git_sha: Option<String>,
    git_dirty: bool,
) -> ReleaseManifest {
    let built_at = humantime::format_rfc3339_seconds(std::time::SystemTime::now()).to_string();
    let built_by = std::env::var("USER")
        .ok()
        .map(|u| format!("{u}@{}", hostname().unwrap_or_else(|| "laptop".into())));

    ReleaseManifest {
        sha: sha.to_string(),
        package: binary.package.clone(),
        target: binary.target.clone(),
        built_at,
        built_by,
        git_sha,
        git_dirty,
        // Per-secret hashes go here in Phase 3 (Step 3.2). Empty for now.
        secrets: Vec::new(),
    }
}

/// Probe git for the current commit and dirty state. Best-effort; returns
/// `(None, false)` if we're not in a git repo or git isn't available.
pub async fn git_state(workspace_root: &Path) -> (Option<String>, bool) {
    let sha = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .current_dir(workspace_root)
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());

    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(workspace_root)
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    (sha, dirty)
}

/// rsync the binary to `<host>:~/apps/<app>/releases/<sha>/bin/<command>`.
///
/// Uses `--mkpath` so the destination directory hierarchy is created during
/// the transfer — saves a separate ssh-mkdir round trip.
pub async fn rsync_to_box(binary: &BuiltBinary, resolved: &ResolvedApp, sha: &str) -> Result<()> {
    let dest = format!(
        "{user}@{host}:apps/{app}/releases/{sha}/bin/{cmd}",
        user = resolved.ssh_user,
        host = resolved.deploy_host,
        app = resolved.app_name,
        sha = sha,
        cmd = resolved.package,
    );

    tracing::info!(dest = %dest, "uploading binary via rsync");

    let status = Command::new("rsync")
        .arg("-e")
        .arg("ssh -o BatchMode=yes -o LogLevel=ERROR")
        .arg("--mkpath")
        .arg("--chmod=u=rwx,go=") // 0700 on the binary
        .arg("-z") // compress in transit
        .arg("--info=progress2")
        .arg(&binary.path)
        .arg(&dest)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("spawning rsync")?
        .wait()
        .await
        .context("waiting for rsync")?;

    if !status.success() {
        bail!("rsync failed with {status}");
    }
    Ok(())
}

/// Locate the workspace root by walking upward looking for a `Cargo.toml`
/// containing a `[workspace]` table.
pub fn workspace_root() -> Result<PathBuf> {
    let mut dir = std::env::current_dir().context("getting current dir")?;
    loop {
        let candidate = dir.join("Cargo.toml");
        if candidate.is_file() {
            let body = std::fs::read_to_string(&candidate).ok();
            if let Some(body) = body {
                if body.contains("[workspace]") {
                    return Ok(dir);
                }
            }
        }
        if !dir.pop() {
            return Err(anyhow!(
                "workspace root not found (no [workspace] Cargo.toml)"
            ));
        }
    }
}

fn parse_build_command(raw: &str) -> Result<Vec<String>> {
    let parts: Vec<String> = raw.split_whitespace().map(|s| s.to_string()).collect();
    if parts.is_empty() {
        return Err(anyhow!("build command is empty"));
    }
    Ok(parts)
}

fn hostname() -> Option<String> {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_build_command() {
        let parts = parse_build_command("cargo zigbuild").expect("parse");
        assert_eq!(parts, vec!["cargo", "zigbuild"]);
    }

    #[test]
    fn parse_build_rejects_empty() {
        assert!(parse_build_command("").is_err());
        assert!(parse_build_command("   ").is_err());
    }

    #[tokio::test]
    async fn hash_is_stable_and_well_formed() {
        let mut path = std::env::temp_dir();
        path.push(format!("shiku-test-hash-{}", std::process::id()));
        std::fs::write(&path, b"hello shiku").expect("write");

        let h1 = hash_file(&path).await.expect("hash 1");
        let h2 = hash_file(&path).await.expect("hash 2");

        // Hex-encoded SHA-256 is always 64 lowercase hex chars.
        assert_eq!(h1.len(), 64);
        assert!(h1
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        // Same content → same hash.
        assert_eq!(h1, h2);

        // Different content → different hash.
        std::fs::write(&path, b"hello shiku!").expect("write 2");
        let h3 = hash_file(&path).await.expect("hash 3");
        assert_ne!(h1, h3);

        let _ = std::fs::remove_file(&path);
    }
}
