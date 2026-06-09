//! The three checks that gate `public = [...]` declarations.
//!
//! Run agent-side at `AppRegister` time, before any DNS or ingress mutation.
//! All three must pass for every entry in the proposed list, or the entire
//! request is rejected — partial application would leave the box in a worse
//! state than rejection.
//!
//!   1. **Zone managed.** The hostname must fall under one of the zones in
//!      `~/.config/shiku/server.toml`. Otherwise the box has no auth to
//!      manage DNS for that zone.
//!   2. **Single-level subdomain.** The hostname must have exactly one
//!      label left of the matched zone. CF tunnel default certs only cover
//!      one level (`*.augminted.cc`); paid Advanced Cert Manager would
//!      lift this, but we don't depend on it.
//!   3. **Not claimed by another app.** No other registered app on this
//!      box can declare the same hostname. Self-republish is fine.
//!
//! See `docs/design/shiku/tunnel.md` §6.3.

use shiku_types::AppName;

use crate::server_config::{labels_left_of_zone, ServerConfig};

/// One reason a `public` entry was rejected. Maps 1:1 to the wire-level
/// `ErrorKind` variants so the dispatch layer is a thin translation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    /// No zone in `server.toml` is a suffix of this hostname.
    ZoneNotManaged {
        hostname: String,
        managed_zones: Vec<String>,
    },
    /// The hostname has more than one label left of its zone.
    MultiLevelSubdomain {
        hostname: String,
        zone: String,
        labels: usize,
    },
    /// Hostname is the bare zone apex (zero labels left of zone). Apex
    /// records aren't allowed (root domains are operator-owned).
    ZoneApexNotAllowed { hostname: String },
    /// Another registered app already claims this hostname.
    AlreadyClaimed {
        hostname: String,
        claimed_by: AppName,
    },
    /// Hostname failed basic syntax checks (empty, leading/trailing dot,
    /// invalid characters).
    InvalidHostname { hostname: String, reason: String },
}

impl ValidationError {
    /// Human-readable detail string. Format matches the design doc's
    /// example error messages.
    pub fn detail(&self) -> String {
        match self {
            Self::ZoneNotManaged {
                hostname,
                managed_zones,
            } => {
                let zones = if managed_zones.is_empty() {
                    "(none — run `shiku tunnel bootstrap` first)".to_string()
                } else {
                    managed_zones.join(", ")
                };
                format!(
                    "zone for `{hostname}` is not managed by this shiku install. \
                     managed zones: {zones}"
                )
            }
            Self::MultiLevelSubdomain {
                hostname,
                zone,
                labels,
            } => format!(
                "hostname `{hostname}`: only single-level subdomains under managed \
                 zones are supported (CF tunnel limitation). got {labels} labels left \
                 of zone `{zone}`."
            ),
            Self::ZoneApexNotAllowed { hostname } => format!(
                "hostname `{hostname}` is a managed-zone apex; only single-level \
                 subdomains are allowed."
            ),
            Self::AlreadyClaimed {
                hostname,
                claimed_by,
            } => format!(
                "hostname `{hostname}` is already claimed by app `{claimed_by}` on \
                 this box. unpublish there first or pick a different subdomain."
            ),
            Self::InvalidHostname { hostname, reason } => {
                format!("hostname `{hostname}`: {reason}")
            }
        }
    }
}

/// One app's claim on a hostname. Used to thread the "other apps' public
/// lists" into the conflict check without requiring the validator to read
/// the apps registry directly (keeps the unit tests pure).
#[derive(Debug, Clone)]
pub struct ExistingClaim<'a> {
    pub app: &'a AppName,
    pub hostname: &'a str,
}

