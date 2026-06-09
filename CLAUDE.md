# CLAUDE.md

Guidance for Claude Code when working in the **shiku** workspace.

Shiku is a declarative deploy platform for native Rust binaries on a single
Linux box: a CLI (`shiku`), a per-user agent (`shikud`), and a runtime library
(`shiku-runtime`), plus `shiku-types` (wire protocol) and `shiku-macros`. See
`README.md` and `docs/` for the operator-facing story.

## Git operations are the user's domain (IMPORTANT)

**The user handles all git operations themselves.** Do NOT run `git commit`,
`git push`, `git branch`/`checkout` to create branches, or open PRs (`gh pr
create`) unless explicitly asked in that moment.

- Make the code/doc changes and leave them in the working tree for the user to
  review, stage, and commit.
- Read-only git is fine: `git status`, `git diff`, `git log`.
- When work is ready, summarise what changed and suggest a commit message / PR
  body for the user to use — don't execute it.

This also means: never use git to revert/reset/checkout/stash to undo changes.
If something breaks, fix it forward or explain it and let the user decide.

## Build & toolchain

This repo is **self-contained** — it has its own `flake.nix` (devshell: rust +
the `aarch64-unknown-linux-musl` target, `cargo-zigbuild`, `zig`, `just`,
`rsync`) and a `rust-toolchain.toml`. It does **not** depend on the
augminted-bots dev shell.

- **nix + direnv:** `direnv allow` once, then `cargo`/`just` just work.
- **nix, no direnv:** `nix develop --command cargo check --workspace`.
- **rustup (no nix):** `rust-toolchain.toml` selects the toolchain + musl
  target; you separately need `cargo install cargo-zigbuild` and a system `zig`.

`just` recipes: `just check`, `just lint`, `just test`, `just shiku -- <args>`,
`just build-arm` (cross-compile CLI + agent for the deploy target,
`aarch64-unknown-linux-musl`, via `cargo zigbuild`).

Note: nix flakes only see git-*tracked* files, so newly-added files must be
`git add`ed before `nix develop`/direnv will pick them up (or use
`nix develop "path:$PWD"` to include untracked files during local iteration).

## CI

`.github/workflows/ci.yml` runs two jobs:
- **fmt · clippy · test** — `cargo fmt --check`, `cargo clippy --workspace
  --all-targets -- -D warnings`, `cargo test --workspace`. Keep all three green.
- **cross-build (aarch64 musl)** — installs Zig **0.15.1 from the pypi
  `ziglang` wheel** (NOT `mlugg/setup-zig` — its mirrors 404 on some versions),
  then `cargo zigbuild` for `shiku` + `shikud`.

`RUSTFLAGS: -D warnings` is set in CI, so warnings fail the build.

## Where the design docs live

The full design treatise (the mdbook: mental model, wire protocol, activation &
rollback, secrets, ports & bindings, ingress, design boundaries) lives in the
**augminted-bots** repo at `docs/design/shiku-platform/`, not here. This repo's
`docs/` are practical operator howtos (getting-started, deploying, secrets,
ingress, operations, cli-reference). Author design prose in augminted-bots;
keep `docs/` here focused on using/operating Shiku.

## Conventions

- `#![forbid(unsafe_code)]` in the binaries — keep it that way.
- All ECS / on-disk writes use tempfile-and-rename for atomicity; follow that
  pattern for any new on-box state the agent writes.
- The generated systemd units (service + agent) carry a hardening baseline;
  when adding directives, keep them safe for a network-egress service and note
  any that could break unusual workloads (e.g. `MemoryDenyWriteExecute` breaks
  JITs). See `docs/deploying.md` § Hardening defaults.
