//! The session table: every session devbox-mcp knows, in memory, backed by [`Store`] records.
//!
//! Tool handlers call into this; it owns the rules of SPEC.md "Sessions" (names, working directories, model and effort,
//! listing, removal) and "Turns" (sends, reads with cursors, interrupts, request IDs), runs each turn as a background
//! task, and keeps memory and the records in step. The table sits behind a `std::sync::Mutex` that is never held across
//! an `.await`, so on devbox-mcp's single-threaded runtime it is never contended; it exists to satisfy the type system
//! and to keep each operation's read-modify-write atomic. The one call that waits (`remove`, for the agent's delete
//! command) releases it while waiting and marks the session instead.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use rmcp::schemars;
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use tokio::sync::oneshot;

use crate::claude::{self, Claude};
use crate::codex::{self, Codex};
use crate::events::{
    Event, EventKind, Keep, MAX_TEXT_BYTES, Outcome, Progress, TurnResult, truncate,
};
use crate::store::{AcceptedRequest, Agent, Record, Store, TurnRecord, TurnState};
use crate::transcript::{self, Page};

/// Most sessions one `session_list` call returns (SPEC_impl.md, "Tools").
pub const LIST_PAGE: usize = 50;

/// Longest final-message preview in a session summary, in bytes.
const PREVIEW_BYTES: usize = 2 * 1024;

/// Settings fixed at startup.
pub struct Config {
    /// Default working directories are `<workdir_root>/<name>` (SPEC.md `SES-3`).
    pub workdir_root: PathBuf,
    pub codex: Codex,
    pub claude: Claude,
}

/// A failed session operation, worded for the calling agent (SPEC.md `DOC-3`): what went wrong and what to do.
#[derive(Debug)]
pub enum SessionError {
    /// The caller asked for something invalid or impossible.
    Invalid(String),
    /// devbox-mcp could not do something it should have been able to (disk, filesystem).
    Internal(String),
}

/// All sessions of this devbox-mcp process: the single owner of session state, shared by every tool call and every
/// running turn.
///
/// The table lock is only ever held briefly and never across an `await` or an agent's run, so no call waits on a turn
/// (SPEC.md `BND-2`). Record writes happen under the lock, which keeps memory and disk in the same order.
pub struct Sessions {
    store: Store,
    config: Config,
    table: Mutex<Table>,
    /// Identifies this devbox-mcp process in cursors, so a cursor issued before a restart is recognized as such
    /// (SPEC_impl.md, "Tools"). Startup time plus PID: unique enough for that, without a randomness dependency. Each
    /// session's cursors carry it together with the session's incarnation (see [`Session::epoch`]).
    epoch: String,
}

/// The sessions known in memory, plus the names blocked by unreadable records.
struct Table {
    sessions: BTreeMap<String, Session>,
    /// Names of records that could not be parsed; such a name cannot be reused until the file is dealt with.
    unreadable: BTreeSet<String>,
    /// The incarnation the next session to enter the table gets; counts up across the process.
    next_incarnation: u64,
}

impl Table {
    /// Puts a session into the table as a new incarnation, with the cursor epoch that goes with it.
    fn insert(&mut self, process_epoch: &str, record: Record) -> &mut Session {
        let epoch = format!("{process_epoch}-{}", self.next_incarnation);
        self.next_incarnation += 1;
        let name = record.name.clone();
        self.sessions
            .insert(name.clone(), Session::new(record, epoch));
        self.sessions.get_mut(&name).expect("just inserted")
    }
}

/// A session's durable record plus what only lives in memory: the event stream of its most recent turn and the handle
/// of a running turn.
struct Session {
    record: Record,
    /// The cursor epoch of this incarnation of the session: the process's epoch plus a number given when the session
    /// entered the table. A session created under the name of a removed one gets a different number, so the removed
    /// session's cursors are treated like cursors from another process instead of silently matching the new session's
    /// sequence numbers (SPEC_impl.md, "Tools").
    epoch: String,
    /// Events of the most recent turn that ran in this process, oldest first, at most [`MAX_HELD_EVENTS`].
    events: VecDeque<Event>,
    /// Sequence number the next event gets; counts up from 1 within this incarnation of the session.
    next_seq: u64,
    /// Sequence number of the most recent turn's first event, if that turn ran in this process.
    turn_start_seq: Option<u64>,
    last_event_at: Option<DateTime<Utc>>,
    /// Present while a turn's task is running, until the turn is settled.
    running: Option<Running>,
    /// Set while `remove` waits for the agent to delete its data, so that no turn can start (and no second removal
    /// can begin) on a session that is going away. In memory only: it cannot outlive the wait, and a restart in the
    /// middle of one simply leaves the session in place.
    removing: bool,
}

/// The handle of a running turn.
struct Running {
    /// Taken (and fired) by the first interrupt.
    interrupt: Option<oneshot::Sender<()>>,
}

impl Session {
    fn new(record: Record, epoch: String) -> Self {
        Self {
            record,
            epoch,
            events: VecDeque::new(),
            next_seq: 1,
            turn_start_seq: None,
            last_event_at: None,
            running: None,
            removing: false,
        }
    }

