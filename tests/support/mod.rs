//! Shared harness for the integration tests: a fake Codex, a per-test sandbox, and process helpers.
//!
//! Every test runs the real devbox-mcp binary against a fake Codex (a small shell script) instead of the real one. That
//! keeps the tests hermetic: no model calls, no network, and no touching the user's real Codex state or default working
//! directory, because every path is injected through command-line flags rather than environment variables.

// Each test crate compiles this module separately and uses a different subset of it.
#![allow(dead_code)]

pub mod dev_proxy;

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::{RoleClient, RunningService};
use serde_json::Value;
use tempfile::TempDir;

/// A stand-in for `codex exec --json` that behaves according to the prompt it reads on stdin.
///
/// It records its argv, stdin, working directory, and parent PID (which is devbox-mcp's) next to itself, so tests can
/// check exactly how devbox-mcp invoked it. Prompts select the behavior:
///
/// - `fail...`: a failed turn.
/// - `hang...`: starts a background `sleep` (standing in for a command the agent launched), records both PIDs, and never
///   finishes.
/// - `hiccup...`: an `error` event, then a normal completion.
/// - anything else: an echo reply.
///
/// It is a shell script because the fake has to be a standalone executable and `sh` is everywhere these tests run.
pub const FAKE_CODEX: &str = r#"#!/bin/sh
dir=$(dirname "$0")
printf '%s\n' "$@" > "$dir/argv"
echo "$PPID" > "$dir/ppid"
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

/// Path of the devbox-mcp binary under test, built by Cargo for this test run.
pub const DEVBOX_MCP: &str = env!("CARGO_BIN_EXE_devbox-mcp");

/// Per-test sandbox: a fake Codex, a working directory root, and scratch space, all in one temporary directory that is
/// removed when the harness is dropped.
pub struct Harness {
    pub tmp: TempDir,
    pub fake_dir: PathBuf,
    pub workdir_root: PathBuf,
}

impl Harness {
    pub fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let fake_dir = tmp.path().join("fake");
        std::fs::create_dir(&fake_dir).unwrap();
        let fake = fake_dir.join("codex");
        std::fs::write(&fake, FAKE_CODEX).unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let workdir_root = tmp.path().join("work");
        Self {
            tmp,
            fake_dir,
            workdir_root,
        }
    }

    /// The command-line flags that point devbox-mcp at this sandbox instead of the user's real setup.
    pub fn devbox_args(&self) -> Vec<String> {
        vec![
            "--workdir-root".to_owned(),
            self.workdir_root.to_str().unwrap().to_owned(),
            "--codex-bin".to_owned(),
            self.fake_dir.join("codex").to_str().unwrap().to_owned(),
        ]
    }

    /// Reads a file the fake Codex recorded about its last invocation.
    pub fn recorded(&self, name: &str) -> String {
        std::fs::read_to_string(self.fake_dir.join(name)).unwrap()
    }

    /// Waits for a `hang` turn to start, and returns the PIDs of the fake agent and the background command it started.
    pub async fn hanging_agent_pids(&self) -> Vec<i32> {
        let pids_file = self.fake_dir.join("pids");
        assert!(
            eventually(Duration::from_secs(10), || pids_file.exists()).await,
            "fake agent never started"
        );
        // The fake writes the file with one `echo`, but a read can still race the write.
        let pids: Vec<i32> = loop {
            let pids: Vec<i32> = self
                .recorded("pids")
                .split_whitespace()
                .filter_map(|p| p.parse().ok())
                .collect();
            if pids.len() == 2 {
                break pids;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert!(pids.iter().all(|&p| process_exists(p)));
        pids
    }
}

/// Calls `run_turn` with the given JSON arguments, failing the test on a protocol-level error.
pub async fn run_turn(client: &RunningService<RoleClient, ()>, args: Value) -> CallToolResult {
    let params =
        CallToolRequestParams::new("run_turn").with_arguments(args.as_object().unwrap().clone());
    client.call_tool(params).await.unwrap()
}

/// The structured result of a `run_turn` call.
pub fn structured(result: &CallToolResult) -> &Value {
    result
        .structured_content
        .as_ref()
        .expect("run_turn returns structured content")
}

/// True if a process with this PID exists. Zombies count as existing, which is why callers poll: a killed process
/// whose parent was also killed is reaped by init shortly after.
pub fn process_exists(pid: i32) -> bool {
    // SAFETY: signal 0 performs only the existence and permission check; nothing is delivered.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Sends `signal` to `pid`, failing the test if that is impossible.
pub fn send_signal(pid: u32, signal: i32) {
    let pid = libc::pid_t::try_from(pid).unwrap();
    // SAFETY: kill has no memory-safety preconditions; it only sends a signal.
    assert_eq!(unsafe { libc::kill(pid, signal) }, 0);
}

/// Polls `cond` until it holds or `timeout` passes, returning whether it held.
pub async fn eventually(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    cond()
}

/// Asserts that every PID in `pids` is gone within a few seconds.
pub async fn assert_all_gone(pids: &[i32], what: &str) {
    let gone = eventually(Duration::from_secs(10), || {
        pids.iter().all(|&p| !process_exists(p))
    })
    .await;
    assert!(gone, "{what}: processes {pids:?} are still running");
}
