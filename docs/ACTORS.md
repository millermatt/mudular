# Mudular — who decisions are for

Two questions, and they take different answers:

- **What has to stay true regardless of who benefits?** — the invariants.
- **Who is this change for?** — the people.

Only one kind of decision here has a winner, and it is a narrow one: see
"Defaults".

## Invariants

Not anyone's convenience, and not traded. A feature that needs one
relaxed is a feature that needs redesigning. They are stated apart from
the people below because they are never the side that loses.

**A module or script reaches no further than its own directory and its own
budget.** The sandbox, the bare-name path checks (`load_scripts`,
`module_path`), the `deny_unknown_fields` schema and the 100ms per-hook
ceiling all serve this (§7.4). It holds whether the code was imported
from a stranger or written by the person running it: trusting the author
is not something the loader can verify, so it is not something the design
may assume. Text a script or a module produces is untrusted for the same
reason, wherever it reaches a screen or a path (§13).

**Local data belongs to whoever ran the client.** Transcripts, capture
logs, maps and stored profiles are written owner-only by default, and
server-controlled strings are untrusted wherever they can reach a
filesystem path (§5, §13). A missing permission check here fails silently
until it is exploited, which is what makes it an invariant rather than a
preference.

## Who plays

Unranked. Appearing here does not win an argument; it means a change
aimed at this person has an audience, and a change aimed at nobody has a
question to answer.

### The multi-boxer

Runs two or more characters at once and relies on one session driving
another — the tank's session healing the cleric's at a health threshold
(§7.5). Most of the architecture exists for this player: per-session
isolation (§4), hotkey focus switching, unread indicators, the
cross-session peer-hook protocol. Multi-character *correctness* — buffer
isolation, peer message ordering — is nearer an invariant than a
preference, since getting it wrong puts one character's output in
another's pane, which is a bug under any framing.

### The solo player

One character, one pane. The most common way to play a MUD, and the
player this client's defaults do **not** favour — see "Defaults". A bet
that is never written down cannot be revisited when it turns out to be
wrong, so it is written down.

### The player using a screen reader

MUDs are among the most-played game genres by blind players, because they
are pure text, and the genre has clients built for them specifically.

What this person needs is a line lifecycle rather than a rendered screen:
new lines announced as they arrive, reading interrupted the moment a
command is sent, and one channel speakable on its own. The first two are
properties of the §6.5 pipeline, which already knows both when a line
completes and when a command is sent; the third reuses channel routing
(§11.1).

A full-screen TUI is not that surface — a screen reader re-reads a
redrawn pane instead of announcing what changed — so serving this player
means a line-oriented output path beside the panes rather than options
bolted onto them. That path is not built (#39); what is settled is that
it is allowed to exist, which is why this entry describes what the person
needs rather than what the client currently does.

### The module author / community sharer

Writes YAML triggers or Lua/JS scripts and distributes them as a
directory others copy in (§7.4). What this person needs from a design is
legibility at the far end: a shared module is read and adjusted by
someone who did not write it and cannot ask its author. Plain patterns
over regex (§7.1) come from that.

Their security properties are not here. They are invariants, where no
one's convenience can trade them away.

### The newcomer

Has never edited YAML and meets the first-run wizard before anything
else. The in-TUI profile form and config editor (§10.2, §15) exist so
this person never has to hand-edit a file. Judge error messages,
defaults, and the wizard's question order by whether they make sense to
someone who has not read `docs/USAGE.md`.

### The power-user scripter

Writes their own Lua/JS rather than importing someone else's, so their
friction is the sandbox's limits rather than its existence. The limits
hold anyway: "I trust this script" is not a thing the loader can check.

## Defaults

There is one default layout, one thing a given key does, one answer for
`map_first`. A default cannot be split between two people, which makes it
the one place precedence is needed:

> **When a default has to favour someone, it favours the multi-boxer.**

That is the product bet: this client is for running several characters at
once, and its defaults should suit that with nothing configured.

It says nothing about anywhere else, because everywhere else the question
does not arise — a preference, a mode or a command can serve one player
without being taken from another. Reach for this rule only when the thing
being decided is genuinely singular.

## "Who is this for?"

Ask it of new surface: features, config keys, client commands, panes. Not
of bug fixes, refactors or tests.

A change that serves nobody above is one to question before building
rather than after (`CLAUDE.md`'s scope-discipline note: no speculative
features). But "serves nobody listed" is a prompt, not a veto. This list
describes; finding a real player it fails to describe is a reason to fix
the list.
