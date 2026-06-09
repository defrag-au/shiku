# Deploying a new service to an existing box

[Getting started](getting-started.md) walks from nothing through bootstrapping a
box. This guide is the common follow-on: you already have a Shiku-managed box,
and you want to add a **new** service to it — for example a second native
Discord bot. It also covers the one decision Getting Started skips: whether the
new service gets its own user.

## First: one user, or its own user?

Every app under a given **service-user** shares that user's secret namespace and
process identity. Shiku's real isolation boundary is the **Linux user**, so the
choice matters:

- **Its own service-user** (recommended for anything independent) — bootstrap a
  dedicated user for the new bot. Its secrets, releases, and process are
  isolated from everything else on the box; a compromise can't reach another
  bot's secrets. This is the intended model.
- **Share an existing user** — fine for closely-related apps you're happy to
  treat as one trust domain. They share the secret store (though the hardened
  service unit makes the on-disk age store [inaccessible](deploying.md#hardening-defaults)
  to each service, so secrets only flow via each app's own injected env file).

For a brand-new, independent bot, prefer a dedicated user. Bootstrap it once:

```sh
shiku bootstrap init --host my-box --admin <sudo-user> \
  --user my-new-bot --github <gh-user>
```

(See [Getting started](getting-started.md) for what `bootstrap init` does.)

## 1. Create the crate and depend on the runtime

A native Discord bot is a normal binary crate that links `shiku-runtime`. It can
live in its own repo, or as a new member of an existing workspace (the latter
inherits the workspace's cross-build tooling and `shiku-runtime` pin, so it's
the least setup).

```toml
# Cargo.toml
[package]
name = "my-new-bot"
edition = "2021"

# Discord gateway bot — no HTTP, so the runtime's axum feature isn't needed.
[dependencies]
shiku-runtime = { git = "https://github.com/defrag-au/shiku", default-features = false }
secrecy = "0.10"   # for ExposeSecret, to read the SecretString that secret!() returns

serenity = { version = "0.12", default-features = false, features = ["client", "gateway", "model", "rustls_backend"] }
tokio = { version = "1", features = ["full"] }
anyhow = "1"
```

## 2. Declare needs in `main`

`init()` handles the `--shiku-manifest` flag the agent reads at deploy time, plus
tracing. Declare secrets with `secret!` — those declarations *are* the deploy
config; there's no separate manifest to keep in sync.

```rust
use secrecy::ExposeSecret;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    shiku_runtime::init();                          // --shiku-manifest + tracing

    // `secret!` declares the need AND reads the value (a `SecretString`)
    // injected from the encrypted store at activation. It returns a Result.
    let token = shiku_runtime::secret!("DISCORD_TOKEN")?;

    // Hand the exposed &str to your Discord client and run it.
    run_bot(token.expose_secret()).await
}
```

> The macro is `shiku_runtime::secret!` (re-exported from the runtime). It
> returns `Result<SecretString>`, so use `?`; `expose_secret()` (from the
> `secrecy` crate) borrows the value when you pass it to your client. This is
> the same pattern the production `eternal-seas` bot uses.

A gateway bot does no inbound listening, so it does **not** call `listen_http!()`
— its health check is a process check (stays running through a settle window),
and no public hostname/ingress is involved.

## 3. Describe it in `shiku.toml`

Add the app to your project's `shiku.toml` (or create one). For a bot under its
own user:

```toml
[shiku]
default_env = "prod"

[apps.my-new-bot]
package = "my-new-bot"

[apps.my-new-bot.env.prod]
deploy_host = "my-box"
ssh_user    = "my-new-bot"      # the dedicated user from the bootstrap step

[apps.my-new-bot.env.prod.vars]
RUST_LOG = "my_new_bot=info,serenity=info"
```

See [deploying.md](deploying.md) for the full schema (resource overrides, health
config, etc.). You inherit the [hardened service unit](deploying.md#hardening-defaults)
automatically — which is tuned for exactly this shape (network-egress, no
inbound).

## 4. Store secrets and deploy

```sh
shiku secret set DISCORD_TOKEN     # masked prompt; encrypted at rest on the box
shiku deploy my-new-bot            # build → upload → activate → health-check
shiku logs my-new-bot -f           # watch it connect to Discord
```

If the bot fails its health check, Shiku rolls back automatically. A clean
deploy ends with the bot active and `shiku status my-new-bot` showing the
current release.

## 5. Verify

```sh
shiku status my-new-bot            # current sha, systemd state, uptime
shiku env my-new-bot               # what got injected (DISCORD_TOKEN shown masked)
```

That's the whole loop. Subsequent changes are just `shiku deploy my-new-bot`
again; rotating the token is `shiku secret set DISCORD_TOKEN` then
`shiku restart my-new-bot` (the env file is read at process start — see
[secrets.md](secrets.md)).
