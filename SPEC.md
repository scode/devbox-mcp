# devbox-mcp specification

NOTE: This is the product specification: what devbox-mcp does and promises, independent of how it is built. It does not
describe the implementation; that lives in [SPEC_impl.md](SPEC_impl.md). When the two disagree, this file wins. Much of
what is specified here is not implemented yet; SPEC_impl.md tracks what is.

devbox-mcp lets a remote agent, such as a ChatGPT Dot, work on a Linux box by driving coding agents (Codex today, others
such as Claude soon) that run on that box. The remote agent creates named sessions, sends them messages, follows their
progress, and comes back to them later with their context intact. The intent is that driving a session feels like
talking to the agent on the box directly, not like being one relay away from it.

The key words MUST, MUST NOT, SHOULD, SHOULD NOT, and MAY are used as in RFC 2119. Requirements carry IDs (such as
`SEC-1`) so that code, tests, and reviews can refer to them.

## Security model

This section is a hard requirement. No agent may weaken, work around, or reinterpret it without extremely explicit
confirmation from the user, given for that specific change. "It was convenient for testing" is not such a confirmation.

The model is SSH key equivalence. Granting an agent access to devbox-mcp is meant to be the security equivalent of
handing it an SSH key to the box: once you have access you have access, period. And just as SSH access relies entirely
on SSH for its security, devbox-mcp relies entirely on the OpenAI Secure MCP Tunnel and its authenticated MCP flow.

- `SEC-1`: devbox-mcp MUST NOT implement its own authentication or authorization layer. There are no per-call policy
  checks, allowlists, approval prompts, or capability restrictions between an authenticated caller and the box. Any
  caller that reaches devbox-mcp has full access as the Unix user devbox-mcp runs as.
- `SEC-2`: The only way into devbox-mcp MUST be the authenticated MCP flow through the OpenAI Secure MCP Tunnel.
  devbox-mcp MUST NOT open any listening socket of any kind: no TCP or UDP port (loopback included), no Unix domain
  socket, no named pipe, no other inbound channel. It talks MCP over stdio to the tunnel client that started it, and
  that is all.
- `SEC-3`: Deploying devbox-mcp MUST NOT require opening inbound ports, changing firewall rules, or adding any other way
  in beyond what the tunnel itself requires (which is outbound only). This covers the programs devbox-mcp is deployed
  with, too: any local interface they offer (such as tunnel-client's own admin interface, which can start agents) MUST
  be reachable only by the Unix user devbox-mcp runs as, who has full access to the box anyway.
- `SEC-4`: Agents launched by devbox-mcp MUST run with full access and with all permission prompts disabled, using
  whatever that means for each agent. Their sandboxes and approval flows are not a security boundary here, and keeping
  them on would only stall unattended work. The one exception is restrictions the user configured themselves in an
  agent's own settings (for example Codex's execution-policy rules): devbox-mcp leaves those in force, as the user's
  deliberate choice, even though they can make an agent refuse some commands.

Because the trust model differs from SSH in one important way, this is stated plainly: the party actually holding the
"key" is whoever can invoke the devbox-mcp connector through the OpenAI tunnel (in practice, the principals the
connected ChatGPT workspace allows to use it), and OpenAI's relay sees every request and response in plaintext. SSH's
end-to-end encryption between you and the box has no equivalent here. Do not connect a box whose data you are not
willing to send through OpenAI.

## Transport

- `TSP-1`: devbox-mcp MUST work as a stdio MCP server started by the official OpenAI tunnel client (`tunnel-client`).
  ChatGPT Dots, reaching the box through the Secure MCP Tunnel, are the motivating client. Other clients that reach the
  box through the same tunnel SHOULD work, but are not specifically targeted.
- `TSP-2`: devbox-mcp MUST tolerate the tunnel client replacing its process and re-initializing MCP at any time. Any
  state a caller needs in order to continue its work (see Sessions) MUST survive that.

## Sessions

A session is a long-lived conversation with one agent on the box. It is the unit a caller comes back to, so work is not
a string of one-off invocations that each lose their context.

- `SES-1`: Sessions MUST be explicitly created, resumed, and removed by the caller. They are not tied to a single agent
  turn: one session holds any number of turns.
- `SES-2`: Every session has a name chosen by the caller. A name MUST start with a lowercase ASCII letter, MUST contain
  only lowercase ASCII letters, digits, `_`, and `-`, and MUST be at most 64 characters long. Names are unique among
  existing sessions.
