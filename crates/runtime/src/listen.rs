//! Listen-address resolution.
//!
//! Apps with `[apps.X.listen.http]` get `SHIKU_LISTEN_HTTP=<port>` injected
//! by the agent at activation time. This module parses it to a usable form.
//!
//! Convention: bind to `127.0.0.1` (localhost only). External traffic comes
//! in via Cloudflare Tunnel / Caddy on the box, configured separately.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use crate::{RuntimeError, RuntimeResult};

/// Env var the agent injects with the listen port.
pub const ENV_LISTEN_HTTP: &str = "SHIKU_LISTEN_HTTP";

/// Default loopback IP. Public so apps can override if they really mean to
/// bind to 0.0.0.0 (rare, and a bad idea behind a tunnel).
pub const DEFAULT_BIND_IP: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// Resolve `SHIKU_LISTEN_HTTP` into a `SocketAddr` bound to localhost.
///
/// Returns `None` if the env var isn't set — apps without a listen block
/// don't need an HTTP server, and this lets caller distinguish "no port"
/// from "bad port".
pub fn lookup_http() -> RuntimeResult<Option<SocketAddr>> {
    let raw = match std::env::var(ENV_LISTEN_HTTP) {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };
    parse_port(&raw).map(|port| Some(SocketAddr::new(DEFAULT_BIND_IP, port)))
}

/// Like [`lookup_http`] but errors if the env var is unset. Use this for
/// services that *must* have an HTTP port (otherwise their deploy config
/// is wrong).
pub fn require_http() -> RuntimeResult<SocketAddr> {
    lookup_http()?.ok_or_else(|| RuntimeError::Env {
        name: ENV_LISTEN_HTTP.into(),
        detail: "not set, but this service requires an HTTP listen port".into(),
    })
}

fn parse_port(raw: &str) -> RuntimeResult<u16> {
    raw.trim().parse::<u16>().map_err(|e| RuntimeError::Env {
        name: ENV_LISTEN_HTTP.into(),
        detail: format!("invalid port '{raw}': {e}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn lookup_returns_none_when_unset() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::remove_var(ENV_LISTEN_HTTP);
        assert!(lookup_http().unwrap().is_none());
    }

    #[test]
    fn lookup_parses_valid_port() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var(ENV_LISTEN_HTTP, "8080");
        let addr = lookup_http().unwrap().expect("present");
        assert_eq!(addr.port(), 8080);
        assert_eq!(addr.ip(), DEFAULT_BIND_IP);
        std::env::remove_var(ENV_LISTEN_HTTP);
    }

    #[test]
    fn lookup_errors_on_invalid() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var(ENV_LISTEN_HTTP, "not-a-port");
        let err = lookup_http().unwrap_err();
        assert!(matches!(err, RuntimeError::Env { .. }));
        std::env::remove_var(ENV_LISTEN_HTTP);
    }

    #[test]
    fn require_errors_on_unset() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::remove_var(ENV_LISTEN_HTTP);
        assert!(require_http().is_err());
    }
}
