# Shiku

A declarative deploy platform for native Rust binaries on a single Linux box: a
CLI, a per-user agent, and a runtime library — atomic activation with
health-gated rollback, a code-declared needs manifest, age-encrypted secrets,
service bindings, and Cloudflare Tunnel ingress.

## Crates

| Crate           | Bin       | Role                                                                 |
| --------------- | --------- | ------------------------------------------------------------------- |
| `shiku-types`   |           | Shared wire-protocol and config types.                              |
| `shiku-macros`  |           | Proc-macros for declaring a service's needs (secrets, bindings, ports). |
| `shiku-runtime` |           | Service-side runtime implementing the deploy contract.             |
| `shiku` (cli)   | `shiku`   | Deploys native services from your laptop to a Shiku-managed box.    |
| `shikud` (agent)| `shikud`  | Per-user box-side daemon that receives deploys.                     |

## Development

```sh
just check   # cargo check the workspace
just lint    # clippy, warnings as errors
just test    # run tests
just shiku -- ping   # run the CLI locally
```

Consumers depend on `shiku-runtime` via git:

```toml
shiku-runtime = { git = "https://github.com/defrag-au/shiku" }
```