- `SES-3`: Every session has a working directory, given as an absolute path at creation. If the caller does not give
  one, the working directory MUST default to a fixed, documented root directory joined with the session name. The
  directory is created if it does not exist. It may already exist and may be shared with other sessions; devbox-mcp
  places no other restriction on it (`SEC-1`).
- `SES-4`: The caller MUST be able to list all existing sessions, however many there are, and see for each one at least
  its name, agent, working directory, and the state of its most recent turn.
- `SES-5`: Sessions MUST survive restarts of devbox-mcp (see `TSP-2`). A caller MUST be able to resume a session after a
  restart and continue it with the agent's context intact, to the extent the agent itself preserves that context. If
  devbox-mcp cannot resume the agent's own conversation (for instance, because the first turn was lost before the agent
  reported how to resume it), it MUST say so when the caller next sends to the session, not silently start a fresh
  conversation under the same session.
- `SES-6`: Removing a session MUST forget it in devbox-mcp and delete the agent's own stored data for that session.
  Removing a session MUST NOT delete or modify its working directory.
- `SES-7`: The caller MUST be able to choose the agent when creating a session, and MAY choose the model and reasoning
  effort. When not chosen, each agent has a documented default.
- `SES-8`: The most recent turn's outcome MUST survive restarts of devbox-mcp along with the session: its state, the
  agent's final message, and, if it failed, why. A caller coming back later can then see how the last turn ended. Older
  turns' outcomes and the full event history of any turn need not survive; the agent's own transcript is the durable
  record (`TRA-2`).

## Turns

A turn is one message from the caller and the agent's work in response, up to the point the agent stops and waits for
the next message.

- `TRN-1`: Sending a message to a session MUST start a turn and return immediately, without waiting for the turn to
  finish or even to make progress (`BND-2`). The turn then runs in the background, owned by the session: once a send has
  been accepted, nothing that happens to the call that made it (the caller cancelling it, the response getting lost)
  affects the turn. Only an interrupt (`TRN-4`) or devbox-mcp stopping (`LIF-1`) ends a turn early.
- `TRN-2`: At most one turn runs per session at a time. Sending a new message to a session whose most recent turn has
  not yet ended MUST be rejected with an error saying that the session is busy; the message is not queued. Different
  sessions MAY run turns concurrently.
- `TRN-3`: The caller MUST be able to follow a session's turns incrementally: each read returns the events that happened
  after a caller-held cursor (agent messages, commands run, file changes, errors, completion), each marked with the turn
  it belongs to, plus the state of the most recent turn. A read returns what is there and never waits for more
  (`BND-2`). When devbox-mcp cannot honor a cursor (for instance, because it was issued before devbox-mcp restarted, or
  the events after it are no longer held), the read MUST say so explicitly rather than silently skip or repeat events,
  and still return the current state and a usable cursor.
- `TRN-4`: The caller MUST be able to interrupt a running turn. The request returns immediately (`BND-2`), and the turn
  reports that it is being interrupted until the agent's processes for that turn have actually stopped; only then is it
  reported as interrupted. If devbox-mcp cannot confirm that everything the agent started has stopped, it MUST say so
  (`LIF-2`).
- `TRN-5`: A turn is running, or being interrupted, until it ends in exactly one of: completed, failed, interrupted, or
  lost. "Lost" means devbox-mcp stopped tracking the turn (for instance because devbox-mcp itself restarted) and cannot
  say how it ended. A turn counts as ended only once everything devbox-mcp reports about it is final.
- `TRN-6`: Sending a message MAY carry a caller-chosen request ID. If a send with the same request ID was already
  accepted for the session, at any point in the session's life, devbox-mcp MUST NOT start another turn and MUST return
  the turn the first send started, even while that turn is still running, so a caller can safely retry a send whose
  response it never received. This check comes before the busy check (`TRN-2`). Reusing a request ID with a different
  message MUST be rejected. Request IDs are scoped to their session.
- `TRN-7`: Every read MUST report, alongside the events, whether the most recent turn's agent process is still alive,
  how long ago the last event was, and how long the turn has been running (or ran), so a caller can tell quiet work from
  a dead agent. Values devbox-mcp does not know (for instance after a restart) are reported as unknown, not guessed.

## Transcripts

- `TRA-1`: Because a session is a resumable conversation, the caller MUST be able to ask the agent about its own earlier
  work in that session (for instance, to analyze its transcript) simply by sending it another message.
- `TRA-2`: The caller MUST be able to read ranges of a session's transcript directly, without going through the agent,
  so it can see the details for itself. The transcript SHOULD be as close to the agent's own record as practical rather
  than a lossy summary of it.

## Lifetime and honest reporting

