//! Minimal Cloudflare REST API client.
//!
//! The agent uses this to manage DNS records for hostnames declared in
//! apps' `public = [...]` lists. Scope is intentionally tiny — four
//! endpoints, all DNS-record CRUD plus zone listing. Anything else
//! (tunnel CRUD, zone settings, account ops) is deliberately out of
//! scope; tunnels are created once at bootstrap, by hand or by a
//! separate code path.
//!
//! Credentials: a CF API token with `Zone.DNS:Edit` on the relevant
//! zones. The token is stored on the box as a Shiku-managed secret
//! (`CF_API_TOKEN` under the `__shiku__` pseudo-app — see
//! `docs/ingress.md`).
//!
//! ## Error shape
//!
//! [`CfError`] carries the failing operation, the underlying cause, and
//! — when CF returned a body — the structured CF error array. Maps to
//! [`shiku_types::ErrorKind::DnsApiFailed`] at the dispatch boundary.

// Phase 2 lands the client in isolation; phase 4 wires it into
// activation. Until then, nothing in the rest of the agent calls into
// this module — silence dead-code warnings without burying the
// individual symbols' intent.
#![allow(dead_code)]

use std::time::Duration;

use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Cloudflare API base. Constant — there's no staging endpoint we'd
/// need to swap in.
pub const API_BASE: &str = "https://api.cloudflare.com/client/v4";

/// Default request timeout. CF's API is generally fast (sub-second);
/// 15s is generous and bounds rare slow paths.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

/// Authenticated CF API client. Cheap to create per-call but cheap to
/// reuse too — internally holds a `reqwest::Client` (connection pool).
pub struct CfApi {
    http: Client,
    token: String,
    base: String,
}

impl CfApi {
    /// Build a client with the given API token. The token is sent as
    /// `Authorization: Bearer ...` on every request.
    pub fn new(token: impl Into<String>) -> Self {
        Self::with_base(token, API_BASE.to_string())
    }

    /// Same as [`new`] but with a custom base URL — used by tests
    /// against a mock server. Production code uses [`new`].
    pub fn with_base(token: impl Into<String>, base: String) -> Self {
        let http = Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .user_agent(concat!("shiku/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest client builder should not fail with default config");
        Self {
            http,
            token: token.into(),
            base,
        }
    }

    /// List zones the token can access. Used at bootstrap to validate
    /// that the token covers each `--zone` the operator declared, and
    /// to look up `zone_id` per zone name.
    ///
    /// Returns every zone the token has *any* permission on; callers
    /// filter by name. CF paginates at 50 per page by default; we
    /// follow pagination here so the caller sees the full list.
    pub async fn list_zones(&self) -> Result<Vec<Zone>, CfError> {
        let mut all = Vec::new();
        let mut page = 1u32;
        loop {
            let url = format!("{}/zones", self.base);
            let resp = self
                .http
                .get(&url)
                .bearer_auth(&self.token)
                .query(&[("page", page.to_string()), ("per_page", "50".to_string())])
                .send()
                .await
                .map_err(|e| CfError::transport("list_zones", e))?;
            let body: CfResponse<Vec<Zone>> = decode_body("list_zones", resp).await?;
            let result = body.result.unwrap_or_default();
            let info = body.result_info.unwrap_or_default();
            let result_len = result.len();
            all.extend(result);
            // Pagination terminator: total_pages == 0 (unusual) or we
            // just consumed the last page. We also stop if a page
            // returned fewer than expected, since some CF endpoints are
            // inconsistent about returning total_pages.
            if info.total_pages <= page || result_len == 0 {
                break;
            }
            page += 1;
        }
        Ok(all)
    }

    /// Find a single DNS record by `(zone_id, name, kind)`. Returns
    /// `Ok(None)` when no record matches — that's the normal "this
    /// hostname isn't published yet" case, not an error.
    ///
    /// `name` is the fully-qualified hostname (e.g. `norn.augminted.cc`),
    /// not the relative form. CF's API accepts and returns FQDNs.
    pub async fn find_dns_record(
        &self,
        zone_id: &str,
        name: &str,
        kind: RecordKind,
    ) -> Result<Option<DnsRecord>, CfError> {
        let url = format!("{}/zones/{}/dns_records", self.base, zone_id);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.token)
            .query(&[("type", kind.as_str()), ("name", name)])
            .send()
            .await
            .map_err(|e| CfError::transport("find_dns_record", e))?;
        let body: CfResponse<Vec<DnsRecord>> = decode_body("find_dns_record", resp).await?;
        let mut records = body.result.unwrap_or_default();
        // The list endpoint returns 0 or 1 results for a name+type
        // filter under normal conditions. If somehow >1, take the
        // first; the caller is interested in whether *any* record
        // exists, and a duplicate would be a CF-side anomaly.
        Ok(records.pop())
    }

