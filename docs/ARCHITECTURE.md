# Mudular — Technical Architecture Document

**Status:** Draft v1 · **Scope:** Full architecture + incremental delivery roadmap

Mudular is a modern, terminal-based MUD client: a lightweight, high-performance,
keyboard-centric alternative to desktop clients like Mudlet, supporting multiple
concurrent character sessions in a single TUI.

Who these decisions are made for, and which actor wins when two conflict:
`docs/ACTORS.md`.

---

## 1. Goals and Non-Goals

### Goals
- Strictly terminal-based UI with ANSI / 256-color / 24-bit TrueColor output.
- Full Unicode correctness: UTF-8 native, emoji and wide-glyph rendering at
  correct visual widths, legacy charset fallback (Latin-1, CP437, …).
- Async networking: plain Telnet (RFC 854) and TLS-encrypted Telnet ("STelnet")
  with concurrent sessions.
- Robust Telnet option negotiation (NAWS, TTYPE/MTTS, CHARSET, ECHO, EOR) and
  out-of-band protocols (GMCP, MSDP) plus MCCP2/MCCP3 stream compression.
- Modular automation: triggers, aliases, and variables defined in YAML modules
  with a hierarchical scope model (global → shared modules → per-profile).
- Multi-character play: ≥2 sessions in split panes or tabs, zero-latency hotkey
  focus switching, per-session unread indicators, strictly isolated buffers.

### Non-Goals (for now)
- No embedded scripting language (Lua/JS) in early milestones — the YAML rule
  engine comes first; scripting is a designed-for extension (§14).
- No GUI/graphics protocols (MXP images, Mudlet mapper drawing). A text mapper
  may come later.
- No client-side multiplayer/shared state; Mudular is a single-user client.

---

## 2. Language & Stack Decision

**Chosen: Rust, with `tokio` (async runtime), `ratatui` (TUI), `crossterm`
(terminal backend).** Go + `bubbletea` was the alternative. Rationale:

1. **Latency and scheduling control.** A MUD client's hot path is
   bytes-in → parse → render. Rust has no GC pauses and no runtime-managed
   goroutine scheduling jitter; tokio lets us pin the render loop and network
   pipelines to a small, predictable task set. For a "zero-latency" input
   feel, worst-case latency matters more than average, and Rust's worst case
   is flatter.
2. **The byte-pipeline shape fits Rust.** MCCP inflation, Telnet IAC state
   machines, and charset decoding are classic incremental byte-parser
   problems. Rust's enums + ownership make sans-IO state machines (§6.1)
   safe and cheap; `bytes::BytesMut` gives zero-copy buffer slicing.
3. **Unicode ecosystem.** `ratatui` composes with `unicode-segmentation`
   (grapheme clusters) and `unicode-width` (terminal cell widths), which is
   exactly the machinery §9.3 requires. Bubbletea/lipgloss handle width via
   `go-runewidth`, which is workable but weaker on grapheme clusters (ZWJ
   emoji sequences, combining marks) — a stated hard requirement here.
4. **TLS story.** `tokio-rustls` is pure Rust, widely deployed, and layers
   over a `TcpStream` with the same `AsyncRead + AsyncWrite` interface, so
   TCP vs TLS is one enum in the transport layer (§5). Go's `crypto/tls` is
   also fine — this one is parity, not a differentiator.
5. **Modular state management.** The Elm architecture (bubbletea) is elegant
   but funnels *all* state through one update loop; with N sessions each
   running decompression + parsing + automation, we want per-session
   pipelines that own their state and communicate via channels, converging
   only at the UI. Tokio tasks + `mpsc` channels model this directly.
6. **Distribution (why not Python).** A stated goal is that non-techies can
   install Mudular on any OS. Python was considered and rejected primarily
   on this axis: it requires an interpreter + package management (pip/pipx/
   venv) or a freezer (PyInstaller/briefcase) that yields large, fragile,
   antivirus-suspicious bundles per OS — plus it is the weakest of the three
   on latency (GIL, GC) for the multi-session byte pipeline. Rust and Go
   both produce a single self-contained executable; with `rustls` (no
   OpenSSL) and static linking, Mudular ships as **one file per platform**
   with zero runtime dependencies. See §15 (Distribution).
7. **Costs acknowledged.** Rust compile times and iteration speed are worse
   than Go's (and far worse than Python's). Mitigation: a thin binary crate
   over well-separated modules, and heavy unit testing at the parser layer
   where iteration is fastest.

Supporting crates:

| Concern | Crate | Notes |
|---|---|---|
| Async runtime | `tokio` | multi-thread runtime; tasks, `mpsc`, `select!` |
| TUI | `ratatui` + `crossterm` | TrueColor, event-stream input |
| TLS | `tokio-rustls` (rustls) | webpki roots + optional pinning |
| YAML | `serde` + `serde_yaml` | archived upstream but stable; isolated behind `config::` so a maintained fork (`serde_yaml_ng`) is a one-file swap |
| Regex | `regex` | Unicode-aware by default; linear-time (no catastrophic backtracking from untrusted trigger patterns) |
| zlib | `flate2` | MCCP2/3 inflate |
| Charsets | `encoding_rs` | legacy fallback decoding (Latin-1, CP437 via lookup, …) |
| Unicode | `unicode-segmentation`, `unicode-width` | grapheme/width correctness |
| Errors/logs | `thiserror`, `anyhow`, `tracing` | file-based logs only (stdout belongs to the TUI) |
| ANSI parsing | `ansi-to-tui` (or `vte` if we outgrow it) | inbound SGR/escape → ratatui spans (§8) |
| Input editing | `tui-input` | grapheme-aware line editor widget |
| Config paths | `directories` | platform config dir discovery |
| Secrets | `keyring` | OS keychain for auto-login passwords (§10.1) |
| Scripting | `mlua` (vendored Lua 5.4) | feature `lua`, on by default; statically linked, so §15 still ships one file (§7.4) |
| Scripting (2nd engine) | `rquickjs` (QuickJS) | feature `js`, off by default; also static, and proves the `ScriptHost` abstraction (§7.4) |

### 2.1 Dependency policy

Prefer established third-party crates over first-party code wherever a
widely-used, maintained crate exists — everything in the table above follows
that rule. First-party code is reserved for the MUD-specific core where no
generally-accepted crate exists: the Telnet option state machine, MCCP
switchover, and GMCP/MSDP codecs. Candidates evaluated there
(`libtelnet-rs`, `telnet`, `nectar`) are niche, partially unmaintained, or
don't expose the mid-stream compression handoff MCCP2 requires; the
replacement is ~200 lines of sans-IO code under our own tests, which is the
cheaper risk. Revisit if the ecosystem matures.

---

## 3. High-Level Architecture

Layered, with strict one-way data flow per session and a single UI event loop.

```
                        ┌───────────────────────────────────────────┐
                        │                UI task (main)             │
                        │  ratatui render · layout · focus · input  │
                        └───────▲───────────────────────┬───────────┘
              SessionEvent mpsc │                       │ SessionCommand mpsc
                (per session)   │                       │   (per session)
        ┌───────────────────────┴───────┐   ┌───────────▼───────────────────┐
        │ Session task (one per char)   │   │  outbound: alias expansion,   │
        │                               │   │  command history, send queue  │
        │  bytes ─► MCCP inflate        │   └───────────┬───────────────────┘
        │        ─► Telnet FSM ─► IAC   │               │
        │        ─► charset decode      │               ▼
        │        ─► ANSI/line assembler │        Transport (write half)
        │        ─► trigger engine      │
        │        ─► scrollback buffer   │
        └───────────────▲───────────────┘
                        │ bytes
                 Transport (read half)
                 TCP  or  TLS(rustls)
```

Key properties:

- **One tokio task per session pipeline.** Each session owns its transport,
  parser state, decompressor, charset decoder, automation engine instance,
  and scrollback buffer. Nothing is shared between sessions ⇒ buffer
  isolation is structural, not disciplined.
- **UI is the only writer to the terminal.** Session tasks never print; they
  emit typed `SessionEvent`s over an `mpsc` channel. The UI task `select!`s
  over terminal input and all session channels.
- **Commands flow down as typed `SessionCommand`s** (send line, request
  reconnect, resize→NAWS, toggle option). The session task serializes writes.

### 3.1 Concurrency model

- Tokio multi-thread runtime; the UI loop runs as the main task using
  crossterm's async `EventStream`.
- Channels: `mpsc::channel` (bounded) per session in each direction.
  Backpressure on a flooding server slows that session's parser, never the
  UI or other sessions.
- No shared mutable state between sessions. The only cross-session state is
  the UI's `AppState` (layout, focus, unread counts) which lives on the UI
  task and is updated from events — no locks anywhere in the hot path.
- Cross-session automation (§7.5) does not weaken this: one session acts on
  another only via typed messages routed through the hub, plus read-only
  state snapshots published over `watch` channels. Display/stream buffers
  remain strictly per-session.

---

## 4. Module Map

Single binary crate for now (fast iteration); boundaries are kept
crate-shaped so modules can be promoted to workspace crates if compile
times ever demand it.

```
src/
  main.rs        CLI entry (clap), tracing init, runtime setup
  app.rs         UI event loop: terminal setup, select! over input + sessions
  ui/            ratatui widgets: pane layout, status bar, input line, tabs,
                 help overlay, config_editor.rs (in-client profile editor)
  scrollback.rs  RetainedLine: what a pane keeps (text + arrival + origin)
  session/       Session task: pipeline assembly, SessionEvent/Command types
  net/           Transport: TCP / TLS connect, unified AsyncRead+Write
  proto/
    telnet.rs    sans-IO Telnet state machine + option negotiation
    mccp.rs      MCCP2 inflate stage (MCCP3 not implemented, §6.4)
    gmcp.rs      GMCP subnegotiation codec (package + JSON payload)
    msdp.rs      MSDP subnegotiation codec (typed VAR/VAL/ARRAY/TABLE)
    charset.rs   CHARSET negotiation + decode with legacy fallback (§9)
  engine/        Triggers, aliases, variables; YAML module loading; scoping
    condition.rs `when:` parser + evaluator (§7.6)
    script/      ScriptHost abstraction, Lua and JS hosts (§7.4)
  config/        YAML config + profiles: schema structs, discovery, merge
```

Dependency rule (enforced by review): `proto` and `engine` depend on
nothing above them and do no I/O; `session` composes them; `ui` knows
nothing about sockets; `app` wires everything.

---

## 5. Networking Layer (`net`)

```rust
pub enum Transport {
    Tcp(TcpStream),
    Tls(TlsStream<TcpStream>),
}
```

- `net::connect(host, port, tls: TlsConfig) -> Transport`, then split into
  read/write halves owned by the session task.
- TLS via `tokio-rustls`: system/webpki root validation by default. MUDs
  frequently run self-signed certs, so profiles support
  `tls: { verify: full | pinned | insecure }` where `pinned` stores the
  certificate's SHA-256 on first connect (TOFU) and `insecure` is allowed
  but loudly surfaced in the UI.
- Reconnect with capped exponential backoff is a session-level policy (M9),
  not a transport concern.

---

## 6. Protocol Layer (`proto`)

### 6.1 Sans-IO design

Every protocol component is a pure state machine: bytes/events in, events/
bytes out, no sockets, no async. `session` feeds them and writes their
output to the transport. This makes the entire protocol layer unit-testable
with byte fixtures (including captures from real MUDs) and reusable if the
transport ever changes.

