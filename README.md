# devbox-mcp

NOTE: This is an early prototype. It runs Codex sessions on the box, with turns that run in the background and are read
incrementally, but has no transcripts, no deployment tooling, and only Codex as an agent yet.

devbox-mcp is going to be an [MCP](https://modelcontextprotocol.io/) server that lets remote agents, such as ChatGPT
Dots, drive coding agents (Codex, and later others) on a Linux box, through the OpenAI Secure MCP Tunnel.

WARNING: Giving an agent access to devbox-mcp is the equivalent of giving it an SSH key to the box. Agents it launches
run with full access and no permission prompts. See the security model in [SPEC.md](SPEC.md).

## Building

- `cargo build` to build.
- `cargo test` to run the automated tests.

[SPEC.md](SPEC.md) describes what devbox-mcp is meant to do, [SPEC_impl.md](SPEC_impl.md) how it is built and how far
along it is, and [docs/testing.md](docs/testing.md) how to test it, including against real agents.

## License

Apache-2.0. See [LICENSE](LICENSE).
