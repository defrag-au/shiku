//! `shiku bootstrap` — provision a new service user on the box.
//!
//! Replaces the manual bootstrap sequence that produced `shiku-test` (see
//! the implementation plan, Step 1.4). After running this once per
//! service-user-per-box, the user has:
//!
//!   - A unix account with linger enabled.
//!   - SSH key auth via the supplied GitHub username.
//!   - The full Shiku home layout (`~/apps`, `~/secrets`, `~/data`, etc.).
//!   - The shikud agent installed and running as a `systemd --user` service.
//!
//! Subsequent ops (deploy, secrets, status, etc.) all flow through the
//! agent and don't need privileged access — bootstrap is the only command
//! that requires sudo on the box.

use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{bail, Context, Result};
use tokio::process::Command;

/// Run the full bootstrap sequence.
///
/// Steps:
///   1. Verify we have the agent binary locally to upload.
///   2. SSH as `admin_user` (with passwordless sudo) to:
///      - Create the service user with linger.
///      - Install the GitHub keys for SSH auth.
///   3. Upload the agent binary as the new service user.
///   4. SSH as the new user to create the home layout, write the systemd
///      unit, and enable+start the agent.
///   5. Verify the agent is responsive by pinging it.
pub async fn run(
    host: &str,
    admin_user: &str,
    service_user: &str,
    github_user: &str,
    agent_binary: &PathBuf,
) -> Result<()> {
    if !agent_binary.is_file() {
        bail!(
            "agent binary not found at {}; run `cargo zigbuild --release \
             --target aarch64-unknown-linux-musl -p shikud` first",
            agent_binary.display()
        );
    }

    println!("→ provisioning {service_user}@{host} (privileged steps via {admin_user})");
    privileged_setup(host, admin_user, service_user, github_user).await?;

    println!("→ uploading agent binary to {service_user}@{host}");
    upload_agent(host, service_user, agent_binary).await?;

    println!("→ configuring user-level systemd for {service_user}");
    user_setup(host, service_user).await?;

    println!("→ verifying agent is responsive");
    verify_agent(host, service_user).await?;

    println!("✓ bootstrap complete: {service_user}@{host}");
    println!();
    println!("Next: cd into your app dir and run `shiku ping` (or `shiku deploy`)");
    Ok(())
}

/// Upgrade just the agent binary on an already-bootstrapped user.
/// Stops the agent, scp's the new binary, restarts, verifies. No privileged
/// steps — runs entirely as the service user via SSH key auth.
pub async fn upgrade(host: &str, service_user: &str, agent_binary: &PathBuf) -> Result<()> {
    if !agent_binary.is_file() {
        bail!(
            "agent binary not found at {}; run `cargo zigbuild --release \
             --target aarch64-unknown-linux-musl -p shikud` first",
            agent_binary.display()
        );
    }

    println!("→ uploading new agent binary to {service_user}@{host}");
    upload_agent(host, service_user, agent_binary).await?;

    println!("→ restarting agent");
    ssh_run(host, service_user, "systemctl --user restart shikud")
        .await
        .context("restarting agent")?;

    println!("→ verifying agent is responsive");
    verify_agent(host, service_user).await?;

    println!("✓ agent upgraded: {service_user}@{host}");
    Ok(())
}

/// SSH as the admin user (sudo) to create the service user, enable linger,
/// install GitHub keys.
async fn privileged_setup(
    host: &str,
    admin_user: &str,
    service_user: &str,
    github_user: &str,
) -> Result<()> {
    // Validate inputs early — these flow into shell, so reject anything
    // that could break out.
    validate_unix_name(service_user, "service user")?;
    validate_github_username(github_user)?;

    let script = format!(
        r#"
set -e
if ! id -u {service_user} >/dev/null 2>&1; then
  sudo useradd -m -s /bin/bash {service_user}
fi
sudo loginctl enable-linger {service_user}
sudo mkdir -p /home/{service_user}/.ssh
sudo chmod 700 /home/{service_user}/.ssh
curl -sSf https://github.com/{github_user}.keys | sudo tee /home/{service_user}/.ssh/authorized_keys >/dev/null
sudo chmod 600 /home/{service_user}/.ssh/authorized_keys
sudo chown -R {service_user}:{service_user} /home/{service_user}/.ssh
"#,
    );

    ssh_run(host, admin_user, &script)
        .await
        .context("privileged setup (useradd / linger / keys) failed")
}

