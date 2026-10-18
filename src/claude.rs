//! Claude agent module: runs one Claude Code turn in print mode (`claude -p --output-format stream-json`), maps its
//! stream into devbox-mcp's agent-agnostic turn events, and finds and deletes Claude's own per-session files.
//!
//! Everything that knows Claude Code's command line, stream format, or file layout lives here. See SPEC_impl.md,
//! "Agent adapters" and "Claude". The shapes were observed with Claude Code 2.1.296; the file layout is undocumented,
//! which is why removal ends with a walk for anything still named with the session ID instead of trusting the list.

use std::collections::HashMap;
use std::future::Future;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::events::{
    AgentEvent, Bounder, EventKind, FileChangeEntry, Keep, Outcome, Progress, TurnResult,
};
use crate::process::{self, AgentCommand, Ended};

/// Default model for Claude sessions, per SPEC_impl.md.
pub const DEFAULT_MODEL: &str = "opus";

/// Default reasoning effort for Claude sessions, per SPEC_impl.md.
pub const DEFAULT_EFFORT: &str = "high";

/// The efforts Claude Code's CLI accepts (`claude --help`, 2.1.296). Checked at creation because Claude ignores any
/// other value with only a warning on stderr, which would silently run the session at its default (SPEC.md `SES-7`).
pub const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// How to reach Claude Code.
#[derive(Debug, Clone)]
pub struct Claude {
    /// Executable to run. Injected so tests can substitute a fake agent.
    pub bin: PathBuf,
    /// Claude Code's configuration directory (`$CLAUDE_CONFIG_DIR`, else `~/.claude`), resolved the way Claude resolves
    /// it, where its per-session files are. devbox-mcp reads and deletes there but never passes it on: setting
    /// `CLAUDE_CONFIG_DIR` would change where Claude reads its global settings (SPEC_impl.md, "Configuration").
    pub home: PathBuf,
}

/// One turn to run.
#[derive(Debug, Clone)]
pub struct TurnRequest<'a> {
    /// The caller's message, passed to Claude on stdin.
    pub prompt: &'a str,
    /// Absolute directory the agent works in.
    pub workdir: &'a Path,
    /// Model as Claude Code expects it on `--model` (an alias such as `opus`, or a full model ID).
    pub model: &'a str,
    /// One of [`EFFORTS`].
    pub effort: &'a str,
    /// The conversation's ID, chosen by devbox-mcp when the session first sent a message.
    pub session_id: &'a str,
    /// Whether to continue the conversation (`--resume`) rather than start it under `session_id` (`--session-id`).
    pub resume: bool,
}

impl Claude {
    /// Builds the argument list for a turn.
    ///
    /// Permission checks are bypassed because SPEC.md `SEC-4` requires launched agents to run with full access and no
    /// prompts. The user's own Claude Code configuration (`CLAUDE.md`, settings, hooks, skills) is deliberately left
    /// in force. The message goes on stdin, never on the command line, where other processes could read it.
    fn args(req: &TurnRequest<'_>) -> Vec<String> {
        let mut args: Vec<String> = [
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--dangerously-skip-permissions",
            "--model",
            req.model,
            "--effort",
            req.effort,
        ]
        .map(str::to_owned)
        .into();
        args.push(
            if req.resume {
                "--resume"
            } else {
                "--session-id"
            }
            .to_owned(),
        );
        args.push(req.session_id.to_owned());
        args
    }

    /// Runs one turn to its end, reporting events as they arrive. Like Codex's, the turn is contained and killed with
    /// its whole process tree when interrupted or dropped (SPEC_impl.md, "Process model").
    pub async fn run_turn(
        &self,
        req: &TurnRequest<'_>,
        mut on_progress: impl FnMut(Progress),
        interrupt: impl Future<Output = ()>,
    ) -> anyhow::Result<Outcome> {
        let command = AgentCommand {
            program: self.bin.clone(),
            args: Self::args(req).into_iter().map(Into::into).collect(),
            cwd: req.workdir.to_owned(),
            stdin: req.prompt.as_bytes().to_vec(),
        };
        let mut turn = ClaudeTurn::new(req.session_id.to_owned());
        let ended = process::run_agent(
            command,
            |line| turn.apply(line, &mut on_progress),
            interrupt,
        )
        .await?;
        Ok(turn.finish(ended))
    }

