//! Codex agent module: runs one Codex turn through `codex exec --json` (fresh or resumed) and maps its event stream into
//! devbox-mcp's agent-agnostic turn events.
//!
//! Everything that knows Codex's command line or its JSONL output lives here; running the process itself is
//! [`process::run_agent`]'s job. See SPEC_impl.md, "Agent adapters" and "Codex". The mapping was checked against
//! codex-cli 0.162.1: the `agent_message`, `command_execution`, and `file_change` item shapes were observed in real
//! runs, the rest follows Codex's event definitions.

use std::future::Future;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::events::{AgentEvent, Bounder, EventKind, FileChangeEntry, Keep};
use crate::process::{self, AgentCommand, Ended};

/// Default model for Codex sessions, per SPEC_impl.md.
pub const DEFAULT_MODEL: &str = "gpt-6.1-sol";

/// Default reasoning effort for Codex sessions, per SPEC_impl.md.
pub const DEFAULT_EFFORT: &str = "high";

/// How to reach the Codex executable.
#[derive(Debug, Clone)]
pub struct Codex {
    /// Executable to run. Injected (rather than always `codex` on `PATH`) so tests can substitute a fake agent.
    pub bin: PathBuf,
}

/// One turn to run.
#[derive(Debug, Clone)]
pub struct TurnRequest<'a> {
    /// The caller's message, passed to Codex on stdin.
    pub prompt: &'a str,
    /// Absolute directory the agent works in.
    pub workdir: &'a Path,
    /// Model name as Codex expects it on `-m`.
    pub model: &'a str,
    /// Reasoning effort as Codex expects it in `model_reasoning_effort`.
    pub effort: &'a str,
    /// Codex's own session ID to continue, or `None` to start a fresh conversation.
    pub resume: Option<&'a str>,
}

/// Something a running turn reports as soon as it is known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    /// Codex's own session ID for this conversation, needed to resume it later (`SES-5`).
    NativeId(String),
    Event(AgentEvent),
}

/// How a turn ended, as far as Codex and its process told us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnResult {
    Completed,
    Failed {
        error: String,
    },
    /// Stopped by an interrupt.
    Interrupted,
}

/// Everything a finished turn leaves behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub result: TurnResult,
    /// False when the turn was stopped (interrupted, or aborted by this module) and not everything the agent started
    /// could be confirmed stopped; the session layer reports that as uncertain cleanup (`LIF-2`). Kept apart from
    /// `result` because an aborted turn is reported as failed, and the uncertainty must survive that too.
    pub cleanup_confirmed: bool,
    /// The text of the agent's last message, uncut; the caller bounds it where it stores or returns it.
    pub final_message: Option<String>,
}

impl Codex {
    /// Builds the argument list for a turn.
    ///
    /// Sandbox and approvals are bypassed because SPEC.md `SEC-4` requires launched agents to run with full access and
    /// no prompts; `--ignore-rules` is deliberately not passed, so the user's own Codex rules stay in force (the
    /// `SEC-4` exception). `--skip-git-repo-check` is needed because working directories need not be git
    /// repositories. The trailing `-` makes Codex read the prompt from stdin. A resumed turn names the session
    /// explicitly; `--last` would pick whatever ran most recently.
    fn args(req: &TurnRequest<'_>) -> Vec<std::ffi::OsString> {
        let mut args: Vec<std::ffi::OsString> = vec!["exec".into()];
        if req.resume.is_some() {
            args.push("resume".into());
        }
        args.extend([
            "--json".into(),
            "--dangerously-bypass-approvals-and-sandbox".into(),
            "--skip-git-repo-check".into(),
        ]);
        if req.resume.is_none() {
            // `exec resume` takes no `-C`; the process's own working directory covers it.
            args.extend(["-C".into(), req.workdir.into()]);
        }
        args.extend([
            "-m".into(),
            req.model.into(),
            "-c".into(),
            // Codex parses the value after `=` as TOML, so the effort must be a quoted TOML string.
            format!("model_reasoning_effort=\"{}\"", req.effort).into(),
        ]);
        if let Some(id) = req.resume {
            args.push(id.into());
        }
        args.push("-".into());
        args
    }

