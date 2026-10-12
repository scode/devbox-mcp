//! End-to-end tests of the real devbox-mcp binary over plain stdio, the transport tunnel-client uses to talk to it.
//!
//! These tests play tunnel-client's role directly, which makes them fast and lets them control exactly what devbox-mcp
//! sees. They focus on how devbox-mcp runs agent processes (outcomes, pipes, shutdown, cleanup); tests/turns.rs covers
//! the session and turn rules, and tests/tunnel.rs repeats the important paths with the real tunnel-client in between.

mod support;

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use support::{
    DEVBOX_MCP, Harness, assert_all_gone, connect, hermetic_env, run_session_turn, send_signal,
};
use tokio::process::Command;

/// Spec ("MCP tools"): the server advertises exactly the tools it implements. The tool list is the first thing any MCP
/// client (and the tunnel's connector setup) sees, so a broken or surprising listing breaks everything downstream.
#[tokio::test]
async fn lists_the_tools() {
    let h = Harness::new();
    let client = connect(&h).await;
    let tools = client.list_all_tools().await.unwrap();
    let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    names.sort();
    assert_eq!(
        names,
        [
            "session_create",
            "session_interrupt",
            "session_list",
            "session_read",
            "session_send"
        ]
    );
    client.cancel().await.unwrap();
}

/// Spec (`DOC-1`, `DOC-2`, SPEC_impl.md "Self-documentation"): the server instructions are not empty, every tool has a
/// description and both an argument and a result schema, and every property anywhere in those schemas has a
/// description.
///
/// A calling model learns devbox-mcp only from this self-description, and a field added without a doc comment silently
/// reaches it as a bare name. This catches such omissions; whether the text is any good is the manual cold-agent check
/// in docs/testing.md.
#[tokio::test]
async fn every_tool_and_field_is_described() {
    let h = Harness::new();
    let client = connect(&h).await;

    let instructions = client
        .peer_info()
        .and_then(|info| info.instructions.clone())
        .unwrap_or_default();
    assert!(!instructions.trim().is_empty(), "no server instructions");

    let mut missing = Vec::new();
    for tool in client.list_all_tools().await.unwrap() {
        let name = tool.name.as_ref();
        if tool.description.as_deref().unwrap_or("").trim().is_empty() {
            missing.push(format!("{name}: tool description"));
        }
        let Some(output) = &tool.output_schema else {
            missing.push(format!("{name}: result schema"));
            continue;
        };
        for (which, schema) in [("arguments", &tool.input_schema), ("result", output)] {
            undescribed_properties(
                &Value::Object((**schema).clone()),
                &format!("{name} {which}"),
                &mut missing,
            );
        }
    }
    assert!(missing.is_empty(), "missing descriptions: {missing:#?}");
    client.cancel().await.unwrap();
}

/// Collects, under `path`, every entry of every `properties` map anywhere in `schema` that has no non-empty
/// `description`.
///
/// It visits every object in the schema rather than following `$ref`s, so nested types and `$defs` are covered without
/// resolving references. Properties fixed to a single value (`const`) are exempt: they are tags, such as the `type`
/// of an event, which schemars cannot attach a description to and whose tagged object carries one. A property whose schema is only a `$ref` gets its description from the referenced type in
/// some generators; schemars puts the field's doc comment next to the reference instead, which is what is checked.
fn undescribed_properties(schema: &Value, path: &str, missing: &mut Vec<String>) {
    match schema {
        Value::Object(map) => {
            if let Some(Value::Object(properties)) = map.get("properties") {
                for (field, property) in properties {
                    // A property fixed to one value is a tag, such as an event's `type`, and the description of the
                    // object it tags explains it.
                    let described = property.get("const").is_some()
                        || property
                            .get("description")
                            .and_then(Value::as_str)
                            .is_some_and(|d| !d.trim().is_empty());
                    if !described {
                        missing.push(format!("{path}: {field}"));
                    }
                }
            }
            for (key, value) in map {
                undescribed_properties(value, &format!("{path}/{key}"), missing);
            }
        }
        Value::Array(items) => {
            for item in items {
                undescribed_properties(item, path, missing);
            }
        }
        _ => {}
    }
}

