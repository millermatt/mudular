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
> number this does not yet assign.
>
> The plain-text projection is now cached on the line (stored only when the
> text has escapes in it), so search will not re-strip the buffer per
> keystroke. Consolidating on it also removed a second, disagreeing
> implementation: `Alt+V` built its pattern via the render path's parser,
> which keeps `ESC M` as a literal `M` where `strip_ansi` drops it — so a
> pattern picked from such a line could not match the line it came from.

Scrollback is `VecDeque<String>` (`SessionPane.scrollback`,
`ChannelPane.lines`). By the time a line arrives there it has been
flattened: highlights spliced into the ANSI (§7.7), channel timestamps and
origin tags prepended *as text*. Nothing retains arrival time, origin
(server vs. client notice), which rule fired, or whether it was a tell.

That one representation blocked seven wishlist items at once (the "while
you were away" digest, tells-as-conversations, session stats, personal
bests, death replay, bookmark-a-moment, RP transcript export — all now
unblocked, filed as issues labeled `idea`). It also blocked `UX_REVIEW.md`
finding D — client-generated warnings were indistinguishable from server
text because they were the same `String` in the same deque — and
screen-reader "announce only new lines," which needs per-line identity to
diff against. §11.5's deferred scrollback search inherited it too:
searching a `VecDeque<String>` with ANSI baked in means either matching
escape bytes or re-stripping the whole buffer per keystroke.

`push_line` is already the single choke point that disk logging and
password masking depend on, so the change is one struct and one funnel
today. After 1.0 it is also the on-disk log format and the exported
transcript.

## Features that would break the architecture

Each of these names an architectural prerequisite, not a UI tweak, which is
why they're filed as issues rather than left as a feature-request list:

