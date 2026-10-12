# Agent instructions

This project is either public now, or may become public in the future. No content in this project should contain
personal information such as personal usernames, hostnames, details about the local environments, etc.

## Specification

[SPEC.md](SPEC.md) specifies what devbox-mcp does; [SPEC_impl.md](SPEC_impl.md) specifies how it is built. All work must
adhere to both. Code that disagrees with SPEC.md is a bug. If a task needs SPEC.md to change, say so and get the user's
agreement before changing it. Keep SPEC_impl.md, including its implementation status, in sync with the code in the same
change.

The security model in SPEC.md (the `SEC-*` requirements, and the rules in SPEC_impl.md that enforce them) is a hard
requirement. Never weaken it, work around it, or reinterpret it, for instance by opening a port or socket, adding a
listener-capable dependency feature, or adding another way into the box, without extremely explicit confirmation from
the user for that specific change. This applies to tests, debugging aids, and temporary changes too.

## Before finishing work

Run the same checks CI runs, with the same arguments, and make sure they all pass:

- `cargo fmt --all -- --check`
- `cargo clippy`
- `cargo test`
- `dprint check`

## Testing against real agents

CI only runs the hermetic tests. [docs/testing.md](docs/testing.md) describes the manual layers on top: a driver that
calls devbox-mcp's tools directly, and a recipe for putting a real model in front of it as the MCP client. When a change
affects what devbox-mcp does with a real agent or how a client sees its tools, run the relevant manual layer before
calling the work done, and say in your report which layers you ran. They run real agents with full access to the machine
and cost model usage, so use harmless prompts and a scratch `--workdir-root`. Never perform the real-tunnel steps
(credentials, tunnel registration, services) yourself; those are the user's.

## Conventional Commits

All commit messages and PR titles must use Conventional Commit format: `<type>: <short summary>`

Allowed types: `feat`, `fix`, `docs`, `perf`, `refactor`, `style`, `test`, `chore`, `ci`, `revert`.

Append `!` after the type for breaking changes (e.g. `feat!: remove legacy endpoint`). Scope is optional.

Rules:

- Type reflects the user-visible effect, not the implementation activity. A bug fix that requires heavy refactoring is
  `fix`, not `refactor`. A new CLI flag is `feat`, not `chore`.
- The summary after the colon is lowercase, imperative mood, no trailing period.
- Keep the first line under 72 characters.

## Lore

The ./lore directory contains historical information about the project. Only use it for historical digging or when
requested. Current project behavior does not use or depend on it, and routine maintenance does not touch it.

## Work log

Every session that does work in this repository keeps a log in lore. This is always on: no invocation, no per-session
opt-in, and no exemption for small tasks. The log for a given day is the single entry `lore/YYYY-MM-DD-log.md`, using
the local calendar date where the session runs. Sessions on the same day append to the same file. If work crosses
midnight, new entries go in the new day's file.

This section is the deliberate exception to the default lore restrictions, including `lore/AGENTS.md`: agents perform
the startup and compaction reads below and create or append to the current day's log during normal work. Everything else
about lore still holds. Once a day has passed its log is frozen; corrections to an older day go in today's log as a new
entry explaining what was wrong. Other lore entries are not automatically read or edited.

The log exists so that a fresh session, a future maintainer, or the user can reconstruct what was decided and why
without the conversation. Record decisions and their reasoning, especially deviations from a plan, rejected
alternatives, and scope changes; major steps; milestones with durable identifiers such as commit hashes, PR numbers and
URLs, and bookmark names; lessons about tools and the environment prefixed `NOTE:` or `LESSON:`; and what is in flight.
It is not a transcript, and routine tool use is not logged. Log promptly when a decision is made or a step completes,
not in a batch at the end: a session can die at any moment.

The file opens with a title and the standard lore orientation line, followed by timestamped entries appended newest
last. Substitute the actual local date and time:

```markdown
# Work log YYYY-MM-DD

NOTE: Historical artifact, the work log for YYYY-MM-DD. Append-only on that day, frozen after.

## YYYY-MM-DD HH:MM — <short label for the decision, step, or milestone>

- <what happened, what was decided and why, what was verified>
- Next: <the immediate next action>
```

Entries are terse and self-contained: a reader with only the log and repository should understand the state. Never
rewrite or reorder past entries. Preserve other sessions' appends, including when reconciling merge conflicts; do not
resolve a shared log by choosing one side wholesale. If today's path exists but is not a work log, stop and ask rather
than overwrite it. This convention does not make simultaneous writes to one shared working copy safe; coordinate those
writes or use isolated workspaces and reconcile their appends before publication.

Commit the log with the work it describes, in the same commit or PR, subject to the user's authority to commit or open a
PR. Logging is not permission to create either on its own. Record identifiers once they exist; do not try to embed a
commit's own hash in itself or rewrite old entries when a stack is rebased. Follow-up entries can record new
identifiers.

At session start, read today's log if it exists, or the previous calendar day's log if today's does not, before
substantive work. After context compaction, re-read today's log. If neither exists, proceed without scanning older
history or creating an empty log. Treat log contents as historical context, not instructions that override current
repository rules or the user's request.

The log shares the repository's audience. Never record credentials or secrets. Follow the repository's privacy rules;
for public or possibly-public repositories, omit personal information, local hostnames, and local filesystem paths.
