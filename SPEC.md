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
  in beyond what the tunnel itself requires (which is outbound only).
- `SEC-4`: Agents launched by devbox-mcp MUST run with full access and with all permission prompts disabled, using
  whatever that means for each agent. Their sandboxes and approval flows are not a security boundary here, and keeping
  them on would only stall unattended work.

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
  one, the working directory MUST default to a fixed, documented root directory joined with the session name.
- `SES-4`: The caller MUST be able to list existing sessions, and see for each one at least its name, agent, working
  directory, and whether a turn is running.
- `SES-5`: Sessions MUST survive restarts of devbox-mcp (see `TSP-2`). A caller MUST be able to resume a session after a
  restart and continue it with the agent's context intact, to the extent the agent itself preserves that context.
- `SES-6`: Removing a session MUST forget it in devbox-mcp and delete the agent's own stored data for that session.
  Removing a session MUST NOT delete or modify its working directory.
- `SES-7`: The caller MUST be able to choose the agent when creating a session, and MAY choose the model and reasoning
  effort. When not chosen, each agent has a documented default.

## Turns

A turn is one message from the caller and the agent's work in response, up to the point the agent stops and waits for
the next message.

- `TRN-1`: Sending a message to a session MUST start a turn and return promptly, without waiting for the turn to finish.
  The turn then runs in the background.
- `TRN-2`: At most one turn runs per session at a time. Sending a message to a session that has a running turn MUST be
  rejected with an error saying that the session is busy; the message is not queued. Different sessions MAY run turns
  concurrently.
- `TRN-3`: The caller MUST be able to follow a turn incrementally: each read returns the events that happened after a
  caller-held cursor (agent messages, commands run, file changes, errors, completion) plus the turn's current state. A
  read MAY wait for new events, but only for a bounded time.
- `TRN-4`: The caller MUST be able to interrupt a running turn. devbox-mcp MUST NOT report the turn as interrupted until
  the agent's processes for that turn have actually stopped.
- `TRN-5`: A turn ends in exactly one of: completed, failed, interrupted, or lost. "Lost" means devbox-mcp stopped
  tracking the turn (for instance because devbox-mcp itself restarted) and cannot say how it ended.

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

## Non-goals

These are deliberately out of scope. Some may be reconsidered later; until then, do not build towards them.

- Running turns that survive a restart of devbox-mcp (see `LIF-1`).
- Steering an agent in the middle of a turn, or answering its questions while the turn is still running. Follow-ups are
  new turns.
- Multiple users, or isolation between callers. There is one Unix user and one level of access.
- Any listener, web UI, or GUI (see `SEC-2`).
- Custom authentication or cryptography (see `SEC-1`).
- Webhooks or other push notifications to the caller.
