//! Per-box Shiku tunnel/server config (`~/.config/shiku/server.toml`).
//!
//! Written by `shiku tunnel bootstrap` and read by the agent when validating
//! `public` hostnames or rendering cloudflared ingress. See
//! `docs/design/shiku/tunnel.md`.
//!
//! On-disk shape:
//!
//! ```toml
//! [tunnel]
//! id = "abc123-..."
//!
//! [[zones]]
//! name    = "augminted.cc"
//! zone_id = "..."
//!
//! [[zones]]
//! name    = "augmint.bot"
//! zone_id = "..."
//! ```
//!
//! The file is the source of truth for which zones this box is authorized
//! to publish into. An app's `public` list cannot reach a zone that isn't
//! present here — the agent rejects it at register time.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Top-level structure of `~/.config/shiku/server.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Tunnel registered for this box.
    pub tunnel: TunnelInfo,
    /// Zones this box is authorized to publish hostnames into.
    #[serde(default)]
    pub zones: Vec<ZoneInfo>,
}

/// One CF tunnel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelInfo {
    /// CF-assigned tunnel UUID.
    pub id: String,
}

/// One CF zone.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZoneInfo {
    /// Zone apex (e.g. `augminted.cc`). Used to suffix-match a hostname.
    pub name: String,
    /// CF zone ID. Cached at bootstrap; used by DNS upsert / delete calls.
    pub zone_id: String,
}

/// Default path: `~/.config/shiku/server.toml`.
pub fn default_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("shiku")
        .join("server.toml"))
}

/// Load the server config from disk, or return `None` if the file is
/// missing — boxes without a tunnel bootstrap are valid; they just can't
/// publish hostnames.
pub fn load() -> Result<Option<ServerConfig>> {
    let path = default_path()?;
    load_from(&path)
}

/// Load from an explicit path. Pulled out for tests.
pub fn load_from(path: &Path) -> Result<Option<ServerConfig>> {
    if !path.exists() {
        return Ok(None);
    }
    let body =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let parsed: ServerConfig =
        toml::from_str(&body).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(parsed))
}

impl ServerConfig {
    /// Find the zone that is the longest suffix-match of `hostname`.
    /// Returns `None` if no configured zone is a suffix of the hostname.
    ///
    /// Suffix-match is on whole-label boundaries: `norn.augmint.bot` matches
    /// zone `augmint.bot` but not `mint.bot`.
    pub fn match_zone(&self, hostname: &str) -> Option<&ZoneInfo> {
        let host_lower = hostname.to_ascii_lowercase();
        let mut best: Option<&ZoneInfo> = None;
        for zone in &self.zones {
            let zname = zone.name.to_ascii_lowercase();
            if hostname_matches_zone(&host_lower, &zname)
                && best.is_none_or(|b| b.name.len() < zone.name.len())
            {
                best = Some(zone);
            }
        }
        best
    }
}

/// Whether `hostname` falls under `zone`. True if hostname == zone, or if
/// hostname ends with `.<zone>` (label boundary).
fn hostname_matches_zone(hostname: &str, zone: &str) -> bool {
    if hostname == zone {
        return true;
    }
    if let Some(stripped) = hostname.strip_suffix(zone) {
        if let Some(prefix_dot) = stripped.strip_suffix('.') {
            return !prefix_dot.is_empty();
        }
    }
    false
}

/// Number of labels in `hostname` left of `zone`. Caller must ensure
/// `hostname_matches_zone(hostname, zone)`. Returns 0 when hostname == zone.
pub fn labels_left_of_zone(hostname: &str, zone: &str) -> usize {
    let host_lower = hostname.to_ascii_lowercase();
    let zone_lower = zone.to_ascii_lowercase();
    if host_lower == zone_lower {
        return 0;
    }
    let stripped = host_lower
        .strip_suffix(&zone_lower)
        .and_then(|s| s.strip_suffix('.'))
        .unwrap_or("");
    if stripped.is_empty() {
        0
    } else {
        stripped.split('.').count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> ServerConfig {
        ServerConfig {
            tunnel: TunnelInfo {
                id: "tunnel-uuid".into(),
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
    fn parses_from_toml() {
        let body = r#"
[tunnel]
id = "abc-123"

[[zones]]
name = "augminted.cc"
zone_id = "z1"

[[zones]]
name = "augmint.bot"
zone_id = "z2"
"#;
        let cfg: ServerConfig = toml::from_str(body).expect("parse");
        assert_eq!(cfg.tunnel.id, "abc-123");
        assert_eq!(cfg.zones.len(), 2);
        assert_eq!(cfg.zones[0].name, "augminted.cc");
    }

    #[test]
    fn match_zone_picks_correct_zone() {
        let cfg = fixture();
        assert_eq!(
            cfg.match_zone("norn.augminted.cc").map(|z| z.name.as_str()),
            Some("augminted.cc")
        );
        assert_eq!(
            cfg.match_zone("eternal-seas.augmint.bot")
                .map(|z| z.name.as_str()),
            Some("augmint.bot")
        );
    }

    #[test]
    fn match_zone_label_boundary() {
        let cfg = fixture();
        // `notmint.bot` ends in `mint.bot` textually but doesn't match any
        // configured zone on label boundary. (Sanity check that the match
        // is label-aware, not substring.)
        assert!(cfg.match_zone("foo.notaugmint.bot").is_none());
    }

    #[test]
    fn match_zone_no_match() {
        let cfg = fixture();
        assert!(cfg.match_zone("norn.example.com").is_none());
    }

    #[test]
    fn match_zone_apex_self() {
        let cfg = fixture();
        // The zone apex itself matches; `labels_left_of_zone` returns 0.
        assert!(cfg.match_zone("augminted.cc").is_some());
        assert_eq!(labels_left_of_zone("augminted.cc", "augminted.cc"), 0);
    }

    #[test]
    fn labels_left_of_zone_counts_labels() {
        assert_eq!(labels_left_of_zone("norn.augminted.cc", "augminted.cc"), 1);
        assert_eq!(
            labels_left_of_zone("api.norn.augminted.cc", "augminted.cc"),
            2
        );
        assert_eq!(labels_left_of_zone("a.b.c.augminted.cc", "augminted.cc"), 3);
    }

    #[test]
    fn labels_left_of_zone_case_insensitive() {
        assert_eq!(labels_left_of_zone("Norn.Augminted.CC", "augminted.cc"), 1);
    }

    #[test]
    fn load_from_missing_returns_none() {
        let cfg = load_from(Path::new("/nonexistent/server.toml")).expect("ok");
        assert!(cfg.is_none());
    }

    #[test]
    fn load_from_real_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("server.toml");
        std::fs::write(
            &path,
            r#"
[tunnel]
id = "abc"

[[zones]]
name = "augminted.cc"
zone_id = "z1"
"#,
        )
        .expect("write");
        let cfg = load_from(&path).expect("ok").expect("some");
        assert_eq!(cfg.tunnel.id, "abc");
        assert_eq!(cfg.zones.len(), 1);
    }
}
