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
/// - `fail-long...`: a failed turn whose reason is 5000 bytes, longer than devbox-mcp keeps.
/// - `fail...`: a failed turn.
/// - `hang...`: never finishes. Like real Codex running a shell command, it first starts a shell in a new session (so
///   in another process group), and that shell starts a background `sleep`. A cleanup that only kills the agent's
///   process group misses both. PIDs go to `pids` (agent, session shell) and `detached` (the sleep).
/// - `hang-resumers...`: never finishes. Starts a bash with job control that starts 50 `resumer` processes, each in
///   its own process group in the same session, ignoring SIGHUP and starting a background `sleep` when continued.
///   Killing a parent before its stopped children orphans their groups, and the kernel then continues them, so a
///   cleanup in the wrong order lets new sleeps escape. PIDs go to `resumers` and, for anything that escaped,
///   `spawned`.
/// - `hang-nonutf8...`: never finishes. Starts, in a new session, a copy of `sleep` named `worker\xff`, whose
///   `/proc/<pid>/stat` is not valid UTF-8. Its PID goes to `odd`.
/// - `many...`: 60 agent messages before the reply, for paging.
/// - `hiccup...`: an `error` event, then a normal completion.
/// - `linger...`: completes normally, but first starts a background `sleep` in its own process group that keeps stdout
///   and stderr open after the agent exits. Its PID goes to `linger`.
/// - `linger-detached...`: like `linger`, but the sleep is in a new session, out of reach of any group kill, before the
///   agent exits. Its PID goes to `linger`.
/// - anything else: an echo reply.
///
/// If a file named `noisy` exists next to the fake, it writes 256 KiB to stderr before reading its prompt, which
/// deadlocks against a large prompt unless devbox-mcp drains output while still writing the prompt.
///
/// While a file named `hold-start` exists next to the fake, it waits before reporting its thread, like a real agent
/// that is slow to start, so tests can look at a turn whose conversation ID is not known yet.
///
/// Run as `delete --force <id>` (how devbox-mcp removes a session's data), it records its argv, one line per argument
/// and a blank line after each call, in `delete-argv`, and behaves according to a file named `delete-mode` next to it:
/// absent or `ok` succeeds, `fail` prints an error and exits 1 (as real Codex does for a session that is already gone),
/// and `hang` writes its PID to `delete-pid` and sleeps for a minute in its place, for testing the timeout.
///
/// It is a shell script because the fake has to be a standalone executable and `sh` is everywhere these tests run.
pub const FAKE_CODEX: &str = r#"#!/bin/sh
dir=$(dirname "$0")
if [ "$1" = delete ]; then
  printf '%s\n' "$@" "" >> "$dir/delete-argv"
  case "$(cat "$dir/delete-mode" 2>/dev/null)" in
    fail) echo "Error: failed to delete session" >&2; exit 1 ;;
    hang) echo $$ > "$dir/delete-pid"; exec sleep 60 ;;
  esac
  exit 0
fi
printf '%s\n' "$@" > "$dir/argv"
echo "$PPID" > "$dir/ppid"
if [ -f "$dir/noisy" ]; then
  head -c 262144 /dev/zero | tr '\0' x >&2
fi
prompt=$(cat)
printf '%s' "$prompt" > "$dir/stdin"
pwd > "$dir/cwd"
while [ -f "$dir/hold-start" ]; do sleep 0.02; done
# A resumed turn (`exec resume ... <id> -`) reports the session it was asked to resume, like real Codex.
if [ "$2" = "resume" ]; then
  eval "thread=\${$(($# - 1))}"
else
  thread=fake-thread-1
fi
echo "{\"type\":\"thread.started\",\"thread_id\":\"$thread\"}"
echo '{"type":"turn.started"}'
case "$prompt" in
  fail-long*)
    echo "{\"type\":\"turn.failed\",\"error\":{\"message\":\"$(head -c 5000 /dev/zero | tr '\0' x)\"}}"
    exit 1
    ;;
  fail*)
    echo '{"type":"turn.failed","error":{"message":"fake failure"}}'
    exit 1
    ;;
  hang-resumers*)
    cat > "$dir/resumer" <<'EOS'
