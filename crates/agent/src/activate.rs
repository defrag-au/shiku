//! Atomic activation: take an uploaded release and make it the running version.
//!
//! Activation is the single moment of change in a deploy. The agent:
//!
//!   1. Verifies the requested release exists.
//!   2. Generates `~/.config/systemd/user/<service>.service` from the app
//!      config + sane hardening defaults.
//!   3. Writes the runtime env file in tmpfs (`$XDG_RUNTIME_DIR/shiku/<app>.env`)
//!      — `vars` for now, secrets and bindings come in later steps.
//!   4. Atomically swaps `~/apps/<app>/current` → `releases/<sha>` (via rename),
//!      with the previous target preserved as `previous` for rollback.
//!   5. `systemctl --user daemon-reload && systemctl --user restart <service>`.
//!
//! Phase 1 of activation: no health check, no rollback. Just confirm
//! `is-active` returns `active` after the restart settles. Step 3.2 adds
//! health check + rollback.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use shiku_types::{ActivateEvent, AppConfig, AppName, HealthSpec, ReleaseSha, SystemdSpec};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::apps;
use crate::bindings;
use crate::health;
use crate::releases;
use crate::secrets;
use secrecy::ExposeSecret;

const APP_HOME_DIR_NAME: &str = "apps";
const RELEASES_DIR_NAME: &str = "releases";
const CURRENT_SYMLINK: &str = "current";
const PREVIOUS_SYMLINK: &str = "previous";
const SETTLE_POLL_INTERVAL_MS: u64 = 200;

/// Activate a release for an app, with automatic rollback on health-check
/// failure.
///
/// Flow:
///   1. Verify release exists, write unit + env file, swap symlink, restart.
///   2. Run the configured health check.
///   3. On success: done.
///   4. On failure: revert the symlink swap, restart, wait briefly for the
///      old version to come back, return an error describing what happened.
///
/// Returns the [`ActivationOutcome`] so callers (and the dispatch path) can
/// distinguish "deployed cleanly" from "rolled back after failure".
/// Backward-compatible non-streaming activate. Currently unused
/// (everything goes through the streaming path); kept for tests and
/// any future caller that doesn't want progress events.
#[allow(dead_code)]
pub async fn activate(app: &AppName, sha: &ReleaseSha) -> ActivationOutcome {
    activate_with_events_inner(app, sha, None).await
}

/// Same as [`activate`] but emits [`ActivateEvent`]s through `events`
/// as the activation progresses. Used by the streaming dispatch path
/// in `main.rs::activate_streaming` so the CLI can render live deploy
/// progress.
pub async fn activate_with_events(
    app: &AppName,
    sha: &ReleaseSha,
    events: mpsc::Sender<ActivateEvent>,
) -> ActivationOutcome {
    activate_with_events_inner(app, sha, Some(events)).await
}

async fn activate_with_events_inner(
    app: &AppName,
    sha: &ReleaseSha,
    events: Option<mpsc::Sender<ActivateEvent>>,
) -> ActivationOutcome {
    let outcome = match activate_inner(app, sha, events.as_ref()).await {
        Ok(()) => {
            emit(
                events.as_ref(),
                ActivateEvent::Activated { sha: sha.clone() },
            )
            .await;
            ActivationOutcome::Activated
        }
        Err(initial_err) => {
            tracing::warn!(error = %initial_err, app = %app, sha = %sha, "activation failed, rolling back");
            emit(events.as_ref(), ActivateEvent::HealthCheckFailed).await;
            match rollback_after_failure(app).await {
                Ok(prev_sha) => {
                    let reason = format!("{initial_err:#}");
                    if let Some(p) = &prev_sha {
                        emit(
                            events.as_ref(),
                            ActivateEvent::RolledBack {
                                to_sha: p.clone(),
                                reason: reason.clone(),
                            },
                        )
                        .await;
                    }
                    ActivationOutcome::RolledBack {
                        reason,
                        previous_sha: prev_sha,
                    }
                }
                Err(rollback_err) => ActivationOutcome::Stuck {
                    activation_error: format!("{initial_err:#}"),
                    rollback_error: format!("{rollback_err:#}"),
                },
            }
        }
    };
    // Closing the channel signals the streaming wrapper that we're
    // done. (Done implicitly when `events` drops at end of scope.)
    drop(events);
    outcome
}

/// Send an event into the optional channel, ignoring channel-closed
/// errors (the receiver may have dropped — we don't fail activation
/// for that).
async fn emit(events: Option<&mpsc::Sender<ActivateEvent>>, event: ActivateEvent) {
    if let Some(tx) = events {
        let _ = tx.send(event).await;
    }
}

