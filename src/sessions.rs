//! The session table: every session devbox-mcp knows, in memory, backed by [`Store`] records.
//!
//! Tool handlers call into this; it owns the rules of SPEC.md "Sessions" (names, working directories, listing) and
//! keeps memory and the records in step. The table sits behind a `std::sync::Mutex` that is never held across an
//! `.await`, so on devbox-mcp's single-threaded runtime it is never contended; it exists to satisfy the type system
//! and to keep each operation's read-modify-write atomic.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Mutex;

use rmcp::schemars;
use serde::Serialize;

use crate::codex;
use crate::events::{Keep, truncate};
use crate::store::{Agent, Record, Store, TurnRecord, TurnState};

/// Most sessions one `session_list` call returns (SPEC_impl.md, "Tools").
pub const LIST_PAGE: usize = 50;

/// Longest final-message preview in a session summary, in bytes.
const PREVIEW_BYTES: usize = 2 * 1024;

/// Settings fixed at startup.
pub struct Config {
    /// Default working directories are `<workdir_root>/<name>` (SPEC.md `SES-3`).
    pub workdir_root: PathBuf,
}

/// A failed session operation, worded for the calling agent (SPEC.md `DOC-3`): what went wrong and what to do.
#[derive(Debug)]
pub enum SessionError {
    /// The caller asked for something invalid or impossible.
    Invalid(String),
    /// devbox-mcp could not do something it should have been able to (disk, filesystem).
    Internal(String),
}

pub struct Sessions {
    store: Store,
    config: Config,
    table: Mutex<Table>,
}

struct Table {
    sessions: BTreeMap<String, Session>,
    /// Names of records that could not be parsed; such a name cannot be reused until the file is dealt with.
    unreadable: BTreeSet<String>,
}

struct Session {
    record: Record,
}

// Results returned to callers. Doc comments on these types become the JSON Schema descriptions a calling model reads.

/// A session as `session_create` and `session_list` show it.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct SessionSummary {
    /// The session's name.
    pub name: String,
    /// The agent the session runs.
    pub agent: Agent,
    /// Absolute path of the directory the agent works in.
    pub workdir: String,
    /// The session's most recent turn, or null if nothing has been sent to it yet.
    pub turn: Option<TurnSummary>,
}

/// A turn as a session summary shows it.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct TurnSummary {
    /// The turn's number within its session, counting from 1.
    pub id: u64,
    /// `running` and `interrupting` mean the turn has not ended; `completed`, `failed`, `interrupted`, and `lost` are
    /// final. `lost` means devbox-mcp restarted while the turn ran, so how it ended is unknown.
    pub state: TurnState,
    /// When the turn started (RFC 3339, UTC).
    pub started_at: String,
    /// When the turn ended (RFC 3339, UTC), or null while it runs or when unknown (a lost turn).
    pub ended_at: Option<String>,
    /// The start of the agent's final message, at most 2 KiB, once the turn has ended. `session_read` returns more.
    pub final_message_preview: Option<String>,
    /// True when the final message is longer than what is shown here.
    pub final_message_truncated: bool,
    /// Why the turn failed, for a failed turn.
    pub error: Option<String>,
    /// True when the turn was interrupted but devbox-mcp could not confirm that everything the agent had started
    /// stopped.
    pub cleanup_uncertain: bool,
}

/// One page of `session_list`.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct SessionPage {
    /// Up to 50 sessions, in name order.
    pub sessions: Vec<SessionSummary>,
    /// When more sessions follow, pass this as `after` to get the next page; null on the last page.
    pub next_after: Option<String>,
}

impl TurnSummary {
    fn from_record(turn: &TurnRecord) -> Self {
        let (final_message_preview, cut) = match &turn.final_message {
            Some(text) => {
                let (preview, cut) = truncate(text, PREVIEW_BYTES, Keep::Head);
                (Some(preview), cut)
            }
            None => (None, false),
        };
        Self {
            id: turn.id,
            state: turn.state,
            started_at: turn.started_at.to_rfc3339(),
            ended_at: turn.ended_at.map(|t| t.to_rfc3339()),
            final_message_preview,
            final_message_truncated: cut || turn.final_message_truncated,
            error: turn.error.clone(),
            cleanup_uncertain: turn.cleanup_uncertain,
        }
    }
}

