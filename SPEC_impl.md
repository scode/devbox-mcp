# devbox-mcp implementation specification

NOTE: This describes how devbox-mcp is built. What it must do is in [SPEC.md](SPEC.md), which wins whenever the two
disagree. Choices here can change when there is a reason to; requirements in SPEC.md change only with the user's
agreement.

## Implementation status

Nothing in SPEC.md is implemented yet. The binary is a placeholder that does not speak MCP.

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
and no IPC, because running turns are not required to survive the process (`LIF-1`).

Each turn is an agent child process started with an argument array, never a shell string. On Linux the child is started
in its own process group with a parent-death signal set, so it is killed if devbox-mcp dies, and devbox-mcp kills the
whole process group on orderly shutdown and on interrupt. Known gap: if devbox-mcp is killed abruptly, the parent-death
signal reaches only the agent process itself, and commands the agent started may outlive it. Closing that gap (with a
subreaper or a cgroup) is future work.

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

Automated tests use only injected paths and fake agents, never the user's real state or a real model. Running real
agents is a manual step. How both work will be documented once the first working version exists.
