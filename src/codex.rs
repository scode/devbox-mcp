//! Codex agent adapter: runs one Codex turn through `codex exec --json`.
//!
//! Everything that knows Codex's command line or its JSONL event stream lives in this module, so the rest of devbox-mcp
//! can stay agent-agnostic (see SPEC_impl.md, "Agent adapters"). Today it supports only a fresh, single-turn session;
//! resuming, transcripts, and removal come with the session work.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::Context as _;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::Command;

/// Default model for Codex sessions, per SPEC_impl.md.
pub const DEFAULT_MODEL: &str = "gpt-6.1-sol";

/// Default reasoning effort for Codex sessions, per SPEC_impl.md.
pub const DEFAULT_EFFORT: &str = "high";

/// Upper bound on how much of Codex's stderr is kept for error reporting.
///
/// stderr is drained concurrently so a chatty child can never block on a full pipe, but only the head is kept: it is
/// diagnostic text for a failed turn, not something a caller needs in full.
const MAX_STDERR_BYTES: usize = 16 * 1024;

/// How to reach the Codex executable.
#[derive(Debug, Clone)]
pub struct Codex {
    /// Executable to run. Injected (rather than always `codex` on `PATH`) so tests can substitute a fake agent.
    pub bin: PathBuf,
}

/// One turn to run in a fresh Codex session.
#[derive(Debug, Clone)]
pub struct TurnRequest<'a> {
    /// The caller's message, passed to Codex on stdin so its size and content never touch the command line.
    pub prompt: &'a str,
    /// Absolute directory the agent works in.
    pub workdir: &'a Path,
    /// Model name as Codex expects it on `-m`.
    pub model: &'a str,
    /// Reasoning effort as Codex expects it in `model_reasoning_effort`.
    pub effort: &'a str,
}

/// What happened during a turn, as far as devbox-mcp could observe it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TurnOutcome {
    /// Codex's native session ID (`thread_id`), needed later to resume the session. `None` if Codex never got far
    /// enough to report one.
    pub session_id: Option<String>,
    /// The agent's messages in the order they completed. The last one is normally its reply to the prompt.
    pub agent_messages: Vec<String>,
    /// True only if Codex reported `turn.completed` and exited successfully. Anything else is a failed turn.
    pub completed: bool,
    /// Error text reported by Codex or derived from its exit, when the turn did not complete.
    pub error: Option<String>,
}

/// The subset of Codex's `exec --json` events that devbox-mcp acts on.
///
/// Checked against codex-cli 0.162.1. Unknown event types and unknown item types are deliberately accepted and ignored
/// (as [`Event::Other`]), since Codex adds new ones over time and none of them should break a turn.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Event {
    ThreadStarted { thread_id: String },
    AgentMessage { text: String },
    TurnCompleted,
    TurnFailed { message: String },
    Error { message: String },
    Other,
}

/// Wire shape of a Codex JSONL event line, before narrowing to [`Event`].
#[derive(Deserialize)]
struct RawEvent {
    #[serde(rename = "type")]
    kind: String,
    thread_id: Option<String>,
    item: Option<RawItem>,
    error: Option<RawError>,
    message: Option<String>,
}

#[derive(Deserialize)]
struct RawItem {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
}

#[derive(Deserialize)]
struct RawError {
    message: String,
}

/// Parses one line of `codex exec --json` output.
///
/// Returns an error only for lines that are not JSON objects at all; a well-formed event of an unknown type is
/// [`Event::Other`].
fn parse_event(line: &str) -> anyhow::Result<Event> {
    let raw: RawEvent =
        serde_json::from_str(line).with_context(|| format!("unparseable Codex event: {line}"))?;
    Ok(match raw.kind.as_str() {
        "thread.started" => match raw.thread_id {
            Some(thread_id) => Event::ThreadStarted { thread_id },
            None => Event::Other,
        },
        "item.completed" => match raw.item {
            Some(RawItem {
                kind,
                text: Some(text),
            }) if kind == "agent_message" => Event::AgentMessage { text },
            _ => Event::Other,
        },
        "turn.completed" => Event::TurnCompleted,
        "turn.failed" => Event::TurnFailed {
            message: raw
                .error
                .map(|e| e.message)
                .unwrap_or_else(|| "turn failed".to_owned()),
        },
        "error" => Event::Error {
            message: raw.message.unwrap_or_else(|| "unknown error".to_owned()),
        },
        _ => Event::Other,
    })
}

