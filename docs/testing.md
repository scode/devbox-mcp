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
  command line, stdin, and working directory. It can also simulate failed and hanging turns, background leftovers, and
  noisy startup. Its hanging turn mirrors how real Codex runs shell commands, in a new session that a process group kill
  would miss; `tests/support/mod.rs` lists all the behaviors.
- `tests/sessions.rs` and `tests/turns.rs` cover the session and turn rules over stdio: names, working directories,
  persistence across restarts, busy sessions, interrupts, retried sends, cursors and gaps, paging, and lost turns.
- `tests/tunnel.rs` repeats the important paths with the real tunnel-client in between, via `dev proxy`: a whole session
  flow through its real request path, its real way of stopping devbox-mcp, and the check that a running turn does not
  hold up other calls, which matters because tunnel-client forwards calls one at a time (see SPEC_impl.md, "Process
  model").

The shutdown and cancellation tests are worth knowing about. They check that devbox-mcp exits cleanly and kills the
agent's whole process tree, across sessions, in every way it can be stopped: tunnel-client stopping it (SIGTERM plus
closing its stdin), tunnel-client dying (stdin closing alone), SIGTERM alone with stdin still open, SIGTERM during MCP
initialization, and the client cancelling the call.

Rules for these tests, all of which keep them hermetic: no real agents and no network; every path (working directories,
the agent executable) is injected through devbox-mcp's command-line flags into a temporary directory; child processes
get an explicit environment (only `PATH`, and a `HOME` in the temporary directory) so a developer's real configuration,
such as tunnel-client settings, cannot leak in; and no test modifies the environment variables of the test process. Test
tooling may listen locally (the dev proxy listens on a Unix socket in the test's temporary directory); devbox-mcp itself
never does.

## 2. Manual driver

`examples/mcp-call.rs` starts a built devbox-mcp, calls tools on it, and prints the raw MCP results as JSON. It is the
quickest way for a person or a coding agent to check behavior against the real Codex:

```
cargo build
scratch="$(mktemp -d)"
cargo run --example mcp-call -- --list \
    --server-arg=--workdir-root="$scratch/work" --server-arg=--state-dir="$scratch/state" \
    session_create '{"name": "demo"}' \
    session_send '{"name": "demo", "message": "Say hello world and nothing else."}' \
    wait_turn '{"name": "demo"}'
```

`wait_turn` is a step of the driver, not an MCP tool: it polls `session_read` once a second until the session's most
recent turn has ended, then prints every event and the final turn, which is what a caller does by hand.

Add `--via-tunnel-client` to route the calls through tunnel-client's dev proxy instead of talking stdio directly. That
is the closest local equivalent of production, and the mode to use whenever timing or the number of calls matters:
tunnel-client forwards requests to devbox-mcp one at a time and gives up on a response after a deadline (30 seconds by
default here, adjustable with `--response-timeout-secs`), so a slow call blocks every other call behind it. The stdio
mode hides both effects. In tunnel mode, tunnel-client (and so devbox-mcp and the agent) gets only an allowlisted part
of your environment, enough for the agent to find its tools and its login, so your own tunnel-client configuration
cannot leak in; see `AGENT_ENV_ALLOWLIST` in the driver if an agent needs more.

Always pass `--workdir-root` and `--state-dir` pointing into a scratch directory, so manual runs neither work in the
default `~/devbox-mcp/work` nor touch the default state directory, which a deployed devbox-mcp uses (and holds a lock
on, so a manual run against it would wait and then fail). Several `TOOL JSON` pairs on one command line run in order
against the same server process; use that whenever a later call depends on state from an earlier one, because each
invocation of the driver starts a fresh server.

Expect `wait_turn` to show the events `turn_started`, `agent_message`, and `turn_ended`, and a turn with state
`completed` and the agent's reply in `final_message`. A hello world turn took about 10 seconds when this was written.
Adding a second `session_send` and `wait_turn` to the same session, asking the agent about its previous turn, checks
that the agent kept its context.

## 3. A model as the client

The driver shows what devbox-mcp returns, but not whether a model can actually use it: whether the tool descriptions and
schemas lead it to make the right calls and understand the results. For that, put a real agent in front of devbox-mcp.
Claude Code in print mode can load devbox-mcp as an MCP server for one session:

```
cargo build
bin="$PWD/target/debug/devbox-mcp"
work="$(mktemp -d)"
cfg="$(printf '{"mcpServers":{"devbox":{"command":"%s","args":["--workdir-root","%s/work","--state-dir","%s/state"]}}}' \
    "$bin" "$work" "$work")"
(cd "$work" && claude -p --mcp-config "$cfg" --strict-mcp-config \
    --tools '' --allowedTools mcp__devbox__session_create mcp__devbox__session_list mcp__devbox__session_send \
        mcp__devbox__session_read mcp__devbox__session_interrupt -- \
    "On the dev box, have the agent say hello world, and tell me exactly what it replied." < /dev/null)
```

Running from the scratch directory keeps this repository's instructions out of the client's context, and
`--strict-mcp-config` keeps any other configured MCP servers out of the test. `--tools ''` removes Claude Code's
built-in tools (shell, file editing, and so on), so the client cannot do the work itself instead of through devbox-mcp.
`--allowedTools` is a different thing: it grants permission to use the listed tools without asking, which print mode
needs because nobody is there to approve a call. List exactly the devbox tools the scenario needs, as
`mcp__devbox__<tool>`. Both options take a variable number of values, so `--` is needed before the prompt, or the prompt
is swallowed as one more tool name.

### The cold-agent check

SPEC_impl.md "Self-documentation" asks for this layer to be run as a "cold agent" check whenever the tools or their
descriptions change: a model that knows nothing about devbox-mcp gets tasks phrased the way a user would give them to a
Dot, with no hints about tool names, polling, or cursors, and has to handle each correctly. Give each scenario its own
scratch `--workdir-root` and `--state-dir`, and add `--output-format stream-json --verbose` to see every call the model
made and what came back. The prompts used so far:

- Normal flow: "On my dev box, have an agent create a file hello.txt containing the word hi in its working directory,
  and tell me what it did. Then have the same agent append a second line with the word there, and show me the file's
  final contents." Expect one session, two turns, each followed to its end with `session_read`.
- Busy session: "On my dev box, have an agent run the shell command 'sleep 90' and then reply with the word done. As
  soon as you have started that, also ask that same agent what today's date is, and report both answers to me." Expect
  the second send to fail as busy, and the model to wait for the first turn (or interrupt it) before sending again.
- Failed turn: start devbox-mcp with `--codex-bin` pointing at a script that reads its stdin, prints an error to stderr,
  and exits 1, then ask "On my dev box, have an agent list the files in its working directory and tell me what it
  found." Expect the model to report that the turn failed, with the reason.
- Interrupt: "On my dev box, have an agent run the shell command 'sleep 600'. Once it has started, I changed my mind:
  stop it, and confirm to me that it has actually stopped." Expect `session_interrupt`, then reads until the turn is
  `interrupted`, and a report that mentions it if the turn has `cleanup_uncertain` set.
- Restart: two runs sharing one `--state-dir`. The first: "On my dev box, have an agent in a session named nightly run
  the shell command 'sleep 300' and then reply with the word done. Do not wait for it to finish; just tell me it has
  started." The run ends while the turn is still going, which stops devbox-mcp and its agent. The second: "Earlier,
  something was started on my dev box in a session named nightly. Find out how it went, and if it did not finish, run it
  again, but with 'sleep 5' instead of 'sleep 300', and tell me the result." Expect the model to find the turn `lost`,
  and send again to the same session.

A model with nothing else to do polls `session_read` back to back, since print mode has no way to wait; around 70 reads
for a 90-second turn were seen, and say nothing about the descriptions. So the budget is generous but finite: a scenario
fails if it takes more than 200 tool calls (count the `tool_use` entries in the stream output), or if the model stops
before the outcome it was asked for. Misuse that does say something about them (wrong tools, resending blindly,
misreading a result) is fixed in the descriptions, not in the prompts. To see what the client actually showed the model,
ask it in one more run to quote the devbox-mcp tool descriptions and any server instructions it was given. When this was
written, Claude Code showed the server instructions, the tool descriptions, and the argument schemas, but not the result
schemas, which is why anything a caller must know about a result is also in the tool's description.

Running from a scratch directory keeps this repository's instructions out, but Claude Code still loads your own
user-level instructions (`~/.claude/CLAUDE.md`), and the model may mention them. `--bare` would skip them but only
accepts API-key authentication.

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
- Move tunnel-client's health and admin listener off TCP before running it. The generated profile has it on
  `127.0.0.1:8080`, and its admin API can start Codex sessions with full access for any local process that asks (see
  "Deployment" in SPEC_impl.md). Under `health:`, replace `listen_addr` with `unix_socket:` and a path in a directory
  only your user can read, such as one under `$XDG_RUNTIME_DIR`. (Step 1's `devbox-mcp setup` will do this for you.)
- `tunnel-client doctor --profile devbox --explain`, then `tunnel-client run --profile devbox`.
- While it runs, add a custom MCP server with a Tunnel connection in
  [ChatGPT's connector settings](https://chatgpt.com/#settings/Connectors), select the tunnel, and ask the Dot to do
  something on the box through it. tunnel-client must be running whenever the connector is created or used.

Keep exactly one tunnel-client running per tunnel ID. tunnel-client shuts itself down when devbox-mcp exits, so a
crashing devbox-mcp shows up as tunnel-client exiting. What response deadline the hosted relay applies has not been
checked; it should not matter, since no call waits for an agent.
