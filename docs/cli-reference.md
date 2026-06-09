# CLI reference

Every `shiku` command and its flags. Run `shiku <command> --help` for the
canonical, version-matched details.

## Global flags

| Flag | Effect |
| --- | --- |
| `--config <PATH>` | Path to the config file (default: `shiku.toml`, searched upward). |
| `--env <ENV>` | Environment selector. Falls back to `shiku.default_env`. |

Most commands take an optional `[app]` positional. It can be omitted when
`shiku.toml` defines exactly one app.

## Lifecycle

| Command | Description |
| --- | --- |
| `shiku ping [app]` | Health-check the agent for the app's environment (`→ pong`). |
| `shiku deploy [app]` | Register + build + upload + atomically activate, with live progress. |
| `shiku rollback [app]` | Roll back to the previously activated release. |
| `shiku restart [app]` | Restart without redeploying (re-renders env; picks up rotated secrets). |
| `shiku status [app] [--all]` | Current sha, systemd state, uptime. `--all` shows every app. |
| `shiku logs [app] [--no-follow] [--since <when>]` | Stream journald output. `--since` takes a `journalctl` value, e.g. `"1 hour ago"`. |
| `shiku env [app]` | Show the env injected at activation, **secrets masked**. |

## Secrets

| Command | Description |
| --- | --- |
| `shiku secret set <NAME> [--from-stdin] [--app <app>]` | Set or rotate a secret (masked prompt unless `--from-stdin`). |
| `shiku secret list [--app <app>]` | List secret names. Values are never printed. |
| `shiku secret remove <NAME> [--app <app>]` | Remove a secret. |
| `shiku secret rotate <NAME> [--from-stdin] [--app <app>]` | Alias for `set`; clarifies intent in shell history. |

## Releases

| Command | Description |
| --- | --- |
| `shiku release upload [app]` | Build, hash, and upload a release without activating it. |
| `shiku release activate [app] --sha <HEX>` | Activate a previously uploaded release by sha. |
| `shiku release list [app]` | List releases on the box (`*` current, `←` previous). |

## Apps and config

| Command | Description |
| --- | --- |
| `shiku app register [app]` | Push the app's resolved `shiku.toml` to the agent. |
| `shiku app remove [app]` | Remove an app and all its releases from the agent. |
| `shiku app list [app]` | List registered apps (same as `shiku apps`). |
| `shiku app inspect [app]` | Print the on-box config the agent holds for the app. |
| `shiku apps [app]` | Alias for `app list`. |
| `shiku config show [app]` | Print the resolved config the rest of the CLI sees (debugging aid). |

## Bootstrap (per box / service-user)

| Command | Description |
| --- | --- |
| `shiku bootstrap init --host <H> --admin <A> --user <U> --github <G> [--agent-binary <PATH>]` | Provision a service-user and install + start the agent. Idempotent. |
| `shiku bootstrap upgrade --host <H> --user <U> [--agent-binary <PATH>]` | Swap in a new agent binary. No privileged steps. |

`--admin` is the existing sudo user used only for the one-time privileged setup;
`--user` is the service-user the agent and services run as; `--github` supplies
the SSH keys to install. `--agent-binary` defaults to
`target/aarch64-unknown-linux-musl/release/shikud`.

## Tunnel (public ingress)

| Command | Description |
| --- | --- |
| `shiku tunnel bootstrap --host <H> --user <U> (--create-tunnel <NAME> \| --tunnel-id <UUID>) --zone <Z>… [--from-stdin]` | One-time tunnel setup. `--create-tunnel` auto-creates via the CF API; `--tunnel-id` adopts an existing tunnel. Repeat `--zone` per zone. |
| `shiku tunnel status --host <H> --user <U>` | Tunnel UUID, cloudflared state, zones, current ingress map. |
| `shiku tunnel zones --host <H> --user <U>` | Zones this box is authorized to publish into. |
| `shiku tunnel upgrade --host <H> --user <U>` | Download the pinned cloudflared release, verify, and swap the binary. |

See [Public ingress](ingress.md) for the full flow.