    /// The error for a send or removal while this session is busy: its most recent turn is still running or being
    /// interrupted, or the session is being removed. `None` when it is free.
    fn busy_error(&self) -> Option<SessionError> {
        let name = &self.record.name;
        if self.removing {
            return Some(SessionError::Invalid(format!(
                "session {name:?} is being removed; check session_list in a moment to see whether it is gone"
            )));
        }
        let turn = self
            .record
            .turn
            .as_ref()
            .filter(|t| !t.state.is_terminal())?;
        Some(SessionError::Invalid(format!(
            "session {name:?} is busy: turn {} is still {}. Poll session_read until it ends, or stop it with \
             session_interrupt, then try again",
            turn.id,
            serde_json::to_value(turn.state)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default()
        )))
    }
}

// Results returned to callers. Doc comments on these types become the JSON Schema descriptions a calling model reads.

/// A session as `session_create` and `session_list` show it.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct SessionSummary {
    /// The session's name.
    pub name: String,
    /// The agent the session runs.
    pub agent: Agent,
    /// The model the agent runs, chosen when the session was created and fixed for its life.
    pub model: String,
    /// The reasoning effort the agent runs at, chosen when the session was created and fixed for its life.
    pub effort: String,
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
    /// Why the turn failed, for a failed turn: at most 4 KiB, ending in ` [cut]` when it was longer.
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
            model: self.record.model.clone(),
            effort: self.record.effort.clone(),
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
            "invalid session name {}: a name starts with a lowercase letter, contains only lowercase letters, \
             digits, '_', and '-', and is at most 64 characters long",
            shown(name)
        )))
    }
}

impl Sessions {
    /// Loads every session record from the store. Records that cannot be parsed are reported on stderr by the store,
    /// kept out of the table, and block their names (SPEC_impl.md, "Storage").
    pub fn new(store: Store, config: Config) -> anyhow::Result<Self> {
        let loaded = store.load()?;
        let epoch = format!(
            "{:x}{:x}",
            Utc::now().timestamp_nanos_opt().unwrap_or_default(),
            std::process::id()
        );
        let mut table = Table {
            sessions: BTreeMap::new(),
            unreadable: loaded.unreadable.into_iter().collect(),
            next_incarnation: 0,
        };
        for record in loaded.records {
            table.insert(&epoch, record);
        }
        Ok(Self {
            store,
            config,
            epoch,
            table: Mutex::new(table),
        })
    }

    /// Creates a session (SPEC.md `SES-1` to `SES-3`, `SES-7`). The session exists once its record is on disk; a
    /// working directory created before a failure is left in place.
    pub fn create(
        &self,
        name: &str,
        workdir: Option<&str>,
        agent: Option<Agent>,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> Result<SessionSummary, SessionError> {
        validate_name(name)?;
        let agent = agent.unwrap_or(Agent::Codex);
        let (default_model, default_effort) = match agent {
            Agent::Codex => (codex::DEFAULT_MODEL, codex::DEFAULT_EFFORT),
            Agent::Claude => (claude::DEFAULT_MODEL, claude::DEFAULT_EFFORT),
        };
        if let Some(model) = model {
            validate_model(model, default_model)?;
        }
        if let Some(effort) = effort {
            validate_effort(effort, default_effort)?;
            // Claude ignores an effort it does not know with only a warning on stderr, so the session would silently
            // run at Claude's default instead (SPEC.md `SES-7`). Codex rejects an unknown effort itself, per turn.
            if agent == Agent::Claude && !claude::EFFORTS.contains(&effort) {
                return Err(SessionError::Invalid(format!(
                    "invalid effort {} for a claude session: Claude accepts {}, or omit it to use the default ({})",
                    shown(effort),
                    claude::EFFORTS.join(", "),
                    claude::DEFAULT_EFFORT
                )));
            }
        }
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
                "workdir must be an absolute path, got {}; omit it to use the default directory for this session",
                shown(&workdir.display().to_string())
            )));
        }
        std::fs::create_dir_all(&workdir).map_err(|e| {
            SessionError::Invalid(format!(
                "cannot use {} as the working directory: {e}",
                shown(&workdir.display().to_string())
            ))
        })?;
        if !workdir.is_dir() {
            return Err(SessionError::Invalid(format!(
                "{} exists but is not a directory; choose another workdir",
                shown(&workdir.display().to_string())
            )));
        }
        let record = Record {
            name: name.to_owned(),
            agent,
            workdir,
            model: model.unwrap_or(default_model).to_owned(),
            effort: effort.unwrap_or(default_effort).to_owned(),
            native_id: None,
            turn: None,
            requests: Vec::new(),
            untracked_conversation: Some(false),
        };
        self.store.write(&record).map_err(|e| {
            SessionError::Internal(format!("could not save the new session: {e:#}"))
        })?;
        Ok(table.insert(&self.epoch, record).summary())
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

// ---------------------------------------------------------------------------------------------------------------------
// Turns: sending, following, interrupting (SPEC.md "Turns", SPEC_impl.md "Turn lifecycle")
// ---------------------------------------------------------------------------------------------------------------------

/// Longest message `session_send` accepts, in bytes.
pub const MAX_MESSAGE_BYTES: usize = 256 * 1024;

/// Most events one `session_read` returns.
pub const READ_PAGE: usize = 50;

/// Most events held per session; older ones are dropped and reads that need them get a gap (SPEC_impl.md, "Storage").
const MAX_HELD_EVENTS: usize = 10_000;

/// Longest final message kept for a turn.
const MAX_FINAL_MESSAGE_BYTES: usize = 32 * 1024;

/// Longest failure reason kept for a turn.
const MAX_ERROR_BYTES: usize = 4 * 1024;
/// Ends a failure reason that was cut to [`MAX_ERROR_BYTES`].
const CUT_MARK: &str = " [cut]";

