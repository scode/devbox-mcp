//! Runs devbox-mcp behind a real tunnel-client, using tunnel-client's local `dev proxy` mode.
//!
//! `tunnel-client dev proxy` runs tunnel-client's actual runtime against an in-memory stand-in for OpenAI's control
//! plane, in one process, and exposes the tunnelled MCP server on a local endpoint. devbox-mcp is started through
//! `--mcp-command` exactly as in production, so everything between the control plane and devbox-mcp is the real
//! tunnel-client code: request forwarding, its one-at-a-time stdio serialization, response deadlines, and the child
//! process lifecycle. What it cannot cover is OpenAI's hosted relay, authentication, and the ChatGPT client.
//!
//! The proxy's MCP endpoint listens on a Unix socket inside a caller-provided scratch directory, never on a network
//! port. Listening is fine here because this is local test tooling, not devbox-mcp itself (see SPEC_impl.md, "Testing").
//!
//! This file is shared by the integration tests and by `examples/mcp-call.rs` (through `#[path]`), so it depends on
//! nothing else in `tests/support`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use rmcp::ServiceExt as _;
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use serde::Deserialize;
use tokio::process::{Child, Command};

/// Where `scripts/install-tunnel-client.sh` puts the pinned tunnel-client.
pub fn default_tunnel_client() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("target/tools/bin/tunnel-client")
}

/// The connection details `dev proxy --url-file` writes once tunnel-client and the MCP server are ready.
#[derive(Debug, Deserialize)]
struct ConnectionInfo {
    mcp_unix_socket: String,
    mcp_url_path: String,
}

/// A running `tunnel-client dev proxy` with devbox-mcp behind it.
///
/// Dropping it SIGKILLs the proxy, which devbox-mcp sees as its stdin closing. To exercise tunnel-client's own orderly
/// shutdown instead, send SIGTERM to [`DevProxy::pid`] and then [`DevProxy::wait`].
pub struct DevProxy {
    child: Child,
    info: ConnectionInfo,
}

impl DevProxy {
    /// Starts the proxy and waits until it reports ready.
    ///
    /// `devbox_mcp` and `devbox_args` form the command tunnel-client launches. `scratch` must be an existing directory
    /// the caller owns; the proxy's socket, connection file, and log go there. `response_timeout` is how long the proxy
    /// waits for devbox-mcp to answer one request, standing in for the hosted relay's deadline.
    ///
    /// `env` is the *entire* environment of tunnel-client, and therefore of devbox-mcp and the agent below it. Nothing is
    /// inherited, because tunnel-client takes configuration (profiles, PID files, control-plane settings) from
    /// environment variables, and a developer's real tunnel setup must not leak into a test.
    pub async fn start(
        tunnel_client: &Path,
        devbox_mcp: &Path,
        devbox_args: &[String],
        scratch: &Path,
        response_timeout: Duration,
        env: &[(String, std::ffi::OsString)],
    ) -> anyhow::Result<Self> {
        if !tunnel_client.exists() {
            bail!(
                "tunnel-client not found at {}; run scripts/install-tunnel-client.sh (see docs/testing.md)",
                tunnel_client.display()
            );
        }
        let url_file = scratch.join("dev-proxy.json");
        let log = scratch.join("dev-proxy.log");
        let mut command_line = quote_arg(devbox_mcp.to_str().context("non-UTF-8 devbox-mcp path")?);
        for arg in devbox_args {
            command_line.push(' ');
            command_line.push_str(&quote_arg(arg));
        }

        let mut child = Command::new(tunnel_client)
            .env_clear()
            .envs(env.iter().map(|(k, v)| (k, v)))
            .args(["dev", "proxy", "--mcp-command"])
            .arg(&command_line)
            .arg("--listen-unix-socket")
            .arg(scratch.join("mcp.sock"))
            .arg("--url-file")
            .arg(&url_file)
            .arg("--response-timeout")
            .arg(format!("{}ms", response_timeout.as_millis()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log)?)
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("cannot start {}", tunnel_client.display()))?;

        // The connection file appears once the proxy is ready. It is polled rather than trusted on first sight because
        // nothing promises it is written atomically.
        let deadline = Instant::now() + Duration::from_secs(20);
        let info = loop {
            if let Some(status) = child.try_wait()? {
                let log = std::fs::read_to_string(&log).unwrap_or_default();
                bail!("tunnel-client dev proxy exited with {status} before becoming ready:\n{log}");
            }
            if let Ok(text) = std::fs::read_to_string(&url_file)
                && let Ok(info) = serde_json::from_str::<ConnectionInfo>(&text)
            {
                break info;
            }
            if Instant::now() > deadline {
                bail!(
                    "tunnel-client dev proxy did not become ready; log at {}",
                    log.display()
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        Ok(Self { child, info })
    }

    /// Connects an MCP client to devbox-mcp through the proxy and completes initialization.
    pub async fn connect(&self) -> anyhow::Result<RunningService<RoleClient, ()>> {
        let transport = StreamableHttpClientTransport::from_unix_socket(
            &self.info.mcp_unix_socket,
            format!("http://localhost{}", self.info.mcp_url_path),
        );
        Ok(().serve(transport).await?)
    }

    /// The proxy's PID, for tests that signal it.
    pub fn pid(&self) -> u32 {
        self.child.id().expect("dev proxy already reaped")
    }

    /// Waits for the proxy to exit, up to `timeout`.
    pub async fn wait(&mut self, timeout: Duration) -> anyhow::Result<std::process::ExitStatus> {
        Ok(tokio::time::timeout(timeout, self.child.wait())
            .await
            .context("tunnel-client dev proxy did not exit")??)
    }
}

/// Quotes one argument for tunnel-client's `--mcp-command`, which splits its value with shell-like quoting rules but
/// never runs a shell. Inside single quotes everything is literal, so only `'` itself needs the `'\''` dance.
fn quote_arg(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', r"'\''"))
}
