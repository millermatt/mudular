# The line-oriented output path — design

Status: approved, not yet built. Tracks [#39](https://github.com/millermatt/mudular/issues/39).
Design authority remains `docs/ARCHITECTURE.md`; this records one decision in
enough detail to implement, and folds into the TAD once it is built.

## 1. What this is

A second front end. Mudular presents a session through `ui` today — ratatui,
the alternate screen, raw mode, panes and columns. This adds a second
presentation that shares the model, the pipeline, the rules and the sessions,
and shares none of the drawing: output is a stream of lines appended to an
ordinary terminal, which keeps its own scrollback.

§4.1 records the seam this needs and states that it is internal and
unversioned. This is the second consumer that section says makes the model
falsifiable.

## 2. Who it is for, and what actually goes wrong today

The player using a screen reader (`docs/ACTORS.md`).

The behaviour below was measured on 2026-08-23 against Orca 50.2 on
Wayland/GNOME, with a synthetic MUD emitting one line every 400ms, in two
configurations: the current TUI, and a prototype of the mode this document
designs. It replaces an earlier account of this section that reasoned from
the alternate screen and was wrong.

**What is not the problem.** Orca announces new lines in both
configurations. It does not sit re-reading a repainted rectangle. The
alternate screen is not, by itself, what breaks this: a scroll-region
renderer inside the alternate screen introduces two changed rows per arriving
line, the same as an append-only one, and Blightmud ships exactly that
(§9.2).

**What is the problem, in two parts.**

*Speech is a hard-bounded channel and the client's output is not.* Synthesis
runs at roughly 150–200 wpm. A line of MUD prose takes two or three seconds
to speak and can arrive every few hundred milliseconds, so the queue grows
without bound and the player falls progressively further behind whatever is
happening. This is the dominant failure mode and it is present in **both**
configurations. Any design here that does not have an answer to it is
unusable at exactly the moment it matters, which is combat.

*The player's own flush already exists — but it lands somewhere different in
each configuration.* Any keypress makes Orca abandon its backlog and catch
up. In the prototype it catches up to **the newest line**. In the current TUI
it catches up to **the top of the visible pane**, so the player then sits
through everything on screen again before reaching what just happened.

That difference is the case for this work. It is not "the client repaints too
much" — it is that after an interruption, one shape of output puts the player
at the newest line and the other puts them a screenful behind it.

**What this buys, beyond the announcement itself.** Reading, review,
interruption, braille and echo policy stay with the tool the player already
has and has configured. A braille display is reached for free, which no
speech-owning design manages. And a terminal client runs over SSH, in WSL and
in containers, where there is no session bus, no audio device and no speech
server — a screen reader reading the local terminal works in all of those,
and a client-owned voice is silent in all of them. For this class of client
that is a large share of the user base, and it is the strongest single
argument for delegating rather than speaking.

**A second player is served incidentally**, and is worth naming because it
decides the flag's name: anyone piping the client's output somewhere, or
driving it from a script, wants the same mode.

## 3. Scope

**In:** a startup mode that never enters the alternate screen; a shared
resolution layer so a configured keypress reaches the same named intent in
either front end; several characters connected with one printed at a time;
spoken refusals where a surface cannot degrade.

**Out, and deliberately:** all speech synthesis; any client-originated
interruption; braille, sound cues, stereo panning; a first-party reading
cursor; making the existing panes readable; a runtime toggle between front
ends. Each is a separate decision with its own evidence, and none is blocked
by this work. The MTTS screen-reader bit is
[#145](https://github.com/millermatt/mudular/issues/145) and stays there —
but see the objection below before building it.

**#145 may be the wrong thing to build, and all three plans agreed on it
without examining it.** MTTS bit 64 advertises "a screen reader is in use"
to the server, unprompted. Mudlet's accessibility manual calls exactly this
out as bad practice:

> "Auto-detection of visually impaired players is frowned upon, as it could
> lead to 'fingerprinting' certain users without their consent. Instead, the
> best practice is to provide a command for the players that enables a
> screenreader-friendly mode."

Disclosing a disability to every server you connect to, by default, is a
privacy decision made on the player's behalf. It is also §13's kind of
question rather than a protocol detail. #145 was ranked "cheapest, ships
first" by all three plans in #39; none of them weighed this. At minimum it
is off by default and opt-in per profile; possibly it should not exist, and
the player should type the MUD's own screen-reader command instead. Recorded
here because #145's issue does not know it yet.

Interruption is left to the player's screen reader because the measurement in
§2 shows it already works: any keypress flushes the backlog and, in this
front end, lands on the newest line. Building a second interrupt would
duplicate one the player has, and a client that owns the voice can take away
the one they had — which is what happened to Blightmud's users.

## 4. Two branches, in order

The work is two sentences, so it is two branches (CLAUDE.md).

1. **The shared intent layer.** A configured keypress resolves to a named
   intent, and `ui` carries it out by that name. No behaviour changes and no
   new configuration.
2. **The line front end.** Built on that layer, adds `--line`. This branch
   also amends `ARCHITECTURE.md` §4.1 (§9.1) and extracts the shared startup
   described in §7.1.

Branch 1's acceptance is "no behaviour change", not "no test edited" — an
earlier draft said the latter and §5.5 records why it is unachievable.

## 5. Branch 1 — the shared intent layer

### 5.1 What already exists

`ClientCommand` (`src/state.rs`) is already the shared vocabulary for two of
the three ways a player asks for something: `ClientCommand::parse` turns a
typed `/reload` into `ClientCommand::Reload`, and the command palette holds
`PaletteEntry::Command(ClientCommand)`. Keybinds never joined —
`app::handle_key` matches twenty `keybinds.<name>.matches(code, modifiers)`
arms in sequence and acts on the model directly.

**Three of the twenty duplicate a `ClientCommand`'s effect**: `toggle_map`
(`/map`), `toggle_channels` (`/comms`) and `config_editor` (`/config`). Two
others look like duplicates and are not, which matters for §5.5:

- `reload` sets `state.reload_requested` for the event loop to service
  asynchronously; `/reload` awaits `reload_rules` directly.
- `help` opens an overlay (`state.show_help = true`); `/help` prints
  `ui::help_lines` into the pane. Different effects, both pinned by tests.

### 5.2 What is shared, and the test for it

An intent enters the shared vocabulary only if **both** of these hold:

1. A front end with no panes, no columns and no cursor addressing can carry
   it out — possibly by entirely different means.
2. That front end can *plausibly one day realise it*, rather than only refuse
   it politely.

The second clause is not decoration. Without it, "prints a refusal" (§8.4)
makes every binding pass, including `swap_columns`, and the vocabulary stops
meaning anything.

"By different means" is what makes the layer worth having. `toggle_map` in
`ui` shows a column; in line mode the same intent prints the room in prose,
which `Map::describe` already produces and `describe_current_room` already
calls on every toggle (§16 keeps the map's knowledge in prose form for
exactly this reason). Same intent, two realisations, one name.

A binding fails when its subject *is* the geometry: a width, a layout, which
pane has focus, or a cursor moving over drawn content.

Classification is against the code, not any list — the criterion above is
normative. Applied to every binding, it gives **12 shared, 8 private**:

| Binding | | Binding |
| --- | --- | --- |
| `quit` (shared, deferred to branch 2 — §5.3) | | `swap_columns` (private) |
| `server_data_inspector` | | `cycle_layout` (private) |
| `toggle_channels` | | `channel_wider` (private) |
| `palette` | | `channel_narrower` (private) |
| `help` | | `map_wider` (private) |
| `config_editor` | | `map_narrower` (private) |
| `line_picker` | | `map_cursor` (private) |
| `reload` | | `focus_next` (private) |
| `toggle_map` | | |
| `toggle_hud` | | |
| `toggle_timestamps` | | |
| `who_needs_me` | | |

The right column is exactly the bindings whose subject is a width, a layout,
or a cursor over drawn content — geometry the criterion excludes by
construction. Everything else is shared. `quit` is shared by the criterion
(any front end can quit) but is matched in `event_loop`, not `handle_key`,
and stays there until branch 2 gives line mode a loop to join.

Four cases are worth recording because they are contested or were previously
filed wrongly:

- **`focus_next` is private.** Its name suggests a character switch and it
  is not one. `AppState::focus_next` walks sessions, then any visible
  channel panes, then the map column, and what it moves is which pane the
  scroll and arrow keys act on — `self.focus`, not `input_session`. Focus
  and input binding are separate in this model: which character the input
  line is bound to is `input_session`, driven by `Alt+1..9`, which is not a
  binding at all yet (§5.4). So a front end with no panes has nothing for
  `focus_next` to walk, and the intent that line mode actually needs from
  this area is the one `Alt+1..9` carries.
- **`toggle_hud` is shared.** Its subject is every character's vitals, not
  geometry; a line front end prints it as lines. **Not as a table** —
  Mudlet's accessibility manual is explicit that tabular layout "can often
  appear as streams of indecipherable data" because screen readers need
  markup to walk cells and plain text has none, and advises breaking the
  information into lines separated by commas or colons. Filing it private while
  `who_needs_me` — which reads the same data — is shared, is not a
  distinction this criterion supports, and the blind multi-boxer wants the
  table most.
- **`line_picker` is shared.** Its subject is turning a retained line into a
  trigger pattern, and §8 records that it deliberately matches
  `RetainedLine::plain`, the stored projection, not anything drawn. "Pick one
  of the last N lines by number" is a textbook different means.
- **`map_cursor` stays private**, because stepping between *drawn* rooms
  needs renderer geometry. This owes §16 an answer: that section says "a
  cursor is also the only form of this a screen reader can follow." That
  sentence is about a TUI affordance, and line mode's answer to it is
  `Map::describe` plus a future "describe the room north", not a cursor.
  §9.1 records the amendment.

### 5.3 Shape, and where it lives

An `Action` enum above `ClientCommand` rather than more variants inside it.
`ClientCommand` means "a thing you can type with a slash and find in the
palette" — `parse` deliberately leaves an unknown `/word` to the MUD, and
`name`/`describes` feed the palette listing and `every_command_is_in_the_palette`.
Widening it would need a hidden-entry concept to suppress the additions again.

**`Action` lives in `state`, beside `ClientCommand`, and so do its
resolvers.** They must not be spelled as methods on `Keybinds`: `Keybinds`
lives in `config`, `state` names `config`, and `config` does not name
`state`. A `Keybinds::resolve -> Option<Action>` therefore adds a `config` →
`state` edge and reopens the module cycle that #6 closed. Rust permits the
inherent `impl` to be written inside `src/state.rs`, where it compiles and
passes both boundary tests — neither guards `config` ↔ `state` — so the cycle
would return silently. Spell them, one per gap §5.5 names:

```
Action::for_key_before_modes(&Keybinds, code, modifiers) -> Option<Action>
Action::for_key_before_errors(&Keybinds, code, modifiers) -> Option<Action>
Action::for_key_after_errors(&Keybinds, code, modifiers) -> Option<Action>
```

`crossterm`'s `KeyCode`/`KeyModifiers` are already imported in both modules,
so nothing new crosses.

**Carrying an action out has three outcomes, not two:**

```
Handled | Declined(reason) | Exit
```

`Exit` exists because `quit` is shared and cannot be expressed otherwise: it
is matched in `event_loop` rather than `handle_key`, and it saves maps and
comms panes before leaving the loop.

`Declined` is what makes a gap visible: a front end that cannot do something
says so in words (§8.4) rather than doing nothing.

### 5.4 What the layer does not touch

**Modal owners stay in `ui`.** Roughly half of `handle_key` is the password
offer, the mark chooser, the profile editor, the new-profile form, the map
cursor, the scrollback line cursor, the help overlay and the palette. Each
takes the keyboard while it is up and returns early. They are not bindings
and do not become `Action`s.

**Deferred-flag bindings keep their flags.** `reload`, the config editor's
save, and the map cursor's walk all set a field that the event loop services
asynchronously afterwards, because `handle_key` is sync and the work is not.
`Action` names the intent; it does not change who performs it. A second front
end therefore reproduces the post-key servicing block as well as the
dispatch, and §7.1 extracts it rather than copying it.

**`Alt+1..9` is not reachable and must be made so.** The session jump is
hardcoded in `handle_key` and is not a `KeyBinding` at all, so it is not
remappable and none of the resolvers can see it. It is also the one intent
branch 2 most needs. Either it becomes a real binding or the character-switch
intent gets its own name; the former is preferable and is a small,
independently sensible fix.

### 5.5 Order is behaviour, and a single lookup will break it

The shared bindings are **not contiguous**. In `handle_key` they interleave
with the modal blocks, and that ordering is load-bearing: `toggle_timestamps`
is checked *before* the help overlay's block, so it works while the overlay
is up, and the palette key while the overlay is up dismisses the overlay
rather than opening the palette.

The general rule: **a shared binding resolves in the gap it already
occupied, and every modal block between gaps is a divider.** A single
`Action::for_key` call at the top of the binding section collapses every gap
into one and inverts every ordering `handle_key` currently depends on, so the
layer needs one resolver per gap, not one for the whole function.

The help overlay is the divider that motivates the rule; the errors panel is
the one that proves it needs to be general rather than "the help overlay and
everything else." `show_errors` does not just gate a block the way the
overlay does — it clears the errors panel and then recursively re-dispatches
the same key, so a binding resolved *after* that block closes the panel as a
side effect of running, while a binding resolved *before* it leaves the panel
up. `palette` is on the "leaves it up" side; the other eight shared bindings
are not. That difference is invisible unless the block that causes it is
named as a divider on its own terms, which is why treating dividers as "the
overlay, plus a residual second lookup" would have missed it.

Three resolvers follow the three gaps: `Action::for_key_before_modes`, for
`toggle_timestamps`, `toggle_hud` and `who_needs_me`, which is checked first
among the overlay blocks — after the mark menu, the config editor and the
new-profile wizard, each of which already owns the keyboard outright while
open, but before the help overlay and the errors panel; `Action::for_key_before_errors`,
which holds only `palette`, checked after the earlier modal blocks but before
the errors panel's own block, so the palette key while the panel is up opens
the palette and leaves the panel exactly as it was, with no clear-and-re-dispatch
in between; and `Action::for_key_after_errors`, for the remaining seven —
`help`, `config_editor`, `toggle_map`, `reload`, `line_picker`,
`server_data_inspector` and `toggle_channels` — each of which
only runs after the errors panel has already cleared and re-dispatched the
key, so an open panel takes their key first.

The rule above — a shared binding resolves in the gap it already
occupied — governs *shared* bindings; it says nothing about the seven
geometry bindings, because they are not in the layer at all. In `handle_key`
they are checked after all three resolvers, `swap_columns` and `cycle_layout`
where `for_key_after_errors` used to be interleaved with them at the branch's
base, the four resize keys and `map_cursor` where they already were. A
geometry binding therefore now loses to any shared binding it happens to
collide with, where at the base it could win one of two gaps depending on
which shared binding it was interleaved with. With the default binds nothing
collides, so no default keypress resolves differently than before. The only
configuration this is observable in is one binding a single key to both a
shared action and a geometry action — `Keybinds` validates neither collisions
nor duplicates, so nothing stops it — and that binding has no defined winner
in either arrangement: the base ordering only happened to pick one because
of where in `handle_key` each arm was written, not because either arm's
intent should win. Restoring the old interleaving would need a resolver
seam for the geometry bindings too, to serve a misconfiguration that was
never a supported case. It stays unrestored.

### 5.6 Done when

- Every shared binding reaches its effect through one of the three resolvers
  §5.5 names, at the gap it occupied.
- The three genuine duplicates have one implementation.
- A test asserts that resolving a key and parsing the equivalent
  slash-command produce the same `Action`, for those three.
- A test asserts the orderings §5.5 names, which are currently unguarded.
- Behaviour is unchanged. Tests may be edited only where they name a
  mechanism that moved; `the_reload_keybind_requests_a_reload` is expected to
  be one, since it asserts the deferred flag by name.

## 6. Branch 2 — the line front end

### 6.1 Entry

`--line`, with `--screen-reader` as an alias. The mechanism is the real name
because the mode is not only for one audience; the alias is what the person
searching `--help` or the README for accessibility will find. No runtime
toggle: the two front ends set the terminal up differently.

**The first-run wizard is on this path and must be handled.**
`app::run_new_profile_wizard` calls `ratatui::init()` before `run` is ever
reached, so `--line` on a fresh config directory enters the alternate screen
before anything else happens. It either refuses with an instruction, or gets
a line-mode form. Refusing is acceptable for a first release; silently
entering the alternate screen is not.

### 6.2 Output

Output lines are appended and no line already written is revisited.
Concretely: the alternate screen is never entered, and each output line is
written exactly once.

**The input line is the one exception, and it constrains the implementation.**
Raw mode makes drawing it the client's job (§6.3). It sits below everything
printed and is not transcript until sent, so rewriting it in place disturbs
nothing a reader is moving over. It must be redrawn with `\r` and `ESC[K`
only — **never cursor addressing**. Measurement confirms a prototype doing
this emits zero `CUP` sequences, which is what makes the §6.7 assertion
writable at all; a scroll region or an absolute cursor move would both break
it, and a scroll region additionally costs the terminal's own scrollback.

**Three sources of output do not pass through `push_line`, and all three need
answering:**

- **Channel-routed lines.** `SessionEvent::Route` goes to `push_routed`,
  which appends to a `ChannelPane` and deliberately *moves the line out of
  the session's scrollback*. With channel panes unavailable, every tell and
  channel message would silently vanish. This is output disappearing, not a
  surface refused, and it is the most user-visible hole in this design. Line
  mode prints routed lines inline, tagged with the channel.
- **The MUD prompt.** `SessionEvent::Prompt` sets `view.prompt` and never
  reaches scrollback. It is also what a player reads most, and it changes in
  place. Printing every update as a new line would flood the queue §2 names
  as the dominant failure. First release: print the prompt only when it
  changes *and* nothing else has been printed since, and make it suppressible.
- **Client notices with no bound session** (`shell_notices`) and kept
  warnings (`record_warning`).

**Provenance is composed, not spliced.** `RetainedLine` carries `origin`
(five variants, including `Rule` for the player's own automation), so line
mode composes any prefix from it the way `ui::draw_channel` does, rather than
inventing a second convention. Note that `Origin::Session` is a
`Vec<String>`, plural on purpose, because §11.1 collapses one broadcast heard
by three characters into a single entry naming all three — so per-character
attribution is undefined in exactly the multi-boxing case, and the prefix
must render the whole list rather than assume one name.

### 6.3 Input

Raw mode, without the alternate screen. The two are separate switches. Raw
mode is what lets a configured keypress reach the layer branch 1 builds, and
the §2 measurement found no cost to it: the prototype used raw mode and was
the configuration where a flush lands on the newest line.

This also follows precedent, though not in the way an earlier draft claimed:
Blightmud's reader mode uses raw mode **and** the alternate screen, with a
scroll region and minimal per-line writes (§9.2). What it demonstrates is
that raw mode is compatible with a screen-reader-friendly mode, not that the
alternate screen must be avoided.

A screen reader's own review keys are not at risk. For `tdsr` this is
structural and verified: it wraps the program in a pseudo-terminal, matches
its own `Esc`-prefixed keymap on stdin, and passes everything else through.
NVDA and Orca do not work that way — they read the terminal's *contents*
through the OS or AT-SPI and capture their keys with a system-level hook, so
they intercept above the application rather than above the terminal. The
practical conclusion is the same and the §2 measurement confirms it on Orca;
the mechanism differs, and an earlier draft conflated them.

**Caveat worth carrying:** the Orca path depends on the terminal emulator's
own accessibility, and that is not uniform. GTK4/VTE and out-of-tree text
widgets are incompletely covered. "Delegate to the platform" is right, and it
is not free everywhere.

### 6.4 Echo, and not moving the reader

Two different things are called echo and only one is a setting.

**Keystroke echo** is not optional: raw mode means the terminal no longer
shows what is typed. It must also honour `view.masked` — see §6.6.

**Sent-command echo** — printing the finished command back into the output
stream — is **off by default**, following Blightmud, which shipped exactly
this for its reader mode on the reasoning that there is no interest in
hearing back what you just sent. Blightmud's version is unconditional; this
one is a setting, because #39's evidence has blind users contradicting each
other about being moved by new text.

Blightmud's third option is better than either branch and is adopted:
**suppressed from the live stream, retained in scrollback**, so it is there
on review without being spoken on arrival.

Both branches ship implemented and tested, but the default is no longer
unfounded. Mudlet's accessibility manual instructs screen-reader users to
configure exactly this, in the client blind testers signed off on:

> "Auto clear the input line after you sent text", should be checked.
> "Show the text you sent", should be unchecked.

Two independent clients, then, arriving at the same default — which is what
§10 asks for before a default is chosen.

The auto-clear half is worth adopting too: mudular keeps the sent command
selected in the input line, and Mudlet names that as a sighted-user
convenience that costs a screen-reader user. Line mode clears on send.

### 6.5 Several characters, one printed

`--line` accepts several profiles, as the TUI does. Every session connects
and fills its scrollback; only the bound one prints. Switching prints a
marker naming the character and begins printing that character's lines.

Interleaving every session into one stream is what a blind multi-boxer would
eventually want, and is not this release: two characters' output interleaved
with no spatial separation is where this becomes hard to read, and given §2's
queue constraint, N sessions serialised into one voice is where it becomes
unusable. Nothing here forecloses it.

### 6.6 Masked input is this front end's own responsibility

`push_line` does **not** keep passwords out of scrollback and the transcript,
despite a comment nearby that reads as if it does. The guard is at the call
site in `submit_input`, which declines to echo when `view.masked` is set.

Since §6.4 makes keystroke echo the client's job, **line mode must consult
`view.masked` itself** — set by `SessionEvent::EchoMask` — and stop echoing
while it holds. Nothing inherits this. It is the one place in this design
where getting it wrong prints a password to the terminal, so it gets its own
assertion in §6.7 and its own test at a real masked prompt.

### 6.7 Refusals are spoken, and typed commands need their own path

A surface that cannot degrade prints a named refusal rather than doing
nothing. Silence is what teaches a blind player the client is broken.

`Declined(reason)` covers keypresses. It does **not** cover typed commands:
`/config` reaches `run_client_command` straight from `submit_input` and never
passes through `Action`. Refusals therefore need a front-end capability the
command dispatch can consult, for at least `/config`, `/errors`,
`/newprofile` and `/map`. This is a change to shared code and is part of
branch 2's cost.

### 6.8 Done when

A pty test drives the real binary under `--line` and asserts:

- no alternate-screen sequence is ever sent — `ESC[?1049h`, `ESC[?1047h` or
  `ESC[?47h` — at any point, including the first-run path;
- no cursor-addressing sequence (`ESC[…H` / `ESC[…f`) is ever sent;
- each output line appears exactly once and contiguously;
- a masked password never appears in the output, driven against a real masked
  prompt;
- a channel-routed line is printed inline rather than vanishing;
- a declined action prints its refusal;
- switching character prints the marker and then that character's lines.

The harness (`tests/pty_smoke.rs`) needs a raw-output accessor: it keeps the
byte stream but exposes only escape-stripped views. Two cautions carried from
CLAUDE.md and from the harness's own history: terminal sizes must be derived
rather than hand-picked, and the existing `30×100` constant is exactly the
hand-tuned value that rule warns about; and `Esc`-prefixed keys go through
`press_esc`, which matters because a character-switch test wants `Alt+2`, a
key this suite has had platform trouble with before.

## 7. Cost this design does not hide

### 7.1 The startup path must be extracted before it is copied

Roughly 215 lines of `event_loop` are peer-mesh setup, session construction,
the `AppState` literal and the saved-layout load, all inline. A line-mode
loop needs every one of them and none of the drawing.

Extracted first, the new loop is roughly 200–280 lines. Copied instead, it is
380–450 **and there are two copies of the `AppState` literal**, which is how
the next field added gets initialised in one front end and not the other.
The extraction is a behaviour-preserving refactor and belongs at the head of
branch 2.

### 7.2 The test matrix grows

Every future visual feature acquires a second question. That is the standing
cost of a second front end and it is accepted, not avoided.

## 8. What this does not achieve

Stated plainly so the gaps are arguable rather than discovered.

**8.1 Skipping and re-reading arriving text.** The most consistently
requested capability across twenty years of blind MUD players. §2 answers
half of it: the player's own flush works and lands on the newest line. It
does not establish that their review cursor lets them go *back* over arriving
text. That remains the question to put to a blind developer — now two
questions, the second of which (§8.1a) may matter more:

> 1. Over a normal scrolling terminal with no repaint, does your own screen
>    reader's review cursor already give you "skip a few boring lines, or
>    re-read that line"?
> 2. Is there any way for a terminal application to tell your screen reader
>    that a group of lines is one unit — a room description, a combat round
>    — so that review moves by chunk rather than by line? Do blank lines
>    achieve this in practice?

**8.1a Chunking — telling the reader that lines belong together.** Review
moves a line at a time, but a MUD's output is not a sequence of independent
lines: a room description is one thing, a combat round is one thing, a tell
is one thing. Being able to step over a chunk, or back into one, is a
different and probably more valuable capability than stepping over a line,
and nothing in this design offers it.

No standard mechanism carries chunk boundaries to a screen reader, and the
obvious workaround is contraindicated. What the field does instead is not
markup at all.

- **Blank lines do not work, and are actively avoided.** The natural idea is
  to separate chunks with a blank line and let paragraph navigation find
  them. Mudlet ships a *Blank Lines* setting whose options are show, hide,
  or replace with a space, and its accessibility manual recommends one of
  the latter two, because "having blank lines can be problematic on Windows
  specifically." A client written for these users offers to remove them.
  Line mode should not add them, and this document previously said it
  should.
- **`OSC 133`.** The FinalTerm shell-integration sequences mark prompt,
  command and output boundaries, and Ghostty, iTerm2, kitty, WezTerm and VS
  Code already implement block-jumping on them. It is a real chunking
  protocol and nearly free to emit. But it drives the *terminal's* own
  navigation, and no evidence was found that any screen reader consumes it,
  so it must not be described as an accessibility feature.
- **Keyed recall of categorised chunks — this is what actually ships.**
  Two Mudlet packages and a MushClient plugin independently implement the
  same model, described as "virtual buffering for screen reader users",
  and its stated premise is blunt: *traditional scrollback is inaccessible*.
  Rather than teach the reader what a chunk is, the client keeps chunks in
  named categories and gives each a key.

  `ChannelHistory` assigns every stored message a category and reviews them
  with `Alt+Left`/`Alt+Right` between categories, `Alt+Up`/`Alt+Down` within
  one, `Alt+Home`/`Alt+End` for first and last, `Alt+1`–`Alt+0` for the ten
  most recent, and `Alt+Shift+T` for timestamps. `QuickOutput` does the
  simpler half: read the ten most recent lines from the MUD. MushClient's
  `channel_history` is the same idea again.

  **Mudular is unusually close to this.** Channel routing (§11.1) already
  does what ChannelHistory asks trigger authors to write by hand, `Origin`
  already tags provenance, and `toggle_timestamps` already exists and maps
  onto their `Alt+Shift+T`. The categories are built; they are currently
  rendered as a drawn column instead of bound to keys.

  **Split out as [#149](https://github.com/millermatt/mudular/issues/149),
  deliberately not in branch 2.** Recall is not line-mode-specific — by
  §5.2's own criterion it is a shared intent both front ends can realise, so
  building it inside the line front end would make it line-mode-only, which
  is the mistake the intent layer exists to prevent. It also collides with
  `Alt+1..9`, which §5.4 records is not yet a real binding, and that is worth
  resolving deliberately rather than under the weight of a larger branch.

  Branch 2 therefore ships only the minimum that is not a hole: routed lines
  print inline with a channel tag, so nothing vanishes (§6.2). That is
  strictly worse than recall — it pushes tells into the queue §2 names as
  the dominant failure — and it is the honest interim, not the answer.

- **A first-party review cursor.** §3 refuses this, and keyed recall is why
  the refusal survives: it is the capability, reached another way.

**What a terminal cannot copy, and should admit.** Mudlet's own answer to
re-reading is to make the output pane focusable — `Ctrl+Tab` or `F6` moves
focus off the input line and into the buffer, where ordinary cursor keys
review by character, word and line, `Ctrl+Home`/`Ctrl+End` jump to the start
of the buffer or the latest content, and the screen reader announces
"Jumped to latest content" as it lands. That works because Mudlet is a GUI
application with a real accessible text widget to hand focus to. A terminal
application has no such object; the terminal emulator owns the only one. So
this specific solution is unavailable here, and it is the clearest
disadvantage of a terminal client for this player. Keyed recall is the
compensating mechanism, not a preference.

**On the client pushing text rather than being read.** Mudlet's "Announce
incoming text in screen reader" is on by default, and turning it *off* is
what its manual recommends on macOS and Linux. That is the delegated-push
path — the client calling the screen reader's own API — which is #39's
later layer and not what this design does. Worth knowing that the most
accessible MUD client does not rely on the reader noticing changed text.

The part that is already done: **mudular knows where the chunks are.** GMCP
`Room.Info` says which lines are a room description, channel routing says
which are a tell, `Origin` separates server text from a trigger's echo and a
client notice, and any trigger marks whatever a player taught it to
recognise. The semantics are in the model; only the expression channel is
missing. Whichever mechanism turns out to work, the expensive half exists.

**8.2 Bounding the speech queue.** §2 identifies the backlog as the dominant
failure mode and this design does nothing about it beyond preserving the
player's own flush. That may be enough — the flush is one keypress and blind
players use it constantly — but it is untested at realistic burst rates, and
"you are forty lines behind" has no expression here.

That the queue is a real and known problem, rather than an artefact of the
§2 measurement, is corroborated by the Mudlet Reader package existing: its
stated purpose includes fixing "some VoiceOver issues regarding queuing
things". It is written by Tyler Spivey, who also wrote `tdsr` — one of the
three developers #39 proposes putting §8.1's questions to, and therefore the
person most likely to already know the answer to this one.

**8.3 Appending on arrival is itself contested.** The one braille user on
record in #39 said the auto-scroll on new text "messes with my ability to
perceive text". Appending is precisely what this design does, and the claim
in §2 that braille is reached for free should be read against that.

**8.4 The client's own surfaces.** The map column, channel panes, party
strip, protocol inspector, unread badges and the settings editor are
unavailable, each a spoken refusal. #39 ranks the client's own UI **fourth**
among what blind players ask for — above most of what this document does
build — so this is a known, ranked gap and not an oversight.

## 9. Amendments this work owes other documents

**9.1 `ARCHITECTURE.md` §4.1** describes the second front end as "no ratatui,
no alternate screen, no raw mode, no keybinds". This design keeps the first
two and reverses the last two, on the §2 measurement. §16's "a cursor is also
the only form of this a screen reader can follow" needs the same treatment
(§5.2). Both edits land with branch 2, when the behaviour exists.

**9.2 Issue #39's research record** contains four claims this work found to
be wrong, worth correcting there so the next reader does not inherit them:

- Blightmud's reader mode does **not** avoid the alternate screen. It uses
  the alternate screen and raw mode, with a scroll region and minimal
  per-line writes. Its own `reader_screen.rs` does not show this; the setup
  is shared in `ui_wrapper.rs`.
- Orca's headless D-Bus name has no digit. `org.gnome.Orca1.Service` does not
  exist on Orca 50.2; the real name is `org.gnome.Orca.Service` at
  `/org/gnome/Orca/Service`. `PresentMessage` on it returns `true` —
  **verified working**, which closes the one unverified Linux claim the
  research flagged as load-bearing.
- **There is no `InterruptSpeech` method** on that interface. It exposes
  `GetVersion`, `ListCommands`, `ListModules`, `PresentMessage`, `Quit` and
  `ShowPreferences`. Any future speech layer must find interruption elsewhere.
- Orca has no `--quit` flag; `Quit` is a D-Bus method.

The record's population evidence should also be read honestly: the only
quantitative datum anyone found, KaVir's 2013 census of God Wars II, is
**GUI-dominated** — of 22 identifiable clients among 98 screen-reader users,
14 were MUSHclient. That is mild evidence against a terminal client being
where these players already are.

## 10. Feedback, if this reaches testers

Adopted whatever else happens, because the failure they prevent is
documented: Mudlet's screen-reader edition stalled with eight blind testers
and contradictory feedback, and no sustained testers afterwards.

- **A defect** — could not complete the task, or something moved their read
  position. One tester is enough. Always fix.
- **A preference** — completed it, would rather it differed. Never becomes a
  default on one voice, and **not on fewer than two independent
  confirmations**. Ships as a setting with both branches implemented and
  tested, or not at all.
- **A mechanism claim** — "my screen reader cannot do X." Not answerable by
  play-testers. Routed to someone who can describe the mechanism. §8.1 and
  §8.2 are both already in this category.

Where blind users contradict each other, the tiebreak is whichever branch
preserves more of the player's own assistive stack.