/// What happened during an activation attempt.
pub enum ActivationOutcome {
    /// New release is live and healthy.
    Activated,
    /// Activation failed health check; the previous release was restored.
    RolledBack {
        reason: String,
        previous_sha: Option<ReleaseSha>,
    },
    /// Activation failed AND rollback failed. Service may be down.
    Stuck {
        activation_error: String,
        rollback_error: String,
    },
}

/// The forward-only activation steps. If any of these returns an error, we
/// roll back. Pulled out as a helper so the rollback code can be written
/// against a clear pre/post boundary.
async fn activate_inner(
    app: &AppName,
    sha: &ReleaseSha,
    events: Option<&mpsc::Sender<ActivateEvent>>,
) -> Result<()> {
    let mut config = apps::inspect(app).context("app not registered")?;
    emit(events, ActivateEvent::UploadComplete).await;

    // Sanity-check the release dir has a binary at the expected path.
    let release_dir = release_dir(app, sha)?;
    if !release_dir.is_dir() {
        bail!("release dir missing: {}", release_dir.display());
    }
    let binary_path = release_dir.join("bin").join(&config.command);
    if !binary_path.is_file() {
        bail!("binary missing at {}", binary_path.display());
    }

    // Read the binary's tracked-usage manifest (extracted at upload). If
    // missing — most commonly because this release was uploaded before the
    // CLI was teaching the agent to extract — extract it now, lazily. That
    // way "release already on box; skip rsync" can't strand us in a state
    // where the binary speaks `--shiku-manifest` but the agent never
    // listened.
    let manifest = match releases::read_manifest(app, sha).context("reading shiku manifest")? {
        Some(m) => Some(m),
        None => {
            tracing::info!(
                app = %app,
                sha = %sha,
                "no shiku manifest cached; extracting now"
            );
            if let Err(e) = releases::extract_manifest(app, sha).await {
                tracing::warn!(
                    error = %e,
                    "manifest extraction failed; binary may not support --shiku-manifest"
                );
                None
            } else {
                releases::read_manifest(app, sha)
                    .context("reading shiku manifest after extraction")?
            }
        }
    };

    // If we have a manifest, it's the source of truth for what platform
    // resources this service needs — overrides anything stale in the
    // on-disk app config.
    if let Some(manifest) = manifest {
        apply_manifest_to_config(&mut config, &manifest);

        // Allocate listen port now that we know whether the service wants
        // one. (Port lives in config.listen.http_port; ensure_allocated
        // is idempotent — keeps an existing assignment, allocates if
        // missing.)
        crate::ports::ensure_allocated(app, &mut config)
            .context("allocating listen port from manifest")?;

        // Persist the manifest-derived config back to disk so subsequent
        // ops see a config consistent with the latest release.
        apps::register(app, &config).context("re-persisting config after manifest")?;
    }

    // 0. Ensure the working dir exists so systemd can chdir into it on
    //    service start.
    let home = std::env::var("HOME").context("HOME not set")?;
    let working_dir = config
        .working_dir
        .clone()
        .unwrap_or_else(|| format!("{home}/data/{app}"));
    std::fs::create_dir_all(&working_dir)
        .with_context(|| format!("creating data dir {working_dir}"))?;

    // 1. Write the systemd unit.
    let unit_path = systemd_unit_path(&config.service)?;
    let unit_body = render_systemd_unit(&config, app)?;
    write_atomic(&unit_path, unit_body.as_bytes())
        .with_context(|| format!("writing {}", unit_path.display()))?;
    tracing::info!(unit = %unit_path.display(), "wrote systemd unit");
    emit(events, ActivateEvent::UnitWritten).await;

    // 2. Resolve bindings (other registered apps' listen addresses).
    let mut resolved_bindings =
        bindings::resolve_for_app(&config.bindings).context("resolving bindings")?;

    // Inject our own listen address as `SHIKU_LISTEN_HTTP` so the runtime
    // can bind to the right port. The agent owns the allocation (see
    // `ports::ensure_allocated`); we just publish the resolved port here.
    if let Some(listen) = &config.listen {
        if listen.http {
            let port = listen.http_port.ok_or_else(|| {
                anyhow!(
                    "app '{app}' wants http but has no allocated port; \
                     re-run `shiku app register` to allocate one"
                )
            })?;
            resolved_bindings.push(("SHIKU_LISTEN_HTTP".to_string(), port.to_string()));
        }
    }

    // 3. Resolve allowlisted secrets.
    let resolved_secrets =
        secrets::resolve_for_app(&config.secrets).context("resolving allowlisted secrets")?;

    // 4. Write the env file in tmpfs (mode 0600). Combines `vars` (plain) +
    //    bindings (`SHIKU_BIND_*`) + decrypted secrets. Plaintext lives only
    //    in this file and the spawned process's memory.
    let env_path = env_file_path(app)?;
    let env_body = render_env_file(&config, &resolved_bindings, &resolved_secrets);
    if let Some(parent) = env_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    write_atomic_with_mode(&env_path, env_body.as_bytes(), 0o600)
        .with_context(|| format!("writing {}", env_path.display()))?;
    tracing::info!(env_file = %env_path.display(), secrets = resolved_secrets.len(), "wrote env file");
    // Drop the resolved-secrets Vec — its SecretString fields zeroize on drop.
    drop(resolved_secrets);
    emit(events, ActivateEvent::SecretsResolved).await;

    // 3. Atomic symlink swap: current -> releases/<sha>; old current -> previous.
    swap_current_symlink(app, sha)?;

    // 4. systemctl daemon-reload + restart.
    systemctl(&["daemon-reload"]).await?;
    systemctl(&["restart", &format!("{}.service", config.service)]).await?;
    emit(events, ActivateEvent::ProcessStarting).await;

    // 5. Health check. On failure, the caller (`activate`) rolls back.
    run_health_check(&config, events).await?;
    emit(events, ActivateEvent::HealthCheckPassed).await;

    // 6. Prune old releases. Keep current + previous (handled by prune itself)
    //    plus a small backlog for emergency forensics. Best-effort: a prune
    //    failure shouldn't fail the deploy.
    if let Err(e) = releases::prune(app, RELEASES_TO_KEEP) {
        tracing::warn!(error = %e, "release prune failed (non-fatal)");
    }

    tracing::info!(app = %app, sha = %sha, "activation complete");
    Ok(())
}

