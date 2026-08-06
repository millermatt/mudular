# Mudular

A modern, keyboard-centric terminal MUD client: a lightweight, high-performance
alternative to desktop clients like Mudlet, with multi-character sessions in
split panes or tabs — strictly in the terminal.

**Status:** scaffold (pre-M0). The module layout, protocol codecs, and config
schemas are in place; connecting to a MUD lands in milestone M0. See the
roadmap in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) (§14).

## Highlights (planned)

- Plain Telnet and TLS ("STelnet") connections, fully async
- Full Telnet negotiation (NAWS, TTYPE/MTTS, CHARSET, ECHO, EOR/GA prompts)
- GMCP + MSDP out-of-band data, MCCP2/3 compression
- Triggers, aliases, variables, and timers in shareable YAML modules with
  global → module → profile scoping
- Multi-engine scripting behind one API: Lua first, JavaScript next,
  more embeddable engines pluggable
- True multi-character play: split panes/tabs, instant hotkey focus
  switching, unread indicators, strictly isolated session buffers
- Cross-session automation: one character's triggers/scripts can command
  or observe another's session (tank auto-calls the cleric's heals)
- Channel panes: tells/gossip/group chat routed to their own panes with
  unread badges, aggregated across characters, WoW-style
- Unicode done right: UTF-8, emoji, wide glyphs at correct cell widths,
  legacy charset fallback (Latin-1, CP437)
- Ships as a single static binary — no runtime dependencies

## Build & run

```sh
cargo build
cargo test
cargo run -- --host mud.example.org --port 4000
```

Example configuration lives in [`examples/config/`](examples/config/); the
real config directory will be `~/.config/mudular/` (M3).

## Architecture

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the full technical
architecture document: stack rationale, session pipeline design, protocol
layer, automation engine, Unicode strategy, and the milestone roadmap.
Development-process notes (including AI model guidance per milestone) live
in [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md).
