# Testing devbox-mcp

NOTE: Only the automated tests run in CI. The manual flows below run real agents with full access to the machine they
run on (SPEC.md `SEC-4`), and they cost model usage. Run them deliberately, on a machine where that is acceptable, with
harmless prompts.

There are four layers, from cheapest and most hermetic to most realistic. Each later layer catches things the earlier
ones cannot, but needs more setup and is less repeatable.

## 1. Automated tests

```
cargo test
```

Unit tests cover the agent adapters in isolation, for instance parsing Codex's event stream from lines captured off a
real Codex run. The integration tests in `tests/stdio.rs` start the real devbox-mcp binary and talk MCP to it over
stdio, the same transport tunnel-client uses, with a fake Codex (a short shell script) in place of the real one. The
fake records how it was invoked, so tests can check the exact command line, stdin, and working directory, and it can
simulate failed and hanging turns.

The shutdown tests are worth knowing about. They reproduce exactly what tunnel-client does to its child (close stdin and
send SIGTERM, or just close stdin when tunnel-client itself dies) and check that devbox-mcp exits cleanly and kills the
agent's whole process group. rmcp's client cannot do this, which is why those tests speak raw JSON-RPC.

Rules for these tests, all of which keep them hermetic: no real agents and no network; every path (working directories,
the agent executable) is injected through devbox-mcp's command-line flags into a temporary directory; and no test
modifies the environment variables of the test process.

## 2. Manual driver

`examples/mcp-call.rs` starts a built devbox-mcp, calls tools on it, and prints the raw MCP results as JSON. It is the
quickest way for a person or a coding agent to check behavior against a real agent, with nothing between the caller and
devbox-mcp but stdio:

```
cargo build
cargo run --example mcp-call -- --list \
    --server-arg=--workdir-root="$(mktemp -d)" \
    run_turn '{"prompt": "Say hello world and nothing else."}'
```

Always pass `--workdir-root` pointing at a scratch directory, so manual turns do not run in the default
`~/devbox-mcp/work`. Several `TOOL JSON` pairs on one command line run in order against the same server process; use
that whenever a later call depends on state from an earlier one, because each invocation of the driver starts a fresh
server.

Expect a `run_turn` result with `"completed": true`, a `reply` from the agent, and Codex's `agent_session_id`. A hello
world turn took about 10 seconds when this was written.

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

This layer is the closest local stand-in for a ChatGPT Dot, but it is not the same client: ChatGPT may present tools,
confirmations, and long-running calls differently.

## 4. Through the real tunnel

The only full end-to-end test is a ChatGPT Dot (or another ChatGPT client) calling devbox-mcp through the OpenAI Secure
MCP Tunnel. This needs an OpenAI organization with tunnel access, a tunnel associated with the ChatGPT workspace, and a
runtime credential, so it is the user's to run. An agent must not create credentials, register tunnels, or install
services on its own.

The rough shape, from the [tunnel-client documentation](https://github.com/openai/tunnel-client) (not yet exercised for
devbox-mcp, so verify each command against the installed release):

```
tunnel-client init --sample sample_mcp_stdio_local --profile devbox \
    --tunnel-id <tunnel id> --mcp-command "/absolute/path/to/devbox-mcp --workdir-root /some/scratch/dir"
tunnel-client doctor --profile devbox --explain
tunnel-client run --profile devbox
```

Then add a custom MCP server with a Tunnel connection in ChatGPT, select the tunnel, and ask the Dot to use `run_turn`.
Keep exactly one tunnel-client running per tunnel ID. tunnel-client shuts itself down when devbox-mcp exits, so a
crashing devbox-mcp shows up as tunnel-client exiting.
