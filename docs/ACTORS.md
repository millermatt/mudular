# Mudular — Actors

Who a design or implementation decision should serve, ranked by how much of
the architecture already exists to serve them. When a decision has to trade
one actor's convenience against another's, the actor listed first wins.

## 1. The multi-boxer

Runs two or more characters at once in split panes and relies on one
session driving another — the tank's session healing the cleric's at a
health threshold (ARCHITECTURE.md §7.5). This is the primary design driver:
the module map's per-session isolation (§4), zero-latency hotkey focus
switching, unread indicators, and the cross-session peer-hook protocol all
exist because of this actor. When a tradeoff pits multi-character
correctness (buffer isolation, peer message ordering) against convenience
elsewhere, this actor wins.

## 2. The module author / community sharer

Writes YAML triggers or Lua/JS scripts and distributes them as a directory
others copy in (§7.4). This actor is either the trusted author (their own
modules) or the attacker vector (an imported module from someone else) —
the sandboxing, bare-name path checks (`load_scripts`, `module_path`), and
`deny_unknown_fields` schema all exist for this actor in both roles at
once. Any feature that lets a module or script reach further than its own
directory or exceed its time/resource budget is a regression for this
actor, not a convenience.

## 3. The newcomer

Has never edited YAML and hits the first-run wizard before anything else.
The in-TUI new-profile form and profile editor (§10.2, §15) exist
specifically so this actor never has to hand-edit a config file. Error
messages, defaults, and the wizard's question order should be judged by
whether they make sense to someone who has never seen `docs/USAGE.md`.

## 4. The security-conscious player on a shared machine

Cares about TLS certificate pinning against MUDs running self-signed certs
(§5, §13), and about who else on the machine can read transcripts,
capture logs, and stored profiles. Mostly invisible until something goes
wrong — a missing permission check or a path-traversal bug is a silent
failure for this actor until it's exploited. Design for them by keeping
newly-written files owner-only by default and treating server-controlled
strings as untrusted wherever they reach a filesystem path.

## 5. The power-user scripter

Writes their own Lua/JS triggers rather than importing someone else's.
Overlaps with actor 2, but trusts their own code, so their friction point
is the sandbox's limits (no `coroutine`, a 100ms per-hook budget) rather
than the sandbox's existence. Don't loosen the sandbox to make this
actor's life easier — actor 2's threat model doesn't change just because
this particular script came from a trusted source.

## Using this list

Actors 1 and 2 should win when a design decision creates a genuine
conflict — the architecture doc's own goals (§1) point there. Actors 3–5
are "don't break this for them," not "optimize for them": their needs are
served by not regressing, not by adding new surface area on their behalf.

If a proposed feature doesn't clearly serve one of these actors, that's a
reason to ask why it's being built before building it (see the top-level
`CLAUDE.md`'s scope-discipline note: no speculative features).
