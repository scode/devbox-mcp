//! End-to-end tests of the real devbox-mcp binary over plain stdio, the transport tunnel-client uses to talk to it.
//!
//! These tests play tunnel-client's role directly, which makes them fast and lets them control exactly what devbox-mcp
//! sees. tests/tunnel.rs repeats the important paths with the real tunnel-client in between.

mod support;

use std::path::Path;
use std::time::Duration;

use rmcp::ServiceExt as _;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::TokioChildProcess;
use serde_json::{Value, json};
use support::{DEVBOX_MCP, Harness, assert_all_gone, run_turn, send_signal, structured};
use tokio::process::Command;

/// Starts devbox-mcp on this harness and completes MCP initialization with it over stdio.
async fn connect(h: &Harness) -> RunningService<RoleClient, ()> {
    let mut cmd = Command::new(DEVBOX_MCP);
    cmd.args(h.devbox_args());
    ().serve(TokioChildProcess::new(cmd).unwrap())
        .await
        .unwrap()
}

/// Spec: the server advertises `run_turn`. This is the first thing any MCP client (and the tunnel's connector setup)
/// sees, so a broken tool listing breaks everything downstream.
#[tokio::test]
async fn lists_run_turn_tool() {
    let h = Harness::new();
    let client = connect(&h).await;
    let tools = client.list_all_tools().await.unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert_eq!(names, ["run_turn"]);
    client.cancel().await.unwrap();
}

/// Spec: a turn sends the prompt to the agent, runs in the default working directory when none is given, and returns
/// the agent's reply and native session ID. Also pins the security-relevant invocation details: the agent must run with
/// permissions bypassed (`SEC-4`) and receive the prompt on stdin, not on its command line.
#[tokio::test]
async fn runs_a_turn_and_returns_the_reply() {
    let h = Harness::new();
    let client = connect(&h).await;

    let result = run_turn(&client, json!({ "prompt": "say hello world" })).await;
    assert_eq!(result.is_error, Some(false));
    let out = structured(&result);
    assert_eq!(out["completed"], true);
    assert_eq!(out["reply"], "fake reply: say hello world");
    assert_eq!(out["agent_session_id"], "fake-thread-1");
    assert_eq!(out["workdir"], h.workdir_root.display().to_string());

    assert_eq!(h.recorded("stdin"), "say hello world");
    assert_eq!(
        Path::new(h.recorded("cwd").trim()),
        h.workdir_root.canonicalize().unwrap()
    );
    let argv = h.recorded("argv");
    assert!(
        argv.lines()
            .any(|a| a == "--dangerously-bypass-approvals-and-sandbox"),
        "argv: {argv}"
    );
    assert!(
        !argv.contains("say hello world"),
        "prompt leaked onto the command line: {argv}"
    );
    client.cancel().await.unwrap();
}

/// Spec (`LIF-2`): a turn the agent reports as failed is reported as not completed, with the agent's own error text,
/// rather than as a generic tool error or, worse, as success.
#[tokio::test]
async fn reports_a_failed_turn_honestly() {
    let h = Harness::new();
    let client = connect(&h).await;

    let result = run_turn(&client, json!({ "prompt": "fail please" })).await;
    let out = structured(&result);
    assert_eq!(out["completed"], false);
    assert_eq!(out["error"], "fake failure");
    client.cancel().await.unwrap();
}

/// Spec (`LIF-2`), the other direction: an `error` event that Codex follows with a normal `turn.completed` must not
/// turn a completed turn into a reported failure. Only `turn.failed`, a failed exit, or a missing completion fail a
/// turn.
#[tokio::test]
async fn an_error_event_does_not_fail_a_completed_turn() {
    let h = Harness::new();
    let client = connect(&h).await;

    let result = run_turn(&client, json!({ "prompt": "hiccup" })).await;
    let out = structured(&result);
    assert_eq!(out["completed"], true);
    assert_eq!(out["reply"], "fake reply: hiccup");
    assert_eq!(out["error"], Value::Null);
    client.cancel().await.unwrap();
}

