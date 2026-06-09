//! Pinned cloudflared release + downloader.
//!
//! Shiku owns which cloudflared version runs on the box. This crate-internal
//! module pins a specific upstream version + SHA-256 per supported arch and
//! provides a download/verify/swap helper used by `shiku tunnel upgrade`.
//!
//! Bumping the pin: change [`PINNED_VERSION`] and the per-arch SHA. The CI
//! integration test (when added) will verify the SHA matches what CF
//! actually serves at the pinned version's release URL.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};

/// The cloudflared version Shiku will install / upgrade to.
///
/// Bumping this pin: update both the version and the per-arch SHA below.
pub const PINNED_VERSION: &str = "2026.3.0";

/// SHA-256 of the linux-arm64 binary at [`PINNED_VERSION`]. Used to
/// verify what we downloaded matches what we expected.
pub const PINNED_SHA256_LINUX_ARM64: &str =
    "0755ba4cbab59980e6148367fcf53a8f3ec85a97deefd63c2420cf7850769bee";

/// Where Shiku installs cloudflared. Always under `~/.local/bin/`.
pub fn install_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home)
        .join(".local")
        .join("bin")
        .join("cloudflared"))
}

/// What [`upgrade`] did. Used by the dispatch path to populate the
/// wire `Response::TunnelUpgrade`.
#[derive(Debug)]
pub struct UpgradeResult {
    pub version: String,
    pub changed: bool,
}

/// Download the pinned cloudflared release, verify its SHA-256, and
/// swap it in at `~/.local/bin/cloudflared`. The caller is responsible
/// for stopping/starting the cloudflared systemd unit around this call.
///
/// If the installed binary already matches the pinned SHA-256, this is
/// a no-op and returns `changed = false`.
pub async fn upgrade() -> Result<UpgradeResult> {
    let target = install_path()?;
    let arch = detect_arch()?;
    let expected_sha = match arch {
        Arch::Aarch64 => PINNED_SHA256_LINUX_ARM64,
    };

    // Idempotency: if the installed binary already matches, no need to
    // download anything.
    if target.is_file() {
        if let Ok(existing_sha) = sha256_file(&target).await {
            if existing_sha == expected_sha {
                tracing::info!(
                    version = PINNED_VERSION,
                    "cloudflared already at pinned version; nothing to do"
                );
                return Ok(UpgradeResult {
                    version: PINNED_VERSION.to_string(),
                    changed: false,
                });
            }
        }
    }

    // Download. CF publishes static binaries at github.com/cloudflare/cloudflared/releases.
    let url = release_url(PINNED_VERSION, arch);
    tracing::info!(%url, "downloading cloudflared");
    let body = http_download(&url).await?;

    // Verify SHA-256.
    let actual_sha = sha256_bytes(&body);
    if actual_sha != expected_sha {
        bail!(
            "cloudflared SHA-256 mismatch: expected {expected_sha}, got {actual_sha} \
             (URL: {url}). This means CF's release content changed under the pin, or \
             the pin in cloudflared_release.rs is stale. Refusing to install."
        );
    }
    tracing::info!(sha256 = actual_sha, "cloudflared SHA-256 verified");

    // Atomic swap: write to a sibling tempfile, set perms, rename in.
    let parent = target
        .parent()
        .ok_or_else(|| anyhow!("install path has no parent"))?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let tmp = parent.join(format!(".cloudflared.tmp.{}", std::process::id()));
    tokio::fs::write(&tmp, &body)
        .await
        .with_context(|| format!("writing tempfile {}", tmp.display()))?;
    let perms = std::fs::Permissions::from_mode(0o755);
    std::fs::set_permissions(&tmp, perms)?;
    std::fs::rename(&tmp, &target)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), target.display()))?;
    tracing::info!(
        path = %target.display(),
        version = PINNED_VERSION,
        "installed cloudflared"
    );

    Ok(UpgradeResult {
        version: PINNED_VERSION.to_string(),
        changed: true,
    })
}

/// Architectures we support. Shiku only deploys to aarch64 today
/// (Netcup ARM, Hetzner ARM, etc); add x86_64 here when an x86_64 box
/// turns up. The match in [`upgrade`] then needs the corresponding SHA.
#[derive(Debug, Clone, Copy)]
enum Arch {
    Aarch64,
}

fn detect_arch() -> Result<Arch> {
    // We're running natively on the box, so `std::env::consts::ARCH`
    // is the truth.
    match std::env::consts::ARCH {
        "aarch64" => Ok(Arch::Aarch64),
        other => bail!(
            "unsupported architecture for cloudflared upgrade: {other}. \
             Shiku currently pins binaries for aarch64 only; add a SHA \
             for {other} in cloudflared_release.rs to extend support."
        ),
    }
}

fn release_url(version: &str, arch: Arch) -> String {
    let asset = match arch {
        Arch::Aarch64 => "cloudflared-linux-arm64",
    };
    format!("https://github.com/cloudflare/cloudflared/releases/download/{version}/{asset}")
}

async fn http_download(url: &str) -> Result<Vec<u8>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .user_agent(concat!("shiku/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building reqwest client")?;
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        bail!("GET {url} returned HTTP {}", resp.status());
    }
    let bytes = resp
        .bytes()
        .await
        .with_context(|| format!("reading body from {url}"))?;
    Ok(bytes.to_vec())
}

async fn sha256_file(path: &std::path::Path) -> Result<String> {
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("reading {}", path.display()))?;
    Ok(sha256_bytes(&bytes))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_url_arm64() {
        assert_eq!(
            release_url("2026.3.0", Arch::Aarch64),
            "https://github.com/cloudflare/cloudflared/releases/download/2026.3.0/cloudflared-linux-arm64"
        );
    }

    #[test]
    fn pinned_sha_is_well_formed_hex() {
        assert_eq!(PINNED_SHA256_LINUX_ARM64.len(), 64);
        assert!(PINNED_SHA256_LINUX_ARM64
            .chars()
            .all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn sha256_known_vector() {
        // Known-good test vector: SHA-256("") = e3b0c44...855
        let empty = sha256_bytes(b"");
        assert_eq!(
            empty,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