/// What `session_remove` returns.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct RemoveResult {
    /// The session as it was just before it was removed.
    pub session: SessionSummary,
    /// True when the agent's own stored data for the session (its conversation) was deleted, or there never was any.
    /// False only for a forced removal, when that data could not be deleted or devbox-mcp could not tell; it may then
    /// remain in the agent's storage.
    pub agent_data_deleted: bool,
}

/// What `session_send` returns.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct SendResult {
    /// The turn this message started (or, for a retried request ID, started earlier).
    pub turn_id: u64,
    /// True when `request_id` matched an earlier send to this session: nothing new was started, and this is the turn
    /// that earlier send started.
    pub replayed: bool,
    /// Pass this to `session_read` to get the turn's events from its start. Null when those events are no longer held
    /// (a retried send of an older turn, or after devbox-mcp restarted); read without a cursor then, keeping in mind
    /// that a read always shows the session's most recent turn, which may be a later one than this.
    pub cursor: Option<String>,
    /// True when this turn starts a fresh conversation without the session's earlier context, because the agent could
    /// not continue the earlier one. For codex, devbox-mcp never learned that conversation's ID (the previous turn
    /// failed or was stopped before the agent got going, or devbox-mcp restarted early in it). For claude, the
    /// agent's own record of the conversation was gone, so the conversation starts over under the same ID.
    pub context_reset: bool,
}

/// What `session_read` returns.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ReadResult {
    /// Events after the cursor, oldest first, at most 50.
    pub events: Vec<Event>,
    /// Pass this as `cursor` on your next `session_read` to get only newer events.
    pub next_cursor: String,
    /// True when more events are already waiting; read again right away to get them, even if the turn has ended.
    pub more: bool,
    /// True when events may be missing between your cursor and the first event returned, for example because devbox-mcp
    /// restarted or the events were too old to keep. The turn state below is still current.
    pub gap: bool,
    /// The session's most recent turn, or null if nothing has been sent to it yet.
    pub turn: Option<TurnDetail>,
    /// Whether the agent is alive and recently active, to tell quiet work from a stuck or dead agent.
    pub liveness: Liveness,
}

/// The most recent turn in full, as `session_read` returns it.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct TurnDetail {
    /// The turn's number within its session, counting from 1.
    pub id: u64,
    /// `running` and `interrupting` mean the turn has not ended; `completed`, `failed`, `interrupted`, and `lost` are
    /// final. `lost` means devbox-mcp restarted while the turn ran, so how it ended is unknown.
    pub state: TurnState,
    /// When the turn started (RFC 3339, UTC).
    pub started_at: String,
    /// When the turn ended (RFC 3339, UTC), or null while it runs or when unknown (a lost turn).
    pub ended_at: Option<String>,
    /// The agent's final message (its reply), once the turn has ended; at most 32 KiB.
    pub final_message: Option<String>,
    /// True when the final message was longer than 32 KiB and has been cut.
    pub final_message_truncated: bool,
    /// Why the turn failed, for a failed turn: at most 4 KiB, ending in ` [cut]` when it was longer.
    pub error: Option<String>,
    /// True when the turn was stopped but devbox-mcp could not confirm that everything the agent had started has
    /// stopped.
    pub cleanup_uncertain: bool,
}

/// How alive the most recent turn's agent is.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Liveness {
    /// True while the turn is running (for a few seconds at its end, the agent may already have exited while its last
    /// output is collected); null when no turn is running.
    pub agent_alive: Option<bool>,
    /// Seconds since the last event, or null when devbox-mcp holds no events for this session (for example after a
    /// restart). A long gap while `agent_alive` is true usually means the agent is busy with a long command.
    pub last_event_age_seconds: Option<i64>,
    /// How long the turn has been running, or how long it ran once it ended; null when unknown (a lost turn).
    pub turn_duration_seconds: Option<i64>,
}

impl Session {
    /// Reports a failed record write to the caller as a non-fatal `error` event: the turn goes on, but the caller learns
    /// what a restart would lose.
    fn push_storage_error(&mut self, message: &str) {
        let (message, cut) = truncate(message, MAX_TEXT_BYTES, Keep::Head);
        self.push_event(
            EventKind::Error {
                message,
                fatal: false,
            },
            cut,
        );
    }

    /// Appends an event to the session's stream, dropping the oldest held event when the stream is full.
    fn push_event(&mut self, kind: EventKind, truncated: bool) {
        let now = Utc::now();
        let turn_id = self.record.turn.as_ref().map_or(0, |t| t.id);
        self.events.push_back(Event {
            seq: self.next_seq,
            turn_id,
            time: now.to_rfc3339(),
            truncated,
            kind,
        });
        self.next_seq += 1;
        self.last_event_at = Some(now);
        if self.events.len() > MAX_HELD_EVENTS {
            self.events.pop_front();
        }
    }

    /// Sequence number of the oldest event held, or of the next event when none is held.
    fn oldest_held(&self) -> u64 {
        self.events.front().map_or(self.next_seq, |e| e.seq)
    }

    /// A cursor positioned just before `turn_id`'s first event, if that turn's events are still held.
    fn cursor_before_turn(&self, turn_id: u64) -> Option<String> {
        let current = self.record.turn.as_ref().map(|t| t.id);
        match self.turn_start_seq {
            Some(start) if current == Some(turn_id) && start >= self.oldest_held() => {
                Some(cursor(&self.record.name, &self.epoch, start - 1))
            }
            _ => None,
        }
    }
}

