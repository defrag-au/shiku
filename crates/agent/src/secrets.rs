//! Per-user secrets store.
//!
//! Secrets are stored as `age`-encrypted files in `~/secrets/<NAME>.age`,
//! encrypted to a per-user master key (`~/secrets/master.key`). The master
//! key is generated automatically on first use and kept at mode 0600.
//!
//! Per the deploy doc:
//!   - Secrets are scoped per-user, not per-app. Multiple apps run by the
//!     same service-user share the secret namespace.
//!   - Per-app `secrets` allowlists in `AppConfig` constrain which secrets
//!     get injected into a given app's environment — defense in depth, not
//!     a security boundary.
//!   - Plaintext travels over SSH (already encrypted in transit). The agent
//!     encrypts to the master key on receipt and zeroizes the plaintext.
//!
//! Memory hygiene: we use `secrecy::SecretString` for the plaintext to make
//! accidental logging or display painful, and `zeroize` runs on drop.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use age::x25519;
use anyhow::{anyhow, bail, Context, Result};
use secrecy::{ExposeSecret, SecretString};
use shiku_types::SecretName;

const MASTER_KEY_FILE: &str = "master.key";

/// Resolve the user's secrets directory (`~/secrets/`). Created on demand
/// with mode 0700 (the master key inside is 0600).
fn secrets_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    let dir = PathBuf::from(home).join("secrets");
    if !dir.exists() {
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        set_mode(&dir, 0o700)?;
    }
    Ok(dir)
}

/// Validate a secret name. Same rules as app names — they become filenames
/// (`<NAME>.age`) and env-var names. Reject anything that could escape the
/// secrets directory or break the env file format.
fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("secret name is empty");
    }
    if name.len() > 64 {
        bail!("secret name too long (max 64 chars)");
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        bail!("secret name '{name}' contains invalid characters (allowed: A-Z 0-9 _)");
    }
    Ok(())
}

fn secret_path(name: &str) -> Result<PathBuf> {
    Ok(secrets_dir()?.join(format!("{name}.age")))
}

fn master_key_path() -> Result<PathBuf> {
    Ok(secrets_dir()?.join(MASTER_KEY_FILE))
}

/// Load the user's master key, generating it if absent.
///
/// The keyfile contains the bech32-encoded x25519 secret key. We persist
/// only the secret half — the public half is derived on demand.
pub fn ensure_master_key() -> Result<x25519::Identity> {
    let path = master_key_path()?;
    if path.is_file() {
        return load_master_key(&path);
    }

    tracing::info!(path = %path.display(), "generating new master key");
    let identity = x25519::Identity::generate();
    // age 0.10 returns its own secrecy::Secret<String> (secrecy 0.8 re-export);
    // use the age-re-exported ExposeSecret to read it.
    let body = age::secrecy::ExposeSecret::expose_secret(&identity.to_string()).clone();
    write_atomic_with_mode(&path, body.as_bytes(), 0o600)?;
    Ok(identity)
}

fn load_master_key(path: &Path) -> Result<x25519::Identity> {
    let body =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let trimmed = body.trim();
    let identity: x25519::Identity = trimmed
        .parse()
        .map_err(|e| anyhow!("parsing master key at {}: {e}", path.display()))?;
    Ok(identity)
}

/// Encrypt and persist a secret value.
///
/// Caller is responsible for not logging the plaintext. We zeroize the
/// reference once we're done.
pub fn set(name: &SecretName, value: SecretString) -> Result<()> {
    validate_name(name)?;
    let identity = ensure_master_key()?;
    let recipient = identity.to_public();

    let plaintext = value.expose_secret().as_bytes();

    let mut ciphertext = Vec::new();
    let encryptor =
        age::Encryptor::with_recipients(
            vec![Box::new(recipient) as Box<dyn age::Recipient + Send>],
        )
        .context("constructing age encryptor")?;
    let mut writer = encryptor
        .wrap_output(&mut ciphertext)
        .context("starting age stream")?;
    writer.write_all(plaintext).context("encrypting secret")?;
    writer.finish().context("finalising age stream")?;

    let path = secret_path(name)?;
    write_atomic_with_mode(&path, &ciphertext, 0o600)
        .with_context(|| format!("writing {}", path.display()))?;

    tracing::info!(secret = %name, "stored secret");
    Ok(())
}

/// Decrypt one secret. Returns `Ok(None)` if the secret isn't set.
pub fn get(name: &SecretName) -> Result<Option<SecretString>> {
    validate_name(name)?;
    let path = secret_path(name)?;
    if !path.is_file() {
        return Ok(None);
    }
    let identity = ensure_master_key()?;

    let ciphertext = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;

    let mut reader = match age::Decryptor::new(&ciphertext[..])
        .with_context(|| format!("opening age stream at {}", path.display()))?
    {
        age::Decryptor::Recipients(d) => d
            .decrypt(std::iter::once(&identity as &dyn age::Identity))
            .context("decrypting age stream")?,
        age::Decryptor::Passphrase(_) => {
            bail!("secret '{name}' is passphrase-encrypted; expected x25519-recipient encryption")
        }
    };

    let mut plaintext = Vec::new();
    reader
        .read_to_end(&mut plaintext)
        .context("reading decrypted body")?;

    let s = String::from_utf8(plaintext)
        .map_err(|e| anyhow!("secret '{name}' is not valid UTF-8: {e}"))?;
    Ok(Some(SecretString::new(s.into())))
}

