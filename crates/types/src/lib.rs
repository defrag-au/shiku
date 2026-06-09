//! Shared wire-protocol and config types for the Shiku service platform.
//!
//! Both `shiku` (the CLI) and `shikud` (the agent) depend on this crate.
//! Keeping the wire types here forces both sides through a single source of
//! truth — the compiler enforces the contract once both sides import the same
//! `Request`/`Response` enums.
//!
//! See `docs/design/shiku/deploy.md` for the full design.
//!
//! ## Layering
//!
//! - [`Request`] / [`Response`] — request/response pairs over the agent's
//!   Unix socket. Both sides postcard-encode these with length-prefixed framing.
//! - [`ActivateEvent`] — streamed progress events emitted during a deploy.
//! - [`AppConfig`] / [`AppManifest`] / [`ReleaseManifest`] — config and
//!   metadata structs that travel inside requests and live as files on disk.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Wire-protocol version. Bumped on breaking changes.
///
/// The CLI sends its `PROTOCOL_VERSION` in the first message; the agent rejects
/// requests it doesn't understand. (Negotiation logic isn't implemented in the
/// stub — it's a Step 1.5 concern.)
pub const PROTOCOL_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

pub mod framing {
    //! Wire framing for Shiku protocol messages.
    //!
    //! Each message is sent as a length-prefixed postcard payload:
    //!
    //! ```text
    //!   ┌────────────┬──────────────────────────────┐
    //!   │ u32 LE len │ postcard-encoded body (len B)│
    //!   └────────────┴──────────────────────────────┘
    //! ```
    //!
    //! The framing functions are I/O-agnostic: callers read the length and
    //! body bytes themselves (sync or async, blocking or not) and pass the
    //! body to [`decode`]. This keeps `shiku-types` free of tokio/std-io deps
    //! while still owning the wire contract.
    //!
    //! `MAX_FRAME_SIZE` bounds bodies at 16 MiB to prevent a malformed length
    //! prefix from triggering a giant allocation.
    use serde::{de::DeserializeOwned, Serialize};
    use thiserror::Error;

    /// Hard cap on a single frame's body. 16 MiB is well above any realistic
    /// Shiku message (release uploads use rsync, not the protocol stream).
    pub const MAX_FRAME_SIZE: u32 = 16 * 1024 * 1024;

    /// Number of bytes in the length prefix. Always 4 (u32 LE).
    pub const LEN_PREFIX_BYTES: usize = 4;

    #[derive(Debug, Error)]
    pub enum FramingError {
        #[error("postcard: {0}")]
        Postcard(#[from] postcard::Error),
        #[error("frame body too large: {0} bytes (max 16777216)")]
        TooLarge(u64),
    }

    /// Encode a message as a length-prefixed postcard frame.
    pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, FramingError> {
        let body = postcard::to_allocvec(msg)?;
        let len = u64::try_from(body.len()).expect("usize fits in u64");
        if len > u64::from(MAX_FRAME_SIZE) {
            return Err(FramingError::TooLarge(len));
        }
        let len_u32 = len as u32;
        let mut out = Vec::with_capacity(LEN_PREFIX_BYTES + body.len());
        out.extend_from_slice(&len_u32.to_le_bytes());
        out.extend_from_slice(&body);
        Ok(out)
    }

    /// Decode a frame body (the bytes after the length prefix).
    pub fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T, FramingError> {
        Ok(postcard::from_bytes(body)?)
    }