/// Formats a cursor: the session, the epoch of the session incarnation that issued it (see [`Session::epoch`]), and the
/// last event seen. Session names cannot contain `:` (`SES-2`), and epochs do not either, so splitting from the right is
/// unambiguous.
fn cursor(name: &str, epoch: &str, seq: u64) -> String {
    format!("{name}:{epoch}:{seq}")
}

/// Parses a cursor into its session, epoch, and sequence number.
fn parse_cursor(cursor: &str) -> Option<(&str, &str, u64)> {
    let mut parts = cursor.rsplitn(3, ':');
    let seq = parts.next()?.parse().ok()?;
    let epoch = parts.next()?;
    let name = parts.next()?;
    Some((name, epoch, seq))
}

/// Hashes a message for request-ID replay checks, so a record keeps the fact that a message was sent without storing
/// the (up to 256 KiB) message itself.
fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// How a send's turn joins the agent's conversation, settled before the turn is accepted because `context_reset` is
/// part of the send's result.
struct ConversationStart {
    /// The native ID saved with the turn: for Codex the one already learned, if any (Codex reports a new one when it
    /// starts); for Claude always set, since devbox-mcp chooses it.
    native_id: Option<String>,
    /// Claude only: continue the conversation (`--resume`) rather than start it under `native_id`.
    resume: bool,
    /// The turn starts a fresh conversation although an earlier turn ran, so the agent has lost what came before
    /// (SPEC.md `SES-5`).
    context_reset: bool,
}

/// Decides how the next turn of `record`'s session joins its conversation (SPEC_impl.md, "Claude" and "Storage").
///
/// Codex: resume the learned ID; without one, a session past its first turn lost its conversation (the first turn
/// ended before Codex reported it), so the turn starts fresh and says so. Claude: the first send chooses the ID; later
/// sends resume it if Claude's transcript is there, start it again under the same ID if the lookup completed and found
/// none, and fail the send if the lookup failed or found several, since resuming the wrong one or silently starting over
/// would both mislead the caller.
///
/// The Claude lookup lists Claude's `projects/` directory and checks one path in each entry. It runs with the table lock
/// held, which blocks devbox-mcp's single thread for that long (about 200 entries on the box this was written on, well
/// under a millisecond to a few milliseconds); doing it outside the lock would need the send's checks done twice.
fn conversation_start(claude: &Claude, record: &Record) -> Result<ConversationStart, SessionError> {
    let earlier_turn = record.turn.is_some();
    match (record.agent, &record.native_id) {
        (Agent::Codex, native_id) => Ok(ConversationStart {
            native_id: native_id.clone(),
            resume: native_id.is_some(),
            context_reset: native_id.is_none() && earlier_turn,
        }),
        (Agent::Claude, None) => {
            let id = claude::new_session_id().map_err(|e| {
                SessionError::Internal(format!(
                    "could not choose an ID for Claude's conversation, so nothing was sent: {e}"
                ))
            })?;
            Ok(ConversationStart {
                native_id: Some(id),
                resume: false,
                context_reset: earlier_turn,
            })
        }
        (Agent::Claude, Some(id)) => {
            let found = claude.find_transcripts(id).map_err(|e| {
                SessionError::Internal(format!(
                    "could not look for Claude's transcript of this session's conversation, so nothing was sent and \
                     Claude was not started: {e}"
                ))
            })?;
            match found.len() {
                1 => Ok(ConversationStart {
                    native_id: Some(id.clone()),
                    resume: true,
                    context_reset: false,
                }),
                0 => Ok(ConversationStart {
                    native_id: Some(id.clone()),
                    resume: false,
                    context_reset: earlier_turn,
                }),
                n => Err(SessionError::Internal(format!(
                    "found {n} transcripts of this session's Claude conversation under {}, so devbox-mcp cannot tell \
                     which one to continue; nothing was sent. A human needs to look at Claude's files; removing the \
                     session (session_remove) deletes all of them",
                    claude.home.join("projects").display()
                ))),
            }
        }
    }
}

/// Checks a caller-chosen model name (SPEC_impl.md, "Tools"): 1 to 128 characters from `A-Z a-z 0-9 . _ : / -`.
///
/// This is only about what can be handed to the agent safely, not about what the agent supports: devbox-mcp keeps no
/// model list (SPEC.md `SES-7`), so a model the agent does not know makes the session's turns fail with the agent's own
/// error instead.
fn validate_model(model: &str, default: &str) -> Result<(), SessionError> {
    let ok = !model.is_empty()
        && model.len() <= 128
        && model
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '/' | '-'));
    if ok {
        Ok(())
    } else {
        Err(SessionError::Invalid(format!(
            "invalid model {}: use 1 to 128 characters from A-Z, a-z, 0-9, '.', '_', ':', '/', and '-', or omit it to \
             use the default ({})",
            shown(model),
            default
        )))
    }
}

/// Checks a caller-chosen reasoning effort (SPEC_impl.md, "Tools"): 1 to 32 characters from `a-z 0-9 _ -`.
///
/// The effort goes into a TOML override on Codex's command line (`-c model_reasoning_effort="<effort>"`), so the
/// characters are restricted to ones that cannot break out of that string. Which efforts a model supports is the
/// agent's business, as for the model.
fn validate_effort(effort: &str, default: &str) -> Result<(), SessionError> {
    let ok = !effort.is_empty()
        && effort.len() <= 32
        && effort
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(SessionError::Invalid(format!(
            "invalid effort {}: use 1 to 32 characters from a-z, 0-9, '_', and '-' (for example low, medium, high, \
             xhigh), or omit it to use the default ({})",
            shown(effort),
            default
        )))
    }
}

