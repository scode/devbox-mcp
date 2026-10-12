//! Manual test driver: starts devbox-mcp over stdio, calls tools on it, and prints the raw results as JSON.
//!
//! This is how a person or a coding agent pokes at a real devbox-mcp from a shell, with the same transport the OpenAI
//! tunnel client uses, but without the tunnel or a chat client in the loop. See docs/testing.md for when to use it.
//!
//! ```text
//! cargo build
//! cargo run --example mcp-call -- --list
//! cargo run --example mcp-call -- --server-arg=--workdir-root=/tmp/devbox-manual \
//!     run_turn '{"prompt": "Say hello world and nothing else."}'
//! ```
//!
//! Several `TOOL JSON` pairs run in order against one server process. That matters once tools depend on in-process
//! state (a turn started by one call and read by the next): separate invocations of this driver get separate servers.

use std::path::PathBuf;

use anyhow::{Context as _, bail};
use clap::Parser;
use rmcp::ServiceExt as _;
use rmcp::model::CallToolRequestParams;
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

    let mut cmd = Command::new(&args.server);
    cmd.args(&args.server_args);
    let transport = TokioChildProcess::new(cmd)
        .with_context(|| format!("cannot start {}", args.server.display()))?;
    let client = ().serve(transport).await.context("MCP initialization failed")?;

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
