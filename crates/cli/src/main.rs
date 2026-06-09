//! `shiku` — deploy CLI for the Shiku service platform.
//!
//! See `docs/deploying.md` for the deploy workflow.

#![forbid(unsafe_code)]

mod bootstrap;
mod config;
mod release;
mod ssh;

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use shiku_types::{Request, Response};

use crate::config::{Config, ResolvedApp};

#[derive(Parser, Debug)]
#[command(
    name = "shiku",
    version,
    about = "Deploy native services to a Shiku-managed box.",
    long_about = None,
)]
struct Cli {
    /// Path to the Shiku config file. Defaults to `shiku.toml` searched
    /// upwards from the current directory.
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Environment selector (e.g. prod, staging). Falls back to
    /// `shiku.default_env` from the config file.
    #[arg(long, global = true)]
    env: Option<String>,

    #[command(subcommand)]
    command: Command_,
}

#[derive(Subcommand, Debug)]
enum Command_ {
    /// Health-check the agent on the box for the given app's environment.
    Ping {
        /// App name. Optional when only one app is defined in shiku.toml.
        app: Option<String>,
    },

    /// Print the resolved configuration for an app/env pair.
    /// Debugging aid — shows what the rest of the CLI sees.
    #[command(name = "config")]
    ConfigCmd {
        #[command(subcommand)]
        sub: ConfigSub,
    },

    /// Manage app registrations on the agent.
    App {
        #[command(subcommand)]
        sub: AppSub,
    },

    /// Manage releases (build, upload, list).
    Release {
        #[command(subcommand)]
        sub: ReleaseSub,
    },

    /// Manage secrets stored on the agent for this service-user.
    Secret {
        #[command(subcommand)]
        sub: SecretSub,
    },

    /// List all registered apps on the agent (alias for `app list`).
    Apps {
        /// App whose env's agent we ask. Optional when only one app exists.
        app: Option<String>,
    },

    /// Build, upload, and atomically activate a service in one shot.
    Deploy { app: Option<String> },

    /// Roll back to the previously activated release.
    Rollback { app: Option<String> },

    /// Show what's running and what version.
    Status {
        /// Restrict to one app. Optional when only one app is defined.
        app: Option<String>,
        /// Show all apps the agent knows about, regardless of which app's
        /// shiku.toml we're running from.
        #[arg(long)]
        all: bool,
    },

    /// Stream service logs from the box.
    Logs {
        app: Option<String>,
        /// Dump existing logs and exit, instead of following.
        #[arg(long)]
        no_follow: bool,
        /// `journalctl --since` value, e.g. `"1 hour ago"`, `"2026-04-30 12:00"`.
        #[arg(long)]
        since: Option<String>,
    },

    /// Restart a service without redeploying.
    Restart { app: Option<String> },

    /// Show the env vars an app receives at activation (with secret values
    /// masked). Useful for debugging deploys — answers "is the agent
    /// actually injecting what I think it is?"
    Env { app: Option<String> },

    /// Provision a new service-user on a box and install the Shiku agent.
    /// One-time operation per (box, service-user) pair.
    ///
    /// Re-running against an already-bootstrapped user is safe (idempotent);
    /// for explicit "upgrade just the agent" semantics use the `upgrade`
    /// subcommand which skips the privileged steps.
    Bootstrap {
        #[command(subcommand)]
        sub: BootstrapSub,
    },

    /// Manage the box's Cloudflare tunnel (for public ingress).
    /// See `docs/ingress.md`.
    Tunnel {
        #[command(subcommand)]
        sub: TunnelSub,
    },
}

#[derive(Subcommand, Debug)]
enum TunnelSub {
    /// Print the box's tunnel state: tunnel UUID, cloudflared status,
    /// authorized zones, and current ingress mapping.
    Status {
        /// SSH host of the box.
        #[arg(long)]
        host: String,
        /// Service user on the box.
        #[arg(long)]
        user: String,
    },
    /// List the zones this box is authorized to publish into.
    Zones {
        #[arg(long)]
        host: String,
        #[arg(long)]
        user: String,
    },
    /// Download Shiku's pinned cloudflared release, SHA-256-verify it,
    /// and swap the binary in. Idempotent — if the installed binary
    /// already matches the pin, it's a no-op.
    Upgrade {
        #[arg(long)]
        host: String,
        #[arg(long)]
        user: String,
    },
    /// Bootstrap the box's cloudflared tunnel. Run once per box.
    ///
    /// Two modes:
    ///
    /// - **Auto-create** (`--create-tunnel <NAME>`): the agent uses the
    ///   API token to create a new tunnel via the CF API. Requires the
    ///   token to have `Account.Cloudflare Tunnel:Edit`. The CLI only
    ///   prompts for the API token.
    ///
    /// - **Manual** (`--tunnel-id <UUID>`): operator already created
    ///   the tunnel in the dashboard. The CLI prompts for the runtime
    ///   token (from the dashboard) plus the API token.
    ///
    /// Idempotent — re-running refreshes tokens and re-installs the
    /// cloudflared unit. Auto-create mode rejects re-bootstrap against
    /// an existing tunnel of the same name (CF doesn't expose tunnel
    /// secrets after creation); switch to `--tunnel-id` if you need to
    /// take over an existing tunnel.
    Bootstrap {
        /// SSH host of the box that owns the tunnel.
        #[arg(long)]
        host: String,
        /// Service user on the box (the agent runs as this user; the
        /// tunnel is registered against its `__shiku__` secrets).
        #[arg(long)]
        user: String,
        /// Auto-create mode: name to give the new tunnel.
        /// Mutually exclusive with `--tunnel-id`.
        #[arg(long, conflicts_with = "tunnel_id", group = "tunnel_source")]
        create_tunnel: Option<String>,
        /// Manual mode: existing tunnel UUID (from CF dashboard).
        /// Mutually exclusive with `--create-tunnel`.
        #[arg(long, conflicts_with = "create_tunnel", group = "tunnel_source")]
        tunnel_id: Option<String>,
        /// Zone apex names to authorize for this box. Repeat for each.
        #[arg(long = "zone", required = true)]
        zones: Vec<String>,
        /// Read tokens from stdin instead of prompting interactively.
        /// Format depends on mode:
        ///   - Auto-create: one line, the API token.
        ///   - Manual:      two lines, tunnel_token then api_token.
        #[arg(long)]
        from_stdin: bool,
    },
}