```rust
pub enum TelnetEvent {
    Data(Bytes),                        // application bytes (post-IAC)
    PromptBoundary,                     // GA or EOR received
    Negotiation { verb: Verb, option: u8 },  // WILL/WONT/DO/DONT
    Subnegotiation { option: u8, data: Bytes },
}
```

`TelnetMachine::feed(&mut self, input: &[u8]) -> Vec<TelnetEvent>` plus
`TelnetMachine::respond(...) -> Bytes` for negotiation replies. IAC-IAC
escaping/unescaping, SB…SE framing, and option state (per-option Q-method
state machine, RFC 1143, to prevent negotiation loops) live here.

### 6.2 In-band options

| Option | Direction | Behavior |
|---|---|---|
| ECHO (1) | server WILL | mask local input (passwords); unmask on WONT |
| SGA (3) | both | accept; suppresses GA only if server insists |
| TTYPE (24) / MTTS | server DO | reply cycle: `mudular` → `xterm-256color` → `MTTS <bitvector>` advertising ANSI+UTF-8+256color+TrueColor per MTTS spec |
| NAWS (31) | client WILL | send window size subneg on connect and on every terminal/pane resize; per-pane size, not full terminal |
| EOR (25) | server WILL | accept; EOR ⇒ `PromptBoundary` |
| CHARSET (42) | server WILL/DO | negotiate UTF-8; on rejection or absence fall back to profile-configured legacy charset (§9.2) |

GA (with or without SGA quirks) also yields `PromptBoundary`. The line
assembler (§8) uses prompt boundaries to distinguish "prompt" from
"incomplete line", which MUD clients that only split on `\n` get wrong.

### 6.3 Out-of-band: GMCP and MSDP

