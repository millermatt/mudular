# Ideas

A wishlist, not a roadmap and not design authority — `ARCHITECTURE.md` is
both of those. Nothing here is committed to, sized, or necessarily a good
idea; it's a place to keep thoughts about what would make Mudular a joy to
play with rather than merely functional, so they aren't lost between
milestones. Scope discipline still applies to code: writing an idea down
here is explicitly *not* permission to build it.

Where an idea already has a home in `ARCHITECTURE.md` §16 (Designed-For
Extensions), it's cross-referenced rather than restated. Structural
blockers are cross-referenced to `ARCH_REVIEW.md`, which reviewed this
document as part of the load the architecture has to carry.

The organizing thought: in a MUD, the client *is* the game. There's no 3D
world to look at — just the player's instrument. Joy comes from it feeling
like an extension of their hands.

Actor references are to `ACTORS.md`.

## Two prerequisites almost everything here shares

`ARCH_REVIEW.md` found that a surprising number of these ideas are blocked
by the same two structural facts, so they're worth stating once up front
rather than repeating per item:

1. **Scrollback is `VecDeque<String>`, already flattened.** Timestamps and
   origin tags are prepended *as text*; nothing retains arrival time,
   origin, or which rule fired. Everything below marked **[line type]**
   needs that changed first — it's one struct at one choke point today, and
   the on-disk log format after 1.0.
2. **The peer mesh is static and `app` isn't in it.** Built before the
   event loop, never revisited, and held only by session tasks. Everything
   marked **[peer mesh]** needs a hub-owned registry with a dynamic handle.

Neither is a reason not to want these features. They're the reason to fix
those two things first, while both are still cheap.

## Never lose the thread

The most universal MUD frustration: something good happens while you're
looking somewhere else.

- **Scroll without going deaf.** Scrolling back currently cuts you off from
  live action. Instead, `PgUp` splits the pane — history above, a live 3–5
  line strip still running at the bottom — so reading and being present
  stop being mutually exclusive. *Not* the cheap `ui`-only change it looks
  like: `session_pane_sizes` derives NAWS from the pane rect, so a naive
  split would tell the server the window shrank and re-wrap its output
  mid-read (ARCH_REVIEW.md). Needs a viewport split that is explicitly not
  server-visible.
- **"While you were away."** **[line type]** A foldable digest after an idle
  stretch: *3 tells from Bob, 1 group invite, attacked twice*. Dismissible
  in one key.
- **Tells as conversations.** **[line type]** Thread tells per-person
  instead of scattering them through combat spam. Channel panes (§11.1)
  already solve the "pull it out of the main buffer" half; this is the "and
  keep it coherent" half.
- **Scrollback search.** **[line type]** Incremental, highlight-in-place.
  Deferred past M9 by §11.5; note it inherits the same blocker, since
  searching a deque of ANSI-baked strings means either matching escape
  bytes or re-stripping the buffer per keystroke.

## The map problem

The single biggest reason players choose one client over another.

- **Auto-mapper and pathfinding** — already §16, deliberately post-1.0 and
  dependent on M6's GMCP plumbing. Noted here only for the corpse run
  below, which rides on it.
- **The corpse run.** You die; the client already knows the path you walked
  in on, and walks you back. This turns MUDing's most miserable ritual into
  a non-event, and it's a small feature *on top of* the mapper rather than
  a second large one.
- **Speedwalk recording.** Walk a route once, name it, get an alias — plus
  auto-reverse for the return trip. Extends M9's macro speedwalks (§"Speed-
  walking") without needing the full mapper.

## Multiboxing as one player, not N players

Serves actor 1 directly, and no competitor does it well.

- **Party HUD.** **[peer mesh]** One strip showing every character's vitals side by side,
  from GMCP already being parsed. Multiboxers currently do this in their
  head, across panes. Two prerequisites neither this doc nor §16 originally
  acknowledged (ARCH_REVIEW.md): the peer mesh is built once before the
  event loop and `app` holds no peer receivers at all, and `Group` would
  have to be added to the hardcoded `Core.Supports.Set`.
- **Follow-the-leader movement.** **[peer mesh]** Move the main; the others
  walk the same path. The most tedious part of duoing, and `send_to:`
  (§7.5) is most of the machinery already.
- **"Who needs me?"** **[peer mesh]** Surface the character in trouble —
  pull the eye to the right pane before a word has been read. Related:
  UX_REVIEW.md finding H, on the unread badge not carrying per-character
  colour identity.

## Make the grind feel like progress

MUDs are enormously repetitive; reframing repetition as accumulation is
nearly free.

- **Session stats.** **[line type]** XP/hour, kills, gold, deaths — a quiet
  end-of-session card. Grinders love this for the same reason fitness
  trackers work.