    /// Runs one turn and waits for it to end, reporting progress as it happens.
    ///
    /// Returns an error only when the process could not be run or watched at all; everything Codex itself reports,
    /// including failures, is in the [`Outcome`].
    pub async fn run_turn(
        &self,
        req: &TurnRequest<'_>,
        mut on_progress: impl FnMut(Progress),
        interrupt: impl Future<Output = ()>,
    ) -> anyhow::Result<Outcome> {
        let command = AgentCommand {
            program: self.bin.clone(),
            args: Self::args(req),
            cwd: req.workdir.to_owned(),
            stdin: req.prompt.as_bytes().to_vec(),
        };
        let mut turn = CodexTurn::new(req.resume.map(str::to_owned));
        let ended = process::run_agent(
            command,
            |line| turn.apply(line, &mut on_progress),
            interrupt,
        )
        .await?;
        Ok(turn.finish(ended))
    }
}

/// What a turn has reported so far, folded from Codex's events.
///
/// A turn completes only if Codex says `turn.completed`, never says `turn.failed`, and exits successfully. A bare
/// `error` event does not by itself fail a turn that Codex then completes; its message is only kept to explain a
/// failure if one follows.
struct CodexTurn {
    /// The native session ID this turn must continue, if it is a resumed one.
    expected_id: Option<String>,
    final_message: Option<String>,
    saw_turn_completed: bool,
    turn_failure: Option<String>,
    last_error: Option<String>,
    /// Set when the turn had to be stopped because Codex did something it must not continue with.
    abort_reason: Option<String>,
}

impl CodexTurn {
    fn new(expected_id: Option<String>) -> Self {
        Self {
            expected_id,
            final_message: None,
            saw_turn_completed: false,
            turn_failure: None,
            last_error: None,
            abort_reason: None,
        }
    }

    /// Folds in one line of Codex output, reporting what it means. Breaks when the turn must be stopped.
    fn apply(&mut self, line: &str, on_progress: &mut impl FnMut(Progress)) -> ControlFlow<()> {
        // After an abort, whatever Codex still printed belongs to a turn that must not continue; it is not reported.
        if self.abort_reason.is_some() || line.trim().is_empty() {
            return ControlFlow::Continue(());
        }
        let raw: RawEvent = match serde_json::from_str(line) {
            Ok(raw) => raw,
            Err(_) => {
                // Not fatal: one garbled line should not cost the caller the rest of the turn (SPEC_impl.md, "Turn
                // lifecycle").
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
            }
        };
        match raw.kind.as_str() {
            "thread.started" => {
                if let Some(id) = raw.thread_id {
                    if let Some(expected) = &self.expected_id
                        && *expected != id
                    {
                        // Resuming an unknown session makes Codex fail rather than start a new one, so this should
                        // never happen; if it does, continuing would silently run in a different conversation (`SES-5`).
                        self.abort_reason = Some(format!(
                            "Codex resumed session {id} instead of {expected}; the turn was stopped"
                        ));
                        return ControlFlow::Break(());
                    }
                    on_progress(Progress::NativeId(id));
                }
            }
            "item.started" => {
                if let Some(item) = raw.item
                    && item.kind == "command_execution"
                {
                    let mut b = Bounder::default();
                    let kind = EventKind::CommandStarted {
                        item_id: b.text(item.id.as_deref().unwrap_or_default(), Keep::Head),
                        command: b.text(item.command.as_deref().unwrap_or_default(), Keep::Head),
                    };
                    on_progress(Progress::Event(b.finish(kind)));
                }
            }
            "item.completed" => {
                if let Some(item) = raw.item
                    && let Some(event) = self.item_completed(item)
                {
                    on_progress(Progress::Event(event));
                }
            }
            "error" => {
                let message = raw.message.unwrap_or_else(|| "unknown error".to_owned());
                self.last_error = Some(message.clone());
                let mut b = Bounder::default();
                let message = b.text(&message, Keep::Head);
                on_progress(Progress::Event(b.finish(EventKind::Error {
                    message,
                    fatal: false,
                })));
            }
            "turn.failed" => {
                let message = raw
                    .error
                    .map(|e| e.message)
                    .unwrap_or_else(|| "turn failed".to_owned());
                self.turn_failure = Some(message.clone());
                let mut b = Bounder::default();
                let message = b.text(&message, Keep::Head);
                on_progress(Progress::Event(b.finish(EventKind::Error {
                    message,
                    fatal: true,
                })));
            }
            "turn.completed" => self.saw_turn_completed = true,
            // `turn.started`, `item.updated`, and event types newer than this code carry nothing callers need.
            _ => {}
        }
        ControlFlow::Continue(())
    }

