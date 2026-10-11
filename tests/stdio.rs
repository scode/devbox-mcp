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
use support::{
    DEVBOX_MCP, Harness, assert_all_gone, hermetic_env, run_turn, send_signal, structured,
};
use tokio::process::Command;

/// Starts devbox-mcp on this harness and completes MCP initialization with it over stdio.
async fn connect(h: &Harness) -> RunningService<RoleClient, ()> {
    let mut cmd = Command::new(DEVBOX_MCP);
    cmd.env_clear()
        .envs(hermetic_env(h.tmp.path()))
        .args(h.devbox_args());
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

/// Spec (`TRN-*`, robustness): a large prompt is delivered even when the agent writes a lot of output before reading
/// its input. If devbox-mcp wrote the whole prompt before draining output, both sides would block on full pipes.
#[tokio::test]
async fn large_prompt_does_not_deadlock_against_early_output() {
    let h = Harness::new();
    std::fs::write(h.fake_dir.join("noisy"), "").unwrap();
    let client = connect(&h).await;

    let prompt = "x".repeat(1024 * 1024);
    let result = tokio::time::timeout(
        Duration::from_secs(20),
        run_turn(&client, json!({ "prompt": prompt })),
    )
    .await
    .expect("turn deadlocked");
    assert_eq!(structured(&result)["completed"], true);
    assert_eq!(h.recorded("stdin").len(), prompt.len());
    client.cancel().await.unwrap();
}

/// Spec (`LIF-1`, `BND-1`): a turn ends when the agent exits, even if something it left running in the background
/// still holds its output pipes open, and that leftover process is killed.
#[tokio::test]
async fn background_leftovers_neither_hang_the_turn_nor_survive_it() {
    let h = Harness::new();
    let client = connect(&h).await;

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        run_turn(&client, json!({ "prompt": "linger" })),
    )
    .await
    .expect("turn waited for a background process's pipes");
    assert_eq!(structured(&result)["completed"], true);
    let lingering: i32 = h.recorded("linger").trim().parse().unwrap();
    assert_all_gone(&[lingering], "background process outlived its turn").await;
    client.cancel().await.unwrap();
}

/// A leftover that escaped into its own session cannot be killed once the agent has exited (SPEC_impl.md documents
/// this gap), but it must still not hold the turn open: devbox-mcp stops reading the pipes after a short grace period.
#[tokio::test]
async fn detached_leftovers_do_not_hang_the_turn() {
    let h = Harness::new();
    let client = connect(&h).await;

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        run_turn(&client, json!({ "prompt": "linger-detached" })),
    )
    .await
    .expect("turn waited for a detached process's pipes");
    assert_eq!(structured(&result)["completed"], true);
    let lingering: i32 = h.recorded("linger").trim().parse().unwrap();
    send_signal(lingering as u32, libc::SIGKILL);
    client.cancel().await.unwrap();
}

/// A devbox-mcp process driven over raw JSON-RPC.
///
/// The shutdown and cancellation tests bypass rmcp's client because they need exact control over what devbox-mcp sees:
/// whether its stdin is closed, which signals arrive, and when a cancellation notification is sent. rmcp's client
/// transport neither exposes the server's stdin nor reliably closes it on shutdown; it falls back to SIGKILL, which is
/// not what tunnel-client does.
struct RawServer {
    server: tokio::process::Child,
    stdin: Option<tokio::process::ChildStdin>,
    stdout: tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
}

impl RawServer {
    fn start(h: &Harness) -> Self {
        use tokio::io::AsyncBufReadExt as _;

        let mut server = Command::new(DEVBOX_MCP)
            .env_clear()
            .envs(hermetic_env(h.tmp.path()))
            .args(h.devbox_args())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = server.stdin.take();
        let stdout = tokio::io::BufReader::new(server.stdout.take().unwrap()).lines();
        Self {
            server,
            stdin,
            stdout,
        }
    }

    async fn send(&mut self, msg: Value) {
        use tokio::io::AsyncWriteExt as _;
        let stdin = self.stdin.as_mut().unwrap();
        stdin
            .write_all(format!("{msg}\n").as_bytes())
            .await
            .unwrap();
        stdin.flush().await.unwrap();
    }

