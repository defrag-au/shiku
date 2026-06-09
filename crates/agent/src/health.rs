//! HTTP health-check probing for `HealthSpec::Http`.
//!
//! Hand-rolled minimal HTTP/1.1 client. Justification: we only ever talk to
//! `127.0.0.1:<port>` from the agent (services are localhost-only; public
//! routing happens out of band via Cloudflare Tunnel / Caddy). Pulling in a
//! full HTTP client crate (reqwest, hyper) for a single GET to localhost
//! would dwarf this code in dep weight.
//!
//! The probe sends `GET <path> HTTP/1.1\r\nHost: localhost:<port>\r\nConnection: close\r\n\r\n`
//! and parses the status line. Any 2xx / 3xx response counts as healthy by
//! default; explicit `expect_status` overrides.

use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Wait until the HTTP probe succeeds, or timeout.
///
/// Polls `http://127.0.0.1:<port><path>` every `interval_secs`, after an
/// initial delay of `initial_delay_secs`. Returns `Ok(())` on the first
/// successful probe; `Err` if the cumulative wait exceeds `timeout_secs`.
pub async fn wait_for_http(
    port: u16,
    path: &str,
    expect_status: &[u16],
    initial_delay_secs: u32,
    interval_secs: u32,
    timeout_secs: u32,
) -> Result<()> {
    if initial_delay_secs > 0 {
        tokio::time::sleep(Duration::from_secs(u64::from(initial_delay_secs))).await;
    }

    let deadline = Instant::now() + Duration::from_secs(u64::from(timeout_secs));
    let interval = Duration::from_secs(u64::from(interval_secs.max(1)));
    let mut attempt: u32 = 0;
    let mut last_err: Option<anyhow::Error> = None;

    while Instant::now() < deadline {
        attempt += 1;
        match probe_once(port, path).await {
            Ok(status) if is_acceptable(status, expect_status) => {
                tracing::debug!(attempt, status, "health check passed");
                return Ok(());
            }
            Ok(status) => {
                last_err = Some(anyhow!("got HTTP {status}"));
                tracing::debug!(attempt, status, "health check rejected status");
            }
            Err(e) => {
                tracing::debug!(attempt, error = %e, "health probe failed");
                last_err = Some(e);
            }
        }
        tokio::time::sleep(interval).await;
    }

    bail!(
        "health check timed out after {timeout_secs}s ({} attempts; last error: {})",
        attempt,
        last_err
            .as_ref()
            .map(|e| format!("{e:#}"))
            .unwrap_or_else(|| "none".into())
    );
}

fn is_acceptable(status: u16, expect: &[u16]) -> bool {
    if expect.is_empty() {
        // Default: any 2xx or 3xx.
        (200..=399).contains(&status)
    } else {
        expect.contains(&status)
    }
}

/// Single HTTP GET against localhost, returning the status code.
///
/// 5-second connection + read deadline. We don't read the body — once we
/// have the status line, the connection gets closed.
async fn probe_once(port: u16, path: &str) -> Result<u16> {
    let mut stream = tokio::time::timeout(
        Duration::from_secs(5),
        TcpStream::connect(("127.0.0.1", port)),
    )
    .await
    .context("connect timed out")?
    .with_context(|| format!("connecting to 127.0.0.1:{port}"))?;

    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\nAccept: */*\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .await
        .context("writing request")?;
    stream.flush().await.ok();

    // Read enough of the response to find the status line. We don't need the
    // body. Cap at 1 KiB to bound memory.
    let mut buf = [0u8; 1024];
    let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
        .await
        .context("read timed out")?
        .context("reading response")?;
    if n == 0 {
        bail!("empty response");
    }

    parse_status_line(&buf[..n])
}

/// Parse the HTTP/1.1 status line from a response prefix.
///
/// Looks for `HTTP/1.<x> NNN ...` at the start of the buffer.
fn parse_status_line(buf: &[u8]) -> Result<u16> {
    // First CRLF terminates the status line. If the buffer doesn't have a
    // CRLF yet, work with whatever ASCII prefix we can.
    let line_end = buf.iter().position(|&b| b == b'\n').unwrap_or(buf.len());
    let line = std::str::from_utf8(&buf[..line_end]).context("non-UTF8 status line")?;

    let mut parts = line.split_whitespace();
    let version = parts
        .next()
        .ok_or_else(|| anyhow!("status line missing version: '{line}'"))?;
    if !version.starts_with("HTTP/") {
        bail!("not an HTTP response (line: '{line}')");
    }
    let status = parts
        .next()
        .ok_or_else(|| anyhow!("status line missing code: '{line}'"))?;
    status
        .parse::<u16>()
        .with_context(|| format!("parsing status code from '{status}'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ok_status() {
        let buf = b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n";
        assert_eq!(parse_status_line(buf).unwrap(), 200);
    }

    #[test]
    fn parses_500_with_reason() {
        let buf = b"HTTP/1.1 500 Internal Server Error\r\n";
        assert_eq!(parse_status_line(buf).unwrap(), 500);
    }

    #[test]
    fn parses_204_no_body() {
        let buf = b"HTTP/1.1 204 No Content\r\n";
        assert_eq!(parse_status_line(buf).unwrap(), 204);
    }

    #[test]
    fn rejects_non_http() {
        let buf = b"GIBBERISH 200\r\n";
        assert!(parse_status_line(buf).is_err());
    }

    #[test]
    fn acceptable_defaults_to_2xx_3xx() {
        assert!(is_acceptable(200, &[]));
        assert!(is_acceptable(204, &[]));
        assert!(is_acceptable(301, &[]));
        assert!(!is_acceptable(404, &[]));
        assert!(!is_acceptable(500, &[]));
    }

    #[test]
    fn acceptable_explicit_list() {
        assert!(is_acceptable(200, &[200, 204]));
        assert!(is_acceptable(204, &[200, 204]));
        assert!(!is_acceptable(301, &[200, 204]));
    }

    /// Smoke test the full probe loop against a tokio listener that returns
    /// `200 OK`. Proves the connect → write request → read status path works.
    #[tokio::test]
    async fn probes_real_server() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // One-shot response server. Accepts one connection, writes a valid
        // HTTP/1.1 response, drops the stream.
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                .await;
            let _ = stream.shutdown().await;
        });

        let result = wait_for_http(port, "/health", &[], 0, 1, 5).await;
        assert!(result.is_ok(), "probe should succeed: {result:?}");
    }

    /// Server returns 503 — probe should report timeout (since we never get
    /// an acceptable status).
    #[tokio::test]
    async fn rejects_unacceptable_status() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // Loop accepting and returning 503 until the probe gives up.
        tokio::spawn(async move {
            loop {
                if let Ok((mut stream, _)) = listener.accept().await {
                    let _ = stream.write_all(
                        b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                    )
                    .await;
                    let _ = stream.shutdown().await;
                }
            }
        });

        // 1s interval, 2s timeout — should give us at least one rejected
        // probe before timing out.
        let result = wait_for_http(port, "/health", &[200], 0, 1, 2).await;
        assert!(result.is_err(), "should timeout on persistent 503");
        let err = format!("{:#}", result.unwrap_err());
        assert!(err.contains("HTTP 503") || err.contains("timed out"));
    }
}