#[derive(Subcommand, Debug)]
enum BootstrapSub {
    /// Full provisioning: create user, install keys, install agent, start.
    /// Idempotent — safe to re-run.
    Init {
        #[arg(long)]
        host: String,
        /// Admin user on the box (has sudo). Used only for the one-time
        /// privileged setup: useradd, enable-linger, SSH key install.
        #[arg(long)]
        admin: String,
        #[arg(long)]
        user: String,
        #[arg(long)]
        github: String,
        #[arg(long)]
        agent_binary: Option<PathBuf>,
    },
    /// Upgrade just the agent binary on an already-bootstrapped user.
    /// Skips the privileged steps (no sudo needed); just stops the agent,
    /// scp's the new binary, and restarts.
    Upgrade {
        #[arg(long)]
        host: String,
        #[arg(long)]
        user: String,
        #[arg(long)]
        agent_binary: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
enum ConfigSub {
    /// Show the resolved (post-defaults) view of an app's config.
    Show { app: Option<String> },
}

#[derive(Subcommand, Debug)]
enum AppSub {
    /// Push the app's resolved config from shiku.toml to the agent.
    /// Re-running updates the on-box config in place.
    Register { app: Option<String> },
    /// Remove an app and all its releases from the agent.
    Remove { app: Option<String> },
    /// List registered apps on the agent (same as `shiku apps`).
    List { app: Option<String> },
    /// Fetch and print an app's on-box config.
    Inspect { app: Option<String> },
}

#[derive(Subcommand, Debug)]
enum ReleaseSub {
    /// Build, hash, and upload a release. Does not activate it.
    Upload { app: Option<String> },
    /// Activate a previously uploaded release by sha.
    Activate {
        /// App name. Optional when shiku.toml defines exactly one app.
        app: Option<String>,
        /// Release sha (full hex from `release upload`).
        #[arg(long)]
        sha: String,
    },
    /// List releases on the box for an app.
    List { app: Option<String> },
}

#[derive(Subcommand, Debug)]
enum SecretSub {
    /// Set or rotate a secret. Prompts for the value (masked) unless
    /// `--from-stdin` is passed.
    Set {
        /// Secret name (UPPER_SNAKE_CASE recommended; becomes an env var).
        name: String,
        /// Read the value from stdin instead of prompting interactively.
        /// Use this for piping (`pass show foo | shiku secret set FOO --from-stdin`).
        #[arg(long)]
        from_stdin: bool,
        /// Which app's environment to address (selects the agent). Optional
        /// when shiku.toml defines exactly one app.
        #[arg(long)]
        app: Option<String>,
    },
    /// List secret names. Values are never printed.
    List {
        #[arg(long)]
        app: Option<String>,
    },
    /// Remove a secret.
    Remove {
        name: String,
        #[arg(long)]
        app: Option<String>,
    },
    /// Rotate a secret (alias for `set`; clarifies intent in CLI history).
    Rotate {
        name: String,
        #[arg(long)]
        from_stdin: bool,
        #[arg(long)]
        app: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();

    match cli.command {
        Command_::Ping { app } => {
            let resolved =
                load_and_resolve(cli.config.as_deref(), app.as_deref(), cli.env.as_deref())?;
            cmd_ping(&resolved).await?;
        }
        Command_::ConfigCmd { sub } => match sub {
            ConfigSub::Show { app } => {
                let resolved =
                    load_and_resolve(cli.config.as_deref(), app.as_deref(), cli.env.as_deref())?;
                print_resolved(&resolved);
            }
        },
        Command_::App { sub } => match sub {
            AppSub::Register { app } => {
                let resolved =
                    load_and_resolve(cli.config.as_deref(), app.as_deref(), cli.env.as_deref())?;
                cmd_app_register(&resolved).await?;
            }
            AppSub::Remove { app } => {
                let resolved =
                    load_and_resolve(cli.config.as_deref(), app.as_deref(), cli.env.as_deref())?;
                cmd_app_remove(&resolved).await?;
            }
            AppSub::List { app } => {
                let resolved =
                    load_and_resolve(cli.config.as_deref(), app.as_deref(), cli.env.as_deref())?;
                cmd_app_list(&resolved).await?;
            }
            AppSub::Inspect { app } => {
                let resolved =
                    load_and_resolve(cli.config.as_deref(), app.as_deref(), cli.env.as_deref())?;
                cmd_app_inspect(&resolved).await?;
            }
        },
        Command_::Apps { app } => {
            let resolved =
                load_and_resolve(cli.config.as_deref(), app.as_deref(), cli.env.as_deref())?;
            cmd_app_list(&resolved).await?;
        }
        Command_::Secret { sub } => match sub {
            SecretSub::Set {
                name,
                from_stdin,
                app,
            } => {
                let resolved =
                    load_and_resolve(cli.config.as_deref(), app.as_deref(), cli.env.as_deref())?;
                cmd_secret_set(&resolved, &name, from_stdin).await?;
            }
            SecretSub::Rotate {
                name,
                from_stdin,
                app,
            } => {
                let resolved =
                    load_and_resolve(cli.config.as_deref(), app.as_deref(), cli.env.as_deref())?;
                cmd_secret_set(&resolved, &name, from_stdin).await?;
            }
            SecretSub::List { app } => {
                let resolved =
                    load_and_resolve(cli.config.as_deref(), app.as_deref(), cli.env.as_deref())?;
                cmd_secret_list(&resolved).await?;
            }
            SecretSub::Remove { name, app } => {
                let resolved =
                    load_and_resolve(cli.config.as_deref(), app.as_deref(), cli.env.as_deref())?;
                cmd_secret_remove(&resolved, &name).await?;
            }
        },
        Command_::Release { sub } => match sub {
            ReleaseSub::Upload { app } => {
                let resolved =
                    load_and_resolve(cli.config.as_deref(), app.as_deref(), cli.env.as_deref())?;
                cmd_release_upload(&resolved).await?;
            }
            ReleaseSub::Activate { app, sha } => {
                let resolved =
                    load_and_resolve(cli.config.as_deref(), app.as_deref(), cli.env.as_deref())?;
                cmd_activate(&resolved, &sha).await?;
            }
            ReleaseSub::List { app } => {
                let resolved =
                    load_and_resolve(cli.config.as_deref(), app.as_deref(), cli.env.as_deref())?;
                cmd_release_list(&resolved).await?;
            }
        },
        Command_::Deploy { app } => {
            let resolved =
                load_and_resolve(cli.config.as_deref(), app.as_deref(), cli.env.as_deref())?;
            cmd_deploy(&resolved).await?;
        }
        Command_::Rollback { app } => {
            let resolved =
                load_and_resolve(cli.config.as_deref(), app.as_deref(), cli.env.as_deref())?;
            cmd_rollback(&resolved).await?;
        }
        Command_::Status { app, all } => {
            let resolved =
                load_and_resolve(cli.config.as_deref(), app.as_deref(), cli.env.as_deref())?;
            cmd_status(&resolved, all).await?;
        }
        Command_::Logs {
            app,
            no_follow,
            since,
        } => {
            let resolved =
                load_and_resolve(cli.config.as_deref(), app.as_deref(), cli.env.as_deref())?;
            cmd_logs(&resolved, !no_follow, since.as_deref()).await?;
        }
        Command_::Restart { app } => {
            let resolved =
                load_and_resolve(cli.config.as_deref(), app.as_deref(), cli.env.as_deref())?;
            cmd_restart(&resolved).await?;
        }
        Command_::Env { app } => {
            let resolved =
                load_and_resolve(cli.config.as_deref(), app.as_deref(), cli.env.as_deref())?;
            cmd_env(&resolved).await?;
        }
        Command_::Bootstrap { sub } => match sub {
            BootstrapSub::Init {
                host,
                admin,
                user,
                github,
                agent_binary,
            } => {
                let binary = agent_binary.unwrap_or_else(default_agent_binary);
                bootstrap::run(&host, &admin, &user, &github, &binary).await?;
            }
            BootstrapSub::Upgrade {
                host,
                user,
                agent_binary,
            } => {
                let binary = agent_binary.unwrap_or_else(default_agent_binary);
                bootstrap::upgrade(&host, &user, &binary).await?;
            }
        },
        Command_::Tunnel { sub } => match sub {
            TunnelSub::Status { host, user } => {
                let resolved = ResolvedApp::for_ssh(host, user);
                cmd_tunnel_status(&resolved).await?;
            }
            TunnelSub::Zones { host, user } => {
                let resolved = ResolvedApp::for_ssh(host, user);
                cmd_tunnel_zones(&resolved).await?;
            }
            TunnelSub::Upgrade { host, user } => {
                let resolved = ResolvedApp::for_ssh(host, user);
                cmd_tunnel_upgrade(&resolved).await?;
            }
            TunnelSub::Bootstrap {
                host,
                user,
                create_tunnel,
                tunnel_id,
                zones,
                from_stdin,
            } => {
                // Tunnel bootstrap is a per-box operation, so we don't load
                // the project's shiku.toml — operator passes (host, user)
                // directly, just like `shiku bootstrap init/upgrade`.
                let resolved = ResolvedApp::for_ssh(host, user);
                let mode = match (create_tunnel, tunnel_id) {
                    (Some(name), None) => CliBootstrapMode::AutoCreate { tunnel_name: name },
                    (None, Some(id)) => CliBootstrapMode::Manual { tunnel_id: id },
                    (None, None) => bail!(
                        "tunnel bootstrap: pass either `--create-tunnel <NAME>` (auto-create) \
                         or `--tunnel-id <UUID>` (manual)"
                    ),
                    (Some(_), Some(_)) => unreachable!("clap conflicts_with prevents this"),
                };
                cmd_tunnel_bootstrap(&resolved, &mode, &zones, from_stdin).await?;
            }
        },
    }

    Ok(())
}

/// Load shiku.toml and resolve to a single app/env. Most subcommands start
/// here.
fn load_and_resolve(
    config_path: Option<&std::path::Path>,
    app: Option<&str>,
    env: Option<&str>,
) -> Result<ResolvedApp> {
    let (path, cfg) = Config::load(config_path).context("loading shiku.toml")?;
    tracing::debug!(config = %path.display(), "loaded config");
    cfg.resolve(app, env).context("resolving app/env")
}

/// Send a `Ping` to the agent over SSH and print the response.
async fn cmd_ping(resolved: &ResolvedApp) -> Result<()> {
    tracing::info!(
        app = %resolved.app_name,
        env = %resolved.env_name,
        host = %resolved.deploy_host,
        user = %resolved.ssh_user,
        "sending Ping"
    );

    let response = ssh::request_one(resolved, &Request::Ping)
        .await
        .context("ping request failed")?;

    match response {
        Response::Pong => {
            println!("pong");
            Ok(())
        }
        Response::Error { kind, detail } => bail!("agent error ({kind:?}): {detail}"),
        other => bail!("unexpected response: {other:?}"),
    }
}

async fn cmd_app_register(resolved: &ResolvedApp) -> Result<()> {
    tracing::info!(
        app = %resolved.app_name,
        env = %resolved.env_name,
        "registering app with agent"
    );
    let config = resolved.to_app_config();
    let response = ssh::request_one(
        resolved,
        &Request::AppRegister {
            name: resolved.app_name.clone(),
            config,
        },
    )
    .await
    .context("AppRegister request failed")?;
    expect_ok(&response, "register")?;
    println!(
        "registered: {} (env: {})",
        resolved.app_name, resolved.env_name
    );
    Ok(())
}

async fn cmd_app_remove(resolved: &ResolvedApp) -> Result<()> {
    tracing::info!(app = %resolved.app_name, "removing app from agent");
    let response = ssh::request_one(
        resolved,
        &Request::AppRemove {
            name: resolved.app_name.clone(),
        },
    )
    .await
    .context("AppRemove request failed")?;
    expect_ok(&response, "remove")?;
    println!("removed: {}", resolved.app_name);
    Ok(())
}

async fn cmd_app_list(resolved: &ResolvedApp) -> Result<()> {
    let response = ssh::request_one(resolved, &Request::AppList)
        .await
        .context("AppList request failed")?;
    match response {
        Response::Apps(names) => {
            if names.is_empty() {
                println!("(no apps registered)");
            } else {
                for name in names {
                    println!("{name}");
                }
            }
            Ok(())
        }
        Response::Error { kind, detail } => bail!("agent error ({kind:?}): {detail}"),
        other => bail!("unexpected response: {other:?}"),
    }
}

async fn cmd_app_inspect(resolved: &ResolvedApp) -> Result<()> {
    let response = ssh::request_one(
        resolved,
        &Request::AppInspect {
            name: resolved.app_name.clone(),
        },
    )
    .await
    .context("AppInspect request failed")?;
    match response {
        Response::AppDetail(config) => {
            // Print as TOML — matches what's persisted on-box and is the most
            // honest representation of what the agent has.
            let body = toml::to_string_pretty(&*config).context("serializing AppConfig as TOML")?;
            println!("{body}");
            Ok(())
        }
        Response::Error { kind, detail } => bail!("agent error ({kind:?}): {detail}"),
        other => bail!("unexpected response: {other:?}"),
    }
}

async fn cmd_release_upload(resolved: &ResolvedApp) -> Result<()> {
    let sha = build_and_upload(resolved).await?;
    println!("uploaded: {} ({})", resolved.app_name, sha);
    Ok(())
}

/// Shared build → upload pipeline. Returns the release sha. Used by both
/// `release upload` (one-shot) and `deploy` (build → upload → activate).
///
/// Skips the rsync + verify steps when the agent already has a release with
/// the computed sha — saves a few seconds on no-op redeploys. The build
/// itself is still cheap (incremental cargo) so we don't try to skip that.
async fn build_and_upload(resolved: &ResolvedApp) -> Result<String> {
    let binary = release::build(resolved).await.context("build failed")?;

    let sha = release::hash_file(&binary.path)
        .await
        .context("hashing built binary")?;
    tracing::info!(sha = %sha, "computed release sha");

    if release_already_present(resolved, &sha).await? {
        tracing::info!(sha = %sha, "release already on box; skipping rsync + verify");
        return Ok(sha);
    }

    let workspace_root = release::workspace_root().context("locating workspace root")?;
    let (git_sha, git_dirty) = release::git_state(&workspace_root).await;
    let manifest = release::build_manifest(&sha, &binary, git_sha, git_dirty);

    release::rsync_to_box(&binary, resolved, &sha)
        .await
        .context("rsync upload failed")?;

    let response = ssh::request_one(
        resolved,
        &Request::ReleaseUpload {
            app: resolved.app_name.clone(),
            sha: sha.clone(),
            manifest,
        },
    )
    .await
    .context("ReleaseUpload request failed")?;
    expect_ok(&response, "release upload")?;

    Ok(sha)
}

/// Check whether the agent already has a release with this sha. Returns
/// false on any error so we fail toward "do the upload" rather than skip it.
async fn release_already_present(resolved: &ResolvedApp, sha: &str) -> Result<bool> {
    let response = ssh::request_one(
        resolved,
        &Request::ReleaseList {
            app: resolved.app_name.clone(),
        },
    )
    .await
    .context("checking existing releases")?;
    match response {
        Response::Releases(infos) => Ok(infos.iter().any(|r| r.sha == sha)),
        // If the agent doesn't know the app yet (e.g. first deploy after
        // bootstrap), there can't be an existing release with this sha.
        Response::Error { .. } => Ok(false),
        _ => Ok(false),
    }
}

async fn cmd_activate(resolved: &ResolvedApp, sha: &str) -> Result<()> {
    tracing::info!(
        app = %resolved.app_name,
        env = %resolved.env_name,
        %sha,
        "activating release"
    );
    activate_streaming(resolved, sha).await
}

/// Open a streaming `Activate` request, render `ActivateProgress`
/// events live, then return based on the terminal `Response::Ok` /
/// `Response::Error`. Used by both `cmd_activate` and `cmd_deploy`.
async fn activate_streaming(resolved: &ResolvedApp, sha: &str) -> Result<()> {
    use shiku_types::Response;
    let mut conn = ssh::Connection::open(resolved)
        .await
        .context("opening SSH connection for activate")?;
    let mut terminal: Option<Response> = None;
    conn.request_streaming(
        &Request::Activate {
            app: resolved.app_name.clone(),
            sha: sha.to_string(),
        },
        |frame| {
            match frame {
                Response::ActivateProgress(event) => render_activate_event(&event),
                terminal_frame @ (Response::Ok | Response::Error { .. }) => {
                    terminal = Some(terminal_frame);
                }
                other => {
                    eprintln!("(unexpected frame during activate: {other:?})");
                }
            }
            Ok(())
        },
    )
    .await
    .context("streaming Activate")?;
    conn.close().await.ok();

    match terminal {
        Some(Response::Ok) => {
            println!("activated: {} -> {}", resolved.app_name, sha);
            Ok(())
        }
        Some(Response::Error { kind, detail }) => {
            bail!("activate failed ({kind:?}): {detail}")
        }
        Some(other) => bail!("activate: unexpected terminal frame: {other:?}"),
        None => bail!("activate: stream closed without terminal frame"),
    }
}

fn render_activate_event(event: &shiku_types::ActivateEvent) {
    use shiku_types::ActivateEvent::*;
    match event {
        UploadComplete => println!("  ✓ release verified"),
        UnitWritten => println!("  ✓ systemd unit written"),
        SecretsResolved => println!("  ✓ secrets + bindings resolved"),
        ProcessStarting => println!("  ✓ process starting"),
        HealthCheckAttempt { detail, .. } => {
            let suffix = detail.as_deref().unwrap_or("");
            println!("  · health check: {suffix}");
        }
        HealthCheckPassed => println!("  ✓ health check passed"),
        HealthCheckFailed => println!("  ✗ health check failed"),
        Activated { sha } => println!("  ✓ activated {sha}"),
        RolledBack { to_sha, reason } => {
            println!("  ↩ rolled back to {to_sha} ({reason})");
        }
    }
}

async fn cmd_deploy(resolved: &ResolvedApp) -> Result<()> {
    tracing::info!(
        app = %resolved.app_name,
        env = %resolved.env_name,
        "deploy: register + build + upload + activate"
    );

    // Register first so TOML changes (env vars, public hostnames, health,
    // systemd overrides) are reflected on the box before activation. This
    // also ensures `app register` is implicit on first deploy — operators
    // shouldn't need to run two commands to get a fresh app live.
    cmd_app_register(resolved).await?;

    let sha = build_and_upload(resolved).await?;
    println!("uploaded: {} ({})", resolved.app_name, sha);

    activate_streaming(resolved, &sha).await
}

async fn cmd_secret_set(resolved: &ResolvedApp, name: &str, from_stdin: bool) -> Result<()> {
    let value = if from_stdin {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
            .context("reading from stdin")?;
        // Strip exactly one trailing newline if present (common from `echo` /
        // `pass show`); preserve any deliberate internal whitespace.
        if buf.ends_with('\n') {
            buf.pop();
            if buf.ends_with('\r') {
                buf.pop();
            }
        }
        buf
    } else {
        rpassword::prompt_password(format!("Value for '{name}': "))
            .context("prompting for value")?
    };

    if value.is_empty() {
        bail!("refusing to set empty secret");
    }

    tracing::info!(secret = %name, "sending secret to agent");
    let response = ssh::request_one(
        resolved,
        &Request::SecretSet {
            name: name.to_string(),
            value,
        },
    )
    .await
    .context("SecretSet request failed")?;
    expect_ok(&response, "secret set")?;
    println!("set: {name}");
    Ok(())
}

/// CLI-side mirror of `shiku_types::TunnelBootstrapMode`. Wire mapping
/// happens inside `cmd_tunnel_bootstrap` once we have all the prompts.
#[derive(Debug)]
enum CliBootstrapMode {
    AutoCreate { tunnel_name: String },
    Manual { tunnel_id: String },
}

async fn cmd_tunnel_status(resolved: &ResolvedApp) -> Result<()> {
    let response = ssh::request_one(resolved, &Request::TunnelStatus)
        .await
        .context("TunnelStatus request failed")?;
    let report = match response {
        Response::TunnelStatus(r) => r,
        Response::Error { kind, detail } => bail!("status failed ({kind:?}): {detail}"),
        other => bail!("unexpected response: {other:?}"),
    };

    match report.tunnel_id {
        Some(id) => println!("tunnel:        {id}"),
        None => {
            println!("tunnel:        (not configured — run `shiku tunnel bootstrap`)");
            return Ok(());
        }
    }
    println!("cloudflared:   {}", report.cloudflared_state);
    if report.zones.is_empty() {
        println!("zones:         (none)");
    } else {
        println!("zones:         {}", report.zones.join(", "));
    }
    if report.ingress.is_empty() {
        println!("ingress:       (no public hostnames declared)");
    } else {
        println!("ingress:");
        let max_host = report
            .ingress
            .iter()
            .map(|r| r.hostname.len())
            .max()
            .unwrap_or(0);
        for rule in &report.ingress {
            let port = if rule.port == 0 {
                "(no port)".to_string()
            } else {
                format!("localhost:{}", rule.port)
            };
            println!(
                "  {:<width$}  →  {}  ({})",
                rule.hostname,
                port,
                rule.app,
                width = max_host
            );
        }
    }
    Ok(())
}

async fn cmd_tunnel_upgrade(resolved: &ResolvedApp) -> Result<()> {
    let response = ssh::request_one(resolved, &Request::TunnelUpgrade)
        .await
        .context("TunnelUpgrade request failed")?;
    match response {
        Response::TunnelUpgrade { version, changed } => {
            if changed {
                println!("upgraded cloudflared to {version}");
            } else {
                println!("cloudflared already at {version}; no change");
            }
            Ok(())
        }
        Response::Error { kind, detail } => bail!("upgrade failed ({kind:?}): {detail}"),
        other => bail!("unexpected response: {other:?}"),
    }
}

async fn cmd_tunnel_zones(resolved: &ResolvedApp) -> Result<()> {
    let response = ssh::request_one(resolved, &Request::TunnelZones)
        .await
        .context("TunnelZones request failed")?;
    let zones = match response {
        Response::TunnelZones(z) => z,
        Response::Error { kind, detail } => bail!("zones failed ({kind:?}): {detail}"),
        other => bail!("unexpected response: {other:?}"),
    };
    if zones.is_empty() {
        println!("(no zones — run `shiku tunnel bootstrap` to authorize one)");
    } else {
        for z in zones {
            println!("{}  {}", z.name, z.zone_id);
        }
    }
    Ok(())
}

async fn cmd_tunnel_bootstrap(
    resolved: &ResolvedApp,
    mode: &CliBootstrapMode,
    zones: &[String],
    from_stdin: bool,
) -> Result<()> {
    // Manual needs (tunnel_token, api_token); auto-create needs (api_token).
    let (tunnel_token_opt, api_token) = match mode {
        CliBootstrapMode::Manual { .. } => {
            let (t, a) = if from_stdin {
                let mut buf = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
                    .context("reading tokens from stdin")?;
                let mut lines = buf.lines();
                let tunnel = lines.next().ok_or_else(|| {
                    anyhow::anyhow!("stdin empty; expected two lines (tunnel token, api token)")
                })?;
                let api = lines.next().ok_or_else(|| {
                    anyhow::anyhow!("stdin had only one line; expected tunnel token then api token")
                })?;
                (tunnel.to_string(), api.to_string())
            } else {
                let tunnel = rpassword::prompt_password("Tunnel runtime token: ")
                    .context("prompting for tunnel token")?;
                let api = rpassword::prompt_password("CF API token: ")
                    .context("prompting for API token")?;
                (tunnel, api)
            };
            (Some(t), a)
        }
        CliBootstrapMode::AutoCreate { .. } => {
            let a = if from_stdin {
                let mut buf = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
                    .context("reading API token from stdin")?;
                buf.lines()
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("stdin empty; expected API token"))?
                    .to_string()
            } else {
                rpassword::prompt_password("CF API token: ").context("prompting for API token")?
            };
            (None, a)
        }
    };

    if api_token.is_empty() {
        bail!("refusing to bootstrap with empty API token");
    }
    if let Some(t) = &tunnel_token_opt {
        if t.is_empty() {
            bail!("refusing to bootstrap with empty tunnel runtime token");
        }
    }

    let wire_mode = match mode {
        CliBootstrapMode::Manual { tunnel_id } => shiku_types::TunnelBootstrapMode::Manual {
            tunnel_id: tunnel_id.clone(),
            tunnel_token: tunnel_token_opt.expect("manual mode resolved tunnel token above"),
        },
        CliBootstrapMode::AutoCreate { tunnel_name } => {
            shiku_types::TunnelBootstrapMode::AutoCreate {
                tunnel_name: tunnel_name.clone(),
            }
        }
    };

    tracing::info!(
        ?mode,
        zones = ?zones,
        host = %resolved.deploy_host,
        "sending TunnelBootstrap"
    );
    let response = ssh::request_one(
        resolved,
        &Request::TunnelBootstrap {
            mode: wire_mode,
            api_token,
            zones: zones.to_vec(),
        },
    )
    .await
    .context("TunnelBootstrap request failed")?;
    expect_ok(&response, "tunnel bootstrap")?;
    let mode_label = match mode {
        CliBootstrapMode::Manual { tunnel_id } => format!("manual (tunnel {tunnel_id})"),
        CliBootstrapMode::AutoCreate { tunnel_name } => {
            format!("auto-create (tunnel name `{tunnel_name}`)")
        }
    };
    println!(
        "bootstrapped tunnel on {} via {} (zones: {})",
        resolved.deploy_host,
        mode_label,
        zones.join(", ")
    );
    Ok(())
}

async fn cmd_secret_list(resolved: &ResolvedApp) -> Result<()> {
    let response = ssh::request_one(resolved, &Request::SecretList)
        .await
        .context("SecretList request failed")?;
    match response {
        Response::Secrets(names) => {
            if names.is_empty() {
                println!("(no secrets stored)");
            } else {
                for name in names {
                    println!("{name}");
                }
            }
            Ok(())
        }
        Response::Error { kind, detail } => bail!("secret list failed ({kind:?}): {detail}"),
        other => bail!("unexpected response: {other:?}"),
    }
}

async fn cmd_secret_remove(resolved: &ResolvedApp, name: &str) -> Result<()> {
    let response = ssh::request_one(
        resolved,
        &Request::SecretRemove {
            name: name.to_string(),
        },
    )
    .await
    .context("SecretRemove request failed")?;
    expect_ok(&response, "secret remove")?;
    println!("removed: {name}");
    Ok(())
}

async fn cmd_status(resolved: &ResolvedApp, all: bool) -> Result<()> {
    let app_filter = if all {
        None
    } else {
        Some(resolved.app_name.clone())
    };
    let response = ssh::request_one(resolved, &Request::Status { app: app_filter })
        .await
        .context("Status request failed")?;

    match response {
        Response::Status(snapshots) => {
            if snapshots.is_empty() {
                println!("(no apps)");
                return Ok(());
            }
            for s in snapshots {
                println!("{}", s.name);
                println!("  registered:    {}", s.registered);
                println!("  systemd:       {}", s.systemd_state);
                if let Some(secs) = s.uptime_secs {
                    println!("  uptime:        {}", format_duration(secs));
                }
                if let Some(sha) = &s.current_sha {
                    println!("  current:       {}", short_sha(sha));
                }
                if let Some(sha) = &s.previous_sha {
                    println!("  previous:      {}", short_sha(sha));
                }
                if let Some(t) = &s.last_activated_at {
                    println!("  activated_at:  {t}");
                }
            }
            Ok(())
        }
        Response::Error { kind, detail } => bail!("status failed ({kind:?}): {detail}"),
        other => bail!("unexpected response: {other:?}"),
    }
}

async fn cmd_logs(resolved: &ResolvedApp, follow: bool, since: Option<&str>) -> Result<()> {
    tracing::info!(
        app = %resolved.app_name,
        follow,
        since = ?since,
        "streaming logs"
    );
    let mut conn = ssh::Connection::open(resolved)
        .await
        .context("opening ssh connection")?;
    conn.request_streaming(
        &Request::Logs {
            app: resolved.app_name.clone(),
            since: since.map(|s| s.to_string()),
            follow,
        },
        |response| match response {
            Response::LogLine(line) => {
                println!("{line}");
                Ok(())
            }
            Response::Error { kind, detail } => {
                bail!("agent error ({kind:?}): {detail}")
            }
            other => bail!("unexpected streaming response: {other:?}"),
        },
    )
    .await
    .context("logs stream failed")?;
    conn.close().await?;
    Ok(())
}

async fn cmd_release_list(resolved: &ResolvedApp) -> Result<()> {
    let response = ssh::request_one(
        resolved,
        &Request::ReleaseList {
            app: resolved.app_name.clone(),
        },
    )
    .await
    .context("ReleaseList request failed")?;

    match response {
        Response::Releases(infos) => {
            if infos.is_empty() {
                println!("(no releases)");
                return Ok(());
            }
            for r in infos {
                let marker = if r.is_current {
                    "*"
                } else if r.is_previous {
                    "←"
                } else {
                    " "
                };
                println!(
                    "{} {}  {}  {:>9}",
                    marker,
                    short_sha(&r.sha),
                    r.built_at,
                    format_bytes(r.size_bytes),
                );
            }
            Ok(())
        }
        Response::Error { kind, detail } => bail!("release list failed ({kind:?}): {detail}"),
        other => bail!("unexpected response: {other:?}"),
    }
}

fn short_sha(sha: &str) -> String {
    if sha.len() <= 12 {
        sha.to_string()
    } else {
        format!("{}…", &sha[..12])
    }
}

fn format_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else if secs < 86400 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d{}h", secs / 86400, (secs % 86400) / 3600)
    }
}

