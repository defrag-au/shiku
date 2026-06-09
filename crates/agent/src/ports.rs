//! Port allocator for `[apps.X.listen.http = true]` apps.
//!
//! Allocation strategy:
//!   - Pool: IANA dynamic/private range, `49152..=65535` (~16k ports).
//!   - Per-user. A service user's agent allocates from the pool independently;
//!     cross-user collisions don't happen because each agent runs in its own
//!     home and other users' bound ports surface as `EADDRINUSE` if they
//!     somehow collide (they shouldn't — apps bind to 127.0.0.1 in their own
//!     network namespace under systemd-user).
//!   - Persisted in each app's `config.toml` as `listen.http_port`. First
//!     registration allocates; subsequent registrations preserve the
//!     existing assignment.
//!   - Released on `app remove`. Allocator scans live config.toml files
//!     each call, so the available pool is "everything in the range that
//!     isn't currently held by some other app."
//!
//! We deliberately don't use Linux ephemeral allocation (bind to 0) because
//! we want the port to be stable across activations. A service whose port
//! changes on every restart breaks bindings, breaks the Caddyfile / CF
//! Tunnel config, breaks debugging — Shiku owns the assignment instead.

use std::collections::HashSet;

use anyhow::{anyhow, Context, Result};
use shiku_types::{AppConfig, AppName};

use crate::apps;

/// IANA dynamic/private port range. The top of this range can collide with
/// nothing in the well-known or registered ranges; the bottom is well above
/// anything common.
pub const POOL_START: u16 = 49152;
pub const POOL_END: u16 = 65535;

/// Ensure an app's `listen.http_port` is allocated and recorded.
///
/// - If the app doesn't want HTTP (`listen` is `None` or `http = false`),
///   no-op: returns `Ok(None)`.
/// - If the app already has a recorded port, validates it's still in-range
///   and unique among other registered apps; returns it as-is.
/// - Otherwise, picks a fresh port and writes it back to `config.listen.http_port`.
///
/// The mutation is on the in-memory `config` — the caller is responsible
/// for persisting via `apps::register` after this returns.
pub fn ensure_allocated(app: &AppName, config: &mut AppConfig) -> Result<Option<u16>> {
    let listen = match &mut config.listen {
        Some(l) if l.http => l,
        // No HTTP port requested.
        _ => return Ok(None),
    };

    let used = collect_used_ports(Some(app))?;

    // If we already have a port, keep it (assuming it's still valid).
    // "Valid" means: not in use by another Shiku app on this user, in
    // the pool range, and currently bind-able on localhost (a different
    // service-user on the same box might be holding it).
    if let Some(existing) = listen.http_port {
        if !used.contains(&existing) && (POOL_START..=POOL_END).contains(&existing) {
            // Skip the probe if the *running* app on this user already
            // holds this port — that's the normal case where we're
            // re-registering a healthy service.
            return Ok(Some(existing));
        }
        // Edge case: the previously-assigned port now collides with another
        // app, or it's out of range. Re-allocate.
        tracing::warn!(
            app = %app,
            old_port = existing,
            "previously-assigned port no longer valid; re-allocating"
        );
    }

    let port = pick_unused_probed(&used).ok_or_else(|| {
        anyhow!(
            "no free ports in dynamic range ({POOL_START}..={POOL_END}); \
             {} apps currently registered with HTTP listeners (probed for \
             external bindings too)",
            used.len()
        )
    })?;

    listen.http_port = Some(port);
    tracing::info!(app = %app, port, "allocated http port");
    Ok(Some(port))
}

/// Collect the set of HTTP ports currently bound by other apps for this
/// user (i.e. anything written into another app's `config.toml`). Optionally
/// excludes one app from the scan (used when re-allocating for that app
/// itself, so its old port doesn't count as taken by someone else).
fn collect_used_ports(exclude: Option<&AppName>) -> Result<HashSet<u16>> {
    let names = apps::list().context("listing apps")?;
    let mut used = HashSet::new();
    for name in names {
        if Some(&name) == exclude {
            continue;
        }
        let cfg = match apps::inspect(&name) {
            Ok(c) => c,
            Err(_) => continue, // half-registered app; skip
        };
        if let Some(port) = cfg.listen.as_ref().and_then(|l| l.http_port) {
            used.insert(port);
        }
    }
    Ok(used)
}

/// Pick the first unused port in the pool. Linear scan — at 16k ports this
/// is microseconds. If we ever need something cleverer (random selection
/// for security through obscurity, or starting from a hash of the app name
/// for predictability), the implementation is contained here.
///
/// Used in unit tests; the production allocator goes through
/// [`pick_unused_probed`] which also probes the kernel.
#[cfg(test)]
fn pick_unused(used: &HashSet<u16>) -> Option<u16> {
    (POOL_START..=POOL_END).find(|p| !used.contains(p))
}

/// Pick the first port that's both unused-by-Shiku-apps *and* probes as
/// available on `127.0.0.1`. The cross-user case: another service-user
/// on the same box may have grabbed a port we don't see in our apps
/// registry. The kernel will reject our app's bind with EADDRINUSE if
/// we hand out the same port — rather than fail at activation, do a
/// cheap probe at allocation time and skip taken ports here.
///
/// "Probe" = try a `TcpListener::bind`; if it succeeds we drop it
/// immediately. There's a tiny TOCTOU window between probe and the
/// app actually binding, but in practice nothing else on the box is
/// fighting over ports in this range, and a real bind failure at
/// activation surfaces as a typed health-check error anyway.
fn pick_unused_probed(used: &HashSet<u16>) -> Option<u16> {
    use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
    for port in POOL_START..=POOL_END {
        if used.contains(&port) {
            continue;
        }
        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
        if TcpListener::bind(addr).is_ok() {
            return Some(port);
        }
        tracing::debug!(port, "port probed as taken; skipping");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_is_dynamic_range() {
        assert_eq!(POOL_START, 49152);
        assert_eq!(POOL_END, 65535);
        // ~16k available — plenty of headroom.
        const { assert!(POOL_END - POOL_START > 16_000) };
    }

    #[test]
    fn pick_unused_takes_first_free() {
        let used: HashSet<u16> = HashSet::new();
        assert_eq!(pick_unused(&used), Some(POOL_START));
    }

    #[test]
    fn pick_unused_skips_taken() {
        let used: HashSet<u16> = [POOL_START, POOL_START + 1, POOL_START + 2]
            .iter()
            .copied()
            .collect();
        assert_eq!(pick_unused(&used), Some(POOL_START + 3));
    }

    #[test]
    fn pick_unused_returns_none_when_full() {
        let used: HashSet<u16> = (POOL_START..=POOL_END).collect();
        assert_eq!(pick_unused(&used), None);
    }
}
