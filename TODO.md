# TODO

NOTE: This list holds work the user has asked to have tracked. Agents add to it only when the user asks them to.

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