    /// Parse a length prefix. Returns the body length, or [`FramingError::TooLarge`]
    /// if the prefix declares a body larger than [`MAX_FRAME_SIZE`].
    pub fn parse_len_prefix(prefix: [u8; 4]) -> Result<u32, FramingError> {
        let len = u32::from_le_bytes(prefix);
        if len > MAX_FRAME_SIZE {
            return Err(FramingError::TooLarge(u64::from(len)));
        }
        Ok(len)
    }
}

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// An app's user-facing name, e.g. `"eternal-seas"` or `"norn-server"`.
///
/// Strings rather than typed newtypes for now — names cross the wire as plain
/// text and we don't want to invent identifier rules before we have evidence
/// the rules need inventing.
pub type AppName = String;

/// A release's content-addressed identifier. Hex-encoded SHA-256 of the binary
/// or a derived value (e.g. `git_sha + content_hash`).
pub type ReleaseSha = String;

/// A secret's name (e.g. `"BOT_TOKEN"`, `"LLM_API_KEY"`). Conventionally
/// SCREAMING_SNAKE_CASE because these become environment variable names.
pub type SecretName = String;

// ---------------------------------------------------------------------------
// Request / Response
// ---------------------------------------------------------------------------

/// Messages sent from the CLI to the agent.
///
/// One request elicits zero-or-more streaming events followed by exactly one
/// terminal response. Streaming requests (`Logs`, `Activate`) emit events
/// during processing; non-streaming requests reply once.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    // ---- Health ----
    /// Liveness probe; agent replies with [`Response::Pong`].
    Ping,

    // ---- Apps ----
    /// Register a new app on the agent.
    AppRegister { name: AppName, config: AppConfig },
    /// Remove an app from the registry. Releases on disk are also pruned.
    AppRemove { name: AppName },
    /// List registered app names.
    AppList,
    /// Fetch the full config for one app.
    AppInspect { name: AppName },

    // ---- Releases ----
    /// Notify the agent that a release has been uploaded (via rsync sidechannel).
    /// The agent verifies the binary matches the manifest and stages it.
    ReleaseUpload {
        app: AppName,
        sha: ReleaseSha,
        manifest: ReleaseManifest,
    },
    /// List existing releases for an app.
    ReleaseList { app: AppName },
    /// Prune old releases, keeping only the latest N (current and previous
    /// are always retained regardless of N).
    ReleasePrune { app: AppName, keep_latest: usize },

    // ---- Activation ----
    /// Atomically activate a release: symlink swap, restart, health-check,
    /// roll back on failure. Streams [`ActivateEvent`]s as it progresses.
    Activate { app: AppName, sha: ReleaseSha },
    /// Restore the previous release's symlink and restart.
    Rollback { app: AppName },
    /// Restart the running release without changing the active version.
    Restart { app: AppName },
    /// Status report for one or all apps.
    Status { app: Option<AppName> },

    // ---- Logs ----
    /// Stream `journalctl --user -u <unit>` lines back to the caller.
    /// Streams [`Response::LogLine`] events until the connection closes.
    Logs {
        app: AppName,
        /// `journalctl --since` value, e.g. `"5 minutes ago"`.
        since: Option<String>,
        /// If true, stream new lines as they appear (`-f`).
        follow: bool,
    },

    // ---- Secrets ----
    /// Set or rotate a secret. The plaintext travels over SSH (already
    /// encrypted in transit); the agent encrypts to the user's master key
    /// and stores at `~/secrets/<name>.age`.
    SecretSet { name: SecretName, value: String },
    /// List secret names. Values are never returned.
    SecretList,
    /// Remove a secret.
    SecretRemove { name: SecretName },

    // ---- Tunnel inspection ----
    /// Report the current tunnel state: tunnel UUID, cloudflared
    /// systemd unit state, and the in-effect ingress rules. Used by
    /// `shiku tunnel status`.
    TunnelStatus,
    /// List the zones this box is authorized to publish into. Used by
    /// `shiku tunnel zones`.
    TunnelZones,
    /// Download Shiku's pinned cloudflared version, SHA-verify, and
    /// swap the binary. Stops + restarts the cloudflared unit around
    /// the swap so the new binary actually runs. Idempotent: if the
    /// installed binary already matches the pin, it's a no-op.
    TunnelUpgrade,

