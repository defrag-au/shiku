//! `shikud` — Shiku agent daemon.
//!
//! Runs as a `systemd --user` service per service-user on the box. Listens on
//! `/run/user/<uid>/shikud.sock` and serves Shiku protocol requests.
//!
//! See `docs/deploying.md` for the deploy workflow.
//!
//! Phase 1 status: socket listener answering Ping (Step 1.5). Real request
//! handling (apps, releases, secrets, activation) lands in Phase 2+.

#![forbid(unsafe_code)]

mod activate;
mod apps;
mod bindings;
mod cf_api;
mod cloudflared;
mod cloudflared_release;
mod health;
mod ingress;
mod logs;
mod ports;
mod public_validation;
mod publish;
mod releases;
mod secrets;
mod server_config;
mod status;
mod tunnel_bootstrap;

use std::path::PathBuf;

use anyhow::{Context, Result};
use secrecy::SecretString;
use shiku_types::{framing, ErrorKind, Request, Response};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{signal, SignalKind};

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let socket_path = resolve_socket_path()?;
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        socket = %socket_path.display(),
        "shikud starting"
    );

    // Remove a stale socket from a previous run (the OS doesn't unlink
    // unix sockets on process exit; we have to do it ourselves).
    if socket_path.exists() {
        std::fs::remove_file(&socket_path)
            .with_context(|| format!("removing stale socket at {}", socket_path.display()))?;
        tracing::warn!("removed stale socket from previous run");
    }

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("binding {}", socket_path.display()))?;

    // Tighten socket file permissions to 0600. systemd-user runtime dir is
    // already user-only (0700) so this is defense in depth.
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&socket_path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&socket_path, perms)?;
    }

    tracing::info!("listening; press Ctrl-C or send SIGTERM to stop");

    let mut sigterm = signal(SignalKind::terminate()).context("installing SIGTERM handler")?;
    let mut sigint = signal(SignalKind::interrupt()).context("installing SIGINT handler")?;

    let result = loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        tokio::spawn(async move {
                            if let Err(e) = handle_connection(stream).await {
                                tracing::warn!(error = %e, "connection handler failed");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "accept failed");
                    }
                }
            }
            _ = sigterm.recv() => {
                tracing::info!("SIGTERM received, shutting down");
                break Ok(());
            }
            _ = sigint.recv() => {
                tracing::info!("SIGINT received, shutting down");
                break Ok(());
            }
        }
    };

    // Best-effort cleanup; ignore failures (the process is exiting anyway).
    let _ = std::fs::remove_file(&socket_path);

    result
}

/// One connection per request. Read a length-prefixed frame, dispatch, write
/// the response, close.
///
/// Streaming responses (Logs, Activate) will keep the connection open and
/// emit multiple frames; that's a Phase 2 concern.
async fn handle_connection(mut stream: UnixStream) -> Result<()> {
    // Read the 4-byte length prefix.
    let mut prefix = [0u8; framing::LEN_PREFIX_BYTES];
    stream
        .read_exact(&mut prefix)
        .await
        .context("reading length prefix")?;
    let body_len = framing::parse_len_prefix(prefix).context("parsing length prefix")?;

    // Read the body.
    let mut body = vec![0u8; body_len as usize];
    stream.read_exact(&mut body).await.context("reading body")?;

    // Decode. Malformed messages get a structured error response rather than
    // a closed connection — easier to debug.
    let request: Request = match framing::decode(&body) {
        Ok(req) => req,
        Err(e) => {
            tracing::warn!(error = %e, "frame decode failed");
            let response = Response::Error {
                kind: ErrorKind::BadRequest,
                detail: format!("frame decode failed: {e}"),
            };
            return write_response(&mut stream, &response).await;
        }
    };

    tracing::debug!(?request, "request received");

    // Streaming requests bypass the single-response dispatch and write
    // their own frames directly.
    match request {
        Request::Logs { app, since, follow } => {
            return logs::stream(&app, since.as_deref(), follow, &mut stream)
                .await
                .context("logs stream");
        }
        Request::Activate { app, sha } => {
            return activate_streaming(&app, &sha, &mut stream).await;
        }
        other => {
            // Single-response requests fall through to dispatch.
            let response = dispatch(other).await;
            tracing::debug!(?response, "response prepared");
            return write_response(&mut stream, &response).await;
        }
    }
}

