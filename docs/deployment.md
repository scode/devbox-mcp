# Deploying devbox-mcp

NOTE: devbox-mcp is an early prototype, and this guide has been followed end to end once, with a real tunnel and a
ChatGPT Dot. Expect rough edges, and check `journalctl` when something does not work.

WARNING: Anyone who can use the tunnel's ChatGPT connector can run anything on the box as your user, because the agents
devbox-mcp starts have full access and no permission prompts. Treat granting access like handing out an SSH key. See the
security model in [SPEC.md](../SPEC.md).

This sets devbox-mcp up the one way it supports: as a `systemctl --user` service on a Linux box with systemd, running
tunnel-client, which starts devbox-mcp over stdio. devbox-mcp itself opens no network port, and the service puts
tunnel-client's own health and admin listener on a Unix socket only your user can reach.
[SPEC_impl.md](../SPEC_impl.md), "Deployment", explains each choice in the unit.

## What you need

- A Linux box with systemd, and a user account on it whose files the agents should work on.
- A Rust toolchain, to build devbox-mcp from this repository.
- Codex installed and logged in as that user (`codex login`). Agents use your Codex login and configuration, including
  your own `AGENTS.md` rules.
- Optionally, Claude Code installed and logged in as that user, for sessions created with agent `claude`. They use your
  Claude Code login and configuration, including your `CLAUDE.md`, settings, hooks, and skills. Without it, Codex
  sessions work as usual and a Claude session's turns fail saying that `claude` could not be started.
- An OpenAI tunnel: its ID (`tunnel_` followed by 32 lowercase letters or digits) from the
  [tunnel management page](https://platform.openai.com/settings/organization/tunnels), and a runtime API key for it from
  the [API keys page](https://platform.openai.com/settings/organization/api-keys).

## Install devbox-mcp and tunnel-client

From a checkout of this repository:

- `cargo install --locked --path . --root ~/.local` installs devbox-mcp as `~/.local/bin/devbox-mcp`.
- `scripts/install-tunnel-client.sh` downloads the tunnel-client release this repository is tested with and checks its
  SHA-256. Then `install -m 755 target/tools/bin/tunnel-client ~/.local/bin/tunnel-client` copies it out of the build
  directory, so a `cargo clean` cannot break the service.

## Give it the runtime key

`devbox-mcp set-key` reads the runtime key from stdin, never from the command line, where other processes could see it,
and writes it to `~/.config/devbox-mcp/tunnel.env`, which only you can read. One way to type the key without it showing
up in your shell history or process list (`printf` is a shell builtin, so it never becomes a process of its own):

```
read -rs key
printf '%s\n' "$key" | ~/.local/bin/devbox-mcp set-key
unset key
```

You need this once per box, and again only to replace the key; rerunning setup leaves the key alone.

## Run setup

```
~/.local/bin/devbox-mcp setup --tunnel-id tunnel_...
```

Run the installed `~/.local/bin/devbox-mcp`, not `cargo run`: setup tells tunnel-client to start the very binary that
ran setup. It finds Codex on your `PATH` (pass `--codex-bin` to pick another one), and Claude Code too if it is there
(`--claude-bin`), and expects tunnel-client at `~/.local/bin/tunnel-client` (or `--tunnel-client`). If you install
Claude Code later, run setup again so the service knows where it is. Paths containing whitespace, quotes, backslashes,
`%`, or `$` are refused rather than escaped.

Setup writes two files and starts nothing:

- `~/.config/tunnel-client/devbox.yaml`, the tunnel-client profile, written by `tunnel-client init`.
- `~/.config/systemd/user/devbox-mcp.service`, the unit, which loads the key file.

Running setup again replaces both, which is how you change the tunnel ID or the agents' `PATH`.

## Start the service

Setup prints these, too:

```
loginctl enable-linger
systemctl --user daemon-reload
systemctl --user enable --now devbox-mcp.service
```

Lingering keeps your user's services running when you are not logged in, and across reboots. After changing the
configuration later, `systemctl --user daemon-reload` and `systemctl --user restart devbox-mcp.service` pick it up.

`journalctl --user -u devbox-mcp -f` shows the logs of tunnel-client and devbox-mcp. The service restarts on its own
whenever it exits, including when devbox-mcp crashes; `systemctl --user stop devbox-mcp.service` stops it for good.
Stopping it also stops every agent it started.

## Connect ChatGPT

Custom MCP servers need ChatGPT's developer mode, and the button to add one does not show until it is on. Where the
toggle lives has moved between ChatGPT releases, so look for "Developer mode" in ChatGPT's settings (under security, or
under the advanced settings for apps). On a Business, Enterprise, or Edu workspace, an admin may have to allow custom
MCP connectors first.

With the service running (tunnel-client must be running whenever the connector is created or used), add a custom MCP
server from ChatGPT's plugin settings, and in its form:

- Set **Connection** to **Tunnel**, not Server URL, and pick your tunnel. If it is not offered, your ChatGPT user may
  lack the "Use" permission on the tunnel.
- Set **Authentication** to **No authentication**. devbox-mcp has no OAuth of its own; access control is the tunnel's,
  and anyone who can use this connector can use the box.
- Acknowledge the warning about custom MCP servers and create it.

A Dot can then use devbox-mcp's tools. The tools describe themselves, so asking the Dot to do something on the box is
enough to get started, for example: "Using devbox, create a session called `smoke` and have the agent run `uname -a` and
`ls -la` in its working directory. Tell me exactly what it reported."

## How the agents run

Agents run as you: as the Unix user the service belongs to, with no sandbox, no approval prompts, and unrestricted
network access, so an agent can do anything you can do on the box with your files and credentials. Each works in its
session's working directory. Commands get no terminal and empty input, so most programs that would ask for input (a
password prompt, an ssh login, a confirmation) fail at once, and a calling model sees that failure in the turn's events.
A program can handle missing input differently, for example by asking again forever, and nothing times commands out, so
such a turn keeps running until it is interrupted; the calling model is told to keep agents non-interactive and to
interrupt a turn that stops producing events. The server's instructions say all this to the calling model, naming the
user, so it knows what it is handing work to.