/// Checks a caller-chosen request ID: 1 to 128 characters from `A-Z a-z 0-9 . _ : -`, so it is safe to echo in
/// messages and to store.
fn validate_request_id(id: &str) -> Result<(), SessionError> {
    let ok = !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'));
    if ok {
        Ok(())
    } else {
        Err(SessionError::Invalid(format!(
            "invalid request_id {}: use 1 to 128 characters from A-Z, a-z, 0-9, '.', '_', ':', and '-'",
            shown(id)
        )))
    }
}

/// Quotes a caller-supplied value for an error message, cut to 128 bytes so an error never echoes an arbitrarily
/// large argument back (SPEC.md `BND-1`).
fn shown(value: &str) -> String {
    let (head, cut) = truncate(value, 128, Keep::Head);
    if cut {
        format!("{head:?}...")
    } else {
        format!("{head:?}")
    }
}

/// The error for an unknown session name, worded to point a calling model at the tools that fix the problem.
fn not_found(name: &str) -> SessionError {
    SessionError::Invalid(format!(
        "no session named {}; create it with session_create, or find existing sessions with session_list",
        shown(name)
    ))
}

impl Sessions {
    /// Sends a message, starting a turn in the background (SPEC.md `TRN-1`, `TRN-2`, `TRN-6`, `SES-5`).
    ///
    /// Checks come in the order of SPEC_impl.md "Tools": unknown session, a known request ID (replayed whatever the
    /// session is doing), busy, then the record write. The turn is accepted only once its record is on disk; if that
    /// write fails, nothing runs.
    pub fn send(
        self: &Arc<Self>,
        name: &str,
        message: &str,
        request_id: Option<&str>,
    ) -> Result<SendResult, SessionError> {
        if message.is_empty() {
            return Err(SessionError::Invalid(
                "message is empty; send the text the agent should act on".to_owned(),
            ));
        }
        if message.len() > MAX_MESSAGE_BYTES {
            return Err(SessionError::Invalid(format!(
                "message is {} bytes; the limit is {MAX_MESSAGE_BYTES} bytes. Put long material in a file in the \
                 session's working directory and refer to it instead",
                message.len()
            )));
        }
        if let Some(id) = request_id {
            validate_request_id(id)?;
        }
        let digest = sha256_hex(message);

        let mut table = self.table.lock().unwrap();
        let session = table
            .sessions
            .get_mut(name)
            .ok_or_else(|| not_found(name))?;

        if let Some(id) = request_id
            && let Some(earlier) = session.record.requests.iter().find(|r| r.request_id == id)
        {
            if earlier.message_sha256 != digest {
                return Err(SessionError::Invalid(format!(
                    "request_id {id:?} was already used for a different message in this session (turn {}); use a \
                     new request_id for a new message",
                    earlier.turn_id
                )));
            }
            return Ok(SendResult {
                turn_id: earlier.turn_id,
                replayed: true,
                cursor: session.cursor_before_turn(earlier.turn_id),
                context_reset: earlier.context_reset,
            });
        }

        if let Some(busy) = session.busy_error() {
            return Err(busy);
        }

        let start = conversation_start(&self.config.claude, &session.record)?;
        let context_reset = start.context_reset;
        let turn_id = session.record.turn.as_ref().map_or(1, |t| t.id + 1);
        let mut record = session.record.clone();
        record.native_id = start.native_id.clone();
        // Saved with the turn itself, so a removal can never miss that an earlier conversation may still exist. Only a
        // Codex reset leaves one behind: a Claude restart reuses the ID devbox-mcp chose, so nothing becomes untracked.
        record.untracked_conversation = Some(
            record.may_have_untracked_conversation()
                || (context_reset && record.agent == Agent::Codex),
        );
        record.turn = Some(TurnRecord {
            id: turn_id,
            state: TurnState::Running,
            started_at: Utc::now(),
            ended_at: None,
            final_message: None,
            final_message_truncated: false,
            error: None,
            cleanup_uncertain: false,
        });
        if let Some(id) = request_id {
            record.requests.push(AcceptedRequest {
                request_id: id.to_owned(),
                message_sha256: digest,
                turn_id,
                context_reset,
            });
        }
        self.store.write(&record).map_err(|e| {
            SessionError::Internal(format!(
                "could not save the new turn, so nothing was sent: {e:#}"
            ))
        })?;
        session.record = record;

        // The stream holds the most recent turn only, so the previous turn's events go now.
        session.events.clear();
        session.turn_start_seq = Some(session.next_seq);
        let (text, cut) = truncate(message, MAX_TEXT_BYTES, Keep::Head);
        session.push_event(EventKind::TurnStarted { message: text }, cut);
        let (interrupt_tx, interrupt_rx) = oneshot::channel();
        session.running = Some(Running {
            interrupt: Some(interrupt_tx),
        });

        let result = SendResult {
            turn_id,
            replayed: false,
            cursor: session.cursor_before_turn(turn_id),
            context_reset,
        };
        let agent = session.record.agent;
        let workdir = session.record.workdir.clone();
        let model = session.record.model.clone();
        let effort = session.record.effort.clone();
        drop(table);

        // The turn belongs to the session, not to this call (`TRN-1`): it is a task of its own, which only an
        // interrupt or devbox-mcp's shutdown (dropping the task, which kills the agent) can end early.
        let this = Arc::clone(self);
        let name = name.to_owned();
        let message = message.to_owned();
        tokio::spawn(async move {
            let interrupt = async {
                // A dropped sender (the turn's bookkeeping going away) is not an interrupt.
                if interrupt_rx.await.is_err() {
                    std::future::pending::<()>().await;
                }
            };
            let on_progress = |progress| this.progress(&name, turn_id, progress);
            let outcome = match (agent, &start.native_id) {
                (Agent::Codex, resume) => {
                    let req = codex::TurnRequest {
                        prompt: &message,
                        workdir: &workdir,
                        model: &model,
                        effort: &effort,
                        resume: resume.as_deref(),
                    };
                    this.config
                        .codex
                        .run_turn(&req, on_progress, interrupt)
                        .await
                }
                (Agent::Claude, Some(session_id)) => {
                    let req = claude::TurnRequest {
                        prompt: &message,
                        workdir: &workdir,
                        model: &model,
                        effort: &effort,
                        session_id,
                        resume: start.resume,
                    };
                    this.config
                        .claude
                        .run_turn(&req, on_progress, interrupt)
                        .await
                }
                (Agent::Claude, None) => {
                    unreachable!("conversation_start always chooses a Claude conversation ID")
                }
            };
            this.finish(&name, turn_id, outcome);
        });
        Ok(result)
    }