impl Codex {
    /// Builds the argument list for a fresh session's first turn.
    ///
    /// Permissions are fully bypassed because SPEC.md `SEC-4` requires launched agents to run with full access and no
    /// prompts. `--skip-git-repo-check` is needed because session working directories need not be git repositories.
    /// The trailing `-` makes Codex read the prompt from stdin.
    fn first_turn_args(req: &TurnRequest<'_>) -> Vec<std::ffi::OsString> {
        let mut args: Vec<std::ffi::OsString> = vec![
            "exec".into(),
            "--json".into(),
            "--dangerously-bypass-approvals-and-sandbox".into(),
            "--skip-git-repo-check".into(),
            "-C".into(),
            req.workdir.into(),
            "-m".into(),
            req.model.into(),
            "-c".into(),
        ];
        // Codex parses the value after `=` as TOML, so the effort must be a quoted TOML string.
        args.push(format!("model_reasoning_effort=\"{}\"", req.effort).into());
        args.push("-".into());
        args
    }

    /// Runs one turn in a fresh Codex session and waits for it to end.
    ///
    /// The child runs in its own process group with a parent-death signal, and the whole group is killed when this
    /// function returns or its future is dropped (for instance because the MCP client cancelled the call). That is how
    /// devbox-mcp keeps `LIF-1`: no agent work keeps running unobserved.
    pub async fn run_turn(&self, req: &TurnRequest<'_>) -> anyhow::Result<TurnOutcome> {
        let mut cmd = Command::new(&self.bin);
        cmd.args(Self::first_turn_args(req))
            .current_dir(req.workdir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0);
        set_parent_death_signal(&mut cmd);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to start {}", self.bin.display()))?;
        let _group = child.id().map(ProcessGroupKiller);

        let mut stdin = child.stdin.take().context("child stdin missing")?;
        stdin
            .write_all(req.prompt.as_bytes())
            .await
            .context("failed to send prompt to Codex")?;
        // Closing stdin is what tells Codex the prompt is complete.
        drop(stdin);

        let stderr = child.stderr.take().context("child stderr missing")?;
        let stderr_task = tokio::spawn(read_bounded(stderr, MAX_STDERR_BYTES));

        // A turn completes only if Codex says `turn.completed`, never says `turn.failed`, and exits successfully. A bare
        // `error` event does not by itself fail a turn that Codex then completes; its message is only kept to explain a
        // failure if one follows.
        let mut outcome = TurnOutcome::default();
        let mut saw_turn_completed = false;
        let mut turn_failure: Option<String> = None;
        let mut last_error: Option<String> = None;
        let mut lines =
            BufReader::new(child.stdout.take().context("child stdout missing")?).lines();
        while let Some(line) = lines
            .next_line()
            .await
            .context("failed to read Codex output")?
        {
            if line.trim().is_empty() {
                continue;
            }
            match parse_event(&line)? {
                Event::ThreadStarted { thread_id } => outcome.session_id = Some(thread_id),
                Event::AgentMessage { text } => outcome.agent_messages.push(text),
                Event::TurnCompleted => saw_turn_completed = true,
                Event::TurnFailed { message } => turn_failure = Some(message),
                Event::Error { message } => last_error = Some(message),
                Event::Other => {}
            }
        }

        let status = child.wait().await.context("failed to wait for Codex")?;
        let stderr = stderr_task.await.unwrap_or_default();
        outcome.completed = saw_turn_completed && turn_failure.is_none() && status.success();
        if !outcome.completed {
            outcome.error = Some(turn_failure.or(last_error).unwrap_or_else(|| {
                format!("Codex exited with {status} without completing the turn; stderr: {stderr}")
            }));
        }
        Ok(outcome)
    }
}

/// Reads at most `limit` bytes of a stream as lossy UTF-8, then keeps draining (and discarding) the rest so the writer
/// never blocks on a full pipe.
async fn read_bounded(mut reader: impl tokio::io::AsyncRead + Unpin, limit: usize) -> String {
    let mut kept = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = limit.saturating_sub(kept.len());
                kept.extend_from_slice(&buf[..n.min(room)]);
            }
        }
    }
    String::from_utf8_lossy(&kept).into_owned()
}