/// Spec: a turn sends the prompt to the agent in the session's working directory and reports the agent's reply. Also
/// pins the security-relevant invocation details: the agent runs with permissions bypassed (`SEC-4`) and receives the
/// prompt on stdin, not on its command line.
#[tokio::test]
async fn runs_a_turn_and_returns_the_reply() {
    let h = Harness::new();
    let client = connect(&h).await;

    let (_, last) = run_session_turn(&client, "alpha", "say hello world").await;
    assert_eq!(last["turn"]["state"], "completed");
    assert_eq!(last["turn"]["final_message"], "fake reply: say hello world");

    assert_eq!(h.recorded("stdin"), "say hello world");
    assert_eq!(
        Path::new(h.recorded("cwd").trim()),
        h.workdir_root.join("alpha").canonicalize().unwrap()
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

/// Spec (`LIF-2`, `SES-8`): a turn the agent reports as failed is reported as failed, with the agent's own error text,
/// rather than as success.
#[tokio::test]
async fn reports_a_failed_turn_honestly() {
    let h = Harness::new();
    let client = connect(&h).await;
    let (events, last) = run_session_turn(&client, "alpha", "fail please").await;
    assert_eq!(last["turn"]["state"], "failed");
    assert_eq!(last["turn"]["error"], "fake failure");
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "error" && e["fatal"] == true),
        "{events:?}"
    );
    client.cancel().await.unwrap();
}

/// Spec (`LIF-2`), the other direction: an `error` event that Codex follows with a normal `turn.completed` must not
/// turn a completed turn into a reported failure. Only `turn.failed`, a failed exit, or a missing completion fail a
/// turn.
#[tokio::test]
async fn an_error_event_does_not_fail_a_completed_turn() {
    let h = Harness::new();
    let client = connect(&h).await;
    let (_, last) = run_session_turn(&client, "alpha", "hiccup").await;
    assert_eq!(last["turn"]["state"], "completed");
    assert_eq!(last["turn"]["final_message"], "fake reply: hiccup");
    assert_eq!(last["turn"]["error"], Value::Null);
    client.cancel().await.unwrap();
}

/// Spec (`TRN-*`, robustness): a large prompt is delivered even when the agent writes a lot of output before reading
/// its input. If devbox-mcp wrote the whole prompt before draining output, both sides would block on full pipes.
#[tokio::test]
async fn large_prompt_does_not_deadlock_against_early_output() {
    let h = Harness::new();
    std::fs::write(h.fake_dir.join("noisy"), "").unwrap();
    let client = connect(&h).await;

    let prompt = "x".repeat(200 * 1024);
    let (_, last) = tokio::time::timeout(
        Duration::from_secs(20),
        run_session_turn(&client, "alpha", &prompt),
    )
    .await
    .expect("turn deadlocked");
    assert_eq!(last["turn"]["state"], "completed");
    assert_eq!(h.recorded("stdin").len(), prompt.len());
    client.cancel().await.unwrap();
}

/// Spec (`LIF-1`, `BND-1`): a turn ends when the agent exits, even if something it left running in the background
/// still holds its output pipes open, and that leftover process is killed.
#[tokio::test]
async fn background_leftovers_neither_hang_the_turn_nor_survive_it() {
    let h = Harness::new();
    let client = connect(&h).await;

    let (_, last) = tokio::time::timeout(
        Duration::from_secs(10),
        run_session_turn(&client, "alpha", "linger"),
    )
    .await
    .expect("turn waited for a background process's pipes");
    assert_eq!(last["turn"]["state"], "completed");
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

    let (_, last) = tokio::time::timeout(
        Duration::from_secs(10),
        run_session_turn(&client, "alpha", "linger-detached"),
    )
    .await
    .expect("turn waited for a detached process's pipes");
    assert_eq!(last["turn"]["state"], "completed");
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
        self.start_turn("hang").await;
        h.hanging_agent_pids().await
    }

    /// Initializes fully, creates session `s` (request 2), and sends it `prompt` (request 3), without waiting for the
    /// turn.
    async fn start_turn(&mut self, prompt: &str) {
        self.initialize().await;
        self.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        self.send(
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
            "name": "session_create", "arguments": {"name": "s"}}}),
        )
        .await;
        self.send(
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
            "name": "session_send", "arguments": {"name": "s", "message": prompt}}}),
        )
        .await;
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