    /// Maps a completed item to an event, or `None` for items that are not passed through.
    fn item_completed(&mut self, item: RawItem) -> Option<AgentEvent> {
        let mut b = Bounder::default();
        let kind = match item.kind.as_str() {
            "agent_message" => {
                let text = item.text.unwrap_or_default();
                let event_text = b.text(&text, Keep::Head);
                self.final_message = Some(text);
                EventKind::AgentMessage { text: event_text }
            }
            "command_execution" => EventKind::CommandFinished {
                item_id: b.text(item.id.as_deref().unwrap_or_default(), Keep::Head),
                command: b.text(item.command.as_deref().unwrap_or_default(), Keep::Head),
                status: b.text(item.status.as_deref().unwrap_or_default(), Keep::Head),
                exit_code: item.exit_code,
                output: b.text(
                    item.aggregated_output.as_deref().unwrap_or_default(),
                    Keep::Tail,
                ),
            },
            "file_change" => {
                let mut changes = b.changes(
                    item.changes
                        .unwrap_or_default()
                        .into_iter()
                        .map(|c| FileChangeEntry {
                            path: c.path,
                            kind: c.kind,
                        })
                        .collect(),
                );
                for change in &mut changes {
                    change.path = b.text(&change.path, Keep::Head);
                    change.kind = b.text(&change.kind, Keep::Head);
                }
                EventKind::FileChange {
                    item_id: b.text(item.id.as_deref().unwrap_or_default(), Keep::Head),
                    status: b.text(item.status.as_deref().unwrap_or_default(), Keep::Head),
                    changes,
                }
            }
            "error" => {
                let message = item.message.unwrap_or_else(|| "unknown error".to_owned());
                self.last_error = Some(message.clone());
                EventKind::Error {
                    message: b.text(&message, Keep::Head),
                    fatal: false,
                }
            }
            // The agent's reasoning is not passed through (SPEC_impl.md, "Turn events").
            "reasoning" => return None,
            other => EventKind::Tool {
                item_id: item.id.map(|id| b.text(&id, Keep::Head)),
                name: b.text(other, Keep::Head),
                status: item.status.map(|s| b.text(&s, Keep::Head)),
            },
        };
        Some(b.finish(kind))
    }

    fn finish(self, ended: Ended) -> Outcome {
        let cleanup_confirmed = match ended {
            Ended::Stopped { cleanup_confirmed } => cleanup_confirmed,
            Ended::Exited { .. } => true,
        };
        let result = match (ended, self.abort_reason) {
            (_, Some(reason)) => TurnResult::Failed { error: reason },
            (Ended::Stopped { .. }, None) => TurnResult::Interrupted,
            (Ended::Exited { status, stderr }, None) => {
                if self.saw_turn_completed && self.turn_failure.is_none() && status.success() {
                    TurnResult::Completed
                } else {
                    TurnResult::Failed {
                        error: self.turn_failure.or(self.last_error).unwrap_or_else(|| {
                            format!("Codex exited with {status} without completing the turn; stderr: {stderr}")
                        }),
                    }
                }
            }
        };
        Outcome {
            result,
            cleanup_confirmed,
            final_message: self.final_message,
        }
    }
}

