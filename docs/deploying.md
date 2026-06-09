# Deploying services

This is the reference for the `shiku.toml` schema, declaring needs from code,
and what a deploy actually does end to end.

## The `shiku.toml` schema

One file at your project root describes every deployable app and its
environments. The CLI walks up from the current directory to find it (override
with `--config <path>`).

```toml
[shiku]
default_env = "prod"            # used when --env is omitted

[apps.my-service]
package = "my-service"          # cargo package to build (required)
target  = "aarch64-unknown-linux-musl"   # default; the only deploy target today
build   = "cargo zigbuild"      # default build command

[apps.my-service.env.prod]
deploy_host = "my-box"          # SSH host the agent lives on (required)
ssh_user    = "service-user"    # service-user that owns this deployment (required)
service     = "my-service"      # systemd unit name; defaults to the app name
public      = ["my-service.example.com"]   # public hostnames (see ingress.md)

[apps.my-service.env.prod.vars]
RUST_LOG = "info"               # plain (non-secret) environment variables

[apps.my-service.env.prod.health]
# Optional. Defaults to a process health check for non-listen apps.

[apps.my-service.env.prod.systemd]
memory_max        = "512M"      # optional systemd unit overrides
cpu_quota_percent = 80
restart           = "on-failure"
restart_sec       = 5
```

Fields under `[apps.<name>]` apply across all environments; per-environment
connection details and overrides live under `[apps.<name>.env.<env>]`. A project
with exactly one app lets you omit the app name on the command line; `--env`
falls back to `shiku.default_env`.

> **Declare needs from code, not config.** `secrets`, `bindings`, and `listen`
> can technically be listed in `shiku.toml`, but those keys are deprecated and
> kept only for third-party binaries that can't be introspected. For your own
> services, declare them with the macros below — the binary becomes the single
> source of truth.

## Declaring needs from code

A service declares what it needs with three macros, re-exported from
`shiku-runtime`. The argument **must be a string literal** — the macros reject
anything dynamic at compile time, because the manifest is built from these
literals.

```rust
let token = shiku::secret!("BOT_TOKEN");   // inject this secret
let url   = shiku::binding!("narrator");    // resolve the URL of the 'narrator' app
let addr  = shiku::listen_http!();           // allocate an HTTP listen port
```

Each macro registers the need into a compile-time-aggregated list **and** emits
the runtime call that uses it. The binary can then print the full set with:

```sh
./my-service --shiku-manifest
```

`shiku_runtime::init()`, called at the top of `main`, handles that flag (and
sets up tracing). At deploy time the agent runs your binary with
`--shiku-manifest`, reads the manifest, and provisions exactly what it asks for.
Add a `shiku::secret!("NEW_TOKEN")` and redeploy, and the agent injects
`NEW_TOKEN` automatically — provided you've [stored its value](secrets.md). The
code change *is* the config change; the two can't drift.

## What a deploy does

`shiku deploy <app>` runs the whole pipeline:

1. **Register** — pushes the resolved `shiku.toml` (vars, public hostnames,
   health, systemd overrides) to the agent, so the box reflects your config
   before activation.
2. **Build** — `cargo zigbuild` to a static `aarch64-unknown-linux-musl` binary.
3. **Hash** — SHA-256 the binary. Releases are **content-addressed**, so an
   identical build collapses to one release and the upload is skipped if the
   box already has that sha.
4. **Upload** — rsync the binary to `~/apps/<app>/releases/<sha>/` on the box.
5. **Activate** — the agent reads the manifest, resolves secrets and bindings,
   allocates a port, renders a hardened systemd unit, writes the env file,
   **atomically swaps the `current` symlink**, and restarts the unit.
6. **Health-check** — healthy → done (old releases pruned, keeping the last
   few). Unhealthy → **automatic rollback** to the previous release.

Every on-disk write — config, unit file, env file, the `current` symlink — uses
tempfile-and-rename, so a crash mid-activation always leaves *either* the old
release running or the new one, never an in-between.

The CLI streams activation progress live:

```
  ✓ release verified
  ✓ systemd unit written
  ✓ secrets + bindings resolved
  ✓ process starting
  · health check: …
  ✓ health check passed
  ✓ activated 9f3c1a…
```

## Lower-level control

`deploy` is build + upload + activate fused. The steps are also available
separately when you want them:

```sh
shiku release upload <app>            # build + hash + upload, no activation
shiku release list <app>              # releases on the box (* current, ← previous)
shiku release activate <app> --sha <full-hex>   # activate a specific release
```

## Health checks

The health gate runs after the restart. The mode is chosen automatically by
whether the app declared `listen_http!()`:

- **Process** (default for non-listen apps): polls `systemctl --user is-active`
  until the service settles. A `failed` report fails fast.
- **HTTP** (for listen apps): polls `http://127.0.0.1:<port>/<path>` until it
  returns an expected status, or the timeout elapses.

Either way, the health check is the gate: pass and the deploy stands; fail and
the agent rolls back. See [Operations](operations.md) for the day-to-day flow.