    /// Records what a running turn reports: its native session ID (saved at once, so a later restart can still resume
    /// the conversation) and its events.
    fn progress(&self, name: &str, turn_id: u64, progress: Progress) {
        let mut table = self.table.lock().unwrap();
        let Some(session) = table.sessions.get_mut(name) else {
            return;
        };
        if session.record.turn.as_ref().map(|t| t.id) != Some(turn_id) {
            return;
        }
        match progress {
            Progress::NativeId(id) => {
                if session.record.native_id.as_deref() != Some(id.as_str()) {
                    session.record.native_id = Some(id);
                    if let Err(e) = self.store.write(&session.record) {
                        // The caller must hear about it (SPEC_impl.md, "Storage"), and the record write at the end
                        // of the turn tries again.
                        session.push_storage_error(&format!(
                            "devbox-mcp could not save the agent's conversation ID, and tries again when the turn \
                             ends; if devbox-mcp restarts before then, the next turn starts a fresh conversation: {e:#}"
                        ));
                    }
                }
            }
            Progress::ForeignConversation => {
                // The agent ran in a conversation devbox-mcp did not ask for and may have stored data under its ID,
                // which removal cannot find. Saved now rather than with the turn's end, which a read error or a
                // shutdown can keep from ever being recorded.
                session.record.untracked_conversation = Some(true);
                if let Err(e) = self.store.write(&session.record) {
                    session.push_storage_error(&format!(
                        "devbox-mcp could not save that the agent ran in a conversation it did not ask for, and tries \
                         again when the turn ends; if devbox-mcp restarts before then, removing this session may \
                         report the agent's data deleted when some of it remains: {e:#}"
                    ));
                }
            }
            Progress::Event(event) => session.push_event(event.kind, event.truncated),
        }
    }

    /// Settles a turn once its agent is gone: final state, final message, and failure reason into the record, the
    /// `turn_ended` event into the stream. The session stays busy until this has happened (SPEC_impl.md, "Turn
    /// lifecycle").
    ///
    /// The final state comes from what actually happened: `interrupted` only if the interrupt stopped the agent. An
    /// agent that finished on its own just before an interrupt arrived keeps its own outcome.
    fn finish(&self, name: &str, turn_id: u64, outcome: anyhow::Result<Outcome>) {
        let mut table = self.table.lock().unwrap();
        let Some(session) = table.sessions.get_mut(name) else {
            return;
        };
        let (state, error, final_message, cleanup_uncertain) = match outcome {
            Err(e) => (TurnState::Failed, Some(format!("{e:#}")), None, false),
            Ok(outcome) => {
                let (state, error) = match outcome.result {
                    TurnResult::Completed => (TurnState::Completed, None),
                    TurnResult::Failed { error } => (TurnState::Failed, Some(error)),
                    TurnResult::Interrupted => (TurnState::Interrupted, None),
                };
                (
                    state,
                    error,
                    outcome.final_message,
                    !outcome.cleanup_confirmed,
                )
            }
        };
        // A cut failure reason says so in its own text, because the record keeps no separate flag for it and the reason
        // outlives the `turn_ended` event that could carry one.
        let (error, error_cut) = match error {
            Some(e) => {
                let (mut e, cut) = truncate(&e, MAX_ERROR_BYTES - CUT_MARK.len(), Keep::Head);
                if cut {
                    e.push_str(CUT_MARK);
                }
                (Some(e), cut)
            }
            None => (None, false),
        };
        let (final_message, final_message_truncated) = match final_message {
            Some(text) => {
                let (text, cut) = truncate(&text, MAX_FINAL_MESSAGE_BYTES, Keep::Head);
                (Some(text), cut)
            }
            None => (None, false),
        };
        let Some(turn) = session.record.turn.as_mut().filter(|t| t.id == turn_id) else {
            return;
        };
        turn.state = state;
        turn.ended_at = Some(Utc::now());
        turn.final_message = final_message;
        turn.final_message_truncated = final_message_truncated;
        turn.error = error.clone();
        turn.cleanup_uncertain = cleanup_uncertain;
        // Written before `turn_ended` is pushed, so a failure can be reported in the stream while `turn_ended` stays
        // the turn's last event. Memory stays right and the session's next write repairs the file; until then a restart
        // reports the turn as lost, which is honest about what was saved (SPEC_impl.md, "Storage").
        if let Err(e) = self.store.write(&session.record) {
            session.push_storage_error(&format!(
                "devbox-mcp could not save how this turn ended, so if it restarts before this session's next send, the \
                 turn will show as lost: {e:#}"
            ));
        }
        session.push_event(
            EventKind::TurnEnded {
                state,
                error,
                cleanup_uncertain,
            },
            error_cut,
        );
        session.running = None;
    }