/// Number of historical releases to retain past `current`/`previous`.
/// Tuned for "enough to debug a recent regression without filling disk."
const RELEASES_TO_KEEP: usize = 5;

/// Apply a release manifest to the on-disk app config, overwriting the
/// platform-need fields (secrets, bindings, listen) with what the binary
/// declares. Preserves agent-allocated state (e.g. `http_port`) when the
/// manifest still requests the same kind of resource.
pub(crate) fn apply_manifest_to_config(
    config: &mut shiku_types::AppConfig,
    manifest: &releases::ShikuManifest,
) {
    config.secrets = manifest.secrets.clone();
    config.bindings = manifest.bindings.clone();

    if manifest.listen_http {
        // Preserve any previously-allocated port; ports::ensure_allocated
        // will fill it in if absent.
        let http_port = config.listen.as_ref().and_then(|l| l.http_port);
        config.listen = Some(shiku_types::ListenSpec {
            http: true,
            http_port,
        });
    } else {
        config.listen = None;
    }
}

/// Restore the `previous` symlink to `current`, restart, and wait briefly.
///
/// Returns the sha now active (i.e. the previous release). `Err` if there
/// is no previous release to fall back to (first deploy of an app), in
/// which case the service is left stopped — that's the safest state for
/// "we couldn't activate and have nothing to fall back to."
async fn rollback_after_failure(app: &AppName) -> Result<Option<ReleaseSha>> {
    let config = apps::inspect(app).context("app not registered")?;
    let app_root = app_root(app)?;
    let current = app_root.join(CURRENT_SYMLINK);
    let previous = app_root.join(PREVIOUS_SYMLINK);

    if !previous.is_symlink() {
        // No previous to roll back to. Stop the service and surface the
        // situation; the caller will return a `Stuck` outcome.
        tracing::warn!("no previous release to rollback to; stopping service");
        let _ = systemctl(&["stop", &format!("{}.service", config.service)]).await;
        bail!("no previous release exists for app '{app}'");
    }

    // Read what we'll be falling back to so we can report it.
    let prev_target = std::fs::read_link(&previous).context("reading previous symlink")?;
    let prev_sha = prev_target
        .file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_string());

    // Move current aside (we'll discard it — that's the "bad" release).
    // Use a uniquely-named graveyard so concurrent rollbacks can't collide.
    let bad_link = app_root.join(format!(".bad.{}", std::process::id()));
    if current.exists() || current.is_symlink() {
        if bad_link.exists() || bad_link.is_symlink() {
            let _ = std::fs::remove_file(&bad_link);
        }
        std::fs::rename(&current, &bad_link)
            .with_context(|| format!("renaming bad current aside to {}", bad_link.display()))?;
    }

    // Promote previous → current via rename. Atomic.
    std::fs::rename(&previous, &current)
        .with_context(|| format!("promoting previous to {}", current.display()))?;

    // Drop the bad link — release dir on disk is untouched (releases/ is
    // append-only), so the bad release can still be re-targeted explicitly
    // if needed.
    let _ = std::fs::remove_file(&bad_link);

    tracing::info!(
        previous_sha = ?prev_sha,
        "promoted previous release back to current; restarting service"
    );

    // Restart the service against the restored binary.
    systemctl(&["daemon-reload"]).await?;
    systemctl(&["restart", &format!("{}.service", config.service)]).await?;

    // Best-effort: wait for the previously-known-good release to settle.
    // Use a short fixed window — this is recovery, not first-time activation.
    let wait_secs = match &config.health {
        HealthSpec::Process { settle_secs } => u64::from(*settle_secs).min(15),
        HealthSpec::Http { timeout_secs, .. } => u64::from(*timeout_secs).min(15),
    };
    if let Err(e) = wait_for_active(&config.service, wait_secs).await {
        tracing::warn!(
            error = %e,
            "previous release didn't return to active during rollback; service may need manual attention"
        );
        // Don't propagate — we did the rollback (symlink + restart). The
        // caller already knows activation failed; caller will report.
    }

    Ok(prev_sha)
}

