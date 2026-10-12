//! Agent-agnostic turn events, and the size bounds applied when they are created.
//!
//! Every agent module maps its own output into these events, so the session layer and callers never see an agent's
//! native format (SPEC_impl.md, "Turn events"). Bounds hold by construction (SPEC_impl.md, "Tools"): every text field is
//! cut when its event is created, so an event is small no matter what the agent printed, and nothing later has to
//! measure responses.

use rmcp::schemars;
use serde::Serialize;

/// Longest text kept in any one event field, in bytes.
pub const MAX_TEXT_BYTES: usize = 8 * 1024;

/// Most file changes listed in one `file_change` event.
pub const MAX_FILE_CHANGES: usize = 50;

// The event kinds as callers see them: tagged by `type`, field names in snake_case. Doc comments on these types become
// the JSON Schema descriptions a calling model reads, so they are written for that model.
/// One thing that happened during a turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    /// A message from the agent, such as an explanation or its final reply.
    AgentMessage {
        /// The message text.
        text: String,
    },
    /// The agent started a shell command.
    CommandStarted {
        /// Identifies this command within the turn; the matching `command_finished` event has the same `item_id`.
        item_id: String,
        /// The command line as the agent ran it.
        command: String,
    },
    /// A shell command the agent ran has finished, or was declined or failed to run.
    CommandFinished {
        /// Matches the `item_id` of the `command_started` event for this command.
        item_id: String,
        /// The command line as the agent ran it.
        command: String,
        /// How the command ended as the agent reports it, for example `completed`, `failed`, or `declined`.
        status: String,
        /// The command's exit code, or null when it did not run to an exit (for example when it was declined).
        exit_code: Option<i64>,
        /// The end of the command's combined standard output and error.
        output: String,
    },
    /// The agent edited files with its own editing tool. Edits made by shell commands appear as commands instead.
    FileChange {
        /// Identifies this edit within the turn.
        item_id: String,
        /// Whether the edit was applied (`completed`) or not (`failed`).
        status: String,
        /// The files touched, at most 50; the event is marked `truncated` if there were more.
        changes: Vec<FileChangeEntry>,
    },
    /// The agent used some other tool (for example a web search), named after the agent's own name for it.
    Tool {
        /// Identifies this tool use within the turn, when the agent gives one.
        item_id: Option<String>,
        /// The agent's name for the kind of tool use.
        name: String,
        /// How the tool use ended as the agent reports it, when it says.
        status: Option<String>,
    },
    /// An error reported during the turn, by the agent or by devbox-mcp.
    Error {
        /// What went wrong.
        message: String,
        /// True for the error that ended the turn; other errors are reported but the turn went on.
        fatal: bool,
    },
}

/// One file touched by a `file_change` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct FileChangeEntry {
    /// Absolute path of the file.
    pub path: String,
    /// What happened to it as the agent reports it, for example `add`, `update`, or `delete`.
    pub kind: String,
}

/// An event as an agent produced it, before the session gives it a sequence number and time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentEvent {
    pub kind: EventKind,
    /// True when any field was cut to fit the bounds.
    pub truncated: bool,
}

/// Which end of a text to keep when it is too long.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keep {
    /// The beginning, for messages and command lines, where the start says what it is about.
    Head,
    /// The end, for command output, where the result and any error usually are.
    Tail,
}

/// Cuts `text` to at most `max` bytes, keeping the `keep` end and never splitting a UTF-8 character. Returns the text
/// and whether it was cut.
pub fn truncate(text: &str, max: usize, keep: Keep) -> (String, bool) {
    if text.len() <= max {
        return (text.to_owned(), false);
    }
    let cut = match keep {
        Keep::Head => {
            let mut end = max;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            &text[..end]
        }
        Keep::Tail => {
            let mut start = text.len() - max;
            while !text.is_char_boundary(start) {
                start += 1;
            }
            &text[start..]
        }
    };
    (cut.to_owned(), true)
}

/// Builds events while recording whether anything was cut, so each constructor stays a single expression.
#[derive(Default)]
pub struct Bounder {
    truncated: bool,
}

impl Bounder {
    /// Cuts a field to [`MAX_TEXT_BYTES`].
    pub fn text(&mut self, text: &str, keep: Keep) -> String {
        let (out, cut) = truncate(text, MAX_TEXT_BYTES, keep);
        self.truncated |= cut;
        out
    }

    /// Keeps at most [`MAX_FILE_CHANGES`] entries.
    pub fn changes(&mut self, mut changes: Vec<FileChangeEntry>) -> Vec<FileChangeEntry> {
        if changes.len() > MAX_FILE_CHANGES {
            changes.truncate(MAX_FILE_CHANGES);
            self.truncated = true;
        }
        changes
    }

    pub fn finish(self, kind: EventKind) -> AgentEvent {
        AgentEvent {
            kind,
            truncated: self.truncated,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bounds hold by construction only if cutting never panics on multibyte text and keeps the promised end; slicing a
    /// `str` in the middle of a character would panic.
    #[test]
    fn truncate_keeps_the_right_end_on_char_boundaries() {
        assert_eq!(
            truncate("short", 10, Keep::Head),
            ("short".to_owned(), false)
        );
        assert_eq!(truncate("abcdef", 3, Keep::Head), ("abc".to_owned(), true));
        assert_eq!(truncate("abcdef", 3, Keep::Tail), ("def".to_owned(), true));
        // "é" is two bytes; a 3-byte head of "aéé" can only keep "aé".
        assert_eq!(truncate("aéé", 4, Keep::Head), ("aé".to_owned(), true));
        assert_eq!(truncate("ééa", 4, Keep::Tail), ("éa".to_owned(), true));
    }

    /// The `truncated` flag is the explicit marker `BND-1` requires; it must be set whenever any field was cut.
    #[test]
    fn bounder_flags_any_cut() {
        let mut b = Bounder::default();
        let text = b.text(&"x".repeat(MAX_TEXT_BYTES + 1), Keep::Head);
        let event = b.finish(EventKind::AgentMessage { text });
        assert!(event.truncated);

        let mut b = Bounder::default();
        let changes = b.changes(
            (0..MAX_FILE_CHANGES + 5)
                .map(|i| FileChangeEntry {
                    path: format!("/f{i}"),
                    kind: "add".to_owned(),
                })
                .collect(),
        );
        assert_eq!(changes.len(), MAX_FILE_CHANGES);
        assert!(
            b.finish(EventKind::Tool {
                item_id: None,
                name: "x".to_owned(),
                status: None
            })
            .truncated
        );
    }
}
