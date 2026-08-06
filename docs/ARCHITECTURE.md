# Mudular — Technical Architecture Document

**Status:** Draft v1 · **Scope:** Full architecture + incremental delivery roadmap

Mudular is a modern, terminal-based MUD client: a lightweight, high-performance,
keyboard-centric alternative to desktop clients like Mudlet, supporting multiple
concurrent character sessions in a single TUI.

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
| Secrets | `keyring` | OS keychain for passwords (M8) |

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
  ui/            ratatui widgets: pane grid, status bar, input line, tabs
  session/       Session task: pipeline assembly, SessionEvent/Command types
  net/           Transport: TCP / TLS connect, unified AsyncRead+Write
  proto/
    telnet.rs    sans-IO Telnet state machine + option negotiation
    mccp.rs      MCCP2/3 inflate stage
    gmcp.rs      GMCP subnegotiation codec (package + JSON payload)
    msdp.rs      MSDP subnegotiation codec (typed VAR/VAL/ARRAY/TABLE)
  engine/        Triggers, aliases, variables; YAML module loading; scoping
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
  `Core.Supports.Set` after negotiation. Events surface to the engine
  (triggers can match on GMCP packages, M7) and to UI consumers (vitals,
  room info) via `SessionEvent::Gmcp`.
- **MSDP (option 69):** typed parser for `VAR`/`VAL`/`TABLE_OPEN/CLOSE`/
  `ARRAY_OPEN/CLOSE` into a recursive `MsdpValue` enum. Where a server
  offers both, GMCP is preferred; MSDP values are normalized into the same
  internal "server data" map so the engine sees one namespace.

### 6.4 Stream control: MCCP2/MCCP3

- **MCCP2 (86):** after the server's `IAC SB 86 IAC SE`, *all subsequent
  inbound bytes* are one zlib stream. The inflate stage therefore sits
  **in front of** the Telnet machine and is toggled by a marker event the
  Telnet machine emits when it sees the MCCP2 subneg. Pipeline handles the
  mid-buffer switchover (bytes after the SE in the same read are compressed).
- **MCCP3 (87):** same, mirrored for client→server writes (compress after we
  send our subneg). Low priority (M6, optional) — outbound volume is tiny.
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
  Actions: send commands, set variables, highlight/gag/substitute the
  line, echo a local note, play the terminal bell, call a script (§7.4),
  send commands to *another* session (§7.5), route the line to a channel
  pane (§11.1).