/// Dispatch on the configured HealthSpec.
///
/// - `Process` — poll `systemctl is-active` until it reports `active`
///   continuously for `settle_secs`. Suits gateway bots, queue workers,
///   anything without an HTTP endpoint.
/// - `Http` — wait for `initial_delay_secs`, then poll the configured
///   localhost endpoint until a 2xx response (or a status in
///   `expect_status`).
async fn run_health_check(
    config: &AppConfig,
    events: Option<&mpsc::Sender<ActivateEvent>>,
) -> Result<()> {
    // We don't have per-poll hooks inside health.rs today; a single
    // "we're checking" event is enough for live UX. The terminal
    // HealthCheckPassed / HealthCheckFailed event is emitted by the
    // outer `activate_with_events_inner` based on the health-check
    // result.
    emit(
        events,
        ActivateEvent::HealthCheckAttempt {
            attempt: 1,
            ok: false,
            detail: Some("checking…".into()),
        },
    )
    .await;
    match &config.health {
        HealthSpec::Process { settle_secs } => {
            wait_for_active(&config.service, u64::from(*settle_secs)).await
        }
        HealthSpec::Http {
            path,
            initial_delay_secs,
            timeout_secs,
            interval_secs,
            expect_status,
        } => {
            let port = config
                .listen
                .as_ref()
                .filter(|l| l.http)
                .and_then(|l| l.http_port)
                .ok_or_else(|| {
                    anyhow!(
                        "HealthSpec::Http requires an HTTP listen port; \
                         declare `[listen] http = true` (the agent allocates the port) \
                         or switch to process-mode health"
                    )
                })?;
            health::wait_for_http(
                port,
                path,
                expect_status,
                *initial_delay_secs,
                *interval_secs,
                *timeout_secs,
            )
            .await
        }
    }
}

/// Poll `systemctl --user is-active` until it reports `active` continuously
/// for `settle_secs` seconds, or fail after a timeout.
async fn wait_for_active(service: &str, settle_secs: u64) -> Result<()> {
    let unit = format!("{service}.service");
    let timeout = Duration::from_secs(settle_secs.max(1));
    let start = std::time::Instant::now();
    let mut consecutive_active = 0u64;
    let needed_consecutive_polls = (settle_secs * 1000) / SETTLE_POLL_INTERVAL_MS.max(1);

    loop {
        if start.elapsed() > timeout + Duration::from_secs(2) {
            bail!(
                "timed out waiting for {unit} to settle as active (configured settle_secs={settle_secs})"
            );
        }

        let output = Command::new("systemctl")
            .args(["--user", "is-active", &unit])
            .output()
            .await
            .context("running systemctl is-active")?;
        let state = String::from_utf8_lossy(&output.stdout).trim().to_string();

        if state == "active" {
            consecutive_active += 1;
            if consecutive_active >= needed_consecutive_polls.max(1) {
                tracing::debug!(state = %state, "service settled active");
                return Ok(());
            }
        } else {
            // If the unit hits "failed" we'd want to surface that quickly
            // rather than sitting on the timeout.
            if state == "failed" {
                bail!("{unit} entered failed state during activation");
            }
            consecutive_active = 0;
        }

        tokio::time::sleep(Duration::from_millis(SETTLE_POLL_INTERVAL_MS)).await;
    }
}