/// Wire shape of a Codex JSONL event line. Only the fields devbox-mcp uses are named; serde ignores the rest, so new
/// fields in newer Codex versions do no harm.
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
    id: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
    command: Option<String>,
    aggregated_output: Option<String>,
    exit_code: Option<i64>,
    status: Option<String>,
    changes: Option<Vec<RawChange>>,
    message: Option<String>,
}

#[derive(Deserialize)]
struct RawChange {
    path: String,
    kind: String,
}

#[derive(Deserialize)]
struct RawError {
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds lines through a turn, collecting what it reports, and returns that with the turn.
    fn feed(
        expected: Option<&str>,
        lines: &[&str],
    ) -> (Vec<Progress>, Vec<ControlFlow<()>>, CodexTurn) {
        let mut turn = CodexTurn::new(expected.map(str::to_owned));
        let mut progress = Vec::new();
        let flows = lines
            .iter()
            .map(|l| turn.apply(l, &mut |p| progress.push(p)))
            .collect();
        (progress, flows, turn)
    }

    fn event(kind: EventKind) -> Progress {
        Progress::Event(AgentEvent {
            kind,
            truncated: false,
        })
    }

    /// These lines were captured from real codex-cli 0.162.1 turns that ran a shell command and used the patch tool.
    /// If Codex changes its event format, this is where it should show up first, rather than as turns that silently
    /// report nothing.
    #[test]
    fn maps_events_from_real_turns() {
        let (progress, _, turn) = feed(
            None,
            &[
                r#"{"type":"thread.started","thread_id":"01a12cc1-130f-7d30-9c04-600328d4f559"}"#,
                r#"{"type":"turn.started"}"#,
                r#"{"type":"item.started","item":{"id":"item_2","type":"command_execution","command":"/bin/bash -lc 'cat a.txt'","aggregated_output":"","exit_code":null,"status":"in_progress"}}"#,
                r#"{"type":"item.completed","item":{"id":"item_2","type":"command_execution","command":"/bin/bash -lc 'cat a.txt'","aggregated_output":"hi\n","exit_code":0,"status":"completed"}}"#,
                r#"{"type":"item.started","item":{"id":"item_3","type":"file_change","changes":[{"path":"/tmp/x/a.txt","kind":"update"}],"status":"in_progress"}}"#,
                r#"{"type":"item.completed","item":{"id":"item_3","type":"file_change","changes":[{"path":"/tmp/x/a.txt","kind":"update"}],"status":"completed"}}"#,
                r#"{"type":"item.completed","item":{"id":"item_4","type":"agent_message","text":"done"}}"#,
                r#"{"type":"turn.completed","usage":{"input_tokens":67806,"output_tokens":148}}"#,
            ],
        );
        assert_eq!(
            progress,
            vec![
                Progress::NativeId("01a12cc1-130f-7d30-9c04-600328d4f559".to_owned()),
                event(EventKind::CommandStarted {
                    item_id: "item_2".to_owned(),
                    command: "/bin/bash -lc 'cat a.txt'".to_owned(),
                }),
                event(EventKind::CommandFinished {
                    item_id: "item_2".to_owned(),
                    command: "/bin/bash -lc 'cat a.txt'".to_owned(),
                    status: "completed".to_owned(),
                    exit_code: Some(0),
                    output: "hi\n".to_owned(),
                }),
                event(EventKind::FileChange {
                    item_id: "item_3".to_owned(),
                    status: "completed".to_owned(),
                    changes: vec![FileChangeEntry {
                        path: "/tmp/x/a.txt".to_owned(),
                        kind: "update".to_owned(),
                    }],
                }),
                event(EventKind::AgentMessage {
                    text: "done".to_owned(),
                }),
            ]
        );
        assert!(turn.saw_turn_completed);
        assert_eq!(turn.final_message.as_deref(), Some("done"));
    }