/// Streaming wrapper around `activate::activate`. Emits
/// `Response::ActivateProgress(...)` frames as the agent walks through
/// the activation steps, then a terminal `Response::Ok` (success) or
/// `Response::Error` (rollback / stuck). The CLI reads frames until the
/// terminal one and renders progress live.
async fn activate_streaming(
    app: &shiku_types::AppName,
    sha: &shiku_types::ReleaseSha,
    stream: &mut tokio::net::UnixStream,
) -> Result<()> {
    use activate::ActivationOutcome::*;
    use shiku_types::{ActivateEvent, Response};
    use tokio::sync::mpsc;

    // The activation flow runs on a dedicated task and posts events
    // to a channel as it makes progress. We pump the channel out to
    // the wire here. Bounded channel is fine — the events are small
    // and infrequent (≤1 per second from the health-check loop).
    let (tx, mut rx) = mpsc::channel::<ActivateEvent>(32);
    let app_owned = app.clone();
    let sha_owned = sha.clone();
    let activation =
        tokio::spawn(
            async move { activate::activate_with_events(&app_owned, &sha_owned, tx).await },
        );

    // Drain events to the wire as they arrive.
    while let Some(event) = rx.recv().await {
        let frame = framing::encode(&Response::ActivateProgress(event))
            .context("encoding ActivateProgress")?;
        if let Err(e) = stream.write_all(&frame).await {
            tracing::debug!(error = %e, "client closed during activate stream");
            break;
        }
    }

    // Channel closed → activation task is done. Collect its outcome.
    let outcome = activation.await.context("activation task panicked")?;
    let terminal = match outcome {
        Activated => Response::Ok,
        RolledBack {
            reason,
            previous_sha,
        } => {
            let prev = previous_sha.as_deref().unwrap_or("(unknown)");
            Response::Error {
                kind: ErrorKind::HealthCheckTimeout,
                detail: format!("activation failed and rolled back to {prev}: {reason}"),
            }
        }
        Stuck {
            activation_error,
            rollback_error,
        } => Response::Error {
            kind: ErrorKind::Internal,
            detail: format!(
                "activation failed AND rollback failed; service may be down. \
                 activation error: {activation_error}; rollback error: {rollback_error}"
            ),
        },
    };
    let frame = framing::encode(&terminal).context("encoding terminal Response")?;
    let _ = stream.write_all(&frame).await;
    let _ = stream.shutdown().await;
    Ok(())
}

