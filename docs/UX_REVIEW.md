# UX Review — 2026-08-08

An adversarial pass through the live TUI (via the `run` skill, against a
throwaway local fake MUD server) reviewed through each persona in
`docs/ACTORS.md`, plus a general workflow read of `docs/USAGE.md` looking
for friction that isn't a bug so much as a rough edge. Findings here are a
snapshot, not a backlog — treat severity as "how bad if unfixed," not
priority order across the two sections.

## Adversarial findings

### High

**1. Newcomer — `Esc` on the first-run wizard silently kills the whole app,
not just the form.** With zero profiles saved, pressing `Esc` on the very
first field (before any error is shown) terminates the mudular process
entirely — no message, no "nothing to connect to" screen. USAGE.md's "`Esc`
at any point backs out without saving or connecting" reads as "you're
returned to the app," not "the app quits." A newcomer backing out to think
it over gets what looks like a crash.

**2. Security-conscious player — TLS pin-mismatch reason is truncated to
illegibility.** When a pinned certificate changes (simulated MITM/rotation),
the only explanation is the pane's one-line status area, which gets cut off
mid-word: `disconnected: TLS handshake with 127.0.0.1:5577: unexpected
error: pinned certificate mi`. Scrollback is empty — no fingerprint
comparison, no guidance. This is exactly the moment a cautious user most
needs a complete explanation, and gets a clipped fragment instead.

### Medium

**3. Module author — duplicate/no-op triggers save silently.** Adding a
second trigger with the exact same `pattern` as an existing one (empty
`send:`) saves with no warning — a dead rule that fires on every match and
does nothing, easy to create by accident and confusing to debug later. The
editor validates regex syntax and missing `id`/`pattern` but not
exact-duplicate patterns or an empty `send:`.

**4. Power-user scripter — Lua runtime-error tracebacks render garbled.**
The headline error (`bad.lua:3: kaboom from script`) is clear and has file/
line, which is good — but the traceback underneath is corrupted on screen:
tabs collapse into `inC?:`/`inCfunction 'error'`, and `[string "bad.lua"]:3`
renders with a stray leading `s`. The failure is legible; the detail needed
to actually debug from it is not.

### Low

**5. Doubled backticks in error messages** — systemic, not a one-off:
`` rule in `profile `tank`` needs an `id` or a `pattern` `` nests backticks
around an interpolated name inside an outer backtick-quoted phrase, across
multiple validation/reload error paths.

**6. No cap on profile name length in the wizard** — a 200-character name is
accepted with no warning, and the dialog box grows to full terminal width to
fit it, breaking the centered layout. The name becomes a filename
(`<name>.yaml`) with no guardrail.

### Worked well (no notable issues)

Multi-session focus switching and unread badges under rapid switching;
password masking (no echo, nothing stored); reconnect-backoff messaging
matching docs exactly; config editor's regex/id validation on save; TLS
trust-on-first-use messaging and its owner-only `known_certs` permissions;
the `F1` help overlay reflecting live keybinds; `/reload` keeping the
previous working rules when a script fails to load.

## General workflow / UI suggestions

Not bugs — places where the documented design already works, but a small
change would make it noticeably easier to live in day to day. Ranked by
which actor (`docs/ACTORS.md`) it serves and how much friction it removes.

**A. No way to add a character to a running instance.** `mudular tank
cleric` starts both at once, but there's no documented way to bring a third
character into an already-running session — the multi-boxer has to quit and
relaunch with all profile names, losing every pane's scrollback that isn't
being disk-logged. A `/connect <profile>` command (opening a new pane the
same way startup does) would match how naturally this actor's day actually
goes: decide to duo an hour after your first character logs in.

**B. Creating a second profile means leaving the TUI.** The first-run wizard
only appears with zero profiles saved (§15); after that, a new character
means hand-writing a YAML file from scratch or copying an existing one.
Given the config editor (`F5`) already exists for editing a profile in the
client, and the wizard already exists for the *first* profile, the gap is
narrow: reuse the wizard as a "new profile" flow reachable any time (a
keybind, or `/newprofile`), not just at zero-profile startup.

