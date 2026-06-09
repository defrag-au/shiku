//! Reconcile a registered app's `public` list against actual DNS + ingress.
//!
//! Called from the `AppRegister` and `AppRemove` dispatch paths after
//! validation has passed. The reconcile order matters:
//!
//!   On *add* (hostname appearing in `public` for the first time):
//!     1. Upsert DNS CNAME → tunnel target. Operator-side first so that
//!        once the ingress rule appears, the hostname routes correctly.
//!     2. Regenerate `~/.cloudflared/config.yml` and SIGHUP cloudflared.
//!
//!   On *remove* (hostname dropping out):
//!     1. Regenerate config.yml and reload cloudflared first — so the
//!        tunnel stops accepting traffic for that hostname.
//!     2. Then delete the DNS record.
//!
//! A failure during reconcile aborts and reports the typed error; the
//! dispatch layer at the call site is responsible for not having
//! committed any persistent state before the publish completes.
//!
//! See `docs/design/shiku/tunnel.md` §6.4.

use anyhow::{Context, Result};
use std::collections::HashSet;

use shiku_types::{AppName, ErrorKind};

use crate::apps;
use crate::cf_api::{CfApi, CfError, DnsRecordSpec, RecordKind};
use crate::cloudflared;
use crate::ingress;
use crate::secrets;
use crate::server_config::{self, ServerConfig};
use secrecy::ExposeSecret;

/// One reconciliation outcome the dispatch layer needs to map to a wire
/// response. Everything that isn't [`PublishOutcome::Ok`] surfaces as a
/// typed `ErrorKind`.
#[derive(Debug)]
pub enum PublishOutcome {
    Ok,
    NoServerConfig,
    DnsApiFailed(String),
    IngressFailed(String),
    ReloadFailed(String),
    MissingTunnelToken,
}

impl PublishOutcome {
    pub fn as_response_parts(&self) -> Option<(ErrorKind, String)> {
        match self {
            Self::Ok => None,
            Self::NoServerConfig => Some((
                ErrorKind::ZoneNotManaged,
                "no `~/.config/shiku/server.toml`; run `shiku tunnel \
                 bootstrap` before declaring `public = [...]`"
                    .to_string(),
            )),
            Self::DnsApiFailed(msg) => Some((ErrorKind::DnsApiFailed, msg.clone())),
            Self::IngressFailed(msg) => Some((ErrorKind::CloudflaredReloadFailed, msg.clone())),
            Self::ReloadFailed(msg) => Some((ErrorKind::CloudflaredReloadFailed, msg.clone())),
            Self::MissingTunnelToken => Some((
                ErrorKind::Internal,
                "CF API token (`CF_API_TOKEN`) not set under the \
                 `__shiku__` pseudo-app. re-run `shiku tunnel bootstrap`."
                    .to_string(),
            )),
        }
    }
}