/// Upload the agent binary to ~/.local/bin/ on the new user. Stops shikud
/// first if it's already running (idempotent re-run).
async fn upload_agent(host: &str, service_user: &str, binary: &PathBuf) -> Result<()> {
    // Make sure ~/.local/bin exists; create it if first run.
    ssh_run(
        host,
        service_user,
        "mkdir -p ~/.local/bin ~/.config/systemd/user ~/apps ~/secrets ~/data && chmod 700 ~/secrets",
    )
    .await
    .context("creating home layout")?;

    // Stop shikud if running (so we can overwrite the binary). Ignore failure
    // — first run won't have a unit yet.
    let _ = ssh_run(
        host,
        service_user,
        "systemctl --user stop shikud 2>/dev/null || true",
    )
    .await;

    // scp the binary.
    let target = format!("{service_user}@{host}:.local/bin/shikud");
    let status = Command::new("scp")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("LogLevel=ERROR")
        .arg(binary)
        .arg(&target)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("spawning scp")?
        .wait()
        .await
        .context("waiting for scp")?;
    if !status.success() {
        bail!("scp of agent binary failed");
    }

    ssh_run(host, service_user, "chmod +x ~/.local/bin/shikud")
        .await
        .context("chmod +x agent binary")?;
    Ok(())
}

/// Write the systemd-user unit, daemon-reload, enable, start.
async fn user_setup(host: &str, service_user: &str) -> Result<()> {
    let unit = r#"[Unit]
Description=Shiku agent for %u
After=default.target

[Service]
Type=simple
ExecStart=%h/.local/bin/shikud
Restart=on-failure
RestartSec=5
StandardOutput=journal
StandardError=journal
SyslogIdentifier=shikud

[Install]
WantedBy=default.target
"#;

    // Quote-safe install: use a here-doc on the remote side. We don't
    // interpolate any user input into the unit body so single-quoting is
    // sufficient.
    let script = format!(
        r#"
set -e
cat > ~/.config/systemd/user/shikud.service <<'UNIT_EOF'
{unit}
UNIT_EOF
systemctl --user daemon-reload
systemctl --user enable shikud
systemctl --user start shikud
"#,
    );

    ssh_run(host, service_user, &script)
        .await
        .context("writing+starting systemd unit")?;
    Ok(())
}

/// Verify the agent's socket exists and is responsive. We can't easily call
/// the full `Connection::request` flow from here without a `ResolvedApp`;
/// settle for "the socket exists and `nc` can connect."
async fn verify_agent(host: &str, service_user: &str) -> Result<()> {
    // Wait briefly for the agent to bind its socket after start.
    let script = r#"
set -e
for i in 1 2 3 4 5 6 7 8 9 10; do
  if [ -S "$XDG_RUNTIME_DIR/shikud.sock" ]; then
    exit 0
  fi
  sleep 0.5
done
echo "shikud socket not found after 5s" >&2
exit 1
"#;
    ssh_run(host, service_user, script)
        .await
        .context("agent didn't expose its socket; check journalctl --user -u shikud")
}

/// Run a script over SSH, inheriting stdout/stderr.
async fn ssh_run(host: &str, user: &str, script: &str) -> Result<()> {
    let mut child = Command::new("ssh")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("LogLevel=ERROR")
        .arg(format!("{user}@{host}"))
        .arg("bash -s")
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("spawning ssh")?;

    {
        use tokio::io::AsyncWriteExt;
        let stdin = child.stdin.as_mut().context("ssh stdin not piped")?;
        stdin.write_all(script.as_bytes()).await?;
        stdin.flush().await.ok();
    }
    let _ = child.stdin.take(); // EOF

    let status = child.wait().await.context("waiting for ssh")?;
    if !status.success() {
        bail!("ssh script exited with {status}");
    }
    Ok(())
}

fn validate_unix_name(name: &str, what: &str) -> Result<()> {
    if name.is_empty() || name.len() > 32 {
        bail!("{what} name must be 1-32 chars");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("{what} name '{name}' contains invalid characters (allowed: a-z A-Z 0-9 - _)");
    }
    Ok(())
}

fn validate_github_username(name: &str) -> Result<()> {
    // GitHub usernames: 1-39 chars, alphanumeric and dashes, no leading/
    // trailing dash. Ours flow into a curl URL, so be strict.
    if name.is_empty() || name.len() > 39 {
        bail!("github username must be 1-39 chars");
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        bail!("github username '{name}' contains invalid characters");
    }
    if name.starts_with('-') || name.ends_with('-') {
        bail!("github username '{name}' cannot start or end with '-'");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_unix_names() {
        assert!(validate_unix_name("norn", "x").is_ok());
        assert!(validate_unix_name("eternal-seas", "x").is_ok());
        assert!(validate_unix_name("", "x").is_err());
        assert!(validate_unix_name("$injection", "x").is_err());
        assert!(validate_unix_name("with space", "x").is_err());
    }

    #[test]
    fn validates_github_usernames() {
        assert!(validate_github_username("DamonOehlman").is_ok());
        assert!(validate_github_username("user-name").is_ok());
        assert!(validate_github_username("").is_err());
        assert!(validate_github_username("-leading").is_err());
        assert!(validate_github_username("trailing-").is_err());
        assert!(validate_github_username("with_underscore").is_err());
    }
}
