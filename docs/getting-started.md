# Getting started

This guide takes you from nothing to a live, health-gated deploy. It assumes
you have a Linux box you can SSH into as a user with `sudo`, and a Rust service
you want to run on it.

## 1. Install the CLI

The `shiku` CLI runs on your laptop:

```sh
cargo install --git https://github.com/defrag-au/shiku shiku
```

You also need [`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild)
and the musl target, because Shiku cross-compiles your service to a static
`aarch64-unknown-linux-musl` binary:

```sh
cargo install --locked cargo-zigbuild
rustup target add aarch64-unknown-linux-musl
```

> Why musl cross-compilation? So the binary your laptop builds is byte-for-byte
> the binary the box runs — including the `--shiku-manifest` introspection the
> agent relies on. No "the build tool saw a different binary than production" gap.

## 2. Bootstrap a box

Onboarding a box is one command. It creates a dedicated *service-user*, enables
lingering (so the agent runs without a login session), installs SSH keys from a
GitHub account, and installs and starts the `shikud` agent:

```sh
shiku bootstrap init \
  --host my-box \
  --admin <sudo-user> \
  --user <service-user> \
  --github <gh-user>
```

`--admin` is the existing sudo user used **only** for the one-time privileged
steps (`useradd`, `enable-linger`, key install). `--user` is the new
service-user the agent and your services run as. `bootstrap init` is
idempotent — safe to re-run.

Confirm the agent answers:

```sh
shiku ping        # → pong
```

Upgrading the agent later needs no privileged steps:

```sh
shiku bootstrap upgrade --host my-box --user <service-user>
```

> `bootstrap upgrade` swaps the agent binary but does **not** re-render the unit
> files of services already deployed — a service keeps its existing unit until
> its next activation. So after upgrading the agent to pick up new systemd
> hardening defaults, re-activate each running service to apply them:
> `shiku deploy <app>` (rebuild) or `shiku release activate <app> --sha <current>`
> (re-render + restart the existing release, no rebuild).

## 3. Describe your app

Create a `shiku.toml` at your project root. The CLI searches upward for it, so
you can run `shiku` from anywhere inside the project:

```toml
[shiku]
default_env = "prod"

[apps.my-service]
package = "my-service"          # the cargo package to build

[apps.my-service.env.prod]
deploy_host = "my-box"          # SSH host
ssh_user    = "<service-user>"  # the service-user from bootstrap
```

See [Deploying services](deploying.md) for the full schema.

## 4. Declare what the service needs

Link `shiku-runtime` and declare needs in code. The declarations *are* the
deploy config — there's no separate manifest file to keep in sync:

```toml
# Cargo.toml
[dependencies]
shiku-runtime = { git = "https://github.com/defrag-au/shiku" }
```

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    shiku_runtime::init();                 // handles --shiku-manifest + tracing

    let token = shiku_runtime::secret!("BOT_TOKEN")?;   // injected at activation
    let addr  = shiku_runtime::listen_http!()?;          // a port, allocated for you

    // ... start your server bound to `addr` ...
    Ok(())
}
```

## 5. Store secrets and deploy

Store any secrets the code declared (values are encrypted at rest on the box):

```sh
shiku secret set BOT_TOKEN     # masked prompt
```

Then deploy — build, upload, atomic activate, health-check, all in one:

```sh
shiku deploy my-service
shiku logs my-service -f       # follow output
```

If the new release fails its health check, Shiku **rolls back automatically** to
the previous good release. A successful deploy is safe to retry; a failed one
reverts cleanly.

## Where to next

- [Deploying services](deploying.md) — the `shiku.toml` schema and the deploy flow in detail.
- [Secrets](secrets.md) — how encryption and rotation work.
- [Public ingress](ingress.md) — give a service a public HTTPS hostname.
- [Operations](operations.md) — the daily workflow once you're live.
