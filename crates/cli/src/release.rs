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
use shiku_types::{AssetBinding, ReleaseManifest};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

// Asset quotas. Per-file size and file count align with Cloudflare Workers
// Static Assets; the total caps are shiku-specific (a box retains several
// releases on one disk, unlike an ephemeral Worker).
const MAX_ASSET_FILE_BYTES: u64 = 25 * 1024 * 1024;
const MAX_ASSET_FILES: usize = 20_000;
const WARN_ASSET_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
const FAIL_ASSET_TOTAL_BYTES: u64 = 256 * 1024 * 1024;

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
    binary_sha: &str,
    binary: &BuiltBinary,
    git_sha: Option<String>,
    git_dirty: bool,
    assets: Vec<AssetBinding>,
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
        binary_sha: binary_sha.to_string(),
        // Per-secret hashes go here in Phase 3 (Step 3.2). Empty for now.
        secrets: Vec::new(),
        assets,
    }
}

/// One collected asset file: absolute source path + its release-relative path
/// (forward-slashed, used both as the upload destination and the manifest key).
pub struct AssetFile {
    pub abs: PathBuf,
    pub rel: String,
}

/// Collect the declared asset entries relative to `project_root`: each entry is
/// a file or a directory (walked recursively). Symlinks are rejected, and the
/// per-file / file-count / total-size quotas are enforced.
pub fn collect_assets(project_root: &Path, entries: &[String]) -> Result<Vec<AssetFile>> {
    let mut out = Vec::new();
    for entry in entries {
        let rel = entry.trim().trim_matches('/');
        if rel.is_empty() || rel.split('/').any(|c| c == "..") {
            bail!("invalid asset entry '{entry}': must be a relative path with no `..`");
        }
        let abs = project_root.join(rel);
        let meta = std::fs::symlink_metadata(&abs)
            .with_context(|| format!("asset '{entry}' not found at {}", abs.display()))?;
        let ft = meta.file_type();
        if ft.is_symlink() {
            bail!("asset '{entry}' is a symlink — symlinks are not bundled into releases");
        } else if ft.is_dir() {
            walk_assets(rel, &abs, &mut out)?;
        } else if ft.is_file() {
            out.push(AssetFile {
                abs,
                rel: rel.to_string(),
            });
        }
    }
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    enforce_asset_quotas(&out)?;
    Ok(out)
}

fn walk_assets(rel_dir: &str, abs_dir: &Path, out: &mut Vec<AssetFile>) -> Result<()> {
    for entry in std::fs::read_dir(abs_dir)
        .with_context(|| format!("reading asset dir {}", abs_dir.display()))?
    {
        let entry = entry?;
        // `file_type()` does not traverse symlinks — exactly what we want.
        let ft = entry.file_type()?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let rel = format!("{rel_dir}/{name}");
        if ft.is_symlink() {
            bail!("asset path contains a symlink ({rel}) — symlinks are not bundled");
        } else if ft.is_dir() {
            walk_assets(&rel, &entry.path(), out)?;
        } else if ft.is_file() {
            out.push(AssetFile {
                abs: entry.path(),
                rel,
            });
        }
    }
    Ok(())
}

fn enforce_asset_quotas(files: &[AssetFile]) -> Result<()> {
    if files.len() > MAX_ASSET_FILES {
        bail!(
            "{} asset files exceeds the {MAX_ASSET_FILES}-file limit",
            files.len()
        );
    }
    let mib = |b: u64| b as f64 / (1024.0 * 1024.0);
    let mut total = 0u64;
    for f in files {
        let sz = std::fs::metadata(&f.abs)
            .with_context(|| format!("statting asset {}", f.rel))?
            .len();
        if sz > MAX_ASSET_FILE_BYTES {
            bail!(
                "asset '{}' is {:.1} MiB, over the {} MiB per-file limit",
                f.rel,
                mib(sz),
                MAX_ASSET_FILE_BYTES / (1024 * 1024)
            );
        }
        total += sz;
    }
    if total > FAIL_ASSET_TOTAL_BYTES {
        bail!(
            "assets total {:.1} MiB, over the {} MiB per-release limit",
            mib(total),
            FAIL_ASSET_TOTAL_BYTES / (1024 * 1024)
        );
    }
    if total > WARN_ASSET_TOTAL_BYTES {
        tracing::warn!(
            "assets total {:.1} MiB (soft limit {} MiB) — large asset sets slow \
             every deploy and are retained across several releases",
            mib(total),
            WARN_ASSET_TOTAL_BYTES / (1024 * 1024)
        );
    }
    Ok(())
}