    /// Create-or-update a DNS record. If a record with the same
    /// `(name, type)` already exists, it's updated in place; otherwise
    /// a new record is created. The returned [`DnsRecord`] reflects
    /// the current state on CF.
    ///
    /// `proxied=true` is the right default for CF tunnels: traffic must
    /// go through CF's edge for the tunnel target to resolve.
    pub async fn upsert_dns_record(
        &self,
        zone_id: &str,
        spec: &DnsRecordSpec,
    ) -> Result<DnsRecord, CfError> {
        // Look first; if we find a match, PATCH. Otherwise POST.
        let existing = self.find_dns_record(zone_id, &spec.name, spec.kind).await?;
        match existing {
            Some(rec) => self.update_dns_record(zone_id, &rec.id, spec).await,
            None => self.create_dns_record(zone_id, spec).await,
        }
    }

    /// Delete a DNS record by ID. Idempotent at the call layer: if
    /// CF returns 404, we treat it as success (record is gone, which
    /// is what the caller wanted).
    pub async fn delete_dns_record(&self, zone_id: &str, record_id: &str) -> Result<(), CfError> {
        let url = format!("{}/zones/{}/dns_records/{}", self.base, zone_id, record_id);
        let resp = self
            .http
            .delete(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| CfError::transport("delete_dns_record", e))?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(());
        }
        // Drain the body for diagnostics on failure paths.
        let _: CfResponse<serde_json::Value> = decode_body("delete_dns_record", resp).await?;
        Ok(())
    }

    // ---- Tunnels ----

    /// List all `cfd_tunnel`s on an account, optionally filtered by
    /// name. CF returns deleted tunnels by default; we filter them out
    /// so callers see only live ones.
    pub async fn list_tunnels(
        &self,
        account_id: &str,
        name: Option<&str>,
    ) -> Result<Vec<Tunnel>, CfError> {
        let url = format!("{}/accounts/{}/cfd_tunnel", self.base, account_id);
        let mut req = self.http.get(&url).bearer_auth(&self.token);
        if let Some(n) = name {
            req = req.query(&[("name", n)]);
        }
        // Filter out deleted tunnels; CF returns them by default.
        req = req.query(&[("is_deleted", "false")]);
        let resp = req
            .send()
            .await
            .map_err(|e| CfError::transport("list_tunnels", e))?;
        let body: CfResponse<Vec<Tunnel>> = decode_body("list_tunnels", resp).await?;
        Ok(body.result.unwrap_or_default())
    }

    /// Create a new `cfd_tunnel` (the kind cloudflared connects to via
    /// `tunnel run --token`). The response includes a base64-encoded
    /// secret which combines with `tunnel_id` and `account_tag` to form
    /// the runtime token — see [`reconstruct_tunnel_token`].
    pub async fn create_tunnel(&self, account_id: &str, name: &str) -> Result<Tunnel, CfError> {
        let url = format!("{}/accounts/{}/cfd_tunnel", self.base, account_id);
        // Body: just `{name, config_src}`. `config_src=cloudflare` means
        // the tunnel's config is managed via dashboard / API; `local`
        // means it's read from a local config.yml. We use `local` because
        // Shiku writes config.yml directly.
        #[derive(Serialize)]
        struct CreateTunnelBody<'a> {
            name: &'a str,
            config_src: &'a str,
        }
        let body = CreateTunnelBody {
            name,
            config_src: "local",
        };
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .map_err(|e| CfError::transport("create_tunnel", e))?;
        let parsed: CfResponse<Tunnel> = decode_body("create_tunnel", resp).await?;
        parsed.result.ok_or(CfError::Empty {
            op: "create_tunnel",
        })
    }

    /// Fetch the runtime token for an existing tunnel. CF used to embed
    /// `tunnel_secret` in the `create_tunnel` response (we'd then
    /// base64-encode `{a, t, s}` ourselves), but the modern API returns
    /// the fully-formed runtime token via a dedicated endpoint instead.
    /// The string CF returns goes straight into `cloudflared tunnel run
    /// --token <X>`.
    ///
    /// The endpoint is callable for tunnels we just created *and* for
    /// existing tunnels where we know the UUID — but it requires the
    /// API token to have `Account.Cloudflare Tunnel:Edit` (the same
    /// scope auto-create needs).
    pub async fn get_tunnel_token(
        &self,
        account_id: &str,
        tunnel_id: &str,
    ) -> Result<String, CfError> {
        let url = format!(
            "{}/accounts/{}/cfd_tunnel/{}/token",
            self.base, account_id, tunnel_id
        );
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| CfError::transport("get_tunnel_token", e))?;
        // CF wraps the token (a plain string) in the standard envelope:
        //   { "success": true, "result": "<token>", ... }
        let body: CfResponse<String> = decode_body("get_tunnel_token", resp).await?;
        body.result.ok_or(CfError::Empty {
            op: "get_tunnel_token",
        })
    }

    /// Delete a tunnel. Used for cleanup symmetry; not called during
    /// regular operation. CF requires the tunnel to be disconnected
    /// (no active connectors) before delete will succeed.
    #[allow(dead_code)]
    pub async fn delete_tunnel(&self, account_id: &str, tunnel_id: &str) -> Result<(), CfError> {
        let url = format!(
            "{}/accounts/{}/cfd_tunnel/{}",
            self.base, account_id, tunnel_id
        );
        let resp = self
            .http
            .delete(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| CfError::transport("delete_tunnel", e))?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(());
        }
        let _: CfResponse<serde_json::Value> = decode_body("delete_tunnel", resp).await?;
        Ok(())
    }

    async fn create_dns_record(
        &self,
        zone_id: &str,
        spec: &DnsRecordSpec,
    ) -> Result<DnsRecord, CfError> {
        let url = format!("{}/zones/{}/dns_records", self.base, zone_id);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .json(spec)
            .send()
            .await
            .map_err(|e| CfError::transport("create_dns_record", e))?;
        let body: CfResponse<DnsRecord> = decode_body("create_dns_record", resp).await?;
        body.result.ok_or_else(|| CfError::Empty {
            op: "create_dns_record",
        })
    }

    async fn update_dns_record(
        &self,
        zone_id: &str,
        record_id: &str,
        spec: &DnsRecordSpec,
    ) -> Result<DnsRecord, CfError> {
        let url = format!("{}/zones/{}/dns_records/{}", self.base, zone_id, record_id);
        let resp = self
            .http
            .patch(&url)
            .bearer_auth(&self.token)
            .json(spec)
            .send()
            .await
            .map_err(|e| CfError::transport("update_dns_record", e))?;
        let body: CfResponse<DnsRecord> = decode_body("update_dns_record", resp).await?;
        body.result.ok_or_else(|| CfError::Empty {
            op: "update_dns_record",
        })
    }
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// The DNS record types we ever touch. Tunnels use CNAME; A/AAAA are
/// here for completeness so callers can disambiguate when querying.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum RecordKind {
    A,
    Aaaa,
    Cname,
}