- **Personal bests.** **[line type]** "Best XP hour yet." Not achievements —
  just noticing.
- **Death replay.** **[line type]** The 20 lines before you died, kept.
  Turns every death from a mystery into a lesson.

## Trigger-writing as a craft

Players are genuinely proud of their setups. Support it as a hobby, not a
config chore. Serves actors 2 and 5.

- **A trigger test bench.** Pick or paste a line; see live which rules
  fire, in what order, and what they'd send. Debugging automation currently
  means playing until it maybe happens. Natural companion to `Alt+V`'s
  scrollback-to-trigger picker (§10.2), and to UX_REVIEW.md suggestion E.
  Needs a dry-run `process_line` and a reply path on `SessionCommand`,
  which is currently fire-and-forget (ARCH_REVIEW.md) — the editor's
  existing validator compiles a *fresh* engine, so it can't evaluate
  `when:` guards against live state.
- **Fire counts.** A quiet number per rule — fired 41× this session, or 0×.
  Instantly exposes the clever rule that's been silently broken for a week.
- **Triggers that suggest themselves.** Noticing you've typed `get all
  corpse` after `is DEAD` five times and *offering* the trigger. Delightful
  when it asks; obnoxious if it ever assumes.
- **Module sharing** — already §16, gated on the §7.4 sandboxing that has
  now largely landed.

## Belonging and ritual

The unglamorous things players are actually sentimental about.

- **Friends online.** Notify when someone you care about logs in. MUDs are
  social clients wearing a game's clothes.
- **RP transcript export.** Roleplayers keep and hand-clean logs
  religiously for forums. Export a scene as readable, timestamped,
  colour-preserved text and you own that community. Disk logging (§"Saving
  a transcript") is the raw material; this is the presentation layer.
- **Bookmark a moment.** Mark the line where something great happened; jump
  back later. Serves RPers and raid leaders equally.
- **Sound packs (MSP)** — already §16 as a `proto` module. Divisive, and
  beloved by the half that loves it.

## Accessibility as joy, not compliance

Worth its own section: **MUDs are among the most-played genres by blind
gamers**, precisely because they're pure text — and most modern clients
serve them badly, leaving a large community on decades-old software.

- Screen-reader output that announces *new* lines rather than the whole
  reflowed pane on every render.
- "Speak only this channel," reusing channel panes' existing routing.
- Done well, this isn't a checkbox — it's the one item on this page that
  could win a whole community rather than incrementally please the current
  one.

## Small things with outsized affection

- **Lag indicator.** Players are obsessive about latency, and it's already
  measured for the pane title.
- **Smart paste.** A multi-line paste asks *"send as 12 commands?"* instead
  of flooding the MUD and getting you kicked.
- **"You're in combat — really quit?"**
- **Command palette.** Fuzzy-find any command, setting, or profile; makes
  the client discoverable without memorizing keys. Would also soften
  UX_REVIEW.md suggestion C (nothing advertises that `F1` exists).
- **Typo grace.** `nrth` → *did you mean north?* A nudge, never an
  autocorrect — sending the wrong command in combat is worse than sending
  none.

## If only three

*Revised after `ARCH_REVIEW.md`.* The original ordering here ranked ideas
by joy-per-effort while quietly assuming each was independently buildable.
Two of the three weren't: the split-scroll strip collides with NAWS, and
the party HUD needs both a dynamic peer mesh and a `Core.Supports.Set`
change. Corrected ordering, dependencies first:

1. **The scrollback line type.** Not a feature and not joyful on its own —
   but it's one struct at one choke point today, it unblocks eight of the
   ideas above, and after 1.0 it's the on-disk log format too. Everything
   else in this list gets cheaper behind it.
2. **Corpse run** (once §16's mapper lands) — still the best pure-joy item
   here. Turns MUDing's most miserable ritual into a non-event for a
   fraction of the mapper's own cost, and it's the one top pick with no
   newly-discovered blocker.
3. **Party HUD** — serves the top-ranked actor and nobody else does it
   well, but it is now correctly understood as *three* pieces of work
   (dynamic peer mesh, `app` joining that mesh, `Group` in Supports), not
   one. Worth it; just not the quick win it was first billed as.

**Split-scroll** drops off the top three, not because it's less valuable —
it's still the most universal pain on the page — but because the honest
version needs a viewport/NAWS separation that nothing has specced yet.
Spec that first, then it's a strong candidate again.

Accessibility remains the one to weigh on separate grounds: not the biggest
joy-per-hour for the current audience, but the only item here that could
bring in an audience that doesn't have a good option today. It also wants
the line type (announcing *new* lines needs per-line identity), which is
one more reason that's item 1.
