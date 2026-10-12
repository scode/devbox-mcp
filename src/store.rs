//! Durable session records: one JSON file per session under the state directory (SPEC_impl.md, "Storage").
//!
//! The in-memory session table is the source of truth while devbox-mcp runs; these files exist so sessions and their
//! latest turn survive a restart (`SES-5`, `SES-8`). Every change rewrites a whole record synchronously, and devbox-mcp
//! runs a single-threaded runtime, so writes can never interleave and need no coordination beyond being done without
//! an `.await` in the middle.

use std::fs::{self, File};
use std::io::Write as _;
use std::os::fd::AsRawFd as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use chrono::{DateTime, Utc};
use rmcp::schemars;
use serde::{Deserialize, Serialize};

/// The agents a session can run. Only Codex so far (SPEC.md `SES-7`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Agent {
    /// OpenAI's Codex CLI.
    Codex,
}

/// The states a turn can be in (SPEC.md `TRN-5`, SPEC_impl.md "Turn lifecycle").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TurnState {
    /// The agent is working on the turn.
    Running,
    /// An interrupt was accepted; the agent's processes are being stopped.
    Interrupting,
    /// The agent finished the turn normally.
    Completed,
    /// The turn ended in an error; see the turn's `error`.
    Failed,
    /// The turn was interrupted and the agent's processes have stopped.
    Interrupted,
    /// devbox-mcp restarted while the turn was running, so how it ended is unknown.
    Lost,
}

impl TurnState {
    pub fn is_terminal(self) -> bool {
        !matches!(self, TurnState::Running | TurnState::Interrupting)
    }
}

/// The durable record of a session's most recent turn (`SES-8`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnRecord {
    pub id: u64,
    pub state: TurnState,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    /// At most 32 KiB, cut with `final_message_truncated` set.
    pub final_message: Option<String>,
    #[serde(default)]
    pub final_message_truncated: bool,
    /// Why a failed turn failed, at most 4 KiB.
    pub error: Option<String>,
    #[serde(default)]
    pub cleanup_uncertain: bool,
}

/// One session's durable state, as stored in `<state-dir>/sessions/<name>.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub name: String,
    pub agent: Agent,
    pub workdir: PathBuf,
    pub model: String,
    pub effort: String,
    /// The agent's own session ID once it has reported one; needed to resume the conversation (`SES-5`).
    pub native_id: Option<String>,
    pub turn: Option<TurnRecord>,
}

/// The state directory: its lock and its session records.
pub struct Store {
    sessions_dir: PathBuf,
    /// Held open for as long as devbox-mcp runs; the `flock` on it is released when the process exits.
    _lock: File,
}

/// How long a new devbox-mcp waits for a previous one to release the state directory (SPEC_impl.md, "Storage").
const LOCK_WAIT: Duration = Duration::from_secs(10);

impl Store {
    /// Takes the state directory's exclusive lock, waiting up to [`LOCK_WAIT`] for a previous instance to exit, and
    /// creates the directories if needed.
    ///
    /// Blocking: call it before the async runtime starts.
    pub fn open(state_dir: &Path) -> anyhow::Result<Self> {
        let sessions_dir = state_dir.join("sessions");
        fs::create_dir_all(&sessions_dir)
            .with_context(|| format!("cannot create state directory {}", sessions_dir.display()))?;
        let lock_path = state_dir.join("lock");
        let lock = File::options()
            .create(true)
            .append(true)
            .open(&lock_path)
            .with_context(|| format!("cannot open {}", lock_path.display()))?;
        let deadline = Instant::now() + LOCK_WAIT;
        loop {
            // SAFETY: flock only reads the file descriptor, which `lock` keeps open.
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                break;
            }
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EWOULDBLOCK) {
                return Err(err).with_context(|| format!("cannot lock {}", lock_path.display()));
            }
            if Instant::now() >= deadline {
                bail!(
                    "another devbox-mcp is using the state directory {} (its lock is held)",
                    state_dir.display()
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(Self {
            sessions_dir,
            _lock: lock,
        })
    }
}

/// What [`Store::load`] found.
pub struct Loaded {
    pub records: Vec<Record>,
    /// Names whose record files could not be read or parsed. They are left untouched for a human to look at.
    pub unreadable: Vec<String>,
}

impl Store {
    /// Reads every session record. A turn that was still running or being interrupted when the record was last
    /// written belongs to a devbox-mcp that is gone, so it is reported as `lost` (`TRN-5`); the file itself is
    /// updated by the session's next write. A record that cannot be read is reported on stderr and listed as unreadable
    /// rather than failing startup, since its name is known and can be protected.
    ///
    /// Failing to list the directory itself is an error, though: devbox-mcp would then start without knowing which
    /// names exist, and creating a session could overwrite a record it never saw (`SES-5`).
    pub fn load(&self) -> anyhow::Result<Loaded> {
        let mut loaded = Loaded {
            records: Vec::new(),
            unreadable: Vec::new(),
        };
        let entries = fs::read_dir(&self.sessions_dir).with_context(|| {
            format!(
                "cannot list session records in {}",
                self.sessions_dir.display()
            )
        })?;
        for entry in entries {
            let path = entry
                .with_context(|| {
                    format!(
                        "cannot list session records in {}",
                        self.sessions_dir.display()
                    )
                })?
                .path();
            // Only `<name>.json`; the fixed-name temporary files of interrupted writes end in `.tmp` and are ignored.
            let Some(name) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".json"))
            else {
                continue;
            };
            let parsed = fs::read(&path)
                .map_err(anyhow::Error::from)
                .and_then(|bytes| Ok(serde_json::from_slice::<Record>(&bytes)?))
                .and_then(|record| {
                    if record.name == name {
                        Ok(record)
                    } else {
                        bail!("it names the session {:?}", record.name)
                    }
                });
            match parsed {
                Ok(mut record) => {
                    if let Some(turn) = &mut record.turn
                        && !turn.state.is_terminal()
                    {
                        turn.state = TurnState::Lost;
                    }
                    loaded.records.push(record);
                }
                Err(e) => {
                    eprintln!(
                        "devbox-mcp: ignoring unreadable session record {}: {e:#}",
                        path.display()
                    );
                    loaded.unreadable.push(name.to_owned());
                }
            }
        }
        Ok(loaded)
    }

    /// Writes a session's whole record so that the file is always either the old or the new version: a temporary file
    /// next to it is written, flushed to disk, and renamed over the record.
    pub fn write(&self, record: &Record) -> anyhow::Result<()> {
        let path = self.sessions_dir.join(format!("{}.json", record.name));
        let tmp = self.sessions_dir.join(format!("{}.json.tmp", record.name));
        let mut file =
            File::create(&tmp).with_context(|| format!("cannot create {}", tmp.display()))?;
        file.write_all(&serde_json::to_vec_pretty(record)?)
            .and_then(|()| file.sync_all())
            .with_context(|| format!("cannot write {}", tmp.display()))?;
        fs::rename(&tmp, &path).with_context(|| format!("cannot replace {}", path.display()))?;
        Ok(())
    }
}