- **`/connect <profile>` into a running instance** (#2) — structurally the
  largest single item this review found. The peer `watch` mesh is built
  from `targets` before the event loop starts and never revisited; needs a
  hub-owned peer registry with a dynamic handle.
- **Party HUD / "who needs me?" / follow-the-leader** (#3) — inherits #2's
  blocker, plus `app` has to join the peer mesh as a reader (only session
  tasks hold receivers today).
- **Auto-mapper, sound packs (MSP), tell threading** — all blocked by the
  same gap as the party HUD: `gmcp::supports_message()` hardcodes the
  advertised package list (#25), so `Group`, `Comm.Channel`, and
  `Client.Media` are never requested.
- **Trigger test bench and fire counts** (#4) — the live `Engine` is
  unreachable from the UI (`process_line` mutates rather than offering a
  dry run, and the command channel is fire-and-forget with no reply path).
- **Split-scroll live strip** (#5) — `session_pane_sizes` derives NAWS from
  the session rect, so a naive split would resize the server's view
  mid-read; needs a viewport split that isn't server-visible.

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

## Found while closing the retained-line item

Not part of the original review — turned up reading §13 against the code
after the line type landed, and recorded here because it is the same
question ("who is allowed to put bytes on the terminal") one layer down.

**§13's escape-injection guarantee held for one path, not every path.**
The scrollback path is sound: server text loses its control bytes before
line assembly, and `ansi-to-tui` reliably consumes escape sequences —
`ansi_lines`'s `Err(_)` fallback, which would have put the raw string with
its escapes into a `Line`, turns out to be unreachable, since the crate's
parser swallows its own failures and always returns `Ok`.

The **raw GMCP inspector** was not. A GMCP payload arrives as a
subnegotiation, so it never meets the line pipeline's filter, and
`ui::draw_session` renders `gmcp_log` with `Line::raw` rather than through
the ANSI parser — and ratatui writes a cell's symbol to crossterm's
`Print` unfiltered. A server sending `Char.Vitals {"hp":<ESC>[2J...}` had
its escape sequences executed by the player's terminal the moment they
pressed `F2`. No script, no config, no cooperation from the player beyond
looking at a debugging view.

Fixed by escaping rather than stripping there (the inspector exists to
show what arrived), and by moving the control-byte filter to
`RetainedLine::new` so it covers every line a pane keeps rather than only
the ones that came off a socket. The narrower lesson is the one worth
keeping: a guarantee written as a property of *a parser* ("the ANSI parser
drops unknown sequences") only holds where that parser is actually in the
path. Stated as a property of the funnel, it holds everywhere.

## Boundaries

The sans-IO core is real, not aspirational: `src/proto/*` references no
`crate::` path outside itself and imports no I/O, and `engine` is the same
(its one outside reference is `tokio::sync::watch` as a type in
`PeerSnapshot` — not I/O, but it does put a tokio type in a
declared-sans-IO signature). `session` composes `proto` + `engine` + `net`
as documented.

Three small drifts, all pointing the same way — `config` and `ui` have
become peers of `app` rather than layers beneath it (`config` reads
`crate::ui::CHANNEL_WIDTH`, `ui` imports `AppState`/`Focus`/`LayoutMode`
from `app`, `ui::config_editor` pulls `VerifyMode` from `net`). All three
are trivial moves, and all three block §16's workspace split until made —
filed as #6.

> **Fixed (doc only).** §11.1 claimed channel panes dock "into the same
> pane grid as session panes." There is no grid — `ui::layout` hard-codes
> body → `[main | fixed-width channel column]`, and every feature wanting
> a new region pays for that. §11.1 now states the real layout instead of
> the aspirational one.

## One-way doors, ranked by cost if deferred

Ranked by cost if left in place while more gets built on top. Tracked as
issues rather than restated here:

1. ~~**The scrollback line type.**~~ Closed — see above. One struct now;
   after 1.0 it would have been the log format, the transcript, and eight
   features.
2. **Dynamic peer mesh and session lifecycle** — #2. Also:
   `Focus::Session(usize)` / `input_session: usize` bake index-as-identity
   into `AppState`, `ui`, and the tests, so adding a session is survivable
   but removing one is not (`SessionCommand::Disconnect` already exists,
   `#[allow(dead_code)]`) — worth resolving alongside the registry.
3. **GMCP subtree semantics and `Core.Supports.Set`** — #25. Once modules
   and scripts depend on the flat merged map, changing what
   `${Char.Affects.0}` means becomes a breaking change.
4. **Client-command dispatch** — #7. `submit_input` is three hardcoded
   `line.trim() ==` comparisons; `/connect`, `/newprofile`, `/errors`, and
   a command palette all want a registry instead.
5. **`VerifyMode` in `net`, `CHANNEL_WIDTH` in `ui`** — #6. Trivial,
   unblocks the workspace split.

## Sequencing problems

- `UX_REVIEW.md` suggestion A (`/connect`, #2) is ranked as a nicety but is
  architecturally the largest item in either doc. Suggestion B
  (`/newprofile`, #16) is genuinely small and correctly ranked.
- §16 says module sharing's prerequisite is sandboxing. True for scripts,
  but the unaddressed prerequisite is distribution integrity — nothing
  covers module provenance or update, and `deny_unknown_fields` means a
  module written against a later version fails to load with no
  forward-compatibility story. Filed as #8.
- Corpse run → mapper → GMCP `Room.*` (#30) is correctly stated, but the
  mapper also needs subtree-replace semantics (#25, exits are an array) —
  it no longer needs the retained-line type as a separate prerequisite,
  since that landed (see above).

## Worth cutting

Each below is a proposal to remove or re-scope something already built,
not a missing feature — filed as issues so the decision has a place to
land:

- **JavaScript as a shipped engine** — #9. §7.4's stated purpose is
  proving the `ScriptHost` abstraction is real, but
  `engine/script/conformance.rs` is what actually proves it; a second VM's
  maintenance is a steep price for what a test double would also provide.
- **`LayoutMode::Splits` as horizontal-only `even_split`** — #10. Either it
  becomes the real grid the party HUD, live strip, and map pane all need,
  or it should stop being described as one.
- **`SessionPane` as a 25-field struct** — #11. Holds render state, an
  mpsc sender, a `BufWriter<File>`, a plaintext password, and rule
  provenance at once; every planned feature adds another field to it.

## Well-designed, leave alone

The sans-IO `proto` layer and its byte-fixture test discipline; the
pipeline ordering in §6.5; receiver-side `expand_aliases` in §7.5 (a sender
genuinely cannot force execution at the far end); `highlight:` and `bell:`
as trigger *actions* rather than parallel lists; and `when:` as a
first-party evaluator rather than a script call — §7.6's reasoning about
feature-gating and coercion semantics is correct, and the alternative would
have been a mess.