/// Validate one entry. Returns `Ok(())` on pass, or the typed reason on
/// reject. Self-claims are filtered by the caller (see [`validate_list`]).
pub fn validate_entry(
    hostname: &str,
    server: &ServerConfig,
    existing_claims: &[ExistingClaim<'_>],
) -> Result<(), ValidationError> {
    // Basic syntax — keep the bar low, just enough that we don't ship
    // garbage to CF and get an opaque API error back.
    syntax_check(hostname)?;

    // Check 1: zone managed.
    let zone = server
        .match_zone(hostname)
        .ok_or_else(|| ValidationError::ZoneNotManaged {
            hostname: hostname.to_string(),
            managed_zones: server.zones.iter().map(|z| z.name.clone()).collect(),
        })?;

    // Check 2: single-level only (no apex, no nested).
    let labels = labels_left_of_zone(hostname, &zone.name);
    match labels {
        0 => {
            return Err(ValidationError::ZoneApexNotAllowed {
                hostname: hostname.to_string(),
            });
        }
        1 => {} // OK
        n => {
            return Err(ValidationError::MultiLevelSubdomain {
                hostname: hostname.to_string(),
                zone: zone.name.clone(),
                labels: n,
            });
        }
    }

    // Check 3: not stolen.
    let host_lower = hostname.to_ascii_lowercase();
    for claim in existing_claims {
        if claim.hostname.to_ascii_lowercase() == host_lower {
            return Err(ValidationError::AlreadyClaimed {
                hostname: hostname.to_string(),
                claimed_by: claim.app.clone(),
            });
        }
    }

    Ok(())
}

/// Validate every hostname in `proposed`. Returns the first rejection or
/// `Ok(())` if every entry passes.
///
/// `self_app` lets the caller exclude its own current claims from the
/// conflict check, so an app re-declaring its own existing hostnames isn't
/// flagged as stealing from itself.
pub fn validate_list(
    self_app: &AppName,
    proposed: &[String],
    server: &ServerConfig,
    other_claims: &[ExistingClaim<'_>],
) -> Result<(), ValidationError> {
    // Disallow duplicates within the same `public` list — the agent would
    // try to upsert the same DNS record twice. Cheap to catch here.
    let mut seen = std::collections::BTreeSet::<String>::new();
    for hostname in proposed {
        let key = hostname.to_ascii_lowercase();
        if !seen.insert(key) {
            return Err(ValidationError::InvalidHostname {
                hostname: hostname.clone(),
                reason: "duplicate entry in `public` list".to_string(),
            });
        }
    }

    let claims_excluding_self: Vec<ExistingClaim<'_>> = other_claims
        .iter()
        .filter(|c| c.app != self_app)
        .cloned()
        .collect();

    for hostname in proposed {
        validate_entry(hostname, server, &claims_excluding_self)?;
    }

    Ok(())
}

fn syntax_check(hostname: &str) -> Result<(), ValidationError> {
    if hostname.is_empty() {
        return Err(ValidationError::InvalidHostname {
            hostname: hostname.to_string(),
            reason: "empty hostname".to_string(),
        });
    }
    if hostname.len() > 253 {
        return Err(ValidationError::InvalidHostname {
            hostname: hostname.to_string(),
            reason: "hostname exceeds 253 chars".to_string(),
        });
    }
    if hostname.starts_with('.') || hostname.ends_with('.') {
        return Err(ValidationError::InvalidHostname {
            hostname: hostname.to_string(),
            reason: "hostname must not start or end with a dot".to_string(),
        });
    }
    if hostname.contains("..") {
        return Err(ValidationError::InvalidHostname {
            hostname: hostname.to_string(),
            reason: "hostname must not contain consecutive dots".to_string(),
        });
    }
    for label in hostname.split('.') {
        if label.is_empty() {
            return Err(ValidationError::InvalidHostname {
                hostname: hostname.to_string(),
                reason: "empty label".to_string(),
            });
        }
        if label.len() > 63 {
            return Err(ValidationError::InvalidHostname {
                hostname: hostname.to_string(),
                reason: "label exceeds 63 chars".to_string(),
            });
        }
        if !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(ValidationError::InvalidHostname {
                hostname: hostname.to_string(),
                reason: "label contains invalid characters (allowed: a-z 0-9 -)".to_string(),
            });
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(ValidationError::InvalidHostname {
                hostname: hostname.to_string(),
                reason: "label must not start or end with a hyphen".to_string(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server_config::{TunnelInfo, ZoneInfo};

    fn server() -> ServerConfig {
        ServerConfig {
            tunnel: TunnelInfo {
                id: "tunnel".into(),
            },
            zones: vec![
                ZoneInfo {
                    name: "augminted.cc".into(),
                    zone_id: "z1".into(),
                },
                ZoneInfo {
                    name: "augmint.bot".into(),
                    zone_id: "z2".into(),
                },
            ],
        }
    }

    #[test]
    fn happy_path() {
        let server = server();
        assert!(validate_entry("norn.augminted.cc", &server, &[]).is_ok());
        assert!(validate_entry("eternal-seas.augmint.bot", &server, &[]).is_ok());
    }

    #[test]
    fn rejects_unmanaged_zone() {
        let server = server();
        let err = validate_entry("svc.example.com", &server, &[]).unwrap_err();
        assert!(matches!(err, ValidationError::ZoneNotManaged { .. }));
        let detail = err.detail();
        assert!(detail.contains("example.com") || detail.contains("svc.example.com"));
        assert!(detail.contains("augminted.cc"));
    }

    #[test]
    fn rejects_multilevel_subdomain() {
        let server = server();
        let err = validate_entry("api.norn.augminted.cc", &server, &[]).unwrap_err();
        match err {
            ValidationError::MultiLevelSubdomain { labels, zone, .. } => {
                assert_eq!(labels, 2);
                assert_eq!(zone, "augminted.cc");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn rejects_zone_apex() {
        let server = server();
        let err = validate_entry("augminted.cc", &server, &[]).unwrap_err();
        assert!(matches!(err, ValidationError::ZoneApexNotAllowed { .. }));
    }

    #[test]
    fn rejects_already_claimed_by_other_app() {
        let server = server();
        let other = "eternal-seas".to_string();
        let claims = vec![ExistingClaim {
            app: &other,
            hostname: "norn.augminted.cc",
        }];
        let err = validate_entry("norn.augminted.cc", &server, &claims).unwrap_err();
        match err {
            ValidationError::AlreadyClaimed { claimed_by, .. } => {
                assert_eq!(claimed_by, "eternal-seas");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn case_insensitive_conflict_check() {
        let server = server();
        let other = "eternal-seas".to_string();
        let claims = vec![ExistingClaim {
            app: &other,
            hostname: "Norn.AUGMINTED.cc",
        }];
        let err = validate_entry("norn.augminted.cc", &server, &claims).unwrap_err();
        assert!(matches!(err, ValidationError::AlreadyClaimed { .. }));
    }

    #[test]
    fn validate_list_self_republish_is_ok() {
        let server = server();
        let me = "norn-server".to_string();
        // The "existing" claim list contains my own current claim — should
        // be filtered out, not flagged as conflict.
        let my_claim = me.clone();
        let other_claims = vec![ExistingClaim {
            app: &my_claim,
            hostname: "norn.augminted.cc",
        }];
        let proposed = vec!["norn.augminted.cc".to_string()];
        assert!(validate_list(&me, &proposed, &server, &other_claims).is_ok());
    }

    #[test]
    fn validate_list_rejects_duplicates_within_same_request() {
        let server = server();
        let me = "norn-server".to_string();
        let proposed = vec![
            "norn.augminted.cc".to_string(),
            "Norn.augminted.cc".to_string(), // case-insensitive duplicate
        ];
        let err = validate_list(&me, &proposed, &server, &[]).unwrap_err();
        assert!(matches!(err, ValidationError::InvalidHostname { .. }));
    }

    #[test]
    fn validate_list_validates_every_entry() {
        let server = server();
        let me = "norn-server".to_string();
        let proposed = vec![
            "norn.augminted.cc".to_string(),     // OK
            "api.norn.augminted.cc".to_string(), // multi-level — should fail
        ];
        let err = validate_list(&me, &proposed, &server, &[]).unwrap_err();
        assert!(matches!(err, ValidationError::MultiLevelSubdomain { .. }));
    }

    #[test]
    fn rejects_invalid_syntax() {
        let server = server();
        for bad in &[
            "",
            ".augminted.cc",
            "augminted.cc.",
            "norn..augminted.cc",
            "-norn.augminted.cc",
            "norn-.augminted.cc",
            "norn$.augminted.cc",
        ] {
            let err = validate_entry(bad, &server, &[]).unwrap_err();
            assert!(
                matches!(err, ValidationError::InvalidHostname { .. }),
                "expected InvalidHostname for {bad:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn empty_managed_zones_message_is_helpful() {
        let server = ServerConfig {
            tunnel: TunnelInfo { id: "t".into() },
            zones: vec![],
        };
        let err = validate_entry("norn.augminted.cc", &server, &[]).unwrap_err();
        let detail = err.detail();
        assert!(detail.contains("shiku tunnel bootstrap"));
    }
}