    /// Reads a page of the agent's own transcript of the session's conversation (SPEC.md `TRA-2`, SPEC_impl.md
    /// "Tools"). The transcript is found and read on a blocking thread, since it can run to megabytes; the table lock
    /// is held only to look up the session's conversation ID.
    pub async fn transcript(
        &self,
        name: &str,
        from: Option<u64>,
        limit: Option<usize>,
    ) -> Result<Page, SessionError> {
        let limit = limit.unwrap_or(transcript::DEFAULT_LIMIT);
        if !(1..=transcript::MAX_LIMIT).contains(&limit) {
            return Err(SessionError::Invalid(format!(
                "limit must be 1 to {}, got {limit}",
                transcript::MAX_LIMIT
            )));
        }
        let (agent, native_id) = {
            let table = self.table.lock().unwrap();
            let session = table.sessions.get(name).ok_or_else(|| not_found(name))?;
            let record = &session.record;
            match &record.native_id {
                Some(id) => (record.agent, id.clone()),
                None if record.turn.is_none() => {
                    return Err(SessionError::Invalid(format!(
                        "session {name:?} has no transcript yet: nothing has been sent to it"
                    )));
                }
                None if session.running.is_some() => {
                    return Err(SessionError::Invalid(format!(
                        "session {name:?} has no transcript devbox-mcp can find yet: the agent has not reported its \
                         conversation ID, which it does as it starts. Follow the turn with session_read and try again \
                         once it shows the agent's first events"
                    )));
                }
                None => {
                    return Err(SessionError::Invalid(format!(
                        "session {name:?} has no transcript devbox-mcp can find: its turn ended before the agent \
                         reported its conversation ID"
                    )));
                }
            }
        };
        let codex = self.config.codex.clone();
        let claude = self.config.claude.clone();
        let read = tokio::task::spawn_blocking(move || {
            let path = match agent {
                Agent::Codex => codex.transcript_path(&native_id)?,
                Agent::Claude => claude.transcript_path(&native_id)?,
            };
            transcript::read(&path, from, limit)
        })
        .await
        .map_err(|e| SessionError::Internal(format!("reading the transcript failed: {e}")))?;
        read.map_err(SessionError::Internal)
    }

    /// Removes a session (SPEC.md `SES-6`, SPEC_impl.md "Tools"): deletes the agent's stored data for it, then its
    /// record, and forgets it; its working directory is left alone.
    ///
    /// Refused while the session is busy. Without `force`, also refused when deletion cannot be confirmed: the session
    /// had a turn but no native ID was ever learned, or an earlier conversation was orphaned by a context reset (both
    /// checked before anything is deleted), or the agent's delete command fails. A failure keeps the session, though
    /// the agent may already have deleted part of its data. With `force`, the session is forgotten anyway and the
    /// result says the agent's data may remain.
    ///
    /// This is the one call that waits on a subprocess (the agent's delete command, bounded by
    /// [`codex::DELETE_TIMEOUT`]). The table lock is not held during that wait; the session's `removing` flag keeps
    /// sends and other removals out instead, and since nothing can create, remove, or start a turn on the name while it
    /// is set, nothing needs re-checking afterwards. rmcp does not drop a handler when the client cancels the call, so
    /// the flag is always cleared on the way out; only shutdown drops this future, and the flag dies with the process.
    pub async fn remove(&self, name: &str, force: bool) -> Result<RemoveResult, SessionError> {
        let (agent, native_id, confirmable) = {
            let mut table = self.table.lock().unwrap();
            let session = table
                .sessions
                .get_mut(name)
                .ok_or_else(|| not_found(name))?;
            if let Some(busy) = session.busy_error() {
                return Err(busy);
            }
            let record = &session.record;
            let confirmable = record.turn.is_none()
                || (record.native_id.is_some() && !record.may_have_untracked_conversation());
            if !confirmable && !force {
                let why = if record.may_have_untracked_conversation() {
                    "an earlier conversation of this session may remain whose ID devbox-mcp never learned (it was \
                     left behind by a context reset, or the record predates devbox-mcp keeping track of that)"
                } else {
                    "the agent never reported an ID for this session's conversation"
                };
                return Err(SessionError::Invalid(format!(
                    "cannot confirm that removing session {name:?} would delete all of the agent's stored data: \
                     {why}, so that data may remain in the agent's storage and devbox-mcp cannot delete it. The \
                     session is kept. Pass force: true to remove it anyway"
                )));
            }
            session.removing = true;
            (record.agent, record.native_id.clone(), confirmable)
        };

        let deleted = match (agent, &native_id) {
            (Agent::Codex, Some(id)) => self.config.codex.delete(id, codex::DELETE_TIMEOUT).await,
            (Agent::Claude, Some(id)) => {
                let claude = self.config.claude.clone();
                let id = id.clone();
                tokio::task::spawn_blocking(move || claude.delete(&id))
                    .await
                    .unwrap_or_else(|e| Err(format!("deleting Claude's files failed: {e}")))
            }
            (_, None) => Ok(()),
        };

        let mut table = self.table.lock().unwrap();
        let session = table
            .sessions
            .get_mut(name)
            .expect("a session being removed stays in the table");
        if let Err(e) = &deleted
            && !force
        {
            session.removing = false;
            return Err(SessionError::Internal(format!(
                "could not delete the agent's stored data for session {name:?}, so the session is kept: {e}. The agent \
                 may already have deleted part of that data. Try again, or pass force: true to remove the session \
                 anyway, leaving whatever the agent still holds"
            )));
        }
        let summary = session.summary();
        if let Err(e) = self.store.remove(name) {
            session.removing = false;
            return Err(SessionError::Internal(format!(
                "could not delete session {name:?}'s record, so the session is kept: {e:#}. The agent's own data for \
                 it {} already deleted; try again to finish the removal",
                if native_id.is_some() && deleted.is_ok() {
                    "was"
                } else {
                    "may be"
                }
            )));
        }
        table.sessions.remove(name);
        Ok(RemoveResult {
            session: summary,
            agent_data_deleted: confirmable && deleted.is_ok(),
        })
    }

