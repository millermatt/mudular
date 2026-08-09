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

> **Fixed.** `main.rs`'s wizard-cancel branch fell straight through to
> `return Ok(())`, exiting the process instead of continuing to the
> ordinary zero-target startup path (the same "no target — run with a
> profile name..." shell shown once a profile exists and `mudular` runs
> bare). It now falls through instead of returning early. Verified live,
> not just in a unit test — main.rs had none — by driving the actual
> binary through a pty with the `run` skill both before and after: the
> process died on `Esc` pre-fix, and stayed up showing the shell post-fix.

**2. Security-conscious player — TLS pin-mismatch reason is truncated to
illegibility.** When a pinned certificate changes (simulated MITM/rotation),
the only explanation is the pane's one-line status area, which gets cut off
mid-word: `disconnected: TLS handshake with 127.0.0.1:5577: unexpected
error: pinned certificate mi`. Scrollback is empty — no fingerprint
comparison, no guidance. This is exactly the moment a cautious user most
needs a complete explanation, and gets a clipped fragment instead.

> **Fixed.** `SessionEvent::Ended` only ever set the one-line status; it now
> also pushes the full reason to scrollback, matching the pattern §13
> already uses for security warnings (both status label and scrollback
> line). Scrollback wraps and never truncates, so the complete reason is
> always reachable regardless of length. Verified with a unit test built
> from the review's own reported message (fails without the fix — empty
> scrollback) and confirmed live via the `run` skill against a real refused
> connection, where the full reason renders correctly through the actual
> render/wrap pipeline.

### Medium

**3. Module author — duplicate/no-op triggers save silently.** Filed as
#12.

> **Partly fixed.** The tab half: `strip_unsafe_controls` dropped tabs
> outright, welding `in\tfunction` into `infunction` — and it only ever ran
> on server text, so a script's traceback reached the screen with its raw
> tabs intact, which ratatui writes to the terminal as real cursor
> commands. Every retained line is now sanitised (§13), and a tab becomes a
> space rather than vanishing. The stray leading `s` on
> `[string "bad.lua"]:3` is not explained by this and has not been
> reproduced; it is still open — #13.

**4. Power-user scripter — Lua runtime-error tracebacks render garbled.**
The headline error is clear and has file/line; the traceback underneath is
not (tabs collapse into `inC?:`/`inCfunction 'error'`, and
`[string "bad.lua"]:3` gets a stray leading `s`). Tab half fixed above;
the stray-`s` half is #13.

### Low

**5. Doubled backticks in error messages** — systemic, not a one-off,
across multiple validation/reload error paths. Filed as #14.

**6. No cap on profile name length in the wizard** — a 200-character name
breaks the centered dialog layout with no guardrail. Filed as #15.

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

**A. No way to add a character to a running instance.** The multi-boxer has
to quit and relaunch with all profile names to duo, losing every pane's
scrollback that isn't disk-logged. Filed as #2.

**B. Creating a second profile means leaving the TUI.** The first-run
wizard only appears with zero profiles saved; reusing it as a "new
profile" flow reachable any time is a narrow gap to close. Filed as #16.

**C. No persistent reminder that `F1` exists.** A one-line hint on the
first screen after the wizard finishes connecting would remove the
biggest "what do I do now" gap for a newcomer. Filed as #17.

**D. Client-generated warnings are as ephemeral as server text.** A failed
`/reload` or a dropped `send_to:` is indistinguishable in retrospect from
anything else that scrolled by. A lightweight `/errors` panel would make a
missed message a `PgUp` away instead of gone. Filed as #18.

**E. No live feedback while writing a trigger pattern.** `Alt+V` prefills
a pattern, but there's no indication whether a hand-edited regex actually
matches anything until save/reload/wait. Filed as #19.

**F. `/reload` has no keybind**, unlike every other frequent action in the
Keys table. Filed as #20.

## Joy, not just function

Everything above is about removing friction. This section is the opposite
question: where does the experience currently work correctly but feel flat,
and what would make it satisfying rather than merely functional? In keeping
with the client's own restraint (§2's latency focus, no GUI/graphics
non-goal) these are small and tasteful — craftsmanship, not decoration. A
terminal app earns delight by being fast, precise, and quietly clever, not
by adding motion or noise.

For the wider, unconstrained version of this question — ideas not limited
to current or planned functionality — see [IDEAS.md](IDEAS.md) and the
GitHub issues labeled `idea`.

**G. The magic tricks are invisible.** Speedwalking, alias expansion, and
trigger substitution all work by silently rewriting what you typed or what
the server sent, with no confirmation the first time a newcomer discovers
one actually did something clever. Filed as #21.

**H. Colour is used for identity but stops at the border.** A profile's
`color:` tints its pane border and tab entry, but the unread badge next to
it renders in a fixed colour. Filed as #22.

**I. Success is silent.** A clean connect, a successful `/reload`, a saved
profile say nothing beyond a scrollback line mixed in with server text.
Filed as #23.

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
