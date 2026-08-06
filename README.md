# Mudular

A modern, keyboard-centric terminal MUD client: a lightweight, high-performance
alternative to desktop clients like Mudlet, with multi-character sessions in
split panes or tabs — strictly in the terminal.

**Status:** M0–M5 done. You can log in and play over plain Telnet or TLS,
with full option negotiation (NAWS, TTYPE/MTTS, CHARSET, ECHO password
masking, EOR/GA prompts), MCCP2 stream compression, TLS with pinning for
self-signed certs, config-file profiles with legacy charset fallback
(Latin-1, CP437), and automation: aliases, triggers, variables, and timers
in shareable YAML modules with global → module → profile scoping and
`/reload`. See the roadmap in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)
(§14) for what's next — GMCP/MSDP, scripting, and multi-character play are
not implemented yet.

## Highlights

Done:

- Plain Telnet and TLS ("STelnet") connections, fully async
- Full Telnet negotiation (NAWS, TTYPE/MTTS, CHARSET, ECHO, EOR/GA prompts)
- MCCP2 compression, including the mid-stream switchover
- TLS with full/pinned/insecure verification modes; TOFU pinning for the
  self-signed certificates many MUDs run
- Config-file profiles, remappable keybinds
- Triggers, aliases, variables, and timers in shareable YAML modules with
  global → module → profile scoping and live `/reload`
- Unicode done right: UTF-8, grapheme-aware wrapping, legacy charset
  fallback (Latin-1, CP437) for MUDs that predate it

Planned:

- GMCP + MSDP out-of-band data
- Multi-engine scripting behind one API: Lua first, JavaScript next,
  more embeddable engines pluggable
- True multi-character play: split panes/tabs, instant hotkey focus
  switching, unread indicators, strictly isolated session buffers
- Cross-session automation: one character's triggers/scripts can command
  or observe another's session (tank auto-calls the cleric's heals)
- Channel panes: tells/gossip/group chat routed to their own panes with
  unread badges, aggregated across characters, WoW-style
- Ships as a single static binary — no runtime dependencies

## Build & run

```sh
cargo build
cargo test
cargo run -- --host mud.example.org --port 4000
```

See [docs/USAGE.md](docs/USAGE.md) for connecting, profiles, TLS
verification modes, charsets, keybind remapping, and the full flag
reference. Example configuration lives in
[`examples/config/`](examples/config/); the real config directory is
`~/.config/mudular/` (or platform equivalent — see docs/USAGE.md).

## Architecture

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the full technical
architecture document: stack rationale, session pipeline design, protocol
layer, automation engine, Unicode strategy, and the milestone roadmap.
Development-process notes (including AI model guidance per milestone) live
in [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md).
