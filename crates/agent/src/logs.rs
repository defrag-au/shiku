//! Streaming log handler for `Request::Logs`.
//!
//! Spawns `journalctl --user -u <unit>` with optional `--since` and `-f`
//! (follow), reads stdout line-by-line, and emits each line as a
//! `Response::LogLine` frame. The connection stays open for the lifetime of
//! the journalctl process; when the client closes its end of the SSH+nc
//! tunnel, the write fails and we kill the child.

use std::process::Stdio;

use anyhow::{Context, Result};
use shiku_types::{framing, AppName, Response};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use crate::apps;

/// Stream `journalctl` output back to the caller via the open Unix socket
/// stream. Returns when the journalctl process exits or the writer fails.
pub async fn stream<W>(
    app: &AppName,
    since: Option<&str>,
    follow: bool,
    writer: &mut W,
) -> Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    let config = apps::inspect(app).context("app not registered")?;
    let unit = format!("{}.service", config.service);

    let mut cmd = Command::new("journalctl");
    cmd.arg("--user")
        .arg("-u")
        .arg(&unit)
        .arg("--no-pager")
        .arg("-o")
        .arg("short-iso")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(s) = since {
        cmd.arg("--since").arg(s);
    }
    if follow {
        cmd.arg("-f");
    } else {
        // Sensible default for non-follow mode: last 200 lines.
        cmd.arg("-n").arg("200");
    }

    let mut child = cmd.spawn().context("spawning journalctl")?;
    let stdout = child.stdout.take().context("journalctl stdout not piped")?;
    let mut lines = BufReader::new(stdout).lines();

    while let Some(line) = lines.next_line().await.context("reading journalctl line")? {
        // Encode as a streaming Response::LogLine frame.
        let frame = match framing::encode(&Response::LogLine(line)) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, "failed to encode log line, stopping stream");
                break;
            }
        };
        if let Err(e) = writer.write_all(&frame).await {
            // Client closed the connection — stop streaming.
            tracing::debug!(error = %e, "client closed; stopping log stream");
            break;
        }
    }

    // Make sure journalctl doesn't keep running once we're done.
    let _ = child.start_kill();
    let _ = child.wait().await;
    Ok(())
}