/// Spec (`TRN-1`): an accepted turn belongs to its session, not to the call that started it, so a client cancelling
/// that call (`notifications/cancelled`) does not stop the agent; only `session_interrupt` or shutdown does.
#[tokio::test]
async fn cancelling_the_send_call_leaves_the_turn_running() {
    let h = Harness::new();
    let mut server = RawServer::start(&h);
    let pids = server.start_hanging_turn(&h).await;
    server
        .send(
            json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
            "params": {"requestId": 3, "reason": "test"}}),
        )
        .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        pids.iter().all(|&p| support::process_running(p)),
        "the agent stopped when its send call was cancelled"
    );
    server.close_stdin();
    server.assert_clean_exit().await;
    assert_all_gone(&pids, "agent survived devbox-mcp shutting down").await;
}

/// Reads a file of whitespace-separated PIDs the fake agent wrote, or nothing if it does not exist.
fn recorded_pids(h: &Harness, name: &str) -> Vec<i32> {
    std::fs::read_to_string(h.fake_dir.join(name))
        .unwrap_or_default()
        .split_whitespace()
        .map(|p| p.parse().unwrap())
        .collect()
}

/// Spec (`LIF-1`): cleanup must not let stopped processes resume and escape. If it killed a parent before its stopped
/// children, the children's process groups would become orphaned and the kernel would continue them, and these
/// fake processes start a new `sleep` whenever they are continued. Found by the second adversarial review, where 3 of
/// 10 runs with killing in the wrong order leaked processes; the race is probabilistic, so a pass here is evidence,
/// not proof.
#[tokio::test]
async fn cleanup_does_not_let_stopped_processes_resume() {
    let h = Harness::new();
    let mut server = RawServer::start(&h);
    server.start_turn("hang-resumers").await;
    assert!(
        support::eventually(Duration::from_secs(10), || recorded_pids(&h, "resumers")
            .len()
            == 50)
        .await,
        "resumers never started"
    );
    let resumers = recorded_pids(&h, "resumers");

    server.signal(libc::SIGTERM);
    server.assert_clean_exit().await;
    assert_all_gone(&resumers, "resumers survived devbox-mcp shutting down").await;
    // Give any process continued during cleanup a moment to have spawned its escapee.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let spawned = recorded_pids(&h, "spawned");
    let escaped: Vec<i32> = spawned
        .iter()
        .copied()
        .filter(|&p| support::process_running(p))
        .collect();
    for &pid in &escaped {
        send_signal(pid as u32, libc::SIGKILL);
    }
    assert!(escaped.is_empty(), "processes escaped cleanup: {escaped:?}");
}

/// Spec (`LIF-1`): a descendant whose command name is not valid UTF-8 is still found and killed. The name is chosen by
/// the process (here, by the name of the executable), and `/proc/<pid>/stat` embeds it raw; a walk that drops records
/// it cannot decode loses that process and everything below it. Found by the second adversarial review.
#[tokio::test]
async fn cleanup_finds_processes_with_non_utf8_names() {
    let h = Harness::new();
    let mut server = RawServer::start(&h);
    server.start_turn("hang-nonutf8").await;
    let odd_file = h.fake_dir.join("odd");
    assert!(
        support::eventually(Duration::from_secs(10), || odd_file.exists()).await,
        "odd process never started"
    );
    let odd = recorded_pids(&h, "odd");

    server.signal(libc::SIGTERM);
    server.assert_clean_exit().await;
    assert_all_gone(&odd, "non-UTF-8 named process survived").await;
}