impl Session {
    fn summary(&self) -> SessionSummary {
        SessionSummary {
            name: self.record.name.clone(),
            agent: self.record.agent,
            workdir: self.record.workdir.display().to_string(),
            turn: self.record.turn.as_ref().map(TurnSummary::from_record),
        }
    }
}

/// Checks a session name against SPEC.md `SES-2`: a lowercase ASCII letter, then lowercase letters, digits, `_`, and
/// `-`, at most 64 characters. These names become file names, so the rule also keeps them safe as such.
pub fn validate_name(name: &str) -> Result<(), SessionError> {
    let mut chars = name.chars();
    let ok = name.len() <= 64
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(SessionError::Invalid(format!(
            "invalid session name {name:?}: a name starts with a lowercase letter, contains only lowercase letters, \
             digits, '_', and '-', and is at most 64 characters long"
        )))
    }
}

impl Sessions {
    /// Loads every session record from the store. Records that cannot be parsed are reported on stderr by the store,
    /// kept out of the table, and block their names (SPEC_impl.md, "Storage").
    pub fn new(store: Store, config: Config) -> anyhow::Result<Self> {
        let loaded = store.load()?;
        let sessions = loaded
            .records
            .into_iter()
            .map(|record| (record.name.clone(), Session { record }))
            .collect();
        Ok(Self {
            store,
            config,
            table: Mutex::new(Table {
                sessions,
                unreadable: loaded.unreadable.into_iter().collect(),
            }),
        })
    }

    /// Creates a session (SPEC.md `SES-1` to `SES-3`, `SES-7`). The session exists once its record is on disk; a
    /// working directory created before a failure is left in place.
    pub fn create(
        &self,
        name: &str,
        workdir: Option<&str>,
        agent: Option<Agent>,
    ) -> Result<SessionSummary, SessionError> {
        validate_name(name)?;
        let mut table = self.table.lock().unwrap();
        if let Some(existing) = table.sessions.get(name) {
            let summary = serde_json::to_string(&existing.summary()).unwrap_or_default();
            return Err(SessionError::Invalid(format!(
                "a session named {name:?} already exists: {summary}. If you meant to continue it, send it a message \
                 with session_send; otherwise pick another name."
            )));
        }
        if table.unreadable.contains(name) {
            return Err(SessionError::Invalid(format!(
                "the name {name:?} is taken by a session record devbox-mcp cannot read; pick another name (the \
                 unreadable record is left alone for a human to look at)"
            )));
        }
        let workdir = match workdir {
            Some(dir) => PathBuf::from(dir),
            None => self.config.workdir_root.join(name),
        };
        if !workdir.is_absolute() {
            return Err(SessionError::Invalid(format!(
                "workdir must be an absolute path, got {:?}; omit it to use the default directory for this session",
                workdir.display().to_string()
            )));
        }
        std::fs::create_dir_all(&workdir).map_err(|e| {
            SessionError::Invalid(format!(
                "cannot use {} as the working directory: {e}",
                workdir.display()
            ))
        })?;
        if !workdir.is_dir() {
            return Err(SessionError::Invalid(format!(
                "{} exists but is not a directory; choose another workdir",
                workdir.display()
            )));
        }
        let record = Record {
            name: name.to_owned(),
            agent: agent.unwrap_or(Agent::Codex),
            workdir,
            model: codex::DEFAULT_MODEL.to_owned(),
            effort: codex::DEFAULT_EFFORT.to_owned(),
            native_id: None,
            turn: None,
        };
        self.store.write(&record).map_err(|e| {
            SessionError::Internal(format!("could not save the new session: {e:#}"))
        })?;
        let session = Session { record };
        let summary = session.summary();
        table.sessions.insert(name.to_owned(), session);
        Ok(summary)
    }

    /// Lists sessions in name order, a page at a time, starting after the name `after` (SPEC.md `SES-4`).
    pub fn list(&self, after: Option<&str>) -> SessionPage {
        let table = self.table.lock().unwrap();
        let mut iter = table
            .sessions
            .iter()
            .filter(|(name, _)| after.is_none_or(|a| name.as_str() > a));
        let sessions: Vec<SessionSummary> = iter
            .by_ref()
            .take(LIST_PAGE)
            .map(|(_, s)| s.summary())
            .collect();
        let next_after = if iter.next().is_some() {
            sessions.last().map(|s| s.name.clone())
        } else {
            None
        };
        SessionPage {
            sessions,
            next_after,
        }
    }
}