- **GMCP (option 201):** subneg payload is `Package.SubPackage <json>`.
  Codec splits package path and parses JSON lazily (`serde_json::Value`).
  Client sends `Core.Hello {"client":"Mudular","version":…}` and
  `Core.Supports.Set` after negotiation. The supported list is currently
  hardcoded to `["Char 1", "Room 1"]` (`gmcp::supports_message`), with no
  way for a profile, module, or script to add a package (tracked as
  [#25](https://github.com/millermatt/mudular/issues/25)). Events surface to the engine
  (triggers can match on GMCP packages, M7) and to UI consumers (vitals,
  room info) via `SessionEvent::Gmcp`, which also feeds the raw inspector
  view (F2, §14 M6).
- **MSDP (option 69):** typed parser for `VAR`/`VAL`/`TABLE_OPEN/CLOSE`/
  `ARRAY_OPEN/CLOSE` into a recursive `MsdpValue` enum. Where a server
  offers both, GMCP is preferred; MSDP values are normalized into the same
  internal "server data" map so the engine sees one namespace. MSDP has no
  literal payload worth showing raw — its wire form is `VAR`/`VAL`
  control-byte framing, not text — so its inspector event
  (`SessionEvent::Msdp`) carries the same flattened key/value pairs the
  store was fed, tagged `[MSDP]` in the same log GMCP lines share.
- **Store semantics:** payloads *merge* into the server-data map key by
  key, so a server sending a partial object update (`Char.Vitals {"hp":90}`
  after a fuller one) does not blank the keys it omitted. Arrays are the
  exception: both protocols flatten them to positional keys
  (`Char.Affects.0`), so a merge alone would keep a longer previous
  array's tail forever — a buff that never expires. Both flatteners
  therefore report each array's length alongside its pairs (`proto::
  Flattened`), and the store drops indices at or past it
  (`Engine::prune_gmcp_array` / `prune_msdp_array`; the MSDP side spares
  keys GMCP owns, since it was never allowed to write them). Removal is a
  change like any other: a key that vanished is reported to `on_peer`
  watchers with an empty value, which is what an absent `${...}` already
  resolves to (§7.1, §7.5).

### 6.4 Stream control: MCCP2/MCCP3

- **MCCP2 (86):** after the server's `IAC SB 86 IAC SE`, *all subsequent
  inbound bytes* are one zlib stream. The inflate stage therefore sits
  **in front of** the Telnet machine and is toggled by a marker event the
  Telnet machine emits when it sees the MCCP2 subneg. Pipeline handles the
  mid-buffer switchover (bytes after the SE in the same read are compressed).
- **MCCP3 (87):** same, mirrored for client→server writes (compress after we
  send our subneg). **Not implemented, and not planned** — outbound volume
  is a few bytes per keystroke, and server-side adoption of option 87 is
  thin enough that the option is never offered in practice. The option
  constant exists in `telnet.rs`; nothing negotiates it.
- Zlib stream end (`Z_STREAM_END`) cleanly returns the pipeline to
  passthrough, per spec, letting servers disable compression.

### 6.5 Pipeline order (inbound, per session)

```
socket bytes → [MCCP inflate?] → Telnet FSM → Data bytes → charset decode →
ANSI-preserving line assembler → trigger engine → scrollback + UI event
```

Subnegotiation events branch off after the Telnet FSM to GMCP/MSDP codecs.
This ordering is load-bearing: compression wraps Telnet; Telnet wraps
charset; charset wraps ANSI text. Getting it wrong corrupts streams — it is
pinned by integration tests with synthetic compressed captures.

---

## 7. Automation & Rule Engine (`engine`)

### 7.1 Concepts

- **Alias:** rewrites *outbound* input. Regex match on the typed line,
  expansion with capture substitution, may emit multiple commands
  (`send:` list) and set variables.
- **Trigger:** fires on *inbound* completed lines (or prompt lines —
  matched against the ANSI-stripped text; a later option can match raw).
  Actions: send commands, set variables, restyle the matched text or the
  whole line (`highlight:`, §7.7), gag/substitute the line, echo a local
  note, play the terminal bell, call a script (§7.4), send commands to
  *another* session (§7.5), route the line to a channel pane (§11.1), mark
  this as the room the character died in (`corpse:`, §16).
- **Condition:** an optional `when:` guard on an alias or trigger. The
  pattern decides what matched; the condition decides whether to act on it,
  reading captures, variables, and live server data (§7.6).
- **Variable:** string values in a per-session store; substituted into
  send/echo actions as `${name}`; captures bind `${1}`, `${name}` from
  named groups. All matching uses the `regex` crate: Unicode-aware classes,
  linear-time guarantee (a hostile server line can't freeze the client).
  Any rule can write one with `set:`, so a value parsed out of one line is
  available to every later rule — `set: {target: '${foe}'}` on a trigger,
  read back as `${target}` by an alias. **Writes land after the current
  line (or typed input) is fully processed**, so rules never observe each
  other's writes within it: two triggers matching the same line both read
  the store as it was before the line, and `;`-separated input parts
  likewise. That is deliberate — it keeps rule *order* semantically inert,
  so a scope layer that shadows a rule (§7.3) and shifts what fires when
  cannot change what any rule reads.
- **Timer:** fires once after a delay or on an interval; defined in YAML
  (`timers: [{every: 60s, send: [...]}]`) or created at runtime by scripts.

### 7.2 YAML module format

One module = one YAML file, shareable between profiles:

```yaml
# modules/uw-combat.yaml
name: uw-combat
description: Combat reactions for Underworld
variables:
  heal_at: "40"
aliases:
  - pattern: '^hh$'
    send: ["cast heal ${target}"]
triggers:
  - pattern: '^(?P<who>\p{L}+) has arrived\.$'
    send: ["look ${who}"]
  - pattern: '^Your health: (?P<hp>\d+)%'
    when: '${hp} < ${heal_at}'      # optional guard, §7.6 (M8)
    send: ["quaff heal"]
    gag: false
```

### 7.3 Hierarchical scope

Effective rule set per session = merge of:

1. **Global defaults** (`config dir/global.yaml`) — always loaded.
2. **Shared modules** — listed by name in the profile (`modules: [...]`),
   loaded from the modules directory, in list order.
3. **Profile overrides** — inline `aliases:`/`triggers:`/`variables:` in the
   profile, or a profile-local module file.

Later layers **shadow** earlier ones by rule `id` (explicit) or by exact
`pattern` (implicit); `enabled: false` in a later layer disables an
inherited rule without redefining it. Each session gets its own engine
instance compiled from this merge at connect time; `/reload` recompiles
(M5) without reconnecting.

### 7.4 Multi-engine scripting (M8)

YAML rules cover the common cases; scripts cover everything else. Scripting
is engine-agnostic behind one trait, so multiple languages are supported
and more can be added without touching the rule engine:

```rust
trait ScriptHost {
    fn load(&mut self, source: &ScriptSource) -> Result<()>;
    fn call(&mut self, hook: Hook, ctx: &mut ScriptCtx) -> Result<()>;
}
```

- **One `mud.*` API surface, identical across languages:** `send`, `echo`,
  `gag`/`substitute`, variables, server data (GMCP/MSDP), timers, and event
  hooks (`on_line`, `on_prompt`, `on_gmcp`, `on_connect`/`on_disconnect`).
  The API is defined once as Rust types; each host binds it to its language.
- **Timers: `mud.timer(seconds, fn)`.** The host keeps the callback and
  hands the engine an id and a delay; the engine holds the deadline
  alongside the YAML `timers:`, and the session sleeps on the earlier of
  the two. A script therefore cannot sleep on the session's task, and
  `engine` needs no clock of its own. They are one-shot: a heartbeat
  re-arms from inside its own callback, which is also what stops a slow
  callback stacking up behind itself.
- **Engines, feature-gated** so the binary carries only what's wanted:
  - **Lua** first, via `mlua` (vendored Lua 5.4, statically linked) — the
    MUD community's lingua franca; eases migration from Mudlet.
  - **JavaScript** second, via `rquickjs` (QuickJS, statically linked) —
    shipping a second engine early proves the abstraction is real. Behind
    the non-default `js` feature: it exists to keep the abstraction honest,
    not because every build should carry two VMs.
  - **The proof is a shared suite, not a shared shape.** Both hosts run the
    same conformance scenarios — same hooks, same effects, same errors,
    same abort — from ports that differ only in syntax. That is what makes
    "identical across languages" a testable claim rather than a promise
    about which functions exist. A session can host both at once: the file
    extension picks the engine per script, and a line reaches every host.
  - Others (e.g. `rhai`) slot in behind the same trait. Engines requiring a
    system runtime (Python/pyo3) are excluded: they break the single-binary
    distribution goal (§15).
- **Declaring scripts:** a module (or profile) lists them by file name:

  ```yaml
  name: uw-combat
  scripts: [uw-combat.lua]     # beside this file; the extension picks the engine
  ```

  A name is a name, not a path: scripts live next to the YAML that
  declares them, so a shared module is one directory to copy and a
  community module cannot reach the rest of the disk by naming `../`.
  Loading follows the scope layering (§7.3) — a profile's script sees what
  a shared module's script defined — and all scripts of one language share
  that session's single VM, so they can build on each other the way a
  Mudlet package expects. `engine` stays sans-IO (§4): the config loader
  reads the files and hands over their text.
- **Wiring:** YAML rules reference scripts
  (`script: {file: combat.lua, fn: on_death}`); scripts can also register
  triggers/aliases/timers programmatically. The function is called with the
  matched line and the rule's captures — numbered groups by position, named
  groups by name — so the pattern stays in the YAML where it can be read
  and overridden per scope, and only the *action* moves into code. Both
  halves are checked when the rules compile: the file must be one a layer
  declared, and the function must exist in it, so a typo'd `fn:` fails at
  load rather than becoming a rule that never does anything. A `when:`
  guard governs a script action like any other (§7.6): no fire, no call.
- **Shared state, not a parallel world.** `mud.get`/`mud.set` read and
  write the same variable store as `variables:` and a rule's `set:`, and
  `mud.data` the same server-data store as `${...}` and `when:`. A hook is
  another way to act on this session's state, not a second copy of it.
  Ordering is fixed and documented: on an inbound line the trigger table
  runs first, then the line hooks, so a script sees the variables that
  line's rules just set and has the last word on gagging it.
- **Execution model:** hooks run synchronously on the owning session's task
  and are expected to return quickly; a 100ms budget per invocation aborts
  runaway scripts so one session's script can't stall its pipeline (and
  never another session's). An aborted or failing hook says so in that
  session's scrollback — a script that stopped working is otherwise exactly
  as invisible as a trigger that stopped firing — and the effects it
  already asked for still stand, since they may already have been half
  applied.
- **Sandboxed by default:** no filesystem, network, or process access
  unless the profile grants it explicitly — shared community modules may
  carry scripts, and untrusted-by-default is the safe posture. The Lua host
  loads only the table, string, math, and utf8 libraries — `io`, `os`,
  `package`, `debug`, and `coroutine` are never created rather than deleted
  afterwards (coroutine would dodge the time budget below, since mlua's
  instruction hook doesn't propagate to coroutine threads) — and drops
  `load`/`dofile`/`require` so a script cannot fetch more code, and `print`
  so it cannot write through the TUI.

### 7.5 Cross-session automation (M7/M8)

Multi-character play is more than two panes: one character's session must be
able to drive another's — the tank's session telling the cleric's session to
heal at a health threshold, or to rebuff when an affect drops. Buffer/state
isolation (§3) is preserved by making every cross-session interaction an
explicit, typed message through the hub; sessions never touch each other's
state directly.

**Addressing.** Sessions are addressed by profile name (`cleric`); a second
session on the same profile gets a numeric suffix (`cleric-2`). `*` targets
all *other* sessions (classic "send to everyone" for group commands).

**YAML: cross-session actions (M7).** Triggers/aliases gain a `send_to`
action alongside `send`:

```yaml
# tank profile — heal me when I drop below 40%
triggers:
  - pattern: '^HP: (?P<hp>\d+)%'
    when: '${hp} < 40'              # optional guard, §7.6 (M8)
    send_to:
      cleric: ["cast 'major heal' Grunk"]
```

Routing: the tank session's engine emits the action; the hub forwards it to
the cleric session as a `SessionCommand::SendLine`. Whether injected
commands run through the **target's** alias expansion is the receiver's
choice — deliberately receiver-side, so the profile whose aliases would
execute is the one that opts in and a sender can never force it:

```yaml
# mudular.yaml — install-wide default
cross_session:
  expand_aliases: false   # injected commands are sent verbatim

# profiles/cleric.yaml — per-profile override
cross_session:
  expand_aliases: true    # let the tank use my `hh`-style aliases
  max_hops: 1             # default 1
```

Loop safety when expansion is on: expansion output is never re-expanded,
and every injected command carries a hop count — rules firing while a
remote command is being handled can only `send_to` again until `max_hops`
is exhausted, so two sessions' rules can't ping-pong forever. Either way
the target pane locally echoes `[from tank] cast 'major heal' Grunk` so
nothing happens invisibly. If the target session isn't connected, the
action is dropped with a warning in the *originating* pane.

**Peeking: peer state snapshots (M8).** Each session continuously publishes
a read-only snapshot — its exported variables plus its server-data map
(GMCP/MSDP: vitals, affects, room) — over a `tokio::sync::watch` channel;
every session holds receivers for all peers. Reads are local, lock-free,
and eventually consistent (staleness is one channel hop, microseconds).
YAML rules reference peer state as `${@tank.hp}`; a `when:` guard (§7.6)
can combine local and peer values.

The channels for every session named at launch are created by the hub
before any of them connects, so a rule may name a peer that is still
dialling — it reads that peer's empty snapshot until there is something to
publish, rather than failing. `/connect <profile>`, below, extends this
past launch: the hub keeps a `peer_registry` mapping every live session's
name to its own publish receiver, growing it as each `/connect` adds one.
A newly connected session is handed the current registry as its own set of
peers to watch (so it can read `${@tank...}` from the moment it starts);
every session already running is told about the new one via
`SessionCommand::AddPeer`, which both live sessions and ones mid-reconnect
handle identically — a peer added during a backoff is not lost by the time
the connection comes back. This is what closed the gap the mesh being
built once before `event_loop` and never revisited had left: a session
added later used to be invisible to every existing one and vice versa.

A session publishes only when its own state has moved, so a quiet character
costs its peers nothing, and it never watches itself: its own variables are
the live ones. `@` is a namespace of its own — a peer name cannot shadow a
local variable or a GMCP key, and an unknown peer or key resolves to
nothing, which leaves a template visibly unexpanded and a guard false, just
as an unknown local name does.

**`/connect <profile>`.** Adds a character to a running instance
without a relaunch, sharing `build_profile_target` with the CLI's own
startup path — a session added this way is constructed identically to one
named on the command line, and gets the same `-2`-suffix handling for a
repeated profile name (above). Deliberately narrow: it only builds and
wires the session (peer mesh both directions, focus moves to it); `--record`
never applies, since it is a startup flag with no CLI invocation to read
from at this point. Session *removal* is a separate, harder problem this
does not touch — `Focus::Session(usize)`/`input_session: usize` bake
index-as-identity into `AppState`, which adding a session is survivable
under and removing one is not (`SessionCommand::Disconnect` exists,
unwired). Left for whenever removal is actually built, since fixing it
without that pressure risks guessing at the wrong shape.

**Scripting API (M8).** The `mud.*` API (§7.4) adds:

- `mud.session("cleric"):send(cmd)` / `:echo(text)` — `send` is routed as
  above; `echo` is display-only, written straight into that character's
  pane by the hub, which owns the panes, so nothing runs at the far end and
  no hop limit applies. Both carry the `[from tank]` tag an injected
  command carries: nothing one session does to another happens anonymously.
  `mud.session` answers `nil` for a name no session holds, so a script can
  ask rather than assume.
- `mud.session("tank").data` / `.vars` — the peer snapshot (read-only), as
  it stood when the handle was made, so a hook reading two keys sees one
  moment of that character's life rather than two.
- `mud.on_peer("tank", "Char.Affects", fn)` — subscribe to a peer's
  server-data updates, by session and by key prefix, called as
  `fn(key, value)` for each key that changed. This is the clean solution
  for "rebuff when a buff wears off": the cleric's script watches the
  tank's affect list and reacts from the cleric's own session, where the
  response commands belong. The event names a dotted key prefix and not a
  protocol (`gmcp:`): the snapshot is source-agnostic by the time it is
  published — GMCP already won over MSDP at the store (§6.3) — so a
  subscription that named the source would break on a server that switched
  protocols without the data changing at all.

Both directions are therefore supported and equivalent: push (tank's rules
command the cleric) and observe (cleric's rules/scripts watch the tank).
Prefer observe for reactions that are really the cleric's job — the rule
lives with the character that acts, and it keeps working no matter which
session detects the condition first.

### 7.6 Rule conditions (`when:`, M8)

A pattern says *what* matched; a condition says *whether to act on it*.
`when:` is an optional guard on any alias or trigger — the rule fires only
if its pattern matches **and** the condition evaluates true:

```yaml
triggers:
  - pattern: '^Your health: (?P<hp>\d+)%'
    when: '${hp} < ${heal_at}'
    send: ["quaff heal"]
```

Without it, thresholds have to be smuggled into the regex, which is both
unreadable and wrong for anything the pattern doesn't literally capture —
a rule cannot otherwise consult a GMCP vital or a variable set by an
earlier rule.

**Not a script call.** The obvious alternative is to compile `when:` down
to a `ScriptHost` invocation (§7.4), and it is rejected deliberately:
script engines are feature-gated, so a plain YAML field would silently
require one to be compiled in, and `${hp} < 40` would mean whatever the
compiled engine's comparison semantics happen to be — Lua and QuickJS do
not agree about string/number coercion. A declarative config field must
mean one thing in every build. Conditions are also hot-path: the guard runs
on every line the pattern matches, and §7.4 hooks run synchronously on the
session task, so crossing a VM boundary to answer `30 < 40` is the wrong
trade. Finally, `engine` is sans-IO and depends on nothing above it (§4);
calling into a script host would invert that.

**Grammar.** Deliberately small — a guard, not a language. Comparison
(`< <= > >= == !=`), boolean `and`/`or`/`not`, parentheses, number and
string literals, and `${...}` terms. Anything beyond that is a script's
job (below).

**Evaluation.**

- **Compiled once,** at `Engine::compile`, alongside the rule's regex: a
  malformed condition fails at load with module context, exactly like an
  invalid pattern, rather than silently never firing at runtime. Matching
  then costs one tree walk over the already-resolved variable stores.
- **`${...}` is a term, never a textual substitution.** The name resolves
  to a value *during* evaluation, using the same order as `send:`
  expansion (§7.1): captures, then variables, then server data. Splicing
  the text in before parsing would let untrusted server data (§13) inject
  operators into the client's own predicate — a value of `0 or 1==1` would
  rewrite the condition rather than be compared by it.
- **Coercion:** compare numerically when both sides parse as numbers,
  lexically otherwise. Everything in the variable and server-data stores is
  a string, so `${hp} < 40` must not become a string comparison.
- **Undefined names evaluate the condition false,** and the rule does not
  fire. This departs from `send:` expansion, which leaves an unresolved
  `${foo}` verbatim so a typo is visible on screen — a boolean has no
  equivalent way to show itself, and "don't fire" is the safe failure for a
  rule that would otherwise send commands.
- **A bare term is not a condition.** `when: '${combat}'` is a load-time
  error, not a rule that quietly never fires: every value in the stores is
  a string, so a term alone has no truthiness to define. Write the
  comparison out.
- **A guarded-out rule does nothing at all** — it does not `gag:` or
  `route:` the line either, since the rule as a whole did not fire. For an
  alias, a false guard is not a match, so the next matching alias, or
  failing that the literal input, still gets its turn.
- **Peer values (M8):** `${@tank.hp}` resolves through the peer snapshot
  (§7.5) with the same rules, so one character's guard can read another's
  vitals.

**First-party, per §2.1.** `evalexpr` and similar crates fit, but bring
their own value model and error type to adapt for a grammar this size;
`rhai` is excluded specifically because §7.4 lists it as a candidate
*script* engine, and using it here would blur the boundary `ScriptHost`
exists to draw. Roughly 200 lines of sans-IO code under byte-level tests,
which is the same trade §2.1 already makes for the protocol core. Revisit
if the grammar ever grows past the list above.

**Where scripts take over.** `when:` covers stateless predicates over
values already in the store. Anything with memory, arithmetic, or
multi-step logic — "heal if HP is low *and* nobody healed in the last three
seconds" — belongs in an `on_line` hook or a `script:` action (§7.4), which
can simply return early. That split is the same one §7.4 already draws:
YAML for the common cases, scripts for everything else.

### 7.7 Highlights (M9, built early)

Recolouring text the server sent is the cheapest attention mechanism a MUD
client has: your name in a wall of chat, the one word in a room
description that matters, a rare drop in a loot list. Unlike a channel
pane it moves nothing and hides nothing, so it is safe to apply liberally.

**A highlight is a trigger action, not a rule type of its own.**

```yaml
triggers:
  - pattern: '\bKestrel\b'
    highlight: {fg: bright_yellow, bold: true}
  - id: low-hp
    pattern: '^You are bleeding'
    highlight: {fg: white, bg: red, whole_line: true}
```

Making it an action rather than a parallel `highlights:` list is the whole
design decision, and everything below follows from it: highlights inherit
`id`-based shadowing and `enabled: false` across scope layers (§7.3), the
`;`-free regex machinery with named captures, `when:` guards when those
land (§7.6), and the ability to sit on a rule that *also* sends or gags. A
separate list would have needed its own copy of all of that.

- **Style fields:** `fg`, `bg` (a colour name, `#rrggbb`, or a 0-255
  palette index — the same vocabulary as a profile's `color:`, §11), and
  the boolean attributes `bold`, `italic`, `underline`, `reverse`. All
  optional; a `highlight:` block that sets none is a load error rather
  than a rule that silently does nothing.
- **Scope of the restyle:** the matched text only, or `whole_line: true`
  for the entire line. Matching a capture group rather than the whole
  match is deliberately *not* offered — the pattern can be narrowed
  instead, and one obvious behaviour beats two.
- **Compiled at load.** The style block becomes an SGR parameter string
  (`"1;93"`) at `Engine::compile`, so a bad colour name fails at startup
  naming the rule, and the hot path per line is a string splice rather
  than a colour lookup.

**Layering: the engine returns ranges, the session applies them.**
`LineOutcome` grows a list of `(byte range, SGR)` spans over the
*stripped* line the engine matched against. It deliberately does not
return styled text: the engine never sees the original ANSI, and
inventing escape sequences there would put rendering decisions in a
sans-IO module (§4). The session, which holds both the raw line and the
stripped projection, maps the ranges back through the same offset walk
`strip_ansi` already performs and splices the sequences in.

**Preserving the server's own colour is the hard part.** A naive
implementation closes a highlight with `ESC[0m` and destroys whatever
colour the server had running for the rest of the line. So the splice
restores the SGR state that was active at that point, recomputed from the
prefix of the raw line. A highlight inside a coloured region must leave
the region looking untouched on both sides — that is the acceptance test,
not an implementation detail.

- **Overlaps: first match wins**, matching `route:`'s rule (§11.1). Spans
  are applied in scope order and a span overlapping one already applied is
  dropped whole rather than nested — nested SGR has no well-defined
  "restore to the middle state", and a rule that silently half-applies is
  worse than one that doesn't.
- Highlights apply to lines and to lines copied into channel panes, since
  both carry the same styled text. Prompts are out of scope: they do not
  go through `process_line`.
- Gagged lines are never highlighted, for the obvious reason. A rule that
  sets both is not an error — `gag` simply wins.

### 7.8 Desktop notifications (`bell:`, M9)

The one alert a highlight can't give: something happened in a pane you
aren't looking at right now.

```yaml
triggers:
  - pattern: 'You have been slain'
    bell: true
```

**Also a trigger action, for the same reason `highlight:` is (§7.7)** — it
inherits shadowing, `when:` guards, and composes with a rule that also
sends or gags, with no parallel list to keep in step.

- **Independent of `gag:`**, unlike `highlight:`: a line worth hiding from
  scrollback (an OOC channel spammed by a bot, say) can still be worth an
  alert, so both may be set on the same rule without one cancelling the
  other.
- **The engine only reports the request.** `process_line` has no notion of
  which pane is focused — that state lives in the hub (`app`), one layer
  up, and `session` (which does know a `Bell` fired) has no terminal to
  ring it on either (§4: neither is allowed to touch the terminal). So a
  fired `bell:` becomes `SessionEvent::Bell`, and the hub — which already
  tracks focus for unread counts (§11) — decides whether to actually ring
  it: only for a session that isn't the focused pane, since a focused
  session's own alert is just noise.
- **Rung as a terminal `BEL` plus an OSC 9 notification** (`ESC ] 9 ;
  text BEL`) naming the session. `BEL` alone reaches terminal multiplexers
  watching for activity (tmux's `visual-activity`) and terminals that beep
  or flash; OSC 9 additionally reaches desktop notification centers on
  terminals that forward it (iTerm2, kitty, foot, and others) — sent
  together so a fired trigger reaches whichever the player's terminal
  understands, and is silently ignored by the rest.

### 7.9 Speedwalk paths (M9)

`.3n2e` — a leading `.` followed by count+direction pairs — is the
TinTin++/zMUD convention for a string of moves without spelling each one
out. Deliberately no room graph: that's the post-1.0 auto-mapper's
pathfinding, which supersedes this (§16). This is pure text expansion.

```
.3n2e       → n, n, n, e, e
.2ne1d      → ne, ne, d          (two-letter diagonals are one move, not two)
home; .2s1w → whatever `home` sends, then s, s, w
```

- **Directions:** `n s e w u d ne nw se sw`, matched longest-first so a
  diagonal isn't misread as its two components — `.ne` is one move, not
  `n` then `e`. A count defaults to 1 when omitted.
- **Expands wherever a send is queued from typed input** — a line typed
  directly, or an alias's `send:` template — so a stored alias *is* a
  speedwalk macro (`send: [".2s1w"]` behind a memorable name) with no
  separate macro schema to add. Trigger and script sends are untouched:
  those react to the server, not the player choosing to walk somewhere.
- **A pattern that doesn't parse — no leading `.`, a token that isn't a
  digit run followed by a known direction, or a count/total past a sanity
  bound (999 either way, so a typo can't queue a send storm, §13) — is
  sent unchanged.** `.` stays an ordinary character in anything that isn't
  a valid path, so a channel command like `.who` or a typed `.` is never
  mistaken for one.
- One command per step, matching how a MUD reads movement: a path is a
  list of separate sends, not one line the server has to parse itself.

---

## 8. Line Assembly & Scrollback

- The assembler (`session::line::LineAssembler`) consumes decoded text +
  prompt boundaries and emits one `SessionEvent::Line`/`Prompt` per
  completed line, still carrying the server's raw ANSI. Triggers match
  against a stripped projection computed separately (`strip_ansi`, §7.1)
  rather than a second stored copy: **inside the pipeline** the line is
  still in flux — a `highlight:` splice or a `mud.substitute` rewrites it
  after the engine has matched — so a projection cached there would be one
  more thing to invalidate, and stripping is cheaper to redo than to keep
  in step. That reasoning stops at the end of the pipeline: what
  `push_line` retains is final and never edited again, so the projection it
  caches (below) cannot drift from the text it describes. Recompute while
  the line can still change; cache once it can't.
- Rule highlights (§7.7) are spliced into that raw text before it reaches
  scrollback, so a highlighted line is an ordinary ANSI line by the time
  it's stored — nothing downstream (rendering, channel routing) knows the
  difference.
- ANSI parsing uses `ansi-to-tui` to turn SGR (16/256/TrueColor) into
  ratatui spans at render time, not at assembly time — one parse pass, run
  when a frame actually needs it, rather than a stored structured
  representation that has to be kept in step with every mutation between
  assembly and render (the highlight splice, `mud.substitute`). If
  MUD-specific quirks outgrow it, the fallback is a thin parser on `vte`
  (Alacritty's escape parser).
- **Two escape parsers, deliberately not unified.** `strip_ansi` (what
  triggers match, §7.1) and `ansi-to-tui` (what renders) do not agree on
  every input: `strip_ansi` treats `ESC` + any byte as a two-byte escape
  and drops both, and consumes DCS/APC/PM strings, where `ansi-to-tui`
  keeps the trailing byte as literal text. So a line containing `ESC M`
  displays as `M` while a trigger matched the line without it. Unifying
  means either loosening `strip_ansi` — which would weaken the projection
  §13 depends on — or normalising every line before render, a per-line
  pass on the hot path §2 exists to protect. Neither is worth paying for
  sequences no MUD sends: the divergence is between two *strict* readings
  of escapes that are already meaningless here. What made it matter was
  `Alt+V` building a pattern with the render-path parser, so a picked line
  could not match itself; that used the retained projection instead
  (§10.2). Revisit if a real server is ever found sending these.
- Scrollback is a bounded `VecDeque<RetainedLine>` per pane — session panes
  and channel panes alike (§11.1) — discarded oldest-first, bounded by the
  `scrollback_size` setting in `mudular.yaml` (a `usize`, 10,000 default,
  no file persistence), the same shape as `history_size` (§11.3).
- A `RetainedLine` (`src/scrollback.rs`) is `text` plus `at` (local arrival
  time) and `origin` — `Server`, `Client` (the client's own notices and
  warnings), `Rule` (the player's automation: a trigger's `echo:`, a
  `mud.echo()`, a timer, `on_connect`), `Echo` (a command echoed back as it
  was sent), or `Session(name)` (a cross-session `echo_to`, §7.5, an
  injected command's local echo, or a channel route naming the character it
  arrived in). `SessionEvent::Line` carries the origin too: a trigger echo,
  an auto-login notice and an injected command all reach a pane by the same
  event as server text, so the session states which it is rather than
  leaving `app` to assume. `text` is the line as it should read — server ANSI with
  `highlight:` splices already applied (§7.7) — and carries nothing a pane's
  own layout can add back at render time: a channel pane's `HH:MM:SS` and
  `[character]` tag are composed by `ui::draw_channel` from `at` and
  `origin`, not spliced into the text when the line was routed.
  The distinction is not decoration. Flattening provenance into the string
  is what makes a client warning indistinguishable from server text once it
  has scrolled (`UX_REVIEW.md` D), and what a "while you were away" digest,
  tell threading, transcript export, and scrollback search (§11.5) would
  each have to parse their way back out of. `push_line` is the one funnel
  every line passes, so the buffer records what it knows and the renderer
  composes — rather than the reverse.
- A retained line also carries its **plain-text projection** — `text` with
  escapes removed, the same `strip_ansi` projection triggers match against
  (§7.1) — computed once on the way in and read through `RetainedLine::plain`.
  Scrollback search (§11.5) matches against it; deriving it per keystroke
  would re-strip the whole buffer on every character typed. It is stored
  only when `text` actually contains escapes, since otherwise the text
  already is the projection. `strip_ansi` lives in `scrollback` for this
  reason — it is a property of a retained line, not of the session
  pipeline, and keeping it there leaves the module a dependency-free leaf
  (§4, §16).
- **Disk logging** (M9): a profile's `log: true` appends every line that
  reaches `push_line` — the same choke point scrollback fills from — to
  `<config dir>/logs/<session name>.log`, opened once at connect and kept
  open for append. No second buffer: the transcript is exactly what the
  pane shows, so a masked line is excluded from the log for the same
  reason it never reaches scrollback (§13). A write or open failure
  disables logging for that session rather than ending it.

---

## 9. Unicode & Charset Strategy

### 9.1 Internal representation
Everything past the charset decoder is Rust `String` (UTF-8) — invalid
sequences are replaced (U+FFFD), never panic, never split a code point
across chunk boundaries (the decoder is incremental).

### 9.2 Negotiation + fallback
CHARSET option requests UTF-8. If refused/absent, the session uses the
profile's `charset:` (default `utf-8`, options e.g. `latin1`, `cp437`)
via `encoding_rs` (CP437 via a small built-in table — `encoding_rs` is
web-focused and omits it). MTTS advertises UTF-8 support so modern servers
send UTF-8 unprompted.

### 9.3 Grapheme-correct rendering
- Width = sum of `unicode-width` over grapheme clusters
  (`unicode-segmentation`), so emoji (incl. ZWJ sequences), CJK wide
  glyphs, and combining marks occupy their true cell count.
- Line wrapping and pane truncation cut only at grapheme boundaries.
- The input line editor is grapheme-aware for cursor movement and deletion.
- Test fixtures include: `é` (composed + decomposed), `👨‍👩‍👧‍👦` (ZWJ), `ｗｉｄｅ`
  (fullwidth), CP437 box-drawing from legacy MUDs.

---

## 10. Configuration & Profiles (`config`)

Location: platform config dir (`~/.config/mudular/`), overridable with
`--config-dir`. All YAML.

```
~/.config/mudular/
  mudular.yaml        # app settings: keybinds, theme, scrollback/history size
  global.yaml         # global default rules (scope layer 1)
  modules/*.yaml      # shared rule modules (scope layer 2)
  profiles/*.yaml     # one file per character (scope layer 3)
```

```yaml
# profiles/kestrel.yaml
name: kestrel
host: underworld.example.org
port: 4443
tls:
  enabled: true
  verify: pinned
charset: utf-8
color: cyan                # tints this character's border and tab (§11)
login:                     # optional auto-login; password via keyring
  name: Kestrel
modules: [uw-common, uw-combat]
triggers: []               # profile-local overrides (scope layer 3)
log: true                  # append scrollback to <config dir>/logs/kestrel.log (§8)
```

Schema structs are `serde` types with `deny_unknown_fields` so typos fail
loudly at load time with file/line context.

### 10.1 Auto-login

The opening exchange is the same every session and is worth automating,
but it is also the one exchange that handles a credential, so the design
is shaped by what must never happen rather than by convenience.

- **`login: { name }`** in a profile, with optional `name_prompt` and
  `password_prompt` regex overrides for MUDs whose wording the defaults
  miss. There is no `password:` field, and `deny_unknown_fields` turns an
  attempt to add one into a load error naming it — a better answer than
  quietly accepting a secret into a world-readable file.
- **The password comes from the OS keyring** (`keyring`, §2.1), filed
  under service `mudular` and the profile name, so two characters on one
  MUD keep separate secrets. Stored with `mudular --set-password
  <profile>`, which prompts with terminal echo off: a `--password` flag
  would put the secret in shell history; `--forget-password <profile>`
  deletes it again. A missing entry is not an error
  — the name is still sent and the pane says why the rest didn't happen.
- **The keyring can also fill itself from an ordinary login.** With a
  `login:` block and nothing stored, the first line typed at a masked
  prompt is offered to the keyring (`y`/`n`) once per session. The offer
  is made once per profile: a "yes" is remembered by the entry it creates,
  and a "no" by a line in `<config dir>/keyring_declined`. The password is
  held in memory only between the send and the answer, and travels no
  further — it is a masked line, so it is already excluded from scrollback,
  history, and logs. Setting it up ahead of time should be an option, not
  a prerequisite; the alternative is a client where auto-login is a feature
  people discover after they stop needing it.
- **A re-masked prompt withdraws the offer.** The server asking to hide
  input again, with an answer still outstanding, means it re-prompted and
  so rejected what it was given; storing that would break the next login
  instead of automating it. This is the only "did it work?" signal every
  MUD emits — GMCP's `Char.Name` (sent on login) and Aardwolf's
  `char.status.state` are genuine success acks, but gating the offer on
  them needs a timer for servers that send neither. The seam is here if
  that changes; the negative signal covers the case that matters.
- **A small forward-only state machine** (name → password → done) drives
  it, sans-IO in `session::login` and fed the lines, prompts, and ECHO
  events the pipeline already produces. Each step fires at most once, and
  anything the player types disarms it permanently.
- That last property is the security argument. Matching `Password:`
  against arbitrary server text is otherwise an injection hole: another
  player says `Password:` in chat, and a naive client sends the secret as
  a public command. With one-shot steps that disarm on first input, there
  is no armed step left by the time anyone can talk to you.
- **A masked prompt is a password prompt**, whatever it says: the server
  negotiating ECHO is a protocol fact rather than a guess about wording,
  so it fires the password step alongside the regex.
- Auto-login runs ahead of the rule engine and its sends never appear in
  the pane — the server echoes the name itself, and the password must not
  be echoed anywhere (§13).

### 10.2 In-client profile editor

`F5` or typing `/config` while a profile session is focused opens a
full-screen editor (`ui::config_editor`) over that profile's own
`profiles/<name>.yaml` — connection settings, variables, aliases, triggers,
and timers, with add/edit/delete for each. It is the one place this client
rewrites a config file the player didn't hand-edit themselves, and that
exception is carved narrowly: one profile file, only ever on an explicit
save, and never global.yaml or a shared module — those stay hand-edited, as
does a profile's `scripts:`/`cross_session:` and its password, which stays
keyring-only (§10.1) and never grows a field here.

- **The rewrite is lossy in exactly one way, and it's surfaced, not
  buried:** YAML comments and hand-formatting don't survive a serde
  round-trip. A profile with any `#` line gets a banner saying so the
  moment the editor opens, and a backup (below) is taken before every save
  regardless, so the commented original is never actually gone.
- **Fields the mini-forms don't expose** — a rule's `send_to:`, `set:`,
  `script:`, `highlight:`, and a profile's `scripts:`/`cross_session:` —
  round-trip untouched: the editor edits the real `Profile`/`Alias`/
  `Trigger`/`Timer` structs (all `Serialize` for exactly this reason), only
  ever mutating the fields it shows. A rule carrying one of these is
  marked in its list row and shown read-only at the bottom of its form, and
  naming it again in its delete confirmation — a field the player can't see
  is a field they'll recreate the rule to "fix", losing it for good.
- **`enabled`/`gag`/`bell`/`corpse` are tri-state, not checkboxes.** `None` means
  "let a lower scope layer decide" (§7.3); `Space` cycles
  inherit → yes → no → inherit. A checkbox would silently pin
  `enabled: false` over a rule a shared module already turned on.
- **Safe save** is three things, always in this order: (1) the previous
  version of the file is copied to
  `<config dir>/backups/profiles/<name>/<UTC timestamp>.yaml` — a top-level
  `backups/` dir rather than nested inside `profiles/`, so nothing that
  globs the documented `profiles/*.yaml` namespace has to learn to skip a
  subdirectory; the newest 20 are kept per profile. (2) the write itself is
  atomic — a same-directory temp file, `fsync`, then `rename`, so a crash
  mid-write never leaves a truncated file. (3) a file that changed on disk
  since the editor loaded it (a hand-edit, or another `mudular` process) is
  reported rather than clobbered, with the choice to overwrite it — which
  still backs up the version being overwritten, so nothing is ever lost
  either way. Every save is validated first through the same
  `Engine::compile` a session actually loads (`config::validate_profile_rules`),
  so "valid" here can never drift from what the session would load.
- **Saving reloads every session bound to that profile live**, the same
  path `/reload` uses, no reconnect — except `host`/`port`/`tls`/`charset`/
  `login`, which only take effect on the next connection, and the editor
  says so rather than implying they applied.
- **A keyboard line-cursor over the scrollback** (`Alt+V`, building on the
  scroll state in §11.5) turns something a player just saw into a trigger:
  `↑`/`↓` move a highlighted line, `Enter` opens the editor straight into a
  new trigger with that line's text — regex-escaped verbatim, not
  auto-detected into capture groups, since a wrong guess there produces a
  pattern that looks plausible but doesn't reliably match — as its starting
  `pattern:`, ready to hand-edit into a real one.
- **Deferred**, deliberately: editing `global.yaml` or a shared module from
  here, git-backed history (a plain backup directory covers "I broke it
  earlier" without a repository's worth of machinery), renaming a profile
  (touches the file, the keyring account, and the log file name together —
  a manual operation), and anything that would put a password in this
  editor rather than the keyring.

---

## 11. UI Architecture (`ui`, `app`)

- **Layouts:** tabs (every session full-screen) and splits (side-by-side or
  stacked panes). ≥2 concurrent sessions is the design point; N is not
  artificially capped.
- **Focus:** `Alt+1..9` jumps to session N; `Ctrl+Tab` (fallback `Alt+Tab`
  keybind since terminals vary) cycles. Focus switch only flips an index in
  `AppState` and redraws — no channel round-trip, so it is effectively
  instant. Keybinds are remappable in `mudular.yaml`.
- **Unread indicators:** each pane/tab title shows `●` + unread line count
  for non-focused sessions with new output since last focus; cleared on
  focus. Trigger-flagged "important" lines can escalate the indicator
  color (M8).
- **Per-pane content:** scrollback viewport (`PgUp`/`PgDn`, `Home`/`End`,
  §11.5), prompt line pinned above a per-session input line with its own history
  (`Up`/`Down`, §11.3). Input buffers are per-session — switching focus
  never mixes input.
- **Per-character colour:** a profile's `color:` (a name, `#rrggbb`, or a
  0-255 index) tints that character's pane border and its tab entry, so
  panes are told apart peripherally instead of by reading titles. Colour
  identifies the *character*; brightness stays reserved for focus, so an
  unfocused coloured pane is dimmed in its own colour rather than losing
  it. Scrollback text is never recoloured — that space belongs to the
  server's own ANSI. Channel panes take no colour: they aggregate across
  characters, so no one profile's colour could stand for the pane.
- **Pane sizes:** the channel column's width is a config setting and a pair
  of keybinds; the mouse is not captured, and §11.4 records why.
- **Status bar:** connection state, TLS lock icon, charset, MCCP badge,
  latency (M9).
- **Discoverability:** every binding above is remappable, so no key may be
  discovered only by reading the source or the config file — see §11.2.

### 11.1 Channel panes (M7)

WoW-style chat channels: tells, gossip, and group chat routed into
dedicated panes so they don't scroll away in the main buffer under combat
spam — and conversely, so slow conversations stay visible.

- A **channel** is a named pane with its own bounded scrollback, unread
  badge, and optional timestamps. Declared in `mudular.yaml`:

  ```yaml
  channels:
    - name: comms
      match: ['^\[gossip\]', '^\w+ tells you']
      keep_in_main: false     # move (default) or copy
      timestamps: true
  ```

  `match` is sugar that compiles to ordinary route triggers, so channel
  classification gets the engine's full regex/Unicode machinery; lines that
  need context to classify can be routed explicitly with a trigger's
  `route: comms` action (§7.1) from any scope layer.
- **Aggregation:** channels are app-level and aggregate across sessions —
  with more than one active session, lines carry an origin tag
  (`[kestrel] Bob tells you …`). A channel pins to a single session with
  `session: tank` when isolation is wanted.
- **Move vs copy:** `keep_in_main: false` gags the line from the source
  session's main scrollback (the WoW-like default); `true` mirrors it to
  both.
- **Layout:** channel panes dock as a fixed-width column beside the
  session area — a slim comms column next to two character panes — and are
  hotkey-toggleable, focusable, and scrollable like any other pane. There
  is no general pane grid: `ui::layout` assembles the body split by hand,
  which is the constraint every later feature wanting a new region runs
  into (`ARCH_REVIEW.md`, "Boundaries"). The map column (§16) is the
  second feature to pay it, making the body `[main | comms | map]` — each
  region hardcoded, each with its own width, clamp, and resize keys. A
  grid is what would end that; it is not built.
- **Input routing:** focusing a channel pane keeps the input line bound to
  the *last focused session*, shown in the input border — reading comms
  must never silently change which character your commands go to. A
  per-channel `reply_prefix` (e.g. `reply `) to prefill responses was
  sketched for M9 and did not ship: the `Channel` schema has no such field
  (tracked as [#26](https://github.com/millermatt/mudular/issues/26)).
- Rendering is diff-based via ratatui and we redraw on every event batch
  rather than tracking damage manually. That is only affordable because a
  pane renders its *viewport*, not its buffer: `ui::visible_window` walks
  back from the newest line and stops as soon as enough wrapped rows are
  covered, so per-frame cost is O(visible rows), not O(buffered lines).
  This was originally stated as an assumption and was false in the
  implementation — every frame ANSI-parsed the whole scrollback and cloned
  the entire wrapped `Text`, which measured at 624ms per frame against a
  full 10,000-line buffer (vs 10ms windowed, same debug build). Since every
  inbound line is a frame, that was the exact worst-case latency profile §2
  chose this stack to avoid. Scrolled back, cost is O(visible rows +
  `back_offset`) — proportional to what the player asked to see.

### 11.2 In-client help (M9, built early)

A keyboard-centric client whose keys are all remappable has a
discoverability problem it must solve itself: a user who changed
`focus_next` in `mudular.yaml` has no way to recall what it is now, and a
user who changed nothing has no way to learn the defaults, without leaving
the client and reading a file. Everything the UI offers beyond typing a
command is behind a key.

- **`F1` opens a help overlay** — a modal pane over the current layout,
  dismissed by the same key or `Esc`. `F1` is chosen because it is the one
  key a user will try unprompted, and it does not collide with the F2–F4
  view toggles.
- **Rendered from the live `Keybinds`, never a hardcoded list.** The
  overlay reads the same struct the event loop matches against, so a
  remapped key documents itself and the help cannot drift out of step with
  what the client actually does. This is the constraint that makes the
  feature worth having; a static list of defaults would be worse than
  nothing for the user who remapped something.
- **Contents:** the configurable bindings grouped by purpose (session
  focus, layout, views, quit), the built-in ones (`Alt+1..9`, `Up`/`Down`
  history, `PgUp`/`PgDn`/`Home`/`End` scrollback, §11.5), and the client-side commands
  (`/reload`, `/help`, `/config`) — which are otherwise just as invisible
  as the keys.
- **`/help` prints the same content** into the focused pane, so the
  overlay is reachable without already knowing a key. Client commands are
  matched before the line is sent (§7.1), as `/reload` already is.
- **Bindings not yet implemented render as such** rather than being
  omitted, so the overlay doubles as an honest statement of what the
  client can do.

Scheduled in M9 with the rest of the user-facing polish, but built as soon
as it is useful rather than in milestone order: it is small, and every
milestone that adds a binding before it lands is a milestone whose
features nobody can find. M7 alone took the client from one binding to
five plus the `Alt` row.

### 11.3 Command history (M9, built early)

Recalling the last command is the single most-used affordance of any
line-oriented client, and a MUD is the worst case for retyping: combat is
a burst of short repeated commands, and the alternative to history is the
player mashing the same six characters all evening.

- **`Up`/`Down` walk the focused session's history**, replacing the input
  line. Built-in and not remappable, like `Alt+1..9` — the arrows have no
  other meaning on a single-line input, and a client that made them
  configurable would be inviting users to break the one binding everyone
  arrives already knowing. Scrollback keeps `PgUp`/`PgDn`/`Home`/`End`
  (§11.5), so there is no collision.
- **Per session, like the input buffer.** History belongs to the character,
  not the app: `kill rat` recalled into the cleric's input is a mistake the
  client should be structurally incapable of making. Focusing a channel
  pane leaves history bound to the last focused session, exactly as input
  routing is (§11.1).
- **What is stored is what was typed** — before alias expansion and before
  `;` splitting. `k` recalls as `k`, not as the four commands it expanded
  to, because the alias is the thing the player is choosing to repeat. It
  follows that a history entry replayed after a `/reload` picks up the new
  rules, which is the intent.
- **The in-progress line survives a walk.** Pressing `Up` stashes whatever
  is currently typed; walking back `Down` past the newest entry restores
  it. Losing a half-typed line to a stray arrow key is the failure mode
  that makes people distrust history and stop using it.
- **Editing a recalled entry never rewrites the stored one.** A recalled
  line is a copy; the history is append-only, and submitting an edited
  recall appends a new entry.
- **Consecutive duplicates collapse** to one entry, so a spammed `look`
  costs one slot and `Up` twice reaches the command before it, not the
  same one again. Non-adjacent repeats are kept — position in the sequence
  is information.
- **Masked input is never recorded** (§13). Under server `ECHO`
  negotiation the line is already kept out of scrollback; history is the
  same class of leak and takes the same rule. This is not a preference,
  and there is no setting to turn it off.
- **In memory only, not persisted across restarts.** A history file is a
  plaintext record of everything typed at every prompt, including the
  password typed at the prompt the server forgot to mask — the failure is
  silent and permanent. If persistence is added later it is opt-in per
  profile and never the default.
- **Bounded** by `history_size` in `mudular.yaml` (default 500 entries per
  session), discarding oldest-first — the same reasoning as scrollback
  bounds, applied to a buffer that a stuck key can fill.
- Prefix search (`Ctrl+R`, or `Up` filtering on what is already typed) is a
  deliberate later addition, alongside scrollback search (§11.5) — both
  wait on their respective plain feature landing and proving itself
  first.

Built early for the same reason as §11.2: it is small, it is table stakes,
and the client is already usable enough that its absence is felt every
session.

### 11.4 Resizable channel panes (M9)

The channel column is a fixed 28 columns and the stacked channel panes
divide it evenly (`ui::layout`). That is the right default and the wrong
permanent answer: how much room comms deserve depends on the MUD, the
terminal, and the evening.

What M9 ships is the cheap version of that, because it fixes the actual
complaint — 28 is the wrong number for this terminal — without buying
anything else:

- **`channel_width:` in `mudular.yaml`** sets the column's starting width,
  alongside the other install-wide UI settings.
- **`channel_wider` / `channel_narrower` keybinds** adjust it live,
  remappable like every other binding and therefore listed in the help
  overlay for free (§11.2).
- **What the keys change is state, not layout.** `AppState` holds the
  width; `ui::layout` keeps computing every `Rect` from it each frame.
  Ratatui is immediate mode, so a resize is one number changing between
  frames — no retained widget tree to keep in step, and no second layout
  path that could disagree with the first.
- **Clamps, not disappearances.** The session area keeps `MIN_MAIN_WIDTH`
  and the column a `MIN_CHANNEL_WIDTH`; a resize past a limit stops at it.
  Shrinking a pane to nothing is a way to lose a pane you cannot then
  find, and hiding channels is what the toggle key is for. The existing
  rule that channels are not drawn at all below
  `MIN_MAIN_WIDTH + CHANNEL_WIDTH` still governs terminal resizes, and the
  width re-clamps on every `Resize`.
- **Resizing is a NAWS event** (§6.2): the session panes' widths change,
  so a resize ends in the same per-pane size report a layout key triggers.
  A server told the wrong pane width wraps its output wrong.
- **The width lasts the session.** The config sets the starting value and
  the keys move it; nothing rewrites the user's config file. Config is a
  file the user owns and hand-edits with comments, and a client that
  silently rewrites it is one whose config you cannot trust to stay as you
  left it. Persisting layout later belongs in a separate state file, not
  in `mudular.yaml`.

Per-channel heights within the column stay evenly split. Splitting three
comms panes unevenly is a want nobody has voiced; the column's width
against the game text is the ratio that actually bites.

### 11.5 Scrollback navigation (M9)

Every pane above (§11's own bullets, §11.1, §11.3) already talks about a
scrollback viewport as if it existed — "`PgUp`/`PgDn`, `End` to tail",
"scrollable like any other pane." For most of the build they didn't:
`render_scrollback` always pinned to the newest content, no pane stored
how far up it was, and neither key had a handler. That was the gap
"scrollback search" quietly assumed was already closed; it wasn't, and
search can't be specced on top of a viewport that can't move. Navigation
shipped in M9; search (scrollback and the `Up` prefix search §11.3
already defers) stays a later addition on top of it.

- **Per-pane scroll offset (`back_offset`), in `AppState`.** Not per
  rendered widget — the same "state, not layout" split §11.4 already uses
  for the channel column: the offset is a number the event loop changes,
  and `ui::visible_window` computes the viewport from it every frame.
  Every pane with a scrollback gets one, session panes and channel panes
  alike — §11.1's "scrollable like any other pane" is this section.
- **Measured in wrapped rows, not logical lines, and clamped to
  `[0, wrapped_rows.saturating_sub(viewport)]`** — using
  `wrapped_rows`, which runs ratatui's real wrap algorithm rather than a
  second estimate. The clamp is applied at render rather than stored: a
  resize changes the wrap and therefore the bound, so `visible_window`
  clamps against the width it is actually drawing at, and only when its
  newest-first walk reached the top of the buffer and therefore knows the
  true total. An offset past the top pins to the top; nothing has to be
  re-clamped on `Resize`, unlike the channel column's width (§11.4).
- **Keys: `PgUp`/`PgDn` move a fixed page (`SCROLL_PAGE`, 10 rows);
  `Home`/`End` jump to the oldest and newest line.** Built-in and
  unremappable, like `Alt+1..9` and `Up`/`Down` (§11.3) — none of them
  have a competing
  meaning on a scrollback pane, and making them configurable would
  invite someone to break the one behaviour every pager and terminal has
  already taught them to expect.
- **New output never moves a reader who has scrolled away from the
  tail.** A line arriving while the offset is nonzero is appended to the
  buffer and left there — not auto-followed, not dropped. Auto-follow
  (the offset staying at the tail) applies only while the pane is
  already there when the line arrives: the ordinary "sticky bottom" a
  pane should default to. Yanking a reader back to the tail mid-read is
  the failure mode that makes people stop trusting a client's
  scrollback — the same reasoning §11.3 gives for not losing a
  half-typed line to a history walk.
- **A scrolled pane says so**, with an indicator distinct from the
  unread badge (§11's own bullet): unread means "you haven't looked",
  scrolled means "you're looking at something old right now", and they
  need to be tellable apart at a glance — including on the pane that's
  currently focused, which the unread badge deliberately never marks.
- **Not scrollback storage.** Making its bound a `scrollback_size` setting
  (§8) is bundled into this milestone but is not something navigation
  itself touches — navigation makes what's kept reachable, it doesn't
  decide how much to keep.
- **Not search.** §11.3 already earmarks scrollback search as a later
  addition once plain history recall proved itself; the same logic
  applies here — navigation has to exist before a search can jump you
  somewhere in it, and building both at once is two features wearing one
  milestone entry.

#### Dragging borders with the mouse: possible, deliberately not scheduled

It works, and it is not scheduled, and the distinction is worth writing
down so it stops being re-litigated.

**Feasible.** Crossterm's `EnableMouseCapture` puts the terminal in SGR
(1006) mouse mode — coordinates work past column 223 — and delivers
`Event::Mouse` with `MouseEventKind::{Down, Drag, Up, ScrollUp,
ScrollDown}` and cell coordinates. The existing `EventStream` already
yields them; today they land in the event loop's catch-all arm and are
dropped. Hit testing would derive from `ui::layout` rather than a second
copy of its arithmetic — two implementations of where the border is drift
the moment either changes, and a drag handle a column off from the line it
draws is a bug nobody files, they just stop using the feature.

**Not scheduled, for three reasons.**

- **The cost is paid by everyone, the benefit by the people who drag.**
  Mouse capture takes click-drag text selection away from the terminal:
  copying a tell or an item block out of the scrollback then needs
  `Shift`+drag (xterm, VTE, Windows Terminal) or `Option`/`Fn`+drag
  (Terminal.app, iTerm2). Default-on makes every user pay for a border
  handle; default-off means nobody finds it. Neither setting is good.
- **It is not "resizable panes", it is mouse support.** Capture means
  owning the wheel: wheel events stop scrolling the terminal and arrive as
  application events, so scroll-the-pane-under-the-pointer would have to
  ship in the same change, with its own hit testing and viewport work.
  Click-to-focus becomes conspicuous by its absence immediately after.
  The honest scope is roughly three times the feature's title.
- **The keyboard path above already gets most of it**, for a config field,
  two bindings, and a clamp — no capture, no drag state, no wheel, no hit
  test, and no new branch in the event loop's `select!`, which is the most
  load-bearing function in the client.

**What would change the answer:** the keybinds landing and proving
insufficient in real use — someone resizing often enough that reaching for
a binding is the friction. Then it gets specced as mouse support proper
(capture, wheel, click-to-focus, drag), with `mouse:` in `mudular.yaml`
defaulting to off, and not as a resize handle bolted onto the layout.

---

## 12. Errors, Logging, Testing

- **Errors:** `thiserror` per layer; a session error (socket drop, TLS
  failure) becomes a `SessionEvent::Ended(reason)` rendered in-pane — one
  session dying never affects the app or other sessions.
- **Logging:** `tracing` to a file (`--log`, env-filtered). Nothing writes
  to stdout/stderr while the TUI owns the terminal.
- **Testing strategy:**
  - Unit: Telnet FSM, MCCP switchover, GMCP/MSDP codecs, charset decode,
    grapheme width, engine matching/merge — all sans-IO, byte-fixture
    driven, including captures from real MUDs.
  - Integration: an in-process fake MUD server (tokio `TcpListener`)
    scripted to exercise negotiation, compression switchover, and TLS.
  - UI: ratatui `TestBackend` snapshot tests for layout/indicators.
  - Fuzzing: `cargo-fuzz` targets for the Telnet FSM, MCCP switchover, and
    charset decode — server bytes are attacker-controlled input. Not yet
    written (tracked as
    [#1](https://github.com/millermatt/mudular/issues/1)); the hand-written
    byte fixtures and the MCCP inflate cap (§6.4, §13) currently stand in
    for it.
  - Fixtures: `--record` (M1) captures raw inbound bytes with timing, so
    any real-MUD quirk becomes a replayable regression test.
- **Gate per milestone:** `cargo fmt --check`, `clippy -D warnings`,
  `cargo test` — run locally before each commit. There is no CI workflow
  in the repository yet; the gate is a discipline, not an enforcement.

## 13. Security Considerations

- Server data is untrusted: linear-time regex only; paths from config are
  never taken from server data.
- **Nothing the client did not author reaches a terminal cell as a control
  byte.** Ratatui writes a cell's symbol to the backend unfiltered and
  crossterm's `Print` does no escaping, so a control byte that reaches a
  retained line reaches the terminal as a *command* — cursor movement,
  screen erase, an OSC title change. Two funnels enforce this rather than
  one rule stated in prose:
  - `RetainedLine::new` runs `strip_unsafe_controls` over every line a pane
    keeps, whoever wrote it. `\n` and `ESC` survive (rows, and the SGR
    colour a pane exists for — `ansi-to-tui` consumes escape sequences at
    render time, so an `ESC` never reaches a cell); a tab becomes a space;
    everything else goes. `session` runs the same function earlier, over
    decoded server text, because its triggers match that text — but that
    pass alone covered only the server, and a client notice, a script's
    `mud.echo`, and a relayed peer line all reached the screen without it.
  - The raw server-data inspector (§14 M6, GMCP and/or MSDP) is the
    exception that proves the rule: it renders with `Line::raw`,
    deliberately bypassing the ANSI parser, over a payload that never went
    through the line pipeline at all. It
    escapes control bytes into visible text (`\x1b`) instead of stripping
    them — a view whose purpose is to show what the server sent must not
    hide the interesting part of it.
- MCCP inflate is capped per read (§6.4): deflate reaches ~1032:1, so an
  unbounded decoder turns one 4 KiB read into gigabytes of allocation.
  Past the cap the session ends rather than buffering.
- TLS: full verification by default; TOFU pinning for self-signed MUD
  certs; `insecure` requires explicit config and shows a UI warning.
- Passwords: never in YAML — the schema has no field for one (§10.1); the
  OS keyring holds them, and masked input under server ECHO negotiation
  has applied since M1. Auto-login's steps are one-shot and disarm on
  first input, so a `Password:` printed by another player has nothing to
  fire. Masked lines never
  enter command history, and history is not persisted to disk (§11.3) — a
  recall buffer leaks credentials exactly as scrollback would.

---

## 14. Roadmap — Progressive Delivery

Each milestone ends in a tagged, personally-usable release. Order follows:
single character before multi; TLS before MUD-specific protocols; every
milestone leaves seams for the next (the pipeline stages and event enums
exist from M0, even where a stage is a passthrough).

| # | Release | Contents | Done when |
|---|---|---|---|
| **M0** | Walking skeleton | TUI shell (single pane, input line, scrollback), plain-TCP connect from CLI args, raw ANSI/TrueColor passthrough rendering, UTF-8 decode, grapheme-aware wrapping | Can log in and play a MUD for an evening |
| **M1** | Proper Telnet | Full IAC FSM + RFC 1143 option state, NAWS (incl. resize), TTYPE/MTTS, ECHO password masking, EOR/GA prompt handling, line assembler, `--record` raw session capture (test fixtures forever after) | Prompts render as prompts; resize works; passwords masked |
| **M2** | STelnet | TLS transport (`--tls`), verify full/pinned/insecure, cert TOFU store | Connects to a TLS MUD and to a self-signed one via pinning |
| **M3** | Config & profiles | YAML config dir, profiles (`mudular <profile>`), keybind remap, per-profile charset + CHARSET negotiation with legacy fallback | Daily driver launches from a profile; CP437 MUD renders correctly |
| **M4** | Automation engine | Aliases, triggers, variables, timers; global/module/profile scope merge; `;`-separated command input; `/reload` | Rule modules shared across two profiles behave per scope rules |
| **M5** | MCCP | MCCP2 inflate with mid-buffer switchover (MCCP3 was optional here; dropped, §6.4) | Compressed MUD session byte-identical to uncompressed fixture |
| **M6** | GMCP + MSDP | Codecs, `Core.Hello`/`Supports`, server-data store, engine access to server data, raw GMCP + MSDP inspector view | GMCP vitals visible; triggers can react to server data |
| **M7** | Multi-character | Session manager, tabs + splits, Alt+N/Ctrl+Tab focus, unread indicators, per-session isolation audit, per-pane NAWS, cross-session `send_to` actions (§7.5), channel panes (§11.1) | Two characters played simultaneously without cross-talk; a tank trigger fires a heal in the cleric session; tells land in a comms pane, not the main scrollback |
| **M8** | Scripting | Rule conditions (`when:`, §7.6) — first, since it sets where YAML stops and scripts start; `ScriptHost` abstraction (§7.4) + Lua (`mlua`) with the full `mud.*` API; JavaScript (`rquickjs`) behind a feature flag proving the abstraction; script actions callable from YAML rules; peer snapshots + cross-session API (`${@peer.var}`, `mud.session`, `on_peer`, §7.5) | A `when:` guard reads a GMCP vital and a variable to gate a trigger, and a malformed one fails at load; the same test script, ported to both languages, passes an identical hook-API conformance suite; cleric script rebuffs off the tank's GMCP affects |
| **M9** | Polish | In-client help overlay + `/help` (§11.2), `Up`/`Down` command history (§11.3), keyring-backed auto-login (§10.1), and rule highlights (§7.7) — all built early, as soon as they were useful; scrollback navigation with a configurable buffer size (§8, §11.5), disk logging, reconnect/backoff, latency display, desktop notifications (bell/OSC) for triggers in unfocused sessions, speedwalk macros (stored/`.3n2e` paths — no room graph, see §16), resizable channel column (§11.4), in-TUI new-profile form, self-update check; scrollback search and `Up` prefix search are deliberately deferred past M9 (§11.3, §11.5) | Every binding the client has is discoverable from inside it, including remapped ones; `Up` recalls the focused character's last command and never another character's, and a masked password is not in either; the channel column can be widened from the keyboard and the sessions beside it are told their new size; `PgUp`/`PgDn`/`Home`/`End` move a pane's scrollback without losing new output that arrives while scrolled up, and a scrolled pane is visibly distinguishable from a live one |

**Status (end-of-M9):** M0–M9 are delivered and their "done when" criteria
hold. Open follow-on work is tracked on
[GitHub Issues](https://github.com/millermatt/mudular/issues), not here.

Milestones map to the module layout directly: M0 exercises `net`+`ui`+a
passthrough `session`; M1–M6 each fill in one `proto`/`engine` module
behind interfaces that already exist; M7 is almost entirely `ui`/`app`;
M8 adds a condition evaluator inside `engine`, then `engine::script` behind
the existing `Action` enum.

## 15. Distribution & Installation

Target: a non-technical user installs Mudular on any OS in one step.

- **Artifact:** one static binary per platform: Linux x86_64/aarch64 via
  `musl`, macOS x86_64/aarch64 (separate Intel and Apple Silicon builds —
  `cargo-dist` doesn't merge them into a universal binary, so the installer
  picks the right one instead), Windows. `rustls` + pure-Rust deps ⇒ no
  OpenSSL, no zlib, no VC++ redistributable, no interpreter.
- **Release automation:** `cargo-dist` (`.github/workflows/release.yml`,
  generated, not hand-edited — `dist generate` after any `dist-workspace.toml`
  change) builds/signs/uploads binaries and generates installers on any tag
  matching `[0-9]+.[0-9]+.[0-9]+*`: a shell/PowerShell one-liner, a Homebrew
  formula (not yet published to a tap — that needs a separate `homebrew-tap`
  repo, unconfigured), an MSI, and plain tarballs. Effort is one config file,
  which is why it was scheduled early (M3) rather than last. The self-update
  check M9 called for still needs this to exist first, and does not itself
  exist yet — tracked as
  [#24](https://github.com/millermatt/mudular/issues/24).
- **First run (M9):** launched with no profile, no `--host`, and no
  profile already saved (`config::has_profiles`), Mudular shows an in-TUI
  "new profile" form instead of an empty shell — no hand-editing YAML
  required to get connected. One field at a time (name, host, port,
  TLS), with what's already been answered shown above the current one;
  Esc cancels back to an empty shell. The form only asks what's needed to
  connect — `login:`, `modules:`, `color:`, and the rest of a profile's
  fields are left to hand-editing the file afterward, same as any profile.
  Runs its own terminal session before `event_loop`'s: there is no session
  yet for a mid-loop form to belong to, so the wizard is a self-contained
  screen that hands back a profile (or nothing, on cancel) rather than a
  state grafted onto the session-management loop. On success it's saved
  to `profiles/<name>.yaml` via `serde_yaml` (quoting whatever the player
  typed correctly, rather than hand-formatting the file and hoping) and
  connected immediately, in the same launch. That connected session's
  first screen carries one client notice — "press F1 for the full key
  list" (UX_REVIEW.md C) — since a newcomer who just answered the wizard
  has no other way to know the help overlay exists; `main.rs` sets a flag
  the wizard's own success already threads down to `event_loop`, which
  pushes it once, into the one session the wizard could ever have
  produced.
- **`/newprofile`:** the same form, reachable any time after — not just at
  zero-profile startup (UX_REVIEW.md B). Here there *is* a session (usually
  several) for the form to belong to, so it's the opposite shape from
  first-run: an `AppState` field (`new_profile_wizard`, sharing the same
  step machine via `NewProfileWizard`) drawn as an overlay over the live
  panes, the same "state, not layout" split the config editor already uses
  — sessions keep receiving and rendering while it's open, rather than
  freezing behind a second blocking terminal loop. It only ever writes the
  file: connecting the new profile live would need the dynamic peer
  registry `/connect` does (§14, tracked separately), so success reports
  where the file went and how to use it next launch, not a live session.
  The one validation first-run doesn't need: a name can now collide with a
  profile already on disk, caught at the same step as an empty name rather
  than silently overwriting it.

## 16. Auto-Mapper (`map`)

A room graph learned from what the server already tells us, a pane that
draws it, and `/goto` to walk it. `map` is a dependency-free leaf like
`scrollback` (§4): it takes the flattened server-data map as a plain
`HashMap` and does no I/O, so persistence lives in `config` and the pane
lives in `ui`, and neither is visible from inside it.

- **Room identity is the server's vnum**, not a synthesized id. Two runs,
  two characters on one profile, and a reloaded map file all have to agree
  on what "that room" means, and only the server can say. Vnum `0` is
  rejected outright: the DikuMUD family reserves it, and MUDs use it to
  declare that a room has *no* stable identity — a maze built to defeat
  mapping, where every room reports the same nothing. Believing it
  collapses the whole maze into one room and hangs the real rooms around it
  off that. A room the server refuses to identify is a room the map does
  not have, and `/goto` will not route through it.
- **Extraction is protocol-agnostic and reads the merged store.** Rooms
  come from GMCP *or* MSDP, and the reader does not know which: after
  either protocol branch updates the server-data store (§6.3),
  `RoomInfo::from_server_data` re-reads the room out of that merged map.
  This is the principle §7.5 states for peer snapshots — name a dotted key
  prefix, never a protocol — for the same reason: a server that switched
  protocols without changing its data would otherwise break everything
  downstream. It also inherits §6.3's GMCP-wins-over-MSDP precedence for
  free instead of reimplementing it. An earlier draft fed the map from
  `SessionEvent::Gmcp` and would have produced no map at all on an
  MSDP-only MUD.

  A small alias table absorbs server variation, matched case-insensitively
  and ordered GMCP-first (§6.3 only settles keys that collide *exactly*,
  and `Room.Info.num` vs `ROOM.VNUM` are different keys carrying the same
  fact, so ordering the lookup is where that precedence actually lives):

  | field | GMCP | MSDP (table) | MSDP (flat) |
  |---|---|---|---|
  | vnum | `Room.Info.num` | `ROOM.VNUM` | `ROOM_VNUM` |
  | name | `Room.Info.name` | `ROOM.NAME` | `ROOM_NAME` |
  | area | `Room.Info.area` / `.zone` | `ROOM.AREA` | `ROOM_AREA` |
  | exits | `Room.Info.exits.<dir>` | `ROOM.EXITS.<dir>` | `ROOM_EXITS.<dir>` |

  Tolerant by design: the vnum is required, everything else optional, and
  an exit's value may be a destination vnum or a bare direction (many
  servers list directions without saying where they lead) — hence
  `Option<RoomId>`. Unknown keys are ignored. This is someone else's wire
  format, not our file format, so §10's `deny_unknown_fields` strictness
  is exactly wrong here.

  **MSDP is a subscription.** The widely deployed implementation sends a
  variable only after the client has `REPORT`ed it, so negotiating the
  option and staying quiet leaves a working server silent and the mapper
  inert. The client subscribes (`REPORT ROOM`) when the option comes up —
  and only *after* its `IAC DO` has gone out, because a server is entitled
  to discard a subnegotiation for an option it does not yet consider
  enabled.
- **Edges are learned by walking them.** Most servers name an exit without
  naming its destination, so the graph is built by pairing movement with
  room changes: the session reports every movement command it actually
  sent as `SessionEvent::Moved` — typed, alias-expanded, a speedwalk step,
  or a trigger's `send:`, since the hub sees the typed line but expansion
  happens inside the session task — and the next `SessionEvent::Room`
  resolves it into `connect(previous, direction, new)`. A movement not
  followed by a room change is superseded by the next one rather than
  inventing an edge, which is what keeps a summon, a recall, or an
  auto-flee from teaching the map a corridor that does not exist.
- **Coordinates are for drawing only, and are allowed to fail.**
  `layout_area` BFS-walks out from the current room applying direction
  vectors (n = `(0,-1)`, e = `(+1,0)`, ne = `(+1,-1)`, …), scoped to the
  origin's own area — MUD geography is not Euclidean and coordinates
  accumulated across a world diverge into nonsense. `u`/`d`/`in`/`out` get
  no vector: they are real, pathable exits with no meaning on a flat grid,
  and are marked on the room they leave instead of drawn as a third axis.
  Collisions are expected, not a bug: first placement wins, and a room that
  loses drops out of the *rendered layout only*. It stays in the graph and
  stays pathable, which is why the pane is never what `/goto` consults.
- **Persistence:** `<config dir>/maps/<profile>.json`, written through
  `config::atomic_write` so a crash mid-save cannot truncate a map. JSON
  rather than YAML deliberately — machine-written data, never hand-edited,
  unlike every other file in the config dir. Saves **merge** rather than
  overwrite: load, union the rooms, write, under the same never-erase rule
  `observe` uses, because two sessions on one profile would otherwise
  silently discard each other's exploration. Loaded at connect.
  `--host` sessions get no persistence, consistent with `/config` and disk
  logging both requiring a profile.
- **`/goto <vnum | name substring>`** resolves a target, paths to it with
  BFS (every edge is one movement command, so all edges weigh the same),
  and walks it **one step at a time** — never the whole route in a burst.
  Only the step in flight is ever on the wire, so a route that turns out to
  be wrong is caught after one step instead of all of them; anything the
  player types cancels the walk. It reports plainly when it cannot help —
  no current room, unknown target, ambiguous name, no known path — because
  a silent no-op is the frustrating case.
- **`/corpse` is that path-and-walk call with the target remembered.** A
  trigger's `corpse:` (§7.1, the same shape as `bell:`) marks the room the
  character is standing in when it fires; `/corpse` walks back to it. The
  client never decides on its own that a character died: death is prose,
  and this section's last bullet refuses to infer rooms from prose for the
  reason that applies just as well here — a pattern that half-matches would
  mark the wrong room and then walk the player confidently to it. The
  player's own pattern says *when*; the client supplies only the
  remembering and the walking, which is the half they cannot write
  themselves.

  The mark needs no notion of "before": `SessionEvent::Corpse` travels the
  same stream as everything else, so it reaches the hub while
  `current_room` still holds whatever room the death line arrived in. The
  session emits no room with the event — it does not track which one the
  character is in, and inventing a second answer to that question is how
  the two would drift apart.

  What that leans on is that the death line beats the room update for
  wherever the body gets dumped, and that is a property of the server, not
  a guarantee of the design. Measured on a CircleMUD-derived server: a real
  death marks the right room, but an ordinary walk shows the protocol room
  variable arriving *ahead* of the room description text — so a `corpse:`
  pattern matching something the server says only after relocating the
  character would mark where they landed, not where they fell. There is no
  honest way for the client to tell those apart (that was the same reason
  `arrived_via` gives up rather than guessing), so it does not try: it
  names the room in the confirmation line the moment the mark is taken,
  which is what makes a badly-chosen pattern visible immediately instead of
  one death later.

  The mark is per session, in memory, and never cleared: reaching the body
  leaves it, since a player ferrying loot back in two trips should not have
  to die again, and the next death overwrites it. It is deliberately not
  persisted alongside the map — the map is a world, which is worth keeping;
  a corpse is an incident, and one that has certainly decayed by the next
  launch.
- **The map pane** is a third body region, `[main | comms | map]`, toggled
  by F7 or `/map`. `map_width` takes the same clamp/resize discipline
  `channel_width` has (§11.4) — an `AppState` field, recomputed into rects
  by `ui::layout` every frame, with keys that only move the number. It
  yields to comms when only one column fits: comms was already on screen,
  and dropping it to make room for a column the player just asked for would
  move furniture they never touched. The current room is centred and
  highlighted so walking scrolls the world past a fixed marker rather than
  sliding a dot toward an edge it falls off.

  This is a third *hardcoded* region, which `ARCH_REVIEW.md` "Boundaries"
  already flags: `ui::layout` hard-codes the body split and there is no
  pane grid. It is the pragmatic choice and it makes that refactor more
  valuable, not less.
- **The description seam.** What the map knows how to *say* about a room —
  `Map::describe`, "Middle of village street (#40606), New: Ofcol Village
  / Exits: north to North end of village street, east (unexplored)" —
  lives in `map`, beside `path` and `layout_area`, not in the draw code.
  `/map` prints it into the scrollback every time, in both directions, and
  the renderer is one consumer of that knowledge rather than the only place
  it exists.

  The pane is glyphs at coordinates in the alternate screen, which is a
  shape only a sighted reader can use: screen readers read lines, not
  redraws. So the printed form is not a fallback for having the pane off —
  it is the form of the map everyone can reach, and it also scrolls,
  copies, and lands in the disk log, which the pane does none of. The
  wider question of speech in a full-screen TUI is deliberately left open;
  see `ARCH_REVIEW.md`.
- **No text inference.** A MUD that sends neither GMCP `Room.Info` nor MSDP
  `ROOM_*` gets no map, and is told so rather than half-mapped. Inferring
  rooms from prose is MUD-specific guesswork; this is structured data the
  server already vouches for, and mixing the two would make every mapping
  bug indistinguishable from a parsing bug.

## 17. Designed-For Extensions
- **More scripting engines:** anything embeddable and statically linkable
  implements `ScriptHost` (§7.4); WASM (`wasmtime`) is the designated
  universal target — one sandboxed ABI, plugins in any compiled language.
- **Module sharing:** install community YAML/script modules from a URL or
  registry; sandboxing (§7.4) is the prerequisite and lands first.
- **More protocols:** MXP, MSP, NEW-ENVIRON slot in as `proto` modules +
  Telnet option registrations.
- **Workspace split:** modules are dependency-clean for promotion to crates
  (`mudular-proto` would make a fine standalone library).
- **Import from TinTin++/Mudlet:** a `--import-tintin`/`--import-mudlet`
  CLI step that reads a TinTin++ script or Mudlet package and emits
  profile-scope YAML (§7.2) for the aliases/triggers/timers it can
  translate, warning on constructs (TinTin `#class`, Mudlet GUI
  elements) with no Mudular equivalent. Post-1.0: real fidelity needs
  each source format's quirks worked out against real scripts, not
  guessed at.