async fn dispatch(request: Request) -> Response {
    match request {
        Request::Ping => Response::Pong,

        Request::AppList => match apps::list_user_visible() {
            Ok(names) => Response::Apps(names),
            Err(e) => internal_error(e),
        },

        Request::AppRegister { name, mut config } => {
            // Manifest-derived fields (listen, secrets, bindings) aren't
            // declared in shiku.toml — they come from the binary's
            // `--shiku-manifest` output at activation time. The CLI's
            // ResolvedApp can't see them, so it sends them empty / None.
            // The on-disk config (last set by activation) is the source
            // of truth; preserve it across simple re-register calls or
            // we'd silently wipe the allocated port and the secrets
            // allowlist on every `shiku app register`.
            let previous_public = match publish::current_public(&name) {
                Ok(p) => p,
                Err(e) => return internal_error(e),
            };
            if let Ok(existing) = apps::inspect(&name) {
                // listen: if CLI sent nothing, use existing.
                // If CLI sent Some but no port, fill in the existing port.
                match (existing.listen, config.listen.as_mut()) {
                    (Some(existing_listen), None) => {
                        config.listen = Some(existing_listen);
                    }
                    (Some(existing_listen), Some(incoming_listen))
                        if incoming_listen.http_port.is_none() =>
                    {
                        incoming_listen.http_port = existing_listen.http_port;
                    }
                    _ => {}
                }
                // secrets / bindings: CLI never sets these (they live in
                // manifest), so always preserve existing on re-register.
                if config.secrets.is_empty() && !existing.secrets.is_empty() {
                    config.secrets = existing.secrets;
                }
                if config.bindings.is_empty() && !existing.bindings.is_empty() {
                    config.bindings = existing.bindings;
                }
            }

            // If listen is still missing but there's a current release
            // with a cached manifest, re-apply it. Heals the edge case
            // where on-disk state lost the listen block (e.g. a stale
            // pre-fix register call wiped it) — operator shouldn't have
            // to know to run `shiku restart` to recover.
            if config.listen.is_none() {
                if let Ok(Some(sha)) = activate::current_release_sha(&name) {
                    if let Ok(Some(manifest)) = releases::read_manifest(&name, &sha) {
                        activate::apply_manifest_to_config(&mut config, &manifest);
                        if let Err(e) = ports::ensure_allocated(&name, &mut config) {
                            tracing::warn!(error = %e, "port allocate during register manifest restore");
                        }
                    }
                }
            }

            // Validate the proposed `public` list before persisting. If
            // the box hasn't been tunnel-bootstrapped (no server.toml),
            // any non-empty `public` list is rejected — there's nowhere
            // to publish to.
            if !config.public.is_empty() {
                if let Err(resp) = validate_public_against_box(&name, &config.public) {
                    return resp;
                }
            }

            // Persist first, then reconcile against DNS + ingress. If
            // reconcile fails the on-disk state still reflects the
            // operator's intent, and a follow-up register call will
            // resume the diff from "what's already applied" toward the
            // desired list — i.e. idempotent recovery.
            if let Err(e) = apps::register(&name, &config) {
                return bad_request(e);
            }

            // Always run reconcile when there are public hostnames or when
            // the previous state had any (so we clean them up). Reconcile
            // is naturally idempotent — DNS upserts no-op if the record
            // matches, ingress regen produces deterministic output from
            // the apps registry, SIGHUP is cheap. Gating on diff would
            // mean a previous failed reconcile (state persisted but
            // ingress not regenerated) silently stays broken on retry.
            if !previous_public.is_empty() || !config.public.is_empty() {
                let outcome = publish::reconcile(&name, &previous_public, &config.public).await;
                if let Some((kind, detail)) = outcome.as_response_parts() {
                    return Response::Error { kind, detail };
                }
            }

            Response::Ok
        }

        Request::AppRemove { name } => {
            // Auto-unpublish any hostnames the app currently owns. The
            // operator declared them; declaring "this app no longer
            // exists" implies "those hostnames go away too." We do the
            // ingress + DNS cleanup before deleting the app dir so a
            // partial failure leaves recoverable state — re-running
            // remove still works.
            let previous_public = match publish::current_public(&name) {
                Ok(p) => p,
                Err(e) => return internal_error(e),
            };
            if !previous_public.is_empty() {
                let outcome = publish::reconcile(&name, &previous_public, &[]).await;
                if let Some((kind, detail)) = outcome.as_response_parts() {
                    return Response::Error { kind, detail };
                }
            }
            match apps::remove(&name) {
                Ok(true) => Response::Ok,
                Ok(false) => Response::Error {
                    kind: ErrorKind::AppNotFound,
                    detail: format!("app '{name}' is not registered"),
                },
                Err(e) => internal_error(e),
            }
        }

        Request::AppInspect { name } => match apps::inspect(&name) {
            Ok(config) => Response::AppDetail(Box::new(config)),
            Err(e) if format!("{e}").contains("not registered") => Response::Error {
                kind: ErrorKind::AppNotFound,
                detail: format!("app '{name}' is not registered"),
            },
            Err(e) => internal_error(e),
        },

        Request::ReleaseUpload { app, sha, manifest } => {
            match releases::verify_and_finalize(&app, &sha, &manifest) {
                Ok(()) => {
                    // Best-effort manifest extraction. A binary that
                    // doesn't support `--shiku-manifest` isn't a fatal
                    // error — it just gets an empty manifest at
                    // activation and won't have any auto-allocated
                    // platform resources.
                    if let Err(e) = releases::extract_manifest(&app, &sha).await {
                        tracing::warn!(
                            error = %e,
                            "failed to extract shiku manifest from binary; \
                             continuing without it (binary may not support \
                             --shiku-manifest)"
                        );
                    }
                    Response::Ok
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    if msg.contains("not registered") {
                        Response::Error {
                            kind: ErrorKind::AppNotFound,
                            detail: format!("app '{app}' is not registered"),
                        }
                    } else if msg.contains("hash mismatch")
                        || msg.contains("binary missing")
                        || msg.contains("release dir missing")
                    {
                        Response::Error {
                            kind: ErrorKind::ReleaseNotFound,
                            detail: msg,
                        }
                    } else {
                        bad_request(e)
                    }
                }
            }
        }

        // Request::Activate is handled as a streaming request in
        // handle_connection() — `activate_streaming` writes its own
        // frames directly. Should never reach dispatch.
        Request::Status { app } => match status::collect(app.as_ref()).await {
            Ok(snapshots) => Response::Status(snapshots),
            Err(e) => internal_error(e),
        },

        Request::ReleaseList { app } => match releases::list(&app) {
            Ok(infos) => Response::Releases(infos),
            Err(e) if format!("{e}").contains("not registered") => Response::Error {
                kind: ErrorKind::AppNotFound,
                detail: format!("app '{app}' is not registered"),
            },
            Err(e) => internal_error(e),
        },

        Request::SecretSet { name, value } => {
            let secret = SecretString::new(value.into());
            match secrets::set(&name, secret) {
                Ok(()) => Response::Ok,
                Err(e) => bad_request(e),
            }
        }

        Request::SecretList => match secrets::list() {
            Ok(names) => Response::Secrets(names),
            Err(e) => internal_error(e),
        },

        Request::SecretRemove { name } => match secrets::remove(&name) {
            Ok(true) => Response::Ok,
            Ok(false) => Response::Error {
                kind: ErrorKind::SecretNotFound,
                detail: format!("secret '{name}' is not set"),
            },
            Err(e) => bad_request(e),
        },

        Request::Rollback { app } => match activate::manual_rollback(&app).await {
            Ok(Some(_sha)) => Response::Ok,
            Ok(None) => Response::Error {
                kind: ErrorKind::ReleaseNotFound,
                detail: format!("no previous release for app '{app}' to roll back to"),
            },
            Err(e) => {
                let msg = format!("{e:#}");
                if msg.contains("not registered") {
                    Response::Error {
                        kind: ErrorKind::AppNotFound,
                        detail: format!("app '{app}' is not registered"),
                    }
                } else if msg.contains("no previous release") {
                    Response::Error {
                        kind: ErrorKind::ReleaseNotFound,
                        detail: msg,
                    }
                } else {
                    internal_error(e)
                }
            }
        },

        Request::TunnelStatus => match build_tunnel_status().await {
            Ok(report) => Response::TunnelStatus(report),
            Err(e) => internal_error(e),
        },

        Request::TunnelZones => match build_tunnel_zones() {
            Ok(zones) => Response::TunnelZones(zones),
            Err(e) => internal_error(e),
        },

        Request::TunnelUpgrade => match perform_tunnel_upgrade().await {
            Ok(out) => Response::TunnelUpgrade {
                version: out.version,
                changed: out.changed,
            },
            Err(e) => internal_error(e),
        },

        Request::TunnelBootstrap {
            mode,
            api_token,
            zones,
        } => match tunnel_bootstrap::bootstrap(&mode, &api_token, &zones).await {
            Ok(()) => Response::Ok,
            Err(e) => {
                let msg = format!("{e:#}");
                if msg.contains("not in the API token") {
                    Response::Error {
                        kind: ErrorKind::ZoneNotManaged,
                        detail: msg,
                    }
                } else if msg.contains("listing zones") {
                    Response::Error {
                        kind: ErrorKind::DnsApiFailed,
                        detail: msg,
                    }
                } else {
                    Response::Error {
                        kind: ErrorKind::Internal,
                        detail: msg,
                    }
                }
            }
        },

        Request::Restart { app } => match activate::restart_app(&app).await {
            Ok(()) => Response::Ok,
            Err(e) => {
                let msg = format!("{e:#}");
                if msg.contains("not registered") {
                    Response::Error {
                        kind: ErrorKind::AppNotFound,
                        detail: format!("app '{app}' is not registered"),
                    }
                } else if msg.contains("systemctl") {
                    Response::Error {
                        kind: ErrorKind::SystemdError,
                        detail: msg,
                    }
                } else {
                    Response::Error {
                        kind: ErrorKind::Internal,
                        detail: msg,
                    }
                }
            }
        },

        other => {
            let detail = format!("{} not implemented yet (Phase 2+)", short_name(&other));
            Response::Error {
                kind: ErrorKind::Unsupported,
                detail,
            }
        }
    }
}

