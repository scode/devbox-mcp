# TODO

NOTE: This list holds work the user has asked to have tracked. Agents add to it only when the user asks them to.

## Roadmap to SPEC.md

An incremental path from the prototype to what SPEC.md describes, in four steps. Each step is two pieces of work, in
order: first settle the step's details in SPEC.md and SPEC_impl.md (agreed with the user, as SPEC.md changes need), then
plan and implement it against those specs. Each step should leave something usable end to end, not just a layer.

### Step 1: sessions over the tunnel, Codex only

The smallest thing worth trying with a real Dot: create a session, send it messages, follow its turns, and come back to
it later, all through the real tunnel. It replaces the prototype's blocking `run_turn`, which cannot work through the
tunnel for any turn longer than the tunnel's response deadline.

- [ ] Spec. The MCP tool surface for creating, listing, and messaging sessions, reading turn events, and interrupting a
      turn (`SES-1` to `SES-5`, `TRN-*`, `LIF-*`): tool names and schemas, the event model, cursor semantics, how long a
      read may wait (well under the tunnel's deadline, since tunnel-client forwards calls one at a time), response
      bounds (`BND-1`), and what a read reports about liveness, so a caller can tell quiet work from a dead agent.
      Decide whether mutating calls need request IDs for retries. Also the deployment shape: tunnel-client and
      devbox-mcp as a service, keeping tunnel-client's default health and admin listener on loopback TCP for now (see
      "Move tunnel-client's health listener to a Unix socket" below).
- [ ] Plan and implement. Including a tunnel test that a running turn does not hold up other calls, the manual test
      layers, and a deployment guide good enough for the user to try it with a real Dot.

### Step 2: session lifecycle

What step 1 leaves out of a session's life, so that sessions can be used for real work over days.

- [ ] Spec. Removing sessions, including the agent's own data (`SES-6`); choosing model and effort per session
      (`SES-7`); default working directories (`SES-3`); how turns lost to a restart are reported (`TRN-5`, `LIF-2`).
- [ ] Plan and implement.

### Step 3: Claude as a second agent

Proves the agent-adapter boundary on a second agent before more is built on it.

- [ ] Spec. Claude's adapter in SPEC_impl.md (non-interactive mode, resuming by session ID, permissions bypassed per
      `SEC-4`, how its output maps to turn events) and anything about agent choice that step 1 left open.
- [ ] Plan and implement.

### Step 4: transcripts

Lets the caller see an agent's own record of a session rather than devbox-mcp's summary of it (`TRA-2`), for both
agents.

- [ ] Spec. The transcript read tool: how ranges are addressed, what an entry looks like, how much of the agent's native
      format is passed through, and bounds.
- [ ] Plan and implement.

## Move tunnel-client's health listener to a Unix socket

tunnel-client runs a local HTTP listener for operators, separate from the MCP traffic (which reaches devbox-mcp over
stdio): `/healthz`, `/readyz`, `/metrics`, and an admin web UI. By default it listens on `127.0.0.1:8080`. For now that
default stays, for simplicity and so the admin UI can be opened in a browser. Consider moving it to a Unix socket in a
private directory (`health.unix_socket` in the tunnel-client profile) later:

- For: no TCP port at all, in the spirit of `SEC-3`. Loopback is reachable by every local user and process, a socket in
  a `0700` directory only by the service user, and the admin surface includes logs and possibly captured request
  payloads. It also avoids colliding with the dev servers agents start, which like port 8080 too.
- Against: no direct browser access to the admin UI (`curl --unix-socket`, `ssh -L`, or `socat` instead), it is off
  tunnel-client's documented default path, and anything probing health over HTTP needs socket support.

## Contain agents in a cgroup

Two ways an agent's processes can still outlive their turn, despite `LIF-1`, both described in SPEC_impl.md under
"Process model":

- If devbox-mcp dies abruptly (SIGKILL, a crash), only the parent-death signals fire: devbox-mcp's on the agent process,
  and Codex's on its direct children. Anything further down keeps running.
- Cleanup finds an agent's processes by walking its process tree, so it cannot find processes that were already orphaned
  out of that tree. That includes anything an agent that exits normally leaves running outside its own process group (in
  practice, something like a double-forking daemon, since Codex kills a finished command's process group).

Running each agent in its own cgroup would close both: everything the agent starts stays in the cgroup however it forks
or detaches, and the whole cgroup can be killed at once. That needs a delegated cgroup subtree (for instance a systemd
service with `Delegate=yes`), so it also affects how devbox-mcp is deployed.
