# Agent instructions

This project is either public now, or may become public in the future. No content in this project should contain
personal information such as personal usernames, hostnames, details about the local environments, etc.

## Before finishing work

Run the same checks CI runs, with the same arguments, and make sure they all pass:

- `cargo fmt --all -- --check`
- `cargo clippy`
- `cargo test`
- `dprint check`

## Conventional Commits

All commit messages and PR titles must use Conventional Commit format: `<type>: <short summary>`

Allowed types: `feat`, `fix`, `docs`, `perf`, `refactor`, `style`, `test`, `chore`, `ci`, `revert`.

Append `!` after the type for breaking changes (e.g. `feat!: remove legacy endpoint`). Scope is optional.

Rules:

- Type reflects the user-visible effect, not the implementation activity. A bug fix that requires heavy refactoring is
  `fix`, not `refactor`. A new CLI flag is `feat`, not `chore`.
- The summary after the colon is lowercase, imperative mood, no trailing period.
- Keep the first line under 72 characters.

## Lore

The ./lore directory contains historical information about the project. Only use it for historical digging or when
requested. Current project behavior does not use or depend on it, and routine maintenance does not touch it.
