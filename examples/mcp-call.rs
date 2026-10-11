//! Manual test driver: starts devbox-mcp, calls tools on it, and prints the raw results as JSON.
//!
//! This is how a person or a coding agent pokes at a real devbox-mcp from a shell. By default the driver talks to
//! devbox-mcp over stdio directly. With `--via-tunnel-client`, it puts the real tunnel-client (in its local `dev proxy`
//! mode) in between, the way production does, minus OpenAI's hosted relay. See docs/testing.md for when to use which.
//!
//! ```text
//! cargo build
//! cargo run --example mcp-call -- --list \
//!     --server-arg=--workdir-root=/tmp/devbox-manual \
//!     run_turn '{"prompt": "Say hello world and nothing else."}'
//! ```
//!
//! Several `TOOL JSON` pairs run in order against one server process. That matters once tools depend on in-process
//! state (a turn started by one call and read by the next): separate invocations of this driver get separate servers.

// Shared with the integration tests, which is why it lives under tests/. The driver uses only part of it.
#[allow(dead_code)]
#[path = "../tests/support/dev_proxy.rs"]
mod dev_proxy;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, bail};
use clap::Parser;
use rmcp::ServiceExt as _;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::TokioChildProcess;
use serde_json::{Value, json};
use tokio::process::Command;

#[derive(Debug, Parser)]
struct Args {
    /// The devbox-mcp binary to start. Build it first; this driver does not.
    #[arg(long, default_value = "target/debug/devbox-mcp")]
    server: PathBuf,

    /// An argument to pass to devbox-mcp. Repeatable; use the `--server-arg=--flag=value` form for flags.
    #[arg(long = "server-arg", allow_hyphen_values = true)]
    server_args: Vec<String>,

    /// Route calls through tunnel-client's local dev proxy instead of talking stdio directly.
    #[arg(long)]
    via_tunnel_client: bool,

    /// The tunnel-client binary for --via-tunnel-client. Defaults to the one scripts/install-tunnel-client.sh installs.
    #[arg(long)]
    tunnel_client: Option<PathBuf>,

    /// With --via-tunnel-client, how long the proxy waits for devbox-mcp to answer one call before giving up, in
    /// seconds. Stands in for the hosted relay's deadline.
    #[arg(long, default_value_t = 30)]
    response_timeout_secs: u64,

    /// Print the server's tool list (names, descriptions, and schemas) before any calls.
    #[arg(long)]
    list: bool,

    /// Tool calls as alternating tool names and JSON argument objects.
    calls: Vec<String>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if args.calls.len() % 2 != 0 {
        bail!("calls must be TOOL JSON pairs");
    }
    let mut calls = Vec::new();
    for pair in args.calls.chunks(2) {
        let arguments: Value =
            serde_json::from_str(&pair[1]).with_context(|| format!("bad JSON for {}", pair[0]))?;
        let Value::Object(arguments) = arguments else {
            bail!("arguments for {} must be a JSON object", pair[0])
        };
        calls.push((pair[0].clone(), arguments));
    }

    // Held until the end of main: dropping either one stops the proxy and, with it, devbox-mcp.
    let scratch = tempfile::TempDir::new()?;
    let mut _proxy = None;
    let client: RunningService<RoleClient, ()> = if args.via_tunnel_client {
        // tunnel-client starts devbox-mcp itself, from a directory of its choosing, so the path must be absolute.
        let server = std::path::absolute(&args.server)?;
        let tunnel_client = args
            .tunnel_client
            .clone()
            .unwrap_or_else(dev_proxy::default_tunnel_client);
        let proxy = dev_proxy::DevProxy::start(
            &tunnel_client,
            &server,
            &args.server_args,
            scratch.path(),
            Duration::from_secs(args.response_timeout_secs),
        )
        .await?;
        let client = proxy.connect().await?;
        _proxy = Some(proxy);
        client
    } else {
        let mut cmd = Command::new(&args.server);
        cmd.args(&args.server_args);
        let transport = TokioChildProcess::new(cmd)
            .with_context(|| format!("cannot start {}", args.server.display()))?;
        ().serve(transport)
            .await
            .context("MCP initialization failed")?
    };

    if args.list {
        let tools = client.list_all_tools().await?;
        println!("{}", serde_json::to_string_pretty(&tools)?);
    }

    let mut failed = false;
    for (tool, arguments) in calls {
        let result = client
            .call_tool(CallToolRequestParams::new(tool.clone()).with_arguments(arguments))
            .await;
        let printed = match result {
            Ok(result) => {
                failed |= result.is_error == Some(true);
                json!({ "tool": tool, "result": result })
            }
            Err(err) => {
                failed = true;
                json!({ "tool": tool, "error": err.to_string() })
            }
        };
        println!("{}", serde_json::to_string_pretty(&printed)?);
    }

    client.cancel().await?;
    if failed {
        bail!("at least one call failed");
    }
    Ok(())
}
