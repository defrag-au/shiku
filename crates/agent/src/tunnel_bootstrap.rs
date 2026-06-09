//! Bootstrap the box's cloudflared tunnel.
//!
//! Run once per box from `shiku tunnel bootstrap`. Writes the cloudflared
//! systemd-user unit, stores the tunnel + API tokens as `__shiku__`-namespaced
//! secrets, looks up zone IDs via the CF API, persists `server.toml`, and
//! starts the tunnel. Idempotent — re-running refreshes the tokens and unit.
//!
//! Two modes (selected by [`TunnelBootstrapMode`]):
//!   - Manual: operator created the tunnel in the dashboard and supplies the
//!     UUID + runtime token directly.
//!   - Auto-create: agent uses the API token to create a new tunnel (or
//!     reuse one of the same name) and reconstructs the runtime token from
//!     the response.
//!
//! See `docs/design/shiku/tunnel.md` §5.

use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};
use secrecy::SecretString;
use shiku_types::{AppConfig, HealthSpec, SystemdSpec, TunnelBootstrapMode};

use crate::apps;
use crate::cf_api::{CfApi, Zone};
use crate::cloudflared;
use crate::secrets;
use crate::server_config::{ServerConfig, TunnelInfo, ZoneInfo};

const TUNNEL_TOKEN_SECRET: &str = "CLOUDFLARED_TUNNEL_TOKEN";
const API_TOKEN_SECRET: &str = "CF_API_TOKEN";

/// Run the bootstrap.
pub async fn bootstrap(
    mode: &TunnelBootstrapMode,
    api_token: &str,
    zones: &[String],
) -> Result<()> {
    if zones.is_empty() {
        bail!("at least one --zone must be specified");
    }

    let cf = CfApi::new(api_token.to_string());

    // 1. Look up zone IDs via the CF API. We trust this for both modes —
    //    even Manual mode needs them so the agent can do DNS upserts at
    //    publish time. This also surfaces obvious auth errors before any
    //    state changes hit disk.
    let zone_list = cf
        .list_zones()
        .await
        .context("listing zones (does the API token have Zone.DNS:Edit?)")?;
    let resolved_zones = resolve_zones(zones, &zone_list)?;

    // 2. Resolve mode-specific tunnel id + runtime token.
    let (tunnel_id, tunnel_token) = match mode {
        TunnelBootstrapMode::Manual {
            tunnel_id,
            tunnel_token,
        } => {
            tracing::info!(tunnel_id, "bootstrapping in manual mode");
            (tunnel_id.clone(), tunnel_token.clone())
        }
        TunnelBootstrapMode::AutoCreate { tunnel_name } => {
            tracing::info!(tunnel_name, "bootstrapping in auto-create mode");
            auto_create_tunnel(&cf, &resolved_zones, tunnel_name).await?
        }
    };

    // 3. Store the tokens. Same shape in both modes — once we have the
    //    runtime token, the secrets store doesn't care where it came from.
    secrets::set(
        &TUNNEL_TOKEN_SECRET.to_string(),
        SecretString::new(tunnel_token.clone().into()),
    )
    .context("storing CLOUDFLARED_TUNNEL_TOKEN")?;
    secrets::set(
        &API_TOKEN_SECRET.to_string(),
        SecretString::new(api_token.to_string().into()),
    )
    .context("storing CF_API_TOKEN")?;
    tracing::info!("stored tunnel + API tokens as __shiku__ secrets");

    // 4. Write server.toml.
    let server_cfg = ServerConfig {
        tunnel: TunnelInfo {
            id: tunnel_id.clone(),
        },
        zones: resolved_zones.clone(),
    };
    write_server_config(&server_cfg)?;

    // 5. Register the __shiku__ pseudo-app.
    apps::register(&apps::PSEUDO_APP_NAME.to_string(), &pseudo_app_config())
        .context("registering __shiku__ pseudo-app")?;

    // 6. Install and start the cloudflared unit.
    cloudflared::install(&tunnel_token, &tunnel_id)
        .await
        .context("installing cloudflared systemd unit")?;

    tracing::info!(
        tunnel_id = %tunnel_id,
        zones = ?resolved_zones.iter().map(|z| &z.name).collect::<Vec<_>>(),
        "tunnel bootstrap complete"
    );
    Ok(())
}

