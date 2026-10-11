//! The session table: every session devbox-mcp knows, in memory, backed by [`Store`] records.
//!
//! Tool handlers call into this; it owns the rules of SPEC.md "Sessions" (names, working directories, listing) and
//! "Turns" (sends, reads with cursors, interrupts, request IDs), runs each turn as a background task, and keeps memory
//! and the records in step. The table sits behind a `std::sync::Mutex` that is never held across an
//! `.await`, so on devbox-mcp's single-threaded runtime it is never contended; it exists to satisfy the type system
//! and to keep each operation's read-modify-write atomic.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use rmcp::schemars;
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use tokio::sync::oneshot;

use crate::codex::{self, Codex, Outcome, Progress, TurnRequest, TurnResult};
use crate::events::{Event, EventKind, Keep, MAX_TEXT_BYTES, truncate};
use crate::store::{AcceptedRequest, Agent, Record, Store, TurnRecord, TurnState};

/// Most sessions one `session_list` call returns (SPEC_impl.md, "Tools").
pub const LIST_PAGE: usize = 50;

/// Longest final-message preview in a session summary, in bytes.
const PREVIEW_BYTES: usize = 2 * 1024;

/// Settings fixed at startup.
pub struct Config {
    /// Default working directories are `<workdir_root>/<name>` (SPEC.md `SES-3`).
    pub workdir_root: PathBuf,
    pub codex: Codex,
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
    /// (SPEC_impl.md, "Tools"). Startup time plus PID: unique enough for that, without a randomness dependency.
    epoch: String,
}

/// The sessions known in memory, plus the names blocked by unreadable records.
struct Table {
    sessions: BTreeMap<String, Session>,
    /// Names of records that could not be parsed; such a name cannot be reused until the file is dealt with.
    unreadable: BTreeSet<String>,
}

/// A session's durable record plus what only lives in memory: the event stream of its most recent turn and the handle
/// of a running turn.
struct Session {
    record: Record,
    /// Events of the most recent turn that ran in this process, oldest first, at most [`MAX_HELD_EVENTS`].
    events: VecDeque<Event>,
    /// Sequence number the next event gets; counts up from 1 within this process.
    next_seq: u64,
    /// Sequence number of the most recent turn's first event, if that turn ran in this process.
    turn_start_seq: Option<u64>,
    last_event_at: Option<DateTime<Utc>>,
    /// Present while a turn's task is running, until the turn is settled.
    running: Option<Running>,
}

/// The handle of a running turn.
struct Running {
    /// Taken (and fired) by the first interrupt.
    interrupt: Option<oneshot::Sender<()>>,
}

impl Session {
    fn new(record: Record) -> Self {
        Self {
            record,
            events: VecDeque::new(),
            next_seq: 1,
            turn_start_seq: None,
            last_event_at: None,
            running: None,
        }
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
        let sessions = loaded
            .records
            .into_iter()
            .map(|record| (record.name.clone(), Session::new(record)))
            .collect();
        let epoch = format!(
            "{:x}{:x}",
            Utc::now().timestamp_nanos_opt().unwrap_or_default(),
            std::process::id()
        );
        Ok(Self {
            store,
            config,
            epoch,
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
            agent: agent.unwrap_or(Agent::Codex),
            workdir,
            model: codex::DEFAULT_MODEL.to_owned(),
            effort: codex::DEFAULT_EFFORT.to_owned(),
            native_id: None,
            turn: None,
            requests: Vec::new(),
        };
        self.store.write(&record).map_err(|e| {
            SessionError::Internal(format!("could not save the new session: {e:#}"))
        })?;
        let session = Session::new(record);
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
    /// True when the agent could not continue its earlier conversation in this session, because devbox-mcp never
    /// learned that conversation's ID (the previous turn failed or was stopped before the agent got going, or
    /// devbox-mcp restarted early in it), so this turn starts a fresh conversation without the earlier context.
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
    fn cursor_before_turn(&self, turn_id: u64, epoch: &str) -> Option<String> {
        let current = self.record.turn.as_ref().map(|t| t.id);
        match self.turn_start_seq {
            Some(start) if current == Some(turn_id) && start >= self.oldest_held() => {
                Some(cursor(&self.record.name, epoch, start - 1))
            }
            _ => None,
        }
    }
}

/// Formats a cursor: the session, the issuing process's epoch, and the last event seen. Session names cannot contain
/// `:` (`SES-2`), so splitting from the right is unambiguous.
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
                cursor: session.cursor_before_turn(earlier.turn_id, &self.epoch),
                context_reset: earlier.context_reset,
            });
        }

        if let Some(turn) = &session.record.turn
            && !turn.state.is_terminal()
        {
            return Err(SessionError::Invalid(format!(
                "session {name:?} is busy: turn {} is still {}. Poll session_read until it ends, or stop it with \
                 session_interrupt, then send again",
                turn.id,
                serde_json::to_value(turn.state)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default()
            )));
        }

        // A conversation that once existed but whose native ID devbox-mcp never learned (the first turn was lost before
        // the agent reported it) cannot be resumed; this turn starts a fresh one and says so (`SES-5`).
        let context_reset = session.record.native_id.is_none() && session.record.turn.is_some();
        let turn_id = session.record.turn.as_ref().map_or(1, |t| t.id + 1);
        let mut record = session.record.clone();
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
            cursor: session.cursor_before_turn(turn_id, &self.epoch),
            context_reset,
        };
        let workdir = session.record.workdir.clone();
        let model = session.record.model.clone();
        let effort = session.record.effort.clone();
        let resume = session.record.native_id.clone();
        drop(table);

        // The turn belongs to the session, not to this call (`TRN-1`): it is a task of its own, which only an
        // interrupt or devbox-mcp's shutdown (dropping the task, which kills the agent) can end early.
        let this = Arc::clone(self);
        let name = name.to_owned();
        let message = message.to_owned();
        tokio::spawn(async move {
            let req = TurnRequest {
                prompt: &message,
                workdir: &workdir,
                model: &model,
                effort: &effort,
                resume: resume.as_deref(),
            };
            let interrupt = async {
                // A dropped sender (the turn's bookkeeping going away) is not an interrupt.
                if interrupt_rx.await.is_err() {
                    std::future::pending::<()>().await;
                }
            };
            let outcome = this
                .config
                .codex
                .run_turn(
                    &req,
                    |progress| this.progress(&name, turn_id, progress),
                    interrupt,
                )
                .await;
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
                if epoch != self.epoch || seq > newest || seq + 1 < oldest {
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
            next_cursor: cursor(name, &self.epoch, last),
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