/// Collect the box's tunnel state for `Request::TunnelStatus`. Pulls
/// from `~/.config/shiku/server.toml` (tunnel id + zones), the
/// cloudflared systemd unit (active state), and the apps registry
/// (current ingress mapping).
async fn build_tunnel_status() -> anyhow::Result<shiku_types::TunnelStatusReport> {
    let server = server_config::load()?;
    let (tunnel_id, zones) = match server.as_ref() {
        Some(cfg) => (
            Some(cfg.tunnel.id.clone()),
            cfg.zones.iter().map(|z| z.name.clone()).collect(),
        ),
        None => (None, Vec::new()),
    };

    let cloudflared_state = match cloudflared::is_active().await {
        Ok(true) => "active".to_string(),
        Ok(false) => "inactive".to_string(),
        Err(e) => format!("unknown ({e})"),
    };

    // Walk the apps registry to compute the live ingress mapping.
    let mut ingress = Vec::new();
    if let Ok(names) = apps::list() {
        for name in names {
            if apps::is_pseudo(&name) {
                continue;
            }
            let cfg = match apps::inspect(&name) {
                Ok(c) => c,
                Err(_) => continue,
            };
            if cfg.public.is_empty() {
                continue;
            }
            let port = cfg
                .listen
                .as_ref()
                .filter(|l| l.http)
                .and_then(|l| l.http_port);
            for hostname in cfg.public {
                ingress.push(shiku_types::TunnelIngressRule {
                    hostname,
                    app: name.clone(),
                    port: port.unwrap_or(0),
                });
            }
        }
    }
    ingress.sort_by(|a, b| a.hostname.cmp(&b.hostname));

    Ok(shiku_types::TunnelStatusReport {
        tunnel_id,
        zones,
        cloudflared_state,
        ingress,
    })
}