impl RecordKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::A => "A",
            Self::Aaaa => "AAAA",
            Self::Cname => "CNAME",
        }
    }
}

/// What we send to CF when creating or updating a DNS record.
#[derive(Debug, Clone, Serialize)]
pub struct DnsRecordSpec {
    /// `"CNAME"`, `"A"`, etc. Matches `RecordKind`.
    #[serde(rename = "type")]
    pub kind: RecordKind,
    /// Fully-qualified hostname (e.g. `norn.augminted.cc`).
    pub name: String,
    /// CNAME target / A address / AAAA address. For tunnels this is
    /// `<tunnel-uuid>.cfargotunnel.com`.
    pub content: String,
    /// 1 = automatic; otherwise a TTL in seconds. Use `1` for tunnel
    /// records (CF picks).
    pub ttl: u32,
    /// Whether to proxy through CF's edge. Always `true` for tunnels.
    pub proxied: bool,
    /// Optional human-readable comment. Useful for marking records
    /// Shiku owns (`"managed by shiku"` etc).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

impl DnsRecordSpec {
    /// Convenience builder for the tunnel CNAME case — the only shape
    /// Shiku actually creates.
    pub fn tunnel_cname(name: impl Into<String>, tunnel_id: &str) -> Self {
        Self {
            kind: RecordKind::Cname,
            name: name.into(),
            content: format!("{tunnel_id}.cfargotunnel.com"),
            ttl: 1,
            proxied: true,
            comment: Some("managed by shiku".to_string()),
        }
    }
}

/// One DNS record as returned by CF.
#[derive(Debug, Clone, Deserialize)]
pub struct DnsRecord {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String, // CF returns the type as a free-form string
    pub name: String,
    pub content: String,
    #[serde(default)]
    pub proxied: bool,
    #[serde(default)]
    pub ttl: u32,
}

/// One zone as returned by CF. We consume `id`, `name`, and the nested
/// `account.id` so callers can find the account a zone belongs to
/// without a separate `/accounts` lookup.
#[derive(Debug, Clone, Deserialize)]
pub struct Zone {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub account: Option<ZoneAccount>,
}

/// The `account` sub-object on a zone response.
#[derive(Debug, Clone, Deserialize)]
pub struct ZoneAccount {
    pub id: String,
    #[serde(default)]
    pub name: String,
}

/// One Cloudflare tunnel as returned by `/accounts/<id>/cfd_tunnel`.
#[derive(Debug, Clone, Deserialize)]
pub struct Tunnel {
    pub id: String,
    pub name: String,
    /// `cfd_tunnel` response includes `account_tag`. We need it both
    /// as a sanity check and (for newly-created tunnels) to
    /// reconstruct the runtime token.
    #[serde(default)]
    pub account_tag: String,
    /// Returned only on the create response. Base64-encoded shared
    /// secret used to reconstruct the runtime token.
    #[serde(default, rename = "tunnel_secret")]
    pub secret: Option<String>,
    /// `null` when the tunnel is alive; populated when it's been deleted.
    #[serde(default)]
    pub deleted_at: Option<String>,
    /// `created`, `healthy`, `degraded`, `down`, `inactive`. We don't
    /// match on this, but it's useful when surfacing errors.
    #[serde(default)]
    pub status: String,
}

// ---------------------------------------------------------------------------
// Runtime-token reconstruction
// ---------------------------------------------------------------------------

/// Construct the cloudflared runtime token from `(account_tag, tunnel_id,
/// tunnel_secret)`. Modern CF API returns the token directly via
/// [`CfApi::get_tunnel_token`]; this helper is kept for the rare legacy
/// case where only the secret is available.
///
/// The token is `base64(json({a, t, s}))`. We never store this token
/// unencrypted; the agent passes it straight into the secrets store.
#[allow(dead_code)]
pub fn reconstruct_tunnel_token(account_tag: &str, tunnel_id: &str, tunnel_secret: &str) -> String {
    use base64::Engine;
    #[derive(Serialize)]
    struct RuntimeToken<'a> {
        a: &'a str,
        t: &'a str,
        s: &'a str,
    }
    let json = serde_json::to_string(&RuntimeToken {
        a: account_tag,
        t: tunnel_id,
        s: tunnel_secret,
    })
    .expect("RuntimeToken serialization is infallible");
    base64::engine::general_purpose::STANDARD.encode(json.as_bytes())
}

