# Design: release assets

**Status:** in progress — foundation landed, CLI + agent remaining.

## Implementation status

**Done (behaviour-neutral, builds + tests green):**

- `shiku-types`: `ReleaseManifest.assets: Vec<AssetBinding>` (`#[serde(default)]`)
  + the new `AssetBinding { path, hash }` type.
- `shiku-runtime`: `assets` module — `asset_path("…")` (checked, path-confined,
  resolve-once) + `asset_dir()`, `RuntimeError::{Asset, MissingAsset}`,
  re-exported. Unit-tested.
- All current call sites build assets-empty, so nothing changes at runtime yet.

**Remaining — one coupled change** (the release sha and the agent's verify must
move together; see below):

1. **CLI `config`:** `assets: Vec<String>` on `AppSection` + `ResolvedApp`;
   thread the project root (dir of `shiku.toml`, from `Config::load`) into
   `ResolvedApp` via `load_and_resolve` so globs resolve against it.
2. **CLI `release`:**
   - `collect_assets(project_root, &globs)` — glob, **reject symlinks**, enforce
     the §7 quotas (25 MiB/file, 20,000 files, warn 64 / fail 256 MiB total).
   - `asset_bindings(&files)` — sha each → `Vec<AssetBinding>`, sorted by path.
   - `release_sha(binary_sha, &bindings)` = `sha256(binary_sha ‖ canonical)`,
     where `canonical` = the sorted `"{path}\n{hash}\n"` lines.
   - `rsync_assets` — stage into a temp dir mirroring rel paths, one rsync to
     `releases/<sha>/assets/`.
   - `build_manifest` gains `assets` **and `binary_sha`** (see §coupling).
3. **CLI `build_and_upload`:** collect assets → compute the **combined**
   `release_sha` → use it for the present-check, rsync dest, and manifest. rsync
   the asset tree when non-empty.
4. **Agent `releases::verify_and_finalize`:** today it re-hashes the binary and
   compares to `manifest.sha`. Once `sha` is the *combined* hash that no longer
   holds — so the manifest must also carry **`binary_sha`** (the raw binary
   hash). Verify: `hash(binary) == manifest.binary_sha`; each
   `hash(assets/<path>) == manifest.assets[i].hash`; and
   `release_sha(binary_sha, assets) == manifest.sha`.
5. **Agent `activate`:** after the working-dir step, when the release manifest
   has assets, add `Environment=SHIKU_ASSETS_DIR=<app_root>/current/assets` to
   the rendered unit (the manifest is already re-read here for secrets).

> **The coupling:** steps 3 (CLI sets `sha = combined`) and 4 (agent verifies
> against `binary_sha`, not `sha`) must land together — shipping 3 without 4
> would make every deploy fail verification. Hence `binary_sha` is added to the
> manifest as part of this change.



Let a service ship companion data files — config, templates, reference images,
static content — *as part of its release*, so those files version, activate,
and roll back atomically with the binary instead of being hand-synced out of
band.

## Problem

A Shiku release is content-addressed on the **binary** and stored as
`~/apps/<app>/releases/<sha>/bin/<command>`. Activation swaps the `current`
symlink; `previous` gives rollback. Mutable working data lives in
`~/data/<app>`, which is persistent and deliberately **not** release-versioned.

That leaves a gap: a service that needs read-only companion files (a Discord
bot with brand assets, a renderer with templates, anything with a config tree)
has nowhere to put them inside the release. Today the only options are:

- **rsync into `~/data/<app>`** — works, but the files now live *outside* the
  release. They aren't atomic with activation, aren't covered by the health
  gate, and — most importantly — **aren't rolled back** when a deploy reverts.
  You're hand-managing the one part of the system Shiku exists to manage.
- **embed in the binary** (`include_bytes!`) — no platform support needed, but
  it bloats every release, forces from-bytes loaders in the service, and bakes
  data into the binary's content hash whether you like it or not.

Neither is the platform giving a first-class answer. This proposal adds one.

## Goals

- Declared read-only files ship **with** the release, in one `shiku deploy`.
- They are **versioned and roll back** with the binary, atomically.
- Identical content collapses to one release (content-addressing still holds),
  but a change to *either* binary or assets produces a new release.
- The service reads them through a stable, relocatable path — never a hardcoded
  one.
- Zero new moving parts at runtime: no extra network dependency, no credentials.

## Non-goals