/// Atomically swap `~/apps/<app>/current` to point at `releases/<sha>`,
/// preserving the old target as `previous` for rollback.
fn swap_current_symlink(app: &AppName, sha: &ReleaseSha) -> Result<()> {
    let app_root = app_root(app)?;
    let current = app_root.join(CURRENT_SYMLINK);
    let previous = app_root.join(PREVIOUS_SYMLINK);

    // The new target. Use a relative path so the symlink survives moves of
    // the user's home dir.
    let new_target = PathBuf::from(RELEASES_DIR_NAME).join(sha);

    // If `current` already points at the new sha, nothing to do.
    if current.exists() {
        if let Ok(existing) = std::fs::read_link(&current) {
            if existing == new_target {
                tracing::info!(
                    current = %current.display(),
                    target = %existing.display(),
                    "symlink already at requested release"
                );
                return Ok(());
            }
        }
    }

    // Move existing `current` aside as `previous`, dropping the old previous.
    if current.exists() || current.is_symlink() {
        if previous.exists() || previous.is_symlink() {
            std::fs::remove_file(&previous)
                .with_context(|| format!("removing old {}", previous.display()))?;
        }
        std::fs::rename(&current, &previous).with_context(|| {
            format!(
                "renaming {} -> {} (atomic swap aside)",
                current.display(),
                previous.display()
            )
        })?;
    }

    // Create the new `current` via tmpfile + rename for atomicity. Using
    // symlink + rename: write a uniquely-named symlink, then rename it to
    // `current` (atomic).
    let tmp = app_root.join(format!(".{CURRENT_SYMLINK}.tmp.{}", std::process::id()));
    if tmp.exists() || tmp.is_symlink() {
        let _ = std::fs::remove_file(&tmp);
    }
    std::os::unix::fs::symlink(&new_target, &tmp)
        .with_context(|| format!("creating tmp symlink {}", tmp.display()))?;
    std::fs::rename(&tmp, &current)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), current.display()))?;

    tracing::info!(
        current = %current.display(),
        target = %new_target.display(),
        "symlink swapped"
    );
    Ok(())
}

/// Render the systemd unit file. Bakes in our hardening defaults; per-app
/// overrides come from `AppConfig.systemd`.
fn render_systemd_unit(config: &AppConfig, app: &AppName) -> Result<String> {
    let home = std::env::var("HOME").context("HOME not set")?;
    let working_dir = config
        .working_dir
        .clone()
        .unwrap_or_else(|| format!("{home}/data/{app}"));

    let env_file = format!("%t/shiku/{app}.env");

    let SystemdSpec {
        memory_max,
        cpu_quota_percent,
        restart,
        restart_sec,
    } = &config.systemd;

    let restart_policy = restart.clone().unwrap_or_else(|| "on-failure".into());
    let restart_sec = restart_sec.unwrap_or(10);

    let mut s = String::new();
    s.push_str("# Generated by shiku — do not edit.\n");
    s.push_str("# Edits will be overwritten on the next activation.\n\n");
    s.push_str("[Unit]\n");
    s.push_str(&format!("Description=Shiku-managed service: {app}\n"));
    s.push_str("After=default.target\n");
    // StartLimit* belong in [Unit], not [Service] (systemd warns otherwise).
    s.push_str("StartLimitIntervalSec=300\n");
    s.push_str("StartLimitBurst=10\n\n");

    s.push_str("[Service]\n");
    s.push_str("Type=simple\n");
    s.push_str(&format!("WorkingDirectory={working_dir}\n"));
    s.push_str(&format!("EnvironmentFile={env_file}\n"));
    s.push_str(&format!(
        "ExecStart={home}/apps/{app}/current/bin/{cmd}\n",
        cmd = config.command
    ));
    s.push_str(&format!("Restart={restart_policy}\n"));
    s.push_str(&format!("RestartSec={restart_sec}\n\n"));

    // Hardening defaults.
    s.push_str("# Hardening (Shiku defaults)\n");
    s.push_str("NoNewPrivileges=yes\n");
    s.push_str("PrivateTmp=yes\n");
    s.push_str("ProtectSystem=strict\n");
    s.push_str("ProtectHome=read-only\n");
    s.push_str(&format!("ReadWritePaths={working_dir}\n"));
    s.push('\n');

    // Per-app resource limits.
    if let Some(m) = memory_max {
        s.push_str(&format!("MemoryMax={m}\n"));
    }
    if let Some(c) = cpu_quota_percent {
        s.push_str(&format!("CPUQuota={c}%\n"));
    }
    if memory_max.is_some() || cpu_quota_percent.is_some() {
        s.push('\n');
    }

    // Logging via journald — picked up automatically for Type=simple but
    // setting SyslogIdentifier makes `journalctl --user -t <name>` work.
    s.push_str("StandardOutput=journal\n");
    s.push_str("StandardError=journal\n");
    s.push_str(&format!("SyslogIdentifier={app}\n\n"));

    s.push_str("[Install]\n");
    s.push_str("WantedBy=default.target\n");

    Ok(s)
}