/// Reconcile a single app's `public` list to the new desired state.
///
/// `previous` is the public list the app held before this register call
/// (or empty for first-register). `desired` is the new list the caller
/// wants to commit. Validation has already ensured every entry in
/// `desired` is well-formed and not stolen.
pub async fn reconcile(app: &AppName, previous: &[String], desired: &[String]) -> PublishOutcome {
    // No diff-based early return: ingress regen + SIGHUP run
    // unconditionally so a partial-failure retry can converge. DNS
    // upsert/delete is set-diffed below — same hostname twice is a
    // no-op at CF.

    let server = match server_config::load() {
        Ok(Some(cfg)) => cfg,
        Ok(None) => return PublishOutcome::NoServerConfig,
        Err(e) => {
            tracing::warn!(error = %e, "reading server.toml during reconcile");
            return PublishOutcome::NoServerConfig;
        }
    };

    let cf = match build_cf_client() {
        Ok(c) => c,
        Err(outcome) => return outcome,
    };

    let prev_set: HashSet<&str> = previous.iter().map(String::as_str).collect();
    let next_set: HashSet<&str> = desired.iter().map(String::as_str).collect();
    let added: Vec<&str> = next_set.difference(&prev_set).copied().collect();
    let removed: Vec<&str> = prev_set.difference(&next_set).copied().collect();

    tracing::info!(
        app = %app,
        added = ?added,
        removed = ?removed,
        "reconciling public hostnames"
    );

    // ---- ADD path ----
    // DNS upserts first, then ingress regen + reload.
    for hostname in &added {
        if let Err(outcome) = upsert_dns(&cf, &server, hostname).await {
            return outcome;
        }
    }

    // Regenerate the entire config.yml from the apps registry. We
    // briefly persist the *new* desired list to a synthetic in-memory
    // view by writing it to disk first... no — to avoid that
    // ordering hazard, callers persist the new config *before* calling
    // reconcile. (See dispatch wiring in main.rs.)
    if let Err(e) = ingress::regenerate(&server.tunnel.id) {
        return PublishOutcome::IngressFailed(format!("{e:#}"));
    }
    if let Err(e) = cloudflared::reload().await {
        return PublishOutcome::ReloadFailed(format!("{e:#}"));
    }

    // ---- REMOVE path ----
    // Ingress reload above already stopped routing for removed
    // hostnames (their entries are gone from config.yml). Now clean up
    // the DNS records.
    for hostname in &removed {
        if let Err(outcome) = delete_dns(&cf, &server, hostname).await {
            // A failed DNS delete is annoying but not catastrophic —
            // the hostname no longer routes anywhere useful (ingress
            // is gone, so cloudflared 404s it). Surface as
            // DnsApiFailed but keep going for any remaining removals.
            tracing::warn!(
                hostname,
                ?outcome,
                "DNS delete failed during reconcile; continuing"
            );
            // Still return failure so the caller sees something went
            // wrong. We've already mutated state (the ingress is
            // updated) so callers should take this as "mostly done."
            return outcome;
        }
    }

    PublishOutcome::Ok
}

/// Pull the CF API token from the `__shiku__` pseudo-app's secrets
/// store and build a client. Returns `MissingTunnelToken` if not set.
fn build_cf_client() -> Result<CfApi, PublishOutcome> {
    let secret = secrets::get(&"CF_API_TOKEN".to_string())
        .map_err(|e| {
            tracing::warn!(error = %e, "decrypting CF_API_TOKEN");
            PublishOutcome::MissingTunnelToken
        })?
        .ok_or(PublishOutcome::MissingTunnelToken)?;
    Ok(CfApi::new(secret.expose_secret().to_string()))
}

async fn upsert_dns(
    cf: &CfApi,
    server: &ServerConfig,
    hostname: &str,
) -> Result<(), PublishOutcome> {
    let zone = server.match_zone(hostname).ok_or_else(|| {
        // Validation should have caught this, but defense in depth.
        PublishOutcome::DnsApiFailed(format!(
            "no managed zone matched `{hostname}` at reconcile time"
        ))
    })?;
    let spec = DnsRecordSpec::tunnel_cname(hostname, &server.tunnel.id);
    cf.upsert_dns_record(&zone.zone_id, &spec)
        .await
        .map(|_| ())
        .map_err(|e: CfError| PublishOutcome::DnsApiFailed(format!("{e:#}")))
}

async fn delete_dns(
    cf: &CfApi,
    server: &ServerConfig,
    hostname: &str,
) -> Result<(), PublishOutcome> {
    let zone = server.match_zone(hostname).ok_or_else(|| {
        PublishOutcome::DnsApiFailed(format!(
            "no managed zone matched `{hostname}` at delete time"
        ))
    })?;
    let existing = cf
        .find_dns_record(&zone.zone_id, hostname, RecordKind::Cname)
        .await
        .map_err(|e: CfError| PublishOutcome::DnsApiFailed(format!("{e:#}")))?;
    if let Some(rec) = existing {
        cf.delete_dns_record(&zone.zone_id, &rec.id)
            .await
            .map_err(|e: CfError| PublishOutcome::DnsApiFailed(format!("{e:#}")))?;
    }
    Ok(())
}

/// Small helper for callers that need to know the "currently registered"
/// public list for an app (the `previous` argument to `reconcile`).
pub fn current_public(app: &AppName) -> Result<Vec<String>> {
    match apps::inspect(app) {
        Ok(cfg) => Ok(cfg.public),
        Err(e) => {
            // If the app isn't registered, treat it as "previously had
            // no hostnames" — the caller is registering it for the first
            // time.
            let msg = format!("{e}");
            if msg.contains("not registered") {
                Ok(Vec::new())
            } else {
                Err(e).context("inspecting app for current public list")
            }
        }
    }
}
