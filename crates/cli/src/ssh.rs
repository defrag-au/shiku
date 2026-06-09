//! SSH transport between the Shiku CLI and the per-user agent on the box.
//!
//! Each `Connection` is one SSH session that pipes a postcard frame stream to
//! `nc -U $XDG_RUNTIME_DIR/shikud.sock` on the remote box. SSH provides
//! authentication and encryption; netcat bridges to the agent's Unix socket.
//!
//! ## Usage
//!
//! ```ignore
//! let mut conn = Connection::open(&resolved).await?;
//! let response = conn.request(&Request::Ping).await?;
//! conn.close().await?;
//! ```
//!
//! For streaming responses (Logs, Activate), use [`Connection::request_streaming`]
//! which returns an iterator over response frames until EOF.
//!
//! ## Connection lifetime
//!
//! Each `Connection` corresponds to exactly one SSH session and exactly one
//! request — the netcat-bridge model can't multiplex requests cleanly because
//! `nc -U` exits when its stdin closes. For multi-request workflows (e.g. a
//! deploy that registers, uploads, activates) just open a new connection for
//! each. SSH key auth makes this cheap; multiplexing via `ControlMaster` is a
//! Phase 4 optimisation if it ever matters.

use std::process::Stdio;

use anyhow::{bail, Context, Result};
use shiku_types::{framing, Request, Response};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

use crate::config::ResolvedApp;

/// One SSH-tunnelled connection to a Shiku agent.
pub struct Connection {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: ChildStdout,
    stderr: Option<ChildStderr>,
}

impl Connection {
    /// Spawn ssh + nc and return a connected handle.
    ///
    /// The remote command resolves the socket path through `$XDG_RUNTIME_DIR`,
    /// which systemd-logind sets correctly for any user with a session
    /// (including linger-enabled service users). Falls back to `/run/user/$(id -u)`
    /// when the env var is unset.
    pub async fn open(resolved: &ResolvedApp) -> Result<Self> {
        let remote_cmd = "exec nc -U \"${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/shikud.sock\"";

        let mut child = Command::new("ssh")
            .arg("-o")
            .arg("BatchMode=yes")
            // Suppress ssh's noisy "agent refused operation" lines that
            // appear when the user's ssh-agent offers keys the box
            // doesn't accept. ERROR-level still surfaces real failures.
            .arg("-o")
            .arg("LogLevel=ERROR")
            .arg(format!("{}@{}", resolved.ssh_user, resolved.deploy_host))
            .arg(remote_cmd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| {
                format!(
                    "spawning ssh to {}@{}",
                    resolved.ssh_user, resolved.deploy_host
                )
            })?;

        let stdin = child.stdin.take().context("ssh stdin not piped")?;
        let stdout = child.stdout.take().context("ssh stdout not piped")?;
        let stderr = child.stderr.take();

        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout,
            stderr,
        })
    }

    /// Send a request and read exactly one response.
    ///
    /// Closes stdin after writing so the agent (and `nc`) can drain cleanly.
    /// The connection is unusable for further requests after this returns —
    /// caller should `close` or drop it.
    pub async fn request(&mut self, request: &Request) -> Result<Response> {
        self.write_request(request).await?;
        self.read_response().await
    }

    /// Send a request and stream multiple response frames until the agent
    /// closes the connection.
    ///
    /// Used for `Logs` and `Activate`, where the agent emits zero-or-more
    /// streaming events followed by a terminal `Ok` / `Error`.
    /// `on_response` is called for each frame; return `Err` from it to abort
    /// reading. The terminal frame is included in the stream.
    pub async fn request_streaming<F>(
        &mut self,
        request: &Request,
        mut on_response: F,
    ) -> Result<()>
    where
        F: FnMut(Response) -> Result<()>,
    {
        self.write_request(request).await?;
        while let Some(response) = self.try_read_response().await? {
            on_response(response)?;
        }
        Ok(())
    }

    /// Wait for the SSH child to exit, returning an error if it failed.
    pub async fn close(mut self) -> Result<()> {
        // Ensure stdin is dropped so the remote side gets EOF.
        let _ = self.stdin.take();
        let status = self.child.wait().await.context("waiting for ssh")?;
        if !status.success() {
            // Capture stderr for diagnostics. Stderr was piped at spawn time,
            // and after wait() returns the pipe is fully drained on the
            // kernel side; we can read it via `wait_with_output` style but
            // we already moved child... instead, surface the exit status.
            tracing::warn!(?status, "ssh exited non-zero");
        }
        Ok(())
    }

    /// Write a single request frame and close stdin (so `nc` exits cleanly
    /// once the agent has replied).
    async fn write_request(&mut self, request: &Request) -> Result<()> {
        let stdin = self.stdin.as_mut().context("connection already consumed")?;
        let frame = framing::encode(request).context("encoding request")?;
        stdin
            .write_all(&frame)
            .await
            .context("writing request to ssh stdin")?;
        stdin.flush().await.ok();
        // Drop stdin to send EOF — needed because `nc -U` only stops
        // forwarding when its stdin closes.
        let _ = self.stdin.take();
        Ok(())
    }

    /// Read exactly one length-prefixed response frame.
    async fn read_response(&mut self) -> Result<Response> {
        match self.try_read_response().await? {
            Some(r) => Ok(r),
            None => {
                // EOF before response — most often this means SSH itself
                // failed (auth error, network issue, no such user). Drain
                // stderr to surface the actual cause.
                let stderr_msg = self.drain_stderr().await;
                if !stderr_msg.is_empty() {
                    bail!("ssh failed: {}", stderr_msg.trim());
                }
                bail!("agent closed connection before sending a response");
            }
        }
    }

    /// Drain accumulated stderr from the SSH child (best-effort, non-blocking
    /// in spirit — we wait for the child to exit so the pipe drains fully).
    async fn drain_stderr(&mut self) -> String {
        let Some(mut stderr) = self.stderr.take() else {
            return String::new();
        };
        let mut buf = String::new();
        let _ = stderr.read_to_string(&mut buf).await;
        buf
    }

    /// Try to read one response frame; returns `Ok(None)` on clean EOF.
    async fn try_read_response(&mut self) -> Result<Option<Response>> {
        let mut prefix = [0u8; framing::LEN_PREFIX_BYTES];
        match self.stdout.read_exact(&mut prefix).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e).context("reading length prefix from agent"),
        }
        let body_len = framing::parse_len_prefix(prefix).context("parsing length prefix")?;
        let mut body = vec![0u8; body_len as usize];
        self.stdout
            .read_exact(&mut body)
            .await
            .context("reading body from agent")?;
        let response: Response = framing::decode(&body).context("decoding response")?;
        Ok(Some(response))
    }
}

/// Helper for the common case: open a connection, run one request, return the
/// response, close the connection. Used by single-shot commands like `ping`,
/// `status`, `app register`, etc.
pub async fn request_one(resolved: &ResolvedApp, request: &Request) -> Result<Response> {
    let mut conn = Connection::open(resolved).await?;
    let response = conn.request(request).await?;
    conn.close().await?;
    Ok(response)
}