/// A relative working directory is a caller mistake and must be rejected before any agent runs, since its meaning
/// would depend on wherever the tunnel client happened to start devbox-mcp.
#[tokio::test]
async fn rejects_relative_workdir() {
    let h = Harness::new();
    let client = connect(&h).await;

    let params = CallToolRequestParams::new("run_turn").with_arguments(
        json!({ "prompt": "hi", "workdir": "relative/dir" })
            .as_object()
            .unwrap()
            .clone(),
    );
    let err = client.call_tool(params).await.unwrap_err();
    assert!(
        err.to_string().contains("absolute"),
        "unexpected error: {err}"
    );
    assert!(
        !h.fake_dir.join("argv").exists(),
        "agent ran despite invalid workdir"
    );
    client.cancel().await.unwrap();
}

/// A devbox-mcp process driven over raw JSON-RPC, with a turn hanging in the fake agent.
///
/// The shutdown tests bypass rmcp's client because they need exact control over the two signals tunnel-client can send
/// its child (closing stdin, and SIGTERM). rmcp's client transport neither exposes the server's stdin nor reliably
/// closes it on shutdown; it falls back to SIGKILL, which is not what tunnel-client does.
struct HangingTurn {
    server: tokio::process::Child,
    stdin: Option<tokio::process::ChildStdin>,
    /// PIDs of the fake agent and of the background command it started.
    agent_pids: Vec<i32>,
}

impl HangingTurn {
    async fn start(h: &Harness) -> Self {
        use tokio::io::AsyncWriteExt as _;

        let mut server = Command::new(DEVBOX_MCP)
            .args(h.devbox_args())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut stdin = server.stdin.take().unwrap();
        // Drain stdout so the server can never block writing a response.
        let mut stdout = server.stdout.take().unwrap();
        tokio::spawn(async move { tokio::io::copy(&mut stdout, &mut tokio::io::sink()).await });

        let messages = [
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-06-18", "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"}}}),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
                "name": "run_turn", "arguments": {"prompt": "hang"}}}),
        ];
        for msg in messages {
            stdin
                .write_all(format!("{msg}\n").as_bytes())
                .await
                .unwrap();
        }
        stdin.flush().await.unwrap();

        let agent_pids = h.hanging_agent_pids().await;
        Self {
            server,
            stdin: Some(stdin),
            agent_pids,
        }
    }

    /// Asserts that devbox-mcp exits on its own, successfully (an orderly shutdown, not a crash or a kill), and that
    /// the agent and every process it started are gone.
    async fn assert_clean_exit(mut self) {
        let status = tokio::time::timeout(Duration::from_secs(10), self.server.wait())
            .await
            .expect("devbox-mcp did not exit")
            .unwrap();
        assert!(status.success(), "devbox-mcp exited with {status}");
        assert_all_gone(&self.agent_pids, "agent survived devbox-mcp shutting down").await;
    }
}

/// Spec (`LIF-1`): when tunnel-client stops devbox-mcp, devbox-mcp exits and takes the agent's processes with it,
/// including commands the agent started, so nothing keeps running on the box unobserved. This mimics tunnel-client's
/// stop sequence exactly: close the child's stdin and send it SIGTERM. Before devbox-mcp handled SIGTERM, this left the
/// agent's background commands running.
#[tokio::test]
async fn tunnel_client_stop_kills_the_agent_and_its_children() {
    let h = Harness::new();
    let mut turn = HangingTurn::start(&h).await;
    drop(turn.stdin.take());
    send_signal(turn.server.id().unwrap(), libc::SIGTERM);
    turn.assert_clean_exit().await;
}

/// Spec (`LIF-1`): if tunnel-client itself dies, devbox-mcp only sees its stdin reach end of file. That alone must be
/// enough for the same orderly shutdown.
#[tokio::test]
async fn stdin_eof_kills_the_agent_and_its_children() {
    let h = Harness::new();
    let mut turn = HangingTurn::start(&h).await;
    drop(turn.stdin.take());
    turn.assert_clean_exit().await;
}
