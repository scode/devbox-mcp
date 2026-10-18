# devbox-mcp implementation specification

NOTE: This describes how devbox-mcp is built. What it must do is in [SPEC.md](SPEC.md), which wins whenever the two
disagree. Choices here can change when there is a reason to; requirements in SPEC.md change only with the user's
agreement.

## Implementation status

The code is an early prototype that implements steps 1 to 4 of the roadmap in TODO.md. Implemented: the stdio transport
and the security posture (`SEC-*`, `TSP-1`), `LIF-1` for orderly shutdowns, sessions (`SES-*`) with the Codex and Claude
adapters, a model and effort chosen at creation, and removal, the session store of "Storage", and turns (`TRN-*`) with
"Turn lifecycle", "Turn events", cursors, request IDs, liveness, and the bounds of `BND-*`, through the session tools,
and `session_transcript` (`TRA-2`) for both agents. "Deployment" is implemented by `devbox-mcp setup`;
[docs/deployment.md](docs/deployment.md) is the guide.

## Language and dependencies

devbox-mcp is a single Rust binary (edition 2024) using tokio and [rmcp](https://crates.io/crates/rmcp), the official
Rust MCP SDK. It uses rmcp's stdio server transport and nothing else. In support of `SEC-2`, no dependency may be built
with a feature that can listen for connections (for rmcp, any of the `server-side-http` or `transport-streamable-http-*`
server features), and no code path may bind a socket. Adding such a dependency or feature is a change to the security
model and needs the same explicit confirmation as changing SPEC.md's security section.

## Process model

There is one process: the stdio MCP server that `tunnel-client` starts through its `--mcp-command`. tunnel-client keeps
one long-lived child per tunnel and pipes every MCP request through it; it replaces the child when tunnel-client itself
restarts, when the pipes fail, and on some timeout paths, and the new child has to be initialized again. (This was read
from tunnel-client's source at commit `b91e6c9`, 2026-10-09; recheck it on upgrades.) There is no separate supervisor
and no IPC, because running turns are not required to survive the process (`LIF-1`). Conversely, when the child exits on
its own, tunnel-client asks for its own shutdown, so a crashing devbox-mcp takes tunnel-client down with it.

tunnel-client forwards requests to its stdio child one at a time, and gives up on a response after a deadline (its local
dev proxy defaults to 30 seconds; what the hosted relay applies has not been checked). Observed with tunnel-client
v0.0.16: a tool call that outlives the deadline fails on the client side while devbox-mcp keeps working on it, and every
other call queued behind it times out too. So every tool call must return quickly no matter how long the agent works,
which is the practical reason behind `TRN-1`, `BND-1`, and `BND-2`.

tunnel-client stops its child by closing the child's stdin and sending it SIGTERM, and a service manager may send
SIGTERM to every process of a service at once, with stdin still open. devbox-mcp treats stdin reaching end of file,
SIGTERM, and SIGINT alike as a request for an orderly shutdown, including while MCP initialization is still in progress.
It shuts its tokio runtime down, which drops every in-flight task, and dropping the task that owns an agent kills the
agent. The runtime's shutdown waits at most a second for blocking threads, because rmcp's stdin reader stays blocked in
`read` for as long as stdin is open. A turn belongs to its session, not to the call that started it, so a tool call the
client cancels (`notifications/cancelled`) never affects a turn (see "Turn lifecycle").

Each turn is an agent child process started with an argument array, never a shell string, in its own process group.
Killing that group is not enough: Codex runs every shell command in a new session, so a command's own children are in
another process group (observed with codex-cli 0.162.1, where `bash -c "sleep 303 & sleep 120"` left both sleeps running
after Codex was killed). A guard owned by the turn therefore kills the agent's whole process tree when the turn is
interrupted or its task is dropped at shutdown. It walks `/proc` one generation at a time, whatever each process's
session or group, and stops each generation (confirming the stops took effect) before reading its children: a stopped
process can neither fork nor reap, so the PIDs it walks cannot be reused under it. It then kills deepest first, because
killing a parent before its stopped children can orphan their process groups, and the kernel continues the stopped
members of a newly orphaned group. Both rules came from the second adversarial review, which showed processes escaping
without them. `/proc/<pid>/stat` is parsed as bytes, since process names need not be UTF-8.

When the agent exits on its own, the turn ends at its exit rather than when its output pipes close, which a leftover
background process could keep open forever. The exit is noticed with `waitid(WNOWAIT)`, which leaves the agent unreaped,
so its process group ID still belongs to it while the guard kills what is left in that group; only then is the agent
reaped. Reading the pipes stops after a two-second grace period. (A pidfd would avoid the blocking thread `waitid`
needs, but polling one asynchronously requires tokio's `net` feature, which also compiles in TCP listeners.)

On Linux the child also gets a parent-death signal, so it is killed if devbox-mcp dies abruptly; it checks after setting
the signal that devbox-mcp did not die in the meantime. The kernel ties that signal to the thread that spawned the
child, not the process, which is why devbox-mcp runs a current-thread tokio runtime: every child is spawned from the
main thread.

Known gaps, both of which need a cgroup per agent to close (tracked in [TODO.md](TODO.md)):

- If devbox-mcp dies abruptly (SIGKILL, a crash), the parent-death signal reaches only the agent process itself, and
  Codex's own parent-death signal only its direct children. Anything further down may outlive it.
- A process that the agent's tree has already lost (its parent exited, so it was reparented to init) cannot be found by
  the tree walk. When an agent exits normally, this is everything it left running outside its own process group. Codex
  cleans up a finished command's process group itself, so in practice this takes something like a double-forking daemon.

## Configuration

All of devbox-mcp's own configuration is through command-line flags, so tests can inject every path and executable
without touching environment variables:

- `--state-dir`: where session records live. Defaults to `$XDG_STATE_HOME/devbox-mcp`, falling back to
  `~/.local/state/devbox-mcp`.
- `--workdir-root`: the root for default session working directories (`SES-3`). Defaults to `~/devbox-mcp/work`, so a
  session named `foo` with no explicit working directory works in `~/devbox-mcp/work/foo`. The directory is created when
  the session is created.
- `--codex-bin`: the Codex executable. Defaults to `codex` on `PATH`.
- `--claude-bin`: the Claude Code executable. Defaults to `claude` on `PATH`.

The agents' own data directories are not flags. devbox-mcp finds each one the way the agent itself does, from the
agent's environment variable or else the home directory (`$CODEX_HOME`, else `~/.codex`; `$CLAUDE_CONFIG_DIR`, else
`~/.claude`), and uses it only for its own reads (transcripts) and Claude's deletions; it sets nothing on the agents it
starts. Exporting a resolved default would not be harmless: Claude Code reads its global `.claude.json` from
`$CLAUDE_CONFIG_DIR` when that is set and from the home directory otherwise, so setting it even to `~/.claude` would
hide the user's global Claude settings from every agent (contrary to `SEC-4`). A relative value in either variable makes
devbox-mcp refuse to start: the agent would resolve it against its own working directory, which differs per session, so
there would be no one directory to read and delete in. Tests stay away from the real directories because they start
devbox-mcp with a temporary `HOME`.

## Storage

Each session is one JSON file, `<state-dir>/sessions/<name>.json`. It holds:

- the name, agent, working directory, model and effort, and the agent's native session ID once known (`SES-5`). For
  Claude, devbox-mcp chooses that ID itself and saves it with the session's first send (see "Claude");
- the most recent turn (`SES-8`): its ID (turn IDs count up from 1 within a session), its state, when it started and
  ended, the agent's final message (at most 32 KiB, cut with a `final_message_truncated` flag), and, for a failed turn,
  the error that ended it (at most 4 KiB, ending in ` [cut]` when it was longer);
- every request ID the session has accepted (`TRN-6`), each with a SHA-256 of its message, the turn it started, and
  whether that turn reset the agent's context (`SES-5`), so a retry reports the reset too. They are kept for the
  session's whole life, so a retry is recognized however late it comes; each costs about a hundred bytes per turn.
  (SHA-256 rather than Rust's built-in hasher, whose output may change between Rust releases and would then reject
  legitimate retries after an upgrade.)

Every change rewrites the whole record synchronously: write a temporary file with a fixed name next to it, `fsync` it,
and rename it over the record. Records hold agents' replies and failure text, so the store keeps them private to the
user whatever the umask is: the directories it creates are mode 0700, the lock and the records 0600, and the temporary
file is created fresh each time rather than written through whatever was left at its path. The runtime is
single-threaded and a write has no `.await` in it, so two writes can never interleave and no further coordination is
needed. A send is accepted only once its record (the new turn in state `running` and its request ID) is on disk, before
the agent is started and before the send returns; if that write fails, the send fails and nothing runs. If a later write
fails, the call or turn that made it reports the error, memory keeps the correct state, and the next successful write of
that session brings the file up to date.

Removing a session deletes its record file after the agent's own data is gone, so a crash in between leaves a record
whose agent data no longer exists. For Codex, removing it again then fails, since its delete command cannot tell
"already gone" from a failure, and the caller forces the removal; a send to it in the meantime fails the way a resume of
a missing conversation fails. For Claude, removing it again finds nothing left and succeeds, and a send in the meantime
starts a fresh conversation under the same ID, reporting `context_reset` (see "Claude").

A record also says whether an earlier conversation of the session was never tracked: whether the agent may hold data
under an ID devbox-mcp cannot account for, which it therefore cannot delete, so removal requires `force`. For Codex that
is set when a send starts a fresh conversation because the previous one could not be resumed (`context_reset`, `SES-5`),
or when a resumed turn reports a different conversation than the one resumed (see "Codex"). For Claude it is set only
when Claude reports a different conversation ID than the one devbox-mcp chose (see "Claude"); a Claude restart under its
own known ID leaves nothing untracked. Records written before this was tracked lack it; for them nobody knows, so a
session past its first turn is treated as possibly having such a conversation (a reset needs an earlier turn), and one
still at its first turn as not having one.

Records are read at startup. A most recent turn still recorded as `running` or `interrupting` is reported as `lost`
(`TRN-5`); the file itself is updated with the session's next write. A record that cannot be parsed is left untouched, a
warning naming it goes to stderr, it is left out of `session_list`, and creating a session with that name fails with an
error saying an unreadable record is in the way. If the records cannot even be listed, devbox-mcp refuses to start:
starting without knowing which names exist would let a create overwrite a record it never saw.

devbox-mcp holds an exclusive lock (`flock`) on `<state-dir>/lock` for as long as it runs, so two instances can never
share a state directory. tunnel-client may start a new devbox-mcp while the previous one is still shutting down, so a
new instance waits up to 10 seconds for the lock before giving up with an error. A file lock is not a listener.

Turn events are kept in memory only, since a turn cannot outlive the process anyway: the events of each session's most
recent turn, at most 10,000 of them, after which the oldest are dropped (a read that asks for them gets a gap, `TRN-3`).
Since every text field is already cut when its event is created (see "Tools"), that count also bounds the memory. The
durable record of what happened is the agent's own transcript (`TRA-2`).

## Tools

The tools named in SPEC.md, as they are implemented. Argument and result fields are JSON; every result is returned both
as MCP structured content and, for clients that ignore that, as the same JSON as text.

- `session_create { name, workdir?, agent?, model?, effort? }`: `workdir` defaults to `<workdir-root>/<name>` and must
  be absolute. It is created with its parents if missing; if it exists it must be a directory (symlinks are followed).
  The session exists once its record is on disk; a directory created before a failure is left in place. `agent` is
  `codex` (the default) or `claude`. `model` is 1 to 128 characters from `A-Z a-z 0-9 . _ : / -`, and `effort` 1 to 32
  characters from `a-z 0-9 _ -`; that check is only about what can be passed to the agent safely (the effort goes into a
  TOML override for Codex), not about what the agent supports. For `claude`, `effort` must also be one Claude's CLI
  accepts (`low`, `medium`, `high`, `xhigh`, `max`), because Claude ignores any other with only a warning (`SES-7`).
  Nothing else is checked against what an agent supports, so the tool's description says that whether an unsupported
  choice is rejected at creation depends on the agent and the value, and that a successful creation does not mean the
  agent supports it. Returns a session summary. On a name collision, the error message includes the existing session's
  summary.
- `session_list { after? }`: returns `{ sessions: [summary], next_after? }`: at most 50 sessions in name order, starting
  after the name `after`; `next_after` is present when more follow. A summary is `{ name, agent, model, effort, workdir,
  turn }`, where `turn` is the most recent turn as `{ id, state, started_at, ended_at?, final_message_preview?,
  final_message_truncated?, error? }` with a preview of at most 2 KiB, or null before the first turn.
- `session_send { name, message, request_id? }`: `message` is at most 256 KiB; `request_id` is 1 to 128 characters from
  `A-Z a-z 0-9 . _ : -`. Checked in this order: unknown session, then a known request ID (same message: return the
  original turn with `replayed: true`, whatever the session is doing; different message: error), then busy, then a
  record write failure. Returns `{ turn_id, replayed, cursor?, context_reset }`; `cursor` points just before the turn's
  first event and is omitted when those events are no longer held (a replay of an older turn, or after a restart).
  `context_reset` is true when this turn starts a fresh agent conversation because the old one could not be resumed
  (`SES-5`).
- `session_read { name, cursor? }`: returns `{ events, next_cursor, more, gap, turn, liveness: { agent_alive,
  last_event_age_seconds, turn_duration_seconds } }`. `turn` is the most recent turn as in a summary, but with the full
  stored `final_message`. `gap` is true when events may be missing between the cursor and the first event returned.
  `more` is true when the response hit its bounds and further events are already waiting. Liveness values are null when
  unknown: `agent_alive` when there is no running turn, `last_event_age_seconds` when no event is held,
  `turn_duration_seconds` when a lost turn's end is unknown. `agent_alive` is true until the turn ends, which can be a
  few seconds after the agent exits, while its last output is collected and anything it left running is stopped;
  tracking the exit separately for that window would not change what a caller does. For a running turn
  `turn_duration_seconds` is the time since it started, for an ended one the time it ran. Times are RFC 3339 timestamps
  in UTC; durations are whole seconds.
- `session_interrupt { name }`: returns the most recent turn as in a summary, normally with state `interrupting`.
- `session_transcript { name, from?, limit? }`: returns `{ entries: [{ index, entry, truncated? }], total, next_from? }`
  from the agent's native transcript of the session's current conversation (`TRA-2`), one entry per line of the agent's
  own JSONL record, numbered from 0 in file order, always in ascending order. `limit` is 1 to 50 (default 20).
  - Without `from`, the read fills from the newest entry backwards, so the end of the transcript is always included and
    reading it needs no knowledge of the length. To page back, pass `from` as the first index already read minus
    `limit`, clamped to 0, and follow `next_from` until reaching entries already read, because a page that hits the size
    cap holds fewer than `limit` entries. With `from`, it fills forwards from that entry. `next_from` is the index after
    the last entry returned, present when more entries follow.
  - `entry` is the line's JSON value, unchanged except that every string longer than 4 KiB is cut to 4 KiB and ends in
    ` [cut]`. If the entry is still larger than 64 KiB once serialized (a huge array, say), it is replaced by its
    serialized JSON cut to 64 KiB as a string, with `truncated: true`. A line that is not valid JSON is returned as a
    string, cut the same way, and a line longer than 4 MiB is not parsed at all: only its first 64 KiB is read, as a
    string with `truncated: true`, so one enormous line cannot pull a huge buffer into memory. So one entry never
    exceeds 64 KiB, and a read always returns at least one entry and moves forward.
  - A read stops adding entries once they total 256 KiB, the newest end first when reading from the end (`BND-1`).
  - The read works on the file as it was when the read started: its length is taken first, and a partial last line (the
    agent is still writing it) is left out.
  - Fails with an error that says why when the session has no conversation yet, when the agent's transcript file cannot
    be found where the adapter looks (it may have been deleted, compressed, or moved by the agent; asking the agent
    itself still works, `TRA-1`), when looking for it fails or finds more than one, when it cannot be read, or when it
    is larger than 1 GiB. It never returns an empty list for a transcript it could not find.
  - Only the current conversation is reachable; one left behind by a context reset has no known ID.
  - The read scans the file to number its lines, on a blocking thread, so its cost grows with the file: about a second
    for 100 MB. Like `session_remove`'s wait, that holds up other calls through the tunnel while it runs; transcripts of
    that size are rare, and per-call scan budgets were judged not worth their extra failure modes.
- `session_remove { name, force? }`: returns `{ session, agent_data_deleted }`, where `session` is the summary as it was
  just before removal. Fails if the session does not exist or its most recent turn is `running` or `interrupting`.
  Without `force`, it also fails when deleting the agent's data fails, and when deletion cannot be confirmed: the
  session has had a turn but its native session ID was never learned, or its record says an earlier conversation was
  never tracked (see "Storage"). With `force`, those cases forget the session anyway, with `agent_data_deleted: false`.
  Removal deletes the agent's data first and the record second. The session stays in memory until its record is gone, so
  a failure at any point leaves it listed; the error says that the agent may already have deleted part of its data,
  which then stays deleted, and a later send fails the way a resume of a missing conversation fails. Failure output from
  the agent's delete command is cut to 2 KiB, ending in ` [cut]` when it was longer.
- While the agent's data is being deleted, the session is marked as being removed, in memory only. New sends and other
  removals fail as busy; a send that repeats a known request ID is still replayed first, as above. The mark is set in
  the same step as the busy check and cleared once the deletion has finished or failed.
- Removal is by name only. Two callers acting on the same name at once (one removing, another recreating) are not
  guarded against: a retried removal whose first response was lost can remove a session another caller has just created
  under the same name. Multiple independent callers are a non-goal (SPEC.md).

Cursors are opaque strings that encode the session name, an ID of the devbox-mcp process that issued them (random at
startup) together with the session's incarnation in that process (a number each session gets when it is loaded or
created, counting up across the process), and the sequence number of the last event the caller has seen (0 for "before
the first event"). Event sequence numbers count up from 1 within a session and process, across turns. The incarnation
means a cursor from a removed session is not mistaken for one of a new session created under the same name: it is
treated like a cursor from another process. Reads have two outcomes for a cursor:

- one that cannot be parsed, or that belongs to another session, is an error naming the problem;
- one that cannot otherwise be honored (from another process, older than the oldest event still held, or beyond the
  newest event) gives `gap: true` and continues from the oldest event held.

Without a cursor, a read starts at the first event of the most recent turn, with `gap: true` if that is no longer held.
With no events after the cursor, `events` is empty and `next_cursor` equals the cursor.

Bounds per `BND-1` hold by construction rather than by measuring responses. Every text field of an event is cut when the
event is created: to its last 8 KiB for command output, its first 8 KiB otherwise, never splitting a UTF-8 character,
with a `truncated` flag on the event; a `file_change` lists at most 50 changes, also flagged. A read returns at most 50
events and sets `more` when others are waiting. With every event bounded, a read's size is bounded too (roughly 50
events of at most a few tens of KiB each, plus the turn's stored final message), and paging always advances. Lines of
agent output are read whole; the agent is trusted under the security model, so an absurdly long line costs memory,
nothing more.

No call waits on an agent or on a background task (`BND-2`): each one reads or updates in-memory state and, at most,
writes one session record. Two calls do work that grows with data outside devbox-mcp. `session_remove` waits for the
agent's data to be deleted: Codex's delete command, bounded by a 10 second timeout, or Claude's files and the name scan
that confirms them gone (a second or two on a large Claude directory). `session_transcript` reads the transcript file
(about a second per 100 MB). Neither is an agent making progress, and while either runs tunnel-client delivers no other
call, which is the price of calls that finish or fail before they return. There is no fixed time limit beyond that; a
call that cannot make progress because the disk is stuck is a failure of the box, not something devbox-mcp can bound.

## Turn lifecycle

A turn is a background task owned by its session, not by the call that started it, so cancelling or losing a tool call
never affects an accepted turn (`TRN-1`). Its states:

- `running` from acceptance until the agent's exit is observed, or until an interrupt is accepted;
- `interrupting` from an accepted interrupt until the agent and its process tree have been killed (see "Process model")
  and the agent reaped. The tree walk sleeps while it waits for processes to stop, so it runs on a blocking thread,
  never on the runtime's only thread, where it would stall every other call;
- then exactly one terminal state: `completed`, `failed`, or `interrupted`, or `lost` if devbox-mcp restarted first.

Which terminal state follows depends on what came first. An interrupt accepted before the agent's exit was observed
makes the turn `interrupted`, even if the agent was finishing at the same moment. Otherwise the agent's own report
decides, by the same rule as the prototype: `completed` only if the agent reported completion, did not report failure,
and exited successfully; `failed` in every other case, including an agent that could not be started, with the reason
recorded. If the process-tree walk hit its bounds (a process that did not stop in time, or the generation limit),
cleanup cannot be confirmed, and the terminal state carries `cleanup_uncertain: true` (`TRN-4`, `LIF-2`). A failure
after the agent started (its output cannot be read, its exit cannot be watched, or it cannot be reaped) is cleaned up
the same way as an interrupt before the turn ends `failed`, and carries the same flag: `cleanup_uncertain` is false only
when the cleanup was confirmed, or when the agent never started.

The turn ends only once all of this is settled: the agent exited or was killed and reaped, its output pipes were drained
(at most two seconds after its exit), the `turn_ended` event was published, and the record was written. Until then the
session counts as busy (`TRN-2`), so a new turn can never receive an old turn's late output.

A line of agent output that is not JSON does not end the turn; it becomes a non-fatal `error` event and the turn goes
on. A JSON event of a type the agent's module does not know is ignored: agents add event types over time, most carry
nothing a caller needs, and reporting each as an error would mislead callers into thinking something went wrong.

If the first turn of a session was lost before the agent reported its native session ID, the agent's conversation may
exist but cannot be found. That is recognizable from the record alone (no native session ID, but an earlier turn), so no
extra state is kept: the next send starts a fresh agent conversation and reports `context_reset: true` (`SES-5`). When a
resumed turn reports a native session ID different from the stored one, the turn fails with an error rather than
continuing a different conversation.

When devbox-mcp shuts down, every session's running turn is killed as before (`LIF-1`) and is reported as `lost` by the
next instance.

## Self-documentation

The `DOC-*` requirements are met through the three places MCP lets a server describe itself: the server instructions
returned at initialization, each tool's description, and the JSON Schemas of each tool's arguments and results. rmcp
builds those schemas from the Rust types, turning doc comments on fields into schema descriptions, so field doc comments
on tool types are written for the calling model; notes meant for developers on those types are plain `//` comments. The
text is written for a capable agent that knows nothing about devbox-mcp: concrete, with the workflow spelled out, and
never relying on SPEC.md or this repository. Because a client may not show a model everything (the server instructions
or output schemas in particular), anything a caller must know to use a tool correctly is also in that tool's
description.

The instructions tell callers how to poll: there is no way to wait inside devbox-mcp, so a caller with nothing else to
do should space out reads (for example, check back after doing other work, or every few seconds at most) rather than
read in a tight loop, and should expect long turns to take minutes. They also describe how agents run, naming the Unix
user devbox-mcp runs as (from `$USER`, or the user ID when that is unset): no sandbox, no approval prompts, unrestricted
network access, and commands without a terminal and with empty input, so most prompts fail at once. They do not promise
that every prompt fails: a program may treat end of input as a reason to ask again, and nothing times commands out, so
the instructions ask callers to keep agents non-interactive and to interrupt a turn that stops producing events. A
caller handing an agent real work needs to know this, and it was checked on a deployed service.

Two checks keep it honest:

- An automated test lists the tools over MCP and fails if the server instructions are empty, if any tool lacks a
  description, or if any property in any argument or result schema lacks one. It simply visits every JSON object
  anywhere in each schema and checks every entry of each `properties` map, so nested types and definitions are covered
  without resolving references. This catches omissions, not bad writing.
- A manual "cold agent" check, using the model-as-client layer in docs/testing.md. A real model gets only devbox-mcp's
  tools and tasks phrased the way a user would give them to a Dot, with no hints about tool names, polling, or cursors,
  and must handle each correctly within a bounded number of calls:
  - create a session, send work, follow the turn to its end, report the result, then continue the same session;
  - a send to a busy session (it should wait for the turn or interrupt it, not resend blindly);
  - a failed turn (it should report the failure and its reason);
  - interrupting a long turn and confirming it stopped;
  - coming back after devbox-mcp restarted (it should notice the lost turn or gap and carry on);
  - removing a session whose turn is still running (it should stop the turn and then remove the session, without
    reaching for `force`).

  Misuse is fixed by improving the descriptions, not the prompts. The check records which parts of the server's
  self-description the client actually showed the model. It is run whenever tools or their descriptions change, and its
  outcome is reported with the change. It does not replace trying the real thing with a Dot through the tunnel, which is
  the user's to do.

## Turn events

Events are agent-agnostic. Each has a `seq`, a `turn_id`, a `time`, a `type`, and, when it was cut down, `truncated`:

- `turn_started { message }`, with the message cut like any text field;
- `agent_message { text }`;
- `command_started { item_id, command }` and `command_finished { item_id, command, status, exit_code, output }`, where
  `status` is `completed`, `failed`, or `declined`, `exit_code` is null only when the command did not run to an exit
  (declined or blocked, interrupted, or for Claude sent to the background) and never just because it succeeded, and
  `output` is the end of its combined output; `item_id` pairs the two;
- `file_change { item_id, status, changes: [{ path, kind }] }`, where `status` says whether the change was applied
  (`completed`) or not (`failed`);
- `tool { item_id, name, status }` for other agent tool use the adapter recognizes but has no specific event for, and
  for item types it does not recognize, named after the type;
- `error { message, fatal }` for errors the agent or devbox-mcp reports during the turn; `fatal` is true for the one
  that ended it;
- `turn_ended { state, error?, cleanup_uncertain? }`, always the last event of a turn. The final message is not repeated
  here; it is in the `turn` returned with every read.

The agent's reasoning is not passed through.

## Agent adapters

Everything agent-specific stays in that agent's module, so adding Claude or another agent means adding a module and not
touching the session logic. An agent's module knows how to:

- build the command line for a first turn and for a resumed turn, with permissions fully disabled (`SEC-4`);
- turn the agent's output stream into devbox-mcp's turn events, and extract the agent's native session ID;
- locate and read the agent's native transcript for a session (step 4 in TODO.md);
- delete the agent's stored data for a session (`SES-6`).

Each agent is a module (`codex.rs`, `claude.rs`) with the same few functions, and the session logic picks one with a
`match` on the session's agent. A trait is not needed for two agents; one is worth introducing only if the match
statements start to repeat or a third agent arrives.

### Codex

The Codex adapter uses non-interactive mode, checked against codex-cli 0.162.1:

- First turn: `codex exec --json --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check -C <workdir> -m
  <model> -c model_reasoning_effort="<effort>" -`, with the message on stdin.
- Later turns: `codex exec resume --json` with the same permission, model, and effort flags, then the native session ID
  and `-`, again with the message on stdin and the child's working directory set to the session's.
- The bypass flag turns off Codex's sandbox and approvals but leaves the user's own execution-policy rules in force; per
  `SEC-4`, devbox-mcp deliberately does not pass `--ignore-rules`. A command those rules reject shows up as a declined
  or failed command in the turn's events.
- The native session ID is the `thread_id` of the `thread.started` event, recorded as soon as it is seen. Never use
  `--last`, which picks whatever session ran most recently. A resumed turn that reports a different `thread_id` than the
  one resumed is aborted, and the session is marked as having an untracked conversation (see "Storage"), since Codex may
  have stored data under the other ID.
- `--skip-git-repo-check` is needed because Codex otherwise refuses to run outside a git repository, and session working
  directories need not be repositories.
- Defaults: model `gpt-6.1-sol`, reasoning effort `high`. Which efforts a model supports varies by model; an unsupported
  one fails the turn with an error from the model API that names the valid values, which is what the caller sees as the
  turn's failure reason.
- Removing a session's data: `codex delete --force <native session ID>`, run with the same environment as the turns, so
  it uses the same Codex home. It deletes the rollout file and Codex's database rows for the session (checked with
  0.162.1). On a timeout of 10 seconds the command's whole process group is killed and the command reaped before the
  removal fails, so it can never delete anything after the session is back in use. The group matters because `codex` may
  be a launcher, such as the npm package's script, that starts the deleting process as its child. Any failure is a
  failure of the removal: Codex reports an already deleted session with the same message and exit status as a real
  failure, and a failure can come after part of the data is already deleted (the rollout before the database rows), so
  devbox-mcp does not try to tell these apart and leaves the decision to the caller's `force`.
- The native transcript is a rollout file under Codex's home directory (see "Configuration"), at
  `sessions/YYYY/MM/DD/rollout-<timestamp>-<session id>.jsonl`, found by walking `sessions/` for a file name ending in
  `-<session id>.jsonl`. Only plain rollouts are read. A compressed `.jsonl.zst` one, or none at all (Codex 0.162 also
  keeps history in its own SQLite store and can migrate rollouts there), makes `session_transcript` fail with an error
  saying so. None of the rollouts on the development box were compressed when this was written.
- Event mapping, from `--json` output (shapes observed with 0.162.1 where noted, otherwise from Codex's event
  definitions):
  - `item.completed` of an `agent_message` item (observed) gives `agent_message`; the final message is the text of the
    last one.
  - `item.started` and `item.completed` of a `command_execution` item (observed; fields `id`, `command`,
    `aggregated_output`, `exit_code`, `status`) give `command_started` and `command_finished`.
  - `item.completed` of a `file_change` item (observed; fields `id`, `changes` with `path` and `kind`, `status`) gives
    `file_change`.
  - `item.completed` of an `error` item, and top-level `error` events, give non-fatal `error` events; `turn.failed`
    gives the fatal one.
  - Other item types (Codex documents tool calls, web searches, and to-do lists) give `tool` on completion, named after
    the item type. `item.started` of those, `item.updated` of anything, and `reasoning` items give nothing.

`codex exec` cannot be steered mid-turn and has no live question channel, which is why SPEC.md makes follow-ups new
turns.

### Claude

The Claude adapter uses Claude Code's print mode, checked against Claude Code 2.1.296:

- Turns: `claude -p --output-format stream-json --verbose --dangerously-skip-permissions --model <model> --effort
  <effort>`, plus `--session-id <id>` to start the conversation or `--resume <id>` to continue it, with the message on
  stdin and the child's working directory set to the session's.
- The bypass flag turns off all of Claude Code's permission checks (`SEC-4`). The user's own Claude Code configuration
  (`CLAUDE.md`, settings, hooks, skills) stays in force, as Codex's rules do; devbox-mcp passes nothing to disable it.
  Nothing in print mode may wait for an answer nobody can give; a question tool that did would be disallowed with
  `--disallowedTools`. Checked with 2.1.296: print mode offers no question tool (its tool list has no
  `AskUserQuestion`), so nothing is disallowed.
- Claude Code opens a Unix socket for messages between its own sessions, at `/run/user/<uid>/cc-socks/<pid>.sock`
  (2.1.296). The directory is the user's runtime directory, mode 0700, so only the user the agent already runs as can
  reach it, which `SEC-3` allows: it is the agent's own local mechanism, not a way into the box.
- The native session ID is chosen by devbox-mcp: a random UUID, saved in the record that accepts the session's first
  send, before Claude starts. A later send looks for Claude's transcript for that ID first, with three outcomes: exactly
  one found, so the turn resumes it; the lookup completed and found none, so the turn starts the conversation again
  under the same ID with `--session-id`, reporting `context_reset` when an earlier turn had run, since the caller cannot
  otherwise tell whether anything was lost (`SES-5`); or the lookup failed or found more than one, so the send fails and
  Claude does not start. If a resume then fails because the transcript has gone in the meantime, the turn fails; it is
  not retried as a fresh start.
- The `session_id` of the stream's `system` `init` event must match the chosen ID; a mismatch aborts the turn, as with
  Codex, and marks the session as having an untracked conversation, since Claude may already have stored data under the
  other ID. For both agents the mark is saved the moment the mismatch is seen, not when the turn is settled, so a turn
  that then fails some other way (unreadable output, a shutdown) cannot lose it. Short of that, the ID is always known
  before Claude runs, so removal can check that the data is gone.
- Defaults: model `opus`, effort `high`. Efforts are checked at creation (see "Tools"); an unknown model fails the turn
  with the API's error, which arrives as an error `result` event.
- Event mapping, from `stream-json` output (shapes observed with 2.1.296):
  - `text` blocks of `assistant` messages give `agent_message`; the final message is the `result` event's `result`.
  - A `tool_use` block named `Bash` gives `command_started` (`item_id` is the block's `id`, `command` its
    `input.command`), and the matching `tool_result` (in a `user` message, same `tool_use_id`) gives `command_finished`:
    `failed` when `is_error` is true, otherwise `completed`, with the result's text as output. Claude reports no exit
    code as a field, so `exit_code` comes from what its results mean: a failed command that ran to an exit has a result
    text starting `Exit code N`, which gives N (the other failures are commands that never ran, null); a successful
    command exited 0, unless it was sent to the background (`input.run_in_background`, or a `backgroundTaskId` in the
    event's `tool_use_result`) or interrupted (`tool_use_result.interrupted`), which give null. Only the structured
    `tool_use_result` can rule those out, and an event has one for all its results, so 0 is claimed only for an event
    carrying that one result with its `tool_use_result`; otherwise the exit code is unknown and null. Checked against
    413 failed Bash calls in a sample of real Claude Code transcripts (versions up to 2.1.296), 281 of them with the
    prefix.
  - `tool_use` of `Write` (kind `add`) or `Edit` and `MultiEdit` (kind `update`) gives `file_change` for
    `input.file_path` when its `tool_result` arrives, `failed` if that is an error.
  - Any other `tool_use` gives `tool`, named after the tool, when its `tool_result` arrives.
  - The `result` event ends the turn: `completed` unless `is_error` is true, otherwise `failed` with its `result` text
    as the reason. An exit without a `result` event is a failure with the process's stderr.
  - `thinking` blocks and other events give nothing.
- The native transcript is `projects/<encoded working directory>/<id>.jsonl` under Claude Code's configuration directory
  (see "Configuration"), found by looking for `<id>.jsonl` in each directory under `projects/` rather than reproducing
  Claude's encoding of the directory name. Only the main conversation is read; subagents' transcripts, under
  `<id>/subagents/`, are not.
- Removing a session's data: there is no Claude command for one session (`claude purge` removes a whole project's
  sessions, which may include other sessions sharing the working directory), so devbox-mcp deletes them itself:
  `<id>.jsonl` and `<id>/` in every directory under `projects/`, `session-env/<id>`, and `file-history/<id>`. It then
  walks the whole configuration directory for any other file or directory whose name contains the ID, and reports the
  data as deleted only when none is left; anything found makes the removal fail unless forced, and the error names it.
  The walk is what keeps a Claude update that adds or moves a per-session file from leaving data behind while removal
  reports it deleted; it cannot see data kept inside files shared between sessions, which this layout (observed with
  2.1.296) does not have for print-mode sessions. The deletion runs on a blocking thread and completes or fails before
  the removal returns, so nothing is deleted after the session is back in use; the walk took under two seconds on a
  13,000-entry Claude directory.

## Deployment

devbox-mcp runs on the box as the user whose files the agents work on, under systemd; nothing else is supported. A
`systemctl --user` service runs `tunnel-client run --profile devbox`, and tunnel-client starts devbox-mcp through the
profile's MCP command over stdio. Lingering (`loginctl enable-linger`) keeps the service running without a login
session.

Nothing specific to one deployment is in this repository: no tunnel ID, runtime key, or hostname. They are given on the
box, by two commands. The key has its own, so that it is pasted once per box and again only to replace it, while setup
can be rerun freely.

`devbox-mcp set-key` reads the runtime key from stdin (never from the command line, where other processes can see it)
and writes it, as `CONTROL_PLANE_API_KEY=<key>`, to `~/.config/devbox-mcp/tunnel.env`, readable only by the user, which
the unit loads; the key appears nowhere else. The new key goes to a fresh file next to it, created with mode 0600, which
is then renamed over the old one: nothing is written through whatever was at the path, so a symlink or hardlink there
cannot carry the key into another file, and a write that fails (a full disk, say) leaves the old key in place rather
than none. Directories set-key and setup create are mode 0700 whatever the umask is.

`devbox-mcp setup --tunnel-id <id>` never touches the key file and does two things:

- runs `tunnel-client init --force --profile devbox --profile-dir ~/.config/tunnel-client --tunnel-id <id> --mcp-command
  "<devbox-mcp> --codex-bin <codex> [--claude-bin <claude>]"`, so tunnel-client writes its own profile and devbox-mcp
  never writes tunnel-client's YAML. `<devbox-mcp>` is the binary running setup. `--claude-bin` is added when setup
  finds `claude` on its `PATH` (or is given `--claude-bin`); without it, Claude sessions use whatever `claude` the
  service's `PATH` holds, and fail their turns with a clear error if there is none. The paths are absolute, because a
  user service's `PATH` is minimal and does not include the directories they usually live in. The profile directory is
  explicit, here and in the unit, so an `XDG_CONFIG_HOME` in the shell running setup cannot put the profile where the
  service does not look;
- writes the unit, `~/.config/systemd/user/devbox-mcp.service`, privately (mode 0600, replaced the same way as the key
  file, since whoever can change it can change what the service runs), from one fixed piece of text with the absolute
  paths filled in, plus `Environment=PATH=` with the `PATH` setup runs with, so that the agents' commands find the
  user's usual tools rather than only those on the systemd user manager's minimal `PATH`. Relative and empty entries
  (the current directory, which a service must not depend on) and entries that would need escaping in a unit file are
  left out; setup reports all but the empty ones.

It then prints the remaining commands (set the key if there is no key file yet, enable lingering, reload, enable and
start the service) rather than running them. It does not pass on `tunnel-client init`'s own output unless init fails:
that output advises running tunnel-client in the foreground, which would serve its admin API on the profile's loopback
TCP port instead of the unit's socket. Running it again replaces the profile and the unit, for example to change the
tunnel ID or the `PATH`. Paths containing characters that would need escaping in a unit file or command line
(whitespace, quotes, `%`, backslashes) are refused rather than escaped. For tests, a flag points both commands at
another home directory, and setup at another tunnel-client binary. The pinned tunnel-client release and devbox-mcp are
installed under the user's home first; the deployment guide says how.

The unit uses:

- `Restart=always`. tunnel-client exits when its MCP child exits, but with status 0 even when devbox-mcp crashed (seen
  in the spec review's probe of tunnel-client v0.0.16), so `on-failure` would leave the tunnel down. An explicit
  `systemctl --user stop` still stops it for good.
- `KillMode=control-group` (the default) with a stop timeout of 15 seconds: stopping the service sends SIGTERM to every
  process in its cgroup, devbox-mcp and the agents included, which devbox-mcp handles as an orderly shutdown (`LIF-1`),
  and SIGKILL to whatever is left after the timeout. So a service stop also stops agent processes that escaped
  devbox-mcp's own cleanup; the gaps in TODO.md remain for devbox-mcp dying while the service keeps running, and for
  cleaning up one turn while others continue.
- `RuntimeDirectory=devbox-mcp` with mode `0700`, for tunnel-client's health and admin listener, which the unit's
  command line (`tunnel-client run --profile devbox --profile-dir ~/.config/tunnel-client --health.unix-socket
  %t/devbox-mcp/tunnel-client.sock`) puts on a Unix socket there instead of the default `127.0.0.1:8080`;
  tunnel-client's command-line flags override its profile (observed with v0.0.16: with the flag, the socket is its only
  listener). That listener is more than health checks: in tunnel-client v0.0.16 its admin API can start Codex sessions
  with full access, guarded only by being on loopback and a same-origin check that non-browser requests pass. On
  loopback TCP any local user could use it, contrary to `SEC-3`; on the socket only the service's own user can.

## Testing

Automated tests use only injected paths and fake agents, never the user's real state or a real model. They drive the
real binary both over plain stdio and through the real tunnel-client, using its local `dev proxy` mode, which runs
tunnel-client's runtime against an in-memory control plane and needs no credentials. The tunnel-client release is pinned
by version and checksum in `scripts/install-tunnel-client.sh`. Running real agents is a manual step, with a driver for
direct tool calls (optionally through the dev proxy) and a recipe for putting a real model in front of devbox-mcp.
[docs/testing.md](docs/testing.md) describes all of it.

`SEC-2` constrains devbox-mcp, not local development tooling. Test harnesses and manual test setups may listen for
connections as long as they listen only locally: on loopback addresses or on Unix sockets in private temporary
directories. devbox-mcp itself never listens, in tests or anywhere else.
