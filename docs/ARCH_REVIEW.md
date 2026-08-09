# Architecture Review — 2026-08-08

An adversarial structural review taken at end-of-M9, with `ARCHITECTURE.md`
§16, `IDEAS.md`, and `UX_REVIEW.md` all treated as the load the current
design has to carry. Not a correctness or security review — those were done
separately. Subject here is structure: module boundaries, coupling, the
state/concurrency model, and what the architecture forecloses.

A snapshot, not a backlog. Findings were spot-checked against the source;
where a claim was corrected on verification, that's noted inline.

## The headline: the latency thesis is contradicted by its own render path

> **Fixed.** `ui::visible_window` now renders the viewport rather than the
> buffer. Measured against a full 10,000-line scrollback, same debug build:
> **624ms → 10ms per frame (~61×)**. §11.1 has been corrected to state the
> windowing as a mechanism rather than an assumption. The analysis below is
> kept as the record of what was wrong and why.

§2 chose Rust for flat worst-case latency, and §11.1 asserts that
"rendering cost scales with the viewport." It does not.

`draw_session` (`src/ui/mod.rs:331`) iterates the **entire** scrollback
deque — up to `scrollback_size`, default 10,000 and user-configurable per
§8 — calling `ansi_lines()` on every line, which is a full ANSI parse
allocating owned spans. It then passes the result to `render_scrollback`
(`src/ui/mod.rs:450`), which does `text.clone()` of the whole `Text` and
runs ratatui's real wrap algorithm across all of it via `line_count` to
compute `wrapped_rows`.

Per pane, per frame — and every inbound line is a frame.

Per-frame cost is therefore O(total buffered lines × panes), not
O(viewport). Under the design's own target load — a multi-boxer with 3–4
panes on a spammy MUD and full buffers — that is tens of thousands of
string allocations and a full re-parse per arriving line: precisely the
worst-case latency profile §2 selected the stack to avoid. Making
`scrollback_size` configurable is an unguarded amplifier; a user setting
100,000 multiplies the per-frame cost tenfold with no warning.

Fixing it means computing the viewport from a windowed slice, which needs
either per-line cached wrapped-row counts (invalidated on resize) or
pre-parsed retained lines — which entangles it with the next finding.
**Resolve the two together, before 1.0.**

## The retained-line problem

> **Fixed.** `src/scrollback.rs` now holds `RetainedLine { text, at,
> origin }`, and both panes store it. `push_line` takes one, so every call
> site states who wrote the line — `Server`, `Client`, `Rule`, `Echo`, or
> `Session(name)` — rather than leaving it to whether someone remembered to
> type `**`. `SessionEvent::Line` carries the origin as well, since a
> trigger's echo, an auto-login notice and an injected command all reach a
> pane by the same event as server text; tagging only at the hub would have
> recorded all three as things the MUD said. The channel timestamp and
> `[character]` tag are no longer spliced into the text: `ui::draw_channel`
> composes them from `at` and `origin` at render time, so a routed line
> stores what the MUD sent. Screen output is unchanged.
>
> §8 now specifies the type. What it unblocks is unblocked, not built —
> `/errors` (finding D) and search are still their own work. Two of this
> finding's three named gaps remain open, and neither is closed by the type
> alone: **which rule fired** needs the engine to report it through
> `LineOutcome` (`Origin::Rule` is the seam, and today carries no id), and
> **per-line identity** for the screen-reader diff would be a sequence
> number this does not yet assign. Search also still re-strips ANSI per
> keystroke — a cached plain projection is a field behind the same funnel,
> deliberately not added ahead of the feature that needs it.

Scrollback is `VecDeque<String>` (`SessionPane.scrollback`,
`ChannelPane.lines`). By the time a line arrives there it has been
flattened: highlights spliced into the ANSI (§7.7), channel timestamps and
origin tags prepended *as text*. Nothing retains arrival time, origin
(server vs. client notice), which rule fired, or whether it was a tell.