- `LIF-1`: Running turns do not survive devbox-mcp. When devbox-mcp exits, it MUST stop the agent processes it started,
  so that nothing keeps running on the box unobserved. Turns that were running are reported as lost afterwards
  (`TRN-5`); the session itself remains and can be resumed.
- `LIF-2`: devbox-mcp MUST report state as it actually is. It MUST NOT report a turn as finished or interrupted when it
  does not know that, and MUST NOT hide gaps such as a lost turn.

## Bounds

- `BND-1`: Every tool call MUST return within a bounded time, and every response MUST be bounded in size. Anything that
  could exceed the bound (turn events, transcript ranges, listings) MUST be paginated or truncated with an explicit
  marker, never silently cut.
- `BND-2`: No tool call waits for an agent to make progress: there are no long-polls, and callers poll for news. The
  OpenAI tunnel client forwards calls to devbox-mcp one at a time, so a call that waits holds up every other call,
  including calls for other sessions.

## Self-documentation

devbox-mcp's callers are agents, and the only documentation they reliably see is what the MCP server itself provides. An
agent pointed at devbox-mcp with no other context MUST be able to understand what it is for and use it correctly. The
quality bar is high: an agent that misuses devbox-mcp because its descriptions were vague, incomplete, or wrong is a bug
in devbox-mcp, not in the agent.

- `DOC-1`: The server's MCP instructions MUST explain the model a caller needs before its first call: what devbox-mcp is
  (coding agents running on a remote Linux box, with full access to it), what sessions and turns are, that turns run in
  the background and calls never wait, so the caller polls, and the typical workflow from creating a session to reading
  a finished turn's reply.
- `DOC-2`: Every tool's description MUST say what it does, when to use it, what it returns, how it fails, and what the
  caller should do next in the common cases (for example, after a busy error, or while a turn is still running). Every
  argument and every result field MUST be described in the tool's schema, including its units, limits, and defaults.
- `DOC-3`: Error messages MUST say what went wrong and what the caller can do about it, in terms of devbox-mcp's own
  tools.
- `DOC-4`: Descriptions MUST match behavior. A change to a tool's behavior updates its description in the same change.

## MCP tools

The tools devbox-mcp exposes, and what each one promises. SPEC_impl.md gives their exact schemas and limits. Tools for
the requirements not yet covered here (removing sessions, reading transcripts) will be added with them.

- `session_create`: creates a session (`SES-1`). Takes a `name` (`SES-2`), an optional absolute `workdir` (`SES-3`), and
  an optional `agent` (`SES-7`; `codex`, the only agent so far, by default). Returns the new session as `session_list`
  would show it. Fails if the name is invalid or already taken (the error then shows the existing session, so a caller
  retrying a create whose response was lost can tell whether it is its own), or if the working directory cannot be
  created or is not a directory.
- `session_list`: lists sessions (`SES-4`) a page at a time, in name order, each with its name, agent, working
  directory, and the state of its most recent turn, including that turn's outcome once it has ended (`SES-8`).
- `session_send`: sends a message to a session, starting a turn (`TRN-1`), with an optional request ID (`TRN-6`).
  Returns the turn's ID, whether this was a retry of an earlier send, and a cursor from which `session_read` returns
  that turn's events if they are still held. Fails if the session does not exist, is busy (`TRN-2`), or the request ID
  was used for a different message. Says so if the agent's earlier context could not be resumed (`SES-5`).
- `session_read`: returns a session's events after a cursor, or from the start of its most recent turn when no cursor is
  given (`TRN-3`), with the cursor to pass next time, whether more events are already waiting, the most recent turn's
  state and outcome (`TRN-5`, `SES-8`), and liveness (`TRN-7`). When more events are waiting, the caller should keep
  reading even if the turn has already ended, to see everything that led to the end.
- `session_interrupt`: starts interrupting a session's running turn (`TRN-4`) and returns immediately with the turn's
  state, which stays "being interrupted" until the agent's processes are gone; the caller follows it with
  `session_read`. If no turn is running, it changes nothing and returns the state of the most recent turn.

## Non-goals

These are deliberately out of scope. Some may be reconsidered later; until then, do not build towards them.

- Running turns that survive a restart of devbox-mcp (see `LIF-1`).
- Steering an agent in the middle of a turn, or answering its questions while the turn is still running. Follow-ups are
  new turns.
- Multiple users, or isolation between callers. There is one Unix user and one level of access.
- Any listener, web UI, or GUI (see `SEC-2`).
- Custom authentication or cryptography (see `SEC-1`).
- Webhooks or other push notifications to the caller.