    // ---- Tunnel ----
    /// Bootstrap the box's cloudflared tunnel. The [`TunnelBootstrapMode`]
    /// enum picks between operator-supplied tunnel credentials (manual)
    /// and agent-side creation via the CF API (auto-create).
    ///
    /// In both modes the agent stores the resolved tunnel id + token as
    /// `__shiku__`-namespaced secrets, registers the pseudo-app, writes
    /// `~/.config/shiku/server.toml`, installs the cloudflared
    /// systemd-user unit, and starts it.
    ///
    /// Idempotent in both modes. See `docs/design/shiku/tunnel.md` §5.
    TunnelBootstrap {
        /// How to obtain the tunnel id + runtime token.
        mode: TunnelBootstrapMode,
        /// CF API token. `Zone.DNS:Edit` on every zone in `zones` is
        /// required; auto-create mode also requires
        /// `Account.Cloudflare Tunnel:Edit`.
        api_token: String,
        /// Zone apex names to authorize for this box. Each must be
        /// accessible to the API token; the agent looks up zone IDs.
        zones: Vec<String>,
    },

    // ---- Misc ----
    /// Ask the agent to shut down cleanly. Used for in-place upgrades.
    Shutdown,
}

/// Messages sent from the agent to the CLI.
///
/// Most requests have a single terminal `Response`. Streaming requests
/// (`Logs`, `Activate`) emit zero-or-more events (`LogLine`, `ActivateProgress`)
/// before the final `Ok` / `Error`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    // ---- Terminal ----
    /// Generic success. Used when the request has no payload to return.
    Ok,
    /// Liveness reply.
    Pong,
    /// Failure. Carries a typed error kind plus a human-readable detail string.
    Error { kind: ErrorKind, detail: String },

    // ---- Data ----
    /// Per-app status snapshot(s).
    Status(Vec<AppStatus>),
    /// Registered app names.
    Apps(Vec<AppName>),
    /// Full config for one app. Boxed because `AppConfig` is larger than the
    /// other variants and would otherwise bloat every `Response` allocation.
    AppDetail(Box<AppConfig>),
    /// Releases on disk for one app.
    Releases(Vec<ReleaseInfo>),
    /// Names of the secrets currently stored.
    Secrets(Vec<SecretName>),

    // ---- Streaming ----
    /// One log line streamed from `journalctl`.
    LogLine(String),
    /// Progress event during an `Activate` flow.
    ActivateProgress(ActivateEvent),

    // ---- Tunnel inspection ----
    /// Reply to [`Request::TunnelStatus`].
    TunnelStatus(TunnelStatusReport),
    /// Reply to [`Request::TunnelZones`]. Each entry is `(zone_name,
    /// zone_id)` — zone IDs are useful for debugging CF API issues.
    TunnelZones(Vec<TunnelZoneInfo>),
    /// Reply to [`Request::TunnelUpgrade`]. Reports the version that
    /// is now installed and whether it changed.
    TunnelUpgrade {
        /// `cloudflared --version`-style string (e.g. `"2026.3.0"`).
        version: String,
        /// Whether this call actually replaced the binary on disk.
        changed: bool,
    },
}

/// Snapshot of the box's tunnel state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelStatusReport {
    /// Tunnel UUID from `~/.config/shiku/server.toml`. `None` if the
    /// box hasn't been tunnel-bootstrapped yet.
    pub tunnel_id: Option<String>,
    /// Names of zones authorized for this box.
    pub zones: Vec<String>,
    /// systemd unit state for `cloudflared.service`, e.g.
    /// `"active (running)"`, `"failed"`, `"inactive"`.
    pub cloudflared_state: String,
    /// Hostnames currently published, with the app and port each maps to.
    pub ingress: Vec<TunnelIngressRule>,
}

/// One row in [`TunnelStatusReport::ingress`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelIngressRule {
    pub hostname: String,
    pub app: AppName,
    pub port: u16,
}

/// One zone in [`Response::TunnelZones`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelZoneInfo {
    pub name: String,
    pub zone_id: String,
}