#!/bin/bash
trap '' HUP
trap 'sleep 1000 & echo $! >> "$1/spawned"' CONT
echo $$ >> "$1/resumers"
while :; do sleep 0.05; done
EOS
    chmod +x "$dir/resumer"
    bash -c 'set -m; for i in $(seq 1 50); do "$1/resumer" "$1" & done; wait' sh "$dir" &
    wait
    ;;
  hang-nonutf8*)
    name=$(printf 'worker\377')
    cp "$(command -v sleep)" "$dir/$name"
    setsid sh -c 'echo $$ > "$1.tmp" && mv "$1.tmp" "$1" && exec "$2" 1000' sh "$dir/odd" "$dir/$name" &
    wait
    ;;
  hang*)
    setsid sh -c 'sleep 1000 & echo $! > "$1.tmp" && mv "$1.tmp" "$1"; wait' sh "$dir/detached" &
    echo "$$ $!" > "$dir/pids.tmp" && mv "$dir/pids.tmp" "$dir/pids"
    wait
    ;;
  many*)
    for i in $(seq 1 60); do
      echo "{\"type\":\"item.completed\",\"item\":{\"id\":\"m$i\",\"type\":\"agent_message\",\"text\":\"message $i\"}}"
    done
    ;;
  hiccup*)
    echo '{"type":"error","message":"fake transient error"}'
    ;;
  linger-detached*)
    # The PID is written from inside the new session and waited for, because exiting before the child has detached
    # would leave it in this process group, where devbox-mcp rightly kills it.
    setsid sh -c 'echo $$ > "$1.tmp" && mv "$1.tmp" "$1" && exec sleep 1000' sh "$dir/linger" &
    while [ ! -f "$dir/linger" ]; do sleep 0.01; done
    ;;
  linger*)
    sleep 1000 &
    echo "$!" > "$dir/linger"
    ;;
esac
echo '{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"fake reply: '"$prompt"'"}}'
echo '{"type":"turn.completed","usage":{}}'
"#;

/// A stand-in for Claude Code in print mode (`claude -p --output-format stream-json ...`), shaped after what Claude Code
/// 2.1.296 was observed to print and store. Like the fake Codex, it lives in the harness's `fake` directory and records
/// its last invocation next to itself: `claude-argv` (one argument per line), `claude-stdin`, and `claude-cwd`.
///
/// It takes the conversation ID from `--session-id` or `--resume` and keeps Claude's per-session files under
/// `$HOME/.claude` (devbox-mcp's test `HOME` is the harness's temporary directory): it appends one line per turn to the
/// transcript `projects/<working directory with / and . as ->/<id>.jsonl`, and creates `session-env/<id>` and
/// `file-history/<id>`. A `--resume` of an ID with no transcript fails the way Claude does, with an error result.
///
/// Its stream is an `init` event with the conversation ID, an assistant message with text and a `Bash` call, the
/// call's result, and a successful `result` whose text echoes the message. A file named `claude-mode` next to it changes
/// that: `error` ends with an error `result`, `no-result` exits 1 after the `init` event with a message on stderr,
/// `mismatch` reports a different conversation ID in `init`, `mismatch-garbage` does that and then prints bytes that
/// are not UTF-8, so reading its output fails after the mismatch was seen, and `leftover` also creates
/// `tasks/<id>.json`, a file devbox-mcp does not know to delete.
pub const FAKE_CLAUDE: &str = r#"#!/bin/sh
dir=$(dirname "$0")
printf '%s\n' "$@" > "$dir/claude-argv"
pwd > "$dir/claude-cwd"
prompt=$(cat)
printf '%s' "$prompt" > "$dir/claude-stdin"
mode=$(cat "$dir/claude-mode" 2>/dev/null)
id=""
resume=no
while [ $# -gt 0 ]; do
  case "$1" in
    --session-id) id=$2; shift ;;
    --resume) id=$2; resume=yes; shift ;;
  esac
  shift