/// Render the runtime env file for systemd's `EnvironmentFile=`.
///
/// Three sources, all flattened into `KEY=VALUE` lines:
///   - `AppConfig.vars` — plain config from shiku.toml.
///   - `bindings` — resolved `SHIKU_BIND_<NAME>=<addr>` lines.
///   - `secrets` — decrypted age-encrypted values from the secrets store.
///
/// Cross-source name collisions are not detected — last write wins, in
/// declaration order (vars → bindings → secrets). In practice the namespaces
/// don't overlap (bindings always have the `SHIKU_BIND_` prefix; secrets are
/// SCREAMING_SNAKE; vars are user-chosen but rarely conflict).
fn render_env_file(
    config: &AppConfig,
    resolved_bindings: &[(String, String)],
    resolved_secrets: &[(String, secrecy::SecretString)],
) -> String {
    let mut s = String::new();
    s.push_str("# Generated by shiku — do not edit.\n");
    s.push_str("# Lives on tmpfs; recreated on every activation.\n");

    for (k, v) in &config.vars {
        s.push_str(&format_env_line(k, v));
    }
    for (k, v) in resolved_bindings {
        s.push_str(&format_env_line(k, v));
    }
    for (k, v) in resolved_secrets {
        s.push_str(&format_env_line(k, v.expose_secret()));
    }
    s
}

fn format_env_line(key: &str, value: &str) -> String {
    // systemd's EnvironmentFile parser treats one line as one var. Newlines
    // would break that (and are invalid in env vars anyway), so neutralise.
    if value.contains('\n') {
        tracing::warn!(key, "value contains newline; replacing with space");
        let safe = value.replace('\n', " ");
        format!("{key}={safe}\n")
    } else {
        format!("{key}={value}\n")
    }
}

/// `~/apps/<app>/`
fn app_root(app: &AppName) -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(APP_HOME_DIR_NAME).join(app))
}

fn release_dir(app: &AppName, sha: &ReleaseSha) -> Result<PathBuf> {
    Ok(app_root(app)?.join(RELEASES_DIR_NAME).join(sha))
}

fn systemd_unit_path(service: &str) -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home)
        .join(".config/systemd/user")
        .join(format!("{service}.service")))
}

fn env_file_path(app: &AppName) -> Result<PathBuf> {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            let uid = nix::unistd::Uid::current().as_raw();
            Some(PathBuf::from(format!("/run/user/{uid}")))
        })
        .ok_or_else(|| anyhow!("could not resolve XDG_RUNTIME_DIR"))?;
    Ok(runtime_dir.join("shiku").join(format!("{app}.env")))
}