- **Variable:** string values in a per-session store; substituted into
  send/echo actions as `${name}`; captures bind `${1}`, `${name}` from
  named groups. All matching uses the `regex` crate: Unicode-aware classes,
  linear-time guarantee (a hostile server line can't freeze the client).
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
    when: '${hp} < ${heal_at}'      # numeric guard, later milestone
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
- **Engines, feature-gated** so the binary carries only what's wanted:
  - **Lua** first, via `mlua` (vendored Lua 5.4, statically linked) — the
    MUD community's lingua franca; eases migration from Mudlet.
  - **JavaScript** second, via `rquickjs` (QuickJS, statically linked) —
    shipping a second engine early proves the abstraction is real.
  - Others (e.g. `rhai`) slot in behind the same trait. Engines requiring a
    system runtime (Python/pyo3) are excluded: they break the single-binary
    distribution goal (§15).
- **Wiring:** YAML rules reference scripts
  (`script: {file: combat.lua, fn: on_death}`); scripts can also register
  triggers/aliases/timers programmatically. Script files live next to the
  YAML module that declares them and follow the same scope layering (§7.3).
- **Execution model:** hooks run synchronously on the owning session's task
  and are expected to return quickly; a time budget logs/aborts runaway
  scripts so one session's script can't stall its pipeline (and never
  another session's).
- **Sandboxed by default:** no filesystem, network, or process access
  unless the profile grants it explicitly — shared community modules may
  carry scripts, and untrusted-by-default is the safe posture.

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
    when: '${hp} < 40'
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
YAML rules reference peer state as `${@tank.hp}`; a `when:` guard can
combine local and peer values.

**Scripting API (M8).** The `mud.*` API (§7.4) adds:

- `mud.session("cleric"):send(cmd)` / `:echo(text)` — routed as above.
- `mud.session("tank").data` / `.vars` — the peer snapshot (read-only).
- `mud.on_peer("tank", "gmcp:Char.Affects", fn)` — subscribe to a peer's
  server-data updates; this is the clean solution for "rebuff when a buff
  wears off": the cleric's script watches the tank's affect list and reacts
  from the cleric's own session, where the response commands belong.

Both directions are therefore supported and equivalent: push (tank's rules
command the cleric) and observe (cleric's rules/scripts watch the tank).
Prefer observe for reactions that are really the cleric's job — the rule
lives with the character that acts, and it keeps working no matter which
session detects the condition first.

---

## 8. Line Assembly & Scrollback

- The assembler consumes decoded text + prompt boundaries and produces
  `Line` values: styled spans (parsed SGR state carried across chunks) +
  plain-text projection (for triggers/search) + kind (`Output | Prompt |
  Local | Gagged`).
- ANSI parsing uses `ansi-to-tui` to turn SGR (16/256/TrueColor) into
  ratatui spans; unknown/unsafe escape sequences are dropped, not rendered
  raw. If MUD-specific quirks outgrow it, the fallback is a thin parser on
  `vte` (Alacritty's escape parser).
- Scrollback is a bounded ring buffer (`VecDeque<Line>`, default 10k lines,
  configurable) per session. Optional disk logging (M9) writes plain text
  or raw-with-ANSI per profile.

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
  mudular.yaml        # app settings: keybinds, theme, scrollback size
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
login:                     # optional auto-login (password via keyring, M9)
  name: Kestrel
modules: [uw-common, uw-combat]
triggers: []               # profile-local overrides (scope layer 3)
```

Schema structs are `serde` types with `deny_unknown_fields` so typos fail
loudly at load time with file/line context.

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
- **Per-pane content:** scrollback viewport (PgUp/PgDn, `End` to tail),
  prompt line pinned above a per-session input line with its own history.
  Input buffers are per-session — switching focus never mixes input.
- **Status bar:** connection state, TLS lock icon, charset, MCCP badge,
  latency (M9).

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
- **Layout:** channel panes dock into the same pane grid as session panes
  (e.g. a slim comms column beside two character panes) and are
  hotkey-toggleable, focusable, and scrollable like any other pane.
- **Input routing:** focusing a channel pane keeps the input line bound to
  the *last focused session*, shown in the input border — reading comms
  must never silently change which character your commands go to. A
  per-channel `reply_prefix` (e.g. `reply `) can prefill responses (M9).
- Rendering is diff-based via ratatui; a full redraw of a 4-pane 200×60
  terminal is well under a millisecond, so we redraw on every event batch
  rather than tracking damage manually.

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
    charset decode — server bytes are attacker-controlled input.
  - Fixtures: `--record` (M1) captures raw inbound bytes with timing, so
    any real-MUD quirk becomes a replayable regression test.
- **CI gate per milestone:** `cargo fmt --check`, `clippy -D warnings`,
  `cargo test`.

## 13. Security Considerations

- Server data is untrusted: linear-time regex only; ANSI parser drops
  unknown escape sequences (terminal escape injection); paths from
  config are never taken from server data.
- MCCP inflate is capped per read (§6.4): deflate reaches ~1032:1, so an
  unbounded decoder turns one 4 KiB read into gigabytes of allocation.
  Past the cap the session ends rather than buffering.
- TLS: full verification by default; TOFU pinning for self-signed MUD
  certs; `insecure` requires explicit config and shows a UI warning.
- Passwords: not stored in YAML; OS keyring integration planned (M9),
  masked input under server ECHO negotiation from M1.

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
| **M5** | MCCP | MCCP2 inflate with mid-buffer switchover (MCCP3 optional) | Compressed MUD session byte-identical to uncompressed fixture |
| **M6** | GMCP + MSDP | Codecs, `Core.Hello`/`Supports`, server-data store, engine access to server data, raw GMCP inspector view | GMCP vitals visible; triggers can react to server data |
| **M7** | Multi-character | Session manager, tabs + splits, Alt+N/Ctrl+Tab focus, unread indicators, per-session isolation audit, per-pane NAWS, cross-session `send_to` actions (§7.5), channel panes (§11.1) | Two characters played simultaneously without cross-talk; a tank trigger fires a heal in the cleric session; tells land in a comms pane, not the main scrollback |
| **M8** | Scripting | `ScriptHost` abstraction (§7.4) + Lua (`mlua`) with the full `mud.*` API; JavaScript (`rquickjs`) behind a feature flag proving the abstraction; script actions callable from YAML rules; peer snapshots + cross-session API (`${@peer.var}`, `mud.session`, `on_peer`, §7.5) | The same test script, ported to both languages, passes an identical hook-API conformance suite; cleric script rebuffs off the tank's GMCP affects |
| **M9** | Polish | Scrollback search, disk logging, reconnect/backoff, keyring passwords + auto-login, latency display, desktop notifications (bell/OSC) for triggers in unfocused sessions, speedwalk macros (stored/`.3n2e` paths — no room graph, see §16), in-TUI new-profile form, self-update check | — |

Milestones map to the module layout directly: M0 exercises `net`+`ui`+a
passthrough `session`; M1–M6 each fill in one `proto`/`engine` module
behind interfaces that already exist; M7 is almost entirely `ui`/`app`;
M8 adds `engine::script` behind the existing `Action` enum.

## 15. Distribution & Installation

Target: a non-technical user installs Mudular on any OS in one step.

- **Artifact:** one static binary per platform (Linux x86_64/aarch64 via
  `musl`, macOS universal, Windows). `rustls` + pure-Rust deps ⇒ no OpenSSL,
  no zlib, no VC++ redistributable, no interpreter.
- **Release automation:** `cargo-dist` in CI builds/signs/uploads binaries
  and generates installers per tag: shell one-liner + Homebrew tap
  (macOS/Linux), MSI + `winget`/Scoop (Windows), plus plain tarballs.
  Effort is one config file, so this lands early (M3, first "someone else
  could install this" milestone) rather than last.
- **First run:** if no config exists, Mudular creates the config dir and a
  commented sample profile, and offers an in-TUI "new profile" form —
  no hand-editing YAML required to get connected (form lands M9).

## 16. Designed-For Extensions
- **More scripting engines:** anything embeddable and statically linkable
  implements `ScriptHost` (§7.4); WASM (`wasmtime`) is the designated
  universal target — one sandboxed ABI, plugins in any compiled language.
- **Auto-mapper:** a map pane driven by GMCP `Room.*` data (falling back to
  movement/exit-line inference), with pathfinding speedwalk-to-room that
  supersedes M9's macro speedwalks. Deliberately post-1.0: it is a large
  feature and the GMCP plumbing it needs is M6.
- **Module sharing:** install community YAML/script modules from a URL or
  registry; sandboxing (§7.4) is the prerequisite and lands first.
- **More protocols:** MXP, MSP, NEW-ENVIRON slot in as `proto` modules +
  Telnet option registrations.
- **Workspace split:** modules are dependency-clean for promotion to crates
  (`mudular-proto` would make a fine standalone library).