done
home="$HOME/.claude"
project="$home/projects/$(pwd | tr '/.' '--')"
reported=$id
case "$mode" in mismatch*) reported=00000000-0000-4000-8000-000000000000 ;; esac
echo "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"$reported\"}"
[ "$mode" = mismatch-garbage ] && printf '\377\376\n' && exit 0
if [ "$resume" = yes ] && ! ls "$home"/projects/*/"$id.jsonl" >/dev/null 2>&1; then
  echo "{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"result\":\"No conversation found with session ID: $id\"}"
  exit 1
fi
mkdir -p "$project" "$home/session-env/$id" "$home/file-history/$id"
echo "{\"type\":\"user\",\"sessionId\":\"$id\"}" >> "$project/$id.jsonl"
[ "$mode" = leftover ] && mkdir -p "$home/tasks" && echo '{}' > "$home/tasks/$id.json"
if [ "$mode" = no-result ]; then
  echo "fake claude crashed" >&2
  exit 1
fi
echo '{"type":"assistant","message":{"content":[{"type":"text","text":"Looking."},{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}}]}}'
echo '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","is_error":false,"content":"a.txt"}]}}'
if [ "$mode" = error ]; then
  echo '{"type":"result","subtype":"success","is_error":true,"result":"API Error: fake model problem"}'
  exit 1
fi
echo '{"type":"result","subtype":"success","is_error":false,"result":"fake claude reply: '"$prompt"'"}'
"#;

/// Path of the devbox-mcp binary under test, built by Cargo for this test run.
pub const DEVBOX_MCP: &str = env!("CARGO_BIN_EXE_devbox-mcp");

/// Per-test sandbox: a fake Codex and a fake Claude, a working directory root, and scratch space, all in one temporary directory that is
/// removed when the harness is dropped.
pub struct Harness {
    pub tmp: TempDir,
    pub fake_dir: PathBuf,
    pub workdir_root: PathBuf,
    /// devbox-mcp's state directory (session records and its lock).
    pub state_dir: PathBuf,
}

impl Harness {
    pub fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let fake_dir = tmp.path().join("fake");
        std::fs::create_dir(&fake_dir).unwrap();
        let fake = fake_dir.join("codex");
        std::fs::write(&fake, FAKE_CODEX).unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let fake = fake_dir.join("claude");
        std::fs::write(&fake, FAKE_CLAUDE).unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let workdir_root = tmp.path().join("work");
        let state_dir = tmp.path().join("state");
        Self {
            tmp,
            fake_dir,
            workdir_root,
            state_dir,
        }
    }

    /// The command-line flags that point devbox-mcp at this sandbox instead of the user's real setup.
    pub fn devbox_args(&self) -> Vec<String> {
        vec![
            "--workdir-root".to_owned(),
            self.workdir_root.to_str().unwrap().to_owned(),
            "--codex-bin".to_owned(),
            self.fake_dir.join("codex").to_str().unwrap().to_owned(),
            "--claude-bin".to_owned(),
            self.fake_dir.join("claude").to_str().unwrap().to_owned(),
            "--state-dir".to_owned(),
            self.state_dir.to_str().unwrap().to_owned(),
        ]
    }

    /// Reads a file the fake Codex recorded about its last invocation.
    pub fn recorded(&self, name: &str) -> String {
        std::fs::read_to_string(self.fake_dir.join(name)).unwrap()
    }

    /// Waits for a `hang` turn to fully start, and returns the PIDs of the fake agent, the shell it started in a new
    /// session, and that shell's background `sleep`.
    pub async fn hanging_agent_pids(&self) -> Vec<i32> {
        let (pids, detached) = (self.fake_dir.join("pids"), self.fake_dir.join("detached"));
        assert!(
            eventually(Duration::from_secs(10), || pids.exists()
                && detached.exists())
            .await,
            "fake agent never started"
        );
        // Both files are written to a temporary name and renamed into place, so they are complete when they exist.
        let pids: Vec<i32> = [self.recorded("pids"), self.recorded("detached")]
            .iter()
            .flat_map(|s| {
                s.split_whitespace()
                    .map(|p| p.parse().unwrap())
                    .collect::<Vec<i32>>()
            })
            .collect();
        assert_eq!(pids.len(), 3);
        assert!(pids.iter().all(|&p| process_running(p)));
        pids
    }
}

/// Starts devbox-mcp on this harness and completes MCP initialization with it over stdio.
pub async fn connect(h: &Harness) -> RunningService<RoleClient, ()> {
    use rmcp::ServiceExt as _;
    let mut cmd = tokio::process::Command::new(DEVBOX_MCP);
    cmd.env_clear()
        .envs(hermetic_env(h.tmp.path()))
        .args(h.devbox_args());
    ().serve(rmcp::transport::TokioChildProcess::new(cmd).unwrap())
        .await
        .unwrap()
}

/// Calls any tool with JSON arguments, returning the protocol-level result so tests can check errors too.
pub async fn call(
    client: &RunningService<RoleClient, ()>,
    tool: &str,
    args: Value,
) -> Result<CallToolResult, rmcp::ServiceError> {
    let params = CallToolRequestParams::new(tool.to_owned())
        .with_arguments(args.as_object().unwrap().clone());
    client.call_tool(params).await
}

/// The structured content of a tool result.
pub fn structured(result: &CallToolResult) -> Value {
    result
        .structured_content
        .clone()
        .expect("the tool returns structured content")
}

/// The error text of a call that must fail, for checking that errors say what went wrong (`DOC-3`).
pub fn error_text(result: Result<CallToolResult, rmcp::ServiceError>) -> String {
    match result {
        Err(err) => err.to_string(),
        Ok(result) => panic!("expected an error, got {result:?}"),
    }
}

/// Calls `session_read`, failing the test on an error.
pub async fn read(
    client: &RunningService<RoleClient, ()>,
    name: &str,
    cursor: Option<&str>,
) -> Value {
    let mut args = serde_json::json!({ "name": name });
    if let Some(c) = cursor {
        args["cursor"] = serde_json::json!(c);
    }
    structured(&call(client, "session_read", args).await.unwrap())
}

/// Polls `session_read` from `cursor` until the most recent turn has ended and no more events are waiting, the way a
/// caller is meant to, and returns every event seen and the final read.
pub async fn read_until_ended(
    client: &RunningService<RoleClient, ()>,
    name: &str,
    mut cursor: Option<String>,
) -> (Vec<Value>, Value) {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut events = Vec::new();
    loop {
        let result = read(client, name, cursor.as_deref()).await;
        events.extend(result["events"].as_array().unwrap().iter().cloned());
        cursor = Some(result["next_cursor"].as_str().unwrap().to_owned());
        let state = result["turn"]["state"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let ended = !matches!(state.as_str(), "running" | "interrupting");
        if ended && result["more"] == false {
            return (events, result);
        }
        assert!(Instant::now() < deadline, "turn never ended: {result}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Runs one turn the way a caller would: creates session `name`, sends `prompt`, and follows the turn to its end.
/// Returns the events and the final read, whose `turn` has the outcome.
pub async fn run_session_turn(
    client: &RunningService<RoleClient, ()>,
    name: &str,
    prompt: &str,
) -> (Vec<Value>, Value) {
    call(
        client,
        "session_create",
        serde_json::json!({ "name": name }),
    )
    .await
    .unwrap();
    let sent = structured(
        &call(
            client,
            "session_send",
            serde_json::json!({ "name": name, "message": prompt }),
        )
        .await
        .unwrap(),
    );
    read_until_ended(client, name, sent["cursor"].as_str().map(str::to_owned)).await
}

/// True if a process with this PID exists and is not a zombie.
///
/// Zombies count as gone: a killed process stays a zombie until its parent reaps it, and in a container whose init does
/// not reap orphans that may be never, even though the process stopped running long ago.
pub fn process_running(pid: i32) -> bool {
    let Ok(stat) = std::fs::read(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // The state follows the command name, which is in parentheses, may itself contain them, and need not be UTF-8, so
    // this works on bytes. An unparseable record counts as running: claiming a process is gone when it is not is the
    // mistake these tests exist to catch.
    let state = stat
        .iter()
        .rposition(|&b| b == b')')
        .and_then(|i| stat[i + 1..].iter().find(|b| !b.is_ascii_whitespace()));
    state != Some(&b'Z')
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

/// Asserts that every PID in `pids` stops running within a few seconds.
pub async fn assert_all_gone(pids: &[i32], what: &str) {
    let gone = eventually(Duration::from_secs(10), || {
        pids.iter().all(|&p| !process_running(p))
    })
    .await;
    assert!(gone, "{what}: processes {pids:?} are still running");
}

/// The environment for child processes that should not see the test runner's configuration.
///
/// Programs under test (tunnel-client in particular) read configuration from environment variables, so inheriting the
/// developer's environment could point them at real profiles or real PID files. Children get only `PATH`, and a `HOME`
/// inside the test's temporary directory. The test process's own environment is never modified.
pub fn hermetic_env(home: &std::path::Path) -> Vec<(String, std::ffi::OsString)> {
    vec![
        (
            "PATH".to_owned(),
            std::env::var_os("PATH").unwrap_or_default(),
        ),
        ("HOME".to_owned(), home.as_os_str().to_owned()),
    ]
}

/// Calls a tool and checks its structured result against the output schema the tool declares, returning the result.
///
/// Some MCP clients validate every result against the declared schema and reject a mismatch outright (Claude Code
/// does), while rmcp's client here does not, so a result that disagrees with its own schema passes every other test and
/// fails only with a real client. Panics on a tool error, a missing schema, or a mismatch.
pub async fn call_checked(
    client: &RunningService<RoleClient, ()>,
    tool: &str,
    args: Value,
) -> Value {
    let tools = client.list_all_tools().await.unwrap();
    let schema = tools
        .iter()
        .find(|t| t.name == tool)
        .and_then(|t| t.output_schema.clone())
        .unwrap_or_else(|| panic!("{tool} declares no output schema"));
    let schema = Value::Object((*schema).clone());
    let result = structured(&call(client, tool, args).await.unwrap());
    if let Some(mismatch) = schema_mismatch(&schema, &schema, &result, "result") {
        panic!("{tool} result does not match its output schema: {mismatch}\nresult: {result}");
    }
    result
}

/// The first place where `value` breaks `schema`, or `None` if it matches.
///
/// Covers what schemars generates and what validating clients were seen to enforce: `type` (one name or a list),
/// `required`, `properties`, `items`, `$ref` into the root's `$defs`, and `allOf`, `anyOf`, and `oneOf` (treated like
/// `anyOf`). Anything else in the schema is ignored, so this is a check for the mistakes a derive can make, not a full
/// JSON Schema validator.
pub fn schema_mismatch(root: &Value, schema: &Value, value: &Value, path: &str) -> Option<String> {
    let Value::Object(schema) = schema else {
        return None;
    };
    if let Some(Value::String(reference)) = schema.get("$ref") {
        let name = reference
            .strip_prefix("#/$defs/")
            .expect("only local $defs references");
        if let Some(m) = schema_mismatch(root, &root["$defs"][name], value, path) {
            return Some(m);
        }
    }
    if let Some(Value::Array(all)) = schema.get("allOf") {
        if let Some(m) = all
            .iter()
            .find_map(|s| schema_mismatch(root, s, value, path))
        {
            return Some(m);
        }
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(Value::Array(any)) = schema.get(key)
            && any
                .iter()
                .all(|s| schema_mismatch(root, s, value, path).is_some())
        {
            return Some(format!("{path} matches none of its {key} alternatives"));
        }
    }
    if let Some(types) = schema.get("type") {
        let names: Vec<&str> = match types {
            Value::String(t) => vec![t.as_str()],
            Value::Array(ts) => ts.iter().filter_map(Value::as_str).collect(),
            _ => vec![],
        };
        let fits = |t: &str| match t {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "integer" => value.is_i64() || value.is_u64(),
            "number" => value.is_number(),
            "boolean" => value.is_boolean(),
            "null" => value.is_null(),
            _ => true,
        };
        if !names.iter().any(|t| fits(t)) {
            return Some(format!("{path} is {value}, not of type {types}"));
        }
    }
    if let Value::Object(fields) = value {
        if let Some(Value::Array(required)) = schema.get("required") {
            for name in required.iter().filter_map(Value::as_str) {
                if !fields.contains_key(name) {
                    return Some(format!("{path} lacks required property {name:?}"));
                }
            }
        }
        if let Some(Value::Object(properties)) = schema.get("properties") {
            for (name, sub) in properties {
                if let Some(field) = fields.get(name)
                    && let Some(m) = schema_mismatch(root, sub, field, &format!("{path}.{name}"))
                {
                    return Some(m);
                }
            }
        }
    }
    if let (Value::Array(items), Some(sub)) = (value, schema.get("items")) {
        for (i, item) in items.iter().enumerate() {
            if let Some(m) = schema_mismatch(root, sub, item, &format!("{path}[{i}]")) {
                return Some(m);
            }
        }
    }
    None
}
