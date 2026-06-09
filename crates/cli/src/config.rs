//! Shiku project config (`shiku.toml`).
//!
//! Lives in the project root. Each project that wants to deploy something
//! has its own `shiku.toml` describing its apps and per-environment settings.
//!
//! See `docs/deploying.md` for the schema design rationale.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use shiku_types::{AppConfig, AppName, HealthSpec, ListenSpec, SecretName, SystemdSpec};

/// Default config-file name. Resolved relative to the current working
/// directory unless `--config <path>` is passed.
pub const DEFAULT_FILENAME: &str = "shiku.toml";

/// Top-level config. The `[shiku]` table holds CLI-wide defaults; `[apps.*]`
/// describes the deployable units.
#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub shiku: ShikuSection,
    #[serde(default)]
    pub apps: BTreeMap<AppName, AppSection>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ShikuSection {
    /// Default `--env` selector when none is passed on the command line.
    pub default_env: Option<String>,
}

/// One app's configuration. The fields here apply across all environments;
/// per-environment overrides live in `env.<name>`.
#[derive(Debug, Deserialize)]
pub struct AppSection {
    /// Cargo package name to build (e.g. `"eternal-seas"`).
    pub package: String,
    /// Target triple for `cargo zigbuild`. Defaults to aarch64-musl since
    /// that's our only deploy target right now; bake explicit when others
    /// appear.
    #[serde(default = "default_target")]
    pub target: String,
    /// Build command. Defaults to `"cargo zigbuild"`.
    #[serde(default = "default_build")]
    pub build: String,
    /// **Deprecated**: prefer declaring platform needs from code via
    /// `shiku::secret!()` etc. Kept here only for cases where the binary
    /// can't be introspected (third-party binaries). Will likely be removed
    /// once we have no remaining users.
    #[serde(default)]
    #[allow(dead_code)]
    pub secrets: Vec<SecretName>,
    /// Same as above — declare via `shiku::binding!()` from code instead.
    #[serde(default)]
    #[allow(dead_code)]
    pub bindings: Vec<AppName>,
    /// Same as above — declare via `shiku::listen_http!()` from code instead.
    #[serde(default)]
    #[allow(dead_code)]
    pub listen: Option<ListenSpec>,
    /// Per-environment configuration. Must contain at least one entry.
    pub env: BTreeMap<String, EnvSection>,
}

fn default_target() -> String {
    "aarch64-unknown-linux-musl".to_string()
}

fn default_build() -> String {
    "cargo zigbuild".to_string()
}

/// Per-environment overrides and connection details.
#[derive(Debug, Deserialize)]
pub struct EnvSection {
    /// SSH target — where the agent for this environment lives.
    pub deploy_host: String,
    /// Service user on the box that owns this app's deployment.
    pub ssh_user: String,
    /// systemd unit name. Defaults to the app name when omitted.
    pub service: Option<String>,
    /// Fully-qualified hostnames this app should be reachable at via the
    /// box's cloudflared tunnel. Validated by the agent at register time;
    /// see `docs/ingress.md` for the rules (zone must be in
    /// the box's `server.toml`; only single-level subdomains; not
    /// claimed by another app).
    #[serde(default)]
    pub public: Vec<String>,
    /// Plain-text environment variables to inject (not secret, not binding-derived).
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
    /// Health-check configuration. Defaults to `Process { settle_secs: 10 }`
    /// for non-listen apps; required for listen apps (caller validates).
    pub health: Option<HealthSpec>,
    /// systemd unit overrides for this environment.
    #[serde(default)]
    pub systemd: SystemdSpec,
}

/// A fully-resolved per-app, per-env view. The CLI's command implementations
/// receive one of these and don't need to think about env fallback or
/// defaulting — that's all done in [`Config::resolve`].
#[derive(Debug, Clone)]
pub struct ResolvedApp {
    pub app_name: AppName,
    pub env_name: String,
    pub package: String,
    pub target: String,
    pub build: String,
    pub deploy_host: String,
    pub ssh_user: String,
    pub service: String,
    pub secrets: Vec<SecretName>,
    pub bindings: Vec<AppName>,
    pub vars: BTreeMap<String, String>,
    /// Fully-qualified hostnames declared in the env's `public` list. Sent
    /// to the agent inside `AppConfig.public` and validated there.
    pub public: Vec<String>,
    pub listen: Option<ListenSpec>,
    pub health: HealthSpec,
    pub systemd: SystemdSpec,
}