    /// Finds Claude's transcripts for a conversation: `<id>.jsonl` directly inside any directory under `projects/`
    /// (one per working directory; Claude's encoding of the directory name is not reproduced). Returns every match, so
    /// callers can tell none from one from several; a failure to look is an error, never "none".
    pub fn find_transcripts(&self, session_id: &str) -> std::io::Result<Vec<PathBuf>> {
        let name = format!("{session_id}.jsonl");
        let mut found = Vec::new();
        for project in crate::transcript::find(&self.home.join("projects"), false, &|_| true)? {
            let candidate = project.join(&name);
            match std::fs::symlink_metadata(&candidate) {
                Ok(_) => found.push(candidate),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotADirectory => {}
                Err(e) => return Err(e),
            }
        }
        Ok(found)
    }

    /// The transcript `session_transcript` reads for a Claude session: the main conversation's file. Errors are worded
    /// for the calling model.
    pub fn transcript_path(&self, session_id: &str) -> Result<PathBuf, String> {
        let projects = self.home.join("projects");
        let found = self.find_transcripts(session_id).map_err(|e| {
            format!(
                "could not search {} for the transcript: {e}",
                projects.display()
            )
        })?;
        match found.as_slice() {
            [one] => Ok(one.clone()),
            [] => Err(format!(
                "Claude's transcript for this session was not found: no {session_id}.jsonl in any directory under {}. \
                 Claude may have deleted or moved it. Asking the agent about its earlier work in this session \
                 (session_send) may still work",
                projects.display()
            )),
            many => Err(format!(
                "found {} files that could be this session's transcript under {}, so devbox-mcp cannot tell which is \
                 right",
                many.len(),
                projects.display()
            )),
        }
    }

    /// Deletes Claude's stored data for a conversation (SPEC.md `SES-6`, SPEC_impl.md "Claude"): `<id>.jsonl` and
    /// `<id>/` in every directory under `projects/`, `session-env/<id>`, and `file-history/<id>`. Then walks the whole
    /// configuration directory for anything else whose name contains the ID, and fails, naming what it found, unless
    /// nothing is left. Blocking; callers run it on a blocking thread, and it completes before they return.
    ///
    /// Claude Code has no command for deleting one session (`claude purge` takes a whole project, including other
    /// sessions sharing the working directory). The final walk is what keeps a Claude update that adds or moves a
    /// per-session file from leaving data behind while removal reports it deleted.
    pub fn delete(&self, session_id: &str) -> Result<(), String> {
        let fail =
            |what: &Path, e: std::io::Error| format!("could not delete {}: {e}", what.display());
        let projects = self.home.join("projects");
        let project_dirs = crate::transcript::find(&projects, false, &|_| true)
            .map_err(|e| format!("could not list {}: {e}", projects.display()))?;
        let mut targets = Vec::new();
        for project in project_dirs {
            targets.push(project.join(format!("{session_id}.jsonl")));
            targets.push(project.join(session_id));
        }
        targets.push(self.home.join("session-env").join(session_id));
        targets.push(self.home.join("file-history").join(session_id));
        for target in &targets {
            remove_any(target).map_err(|e| fail(target, e))?;
        }
        let left = crate::transcript::find(&self.home, true, &|name| name.contains(session_id))
            .map_err(|e| format!("could not check {} for leftovers: {e}", self.home.display()))?;
        if left.is_empty() {
            return Ok(());
        }
        let shown: Vec<String> = left
            .iter()
            .take(5)
            .map(|p| p.display().to_string())
            .collect();
        Err(format!(
            "deleted Claude's known files for this session, but {} more named with its ID remain, which devbox-mcp \
             does not know how to remove safely: {}{}",
            left.len(),
            shown.join(", "),
            if left.len() > shown.len() {
                ", ..."
            } else {
                ""
            }
        ))
    }
}