- **Mutable data.** Assets are read-only release content. Writable state stays
  in `~/data/<app>` as today.
- **Large blobs** (models, multi-GB datasets). Release assets target the
  config / template / reference-image scale (KB–low MB). A separate
  "data volumes" concept can address large or independently-versioned data
  later.
- **Per-environment asset sets.** Assets are part of the build artifact, shared
  across environments, like the binary. (Per-env *values* belong in vars /
  secrets.)

## Design

### 1. Declaration — in `shiku.toml`

Unlike secrets / bindings / ports, assets are a **packaging** concern (what to
upload from the laptop), not a runtime *need* the binary introspects via
`--shiku-manifest`. So they're declared in config, under `[apps.<name>]`
(they're build-artifact properties, shared across environments):

```toml
[apps.meme-bot]
package = "meme-bot"
assets  = ["projects/aliens-polaroid", "characters/alien"]
```

`assets` is a list of paths or globs, resolved relative to the project root
(the directory containing `shiku.toml`). Each entry is copied into the release
preserving its relative path, so `characters/alien/grey-plush.png` lands at
`<release>/assets/characters/alien/grey-plush.png`.

**Symlinks in the source tree are not followed.** A symlink within a declared
asset path is a deploy error, not a copy of its target: following them would let
a release pull in arbitrary files from outside the project (an injection vector)
and produce non-reproducible bundles.

### 2. Release identity must cover assets

This is the load-bearing decision. Today the release sha is the SHA-256 of the
binary, and an identical build skips the upload. If assets weren't part of that
identity, changing a reference image without touching code would silently
**no-op** — the box already has that binary sha, so nothing uploads.

So the release id becomes a hash over **binary + asset manifest**:

```
asset_manifest = sorted list of (relative_path, sha256(file_bytes))
release_sha    = sha256(binary_sha256 || canonical(asset_manifest))
```

Consequences, all desirable:

- An asset-only change yields a new `release_sha` → a new, fully rollback-able
  release.
- Identical binary **and** identical assets still collapse to one release and
  skip the upload.
- The manifest is order- and path-canonicalised so the hash is stable.

### 3. Storage & activation

A release directory gains an `assets/` subtree beside `bin/`:

```
~/apps/<app>/releases/<sha>/
    bin/<command>
    assets/…                <-- NEW: declared files, preserved tree
    .asset-manifest         <-- NEW: the (path, sha) list, for verification
```

The upload step rsyncs `assets/` alongside the binary; the agent verifies each
file against `.asset-manifest` (same way it re-hashes the binary today).
**Activation is unchanged** — the existing atomic `current` → `releases/<sha>`
symlink swap already moves the whole release dir as a unit, so assets activate
and roll back for free.

### 4. Runtime access

The service must read assets from the **active release**, never a fixed path.
The agent exports the location into the unit's environment:

```
SHIKU_ASSETS_DIR=/home/<user>/apps/<app>/current/assets
```

and the runtime offers a **checked** accessor mirroring `secret!` / `binding!`:

```rust
let cfg = shiku_runtime::asset_path("projects/aliens-polaroid/project.toml")?;
```

`asset_path(rel)` joins `rel` onto the assets dir and returns the path only if
it exists, else an error naming the missing asset and its likely cause — that
it wasn't listed under `assets` in `shiku.toml`. The binary can't see the
`shiku.toml` declaration list, so the check is *existence in the release*; that
still catches the forgot-to-declare case with a clear message at the call site
instead of a vague file-not-found deeper in. (`rel` is also confined to the
assets dir — a `..` that escapes it is rejected.)

The accessor **resolves the `current` symlink to its concrete
`releases/<sha>/assets` once at startup** and caches that base, then joins onto
it. This pins a long-running process to *its own* release's assets even if
`current` swaps under it during a later deploy — it keeps reading a consistent
set until it restarts (which activation does anyway). Joining through the live
symlink on every call would instead let a mid-run swap change assets underneath
the process; resolve-once avoids that.

> A `manifest` entry (like `Need::Secret`) could also be emitted so
> `--shiku-manifest` reports "this binary expects assets", letting the agent
> warn on a deploy that declares none. Optional; the config is the source of
> truth either way.

### 5. Interaction with the hardened unit

Assets are **read-only**, which is exactly the right shape. The default unit
runs `ProtectHome=read-only` with a single writable `WorkingDirectory`
(`~/data/<app>`). Read-only still permits reads, so `~/apps/<app>/current/assets`
is readable with no hardening change, while writable scratch stays in
`~/data/<app>`. The read-only-ness is enforced by the release dir being
immutable release content, not by unit config — a service can't accidentally
mutate an asset and desync it from its hash.

### 6. Pruning

Unchanged: pruning keeps `current` + `previous` + the last few releases, and
assets are inside those release dirs, so they're retained and pruned with their
release. No separate lifecycle.

### 7. Size quota

Bundling data into releases needs a ceiling — to keep uploads and activations
fast, and because (unlike a Worker) a Shiku box **retains several releases**
(`current` + `previous` + a few), so every asset megabyte is multiplied by
retention on one disk.

Where it maps, align with Cloudflare Workers **Static Assets** — the closest
analog, read-only files shipped with a deployment
([limits](https://developers.cloudflare.com/workers/static-assets/billing-and-limitations/)):

- **Per file: 25 MiB** — adopt directly.
- **File count: 20,000 per release** — adopt Cloudflare's free-tier number (paid
  is 100,000). Far more than any config/asset tree needs, and a useful
  runaway-glob backstop.

Cloudflare publishes no *total* bundle cap for static assets (their model is
ephemeral + distributed), but Shiku should add one because of multi-release
retention on a single box. Propose a **total soft cap (warn)** and a
configurable **hard cap**, defaulting conservatively — e.g. warn at 64 MiB, fail
at 256 MiB per release — overridable per app in `shiku.toml` for the occasional
heavier service. The CLI enforces all three at build time, before upload, with a
clear message.

## What this touches

- **CLI:** resolve `assets` globs (rejecting symlinks); enforce the per-file /
  file-count / total-size quotas (§7) before upload; build the asset manifest +
  per-file hashes; fold them into `release_sha`; rsync `assets/` +
  `.asset-manifest` into the release dir; skip when the sha already exists on
  the box.
- **Agent:** verify uploaded assets against `.asset-manifest`; export
  `SHIKU_ASSETS_DIR` in the rendered unit; (no activation/prune changes).
- **Runtime:** `asset_dir()` (resolve-once); optional `Need::Assets` manifest
  entry.
- **Schema/docs:** the `assets` key in `shiku.toml`; a section in
  `deploying.md`.

## Alternatives considered

| Option | Atomic w/ release | Rollback | Cost |
|---|---|---|---|
| **Release assets** (this) | ✓ | ✓ | platform feature, contained |
| rsync to `~/data` (today) | ✗ | ✗ | out-of-band, drifts |
| `include_bytes!` in binary | ✓ | ✓ | binary bloat, from-bytes loaders |
| R2 / object store at startup | ✗ | ✗ | runtime net dep + creds |

Release assets is the only option that is both atomic-with-the-binary and free
of a runtime dependency, at the file scale services actually carry.

## Resolved decisions

- **Declaration: `shiku.toml` `assets = [...]`** (config, not a code macro). The
  TOML is a convenient single place to see which directories get injected into
  the Shiku asset store.
- **Do not follow symlinks** — a symlink inside a declared asset path is a deploy
  error (injection vector + non-reproducible bundles).
- **API: `asset_path("…")`** — the checked, path-confined join (§4), not a raw
  `asset_dir()`.
- **Size quota:** align per-file (25 MiB) and file-count (20,000) with Cloudflare
  Workers Static Assets; add a Shiku-specific total cap (warn/fail) for the
  multi-release-retention case (§7).

## Still open

- The glob crate / syntax.
- The total-size warn/fail defaults (proposed 64 / 256 MiB) and whether the hard
  cap is per-app overridable.
- Whether to also emit a `--shiku-manifest` "expects assets" entry so the agent
  can warn on a deploy that declares none.

## Worked example: meme-bot

Before (out-of-band): `rsync projects/ characters/` to `~/data/meme-bot`, and
the bot reads `Path::new("projects")` from its working dir — unversioned, not
rolled back.

After (release assets):

```toml
[apps.meme-bot]
package = "meme-bot"
assets  = ["projects/aliens-polaroid", "characters/alien"]
```

```rust
let projects = shiku_runtime::asset_path("projects")?;
let cfg = ProjectConfig::load(&projects, "aliens-polaroid")?;
```

`shiku deploy meme-bot` now ships code **and** brand assets as one atomic,
rollback-able release. No rsync, no embed.