/// How `Request::TunnelBootstrap` obtains the tunnel id + runtime token.
///
/// Each mode is a distinct variant rather than a bag of optionals so the
/// CLI and agent dispatch sites match on a clear case. Postcard-friendly
/// (externally tagged enum).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TunnelBootstrapMode {
    /// Operator created the tunnel in the CF dashboard. Supplies the
    /// UUID and the runtime token directly.
    Manual {
        /// Tunnel UUID.
        tunnel_id: String,
        /// Runtime token (`tunnel run --token`).
        tunnel_token: String,
    },
    /// Agent calls `POST /accounts/<id>/cfd_tunnel` to create a new
    /// tunnel (or reuse an existing one with the same name) and
    /// reconstructs the runtime token from the response. Requires
    /// `Account.Cloudflare Tunnel:Edit` on the API token.
    AutoCreate {
        /// Name to give the tunnel. Reused on re-bootstrap if a tunnel
        /// of this name already exists on the account.
        tunnel_name: String,
    },
}

/// Categorised error kinds. The CLI maps these to exit codes and user-facing
/// messages; matching on the kind (rather than the detail string) keeps the
/// error handling robust to message-text changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorKind {
    AppNotFound,
    AppAlreadyRegistered,
    SecretNotInAllowlist,
    SecretNotFound,
    ReleaseNotFound,
    HealthCheckTimeout,
    DiskFull,
    SystemdError,
    DecryptionFailed,
    BadRequest,
    Unsupported,
    Internal,
    // ---- Tunnel / public ingress ----
    /// A hostname in `public` is not under any zone configured for this
    /// box's tunnel (per `~/.config/shiku/server.toml`).
    ZoneNotManaged,
    /// A hostname in `public` has more than one label left of the matched
    /// zone. CF tunnel default certs only cover one level.
    MultiLevelSubdomainUnsupported,
    /// A hostname in `public` is already claimed by a different registered
    /// app on this box.
    HostnameAlreadyClaimed,
    /// CF DNS API call failed (network, auth, rate-limit, etc).
    DnsApiFailed,
    /// Generating or reloading the cloudflared ingress config failed.
    CloudflaredReloadFailed,
}

// ---------------------------------------------------------------------------
// Streaming events (Activate)
// ---------------------------------------------------------------------------

/// Progress events emitted during an [`Request::Activate`] flow. The CLI
/// renders these to the user so they see the deploy progressing in real time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ActivateEvent {
    /// Binary upload has been verified against the manifest.
    UploadComplete,
    /// Allowlisted secrets decrypted and prepared for injection.
    SecretsResolved,
    /// systemd unit file written to `~/.config/systemd/user/<app>.service`.
    UnitWritten,
    /// `systemctl --user restart <app>` issued.
    ProcessStarting,
    /// One health-check attempt completed.
    HealthCheckAttempt {
        attempt: u32,
        ok: bool,
        /// Optional detail (e.g. HTTP status, error message).
        detail: Option<String>,
    },
    /// Health check passed; deploy is committed.
    HealthCheckPassed,
    /// Health check failed; rollback is starting.
    HealthCheckFailed,
    /// Deploy committed successfully.
    Activated { sha: ReleaseSha },
    /// Rollback completed after a failed activation.
    RolledBack { to_sha: ReleaseSha, reason: String },
}

// ---------------------------------------------------------------------------
// AppConfig
// ---------------------------------------------------------------------------

