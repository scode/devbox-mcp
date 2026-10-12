# Testing devbox-mcp

NOTE: Only the automated tests run in CI. The manual flows below run real agents with full access to the machine they
run on (SPEC.md `SEC-4`), and they cost model usage. Run them deliberately, on a machine where that is acceptable, with
harmless prompts.

There are four layers, from cheapest and most hermetic to most realistic. Each later layer catches things the earlier
ones cannot, but needs more setup and is less repeatable.

## Prerequisite: tunnel-client

The tunnel tests and the driver's tunnel mode use OpenAI's [tunnel-client](https://github.com/openai/tunnel-client), the
same program that runs devbox-mcp in production. Install the pinned, checksum-verified release into `target/tools/`
with:

```
scripts/install-tunnel-client.sh
```

Nothing is installed system-wide, and no OpenAI account or credentials are needed: the tests use tunnel-client's local
`dev proxy` mode, which runs its real runtime against an in-memory stand-in for OpenAI's control plane.

## 1. Automated tests

```
cargo test
```

- Unit tests cover the agent adapters in isolation, for instance parsing Codex's event stream from lines captured off a
  real Codex run.
- `tests/stdio.rs` starts the real devbox-mcp binary and plays tunnel-client's role over stdio, with a fake Codex (a
  short shell script) in place of the real one. The fake records how it was invoked, so tests can check the exact
  command line, stdin, and working directory, and it can simulate failed and hanging turns.
- `tests/tunnel.rs` repeats the important paths with the real tunnel-client in between, via `dev proxy`: a turn through
  its real request path, and its real way of stopping devbox-mcp. No test yet covers its one-at-a-time forwarding or its
  response deadline (see SPEC_impl.md, "Process model"); the prototype's blocking `run_turn` would simply fail such a
  test. Once turns run in the background, add one that checks a running turn does not hold up other calls.

The shutdown tests in both files are worth knowing about. They check that when tunnel-client stops (SIGTERM plus closing
devbox-mcp's stdin) or dies (stdin closing alone), devbox-mcp exits cleanly and kills the agent's whole process group.

Rules for these tests, all of which keep them hermetic: no real agents and no network; every path (working directories,
the agent executable) is injected through devbox-mcp's command-line flags into a temporary directory; and no test
modifies the environment variables of the test process. Test tooling may listen locally (the dev proxy listens on a Unix
socket in the test's temporary directory); devbox-mcp itself never does.

## 2. Manual driver

`examples/mcp-call.rs` starts a built devbox-mcp, calls tools on it, and prints the raw MCP results as JSON. It is the
quickest way for a person or a coding agent to check behavior against the real Codex:

```
cargo build
cargo run --example mcp-call -- --list \
    --server-arg=--workdir-root="$(mktemp -d)" \
    run_turn '{"prompt": "Say hello world and nothing else."}'
```

Add `--via-tunnel-client` to route the calls through tunnel-client's dev proxy instead of talking stdio directly. That
is the closest local equivalent of production, and the mode to use whenever timing or the number of calls matters:
tunnel-client forwards requests to devbox-mcp one at a time and gives up on a response after a deadline (30 seconds by
default here, adjustable with `--response-timeout-secs`), so a slow call blocks every other call behind it. The stdio
mode hides both effects.

Always pass `--workdir-root` pointing at a scratch directory, so manual turns do not run in the default
`~/devbox-mcp/work`. Several `TOOL JSON` pairs on one command line run in order against the same server process; use
that whenever a later call depends on state from an earlier one, because each invocation of the driver starts a fresh
server.

Expect a `run_turn` result with `"completed": true`, a `reply` from the agent, and Codex's `agent_session_id`. A hello
world turn took about 10 seconds when this was written, in either mode.

## 3. A model as the client

The driver shows what devbox-mcp returns, but not whether a model can actually use it: whether the tool descriptions and
schemas lead it to make the right calls and understand the results. For that, put a real agent in front of devbox-mcp.
Claude Code in print mode can load devbox-mcp as an MCP server for one session:

```
cargo build
bin="$PWD/target/debug/devbox-mcp"
work="$(mktemp -d)"
cfg="$(printf '{"mcpServers":{"devbox":{"command":"%s","args":["--workdir-root","%s"]}}}' "$bin" "$work")"
(cd "$work" && claude -p --mcp-config "$cfg" --strict-mcp-config \
    --allowedTools mcp__devbox__run_turn \
    "Use the devbox run_turn tool to ask the remote agent to say hello world. \
     Then report exactly what the tool returned (all fields), and nothing else.")
```

Running from the scratch directory keeps this repository's instructions out of the client's context, and
`--strict-mcp-config` keeps any other configured MCP servers out of the test. `--allowedTools` should list exactly the
devbox tools the scenario needs (as `mcp__devbox__<tool>`), so the client cannot do the work with its own tools instead.

This talks stdio directly, without tunnel-client, so it says nothing about deadlines or serialization; combine it with
layer 2's tunnel mode for those. It is also not the same client as a ChatGPT Dot, which may present tools,
confirmations, and long-running calls differently.

## 4. Through the real tunnel

The only full end-to-end test is a ChatGPT Dot (or another ChatGPT client) calling devbox-mcp through the OpenAI Secure
MCP Tunnel. It needs an OpenAI organization with tunnel access, a tunnel associated with the ChatGPT workspace, and a
runtime API key, so it is the user's to run. An agent must not create credentials, register tunnels, or install services
on its own.

The commands below were checked against tunnel-client v0.0.16 (`init` was run; `doctor` and `run` need real credentials
and were not):

- Get a tunnel ID from [Tunnels management](https://platform.openai.com/settings/organization/tunnels) and a runtime API
  key from [Runtime API keys](https://platform.openai.com/settings/organization/api-keys), and make the key available as
  `CONTROL_PLANE_API_KEY` in tunnel-client's environment.
- Create a profile. The executable must exist and the path must be absolute, because `init` checks it: `tunnel-client
  init --profile devbox --tunnel-id <tunnel id> --mcp-command "/abs/path/devbox-mcp --workdir-root /abs/scratch/dir"`.
  This writes `~/.config/tunnel-client/devbox.yaml`.
- Decide about the health and admin listener. The generated profile has tunnel-client listen on `127.0.0.1:8080` for
  health checks and a local admin web UI. That is tunnel-client's listener, not devbox-mcp's, but it is a local way into
  tunnel-client; to avoid TCP entirely, replace `listen_addr` under `health:` with `unix_socket:` and a path in a
  directory only the service user can read.
- `tunnel-client doctor --profile devbox --explain`, then `tunnel-client run --profile devbox`.
- While it runs, add a custom MCP server with a Tunnel connection in
  [ChatGPT's connector settings](https://chatgpt.com/#settings/Connectors), select the tunnel, and ask the Dot to use
  `run_turn`. tunnel-client must be running whenever the connector is created or used.

Keep exactly one tunnel-client running per tunnel ID. tunnel-client shuts itself down when devbox-mcp exits, so a
crashing devbox-mcp shows up as tunnel-client exiting. What response deadline the hosted relay applies has not been
checked. If it has one, a `run_turn` that takes longer than that fails on the client side while the turn carries on and
its result is lost, which is what the dev proxy does at its own deadline. Background turns will remove that problem.
