# devbox-mcp implementation specification

NOTE: This describes how devbox-mcp is built. What it must do is in [SPEC.md](SPEC.md), which wins whenever the two
disagree. Choices here can change when there is a reason to; requirements in SPEC.md change only with the user's
agreement.

## Implementation status

The code is a primordial prototype whose job is to prove the end-to-end path (MCP client, devbox-mcp, agent, reply) and
the testing flow around it. It exposes a single tool, `run_turn`, which runs one Codex turn in a fresh session and
blocks until the turn ends, returning the agent's final message. Implemented so far: the stdio transport and the
security posture (`SEC-*`, `TSP-1`), a first Codex adapter, and `LIF-1` for orderly shutdowns.

Not yet implemented, and contradicted by the prototype until it is: sessions (`SES-*`), turns that run in the background
and are read incrementally (`TRN-*`; `run_turn` blocks instead), transcripts (`TRA-*`), model and effort selection
(`SES-7`), and response bounds (`BND-1`; `run_turn` returns the whole reply and blocks for as long as the turn takes).
`run_turn` goes away once sessions exist. Of the configuration flags below, only `--workdir-root` and `--codex-bin`
exist so far, and `--workdir-root` is currently used directly as the default working directory.

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
which is the practical reason behind `TRN-1` and `BND-1`, and long-polling reads must wait well under the deadline.

tunnel-client stops its child by closing the child's stdin and sending it SIGTERM, and a service manager may send
SIGTERM to every process of a service at once, with stdin still open. devbox-mcp treats stdin reaching end of file,
SIGTERM, and SIGINT alike as a request for an orderly shutdown, including while MCP initialization is still in progress.
It shuts its tokio runtime down, which drops every in-flight tool call, and dropping a call kills its agent. The
runtime's shutdown waits at most a second for blocking threads, because rmcp's stdin reader stays blocked in `read` for
as long as stdin is open. A tool call the client cancels (`notifications/cancelled`) is dropped the same way; rmcp only
signals the cancellation, so the handler races its work against it.

Each turn is an agent child process started with an argument array, never a shell string, in its own process group.
Killing that group is not enough: Codex runs every shell command in a new session, so a command's own children are in
another process group (observed with codex-cli 0.162.1, where `bash -c "sleep 303 & sleep 120"` left both sleeps running
after Codex was killed). A guard owned by the turn therefore kills the agent's whole process tree when the call is
dropped. It walks `/proc` for every descendant, whatever its session or group, stopping each one so nothing can fork or
escape while the walk runs, then kills them all. When the agent exits on its own, the turn ends at its exit rather than
when its output pipes close, which a leftover background process could keep open forever; the guard then kills what is
left in the agent's process group, and reading the pipes stops after a two-second grace period.

On Linux the child also gets a parent-death signal, so it is killed if devbox-mcp dies abruptly; it checks after setting
the signal that devbox-mcp did not die in the meantime. The kernel ties that signal to the thread that spawned the
child, not the process, which is why devbox-mcp runs a current-thread tokio runtime: every child is spawned from the
main thread.

Known gaps, both of which need a cgroup per agent to close:

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

Session records (name, agent, working directory, model and effort, and the agent's native session ID once known) are
stored one JSON file per session under the state directory, written atomically by writing a temporary file and renaming
it into place. That is enough to resume sessions after a restart (`SES-5`).

Turn events are kept in memory only, since a turn cannot outlive the process anyway. After a restart, a session whose
last turn never recorded an end reports that turn as lost (`TRN-5`). The durable record of what happened is the agent's
own transcript, which is also what transcript reads return (`TRA-2`).

## Agent adapters

Everything agent-specific lives behind an adapter interface, so adding Claude or another agent means adding an adapter
and not touching the session logic. An adapter knows how to:

- build the command line for a first turn and for a resumed turn, with permissions fully disabled (`SEC-4`);
- turn the agent's output stream into devbox-mcp's turn events, and extract the agent's native session ID;
- locate and read the agent's native transcript for a session;
- delete the agent's stored data for a session (`SES-6`).

### Codex

The Codex adapter uses non-interactive mode, checked against codex-cli 0.162.1:

- First turn: `codex exec --json --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check -C <workdir> -m
  <model> -c model_reasoning_effort="<effort>" -`, with the message on stdin.
- Later turns: `codex exec resume --json` with the same permission, model, and effort flags, then the native session ID
  and `-`, again with the message on stdin and the child's working directory set to the session's.
- The native session ID is the `thread_id` of the `thread.started` event, recorded as soon as it is seen. Never use
  `--last`, which picks whatever session ran most recently.
- `--skip-git-repo-check` is needed because Codex otherwise refuses to run outside a git repository, and session working
  directories need not be repositories.
- Defaults: model `gpt-6.1-sol`, reasoning effort `high`.
- The native transcript is the rollout file `~/.codex/sessions/YYYY/MM/DD/rollout-<timestamp>-<session id>.jsonl`.

`codex exec` cannot be steered mid-turn and has no live question channel, which is why SPEC.md makes follow-ups new
turns.

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