/// List secret names. Values are never returned by this API.
pub fn list() -> Result<Vec<SecretName>> {
    let dir = secrets_dir()?;
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let name = entry
            .file_name()
            .to_str()
            .map(|s| s.to_string())
            .unwrap_or_default();
        if let Some(stem) = name.strip_suffix(".age") {
            out.push(stem.to_string());
        }
    }
    out.sort();
    Ok(out)
}

/// Remove a secret. Returns `Ok(false)` if it didn't exist.
pub fn remove(name: &SecretName) -> Result<bool> {
    validate_name(name)?;
    let path = secret_path(name)?;
    if !path.exists() {
        return Ok(false);
    }
    std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    tracing::info!(secret = %name, "removed secret");
    Ok(true)
}

/// Decrypt the allowlisted secrets for an app. Used by the activation flow
/// when building the env file.
///
/// Errors fast on any individual decryption failure or missing secret —
/// activating a service against an incomplete secret set silently would be
/// worse than failing loudly.
pub fn resolve_for_app(allowlist: &[SecretName]) -> Result<Vec<(SecretName, SecretString)>> {
    let mut out = Vec::with_capacity(allowlist.len());
    for name in allowlist {
        let value =
            get(name)?.ok_or_else(|| anyhow!("secret '{name}' is in app allowlist but not set"))?;
        out.push((name.clone(), value));
    }
    Ok(out)
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(mode);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

fn write_atomic_with_mode(path: &Path, body: &[u8], mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("path has no parent: {}", path.display()))?;
    let tmp_name = format!(
        ".{}.tmp.{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("secret"),
        std::process::id()
    );
    let tmp_path = parent.join(tmp_name);

    {
        let mut f = std::fs::File::create(&tmp_path)
            .with_context(|| format!("creating tempfile {}", tmp_path.display()))?;
        f.write_all(body).context("writing tempfile")?;
        f.sync_all().context("fsync")?;
        let mut perms = f.metadata()?.permissions();
        perms.set_mode(mode);
        f.set_permissions(perms)?;
    }

    std::fs::rename(&tmp_path, path)
        .with_context(|| format!("renaming {} -> {}", tmp_path.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_temp_home<F: FnOnce()>(f: F) {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::env::set_var("HOME", tmp.path());
        f();
    }

    #[test]
    fn validates_names() {
        assert!(validate_name("BOT_TOKEN").is_ok());
        assert!(validate_name("JWT_SECRET").is_ok());
        assert!(validate_name("X").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("../etc/passwd").is_err());
        assert!(validate_name("with space").is_err());
        assert!(validate_name("with-dash").is_err()); // reserved for future
    }

    #[test]
    #[serial_test::serial(home_env)]
    fn set_get_roundtrip() {
        with_temp_home(|| {
            let name = "BOT_TOKEN".to_string();
            let value = SecretString::new("super-secret-discord-token".into());
            set(&name, value.clone()).expect("set");
            let recovered = get(&name).expect("get").expect("present");
            assert_eq!(recovered.expose_secret(), value.expose_secret());
        });
    }

    #[test]
    #[serial_test::serial(home_env)]
    fn missing_secret_returns_none() {
        with_temp_home(|| {
            let result = get(&"NEVER_SET".into()).expect("get");
            assert!(result.is_none());
        });
    }

    #[test]
    #[serial_test::serial(home_env)]
    fn list_after_set() {
        with_temp_home(|| {
            set(&"A".into(), SecretString::new("1".into())).unwrap();
            set(&"B".into(), SecretString::new("2".into())).unwrap();
            let names = list().expect("list");
            assert_eq!(names, vec!["A".to_string(), "B".to_string()]);
        });
    }

    #[test]
    #[serial_test::serial(home_env)]
    fn remove_idempotent() {
        with_temp_home(|| {
            assert!(!remove(&"NOPE".into()).unwrap());
            set(&"X".into(), SecretString::new("1".into())).unwrap();
            assert!(remove(&"X".into()).unwrap());
            assert!(!remove(&"X".into()).unwrap());
        });
    }

    #[test]
    #[serial_test::serial(home_env)]
    fn resolve_for_app_fails_on_missing() {
        with_temp_home(|| {
            set(&"PRESENT".into(), SecretString::new("v".into())).unwrap();
            let err = resolve_for_app(&["PRESENT".into(), "MISSING".into()]).unwrap_err();
            assert!(format!("{err}").contains("MISSING"));
        });
    }

    #[test]
    #[serial_test::serial(home_env)]
    fn resolve_for_app_returns_in_order() {
        with_temp_home(|| {
            set(&"A".into(), SecretString::new("a-val".into())).unwrap();
            set(&"B".into(), SecretString::new("b-val".into())).unwrap();
            let resolved = resolve_for_app(&["B".into(), "A".into()]).unwrap();
            assert_eq!(resolved.len(), 2);
            assert_eq!(resolved[0].0, "B");
            assert_eq!(resolved[1].0, "A");
        });
    }

    #[test]
    #[serial_test::serial(home_env)]
    fn master_key_persists_across_calls() {
        with_temp_home(|| {
            let id1 = ensure_master_key().expect("ensure 1");
            let id2 = ensure_master_key().expect("ensure 2");
            assert_eq!(
                id1.to_public().to_string(),
                id2.to_public().to_string(),
                "master key should persist, not regenerate"
            );
        });
    }
}