/// Perform a tunnel binary upgrade: stop the cloudflared unit if it's
/// running, swap in the pinned binary, restart. Idempotent — if the
/// binary already matches the pin, the swap is a no-op and the unit
/// is left running.
async fn perform_tunnel_upgrade() -> anyhow::Result<cloudflared_release::UpgradeResult> {
    use anyhow::Context;
    let was_active = cloudflared::is_active().await.unwrap_or(false);
    if was_active {
        tracing::info!("stopping cloudflared for binary swap");
        cloudflared::stop()
            .await
            .context("stopping cloudflared before upgrade")?;
    }

    let result = cloudflared_release::upgrade()
        .await
        .context("downloading + verifying cloudflared")?;

    if was_active {
        tracing::info!("restarting cloudflared after binary swap");
        cloudflared::restart()
            .await
            .context("restarting cloudflared after upgrade")?;
    }

    Ok(result)
}

fn build_tunnel_zones() -> anyhow::Result<Vec<shiku_types::TunnelZoneInfo>> {
    let cfg = server_config::load()?;
    Ok(cfg
        .map(|c| {
            c.zones
                .into_iter()
                .map(|z| shiku_types::TunnelZoneInfo {
                    name: z.name,
                    zone_id: z.zone_id,
                })
                .collect()
        })
        .unwrap_or_default())
}

