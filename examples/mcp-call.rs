//! Manual test driver: starts devbox-mcp, calls tools on it, and prints the raw results as JSON.
//!
//! This is how a person or a coding agent pokes at a real devbox-mcp from a shell. By default the driver talks to
//! devbox-mcp over stdio directly. With `--via-tunnel-client`, it puts the real tunnel-client (in its local `dev proxy`
//! mode) in between, the way production does, minus OpenAI's hosted relay. See docs/testing.md for when to use which.
//!
//! ```text
//! cargo build
//! cargo run --example mcp-call -- --list \
//!     --server-arg=--workdir-root=/tmp/devbox-manual/work --server-arg=--state-dir=/tmp/devbox-manual/state \
//!     session_create '{"name": "demo"}' \
//!     session_send '{"name": "demo", "message": "Say hello world and nothing else."}' \
//!     wait_turn '{"name": "demo"}'
//! ```
//!
//! Several `TOOL JSON` pairs run in order against one server process. That matters because a turn runs inside the
//! server that started it: separate invocations of this driver get separate servers, and a turn started by one dies
//! with it. `wait_turn` is not an MCP tool but a step of this driver: it polls `session_read` once a second until the
//! session's most recent turn has ended, and prints every event and the final turn, which is what a caller of
//! devbox-mcp would do by hand.

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

/// Environment variables passed through to tunnel-client, and so to devbox-mcp and the real agent, in tunnel mode.
///
/// An allowlist rather than the full environment, because tunnel-client reads its configuration (profiles, PID files,
/// control-plane settings) from environment variables, and a developer's real tunnel setup must not leak into a local
/// test. What is left is what a real agent needs to find its tools and its login: Codex keeps its credentials under
/// `HOME` (or `CODEX_HOME`), and Claude Code under `HOME` (or `CLAUDE_CONFIG_DIR`).
const AGENT_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "TMPDIR",
    "CODEX_HOME",
    "CLAUDE_CONFIG_DIR",
];

fn agent_env() -> Vec<(String, std::ffi::OsString)> {
    AGENT_ENV_ALLOWLIST
        .iter()
        .filter_map(|&name| std::env::var_os(name).map(|value| (name.to_owned(), value)))
        .collect()
}

/// The driver's `wait_turn` step: polls `session_read` for the named session until its most recent turn has ended
/// and no more events are waiting, then returns all events seen and the final turn.
async fn wait_turn(
    client: &RunningService<RoleClient, ()>,
    arguments: serde_json::Map<String, Value>,
) -> Value {
    let mut cursor: Option<Value> = None;
    let mut events = Vec::new();
    loop {
        let mut args = arguments.clone();
        if let Some(c) = &cursor {
            args.insert("cursor".to_owned(), c.clone());
        }
        let read = match client
            .call_tool(CallToolRequestParams::new("session_read").with_arguments(args))
            .await
        {
            Ok(result) => result.structured_content.unwrap_or_default(),
            Err(err) => return json!({ "tool": "wait_turn", "error": err.to_string() }),
        };
        events.extend(read["events"].as_array().cloned().unwrap_or_default());
        cursor = Some(read["next_cursor"].clone());
        let state = read["turn"]["state"].as_str().unwrap_or_default();
        if !matches!(state, "running" | "interrupting") && read["more"] == false {
            return json!({ "tool": "wait_turn", "events": events, "turn": read["turn"] });
        }
        if read["more"] != true {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
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
            &agent_env(),
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
        if tool == "wait_turn" {
            let printed = wait_turn(&client, arguments).await;
            failed |= printed.get("error").is_some();
            println!("{}", serde_json::to_string_pretty(&printed)?);
            continue;
        }
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