/// Restart a service, regenerating its env file from current secrets +
/// bindings + vars. This is what `shiku restart` invokes — it's a
/// deliberate alias for "re-apply config to the running release."
///
/// Why we regen the env file rather than just restarting the unit: the
/// most common reason to restart is "I changed a secret" or "I changed
/// a var." A bare `systemctl restart` would pick up the *cached* env
/// file from the last activation, silently discarding the change. Doing
/// the regen here makes `shiku secret set X && shiku restart` work as
/// the user expects.
pub async fn restart_app(app: &AppName) -> Result<()> {
    let mut config = apps::inspect(app).context("app not registered")?;

    // Apply the latest release's manifest if available — keeps the
    // restart consistent with what `Activate` would do for the same
    // release. (Most restarts happen against an existing `current`
    // symlink, so the manifest from that release is the right source.)
    if let Some(current_sha) = current_release_sha(app)? {
        if let Some(manifest) = releases::read_manifest(app, &current_sha)? {
            apply_manifest_to_config(&mut config, &manifest);
            crate::ports::ensure_allocated(app, &mut config)
                .context("re-allocating listen port during restart")?;
            apps::register(app, &config).context("re-persisting config during restart")?;
        }
    }

    // Resolve current bindings + secrets and rewrite the env file.
    let mut resolved_bindings =
        bindings::resolve_for_app(&config.bindings).context("resolving bindings during restart")?;
    if let Some(listen) = &config.listen {
        if listen.http {
            let port = listen.http_port.ok_or_else(|| {
                anyhow!(
                    "app '{app}' wants http but has no allocated port; \
                     run `shiku app register` to allocate one"
                )
            })?;
            resolved_bindings.push(("SHIKU_LISTEN_HTTP".to_string(), port.to_string()));
        }
    }
    let resolved_secrets =
        secrets::resolve_for_app(&config.secrets).context("resolving secrets during restart")?;

    let env_path = env_file_path(app)?;
    let env_body = render_env_file(&config, &resolved_bindings, &resolved_secrets);
    if let Some(parent) = env_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    write_atomic_with_mode(&env_path, env_body.as_bytes(), 0o600)
        .with_context(|| format!("writing {}", env_path.display()))?;
    drop(resolved_secrets);

    // Best-effort start-limit reset — if the unit hit StartLimitBurst
    // because of previous failures (stale secrets, missing config, etc.)
    // a plain `systemctl restart` would refuse. We *want* to retry.
    let _ = systemctl(&["reset-failed", &format!("{}.service", config.service)]).await;

    // daemon-reload picks up any unit-level config changes; restart
    // applies the new env file.
    systemctl(&["daemon-reload"]).await?;
    systemctl(&["restart", &format!("{}.service", config.service)]).await?;

    tracing::info!(app = %app, "restarted with regenerated env");
    Ok(())
}

/// Read the sha currently pointed at by `~/apps/<app>/current`, if any.
pub(crate) fn current_release_sha(app: &AppName) -> Result<Option<ReleaseSha>> {
    let app_root = app_root(app)?;
    let current = app_root.join(CURRENT_SYMLINK);
    if !current.is_symlink() {
        return Ok(None);
    }
    let target =
        std::fs::read_link(&current).with_context(|| format!("reading {}", current.display()))?;
    Ok(target
        .file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_string()))
}

/// Manual rollback: promote `previous` to `current` and restart, regardless of
/// whether the current activation is healthy. Used by `shiku rollback` when
/// an operator has decided "the current version is bad, go back."
///
/// Differs from `rollback_after_failure` in one important way: when there's
/// no previous release to fall back to, we leave the current (running) service
/// alone and just return an error. The auto-rollback path stops the broken
/// service in the same situation because it knows the *current* attempt
/// failed; the manual path can't make that assumption.
///
/// Returns the sha now active.
pub async fn manual_rollback(app: &AppName) -> Result<Option<ReleaseSha>> {
    let app_root = app_root(app)?;
    if !app_root.join(PREVIOUS_SYMLINK).is_symlink() {
        bail!("no previous release exists for app '{app}'");
    }
    rollback_after_failure(app).await
}

async fn systemctl(args: &[&str]) -> Result<()> {
    let mut cmd = Command::new("systemctl");
    cmd.arg("--user");
    cmd.args(args);
    let output = cmd
        .output()
        .await
        .with_context(|| format!("running systemctl --user {}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!(
            "systemctl --user {} failed (exit {}): {}",
            args.join(" "),
            output.status,
            stderr
        );
    }
    Ok(())
}

fn write_atomic(path: &Path, body: &[u8]) -> Result<()> {
    write_atomic_with_mode(path, body, 0o644)
}

