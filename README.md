# devbox-mcp

> [!IMPORTANT]
> This repository is public, and you are welcome to use it, copy from it, or point your own agent at it. But unless real
> users show up and ask for it, there will be no release engineering: no releases, no prebuilt binaries, no packages or
> installers. You build it from source. I may also make breaking changes at any time, without notice or a migration
> path.

## What it is

ChatGPT Dots are not currently allowed to SSH into machines, so a Dot cannot work on your developer box the way you
would. devbox-mcp closes that gap. It is an [MCP](https://modelcontextprotocol.io/) server that runs on the box (an
Ubuntu machine is what it is developed and tested on) and is reached through the OpenAI Secure MCP Tunnel, so the box
opens no ports. Through it the Dot starts coding agents on the box (Codex or Claude Code), hands them work in long-lived
sessions, follows their progress, and reads their transcripts.

The [architecture diagram](https://snippets.scode.org/s/devbox-mcp-architecture/) shows how the pieces fit together and
which of them come from this repository, from OpenAI's upstream tooling, and from OpenAI's hosted services.

> [!WARNING]
> Giving an agent access to devbox-mcp is the equivalent of giving it an SSH key to the box. Agents it launches run with
> full access and no permission prompts. See the security model in [SPEC.md](SPEC.md).

## Building

- `cargo build` to build.
- `cargo test` to run the automated tests.

[docs/deployment.md](docs/deployment.md) explains how to set it up on a box as a systemd user service behind the tunnel.
[SPEC.md](SPEC.md) describes what devbox-mcp is meant to do, [SPEC_impl.md](SPEC_impl.md) how it is built and how far
along it is, and [docs/testing.md](docs/testing.md) how to test it, including against real agents.

## License

Apache-2.0. See [LICENSE](LICENSE).
