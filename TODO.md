# TODO

NOTE: This list holds work the user has asked to have tracked. Agents add to it only when the user asks them to.

Items are in one of two buckets. "Near term" is work that is planned. "Possible future improvements" are ideas worth
keeping that nothing currently plans for; each says why it is not near term.

## Near term

### Roadmap to SPEC.md

An incremental path from the prototype to what SPEC.md describes, in four steps. Each step is two pieces of work, in
order: first settle the step's details in SPEC.md and SPEC_impl.md (agreed with the user, as SPEC.md changes need), then
plan and implement it against those specs. Each step should leave something usable end to end, not just a layer.

#### Step 1: sessions over the tunnel, Codex only

The smallest thing worth trying with a real Dot: create a session, send it messages, follow its turns, and come back to
it later, all through the real tunnel. It replaces the prototype's blocking `run_turn`, which cannot work through the
tunnel for any turn longer than the tunnel's response deadline.

- [x] Spec. The MCP tool surface for creating, listing, and messaging sessions, reading turn events, and interrupting a
      turn: tool names and schemas, the event model, turn states, cursors, retries, bounds, liveness, and durability.
      Drafted in SPEC.md and SPEC_impl.md, revised after an adversarial review and a complexity review, and agreed.
- [x] Plan and implement. Including a tunnel test that a running turn does not hold up other calls, the manual test
      layers, a passing "cold agent" check of the tool descriptions (`DOC-*`), the `devbox-mcp setup` command, and a
      deployment guide good enough for the user to try it with a real Dot.

#### Step 2: session lifecycle

What step 1 leaves out of a session's life, so that sessions can be used for real work over days.

- [x] Spec. Removing sessions, including the agent's own data (`SES-6`), and choosing model and effort per session
      (`SES-7`). (Default working directories and reporting turns lost to a restart moved into step 1.)
- [x] Plan and implement.

#### Step 3: Claude as a second agent

Proves the agent-adapter boundary on a second agent before more is built on it.

- [x] Spec. Claude's adapter in SPEC_impl.md (non-interactive mode, resuming by session ID, permissions bypassed per
      `SEC-4`, how its output maps to turn events) and anything about agent choice that step 1 left open.
- [ ] Plan and implement.

#### Step 4: transcripts

Lets the caller see an agent's own record of a session rather than devbox-mcp's summary of it (`TRA-2`), for both
agents.

- [x] Spec. The transcript read tool: how ranges are addressed, what an entry looks like, how much of the agent's native
      format is passed through, and bounds.
- [ ] Plan and implement.

### Contain agents in a cgroup

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

## Possible future improvements

### Exact agent liveness

`session_read` reports `agent_alive` as true until the turn ends, which can be a few seconds after the agent process has
exited, while its last output is collected and anything it left running is stopped (SPEC_impl.md, "Tools"). Tracking the
exit separately would make it exact, but it would need a new signal passed from the process runner through the agent
adapter into the session table, and a caller would not act any differently on the more precise value.