fn format_bytes(b: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if b >= GIB {
        format!("{:.1} GiB", b as f64 / GIB as f64)
    } else if b >= MIB {
        format!("{:.1} MiB", b as f64 / MIB as f64)
    } else if b >= KIB {
        format!("{:.1} KiB", b as f64 / KIB as f64)
    } else {
        format!("{b} B")
    }
}

async fn cmd_rollback(resolved: &ResolvedApp) -> Result<()> {
    tracing::info!(app = %resolved.app_name, "rolling back to previous release");
    let response = ssh::request_one(
        resolved,
        &Request::Rollback {
            app: resolved.app_name.clone(),
        },
    )
    .await
    .context("Rollback request failed")?;
    expect_ok(&response, "rollback")?;
    println!("rolled back: {}", resolved.app_name);
    println!("(run `shiku status` to confirm the now-active release sha)");
    Ok(())
}

async fn cmd_env(resolved: &ResolvedApp) -> Result<()> {
    // Pull the on-box AppConfig (which has the registered allowlist + vars)
    // so we show what the agent will *actually* inject, not just what's in
    // shiku.toml today (which may differ if `app register` is stale).
    let inspect = ssh::request_one(
        resolved,
        &Request::AppInspect {
            name: resolved.app_name.clone(),
        },
    )
    .await
    .context("AppInspect request failed")?;
    let config = match inspect {
        Response::AppDetail(c) => *c,
        Response::Error { kind, detail } => bail!("inspect failed ({kind:?}): {detail}"),
        other => bail!("unexpected response: {other:?}"),
    };

    // Pull the secrets list from the agent so we can show which secrets
    // *would* be injected without ever revealing the values.
    let secrets_resp = ssh::request_one(resolved, &Request::SecretList)
        .await
        .context("SecretList request failed")?;
    let stored_secrets: std::collections::HashSet<String> = match secrets_resp {
        Response::Secrets(names) => names.into_iter().collect(),
        Response::Error { kind, detail } => {
            tracing::warn!(?kind, %detail, "could not list secrets; continuing");
            Default::default()
        }
        _ => Default::default(),
    };

    println!("# vars (plain config from shiku.toml):");
    for (k, v) in &config.vars {
        println!("{k}={v}");
    }
    if !config.bindings.is_empty() {
        println!("\n# bindings (resolved from registered apps' listen ports):");
        for binding in &config.bindings {
            println!(
                "SHIKU_BIND_{}=<resolved at activation>",
                binding.replace('-', "_").to_uppercase()
            );
        }
    }
    if !config.secrets.is_empty() {
        println!("\n# secrets (decrypted at activation, shown masked here):");
        for name in &config.secrets {
            let status = if stored_secrets.contains(name) {
                "********"
            } else {
                "(NOT SET — activation will fail)"
            };
            println!("{name}={status}");
        }
    }
    if let Some(listen) = &config.listen {
        if listen.http {
            println!("\n# listen address (allocated by the agent):");
            match listen.http_port {
                Some(p) => println!("SHIKU_LISTEN_HTTP={p}"),
                None => println!("SHIKU_LISTEN_HTTP=<not yet allocated; run `shiku app register`>"),
            }
        }
    }
    Ok(())
}