// ---------------------------------------------------------------------------
// Generic CF response envelope
// ---------------------------------------------------------------------------

/// CF wraps every response in `{success, errors, messages, result}`.
/// `result` is the endpoint-specific payload; `errors` is a structured
/// array we surface verbatim on failure.
#[derive(Debug, Deserialize)]
struct CfResponse<T> {
    success: bool,
    #[serde(default)]
    errors: Vec<CfApiError>,
    result: Option<T>,
    result_info: Option<ResultInfo>,
}

#[derive(Debug, Default, Deserialize, Clone)]
struct ResultInfo {
    #[serde(default)]
    total_pages: u32,
}

/// One entry in CF's `errors` array.
#[derive(Debug, Clone, Deserialize)]
pub struct CfApiError {
    pub code: i64,
    pub message: String,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors from CF API calls. Each variant carries the operation name
/// for cleaner logs / detail strings.
#[derive(Debug, Error)]
pub enum CfError {
    #[error("{op}: transport: {source}")]
    Transport {
        op: &'static str,
        #[source]
        source: reqwest::Error,
    },

    #[error("{op}: HTTP {status}{}", format_cf_errors(.errors))]
    Api {
        op: &'static str,
        status: StatusCode,
        errors: Vec<CfApiError>,
    },

    #[error("{op}: response decode: {source}")]
    Decode {
        op: &'static str,
        #[source]
        source: serde_json::Error,
    },