/// Hash each collected asset into a sorted [`AssetBinding`] list (the manifest
/// asset set). `files` is assumed already sorted by `rel`.
pub async fn asset_bindings(files: &[AssetFile]) -> Result<Vec<AssetBinding>> {
    let mut out = Vec::with_capacity(files.len());
    for f in files {
        let hash = hash_file(&f.abs)
            .await
            .with_context(|| format!("hashing asset {}", f.rel))?;
        out.push(AssetBinding {
            path: f.rel.clone(),
            hash,
        });
    }
    Ok(out)
}

/// rsync the collected asset files to `<host>:~/apps/<app>/releases/<sha>/assets/`,
/// preserving their relative paths. Stages into a temp tree first so a single
/// rsync transfers the whole set.
pub async fn rsync_assets_to_box(
    files: &[AssetFile],
    resolved: &ResolvedApp,
    sha: &str,
) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }
    let staging = std::env::temp_dir().join(format!("shiku-assets-{}-{sha}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    for f in files {
        let dest = staging.join(&f.rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("staging {}", parent.display()))?;
        }
        std::fs::copy(&f.abs, &dest)
            .with_context(|| format!("staging asset {}", f.rel))?;
    }

    let dest = format!(
        "{user}@{host}:apps/{app}/releases/{sha}/assets/",
        user = resolved.ssh_user,
        host = resolved.deploy_host,
        app = resolved.app_name,
        sha = sha,
    );
    let src = format!("{}/", staging.display()); // trailing slash → copy contents

    tracing::info!(files = files.len(), dest = %dest, "uploading assets via rsync");

    let result = Command::new("rsync")
        .arg("-e")
        .arg("ssh -o BatchMode=yes -o LogLevel=ERROR")
        .arg("-r")
        .arg("--mkpath")
        .arg("--chmod=Du=rwx,Dgo=,Fu=r,Fgo=") // read-only release content
        .arg("-z")
        .arg(&src)
        .arg(&dest)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("spawning rsync (assets)")?
        .wait()
        .await
        .context("waiting for rsync (assets)");

    let _ = std::fs::remove_dir_all(&staging);
    if !result?.success() {
        bail!("rsync (assets) failed");
    }
    Ok(())
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

    #[test]
    fn collect_assets_walks_sorted_and_rejects_bad_entries() {
        let root = std::env::temp_dir().join(format!("shiku-collect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("projects/p")).unwrap();
        std::fs::write(root.join("projects/p/b.toml"), b"b").unwrap();
        std::fs::write(root.join("projects/p/a.toml"), b"a").unwrap();

        // Directory entry → walked recursively, returned sorted by rel path.
        let files = collect_assets(&root, &["projects/p".to_string()]).expect("collect dir");
        let rels: Vec<&str> = files.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(rels, ["projects/p/a.toml", "projects/p/b.toml"]);

        // A single file entry works too.
        let one = collect_assets(&root, &["projects/p/a.toml".to_string()]).expect("collect file");
        assert_eq!(one.len(), 1);

        // Missing entry and `..` escapes are rejected.
        assert!(collect_assets(&root, &["nope".to_string()]).is_err());
        assert!(collect_assets(&root, &["../escape".to_string()]).is_err());

        // Symlinks anywhere in the tree are rejected.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                root.join("projects/p/a.toml"),
                root.join("projects/link.toml"),
            )
            .unwrap();
            assert!(collect_assets(&root, &["projects".to_string()]).is_err());
        }

        let _ = std::fs::remove_dir_all(&root);
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