/// Per-app configuration. Stored on disk at `~/apps/<app>/config.toml`,
/// also travels in [`Request::AppRegister`] when registering a new app.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Display name (matches the registry key but kept here for convenience).
    pub name: AppName,
    /// Binary name in `releases/<sha>/bin/`.
    pub command: String,
    /// systemd unit name (typically the same as `name`).
    pub service: String,
    /// Working directory passed to systemd. Defaults to `~/data/<app>` when
    /// `None`.
    pub working_dir: Option<String>,
    /// Names of secrets the app is allowed to receive in its environment.
    /// Defense in depth — the agent only injects these.
    pub secrets: Vec<SecretName>,
    /// Names of services this app depends on. Resolved to `SHIKU_BIND_<UPPER>`
    /// env vars at activation time.
    pub bindings: Vec<AppName>,
    /// Plain-text environment variables (not secret, not binding-derived).
    /// E.g. `RUST_LOG=info`, `BOT_ENV=production`. `BTreeMap` rather than
    /// `Vec` because env var names are unique and alphabetic-stable ordering
    /// makes TOML diffs readable.
    pub vars: BTreeMap<String, String>,
    /// What the app wants from the listen layer. Either it doesn't need a
    /// port (`None`, default) or it wants HTTP — in which case the agent
    /// allocates a port from the dynamic range and pins it here. Users
    /// don't pick numbers.
    pub listen: Option<ListenSpec>,
    /// How the agent verifies the activation succeeded.
    pub health: HealthSpec,
    /// systemd unit overrides. Sensible defaults applied when omitted.
    pub systemd: SystemdSpec,
    /// Fully-qualified hostnames this app should be reachable at. Drives
    /// the cloudflared tunnel ingress / DNS reconcile loop on the agent.
    /// Empty when the app isn't published. See `docs/design/shiku/tunnel.md`.
    ///
    /// Defaulted (`#[serde(default)]`) so existing on-disk configs without
    /// the field continue to deserialize cleanly.
    #[serde(default)]
    pub public: Vec<String>,
}

/// Listen configuration. The user-facing knob in `shiku.toml` is intent
/// (`http = true` means "this app wants an HTTP port"); the agent owns the
/// resolved port number and stores it back into the on-disk config.toml
/// after first registration.
///
/// Manual port selection is intentionally not supported — Shiku owns
/// allocation across all apps for a service-user, picks from the IANA
/// dynamic range (49152-65535), and avoids collisions automatically. The
/// resolved port surfaces via `shiku env <app>` and via the agent's
/// binding system (`SHIKU_BIND_<NAME>`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ListenSpec {
    /// Whether this app wants an HTTP port allocated.
    #[serde(default)]
    pub http: bool,
    /// Agent-allocated HTTP port. Populated on registration; `None` until
    /// then. Users should not set this manually — it's persisted by the
    /// agent so re-registrations preserve the existing assignment.
    ///
    /// Note: no `skip_serializing_if` — postcard needs a consistent
    /// field set on the wire (it's a non-self-describing binary format).
    #[serde(default)]
    pub http_port: Option<u16>,
}

/// How the agent decides a deploy is healthy. `Process` is the default for
/// non-HTTP services (Discord bots, queue workers, etc.); `Http` for services
/// that bind a port.
///
/// Externally-tagged on the wire (postcard doesn't support internally-tagged
/// enums). TOML form looks like:
///
/// ```toml
/// [apps.X.health.process]
/// settle_secs = 10
///
/// # or
/// [apps.X.health.http]
/// path = "/api/health"
/// timeout_secs = 30
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthSpec {
    /// Consider activated if the systemd unit reports `active (running)`
    /// after `settle_secs` without entering a restart loop.
    Process {
        #[serde(default = "default_settle_secs")]
        settle_secs: u32,
    },
    /// Poll an HTTP endpoint on `localhost:<listen.http>`.
    Http {
        path: String,
        #[serde(default = "default_initial_delay_secs")]
        initial_delay_secs: u32,
        #[serde(default = "default_health_timeout_secs")]
        timeout_secs: u32,
        #[serde(default = "default_health_interval_secs")]
        interval_secs: u32,
        #[serde(default = "default_health_expect_status")]
        expect_status: Vec<u16>,
    },
}

fn default_settle_secs() -> u32 {
    10
}
fn default_initial_delay_secs() -> u32 {
    2
}
fn default_health_timeout_secs() -> u32 {
    30
}
fn default_health_interval_secs() -> u32 {
    1
}
fn default_health_expect_status() -> Vec<u16> {
    vec![200, 204]
}