    /// Reads the next message from devbox-mcp.
    async fn recv(&mut self) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(10), self.stdout.next_line())
            .await
            .expect("no response from devbox-mcp")
            .unwrap()
            .expect("devbox-mcp closed stdout");
        serde_json::from_str(&line).unwrap()
    }

    /// Sends `initialize` and waits for its response, leaving the session one step short of fully initialized.
    async fn initialize(&mut self) {
        self.send(
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "test", "version": "0"}}}),
        )
        .await;
        assert_eq!(self.recv().await["id"], 1);
    }

    /// Initializes fully and starts a `hang` turn as request 2, returning the agent's PIDs (see
    /// `Harness::hanging_agent_pids`).
    async fn start_hanging_turn(&mut self, h: &Harness) -> Vec<i32> {
        self.initialize().await;
        self.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        self.send(
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
            "name": "run_turn", "arguments": {"prompt": "hang"}}}),
        )
        .await;
        h.hanging_agent_pids().await
    }

    fn close_stdin(&mut self) {
        drop(self.stdin.take());
    }

    fn signal(&self, signal: i32) {
        send_signal(self.server.id().unwrap(), signal);
    }

    /// Asserts that devbox-mcp exits on its own, successfully (an orderly shutdown, not a crash or a kill).
    async fn assert_clean_exit(mut self) {
        let status = tokio::time::timeout(Duration::from_secs(10), self.server.wait())
            .await
            .expect("devbox-mcp did not exit")
            .unwrap();
        assert!(status.success(), "devbox-mcp exited with {status}");
    }
}

/// Spec (`LIF-1`): when tunnel-client stops devbox-mcp, devbox-mcp exits and takes the agent's processes with it,
/// including commands the agent started in other sessions, the way Codex runs shell commands. This mimics
/// tunnel-client's stop sequence exactly: close the child's stdin and send it SIGTERM.
#[tokio::test]
async fn tunnel_client_stop_kills_the_agent_and_its_children() {
    let h = Harness::new();
    let mut server = RawServer::start(&h);
    let pids = server.start_hanging_turn(&h).await;
    server.close_stdin();
    server.signal(libc::SIGTERM);
    server.assert_clean_exit().await;
    assert_all_gone(&pids, "agent survived devbox-mcp shutting down").await;
}

/// Spec (`LIF-1`): if tunnel-client itself dies, devbox-mcp only sees its stdin reach end of file. That alone must be
/// enough for the same orderly shutdown.
#[tokio::test]
async fn stdin_eof_kills_the_agent_and_its_children() {
    let h = Harness::new();
    let mut server = RawServer::start(&h);
    let pids = server.start_hanging_turn(&h).await;
    server.close_stdin();
    server.assert_clean_exit().await;
    assert_all_gone(&pids, "agent survived devbox-mcp shutting down").await;
}

/// Spec (`LIF-1`): SIGTERM alone, with stdin still open, also shuts devbox-mcp down. This is what happens when a service
/// manager stops every process of a service at once. Shutdown used to hang here, waiting on the blocking stdin read.
#[tokio::test]
async fn sigterm_with_stdin_open_still_shuts_down() {
    let h = Harness::new();
    let mut server = RawServer::start(&h);
    let pids = server.start_hanging_turn(&h).await;
    server.signal(libc::SIGTERM);
    server.assert_clean_exit().await;
    assert_all_gone(&pids, "agent survived devbox-mcp shutting down").await;
}

/// A client that never finishes MCP initialization must not make devbox-mcp unstoppable: SIGTERM during
/// initialization shuts it down cleanly.
#[tokio::test]
async fn sigterm_during_initialization_shuts_down() {
    let h = Harness::new();
    let mut server = RawServer::start(&h);
    server.initialize().await;
    server.signal(libc::SIGTERM);
    server.assert_clean_exit().await;
}

/// Spec (`LIF-1`): when the client cancels a running call (`notifications/cancelled`), the agent and everything it
/// started are killed, and devbox-mcp itself keeps serving. rmcp does not drop a cancelled handler by itself, so this
/// fails if the handler ignores its cancellation token.
#[tokio::test]
async fn cancelling_a_call_kills_its_agent() {
    let h = Harness::new();
    let mut server = RawServer::start(&h);
    let pids = server.start_hanging_turn(&h).await;
    server
        .send(
            json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
            "params": {"requestId": 2, "reason": "test"}}),
        )
        .await;
    assert_all_gone(&pids, "agent survived its call being cancelled").await;

    server
        .send(json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list"}))
        .await;
    // Skip anything devbox-mcp sends about the cancelled call; what matters is that it still answers.
    loop {
        if server.recv().await["id"] == 3 {
            break;
        }
    }
    server.close_stdin();
    server.assert_clean_exit().await;
}