    /// Errors must surface with their message, and only `turn.failed` is fatal: Codex can report an error and still
    /// complete the turn. A declined command keeps its null exit code rather than inventing one.
    #[test]
    fn maps_errors_and_declined_commands() {
        let (progress, _, turn) = feed(
            None,
            &[
                r#"{"type":"error","message":"stream disconnected"}"#,
                r#"{"type":"item.completed","item":{"id":"i1","type":"error","message":"rule rejected"}}"#,
                r#"{"type":"item.completed","item":{"id":"i2","type":"command_execution","command":"rm -rf x","aggregated_output":"","exit_code":null,"status":"declined"}}"#,
                r#"{"type":"turn.failed","error":{"message":"model unavailable"}}"#,
            ],
        );
        assert_eq!(
            progress,
            vec![
                event(EventKind::Error {
                    message: "stream disconnected".to_owned(),
                    fatal: false
                }),
                event(EventKind::Error {
                    message: "rule rejected".to_owned(),
                    fatal: false
                }),
                event(EventKind::CommandFinished {
                    item_id: "i2".to_owned(),
                    command: "rm -rf x".to_owned(),
                    status: "declined".to_owned(),
                    exit_code: None,
                    output: String::new(),
                }),
                event(EventKind::Error {
                    message: "model unavailable".to_owned(),
                    fatal: true
                }),
            ]
        );
        assert_eq!(turn.turn_failure.as_deref(), Some("model unavailable"));
    }

    /// New Codex item and event types must not break a turn: unknown items become generic `tool` events, reasoning is
    /// dropped, and a line that is not JSON at all becomes a non-fatal error event instead of ending the turn.
    #[test]
    fn tolerates_unknown_and_garbled_output() {
        let (progress, flows, _) = feed(
            None,
            &[
                r#"{"type":"something.new","x":1}"#,
                r#"{"type":"item.completed","item":{"id":"i1","type":"web_search","status":"completed"}}"#,
                r#"{"type":"item.completed","item":{"id":"i2","type":"reasoning","text":"thinking"}}"#,
                "not json",
            ],
        );
        assert_eq!(
            progress,
            vec![
                event(EventKind::Tool {
                    item_id: Some("i1".to_owned()),
                    name: "web_search".to_owned(),
                    status: Some("completed".to_owned()),
                }),
                event(EventKind::Error {
                    message: "agent printed a line that is not a JSON event: not json".to_owned(),
                    fatal: false
                }),
            ]
        );
        assert!(flows.iter().all(|f| f.is_continue()));
    }

    /// Long command output keeps its end (where results and errors are), with the event marked as truncated, so every
    /// event stays small however much the agent printed (`BND-1`).
    #[test]
    fn bounds_long_output() {
        let output = format!("{}END", "x".repeat(20_000));
        let line =
            serde_json::json!({"type":"item.completed","item":{"id":"i","type":"command_execution",
            "command":"yes","aggregated_output":output,"exit_code":0,"status":"completed"}})
            .to_string();
        let (progress, _, _) = feed(None, &[&line]);
        let Progress::Event(AgentEvent {
            kind: EventKind::CommandFinished { output, .. },
            truncated,
        }) = &progress[0]
        else {
            panic!("unexpected {progress:?}");
        };
        assert!(*truncated);
        assert!(output.len() <= crate::events::MAX_TEXT_BYTES && output.ends_with("END"));
    }