/// systemd unit overrides. The agent generates units with hardening defaults
/// (NoNewPrivileges, PrivateTmp, ProtectSystem=strict, etc.); these fields
/// override the per-app knobs that vary in practice.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SystemdSpec {
    /// E.g. `"512M"`, `"2G"`. Maps to systemd's `MemoryMax=`.
    pub memory_max: Option<String>,
    /// Percentage as integer, e.g. `100` for 100% of one core. Maps to `CPUQuota=`.
    pub cpu_quota_percent: Option<u32>,
    /// systemd `Restart=` value (default `"always"`).
    pub restart: Option<String>,
    /// systemd `RestartSec=` (default `10`).
    pub restart_sec: Option<u32>,
}

// ---------------------------------------------------------------------------
// Release metadata
// ---------------------------------------------------------------------------

/// Manifest written alongside an uploaded binary at
/// `~/apps/<app>/releases/<sha>/manifest.toml`. Read at activation time to
/// verify secret hashes and bind the release to a known build state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseManifest {
    /// Content-addressed sha (matches the parent directory name).
    pub sha: ReleaseSha,
    /// Cargo package name.
    pub package: String,
    /// Target triple (e.g. `aarch64-unknown-linux-musl`).
    pub target: String,
    /// RFC3339 build timestamp.
    pub built_at: String,
    /// Optional builder identifier (e.g. `"damo@laptop"`).
    pub built_by: Option<String>,
    /// Git SHA at build time.
    pub git_sha: Option<String>,
    /// Whether the working tree was dirty at build time.
    pub git_dirty: bool,
    /// Hashes of allowlisted secrets at deploy-prep time. The agent rejects
    /// activation if any of these have changed since the release was prepared.
    pub secrets: Vec<SecretBinding>,
}

/// A secret reference within a [`ReleaseManifest`]. The agent compares
/// `hash` to the current secret's hash at activation time; mismatch means
/// the release was prepared against a different secret value than what's
/// currently stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretBinding {
    pub name: SecretName,
    /// Hex-encoded SHA-256 of the secret value at deploy-prep time.
    pub hash: String,
}