    #[error("{op}: response had success=true but no result body")]
    Empty { op: &'static str },
}

impl CfError {
    fn transport(op: &'static str, source: reqwest::Error) -> Self {
        Self::Transport { op, source }
    }
}

fn format_cf_errors(errs: &[CfApiError]) -> String {
    if errs.is_empty() {
        String::new()
    } else {
        let joined = errs
            .iter()
            .map(|e| format!("{}={}", e.code, e.message))
            .collect::<Vec<_>>()
            .join(", ");
        format!(" — {joined}")
    }
}

/// Read the body, parse, and return the typed result. On HTTP error or
/// `success=false`, surface a [`CfError::Api`] with the structured
/// errors array. On JSON parse failure, [`CfError::Decode`].
async fn decode_body<T: for<'de> Deserialize<'de>>(
    op: &'static str,
    resp: reqwest::Response,
) -> Result<CfResponse<T>, CfError> {
    let status = resp.status();
    let bytes = resp.bytes().await.map_err(|e| CfError::transport(op, e))?;

    // Attempt to parse the envelope even on HTTP error — CF returns the
    // structured envelope for most failures, and we want the errors
    // array to surface in our error type.
    let parsed: Result<CfResponse<T>, serde_json::Error> = serde_json::from_slice(&bytes);

    match parsed {
        Ok(body) if body.success => Ok(body),
        Ok(body) => Err(CfError::Api {
            op,
            status,
            errors: body.errors,
        }),
        Err(e) if !status.is_success() => {
            // Body wasn't the standard envelope (rare — usually only on
            // gateway-level failures). Surface the HTTP status with no
            // structured errors.
            tracing::debug!(op, %status, "CF response was not a parseable envelope: {e}");
            Err(CfError::Api {
                op,
                status,
                errors: Vec::new(),
            })
        }
        Err(e) => Err(CfError::Decode { op, source: e }),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_kind_serialises_uppercase() {
        let spec = DnsRecordSpec::tunnel_cname("norn.augminted.cc", "abc-123");
        let json = serde_json::to_string(&spec).expect("serialize");
        assert!(json.contains("\"type\":\"CNAME\""));
        assert!(json.contains("\"proxied\":true"));
        assert!(json.contains("\"name\":\"norn.augminted.cc\""));
        assert!(json.contains("\"content\":\"abc-123.cfargotunnel.com\""));
        assert!(json.contains("\"ttl\":1"));
    }

    #[test]
    fn tunnel_cname_builder_sets_proxied() {
        let spec = DnsRecordSpec::tunnel_cname("foo.augminted.cc", "uuid");
        assert_eq!(spec.kind, RecordKind::Cname);
        assert!(spec.proxied);
        assert_eq!(spec.ttl, 1);
        assert_eq!(spec.content, "uuid.cfargotunnel.com");
    }

    #[test]
    fn cf_response_decodes_success_envelope() {
        let body = r#"{
            "success": true,
            "errors": [],
            "messages": [],
            "result": {
                "id": "rec-1",
                "type": "CNAME",
                "name": "norn.augminted.cc",
                "content": "abc.cfargotunnel.com",
                "proxied": true,
                "ttl": 1
            }
        }"#;
        let parsed: CfResponse<DnsRecord> = serde_json::from_str(body).expect("parse");
        assert!(parsed.success);
        let rec = parsed.result.expect("result");
        assert_eq!(rec.name, "norn.augminted.cc");
        assert_eq!(rec.kind, "CNAME");
    }

    #[test]
    fn cf_response_decodes_error_envelope() {
        let body = r#"{
            "success": false,
            "errors": [{ "code": 9999, "message": "unauthorized" }],
            "messages": [],
            "result": null
        }"#;
        let parsed: CfResponse<DnsRecord> = serde_json::from_str(body).expect("parse");
        assert!(!parsed.success);
        assert_eq!(parsed.errors.len(), 1);
        assert_eq!(parsed.errors[0].code, 9999);
    }

