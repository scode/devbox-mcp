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

use crate::process;

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
    /// The child is contained (see [`crate::process`]): if this future is dropped before the turn ends, for instance
    /// because the MCP call was cancelled or devbox-mcp is shutting down, the agent and everything it started are
    /// killed. That is how devbox-mcp keeps `LIF-1`.
    ///
    /// Two shapes of agent behavior must not hang a turn. The prompt is delivered while output is already being drained,
    /// so an agent that writes a lot before reading its input cannot deadlock against a large prompt. And the turn ends
    /// when the agent exits, not when its output pipes close, because something the agent started in the background may
    /// hold them open indefinitely.
    pub async fn run_turn(&self, req: &TurnRequest<'_>) -> anyhow::Result<TurnOutcome> {
        let mut cmd = Command::new(&self.bin);
        cmd.args(Self::first_turn_args(req))
            .current_dir(req.workdir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        process::contain(&mut cmd);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to start {}", self.bin.display()))?;
        let mut guard = process::TreeGuard::new(child.id());

        let stderr = child.stderr.take().context("child stderr missing")?;
        let mut stderr_task = AbortOnDrop(tokio::spawn(read_bounded(stderr, MAX_STDERR_BYTES)));
        let mut stdin = child.stdin.take().context("child stdin missing")?;
        let prompt = req.prompt.to_owned();
        // A write error here means Codex exited without reading its input; that shows up as a failed turn below.
        let _stdin_task = AbortOnDrop(tokio::spawn(async move {
            let _ = stdin.write_all(prompt.as_bytes()).await;
            // Dropping stdin closes it, which is what tells Codex the prompt is complete.
        }));

        let mut state = TurnState::default();
        let mut lines =
            BufReader::new(child.stdout.take().context("child stdout missing")?).lines();
        let status = loop {
            tokio::select! {
                line = lines.next_line() => match line.context("failed to read Codex output")? {
                    Some(line) => state.apply(&line)?,
                    // Codex closed stdout; it is about to exit, if it has not already.
                    None => break child.wait().await.context("failed to wait for Codex")?,
                },
                status = child.wait() => break status.context("failed to wait for Codex")?,
            }
        };
        // No `.await` between reaping the child (just above) and this call; see `TreeGuard::reaped`.
        guard.reaped();

        // Codex has exited, so everything it wrote is already in the pipes. Read the rest, but only briefly: a
        // background process it started in another session may still hold the pipes open. Both pipes share one
        // deadline so such a leftover delays the turn's result by at most PIPE_DRAIN_TIMEOUT.
        let deadline = tokio::time::Instant::now() + PIPE_DRAIN_TIMEOUT;
        let drain = async {
            while let Some(line) = lines.next_line().await? {
                state.apply(&line)?;
            }
            anyhow::Ok(())
        };
        if let Ok(result) = tokio::time::timeout_at(deadline, drain).await {
            result.context("failed to read Codex output")?;
        }
        let stderr = tokio::time::timeout_at(deadline, &mut stderr_task.0)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        Ok(state.finish(status, &stderr))
    }
}

/// How long to keep reading an exited agent's output pipes before giving up on them.
const PIPE_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Aborts a spawned task when dropped, so helper tasks never outlive the turn that started them.
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// What a turn has reported so far, folded from Codex's events.
///
/// A turn completes only if Codex says `turn.completed`, never says `turn.failed`, and exits successfully. A bare
/// `error` event does not by itself fail a turn that Codex then completes; its message is only kept to explain a
/// failure if one follows.
#[derive(Default)]
struct TurnState {
    session_id: Option<String>,
    agent_messages: Vec<String>,
    saw_turn_completed: bool,
    turn_failure: Option<String>,
    last_error: Option<String>,
}

impl TurnState {
    fn apply(&mut self, line: &str) -> anyhow::Result<()> {
        if line.trim().is_empty() {
            return Ok(());
        }
        match parse_event(line)? {
            Event::ThreadStarted { thread_id } => self.session_id = Some(thread_id),
            Event::AgentMessage { text } => self.agent_messages.push(text),
            Event::TurnCompleted => self.saw_turn_completed = true,
            Event::TurnFailed { message } => self.turn_failure = Some(message),
            Event::Error { message } => self.last_error = Some(message),
            Event::Other => {}
        }
        Ok(())
    }

    fn finish(self, status: std::process::ExitStatus, stderr: &str) -> TurnOutcome {
        let completed = self.saw_turn_completed && self.turn_failure.is_none() && status.success();
        let error = (!completed).then(|| {
            self.turn_failure.or(self.last_error).unwrap_or_else(|| {
                format!("Codex exited with {status} without completing the turn; stderr: {stderr}")
            })
        });
        TurnOutcome {
            session_id: self.session_id,
            agent_messages: self.agent_messages,
            completed,
            error,
        }
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