/// Match the operator-supplied `--zone` names against the API's
/// authoritative zone list, returning `(name, zone_id)` for each.
fn resolve_zones(zones: &[String], zone_list: &[Zone]) -> Result<Vec<ZoneInfo>> {
    let mut resolved = Vec::with_capacity(zones.len());
    for zone_name in zones {
        let zone = zone_list
            .iter()
            .find(|z| z.name.eq_ignore_ascii_case(zone_name))
            .ok_or_else(|| {
                anyhow!(
                    "zone `{zone_name}` not in the API token's accessible \
                     zones; available: {}",
                    zone_list
                        .iter()
                        .map(|z| z.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
        resolved.push(ZoneInfo {
            name: zone.name.clone(),
            zone_id: zone.id.clone(),
        });
    }
    Ok(resolved)
}

/// Auto-create mode: find or create a tunnel of the given name on the
/// account that owns the resolved zones, and return `(id, runtime_token)`.
///
/// CF exposes the runtime token via a dedicated `/token` endpoint that
/// works for both newly-created and existing tunnels — so re-bootstrap
/// against an existing tunnel of the same name is supported (idempotent).
async fn auto_create_tunnel(
    cf: &CfApi,
    resolved_zones: &[ZoneInfo],
    tunnel_name: &str,
) -> Result<(String, String)> {
    let account_id = derive_account_id(cf, resolved_zones).await?;

    // If a tunnel with this name already exists on the account, reuse
    // it. The runtime token comes from a dedicated endpoint so we don't
    // need create-time secrets to recover it.
    let existing = cf
        .list_tunnels(&account_id, Some(tunnel_name))
        .await
        .context("listing existing tunnels")?;
    let tunnel_id = if let Some(tunnel) = existing.into_iter().next() {
        tracing::info!(
            tunnel_id = %tunnel.id,
            tunnel_name = %tunnel.name,
            "reusing existing tunnel"
        );
        tunnel.id
    } else {
        let tunnel = cf
            .create_tunnel(&account_id, tunnel_name)
            .await
            .context("creating tunnel via CF API")?;
        tracing::info!(
            tunnel_id = %tunnel.id,
            tunnel_name = %tunnel.name,
            "created tunnel via CF API"
        );
        tunnel.id
    };

    let token = cf
        .get_tunnel_token(&account_id, &tunnel_id)
        .await
        .context("fetching tunnel runtime token")?;

    Ok((tunnel_id, token))
}

/// Find the CF account ID that owns the resolved zones. We use the
/// nested `account.id` on each zone's record. All resolved zones should
/// belong to the same account; we error out if they don't (a token with
/// cross-account zone access is unusual and the bootstrap flow doesn't
/// support it).
async fn derive_account_id(cf: &CfApi, resolved_zones: &[ZoneInfo]) -> Result<String> {
    // We need the *full* zone records (which include `account.id`); the
    // `ZoneInfo` we hold only carries name + zone_id. Re-list zones (the
    // call is cached at the http-client level via reqwest's pool, and
    // even uncached it's a single API hit) and pull the account from
    // there.
    let all = cf
        .list_zones()
        .await
        .context("re-listing zones for account_id")?;
    let mut accounts = std::collections::BTreeSet::<String>::new();
    for zone in resolved_zones {
        let full = all
            .iter()
            .find(|z| z.id == zone.zone_id)
            .ok_or_else(|| anyhow!("resolved zone `{}` vanished from API", zone.name))?;
        let acct = full.account.as_ref().ok_or_else(|| {
            anyhow!(
                "zone `{}` API response had no `account` field; \
                     cannot derive account_id",
                zone.name
            )
        })?;
        accounts.insert(acct.id.clone());
    }
    match accounts.len() {
        0 => bail!("no resolved zones; cannot derive account_id"),
        1 => Ok(accounts.into_iter().next().unwrap()),
        _ => bail!(
            "resolved zones span multiple CF accounts ({:?}); \
             auto-create requires all --zone arguments to belong to a \
             single account",
            accounts
        ),
    }
}

fn server_config_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("shiku")
        .join("server.toml"))
}

fn write_server_config(cfg: &ServerConfig) -> Result<()> {
    let path = server_config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let body = toml::to_string_pretty(cfg).context("serializing ServerConfig")?;
    write_atomic_with_mode(&path, body.as_bytes(), 0o600)?;
    tracing::info!(path = %path.display(), "wrote server.toml");
    Ok(())
}

/// AppConfig for the `__shiku__` pseudo-app. Minimal — just enough to
/// hold the secrets allowlist; no binary, no listen, no health check.
fn pseudo_app_config() -> AppConfig {
    AppConfig {
        name: apps::PSEUDO_APP_NAME.to_string(),
        command: String::new(),
        service: apps::PSEUDO_APP_NAME.to_string(),
        working_dir: None,
        secrets: vec![
            TUNNEL_TOKEN_SECRET.to_string(),
            API_TOKEN_SECRET.to_string(),
        ],
        bindings: vec![],
        vars: Default::default(),
        listen: None,
        health: HealthSpec::Process { settle_secs: 0 },
        systemd: SystemdSpec::default(),
        public: vec![],
    }
}

fn write_atomic_with_mode(path: &std::path::Path, body: &[u8], mode: u32) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("path has no parent: {}", path.display()))?;
    let tmp = parent.join(format!(
        ".{}.tmp.{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        std::process::id()
    ));
    {
        let mut f = std::fs::File::create(&tmp)
            .with_context(|| format!("creating tempfile {}", tmp.display()))?;
        f.write_all(body)
            .with_context(|| format!("writing tempfile {}", tmp.display()))?;
        f.sync_all().context("fsync tempfile")?;
    }
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}
