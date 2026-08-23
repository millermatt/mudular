# The line-oriented output path — design

Status: approved, not yet built. Tracks [#39](https://github.com/millermatt/mudular/issues/39).
Design authority remains `docs/ARCHITECTURE.md`; this records one decision in
enough detail to implement, and folds into the TAD once it is built.

## 1. What this is

A second front end. Mudular presents a session through `ui` today — ratatui,
the alternate screen, raw mode, panes and columns. This adds a second
presentation that shares the model, the pipeline, the rules and the sessions,
and shares none of the drawing: output is a stream of lines written to an
ordinary terminal, and the terminal keeps its own scrollback.

§4.1 already records the seam this needs and states that it is internal and
unversioned. This is the second consumer that section says makes the model
falsifiable.

## 2. Who it is for

The player using a screen reader (`docs/ACTORS.md`). A screen reader announces
text the terminal reports as *inserted*. A full-screen application in the
alternate screen never inserts anything — it owns every cell and overwrites
them, so the event never fires and there is nothing to announce. That is the
whole obstacle, and it is a property of the alternate screen rather than of
terminals: blind players run shells, `tail -f` and older line-based MUD clients
without difficulty.

Leaving the alternate screen hands reading, review, interruption, braille and
echo policy back to the tool the player already has and has configured. It
reaches a braille display for free, which no speech-owning design does.

A second player is served incidentally and is worth naming because it affects
the flag: anyone who wants to pipe the client's output somewhere, or drive it
from a script, wants the same mode.

## 3. Scope

**In:** a startup mode that never enters the alternate screen; a shared
resolution layer so a configured keypress reaches the same named intent in
either front end; connecting several characters with one printed at a time;
spoken refusals where a surface cannot degrade.

**Out, and deliberately:** all speech synthesis; any client-originated
interruption; braille, sound cues, stereo panning; a first-party reading
cursor; making the existing panes readable; a runtime toggle between front
ends. Each is a separate decision with its own evidence, and none is blocked by
this work. The MTTS screen-reader bit is [#145](https://github.com/millermatt/mudular/issues/145)
and stays there.

Interruption is left to the player's screen reader on purpose. The evidence is
consistent across three clients that players want speech cut off when *they*
send a command and switch it off when it cuts off on arrival — and their own
screen reader already does exactly the former. A client that owned the voice
would have to reimplement an interrupt the player already has, which is how
Blightmud came to take one away.

## 4. Two branches, in order

The work is two sentences, so it is two branches (CLAUDE.md).

1. **The shared intent layer.** A configured keypress resolves to a named
   intent, and `ui` carries it out by that name. No behaviour changes, no new
   surface, no new configuration. Justifiable on its own: it removes the last
   entry point that bypasses `ClientCommand`.
2. **The line front end.** Built on the layer, adds `--line`.

Branch 1 must leave every existing test passing unmodified. A test that has to
change is a behaviour change, which this branch is not.

## 5. Branch 1 — the shared intent layer

### 5.1 What already exists

`ClientCommand` (`src/state.rs`) is already the shared vocabulary for two of the
three ways a player asks for something: `ClientCommand::parse` turns a typed
`/reload` into `ClientCommand::Reload`, and the command palette holds
`PaletteEntry::Command(ClientCommand)`. Keybinds are the entry point that never
joined — `app::handle_key` matches twenty `keybinds.<name>.matches(code,
modifiers)` arms in sequence and acts on the model directly, so five bindings
carry out a `ClientCommand`'s effect in a second place.

### 5.2 What is shared, and the test for it

Only intents that survive the absence of a screen enter the shared vocabulary.
The test is not "does the TUI have a key for it" but:

> Can a front end with no panes, no columns and no cursor addressing carry out
> this intent — possibly by different means?

"By different means" is load-bearing and is what makes the layer worth having.
`toggle_map` in `ui` shows a column; in line mode the same intent prints the
room in prose, which `Map::describe` already produces (§16 keeps the map's
knowledge in prose form for exactly this reason). Same intent, two
realisations, one name.

A binding fails the test when its subject *is* the geometry: a width, a layout,
which pane has focus, or a cursor moving over drawn content. Those never enter
the shared vocabulary, stay in a private tail of `ui`'s key handling, and cost
a second front end nothing.

Provisional classification, to be confirmed against the code while
implementing — the criterion above is what is normative here, not the list:

| Shared | Private to `ui` |
|---|---|
| `quit`, `help`, `reload`, `config_editor`, `palette`, `toggle_map`, `toggle_channels`, `server_data_inspector`, `toggle_timestamps`, `who_needs_me` | `cycle_layout`, `swap_columns`, `channel_wider`, `channel_narrower`, `map_wider`, `map_narrower`, `toggle_hud`, `focus_next`, `map_cursor`, `line_picker` |

### 5.3 Shape

An `Action` enum above `ClientCommand` rather than more variants inside it.
`ClientCommand` means "a thing you can type with a slash and find in the
palette" — `parse` deliberately leaves an unknown `/word` to the MUD, and
`name`/`describes` feed the palette listing. Widening it to hold intents that
are neither typable nor listable would need a hidden-entry concept to suppress
them again.

```
Keybinds::resolve(code, modifiers) -> Option<Action>
```

`Action` carries the shared intents. Those that are already commands wrap
`ClientCommand`; the rest are named directly. Each front end answers whether it
carried the action out, so declining is a value rather than a silent no-op:

```
Handled | Declined(reason)
```

`Declined` is what makes a gap visible. A front end that cannot do something
says so, in words, and a test can assert which actions a front end declines —
which is the mechanism by which parity grows deliberately rather than by
accident.

### 5.4 Modes are not bindings

Roughly half of `handle_key` is modal owners — the password offer, the mark
chooser, the profile editor, the new-profile form, the map cursor, the
scrollback line cursor, the help overlay, the palette. Each takes the keyboard
while it is up and returns early. They are not bindings and do not become
`Action`s. They stay where they are, in `ui`, under both branches.

### 5.5 Done when

- Every shared binding reaches its effect through `Keybinds::resolve` and one
  `apply`, and no shared binding is still matched inline.
- The five bindings that duplicated a `ClientCommand` have one implementation.
- A test asserts that resolving a key and parsing the equivalent slash-command
  produce the same `Action`.
- The existing suite passes with no test edited.

## 6. Branch 2 — the line front end

### 6.1 Entry

`--line`, with `--screen-reader` as an alias. The mechanism is the real name
because the mode is not only for one audience; the alias is what the person
searching `--help` or the README for accessibility will actually find. No
runtime toggle: the two front ends set the terminal up differently, and
switching mid-session is a second problem with no evidence anyone wants it.

### 6.2 Output

Lines are appended and nothing is ever repainted. Concretely: the alternate
screen is never entered, each output line is written exactly once, and no line
already written is revisited.

The one place the cursor moves is the line currently being typed, which raw
mode makes the client's responsibility (§6.3). That line is below every line
already printed and is never itself part of the transcript until it is sent, so
editing it in place does not disturb text a reader may be moving over.
Blightmud's reader mode does the same thing, holding the input at a fixed
position while output accumulates above it.

Every line the client shows already passes through `SessionPane::push_line` —
the same choke point that fills scrollback, writes the transcript, and keeps
masked passwords out of both (§8, §13). That is the hook, and there is exactly
one of it.

`RetainedLine` carries `origin`, so a client notice, a trigger's echo and
server text stay distinguishable rather than being flattened into one
indistinguishable stream (§8). Line mode composes its prefix from `origin` the
way `ui::draw_channel` composes its own, rather than inventing a second
convention.

### 6.3 Input

Raw mode, without the alternate screen. The two are separate switches and only
the second breaks the announce-on-insert mechanic. Raw mode is what lets a
configured keypress reach the layer branch 1 builds; it also means the client
echoes typed characters itself, which §6.4 constrains.

This follows precedent rather than reasoning: Blightmud's reader mode
(`src/ui/reader_screen.rs`) avoids the alternate screen while keeping raw mode
and rebindable keys, and is what its blind users run. A screen reader's own
review keys are not at risk, because the screen reader sits upstream of the
terminal — `tdsr` wraps the program in a pseudo-terminal and takes its keys
before passing the rest through; NVDA and Orca hook above the terminal
emulator entirely.

The residual uncertainty is the NVDA and Orca mechanism specifically, which is
asserted here from documentation rather than tested. It is the kind of claim
#39 says to route to a blind developer rather than settle by argument, and it
is recorded as such rather than treated as settled.

### 6.4 Echo, and not moving the reader

Two different things are called echo here and only one of them is a setting.

**Keystroke echo** is not optional. Raw mode means the terminal no longer shows
what is typed, so the client must, or the player types blind. This is the input
line of §6.2.

**Sent-command echo** — printing the finished command back into the output
stream after Enter — is **off by default** and enabled by a setting. New text
arriving is what steals a screen reader's read position, and a client that
echoes what you just typed moves you twice for one command.

This is a preference and blind users contradict each other on it, so it ships
as a setting with both branches implemented and both tested, rather than as a
default chosen on one voice. That rule applies to every accessibility
preference this work adds.

### 6.5 Several characters, one printed

`--line` accepts several profiles, as the TUI does. Every session connects and
fills its scrollback; only the bound one prints. Switching prints a marker
naming the character and begins printing that character's lines.

Interleaving every session into one stream is what a blind multi-boxer would
eventually want, and is not this release: two characters' output interleaved
without any spatial separation is where the design becomes genuinely hard to
read, and it deserves its own evidence. Nothing here forecloses it — the lines
are already retained per session, tagged with their origin.

### 6.6 Refusals are spoken

A surface that cannot degrade prints a named refusal rather than doing nothing.
`/config` in line mode says that the settings editor needs the full-screen
front end and where the file is; it does not silently no-op. Same for any
`Action` this front end declines.

Silence is the failure mode that teaches a blind player the client is broken.
A refusal is honest, is one line, and is what makes the remaining gaps
enumerable.

### 6.7 Done when

A pty test drives the real binary under `--line` and asserts:

- the alternate-screen sequence is never sent, at any point in the session;
- each line appears exactly once and contiguously, with no cursor addressing
  between arrivals;
- a masked password never appears in the output;
- a declined action prints its refusal;
- switching character prints the marker and then that character's lines.

The pty harness already exists (`tests/pty_smoke.rs`), including the
`press_esc` discipline for `Esc`-prefixed keys.

## 7. What this does not achieve

Stated plainly so the gap is arguable rather than discovered.

The player still cannot skip or re-read text as it arrives from inside the
client — the most consistently requested capability across twenty years of
blind MUD players. This design's position is that their screen reader's own
review cursor already provides it over a plain scrolling terminal. **That
position is unverified**, and verifying it is the single question worth putting
to a blind developer:

> Over a normal scrolling terminal with no repaint, does your own screen
> reader's review cursor already give you "skip a few boring lines, or re-read
> that line"?

Yes, and there is nothing further to build. No, and there is a specific,
arbitrable statement of what is missing, from someone who can describe the
mechanism.

Also unavailable in line mode: the map column, channel panes, party strip,
protocol inspector, unread badges, and the settings editor. Each is a spoken
refusal (§6.6) rather than a silence.

## 8. Feedback, if this reaches testers

Three rules, adopted whatever else happens, because the failure they prevent is
documented: Mudlet's screen-reader edition collapsed with eight blind testers
and contradictory feedback.

- **A defect** — could not complete the task, or something moved their read
  position. One tester is enough. Always fix.
- **A preference** — completed it, would rather it differed. Never becomes a
  default on one voice. Ships as a setting with both branches implemented and
  tested, or not at all.
- **A mechanism claim** — "my screen reader cannot do X." Not answerable by
  play-testers, and routed to someone who can describe the mechanism.

Where blind users contradict each other, the tiebreak is whichever branch
preserves more of the player's own assistive stack.