fn write_atomic_with_mode(path: &Path, body: &[u8], mode: u32) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("path has no parent: {}", path.display()))?;
    let tmp_name = format!(
        ".{}.tmp.{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
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
    use serial_test::serial;
    use shiku_types::{HealthSpec, SystemdSpec};
    use std::collections::BTreeMap;

    fn fake_config() -> AppConfig {
        AppConfig {
            name: "test-app".into(),
            command: "test-app".into(),
            service: "test-app".into(),
            working_dir: Some("/home/user/data/test-app".into()),
            secrets: vec![],
            bindings: vec![],
            vars: BTreeMap::from([
                ("RUST_LOG".into(), "info".into()),
                ("BOT_ENV".into(), "production".into()),
            ]),
            listen: None,
            health: HealthSpec::Process { settle_secs: 5 },
            systemd: SystemdSpec {
                memory_max: Some("512M".into()),
                cpu_quota_percent: Some(100),
                ..Default::default()
            },
            public: vec![],
        }
    }

    #[test]
    #[serial(home_env)]
    fn unit_render_includes_essentials() {
        std::env::set_var("HOME", "/home/user");
        let unit = render_systemd_unit(&fake_config(), &"test-app".into()).unwrap();
        assert!(unit.contains("Description=Shiku-managed service: test-app"));
        assert!(unit.contains("ExecStart=/home/user/apps/test-app/current/bin/test-app"));
        assert!(unit.contains("EnvironmentFile=%t/shiku/test-app.env"));
        assert!(unit.contains("MemoryMax=512M"));
        assert!(unit.contains("CPUQuota=100%"));
        assert!(unit.contains("NoNewPrivileges=yes"));
        assert!(unit.contains("ProtectSystem=strict"));
        assert!(unit.contains("Restart=on-failure"));
    }

    #[test]
    fn env_file_render_alphabetises() {
        let env = render_env_file(&fake_config(), &[], &[]);
        // BTreeMap → alphabetical → BOT_ENV before RUST_LOG.
        let bot_idx = env.find("BOT_ENV=").unwrap();
        let log_idx = env.find("RUST_LOG=").unwrap();
        assert!(bot_idx < log_idx);
        assert!(env.contains("BOT_ENV=production"));
        assert!(env.contains("RUST_LOG=info"));
    }

    #[test]
    fn env_file_neutralises_newlines() {
        let mut config = fake_config();
        config
            .vars
            .insert("MULTILINE".into(), "first\nsecond".into());
        let env = render_env_file(&config, &[], &[]);
        assert!(env.contains("MULTILINE=first second"));
        assert!(!env.contains("first\nsecond"));
    }

    #[test]
    fn env_file_includes_secrets() {
        let config = fake_config();
        let secrets = vec![
            (
                "BOT_TOKEN".to_string(),
                secrecy::SecretString::new("abc123".into()),
            ),
            (
                "JWT_SECRET".to_string(),
                secrecy::SecretString::new("def456".into()),
            ),
        ];
        let env = render_env_file(&config, &[], &secrets);
        assert!(env.contains("BOT_TOKEN=abc123"));
        assert!(env.contains("JWT_SECRET=def456"));
        assert!(env.contains("RUST_LOG=info"));
    }

    #[test]
    fn env_file_includes_bindings() {
        let config = fake_config();
        let bindings = vec![
            (
                "SHIKU_BIND_NARRATOR".to_string(),
                "http://127.0.0.1:9090".to_string(),
            ),
            (
                "SHIKU_BIND_OWNERSHIP".to_string(),
                "http://127.0.0.1:9091".to_string(),
            ),
        ];
        let env = render_env_file(&config, &bindings, &[]);
        assert!(env.contains("SHIKU_BIND_NARRATOR=http://127.0.0.1:9090"));
        assert!(env.contains("SHIKU_BIND_OWNERSHIP=http://127.0.0.1:9091"));
    }

    /// Symlink swap test: simulates two consecutive activations and verifies
    /// the on-disk shape (`current` → new, `previous` → old).
    ///
    /// We invoke the swap logic directly against a tempdir-backed `~/apps`
    /// to avoid needing systemd. Tests the file-system half of activation
    /// without the systemctl half.
    #[test]
    #[serial(home_env)]
    fn symlink_swap_preserves_previous() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().to_path_buf();
        std::env::set_var("HOME", &home);

        let app: AppName = format!("symlink-test-{}", std::process::id());
        let app_dir = home.join("apps").join(&app);
        std::fs::create_dir_all(app_dir.join("releases/sha-a")).unwrap();
        std::fs::create_dir_all(app_dir.join("releases/sha-b")).unwrap();

        // First swap: no current, no previous — should just create current.
        swap_current_symlink(&app, &"sha-a".into()).unwrap();
        assert_eq!(
            std::fs::read_link(app_dir.join("current")).unwrap(),
            std::path::PathBuf::from("releases/sha-a")
        );
        assert!(!app_dir.join("previous").exists());

        // Second swap: current=sha-a → previous=sha-a, current=sha-b.
        swap_current_symlink(&app, &"sha-b".into()).unwrap();
        assert_eq!(
            std::fs::read_link(app_dir.join("current")).unwrap(),
            std::path::PathBuf::from("releases/sha-b")
        );
        assert_eq!(
            std::fs::read_link(app_dir.join("previous")).unwrap(),
            std::path::PathBuf::from("releases/sha-a")
        );

        // Third swap with the same sha as current — no-op.
        swap_current_symlink(&app, &"sha-b".into()).unwrap();
        // previous should still be sha-a (we shouldn't have overwritten it).
        assert_eq!(
            std::fs::read_link(app_dir.join("previous")).unwrap(),
            std::path::PathBuf::from("releases/sha-a")
        );
    }
}
