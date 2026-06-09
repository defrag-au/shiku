# Public ingress

Shiku services bind to localhost. Making one reachable from the public internet
— `my-service.example.com` resolving to the service on the box — is a separate,
opt-in concern handled through **Cloudflare Tunnel**. This covers how a declared
`public = ["my-service.example.com"]` becomes a live HTTPS endpoint.

## The boundary

Shiku gives you a localhost port and a stable systemd unit; **routing is
upstream's job.** The agent doesn't terminate TLS — certificate handling and
public routing live in front, in Cloudflare Tunnel. The ingress feature
integrates *with* a tunnel; it doesn't reimplement a reverse proxy. The core
deploy machinery works identically whether or not an app is public.

## Two credentials, agent-owned

Public ingress needs two Cloudflare credentials, both stored per-box as Shiku
secrets and never leaving the box:

| Credential | Purpose |
| --- | --- |
| API token | DNS edits (plus tunnel CRUD in auto-create mode) |
| Tunnel runtime token | cloudflared runtime auth |

They belong to a reserved `__shiku__` pseudo-app — it reuses the standard
secret machinery but has no binary, port, or health check, and is hidden from
`shiku app list`. The operator pastes the API token once at bootstrap; from
there the agent owns it.

## 1. Bootstrap the tunnel (once per box)

```sh
# Auto-create mode — the agent creates a new tunnel via the CF API.
# Requires the API token to have Account · Cloudflare Tunnel: Edit.
shiku tunnel bootstrap --host my-box --user service-user \
  --create-tunnel my-box-tunnel --zone example.com

# Manual mode — you created the tunnel in the dashboard already.
shiku tunnel bootstrap --host my-box --user service-user \
  --tunnel-id <UUID> --zone example.com
```

Bootstrap resolves your `--zone` arguments against what the API token can reach,
picks or creates the tunnel, stores both tokens, records the tunnel UUID and
authorized zones, and installs the `cloudflared` systemd unit. Every step is
idempotent — a failed bootstrap is just re-run. Pass `--from-stdin` to feed
tokens non-interactively (auto-create: one line, the API token; manual: two
lines, tunnel token then API token).

Inspect the box's tunnel state at any time:

```sh
shiku tunnel status --host my-box --user service-user
shiku tunnel zones  --host my-box --user service-user
shiku tunnel upgrade --host my-box --user service-user   # update cloudflared
```

## 2. Declare hostnames on the app

List the hostnames the app should own in its env section of `shiku.toml`:

```toml
[apps.my-service.env.prod]
deploy_host = "my-box"
ssh_user    = "service-user"
public      = ["my-service.example.com"]
```

Then deploy. Registration triggers a **reconcile** that makes DNS and the tunnel
match the declaration:

```sh
shiku deploy my-service
```

Publishing is declarative — list the hostnames, deploy, and the agent reconciles
to match: it upserts a CNAME for each new hostname, regenerates the tunnel
ingress map from the full app registry, reloads cloudflared (SIGHUP,
zero-downtime), and removes CNAMEs for hostnames you dropped. Re-running converges
because every step is idempotent.

## Validation rules

Before any DNS or ingress change, each requested hostname is checked — so an
invalid request is rejected cleanly rather than half-applied:

1. **Zone managed** — the hostname must fall under a zone authorized at bootstrap
   (longest-suffix match).
2. **Single-level subdomain** — exactly one label left of the zone (no
   `a.b.example.com`, no apex).
3. **Not claimed by another app** — no other registered app may already own the
   hostname (re-publishing your own is fine).

## Generated config

The agent owns `~/.cloudflared/config.yml` entirely and regenerates it on every
reconcile (sorted, with a "do not edit" banner and a mandatory `http_status:404`
catch-all last). Each hostname maps to its app's allocated localhost port. Don't
hand-edit it — your changes are discarded on the next reconcile.
