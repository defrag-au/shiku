//! Status reporting for `Request::Status`.
//!
//! Reports the runtime state of one or all registered apps:
//!   - systemd unit state (active / inactive / failed / activating / etc.)
//!   - Currently activated release sha (from the `current` symlink target).
//!   - Previously activated sha (from the `previous` symlink, if present).
//!   - Last-activated timestamp (mtime of `current`'s symlink — proxy for
//!     when the swap happened).
//!   - Approximate uptime in seconds (from systemd's `ActiveEnterTimestamp`).

use std::path::PathBuf;
use std::time::SystemTime;

use anyhow::{Context, Result};
use shiku_types::{AppName, AppStatus, ReleaseSha};
use tokio::process::Command;

use crate::apps;

/// Build status snapshots for one or all apps.
pub async fn collect(filter: Option<&AppName>) -> Result<Vec<AppStatus>> {
    let names = match filter {
        Some(n) => vec![n.clone()],
        // Skip the __shiku__ pseudo-app: it has no service to report on.
        None => apps::list_user_visible().context("listing apps")?,
    };

    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let status = build_status(&name).await?;
        out.push(status);
    }
    Ok(out)
}

async fn build_status(app: &AppName) -> Result<AppStatus> {
    let app_root = app_root(app)?;

    // Registered? If config.toml is missing, this whole row is meaningful
    // mostly as "registered = false". This shouldn't normally happen — the
    // caller filters via `apps::list()` — but the explicit Status request
    // for a single app might hit it.
    let registered = app_root.join("config.toml").is_file();

    let (current_sha, last_activated_at) = read_symlink_target(&app_root.join("current"));
    let (previous_sha, _) = read_symlink_target(&app_root.join("previous"));

    let service = if registered {
        apps::inspect(app)
            .map(|c| c.service)
            .unwrap_or_else(|_| app.clone())
    } else {
        app.clone()
    };

    let (systemd_state, uptime_secs) = systemd_state(&service).await;

    Ok(AppStatus {
        name: app.clone(),
        registered,
        current_sha,
        previous_sha,
        systemd_state,
        uptime_secs,
        last_activated_at,
    })
}

/// Read a symlink target. Returns (sha, mtime-as-rfc3339).
///
/// The target is a relative `releases/<sha>` path; we extract the trailing
/// component as the sha. The link's mtime is a proxy for "when the swap
/// happened" — atomic-rename preserves mtime predictably.
fn read_symlink_target(path: &std::path::Path) -> (Option<ReleaseSha>, Option<String>) {
    if !path.is_symlink() {
        return (None, None);
    }
    let target = std::fs::read_link(path).ok();
    let sha = target
        .as_ref()
        .and_then(|t| t.file_name())
        .and_then(|n| n.to_str())
        .map(|s| s.to_string());

    // Use lstat so we get the symlink's own mtime, not the target's.
    let mtime = std::fs::symlink_metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .map(format_rfc3339);

    (sha, mtime)
}

/// Query systemd for the unit's current state and uptime.
async fn systemd_state(service: &str) -> (String, Option<u64>) {
    let unit = format!("{service}.service");

    // ActiveState gives `active` / `inactive` / `failed` / `activating`.
    // SubState gives more detail (`running` / `dead` / `failed` / `auto-restart`).
    // Combine for a useful "active (running)" / "inactive (dead)" form.
    let output = Command::new("systemctl")
        .args([
            "--user",
            "show",
            &unit,
            "-p",
            "ActiveState",
            "-p",
            "SubState",
            "-p",
            "ActiveEnterTimestampMonotonic",
        ])
        .output()
        .await;

    let raw = match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return ("unknown".to_string(), None),
    };

    let mut active_state = "unknown".to_string();
    let mut sub_state = String::new();
    let mut active_enter_us: Option<u64> = None;
    for line in raw.lines() {
        if let Some(v) = line.strip_prefix("ActiveState=") {
            active_state = v.to_string();
        } else if let Some(v) = line.strip_prefix("SubState=") {
            sub_state = v.to_string();
        } else if let Some(v) = line.strip_prefix("ActiveEnterTimestampMonotonic=") {
            active_enter_us = v.parse().ok().filter(|n| *n > 0);
        }
    }

    let state = if sub_state.is_empty() || sub_state == active_state {
        active_state
    } else {
        format!("{active_state} ({sub_state})")
    };

    let uptime_secs = active_enter_us.and_then(monotonic_age_secs);

    (state, uptime_secs)
}

/// Compute "now − monotonic-timestamp" in seconds. systemd reports
/// `ActiveEnterTimestampMonotonic` in microseconds since system boot.
fn monotonic_age_secs(active_enter_us: u64) -> Option<u64> {
    let mut info: libc_sysinfo::Sysinfo = libc_sysinfo::Sysinfo::default();
    if libc_sysinfo::sysinfo(&mut info).is_err() {
        return None;
    }
    let now_us = info.uptime_secs.checked_mul(1_000_000)?;
    if now_us < active_enter_us {
        return None;
    }
    Some((now_us - active_enter_us) / 1_000_000)
}

fn app_root(app: &AppName) -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join("apps").join(app))
}

fn format_rfc3339(t: SystemTime) -> String {
    humantime::format_rfc3339_seconds(t).to_string()
}

/// Tiny inline wrapper around `libc::sysinfo` so we don't need another crate.
mod libc_sysinfo {
    use std::io;

    #[derive(Default)]
    pub struct Sysinfo {
        pub uptime_secs: u64,
    }

    pub fn sysinfo(out: &mut Sysinfo) -> io::Result<()> {
        // /proc/uptime is "<uptime_seconds> <idle_seconds>"; first field is
        // monotonic uptime. Avoids needing the `libc` crate or unsafe FFI.
        let s = std::fs::read_to_string("/proc/uptime")?;
        let first = s.split_whitespace().next().unwrap_or("0");
        let secs: f64 = first.parse().unwrap_or(0.0);
        out.uptime_secs = secs.max(0.0) as u64;
        Ok(())
    }
}