**C. No persistent reminder that `F1` exists.** The help overlay is
complete and stays accurate (confirmed above), but a newcomer has to
already know to press `F1` to discover it. A one-line hint on the very
first screen after the wizard finishes connecting — "press F1 for the full
key list" — costs one line and removes the single biggest "what do I do
now" gap for actor 3.

**D. Client-generated warnings are as ephemeral as server text.** A failed
`/reload`, a dropped `send_to:` (unconnected target), or a rejected
keybind at startup all land as ordinary scrollback lines or a status
message — indistinguishable in retrospect from anything else that scrolled
by, and gone once it scrolls off unless disk logging happens to be on. A
lightweight `/errors` (or a small persistent panel) that keeps the last N
client-generated warnings — separate from server text — would mean a
missed message is a `PgUp` away instead of gone.

**E. No live feedback while writing a trigger pattern.** `Alt+V` (pick a
scrollback line, pre-fill the pattern) is a strong start, but once you're
editing the regex by hand in the config editor there's no indication of
whether it actually matches anything in recent scrollback until you save,
`/reload`, and wait for a real line. Showing a match/no-match indicator
against the pane's existing scrollback while the pattern field has focus
would catch most regex mistakes before they ever reach disk.

**F. `/reload` has no keybind.** Every other frequent action in the Keys
table (§"Keys") has a default binding; `/reload` is command-only. Anyone
editing rule files in their own editor while playing — the module-author
actor's normal workflow — retypes it by hand every time. A default
keybind (with the same "remappable" treatment as everything else) would
make it consistent with the rest of the client's own design.

## Joy, not just function

Everything above is about removing friction. This section is the opposite
question: where does the experience currently work correctly but feel flat,
and what would make it satisfying rather than merely functional? In keeping
with the client's own restraint (§2's latency focus, no GUI/graphics
non-goal) these are small and tasteful — craftsmanship, not decoration. A
terminal app earns delight by being fast, precise, and quietly clever, not
by adding motion or noise.

For the wider, unconstrained version of this question — ideas not limited
to current or planned functionality — see [IDEAS.md](IDEAS.md).

**G. The magic tricks are invisible.** Speedwalking (`.3n2e`), alias
expansion, and trigger substitution all work by silently rewriting what you
typed or what the server sent — which is exactly right for steady-state
play, but means a newcomer who *discovers* `.3n2e` for the first time gets
no confirmation it did anything clever; it just... sends five commands.
A first-use-only echo (`.3n2e → n, n, n, e, e`, shown once per session the
first time speedwalking actually expands something) would let the feature
introduce itself instead of working invisibly on faith. The same idea
applies to a trigger firing for the very first time in a session — not a
running commentary, just the one moment where the automation proves itself
to a player who just wrote it.

**H. Colour is used for identity but stops at the border.** A profile's
`color:` tints its pane border and tab entry (§"Playing several characters
at once") — a genuinely nice touch for telling characters apart at a
glance. The unread badge (`● 3`) next to it renders in a fixed color,
though, so the one place your eye jumps to first when something happens
elsewhere doesn't carry the same per-character identity the rest of the
tab does. Tinting the badge to match would make the whole tab read as one
character's, not two decisions layered on top of each other.

**I. Success is silent.** A clean connect, a successful `/reload`, a saved
profile — each of these either says nothing beyond a scrollback line
mixed in with server text, or (per finding 3) doesn't confirm at all. None
of this needs fanfare, but the client already has a vocabulary for "this
worked" — `highlight:`, the profile's own colour — that user-facing rules
get and the client's own feedback doesn't reuse. A save/reload confirmation
that borrows the same restrained, coloured-text treatment a trigger gets
would make the tool feel like it's speaking the same visual language back
to you that you use to talk to it.

## Priority read

If picking a small set to act on: **1** and **2** from the adversarial
findings are "actor loses trust or gets stuck," the worst outcomes the
severity rubric defines — fix those first. Of the workflow suggestions,
**A** and **B** are the two that serve the top-ranked actor
(`docs/ACTORS.md` §1, the multi-boxer) and remove the most friction from
sessions that already happen constantly, rather than edge cases. Of the
joy suggestions, **G** costs the least and pays back the most: it's a
one-time, opt-out-by-nature echo (fires once, then gets out of the way)
that turns two of the client's cleverest features from silent plumbing
into something a newcomer actually notices and remembers.