/// Read-only view of a release on disk. Returned by [`Request::ReleaseList`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseInfo {
    pub sha: ReleaseSha,
    pub built_at: String,
    pub is_current: bool,
    pub is_previous: bool,
    /// Bytes on disk for the release directory.
    pub size_bytes: u64,
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// Snapshot of an app's runtime state. Returned by [`Request::Status`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppStatus {
    pub name: AppName,
    pub registered: bool,
    pub current_sha: Option<ReleaseSha>,
    pub previous_sha: Option<ReleaseSha>,
    /// systemd unit state, e.g. `"active (running)"`, `"failed"`, `"inactive"`.
    pub systemd_state: String,
    /// How long the unit has been in its current state, in seconds.
    pub uptime_secs: Option<u64>,
    /// Last activation timestamp (RFC3339), if known.
    pub last_activated_at: Option<String>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Roundtrip check: every variant we expect to send over the wire should
    /// encode and decode losslessly via postcard. Worth keeping for the
    /// representative cases — drift in serde-attribute coverage often shows
    /// up here first.
    #[test]
    fn request_roundtrip() {
        let cases = vec![
            Request::Ping,
            Request::AppList,
            Request::AppRemove {
                name: "eternal-seas".to_string(),
            },
            Request::Activate {
                app: "eternal-seas".to_string(),
                sha: "abc123".to_string(),
            },
            Request::Logs {
                app: "eternal-seas".to_string(),
                since: Some("1 hour ago".to_string()),
                follow: true,
            },
            Request::SecretSet {
                name: "BOT_TOKEN".to_string(),
                value: "xyz".to_string(),
            },
        ];

        for case in cases {
            let buf: Vec<u8> = postcard::to_allocvec(&case).expect("encode");
            let _: Request = postcard::from_bytes(&buf).expect("decode");
        }
    }

    #[test]
    fn response_roundtrip() {
        let cases = vec![
            Response::Ok,
            Response::Pong,
            Response::Apps(vec!["a".into(), "b".into()]),
            Response::Error {
                kind: ErrorKind::AppNotFound,
                detail: "no such app".into(),
            },
            Response::ActivateProgress(ActivateEvent::HealthCheckAttempt {
                attempt: 1,
                ok: false,
                detail: Some("connection refused".into()),
            }),
        ];

        for case in cases {
            let buf: Vec<u8> = postcard::to_allocvec(&case).expect("encode");
            let _: Response = postcard::from_bytes(&buf).expect("decode");
        }
    }

    #[test]
    fn app_config_roundtrip() {
        let cfg = AppConfig {
            name: "eternal-seas".into(),
            command: "eternal-seas".into(),
            service: "eternal-seas".into(),
            working_dir: None,
            secrets: vec!["BOT_TOKEN".into(), "JWT_SECRET".into()],
            bindings: vec![],
            vars: BTreeMap::from([
                ("RUST_LOG".into(), "info".into()),
                ("BOT_ENV".into(), "production".into()),
            ]),
            listen: None,
            health: HealthSpec::Process { settle_secs: 10 },
            systemd: SystemdSpec {
                memory_max: Some("512M".into()),
                cpu_quota_percent: Some(100),
                restart: None,
                restart_sec: None,
            },
            public: vec!["norn.augminted.cc".into()],
        };

        let buf: Vec<u8> = postcard::to_allocvec(&cfg).expect("encode");
        let decoded: AppConfig = postcard::from_bytes(&buf).expect("decode");
        assert_eq!(decoded.name, cfg.name);
        assert_eq!(decoded.secrets.len(), 2);
        assert!(decoded.listen.is_none());
        assert!(matches!(
            decoded.health,
            HealthSpec::Process { settle_secs: 10 }
        ));
        assert_eq!(decoded.public, vec!["norn.augminted.cc".to_string()]);
    }

    #[test]
    fn release_manifest_roundtrip() {
        let manifest = ReleaseManifest {
            sha: "abc123".into(),
            package: "eternal-seas".into(),
            target: "aarch64-unknown-linux-musl".into(),
            built_at: "2026-04-29T12:00:00Z".into(),
            built_by: Some("damo@laptop".into()),
            git_sha: Some("0192a4b3".into()),
            git_dirty: false,
            secrets: vec![SecretBinding {
                name: "BOT_TOKEN".into(),
                hash: "sha256:deadbeef".into(),
            }],
        };

        let buf: Vec<u8> = postcard::to_allocvec(&manifest).expect("encode");
        let decoded: ReleaseManifest = postcard::from_bytes(&buf).expect("decode");
        assert_eq!(decoded.sha, "abc123");
        assert_eq!(decoded.secrets.len(), 1);
        assert_eq!(decoded.secrets[0].name, "BOT_TOKEN");
    }

    #[test]
    fn protocol_version_is_present() {
        assert_eq!(PROTOCOL_VERSION, 1);
    }

    #[test]
    fn framing_roundtrip() {
        let req = Request::SecretSet {
            name: "BOT_TOKEN".into(),
            value: "secret-value".into(),
        };
        let frame = framing::encode(&req).expect("encode");
        // First 4 bytes are the LE length prefix.
        assert!(frame.len() >= framing::LEN_PREFIX_BYTES);
        let prefix: [u8; 4] = frame[..4].try_into().unwrap();
        let body_len = framing::parse_len_prefix(prefix).expect("parse len");
        assert_eq!(body_len as usize, frame.len() - 4);
        let body = &frame[4..];
        let decoded: Request = framing::decode(body).expect("decode");
        match decoded {
            Request::SecretSet { name, value } => {
                assert_eq!(name, "BOT_TOKEN");
                assert_eq!(value, "secret-value");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn framing_rejects_oversized_prefix() {
        let prefix = (framing::MAX_FRAME_SIZE + 1).to_le_bytes();
        let err = framing::parse_len_prefix(prefix);
        assert!(matches!(err, Err(framing::FramingError::TooLarge(_))));
    }
}
