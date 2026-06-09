# Secrets

Shiku lets a service read `BOT_TOKEN` from its environment without that token
ever touching persistent disk in plaintext. This covers encryption at rest,
delivery, injection, rotation, and the one rule that matters most: **don't
`cat` the env file.**

## At rest: age encryption

Secrets are encrypted with [`age`](https://age-encryption.org) (x25519). On
first use the agent generates a per-user master key at `~/secrets/master.key`
(mode 0600); each secret is stored as `~/secrets/<NAME>.age` (also 0600).

Encryption is **per-user, not per-app** — apps sharing a service-user share the
secret namespace, with each app's allowlist controlling which secrets it
actually receives.

## Setting and rotating

```sh
shiku secret set BOT_TOKEN                 # masked prompt
echo "$TOKEN" | shiku secret set BOT_TOKEN --from-stdin   # pipe from a manager
shiku secret list                          # names only, never values
shiku secret remove BOT_TOKEN
shiku secret rotate BOT_TOKEN              # alias for set; clarifies intent in history
```

The CLI sends the plaintext to the agent over SSH (encrypted in transit by SSH
itself); the agent encrypts it to the master key, writes the ciphertext, and
drops the value from memory. The CLI never echoes the value back, and refuses to
set an empty secret.

**A rotated secret takes effect on the next `restart`.** The env file is read at
process start, so changing a value requires regenerating that file:

```sh
shiku secret set BOT_TOKEN     # new value
shiku restart my-service       # service picks it up
```

## At activation: a tmpfs env file

When an app activates, the agent decrypts each allowed secret and writes them
into an env file at `$XDG_RUNTIME_DIR/shiku/<app>.env` — mode 0600, on
**tmpfs**. The systemd unit reads it via `EnvironmentFile=`, so the secrets land
in the service's process environment. The decrypted values are dropped
immediately after the file is written.

The key property: the env file is the only on-disk plaintext, it's 0600, it
lives on tmpfs, it's never written to persistent disk or any backup, and it's
gone on reboot.

## The don't-`cat` rule

> **Do not `cat` the env file.** It's 0600 on tmpfs and safe where it sits. But
> `cat`-ing it copies the plaintext into your shell scrollback, history, and any
> recording session — which are **not** 0600 and **do** persist. The leak isn't
> the file; it's where its contents end up when you read it carelessly.

The supported way to inspect what got injected is `shiku env <app>`, which shows
the resolved config but **masks every secret value**:

```sh
shiku env my-service
# BOT_TOKEN=********
# (or "(NOT SET — activation will fail)" if a declared secret has no value)
```

## Allowlist enforcement

Each app receives only the secrets it declares. The allowlist is populated
automatically from the [manifest](deploying.md) — `shiku::secret!("BOT_TOKEN")`
in your code puts `BOT_TOKEN` in the app's allowlist. A secret stored on the box
but absent from an app's allowlist is never decrypted or injected for that app.
A secret *in* the allowlist but missing from storage fails activation fast,
before any restart — so you find out at deploy time, not when the service
crashes.

## A clear-eyed boundary

The allowlist is **defense in depth, not a security boundary.** A compromised
app running as the service-user can read any `~/secrets/*.age` and decrypt them
with the master key it has access to. The real isolation boundary is the **Linux
user** — which is why apps that must be isolated from each other should run as
*different service-users*. Kernel for isolation, allowlist for hygiene.