    #[test]
    fn reconstruct_tunnel_token_round_trips() {
        use base64::Engine;
        let token = reconstruct_tunnel_token("acct-tag", "tunnel-uuid", "secret-base64");
        // Decoding the token should yield JSON with the expected shape.
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&token)
            .expect("base64 decode");
        let json: serde_json::Value = serde_json::from_slice(&decoded).expect("json");
        assert_eq!(json["a"], "acct-tag");
        assert_eq!(json["t"], "tunnel-uuid");
        assert_eq!(json["s"], "secret-base64");
    }

    #[test]
    fn zone_decodes_with_account() {
        let body = r#"{
            "id": "zone-id",
            "name": "augminted.cc",
            "status": "active",
            "account": { "id": "acct-id", "name": "My Account" }
        }"#;
        let z: Zone = serde_json::from_str(body).expect("parse");
        assert_eq!(z.id, "zone-id");
        assert_eq!(z.account.as_ref().expect("account").id, "acct-id");
    }

    #[test]
    fn tunnel_decodes_create_response_shape() {
        let body = r#"{
            "id": "tunnel-uuid",
            "name": "shiku-test",
            "account_tag": "acct-tag",
            "tunnel_secret": "abc123base64==",
            "deleted_at": null,
            "status": "inactive"
        }"#;
        let t: Tunnel = serde_json::from_str(body).expect("parse");
        assert_eq!(t.id, "tunnel-uuid");
        assert_eq!(t.account_tag, "acct-tag");
        assert_eq!(t.secret.as_deref(), Some("abc123base64=="));
        assert!(t.deleted_at.is_none());
    }

    #[test]
    fn format_cf_errors_handles_empty_and_populated() {
        assert_eq!(format_cf_errors(&[]), "");
        let errs = vec![
            CfApiError {
                code: 1,
                message: "first".into(),
            },
            CfApiError {
                code: 2,
                message: "second".into(),
            },
        ];
        let formatted = format_cf_errors(&errs);
        assert!(formatted.contains("1=first"));
        assert!(formatted.contains("2=second"));
    }

    // ---- Live integration tests ----
    //
    // Skipped unless both `SHIKU_CF_TEST_TOKEN` and `SHIKU_CF_TEST_ZONE`
    // are set. The token must have `Zone.DNS:Edit` on the zone. The zone
    // must be the bare apex (e.g. `augminted.cc`).
    //
    // To run:
    //   SHIKU_CF_TEST_TOKEN=... SHIKU_CF_TEST_ZONE=augminted.cc \
    //       cargo test -p shikud --bin shikud -- --ignored --test-threads=1 cf_api::tests::live
    //
    // The tests pick a random subdomain prefix per run so concurrent
    // runs (and crashed-mid-run leftovers) don't collide.

    fn live_token() -> Option<String> {
        std::env::var("SHIKU_CF_TEST_TOKEN").ok()
    }
    fn live_zone() -> Option<String> {
        std::env::var("SHIKU_CF_TEST_ZONE").ok()
    }

    fn rand_subdomain() -> String {
        // Don't pull rand just for this — use the lower bytes of nanos
        // since UNIX_EPOCH. Plenty of entropy for "no collision in a
        // test run that takes seconds."
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("shiku-test-{:x}", nanos & 0xffff_ffff)
    }

    #[tokio::test]
    #[ignore = "requires SHIKU_CF_TEST_TOKEN + SHIKU_CF_TEST_ZONE"]
    async fn live_list_zones_finds_test_zone() {
        let token = match live_token() {
            Some(t) => t,
            None => return,
        };
        let zone_name = live_zone().expect("SHIKU_CF_TEST_ZONE required");
        let api = CfApi::new(token);
        let zones = api.list_zones().await.expect("list_zones");
        assert!(
            zones.iter().any(|z| z.name == zone_name),
            "expected zone `{zone_name}` not found in token-accessible zones; \
             got: {:?}",
            zones.iter().map(|z| &z.name).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    #[ignore = "requires SHIKU_CF_TEST_TOKEN + SHIKU_CF_TEST_ZONE"]
    async fn live_dns_record_lifecycle() {
        let token = match live_token() {
            Some(t) => t,
            None => return,
        };
        let zone_name = live_zone().expect("SHIKU_CF_TEST_ZONE required");
        let api = CfApi::new(token);

        // Look up zone_id for the test zone.
        let zones = api.list_zones().await.expect("list_zones");
        let zone = zones
            .into_iter()
            .find(|z| z.name == zone_name)
            .expect("test zone not in token-accessible zones");

        let host = format!("{}.{}", rand_subdomain(), zone_name);
        let spec = DnsRecordSpec::tunnel_cname(&host, "shiku-test-fake-tunnel-uuid");

        // 1. Pre-flight: confirm the record doesn't exist.
        let pre = api
            .find_dns_record(&zone.id, &host, RecordKind::Cname)
            .await
            .expect("find pre-create");
        assert!(pre.is_none(), "test hostname `{host}` already had a record");

        // 2. Create.
        let created = api
            .upsert_dns_record(&zone.id, &spec)
            .await
            .expect("upsert create");
        assert_eq!(created.name, host);
        assert_eq!(created.kind, "CNAME");

        // 3. Confirm via find.
        let found = api
            .find_dns_record(&zone.id, &host, RecordKind::Cname)
            .await
            .expect("find post-create")
            .expect("record present");
        assert_eq!(found.id, created.id);

        // 4. Upsert again — should update in place, not create a duplicate.
        let updated_spec = DnsRecordSpec {
            content: "shiku-test-different-uuid.cfargotunnel.com".to_string(),
            ..spec
        };
        let updated = api
            .upsert_dns_record(&zone.id, &updated_spec)
            .await
            .expect("upsert update");
        assert_eq!(
            updated.id, created.id,
            "upsert created a new record instead of updating"
        );
        assert_eq!(updated.content, updated_spec.content);

        // 5. Delete.
        api.delete_dns_record(&zone.id, &created.id)
            .await
            .expect("delete");

        // 6. Confirm gone.
        let post_delete = api
            .find_dns_record(&zone.id, &host, RecordKind::Cname)
            .await
            .expect("find post-delete");
        assert!(post_delete.is_none(), "record still present after delete");

        // 7. Delete a second time — should be idempotent (CF returns 404,
        //    we treat as success).
        api.delete_dns_record(&zone.id, &created.id)
            .await
            .expect("delete idempotent");
    }
}