That one representation blocks, from `IDEAS.md`: the "while you were away"
digest, tells-as-conversations, session stats, personal bests, death
replay, bookmark-a-moment, and RP transcript export. It blocks
`UX_REVIEW.md` finding D — client-generated warnings are indistinguishable
from server text because they are the same `String` in the same deque. It
blocks screen-reader "announce only new lines," which needs per-line
identity to diff against. And §11.5's deferred scrollback search inherits
it too: searching a `VecDeque<String>` with ANSI baked in means either
matching escape bytes or re-stripping the whole buffer per keystroke.

`push_line` is already the single choke point that disk logging and
password masking depend on, so the change is one struct and one funnel
today. After 1.0 it is also the on-disk log format and the exported
transcript.

## Features that would break the architecture

**`/connect <profile>` into a running instance.** `UX_REVIEW.md` ranks this
as a workflow convenience; structurally it is the largest item in either
document. The peer `watch` mesh is built from `targets` before the event
loop starts, handed to each session at spawn, and never revisited;
`SessionCommand` has no `AddPeer`. A session added later would be invisible
to every existing one and vice versa, so `${@newchar.hp}` and
`mud.on_peer` resolve empty for exactly the character just added. Needs a
hub-owned peer registry with a dynamic handle.

**Party HUD / "who needs me?" / follow-the-leader.** Inherits the above,
and adds a second problem: the HUD lives in `ui`/`app`, which holds no peer
receivers at all — only session tasks do. `app` has to join the mesh as a
reader.

**Auto-mapper, sound packs (MSP), tell threading, party HUD.**
`gmcp::supports_message()` advertises a hardcoded `["Char 1", "Room 1"]`
with no way for a profile, module, or script to add a package. `Group`,
`Comm.Channel`, and `Client.Media` will therefore never be requested, and
servers that gate pushes on `Core.Supports.Set` will never send them. Small
fix, but it must land before any of those features, and it becomes a
compatibility surface as soon as profiles can declare packages.

**Trigger test bench and fire counts.** The live `Engine` is unreachable
from the UI: the only ways in are `SessionCommand::SetRules` (wholesale
replace) and events out, `process_line` mutates rather than offering a dry
run, and the command channel is fire-and-forget with no reply path. The
config editor's existing validation compiles a *fresh* engine, which by
construction has none of the live session's variables or server data, so
`when:` guards cannot be meaningfully evaluated there.

**Split-scroll live strip.** `session_pane_sizes` (`src/ui/mod.rs:173`)
derives NAWS from the session rect, so splitting a pane into history plus a
live strip changes the height the server is told — pressing `PgUp` would
send a NAWS resize mid-read and re-wrap the server's output. §11.4
deliberately made resize a NAWS event; this feature needs the opposite, a
viewport split that is explicitly not server-visible.

## A real defect in a documented feature

> **Fixed.** Flattening now reports each array's length, and
> `Engine::prune_gmcp_array` drops indices at or past it — narrower than a
> package-wide subtree replace, so partial object updates still merge.
> `poll_peer` also reports vanished keys (empty value), without which the
> array form of a dropped buff produced no event at all. §6.3 now states
> the merge/replace semantics that were previously unspecified.

`Engine::update_server_data_from_gmcp` (`src/engine/mod.rs:703`) only ever
inserts; keys are never removed. Combined with `gmcp::flatten_json`'s
positional array indexing, a shrinking array leaves a phantom entry that
never expires: `Char.Affects` going from `["bless","haste"]` to
`["haste"]` leaves `Char.Affects.1` reading `haste` forever.

*Correction on verification:* the review as first reported claimed no
`on_peer` fires at all. Index 0 does change (`bless`→`haste`) and does
fire. The accurate statement is that an event fires but the store is
silently corrupted, and the removal of `bless` produces no event of its
own — so §7.5's headline example ("rebuff the tank when his blessing
drops") does not work as documented. Fix is subtree-replace semantics per
GMCP package, or a length key on arrays.

## Boundaries

The sans-IO core is real, not aspirational: `src/proto/*` references no
`crate::` path outside itself and imports no I/O, and `engine` is the same
(its one outside reference is `tokio::sync::watch` as a type in
`PeerSnapshot` — not I/O, but it does put a tokio type in a
declared-sans-IO signature). `session` composes `proto` + `engine` + `net`
as documented.

Three small drifts, all pointing the same way — `config` and `ui` have
become peers of `app` rather than layers beneath it:

1. `src/config/mod.rs:354` reads `crate::ui::CHANNEL_WIDTH` — an inverted
   edge, file parsing depending on drawing.
2. `src/ui/mod.rs:18` imports `AppState`/`Focus`/`LayoutMode` from `app`.
   Defensible (app owns the model, ui renders it), but it means `ui`
   cannot be promoted to a crate until `AppState` moves.
3. `src/ui/config_editor.rs` pulls `VerifyMode` from `net` — an enum, not a
   socket, so "ui knows nothing about sockets" survives, but the type
   arguably belongs in `config`.

All three are trivial moves, and all three block §16's workspace split
until made.

Separately, a doc/implementation divergence: §11.1 describes channel panes
docking "into the same pane grid as session panes." There is no grid.
`ui::layout` hard-codes body → `[main | fixed-width channel column]`. Every
feature wanting a new region pays for this.

## One-way doors, ranked by cost if deferred

1. ~~**The scrollback line type.**~~ Closed — see above. One struct now;
   after 1.0 it would have been the log format, the transcript, and eight
   features.
2. **Dynamic peer mesh and session lifecycle.** `Focus::Session(usize)` and
   `input_session: usize` bake index-as-identity into `AppState`, `ui`, and
   the tests. Adding a session is survivable; removing one is not — and
   `SessionCommand::Disconnect` already exists, `#[allow(dead_code)]`.
3. **GMCP subtree semantics and `Core.Supports.Set`.** Once modules and
   scripts depend on the flat merged map, changing what
   `${Char.Affects.0}` means is a breaking change.
4. **Client-command dispatch.** `submit_input` is three hardcoded
   `line.trim() ==` comparisons; `/connect`, `/newprofile`, `/errors`, and
   a command palette all want a registry.
5. **`VerifyMode` in `net`, `CHANNEL_WIDTH` in `ui`.** Trivial, unblocks
   the workspace split.

## Sequencing problems

- `IDEAS.md`'s "if only three" was misordered against its own
  dependencies (corrected in that document as of this review).
- `UX_REVIEW.md` suggestion A (`/connect`) is ranked as a nicety but is
  architecturally the largest item in either doc. Suggestion B
  (`/newprofile`) is genuinely small and correctly ranked.
- §16 says module sharing's prerequisite is sandboxing. True for scripts,
  but the unaddressed prerequisite is distribution integrity — nothing
  covers module provenance or update, and `deny_unknown_fields` means a
  module written against a later version fails to load with no
  forward-compatibility story.
- Corpse run → mapper → GMCP `Room.*` is correctly stated, but the mapper
  also needs the retained-line type (movement inference reads the line
  stream) and subtree-replace semantics (exits are an array).

## Worth cutting

- **JavaScript as a shipped engine.** §7.4's stated purpose is proving the
  `ScriptHost` abstraction is real — but `engine/script/conformance.rs` is
  what actually proves it. 572 lines in `js.rs` plus a second VM's
  maintenance is a steep price for a demonstration a test double would
  also provide. Keep the trait and the suite; make JS an example.
- **`LayoutMode::Splits` as horizontal-only `even_split`.** Either it
  becomes the real grid §11.1 already claims — which the party HUD, live
  strip, and map pane all need — or it should stop being described as one.
  Currently it is too rigid to extend while complex enough to carry its own
  NAWS path.
- **`SessionPane` as a 25-field struct.** It holds render state, an mpsc
  sender, a `BufWriter<File>`, a plaintext password, and rule provenance at
  once. This is not over-abstraction, it is the absence of one, and every
  planned feature adds a field. Splitting presentation state from
  session-handle state is the single cheapest change that makes the rest of
  the roadmap tractable.

## Well-designed, leave alone

The sans-IO `proto` layer and its byte-fixture test discipline; the
pipeline ordering in §6.5; receiver-side `expand_aliases` in §7.5 (a sender
genuinely cannot force execution at the far end); `highlight:` and `bell:`
as trigger *actions* rather than parallel lists; and `when:` as a
first-party evaluator rather than a script call — §7.6's reasoning about
feature-gating and coercion semantics is correct, and the alternative would
have been a mess.