/// Removes a file, a directory tree, or a symbolic link (not its target) at `path`; a missing path is fine.
fn remove_any(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotADirectory => Ok(()),
        Err(e) => Err(e),
    }
}

/// A tool call waiting for its result, kept so the result can be reported against what was asked.
struct PendingTool {
    name: String,
    input: Value,
}

/// What a turn has reported so far, folded from Claude's stream.
///
/// A turn completes only with a `result` event that is not an error and a clean exit; Claude's own `result` text is
/// the final message. A tool call is reported when its result arrives (commands also when they start), because only
/// then is its outcome known.
struct ClaudeTurn {
    /// The conversation ID the turn must run in.
    expected_id: String,
    pending: HashMap<String, PendingTool>,
    /// Claude's `result` event: whether it was an error, and its text.
    result: Option<(bool, String)>,
    /// Set when the turn had to be stopped because Claude did something it must not continue with.
    abort_reason: Option<String>,
}

impl ClaudeTurn {
    fn new(expected_id: String) -> Self {
        Self {
            expected_id,
            pending: HashMap::new(),
            result: None,
            abort_reason: None,
        }
    }

    /// Folds in one line of Claude's output, reporting what it means. Breaks when the turn must be stopped.
    fn apply(&mut self, line: &str, on_progress: &mut impl FnMut(Progress)) -> ControlFlow<()> {
        if self.abort_reason.is_some() || line.trim().is_empty() {
            return ControlFlow::Continue(());
        }
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            let mut b = Bounder::default();
            let message = b.text(
                &format!("agent printed a line that is not a JSON event: {line}"),
                Keep::Head,
            );
            on_progress(Progress::Event(b.finish(EventKind::Error {
                message,
                fatal: false,
            })));
            return ControlFlow::Continue(());
        };
        match event["type"].as_str().unwrap_or_default() {
            "system" if event["subtype"] == "init" => {
                let id = event["session_id"].as_str().unwrap_or_default();
                if id != self.expected_id {
                    // Continuing would run in a conversation devbox-mcp did not choose and could not resume or delete
                    // later; the session is marked as possibly holding untracked data instead.
                    self.abort_reason = Some(format!(
                        "Claude started conversation {id} instead of {}; the turn was stopped",
                        self.expected_id
                    ));
                    on_progress(Progress::ForeignConversation);
                    return ControlFlow::Break(());
                }
            }
            "assistant" => {
                for block in blocks(&event) {
                    match block["type"].as_str().unwrap_or_default() {
                        "text" => {
                            let mut b = Bounder::default();
                            let text =
                                b.text(block["text"].as_str().unwrap_or_default(), Keep::Head);
                            on_progress(Progress::Event(
                                b.finish(EventKind::AgentMessage { text }),
                            ));
                        }
                        "tool_use" => {
                            let id = block["id"].as_str().unwrap_or_default().to_owned();
                            let name = block["name"].as_str().unwrap_or_default().to_owned();
                            if name == "Bash" {
                                let mut b = Bounder::default();
                                let kind = EventKind::CommandStarted {
                                    item_id: b.text(&id, Keep::Head),
                                    command: b.text(
                                        block["input"]["command"].as_str().unwrap_or_default(),
                                        Keep::Head,
                                    ),
                                };
                                on_progress(Progress::Event(b.finish(kind)));
                            }
                            self.pending.insert(
                                id,
                                PendingTool {
                                    name,
                                    input: block["input"].clone(),
                                },
                            );
                        }
                        // Thinking and anything newer carry nothing callers need (SPEC_impl.md, "Turn events").
                        _ => {}
                    }
                }
            }
            "user" => {
                // The event's `tool_use_result` holds the structured result (for Bash: whether it was interrupted or
                // sent to the background), but there is one per event, so it is used only when the event carries a
                // single tool result it can belong to.
                let results = blocks(&event)
                    .filter(|b| b["type"] == "tool_result")
                    .count();
                let structured = if results == 1 {
                    &event["tool_use_result"]
                } else {
                    &Value::Null
                };
                for block in blocks(&event) {
                    if block["type"] == "tool_result"
                        && let Some(id) = block["tool_use_id"].as_str()
                        && let Some(tool) = self.pending.remove(id)
                    {
                        on_progress(Progress::Event(tool_finished(id, &tool, block, structured)));
                    }
                }
            }
            "result" => {
                let is_error = event["is_error"].as_bool().unwrap_or(true);
                // Error results may explain themselves in `errors` (a list of strings) and carry no `result` text,
                // as Claude's own SDK expects; the subtype alone (`error_during_execution`) would tell the caller
                // nothing.
                let errors: Vec<&str> = event["errors"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .filter(|e| !e.is_empty())
                    .collect();
                let text = match &event["result"] {
                    Value::String(s) if !s.is_empty() => s.clone(),
                    Value::Null | Value::String(_) if !errors.is_empty() => errors.join("; "),
                    Value::Null | Value::String(_) => {
                        event["subtype"].as_str().unwrap_or("no result").to_owned()
                    }
                    other => other.to_string(),
                };
                self.result = Some((is_error, text));
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }

    fn finish(self, ended: Ended) -> Outcome {
        let cleanup_confirmed = match ended {
            Ended::Stopped { cleanup_confirmed }
            | Ended::Failed {
                cleanup_confirmed, ..
            } => cleanup_confirmed,
            Ended::Exited { .. } => true,
        };
        let final_message = self
            .result
            .as_ref()
            .filter(|(e, _)| !e)
            .map(|(_, t)| t.clone());
        let result = match (ended, self.abort_reason) {
            (_, Some(reason)) => TurnResult::Failed { error: reason },
            (Ended::Stopped { .. }, None) => TurnResult::Interrupted,
            (Ended::Failed { error, .. }, None) => TurnResult::Failed { error },
            (Ended::Exited { status, stderr }, None) => match self.result {
                Some((false, _)) if status.success() => TurnResult::Completed,
                Some((true, text)) => TurnResult::Failed { error: text },
                _ => TurnResult::Failed {
                    error: format!(
                        "Claude exited with {status} without finishing the turn; stderr: {stderr}"
                    ),
                },
            },
        };
        Outcome {
            result,
            cleanup_confirmed,
            final_message,
        }
    }
}

/// The content blocks of an `assistant` or `user` message.
fn blocks(event: &Value) -> impl Iterator<Item = &Value> {
    event["message"]["content"].as_array().into_iter().flatten()
}

/// The event for a tool call whose result has arrived: `command_finished` for `Bash`, `file_change` for the editing
/// tools, `tool` for the rest (SPEC_impl.md, "Claude").
fn tool_finished(id: &str, tool: &PendingTool, result: &Value, structured: &Value) -> AgentEvent {
    let failed = result["is_error"].as_bool().unwrap_or(false);
    let text = result_text(&result["content"]);
    let status = if failed { "failed" } else { "completed" };
    let mut b = Bounder::default();
    let kind = match tool.name.as_str() {
        "Bash" => EventKind::CommandFinished {
            item_id: b.text(id, Keep::Head),
            command: b.text(
                tool.input["command"].as_str().unwrap_or_default(),
                Keep::Head,
            ),
            status: status.to_owned(),
            exit_code: bash_exit_code(failed, &text, &tool.input, structured),
            output: b.text(&text, Keep::Tail),
        },
        "Write" | "Edit" | "MultiEdit" => {
            let kind = if tool.name == "Write" {
                "add"
            } else {
                "update"
            };
            let path = b.text(
                tool.input["file_path"].as_str().unwrap_or_default(),
                Keep::Head,
            );
            EventKind::FileChange {
                item_id: b.text(id, Keep::Head),
                status: status.to_owned(),
                changes: b.changes(vec![FileChangeEntry {
                    path,
                    kind: kind.to_owned(),
                }]),
            }
        }
        name => EventKind::Tool {
            item_id: Some(b.text(id, Keep::Head)),
            name: b.text(name, Keep::Head),
            status: Some(status.to_owned()),
        },
    };
    b.finish(kind)
}

/// The exit code of a finished `Bash` call, which Claude does not report as a field (SPEC_impl.md, "Claude").
///
/// Claude marks a command that exited nonzero as an error whose text starts `Exit code N` (281 of 413 failed Bash
/// calls in a sample of real Claude Code transcripts, across versions up to 2.1.296); the other failures are commands
/// that never ran (blocked, or needing approval), which have no exit code. A call that is not an error is a command
/// that exited 0, unless it was sent to the background (it returns at once, before the command ends) or interrupted.
///
/// Only the structured result (`tool_use_result`) can rule those two out, so 0 is claimed only when it is present; without
/// it (an event carrying several results, or a Claude version that leaves it out) the exit code is unknown and null.
fn bash_exit_code(failed: bool, text: &str, input: &Value, structured: &Value) -> Option<i64> {
    if failed {
        let rest = text.strip_prefix("Exit code ")?;
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        return digits.parse().ok();
    }
    if !structured.is_object() {
        return None;
    }
    let backgrounded =
        input["run_in_background"] == true || !structured["backgroundTaskId"].is_null();
    if backgrounded || structured["interrupted"] == true {
        return None;
    }
    Some(0)
}

/// The text of a tool result's content, which Claude sends either as a string or as a list of text blocks.
fn result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// A new conversation ID for Claude's `--session-id`: a random version-4 UUID from the kernel's random source, so no
/// randomness dependency is needed.
pub fn new_session_id() -> std::io::Result<String> {
    let mut bytes = [0u8; 16];
    // SAFETY: the buffer is valid for its length, and getrandom only writes into it.
    let n = unsafe { libc::getrandom(bytes.as_mut_ptr().cast(), bytes.len(), 0) };
    if n != bytes.len() as isize {
        return Err(std::io::Error::last_os_error());
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds stream lines through a turn, collecting what it reports.
    fn feed(expected: &str, lines: &[Value]) -> (Vec<Progress>, ClaudeTurn) {
        let mut turn = ClaudeTurn::new(expected.to_owned());
        let mut progress = Vec::new();
        for line in lines {
            let _ = turn.apply(&line.to_string(), &mut |p| progress.push(p));
        }
        (progress, turn)
    }

    fn kinds(progress: &[Progress]) -> Vec<EventKind> {
        progress
            .iter()
            .filter_map(|p| match p {
                Progress::Event(e) => Some(e.kind.clone()),
                _ => None,
            })
            .collect()
    }

    fn exited_ok() -> Ended {
        use std::os::unix::process::ExitStatusExt as _;
        Ended::Exited {
            status: std::process::ExitStatus::from_raw(0),
            stderr: String::new(),
        }
    }

    /// The mapping observed with Claude Code 2.1.296: text becomes agent messages, a Bash call becomes a command started
    /// and, with its result, finished (no exit code), a Write becomes a file change, other tools become `tool`, and
    /// the `result` event completes the turn with its text as the final message.
    #[test]
    fn maps_a_turn() {
        let (progress, turn) = feed(
            "abc",
            &[
                serde_json::json!({"type":"system","subtype":"init","session_id":"abc"}),
                serde_json::json!({"type":"assistant","message":{"content":[
                    {"type":"thinking","thinking":"hmm"},
                    {"type":"text","text":"Let me look."},
                    {"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}},
                    {"type":"tool_use","id":"t2","name":"Write","input":{"file_path":"/w/a.txt","content":"x"}},
                    {"type":"tool_use","id":"t3","name":"Read","input":{"file_path":"/w/b.txt"}}]}}),
                serde_json::json!({"type":"user","message":{"content":[
                    {"type":"tool_result","tool_use_id":"t1","is_error":false,"content":"a.txt"}]},
                    "tool_use_result":{"stdout":"a.txt","stderr":"","interrupted":false,"isImage":false}}),
                serde_json::json!({"type":"user","message":{"content":[
                    {"type":"tool_result","tool_use_id":"t2","content":[{"type":"text","text":"ok"}]},
                    {"type":"tool_result","tool_use_id":"t3","is_error":true,"content":"no such file"}]}}),
                serde_json::json!({"type":"result","subtype":"success","is_error":false,"result":"done"}),
            ],
        );
        assert_eq!(
            kinds(&progress),
            [
                EventKind::AgentMessage {
                    text: "Let me look.".into()
                },
                EventKind::CommandStarted {
                    item_id: "t1".into(),
                    command: "ls".into()
                },
                EventKind::CommandFinished {
                    item_id: "t1".into(),
                    command: "ls".into(),
                    status: "completed".into(),
                    exit_code: Some(0),
                    output: "a.txt".into(),
                },
                EventKind::FileChange {
                    item_id: "t2".into(),
                    status: "completed".into(),
                    changes: vec![FileChangeEntry {
                        path: "/w/a.txt".into(),
                        kind: "add".into()
                    }],
                },
                EventKind::Tool {
                    item_id: Some("t3".into()),
                    name: "Read".into(),
                    status: Some("failed".into())
                },
            ]
        );
        let outcome = turn.finish(exited_ok());
        assert_eq!(outcome.result, TurnResult::Completed);
        assert_eq!(outcome.final_message.as_deref(), Some("done"));
        assert!(!progress.contains(&Progress::ForeignConversation));
    }

    /// An error `result` (an unknown model, for example) fails the turn with its text; an exit without a `result`
    /// fails it with stderr.
    #[test]
    fn reports_failures() {
        // A run that failed after Claude started is a failed turn that keeps the cleanup result (`LIF-2`).
        let (_, turn) = feed("abc", &[]);
        let outcome = turn.finish(Ended::Failed {
            error: "failed to read agent output".into(),
            cleanup_confirmed: false,
        });
        assert_eq!(
            outcome.result,
            TurnResult::Failed {
                error: "failed to read agent output".into()
            }
        );
        assert!(!outcome.cleanup_confirmed);

        let (_, turn) = feed(
            "abc",
            &[
                serde_json::json!({"type":"result","subtype":"success","is_error":true,
                "result":"API Error: 400 unknown provider for model x"}),
            ],
        );
        let outcome = turn.finish(exited_ok());
        assert_eq!(
            outcome.result,
            TurnResult::Failed {
                error: "API Error: 400 unknown provider for model x".into()
            }
        );
        assert_eq!(outcome.final_message, None);

        use std::os::unix::process::ExitStatusExt as _;
        let (_, turn) = feed("abc", &[]);
        let outcome = turn.finish(Ended::Exited {
            status: std::process::ExitStatus::from_raw(1 << 8),
            stderr: "boom".into(),
        });
        let TurnResult::Failed { error } = outcome.result else {
            panic!("not failed")
        };
        assert!(error.contains("boom"), "{error}");

        let (_, turn) = feed(
            "abc",
            &[
                serde_json::json!({"type":"result","subtype":"error_during_execution","is_error":true,
                "errors":["first problem", "", "second problem"]}),
            ],
        );
        assert_eq!(
            turn.finish(exited_ok()).result,
            TurnResult::Failed {
                error: "first problem; second problem".into()
            }
        );
    }

    /// A conversation ID other than the chosen one stops the turn and is reported as a foreign conversation at once,
    /// since Claude may have stored data under an ID devbox-mcp cannot account for.
    #[test]
    fn stops_on_a_foreign_conversation_id() {
        let (progress, turn) = feed(
            "abc",
            &[serde_json::json!({"type":"system","subtype":"init","session_id":"xyz"})],
        );
        assert_eq!(progress, [Progress::ForeignConversation]);
        let outcome = turn.finish(exited_ok());
        assert!(matches!(outcome.result, TurnResult::Failed { .. }));
    }

    /// Claude reports no exit code as a field, so it is recovered where Claude's own behavior determines it: the
    /// `Exit code N` prefix of a failed command, and 0 for a foreground command that succeeded. A command that never ran,
    /// was sent to the background, or was interrupted has no exit code, and claiming one would mislead the caller; so does
    /// a success without the structured result, which alone could rule out the last two.
    #[test]
    fn recovers_bash_exit_codes() {
        let run = |input: Value, result: Value, structured: Value| {
            let tool = PendingTool {
                name: "Bash".into(),
                input,
            };
            match tool_finished("t", &tool, &result, &structured).kind {
                EventKind::CommandFinished { exit_code, .. } => exit_code,
                other => panic!("unexpected {other:?}"),
            }
        };
        let cmd = serde_json::json!({ "command": "x" });
        let ok = serde_json::json!({ "is_error": false, "content": "done" });
        let ran = serde_json::json!({ "stdout": "done", "interrupted": false });
        assert_eq!(run(cmd.clone(), ok.clone(), ran.clone()), Some(0));
        assert_eq!(run(cmd.clone(), ok.clone(), Value::Null), None);
        assert_eq!(
            run(
                cmd.clone(),
                serde_json::json!({ "is_error": true, "content": "Exit code 2\nno such file" }),
                Value::Null
            ),
            Some(2)
        );
        assert_eq!(
            run(
                cmd.clone(),
                serde_json::json!({ "is_error": true, "content": "This command requires approval" }),
                Value::Null
            ),
            None
        );
        assert_eq!(
            run(
                serde_json::json!({ "command": "x", "run_in_background": true }),
                ok.clone(),
                ran
            ),
            None
        );
        assert_eq!(
            run(
                cmd.clone(),
                ok.clone(),
                serde_json::json!({ "backgroundTaskId": "b1" })
            ),
            None
        );
        assert_eq!(
            run(cmd, ok, serde_json::json!({ "interrupted": true })),
            None
        );
    }

    /// First turns start the chosen conversation and later ones resume it; the message is never on the command line.
    #[test]
    fn builds_the_command_line() {
        let mut req = TurnRequest {
            prompt: "secret",
            workdir: Path::new("/w"),
            model: "sonnet",
            effort: "low",
            session_id: "abc",
            resume: false,
        };
        let args = Claude::args(&req);
        assert!(args.windows(2).any(|w| w == ["--session-id", "abc"]));
        assert!(args.contains(&"--dangerously-skip-permissions".to_owned()));
        assert!(!args.iter().any(|a| a.contains("secret")));
        req.resume = true;
        assert!(
            Claude::args(&req)
                .windows(2)
                .any(|w| w == ["--resume", "abc"])
        );
    }

    /// Session IDs are version-4 UUIDs, different each time.
    #[test]
    fn session_ids_are_v4_uuids() {
        let a = new_session_id().unwrap();
        let b = new_session_id().unwrap();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(&a[14..15], "4");
        assert!(matches!(&a[19..20], "8" | "9" | "a" | "b"));
    }

    /// Removal deletes the known per-session paths and then fails, naming it, if anything else named with the ID is
    /// left, so a layout change cannot make removal report data deleted that is still there.
    #[test]
    fn deletes_known_files_and_reports_leftovers() {
        let dir = tempfile::TempDir::new().unwrap();
        let claude = Claude {
            bin: "claude".into(),
            home: dir.path().to_owned(),
        };
        let id = "abc-123";
        let project = dir.path().join("projects/-w");
        std::fs::create_dir_all(project.join(id).join("subagents")).unwrap();
        std::fs::write(project.join(format!("{id}.jsonl")), "{}\n").unwrap();
        std::fs::write(project.join("other.jsonl"), "{}\n").unwrap();
        std::fs::create_dir_all(dir.path().join("session-env").join(id)).unwrap();
        std::fs::create_dir_all(dir.path().join("file-history").join(id)).unwrap();
        assert_eq!(claude.find_transcripts(id).unwrap().len(), 1);
        claude.delete(id).unwrap();
        assert!(claude.find_transcripts(id).unwrap().is_empty());
        assert!(project.join("other.jsonl").exists());

        std::fs::create_dir_all(dir.path().join("tasks")).unwrap();
        std::fs::write(dir.path().join(format!("tasks/{id}.json")), "{}").unwrap();
        let error = claude.delete(id).unwrap_err();
        assert!(error.contains("tasks"), "{error}");
    }
}