async fn cmd_restart(resolved: &ResolvedApp) -> Result<()> {
    tracing::info!(app = %resolved.app_name, "restarting service");
    let response = ssh::request_one(
        resolved,
        &Request::Restart {
            app: resolved.app_name.clone(),
        },
    )
    .await
    .context("Restart request failed")?;
    expect_ok(&response, "restart")?;
    println!("restarted: {}", resolved.app_name);
    Ok(())
}

fn expect_ok(response: &Response, op: &str) -> Result<()> {
    match response {
        Response::Ok => Ok(()),
        Response::Error { kind, detail } => bail!("{op} failed ({kind:?}): {detail}"),
        other => bail!("{op}: unexpected response: {other:?}"),
    }
}

fn print_resolved(r: &ResolvedApp) {
    println!("app:           {}", r.app_name);
    println!("env:           {}", r.env_name);
    println!("package:       {}", r.package);
    println!("target:        {}", r.target);
    println!("build:         {}", r.build);
    println!("deploy_host:   {}", r.deploy_host);
    println!("ssh_user:      {}", r.ssh_user);
    println!("service:       {}", r.service);
    if !r.public.is_empty() {
        println!("public:");
        for url in &r.public {
            println!("  - {url}");
        }
    }
    if let Some(listen) = &r.listen {
        println!("listen.http:   {}", listen.http);
    }
    if !r.secrets.is_empty() {
        println!("secrets:       {}", r.secrets.join(", "));
    }
    if !r.bindings.is_empty() {
        println!("bindings:      {}", r.bindings.join(", "));
    }
    if !r.vars.is_empty() {
        println!("vars:");
        for (k, v) in &r.vars {
            println!("  {k} = {v}");
        }
    }
    println!("health:        {:?}", r.health);
    if r.systemd.memory_max.is_some()
        || r.systemd.cpu_quota_percent.is_some()
        || r.systemd.restart.is_some()
        || r.systemd.restart_sec.is_some()
    {
        println!("systemd overrides:");
        if let Some(m) = &r.systemd.memory_max {
            println!("  memory_max = {m}");
        }
        if let Some(c) = r.systemd.cpu_quota_percent {
            println!("  cpu_quota_percent = {c}");
        }
        if let Some(r2) = &r.systemd.restart {
            println!("  restart = {r2}");
        }
        if let Some(s) = r.systemd.restart_sec {
            println!("  restart_sec = {s}");
        }
    }
}

fn default_agent_binary() -> PathBuf {
    PathBuf::from("target/aarch64-unknown-linux-musl/release/shikud")
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_env("SHIKU_LOG").unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}