/// Kills a child's whole process group when dropped.
///
/// The group ID equals the child's PID because the child is spawned with `process_group(0)`. Killing the group, not
/// just the child, also stops commands the agent started. `ESRCH` (nothing left in the group) is the normal case after
/// a clean exit and is ignored.
struct ProcessGroupKiller(u32);

impl Drop for ProcessGroupKiller {
    fn drop(&mut self) {
        let Ok(pgid) = libc::pid_t::try_from(self.0) else {
            return;
        };
        // SAFETY: killpg has no memory-safety preconditions; it only sends a signal.
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
    }
}

/// Asks the kernel to SIGKILL the child if devbox-mcp dies, so an abrupt exit does not leave the agent running.
///
/// Linux delivers this signal when the *thread* that spawned the child exits, not the process. devbox-mcp runs a
/// current-thread tokio runtime, so children are always spawned from the main thread, which lives as long as the
/// process. Switching to a multi-threaded runtime would silently break this.
#[cfg(target_os = "linux")]
fn set_parent_death_signal(cmd: &mut Command) {
    // SAFETY: the closure runs in the forked child before exec and only calls prctl, which is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn set_parent_death_signal(_cmd: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// The event lines below were captured from a real codex-cli 0.162.1 hello-world turn. If Codex changes its event
    /// format, these tests are where that should show up first, rather than as a turn that silently reports nothing.
    #[test]
    fn parses_events_from_a_real_turn() {
        let lines = [
            r#"{"type":"thread.started","thread_id":"01a12c07-46e5-77e0-a7ca-b6f612fc548d"}"#,
            r#"{"type":"turn.started"}"#,
            r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"hello world"}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":19685,"output_tokens":35}}"#,
        ];
        let events: Vec<Event> = lines.iter().map(|l| parse_event(l).unwrap()).collect();
        assert_eq!(
            events,
            vec![
                Event::ThreadStarted {
                    thread_id: "01a12c07-46e5-77e0-a7ca-b6f612fc548d".to_owned()
                },
                Event::Other,
                Event::AgentMessage {
                    text: "hello world".to_owned()
                },
                Event::TurnCompleted,
            ]
        );
    }

    /// Failure events must surface their message, since that is all the caller gets to see about why a turn failed.
    #[test]
    fn parses_failure_events() {
        assert_eq!(
            parse_event(r#"{"type":"turn.failed","error":{"message":"model unavailable"}}"#)
                .unwrap(),
            Event::TurnFailed {
                message: "model unavailable".to_owned()
            }
        );
        assert_eq!(
            parse_event(r#"{"type":"error","message":"stream disconnected"}"#).unwrap(),
            Event::Error {
                message: "stream disconnected".to_owned()
            }
        );
    }

    /// New Codex event and item types must not break a turn; only non-JSON output is an error.
    #[test]
    fn tolerates_unknown_events_and_items() {
        assert_eq!(
            parse_event(r#"{"type":"something.new","x":1}"#).unwrap(),
            Event::Other
        );
        assert_eq!(
            parse_event(
                r#"{"type":"item.completed","item":{"type":"command_execution","command":"ls"}}"#
            )
            .unwrap(),
            Event::Other
        );
        assert!(parse_event("not json").is_err());
    }

    /// The bypass flag is what makes `SEC-4` true for Codex, and `-` is what makes Codex read the prompt from stdin;
    /// losing either would silently change behavior, so the exact argument list is pinned.
    #[test]
    fn first_turn_args_bypass_permissions_and_read_stdin() {
        let req = TurnRequest {
            prompt: "hi",
            workdir: Path::new("/work/dir"),
            model: "m",
            effort: "high",
        };
        let args: Vec<String> = Codex::first_turn_args(&req)
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect();
        assert_eq!(
            args,
            [
                "exec",
                "--json",
                "--dangerously-bypass-approvals-and-sandbox",
                "--skip-git-repo-check",
                "-C",
                "/work/dir",
                "-m",
                "m",
                "-c",
                "model_reasoning_effort=\"high\"",
                "-",
            ]
        );
    }
}