impl Config {
    /// Locate and parse `shiku.toml`. If `path` is provided, it's used directly;
    /// otherwise we walk up from the current dir looking for `shiku.toml`.
    pub fn load(path: Option<&Path>) -> Result<(PathBuf, Self)> {
        let path = match path {
            Some(p) => p.to_path_buf(),
            None => find_upwards(DEFAULT_FILENAME).ok_or_else(|| {
                anyhow!(
                    "no shiku.toml found in current directory or any parent. \
                     Run from inside an app directory (e.g. native/eternal-seas), \
                     or pass --config <path>."
                )
            })?,
        };

        let body = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let config: Config =
            toml::from_str(&body).with_context(|| format!("parsing {}", path.display()))?;

        Ok((path, config))
    }

    /// Resolve an `(app, env)` pair into a flat [`ResolvedApp`].
    ///
    /// `app_name` may be `None` when there's exactly one app defined, in
    /// which case it's inferred. `env_name` falls back to `shiku.default_env`.
    pub fn resolve(&self, app_name: Option<&str>, env_name: Option<&str>) -> Result<ResolvedApp> {
        let app_name = match app_name {
            Some(n) => n.to_string(),
            None => {
                let names: Vec<_> = self.apps.keys().collect();
                match names.as_slice() {
                    [only] => (*only).clone(),
                    [] => bail!("no apps defined in shiku.toml"),
                    _ => bail!(
                        "multiple apps defined ({}); pass <app> explicitly",
                        names
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                }
            }
        };

        let app = self
            .apps
            .get(&app_name)
            .ok_or_else(|| anyhow!("app '{app_name}' not in shiku.toml"))?;

        let env_name = match env_name {
            Some(n) => n.to_string(),
            None => self
                .shiku
                .default_env
                .clone()
                .ok_or_else(|| anyhow!("no --env passed and no shiku.default_env set"))?,
        };

        let env = app.env.get(&env_name).ok_or_else(|| {
            anyhow!(
                "env '{env_name}' not defined for app '{app_name}'. Available: {}",
                app.env
                    .keys()
                    .map(|k| k.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;

        let service = env.service.clone().unwrap_or_else(|| app_name.clone());

        // Default health spec: HTTP if a listen port is declared, otherwise
        // process-mode (e.g. Discord bots).
        let health = env.health.clone().unwrap_or_else(|| match &app.listen {
            Some(_) => HealthSpec::Http {
                path: "/api/health".to_string(),
                initial_delay_secs: 2,
                timeout_secs: 30,
                interval_secs: 1,
                expect_status: vec![200, 204],
            },
            None => HealthSpec::Process { settle_secs: 10 },
        });

        Ok(ResolvedApp {
            app_name,
            env_name,
            package: app.package.clone(),
            target: app.target.clone(),
            build: app.build.clone(),
            deploy_host: env.deploy_host.clone(),
            ssh_user: env.ssh_user.clone(),
            service,
            secrets: app.secrets.clone(),
            bindings: app.bindings.clone(),
            vars: env.vars.clone(),
            public: env.public.clone(),
            listen: app.listen.clone(),
            health,
            systemd: env.systemd.clone(),
        })
    }
}

impl ResolvedApp {
    /// Build a minimal `ResolvedApp` carrying only the SSH connection
    /// fields. Used by per-box commands (`tunnel bootstrap`, etc.) that
    /// don't need a project's `shiku.toml` — they just need `(host, user)`
    /// to reach the agent. App-shaped fields are filled with safe
    /// placeholder values; nothing other than the SSH layer reads them.
    pub fn for_ssh(deploy_host: String, ssh_user: String) -> Self {
        Self {
            app_name: "__ssh__".to_string(),
            env_name: "__ssh__".to_string(),
            package: String::new(),
            target: String::new(),
            build: String::new(),
            deploy_host,
            ssh_user,
            service: String::new(),
            secrets: Vec::new(),
            bindings: Vec::new(),
            vars: BTreeMap::new(),
            public: Vec::new(),
            listen: None,
            health: HealthSpec::Process { settle_secs: 10 },
            systemd: SystemdSpec::default(),
        }
    }

    /// Convert into the wire `AppConfig` the agent persists. The CLI's
    /// `ResolvedApp` carries env-specific connection details that the agent
    /// doesn't need (host, ssh_user, public URLs); those are dropped here.
    pub fn to_app_config(&self) -> AppConfig {
        AppConfig {
            name: self.app_name.clone(),
            command: self.package.clone(),
            service: self.service.clone(),
            working_dir: None,
            secrets: self.secrets.clone(),
            bindings: self.bindings.clone(),
            vars: self.vars.clone(),
            listen: self.listen.clone(),
            health: self.health.clone(),
            systemd: self.systemd.clone(),
            public: self.public.clone(),
        }
    }
}

/// Walk up from the current directory looking for a file named `name`.
fn find_upwards(name: &str) -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_str(s: &str) -> Result<Config> {
        Ok(toml::from_str(s)?)
    }

    #[test]
    fn parses_minimal() {
        let toml = r#"
[shiku]
default_env = "prod"

[apps.eternal-seas]
package = "eternal-seas"
secrets = ["BOT_TOKEN", "JWT_SECRET"]

[apps.eternal-seas.env.prod]
deploy_host = "v2202604354294454929.supersrv.de"
ssh_user = "eternal-seas"

[apps.eternal-seas.env.prod.vars]
RUST_LOG = "info"
BOT_ENV = "production"
"#;
        let cfg = load_str(toml).expect("parse");
        assert_eq!(cfg.shiku.default_env.as_deref(), Some("prod"));
        let resolved = cfg.resolve(None, None).expect("resolve default");
        assert_eq!(resolved.app_name, "eternal-seas");
        assert_eq!(resolved.env_name, "prod");
        assert_eq!(resolved.target, "aarch64-unknown-linux-musl"); // default
        assert_eq!(resolved.build, "cargo zigbuild"); // default
        assert_eq!(resolved.service, "eternal-seas"); // defaulted from app name
        assert_eq!(resolved.secrets, vec!["BOT_TOKEN", "JWT_SECRET"]);
        assert_eq!(
            resolved.vars.get("RUST_LOG").map(|s| s.as_str()),
            Some("info")
        );
        assert!(matches!(resolved.health, HealthSpec::Process { .. }));
    }

    #[test]
    fn http_health_default_for_listen_apps() {
        let toml = r#"
[apps.norn-server]
package = "norn-server"

[apps.norn-server.listen]
http = true

[apps.norn-server.env.prod]
deploy_host = "unify.space"
ssh_user = "norn"
"#;
        let cfg = load_str(toml).expect("parse");
        let resolved = cfg
            .resolve(Some("norn-server"), Some("prod"))
            .expect("resolve");
        assert!(matches!(resolved.health, HealthSpec::Http { .. }));
    }

    #[test]
    fn ambiguous_app_requires_explicit() {
        let toml = r#"
[apps.a]
package = "a"
[apps.a.env.prod]
deploy_host = "x"
ssh_user = "y"

[apps.b]
package = "b"
[apps.b.env.prod]
deploy_host = "x"
ssh_user = "y"
"#;
        let cfg = load_str(toml).expect("parse");
        let err = cfg.resolve(None, Some("prod")).unwrap_err();
        assert!(err.to_string().contains("multiple apps"));
    }

    #[test]
    fn missing_env_lists_available() {
        let toml = r#"
[apps.a]
package = "a"
[apps.a.env.prod]
deploy_host = "x"
ssh_user = "y"
[apps.a.env.staging]
deploy_host = "x"
ssh_user = "y"
"#;
        let cfg = load_str(toml).expect("parse");
        let err = cfg.resolve(Some("a"), Some("dev")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("env 'dev'"));
        assert!(msg.contains("prod") || msg.contains("staging"));
    }

    #[test]
    fn public_list_survives_roundtrip() {
        let toml = r#"
[apps.norn-server]
package = "norn-server"

[apps.norn-server.env.prod]
deploy_host = "box.example.com"
ssh_user    = "norn"
public      = ["norn.augminted.cc", "alt.augminted.cc"]
"#;
        let cfg = load_str(toml).expect("parse");
        let resolved = cfg
            .resolve(Some("norn-server"), Some("prod"))
            .expect("resolve");
        assert_eq!(
            resolved.public,
            vec![
                "norn.augminted.cc".to_string(),
                "alt.augminted.cc".to_string()
            ]
        );
        let app_cfg = resolved.to_app_config();
        assert_eq!(app_cfg.public.len(), 2);
        assert_eq!(app_cfg.public[0], "norn.augminted.cc");
    }

    #[test]
    fn explicit_health_overrides_default() {
        let toml = r#"
[apps.x]
package = "x"
[apps.x.env.prod]
deploy_host = "h"
ssh_user = "u"
[apps.x.env.prod.health.process]
settle_secs = 30
"#;
        let cfg = load_str(toml).expect("parse");
        let resolved = cfg.resolve(Some("x"), Some("prod")).expect("resolve");
        assert!(matches!(
            resolved.health,
            HealthSpec::Process { settle_secs: 30 }
        ));
    }
}