/// Validate one app's proposed `public` list against the box's tunnel
/// config and other apps' existing claims. Returns `Ok(())` on pass, or
/// a fully-formed `Response::Error` to short-circuit dispatch.
///
/// Phase-3 scope: validation only, no DNS or ingress mutation. The agent
/// will refuse to *persist* a config with an invalid public list, but
/// hasn't learned to actually publish anything yet.
fn validate_public_against_box(
    self_app: &shiku_types::AppName,
    proposed: &[String],
) -> Result<(), Response> {
    use public_validation::{validate_list, ExistingClaim, ValidationError};

    let server_cfg = match server_config::load() {
        Ok(Some(cfg)) => cfg,
        Ok(None) => {
            return Err(Response::Error {
                kind: ErrorKind::ZoneNotManaged,
                detail: format!(
                    "this box has no tunnel configured (no `~/.config/shiku/server.toml`); \
                     run `shiku tunnel bootstrap` before declaring `public = [...]` for \
                     app `{self_app}`"
                ),
            });
        }
        Err(e) => {
            tracing::warn!(error = %e, "reading server.toml");
            return Err(Response::Error {
                kind: ErrorKind::Internal,
                detail: format!("reading server.toml: {e:#}"),
            });
        }
    };

    // Gather every other app's currently-registered public list.
    let other_apps = match apps::list() {
        Ok(names) => names,
        Err(e) => {
            tracing::warn!(error = %e, "listing apps for public-conflict check");
            return Err(Response::Error {
                kind: ErrorKind::Internal,
                detail: format!("listing apps: {e:#}"),
            });
        }
    };

    // Owned (app_name, hostname) pairs for stable borrows.
    let mut owned_claims: Vec<(shiku_types::AppName, String)> = Vec::new();
    for other in &other_apps {
        if other == self_app {
            continue;
        }
        match apps::inspect(other) {
            Ok(cfg) => {
                for h in cfg.public {
                    owned_claims.push((other.clone(), h));
                }
            }
            Err(e) => {
                tracing::warn!(app = %other, error = %e, "inspecting app for claims");
                // A corrupt config is a real problem but shouldn't block
                // every register — flag it and move on. The agent's
                // `apps::list` already filtered by config.toml presence,
                // so this is rare.
            }
        }
    }
    let claim_refs: Vec<ExistingClaim<'_>> = owned_claims
        .iter()
        .map(|(app, host)| ExistingClaim {
            app,
            hostname: host.as_str(),
        })
        .collect();

    match validate_list(self_app, proposed, &server_cfg, &claim_refs) {
        Ok(()) => Ok(()),
        Err(ve) => {
            let kind = match ve {
                ValidationError::ZoneNotManaged { .. } => ErrorKind::ZoneNotManaged,
                ValidationError::MultiLevelSubdomain { .. }
                | ValidationError::ZoneApexNotAllowed { .. } => {
                    ErrorKind::MultiLevelSubdomainUnsupported
                }
                ValidationError::AlreadyClaimed { .. } => ErrorKind::HostnameAlreadyClaimed,
                ValidationError::InvalidHostname { .. } => ErrorKind::BadRequest,
            };
            Err(Response::Error {
                kind,
                detail: ve.detail(),
            })
        }
    }
}

fn internal_error(e: anyhow::Error) -> Response {
    tracing::warn!(error = %e, "internal error");
    Response::Error {
        kind: ErrorKind::Internal,
        detail: format!("{e:#}"),
    }
}

fn bad_request(e: anyhow::Error) -> Response {
    tracing::warn!(error = %e, "bad request");
    Response::Error {
        kind: ErrorKind::BadRequest,
        detail: format!("{e:#}"),
    }
}

fn short_name(req: &Request) -> &'static str {
    match req {
        Request::Ping => "Ping",
        Request::AppRegister { .. } => "AppRegister",
        Request::AppRemove { .. } => "AppRemove",
        Request::AppList => "AppList",
        Request::AppInspect { .. } => "AppInspect",
        Request::ReleaseUpload { .. } => "ReleaseUpload",
        Request::ReleaseList { .. } => "ReleaseList",
        Request::ReleasePrune { .. } => "ReleasePrune",
        Request::Activate { .. } => "Activate",
        Request::Rollback { .. } => "Rollback",
        Request::Restart { .. } => "Restart",
        Request::Status { .. } => "Status",
        Request::Logs { .. } => "Logs",
        Request::SecretSet { .. } => "SecretSet",
        Request::SecretList => "SecretList",
        Request::SecretRemove { .. } => "SecretRemove",
        Request::TunnelStatus => "TunnelStatus",
        Request::TunnelZones => "TunnelZones",
        Request::TunnelUpgrade => "TunnelUpgrade",
        Request::TunnelBootstrap { .. } => "TunnelBootstrap",
        Request::Shutdown => "Shutdown",
    }
}

async fn write_response(stream: &mut UnixStream, response: &Response) -> Result<()> {
    let frame = framing::encode(response).context("encoding response")?;
    stream.write_all(&frame).await.context("writing response")?;
    stream.shutdown().await.context("shutting down stream")?;
    Ok(())
}

/// Resolve the socket path: `$XDG_RUNTIME_DIR/shikud.sock`, falling back to
/// `/run/user/<uid>/shikud.sock` if the env var is unset (which shouldn't
/// happen under systemd-user but is worth handling gracefully).
fn resolve_socket_path() -> Result<PathBuf> {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        return Ok(PathBuf::from(runtime_dir).join("shikud.sock"));
    }
    let uid = nix::unistd::Uid::current().as_raw();
    Ok(PathBuf::from(format!("/run/user/{uid}/shikud.sock")))
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_env("SHIKUD_LOG").unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}