    /// A resumed turn that reports a different native session must be stopped, not continued in the wrong conversation
    /// (`SES-5`); a matching one is reported as usual.
    #[test]
    fn stops_on_a_mismatched_resume() {
        let (progress, flows, turn) =
            feed(Some("a"), &[r#"{"type":"thread.started","thread_id":"b"}"#]);
        assert!(progress.is_empty());
        assert!(flows[0].is_break());
        assert!(turn.abort_reason.is_some());

        let (progress, flows, _) =
            feed(Some("a"), &[r#"{"type":"thread.started","thread_id":"a"}"#]);
        assert_eq!(progress, vec![Progress::NativeId("a".to_owned())]);
        assert!(flows[0].is_continue());
    }

    /// The outcome rule: completed needs completion, no failure, and a clean exit; an interrupt and an abort are
    /// reported as such.
    #[test]
    fn classifies_outcomes() {
        use std::os::unix::process::ExitStatusExt as _;
        let ok = std::process::ExitStatus::from_raw(0);
        let bad = std::process::ExitStatus::from_raw(1 << 8);

        let (_, _, turn) = feed(None, &[r#"{"type":"turn.completed"}"#]);
        let exited = Ended::Exited {
            status: ok,
            stderr: String::new(),
        };
        assert_eq!(turn.finish(exited).result, TurnResult::Completed);

        let (_, _, turn) = feed(None, &[r#"{"type":"turn.completed"}"#]);
        let exited = Ended::Exited {
            status: bad,
            stderr: "boom".to_owned(),
        };
        assert!(
            matches!(turn.finish(exited).result, TurnResult::Failed { error } if error.contains("boom"))
        );

        let (_, _, turn) = feed(None, &[]);
        let outcome = turn.finish(Ended::Stopped {
            cleanup_confirmed: false,
        });
        assert_eq!(outcome.result, TurnResult::Interrupted);
        assert!(!outcome.cleanup_confirmed);

        // An aborted turn is reported as failed, but uncertain cleanup must survive that classification.
        let (_, _, turn) = feed(Some("a"), &[r#"{"type":"thread.started","thread_id":"b"}"#]);
        let outcome = turn.finish(Ended::Stopped {
            cleanup_confirmed: false,
        });
        assert!(matches!(outcome.result, TurnResult::Failed { .. }));
        assert!(!outcome.cleanup_confirmed);
    }

    /// Once a turn is aborted, output Codex still prints (drained after the stop) belongs to the wrong conversation and
    /// is not reported.
    #[test]
    fn ignores_output_after_an_abort() {
        let (progress, _, _) = feed(
            Some("a"),
            &[
                r#"{"type":"thread.started","thread_id":"b"}"#,
                r#"{"type":"item.completed","item":{"id":"i","type":"agent_message","text":"hi"}}"#,
            ],
        );
        assert!(progress.is_empty());
    }

    /// Every string field is bounded, not only the obviously long ones: a file path, an ID, or a status from a
    /// misbehaving agent must not make an event unboundedly large (`BND-1`).
    #[test]
    fn bounds_every_string_field() {
        let long = "p".repeat(crate::events::MAX_TEXT_BYTES + 100);
        let line =
            serde_json::json!({"type":"item.completed","item":{"id":long,"type":"file_change",
            "status":long,"changes":[{"path":long,"kind":long}]}})
            .to_string();
        let (progress, _, _) = feed(None, &[&line]);
        let Progress::Event(AgentEvent {
            kind:
                EventKind::FileChange {
                    item_id,
                    status,
                    changes,
                },
            truncated,
        }) = &progress[0]
        else {
            panic!("unexpected {progress:?}");
        };
        let max = crate::events::MAX_TEXT_BYTES;
        assert!(*truncated);
        assert!(item_id.len() <= max && status.len() <= max);
        assert!(changes[0].path.len() <= max && changes[0].kind.len() <= max);
    }

    /// The bypass flag makes `SEC-4` true for Codex, `-` makes it read the prompt from stdin, and a resumed turn names
    /// its session explicitly; losing any of these silently changes behavior, so the argument lists are pinned.
    #[test]
    fn builds_fresh_and_resume_args() {
        let mut req = TurnRequest {
            prompt: "hi",
            workdir: Path::new("/work/dir"),
            model: "m",
            effort: "high",
            resume: None,
        };
        let strings = |req: &TurnRequest<'_>| -> Vec<String> {
            Codex::args(req)
                .into_iter()
                .map(|a| a.into_string().unwrap())
                .collect()
        };
        assert_eq!(
            strings(&req),
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
        req.resume = Some("abc");
        assert_eq!(
            strings(&req),
            [
                "exec",
                "resume",
                "--json",
                "--dangerously-bypass-approvals-and-sandbox",
                "--skip-git-repo-check",
                "-m",
                "m",
                "-c",
                "model_reasoning_effort=\"high\"",
                "abc",
                "-",
            ]
        );
        assert!(!strings(&req).iter().any(|a| a == "--ignore-rules"));
    }
}