Commands an agent runs see the service's environment, not your login shell's, with one exception: setup writes the
`PATH` of the shell you ran it in into the unit, so agents find the same tools you do. Without that, the service would
get the systemd user manager's `PATH`, which usually lacks directories such as `~/.local/bin` or `~/.cargo/bin`. Setup
leaves out relative entries and entries that would need escaping in a unit file, and says which.

To change the agents' `PATH`, run setup again from a shell with the `PATH` you want, then reload and restart the
service. Anything else your shell sets up (other environment variables, aliases, `nvm` and the like) is not carried
over.

Sessions keep their records in `~/.local/state/devbox-mcp`, and sessions created without an explicit working directory
work under `~/devbox-mcp/work`.

## Upgrading

Install the new devbox-mcp (and, if the repository pins a newer one, tunnel-client) the same way as above, then restart
the service. Restarting stops any turn in progress; its session reports the turn as lost afterwards.

ChatGPT keeps the tool list it fetched when the connector was created and does not fetch it again on its own. After an
upgrade that adds or changes tools, open the app in ChatGPT's settings and press Refresh, then start a new chat; chats
that already existed may keep the old tools. On a workspace where an admin approved the app, the admin has to review and
publish the update before anyone sees it.

## Removing it

```
systemctl --user disable --now devbox-mcp.service
rm ~/.config/systemd/user/devbox-mcp.service ~/.config/devbox-mcp/tunnel.env ~/.config/tunnel-client/devbox.yaml
systemctl --user daemon-reload
```

Session records (`~/.local/state/devbox-mcp`), working directories (`~/devbox-mcp/work`), and the agents' own data
(under `~/.codex` for Codex and `~/.claude` for Claude Code) are left alone; delete them too if you no longer want them.
Revoke the runtime key on the API keys page if it should stop working.
