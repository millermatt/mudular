# Mudular — project instructions

Terminal MUD client in Rust (ratatui + tokio). Design authority is
docs/ARCHITECTURE.md; build order is its §14 roadmap; per-milestone model
guidance is docs/DEVELOPMENT.md.

## Working on a milestone

- Before coding, read the milestone's row in ARCHITECTURE.md §14 and the TAD
  sections it references. The "done when" column is the acceptance criterion —
  the milestone is finished when it holds, not before, not beyond.
- Respect the module boundaries (TAD §4): `proto` and `engine` are sans-IO and
  depend on nothing above them; `session` composes them; `ui` never touches
  sockets. Don't cross these to save a few lines.
- Protocol/parser changes come with byte-fixture unit tests — follow the
  pattern in src/proto/telnet.rs.
- Prefer established crates per TAD §2.1. First-party code is only for the
  Telnet/MCCP/GMCP/MSDP core.

## Scope discipline

- Build only the current milestone. Later-milestone stubs stay stubs; leave
  seams, not speculative features.
- No new dependencies, abstractions, or config options beyond what the
  milestone needs.

## Commands

- `cargo test` — must pass
- `cargo clippy --all-targets` — no warnings
- `cargo fmt` — run before committing
- Conventional Commits for commit messages
