# devbox-mcp implementation specification

NOTE: This describes how devbox-mcp is built. What it must do is in [SPEC.md](SPEC.md), which wins whenever the two
disagree. Choices here can change when there is a reason to; requirements in SPEC.md change only with the user's
agreement.

## Implementation status

The code is an early prototype, part way through step 1 of the roadmap in TODO.md. Implemented: the stdio transport and
the security posture (`SEC-*`, `TSP-1`), `LIF-1` for orderly shutdowns, sessions (`SES-*` except removal) with the Codex
adapter, the session store of "Storage", and turns (`TRN-*`) with "Turn lifecycle", "Turn events", cursors, request IDs,
liveness, and the bounds of `BND-*`, through the five session tools.

Not yet implemented: transcripts (`TRA-*`) and session removal (`SES-1`, `SES-6`). "Deployment" is implemented by
`devbox-mcp setup`; [docs/deployment.md](docs/deployment.md) is the guide.

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

All configuration is through command-line flags, so tests can inject every path and executable without touching
environment variables:

- `--state-dir`: where session records live. Defaults to `$XDG_STATE_HOME/devbox-mcp`, falling back to
  `~/.local/state/devbox-mcp`.
- `--workdir-root`: the root for default session working directories (`SES-3`). Defaults to `~/devbox-mcp/work`, so a
  session named `foo` with no explicit working directory works in `~/devbox-mcp/work/foo`. The directory is created when
  the session is created.
- `--codex-bin`: the Codex executable. Defaults to `codex` on `PATH`.

## Storage

Each session is one JSON file, `<state-dir>/sessions/<name>.json`. It holds:

- the name, agent, working directory, model and effort, and the agent's native session ID once known (`SES-5`);
- the most recent turn (`SES-8`): its ID (turn IDs count up from 1 within a session), its state, when it started and
  ended, the agent's final message (at most 32 KiB, cut with a `final_message_truncated` flag), and, for a failed turn,
  the error that ended it (at most 4 KiB, ending in ` [cut]` when it was longer);
- every request ID the session has accepted (`TRN-6`), each with a SHA-256 of its message, the turn it started, and
  whether that turn reset the agent's context (`SES-5`), so a retry reports the reset too. They are kept for the
  session's whole life, so a retry is recognized however late it comes; each costs about a hundred bytes per turn.
  (SHA-256 rather than Rust's built-in hasher, whose output may change between Rust releases and would then reject
  legitimate retries after an upgrade.)

Every change rewrites the whole record synchronously: write a temporary file with a fixed name next to it, `fsync` it,
and rename it over the record. The runtime is single-threaded and a write has no `.await` in it, so two writes can never
interleave and no further coordination is needed. A send is accepted only once its record (the new turn in state
`running` and its request ID) is on disk, before the agent is started and before the send returns; if that write fails,
the send fails and nothing runs. If a later write fails, the call or turn that made it reports the error, memory keeps
the correct state, and the next successful write of that session brings the file up to date.

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

- `session_create { name, workdir?, agent? }`: `workdir` defaults to `<workdir-root>/<name>` and must be absolute. It is
  created with its parents if missing; if it exists it must be a directory (symlinks are followed). The session exists
  once its record is on disk; a directory created before a failure is left in place. `agent` must be `codex`. Returns a
  session summary. On a name collision, the error message includes the existing session's summary.
- `session_list { after? }`: returns `{ sessions: [summary], next_after? }`: at most 50 sessions in name order, starting
  after the name `after`; `next_after` is present when more follow. A summary is `{ name, agent, workdir, turn }`, where
  `turn` is the most recent turn as `{ id, state, started_at, ended_at?, final_message_preview?,
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

Cursors are opaque strings that encode the session name, an ID of the devbox-mcp process that issued them (random at
startup), and the sequence number of the last event the caller has seen (0 for "before the first event"). Event sequence
numbers count up from 1 within a session and process, across turns. Reads have two outcomes for a cursor:

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
writes one session record. There is no fixed time limit beyond that; a call that cannot make progress because the disk
is stuck is a failure of the box, not something devbox-mcp can bound.

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
cleanup cannot be confirmed, and the terminal state carries `cleanup_uncertain: true` (`TRN-4`, `LIF-2`).

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
read in a tight loop, and should expect long turns to take minutes.

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
  - coming back after devbox-mcp restarted (it should notice the lost turn or gap and carry on).

  Misuse is fixed by improving the descriptions, not the prompts. The check records which parts of the server's
  self-description the client actually showed the model. It is run whenever tools or their descriptions change, and its
  outcome is reported with the change. It does not replace trying the real thing with a Dot through the tunnel, which is
  the user's to do.

## Turn events

Events are agent-agnostic. Each has a `seq`, a `turn_id`, a `time`, a `type`, and, when it was cut down, `truncated`:

- `turn_started { message }`, with the message cut like any text field;
- `agent_message { text }`;
- `command_started { item_id, command }` and `command_finished { item_id, command, status, exit_code, output }`, where
  `status` is `completed`, `failed`, or `declined`, `exit_code` is null when the command did not run to an exit, and
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
- delete the agent's stored data for a session (`SES-6`, step 2).

With only Codex so far, the boundary is simply `codex.rs`'s functions: no trait, no dynamic dispatch, and nothing for
the later steps until they are built. The shape of a shared interface is decided when the second agent arrives (step 3),
from two real examples instead of one guess.

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
  `--last`, which picks whatever session ran most recently.
- `--skip-git-repo-check` is needed because Codex otherwise refuses to run outside a git repository, and session working
  directories need not be repositories.
- Defaults: model `gpt-6.1-sol`, reasoning effort `high`.
- The native transcript is a rollout file under Codex's home directory (`$CODEX_HOME`, by default `~/.codex`), at
  `sessions/YYYY/MM/DD/rollout-<timestamp>-<session id>.jsonl`. Codex compresses old rollouts to `.jsonl.zst` and
  removes the plain file, so reading or deleting a session's native data (steps 2 and 4 in TODO.md) must handle both.
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
the unit loads; the key appears nowhere else. The file is created afresh each time, never written through whatever was
at its path, so a symlink or hardlink there cannot carry the key into another file.

`devbox-mcp setup --tunnel-id <id>` never touches the key file and does two things:

- runs `tunnel-client init --force --profile devbox --profile-dir ~/.config/tunnel-client --tunnel-id <id> --mcp-command
  "<devbox-mcp> --codex-bin <codex>"`, so tunnel-client writes its own profile and devbox-mcp never writes
  tunnel-client's YAML. `<devbox-mcp>` is the binary running setup. The paths are absolute, because a user service's
  `PATH` is minimal and does not include the directories they usually live in. The profile directory is explicit, here
  and in the unit, so an `XDG_CONFIG_HOME` in the shell running setup cannot put the profile where the service does not
  look;
- writes the unit, `~/.config/systemd/user/devbox-mcp.service`, from one fixed piece of text with the absolute paths
  filled in, plus `Environment=PATH=` with the `PATH` setup runs with, so that the agents' commands find the user's
  usual tools rather than only those on the systemd user manager's minimal `PATH`. Relative and empty entries (the
  current directory, which a service must not depend on) and entries that would need escaping in a unit file are left
  out; setup reports all but the empty ones.

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
