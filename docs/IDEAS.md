# Ideas

A wishlist, not a roadmap and not design authority — `ARCHITECTURE.md` is
both of those. Nothing here is committed to, sized, or necessarily a good
idea; it's a place to keep thoughts about what would make Mudular a joy to
play with rather than merely functional, so they aren't lost between
milestones. Scope discipline still applies to code: writing an idea down
here is explicitly *not* permission to build it.

Where an idea already has a home in `ARCHITECTURE.md` §16 (Designed-For
Extensions), it's cross-referenced rather than restated.

The organizing thought: in a MUD, the client *is* the game. There's no 3D
world to look at — just the player's instrument. Joy comes from it feeling
like an extension of their hands.

Actor references are to `ACTORS.md`.

## Never lose the thread

The most universal MUD frustration: something good happens while you're
looking somewhere else.

- **Scroll without going deaf.** Scrolling back currently cuts you off from
  live action. Instead, `PgUp` splits the pane — history above, a live 3–5
  line strip still running at the bottom — so reading and being present
  stop being mutually exclusive. Pure `ui` work over buffers that already
  exist.
- **"While you were away."** A foldable digest after an idle stretch: *3
  tells from Bob, 1 group invite, attacked twice*. Dismissible in one key.
- **Tells as conversations.** Thread tells per-person instead of scattering
  them through combat spam. Channel panes (§11.1) already solve the
  "pull it out of the main buffer" half; this is the "and keep it
  coherent" half.
- **Scrollback search.** Incremental, highlight-in-place. Unglamorous and
  conspicuously missing from most clients.

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

- **Party HUD.** One strip showing every character's vitals side by side,
  from GMCP already being parsed. Multiboxers currently do this in their
  head, across panes.
- **Follow-the-leader movement.** Move the main; the others walk the same
  path. The most tedious part of duoing, and `send_to:` (§7.5) is most of
  the machinery already.
- **"Who needs me?"** Surface the character in trouble — pull the eye to
  the right pane before a word has been read. Related: UX_REVIEW.md finding
  H, on the unread badge not carrying per-character colour identity.

## Make the grind feel like progress

MUDs are enormously repetitive; reframing repetition as accumulation is
nearly free.

- **Session stats.** XP/hour, kills, gold, deaths — a quiet end-of-session
  card. Grinders love this for the same reason fitness trackers work.
- **Personal bests.** "Best XP hour yet." Not achievements — just noticing.
- **Death replay.** The 20 lines before you died, kept. Turns every death
  from a mystery into a lesson.

## Trigger-writing as a craft

Players are genuinely proud of their setups. Support it as a hobby, not a
config chore. Serves actors 2 and 5.

- **A trigger test bench.** Pick or paste a line; see live which rules
  fire, in what order, and what they'd send. Debugging automation currently
  means playing until it maybe happens. Natural companion to `Alt+V`'s
  scrollback-to-trigger picker (§10.2), and to UX_REVIEW.md suggestion E.
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

1. **Split-scroll live strip** — fixes a decades-old universal pain, and
   it's `ui`-layer work over buffers that already exist.
2. **Corpse run** (once the §16 mapper lands) — turns the worst moment in
   MUDing into a delight, for a fraction of the mapper's own cost.
3. **Party HUD** — serves the top-ranked actor, uses GMCP data already
   parsed, and nobody else does it well.

Accessibility is the one to weigh on separate grounds: not the biggest
joy-per-hour for the current audience, but the only item here that could
bring in an audience that doesn't have a good option today.
