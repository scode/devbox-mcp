//! End-to-end tests of the real devbox-mcp binary over stdio, the same transport the OpenAI tunnel client uses.
//!
//! Each test starts the compiled binary as a child process, talks MCP to it with rmcp's client, and points it at a fake
//! Codex (a small shell script) instead of the real one. That keeps the tests hermetic: no model calls, no network, and
//! no touching the user's real Codex state or default working directory, because every path is injected through
//! command-line flags rather than environment variables.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rmcp::ServiceExt as _;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::TokioChildProcess;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::process::Command;

/// A stand-in for `codex exec --json` that behaves according to the prompt it reads on stdin.
///
/// It records its argv, stdin, and working directory next to itself so tests can check exactly how devbox-mcp invoked
/// it. Prompts starting with `fail` produce a failed turn; prompts starting with `hang` start a background `sleep`
/// (standing in for a command the agent launched), record both PIDs, and never finish; prompts starting with `hiccup`
/// emit an `error` event and then complete normally. Anything else gets an echo reply. It is a shell script because the fake has to be a standalone executable and `sh` is everywhere this runs.
const FAKE_CODEX: &str = r#"#!/bin/sh
dir=$(dirname "$0")
printf '%s\n' "$@" > "$dir/argv"
prompt=$(cat)
printf '%s' "$prompt" > "$dir/stdin"
pwd > "$dir/cwd"
echo '{"type":"thread.started","thread_id":"fake-thread-1"}'
echo '{"type":"turn.started"}'
case "$prompt" in
  fail*)
    echo '{"type":"turn.failed","error":{"message":"fake failure"}}'
    exit 1
    ;;
  hang*)
    sleep 1000 &
    echo "$$ $!" > "$dir/pids"
    wait
    ;;
  hiccup*)
    echo '{"type":"error","message":"fake transient error"}'
    ;;
esac
echo '{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"fake reply: '"$prompt"'"}}'
echo '{"type":"turn.completed","usage":{}}'
"#;

/// Per-test sandbox: a fake Codex and a working directory root, both in a temporary directory.
struct Harness {
    _tmp: TempDir,
    fake_dir: PathBuf,
    workdir_root: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let fake_dir = tmp.path().join("fake");
        std::fs::create_dir(&fake_dir).unwrap();
        let fake = fake_dir.join("codex");
        std::fs::write(&fake, FAKE_CODEX).unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let workdir_root = tmp.path().join("work");
        Self {
            _tmp: tmp,
            fake_dir,
            workdir_root,
        }
    }

    /// Starts the devbox-mcp binary under test and completes MCP initialization with it.
    async fn connect(&self) -> RunningService<RoleClient, ()> {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_devbox-mcp"));
        cmd.arg("--workdir-root")
            .arg(&self.workdir_root)
            .arg("--codex-bin")
            .arg(self.fake_dir.join("codex"));
        ().serve(TokioChildProcess::new(cmd).unwrap())
            .await
            .unwrap()
    }

    /// Reads a file the fake Codex recorded about its last invocation.
    fn recorded(&self, name: &str) -> String {
        std::fs::read_to_string(self.fake_dir.join(name)).unwrap()
    }
}

async fn run_turn(client: &RunningService<RoleClient, ()>, args: Value) -> CallToolResult {
    let params =
        CallToolRequestParams::new("run_turn").with_arguments(args.as_object().unwrap().clone());
    client.call_tool(params).await.unwrap()
}

fn structured(result: &CallToolResult) -> &Value {
    result
        .structured_content
        .as_ref()
        .expect("run_turn returns structured content")
}

/// True if a process with this PID exists. Zombies count as existing, which is why callers poll: a killed process
/// whose parent was also killed is reaped by init shortly after.
fn process_exists(pid: i32) -> bool {
    // SAFETY: signal 0 performs only the existence and permission check; nothing is delivered.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Polls `cond` until it holds or `timeout` passes, returning whether it held.
async fn eventually(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    cond()
}

/// Spec: the server advertises `run_turn`. This is the first thing any MCP client (and the tunnel's connector setup)
/// sees, so a broken tool listing breaks everything downstream.
#[tokio::test]
async fn lists_run_turn_tool() {
    let h = Harness::new();
    let client = h.connect().await;
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
    let client = h.connect().await;

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
    let client = h.connect().await;

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
    let client = h.connect().await;

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
    let client = h.connect().await;

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

        let mut server = Command::new(env!("CARGO_BIN_EXE_devbox-mcp"))
            .arg("--workdir-root")
            .arg(&h.workdir_root)
            .arg("--codex-bin")
            .arg(h.fake_dir.join("codex"))
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

        let pids_file = h.fake_dir.join("pids");
        assert!(
            eventually(Duration::from_secs(10), || pids_file.exists()).await,
            "fake agent never started"
        );
        // The fake writes the file in one `echo`, but give it a moment in case the read races the write.
        let agent_pids: Vec<i32> = loop {
            let pids: Vec<i32> = h
                .recorded("pids")
                .split_whitespace()
                .filter_map(|p| p.parse().ok())
                .collect();
            if pids.len() == 2 {
                break pids;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert!(agent_pids.iter().all(|&p| process_exists(p)));
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
        let pids = self.agent_pids;
        let gone = eventually(Duration::from_secs(10), || {
            pids.iter().all(|&p| !process_exists(p))
        })
        .await;
        assert!(
            gone,
            "agent processes {pids:?} survived devbox-mcp shutting down"
        );
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
    let pid = libc::pid_t::try_from(turn.server.id().unwrap()).unwrap();
    // SAFETY: sending a signal to our own child process has no memory-safety implications.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
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