    /// Starts interrupting a session's running turn and returns at once (SPEC.md `TRN-4`, `BND-2`). The turn reports
    /// `interrupting` until its agent and everything it started are gone; callers follow it with `session_read`.
    pub fn interrupt(&self, name: &str) -> Result<TurnSummary, SessionError> {
        let mut table = self.table.lock().unwrap();
        let session = table
            .sessions
            .get_mut(name)
            .ok_or_else(|| not_found(name))?;
        let Some(turn) = session.record.turn.as_mut() else {
            return Err(SessionError::Invalid(format!(
                "session {name:?} has no turns yet, so there is nothing to interrupt"
            )));
        };
        if turn.state == TurnState::Running
            && let Some(running) = session.running.as_mut()
            && let Some(tx) = running.interrupt.take()
        {
            // The record keeps saying `running`: both map to `lost` after a restart, so writing it would add nothing.
            turn.state = TurnState::Interrupting;
            let _ = tx.send(());
        }
        Ok(TurnSummary::from_record(turn))
    }

    /// Returns a session's events after a cursor, with the most recent turn and liveness (SPEC.md `TRN-3`, `TRN-7`).
    /// Never waits (`BND-2`).
    ///
    /// Cursors have two outcomes when they cannot be followed exactly (SPEC_impl.md, "Tools"): an unparseable cursor or
    /// one for another session is an error; any other cursor that cannot be honored (from another process, older than
    /// what is held, or beyond the newest event) reports a gap and continues from the oldest event held.
    pub fn read(&self, name: &str, cursor_arg: Option<&str>) -> Result<ReadResult, SessionError> {
        let table = self.table.lock().unwrap();
        let session = table.sessions.get(name).ok_or_else(|| not_found(name))?;
        let newest = session.next_seq - 1;
        let oldest = session.oldest_held();

        let (after, gap) = match cursor_arg {
            None => match session.turn_start_seq {
                Some(start) if start >= oldest => (start - 1, false),
                Some(_) => (oldest - 1, true),
                // No turn has run in this process: either there was never a turn, or its events died with an earlier
                // devbox-mcp.
                None => (newest, session.record.turn.is_some()),
            },
            Some(c) => {
                let Some((cursor_name, epoch, seq)) = parse_cursor(c) else {
                    return Err(SessionError::Invalid(format!(
                        "malformed cursor {}; pass a cursor exactly as returned, or omit it to read from the start \
                         of the most recent turn",
                        shown(c)
                    )));
                };
                if cursor_name != name {
                    return Err(SessionError::Invalid(format!(
                        "cursor {} belongs to session {}, not {name:?}",
                        shown(c),
                        shown(cursor_name)
                    )));
                }
                if epoch != session.epoch || seq > newest || seq + 1 < oldest {
                    (oldest - 1, true)
                } else {
                    (seq, false)
                }
            }
        };

        let mut pending = session.events.iter().filter(|e| e.seq > after);
        let events: Vec<Event> = pending.by_ref().take(READ_PAGE).cloned().collect();
        let more = pending.next().is_some();
        let last = events.last().map_or(after, |e| e.seq);

        let now = Utc::now();
        let turn = session.record.turn.as_ref();
        let liveness = Liveness {
            agent_alive: session.running.as_ref().map(|_| true),
            last_event_age_seconds: session.last_event_at.map(|t| (now - t).num_seconds()),
            turn_duration_seconds: turn.and_then(|t| match (t.state.is_terminal(), t.ended_at) {
                (false, _) => Some((now - t.started_at).num_seconds()),
                (true, Some(end)) => Some((end - t.started_at).num_seconds()),
                (true, None) => None,
            }),
        };
        Ok(ReadResult {
            events,
            next_cursor: cursor(name, &session.epoch, last),
            more,
            gap,
            turn: turn.map(TurnDetail::from_record),
            liveness,
        })
    }
}

impl TurnDetail {
    fn from_record(turn: &TurnRecord) -> Self {
        Self {
            id: turn.id,
            state: turn.state,
            started_at: turn.started_at.to_rfc3339(),
            ended_at: turn.ended_at.map(|t| t.to_rfc3339()),
            final_message: turn.final_message.clone(),
            final_message_truncated: turn.final_message_truncated,
            error: turn.error.clone(),
            cleanup_uncertain: turn.cleanup_uncertain,
        }
    }
}
