# Using Mudular

This covers day-to-day use: connecting, profiles, config files, and
command-line flags. For how it's built, see
[ARCHITECTURE.md](ARCHITECTURE.md).

## Quick connect

Connect straight to a MUD without any config file:

```sh
mudular --host mud.example.org --port 4000
```

Add `--tls` for a TLS ("STelnet") MUD:

```sh
mudular --host mud.example.org --port 4443 --tls
```

Type a line and press Enter to send it. What you typed is echoed into
your own scrollback as `> line`, so it's part of the transcript alongside
the server's replies. `Ctrl+C` quits (remappable, see below). If the
server masks input (a password prompt), the input box shows `(hidden)`
and what you type isn't echoed or kept in scrollback.

## Profiles

For a MUD you play regularly, save it as a profile instead of typing
flags every time. Profiles live at
`<config dir>/profiles/<name>.yaml`:

```yaml
# ~/.config/mudular/profiles/kestrel.yaml
name: kestrel
host: mud.example.org
port: 4443
tls:
  enabled: true
  verify: pinned   # full | pinned | insecure — see below
charset: utf-8     # or a legacy fallback: latin1, cp437
color: cyan        # tints this character's pane border and tab
```

Then just:

```sh
mudular kestrel
```

See [`examples/config/profiles/kestrel.yaml`](../examples/config/profiles/kestrel.yaml)
for a fuller example.

### Logging in automatically

Add a `login:` block naming your character, and Mudular answers the
server's opening prompts for you:

```yaml
login:
  name: Kestrel
```

The password does **not** go in the file — there is no field for it, and
putting one there is a startup error rather than a silently stored
secret. Store it in your OS keyring instead (GNOME Keyring, macOS
Keychain, Windows Credential Manager):

```sh
mudular --set-password kestrel
```

It prompts without echoing, so the password never reaches your shell
history or your screen. Re-run it any time to change the stored one, or
drop it again with:

```sh
mudular --forget-password kestrel
```

You don't have to set it up in advance, though. The first time you log in
on a profile with a `login:` block and nothing stored, mudular sends your
character name, you type the password as usual, and the pane asks:

```
** Save this password in the OS keyring for `kestrel`, so it logs you in
   next time? (y/n)
```

`y` stores it — the next login is automatic. `n` is remembered, and you
are not asked again for that profile; the refusal is a line in
`<config dir>/keyring_declined`, so deleting it puts the offer back. Any
other key just dismisses the question, and it comes back next login. The
password is only offered once per session, and only for what you type at
a masked prompt.

If the MUD rejects the password and asks for another, the question is
withdrawn before you can answer it — a password that didn't work is not
worth keeping. You are only ever asked about a profile with nothing
stored: once `--set-password` or a `y` has put one in the keyring,
nothing asks again.

If your MUD's prompts aren't recognised, override them with regexes:

```yaml
login:
  name: Kestrel
  name_prompt: '^Who goes there\?'
  password_prompt: '^Speak the word'
```

The password step also fires whenever the server hides your typing, so on
MUDs that mask the password prompt the wording doesn't matter.

With no password stored, the name is still sent and the pane tells you
what's missing — you type the password as usual.

**How it stays safe:** each step fires at most once per connection, and
anything you type shuts the whole thing off for the rest of the session.
So a `Password:` that another player says in chat an hour later has
nothing left to trigger.

### Where the config directory is

By default, the platform config directory:

- Linux: `~/.config/mudular/`
- macOS: `~/Library/Application Support/mudular/`
- Windows: `%APPDATA%\mudular\`

Override it with `--config-dir <path>` (useful for testing, or running
multiple isolated configs).

### If the connection drops

A connection that goes away on its own — the server rebooting, your
network blinking — is retried automatically. The status line above the
input says what happened and when the next attempt is:

```
reconnecting in 4s (attempt 3): connection closed
```

The wait starts at a second, doubles with each failed attempt, stops
growing at a minute, and starts over as soon as a connection comes back.
Your aliases, variables, and rules survive the gap untouched; the
connection itself is new, so `on_connect` hooks run and timers restart
just as they do on `/reload`. An address that never answered in the first
place is reported rather than retried.

## Playing several characters at once

Name more than one profile and each gets its own session, with its own
scrollback, prompt, input buffer, and rules:

```sh
mudular tank cleric
```

- **Layout:** tabs by default (one character full-screen, with a tab bar
  naming the rest); `F3` switches to side-by-side panes.
- **Focus:** `Alt+1`/`Alt+2` jump to a character by number, `Ctrl+Tab`
  cycles. A background session shows `● 3` in its tab for the lines you
  haven't seen yet, cleared when you focus it.
- **Typing** always goes to the focused character, and the input box says
  which one (`input → tank`). Nothing is shared between sessions — the
  input buffers are separate, so switching mid-sentence keeps both.
- **Colour:** give a profile `color: cyan` and that character's pane
  border and tab entry are drawn in it, so you can tell panes apart
  without reading the titles. Any colour name (`cyan`, `light blue`), a
  `#rrggbb` value, or a 0-255 palette index works; an unrecognised one is
  rejected at startup rather than ignored. Focus is still shown by
  brightness, so an unfocused pane is dimmed in its own colour.
- Opening the same profile twice gives the second one a `-2` suffix
  (`cleric-2`), which is how rules address it.

### Channel panes

Tells and chat can be pulled out of the main scrollback into their own
pane, so they don't scroll away under combat spam. Declare them in
`mudular.yaml`:

```yaml
channels:
  - name: comms
    match:
      - '^\[gossip\]'
      - '^\w+ tells you'
    keep_in_main: false   # false (default) moves the line; true copies it
    timestamps: true
```

The panes dock in a column beside the character panes; `F4` shows or hides
them. With more than one character connected, lines are tagged with the
one that received them (`[cleric] Bob tells you …`). Add `session: tank`
to a channel to pin it to a single character instead.

Focusing a channel pane does **not** change where your typing goes — it
stays with the last character you focused, which is what the input box
border is telling you.

## Automation: aliases, triggers, timers

Rules can go directly in a profile, or in a shared module that several
profiles reuse.

```yaml
variables:
  target: rat

aliases:            # rewrite what you type
  - pattern: '^k$'
    send: ["kill ${target}"]
  - pattern: '^gh (.+)$'
    send: ["get ${1}", "wear ${1}"]

triggers:           # react to what the server sends
  - pattern: '^(?P<who>\w+) has arrived\.$'
    send: ["say welcome ${who}"]
  - pattern: 'is DEAD'
    send: ["get all corpse"]
  - pattern: '\[gossip\]'
    gag: true       # hide the line entirely

timers:             # act on a schedule
  - every: 5m
    send: ["save"]
  - after: 30s      # one-shot instead of repeating
    send: ["stand"]
```

- **Patterns** are regular expressions, matched against the line with
  colour codes already stripped — so a pattern never has to account for
  ANSI escapes.
- **`${...}`** substitutes a capture group (`${1}`, or `${name}` for a
  named group like `(?P<who>...)`), a variable, or a value the server sent
  over GMCP/MSDP (`${Char.Vitals.hp}`). A name that resolves to none of
  them is left as-is, so a typo is visible rather than silently blank.
- **`set:`** on any rule updates a variable, so a trigger can record
  something (say, your current target) for a later alias to use.
- **Typing several commands at once**: `north; k; look` is split on `;`
  and each part expanded separately. Alias output is never re-expanded,
  so aliases cannot loop.
- **Durations** need a unit: `500ms`, `30s`, `5m`, `2h`.

### Firing only when it matters

A pattern says *what* matched; `when:` says whether to act on it. Add it
to any alias or trigger and the rule fires only if the pattern matches
**and** the condition is true:

```yaml
variables:
  heal_at: 40

triggers:
  - pattern: '^Your health: (?P<hp>\d+)%'
    when: '${hp} < ${heal_at}'
    send: ["quaff heal"]
  - pattern: '^(?P<who>\w+) tells you'
    when: '${Char.Status.combat} == "0" and ${who} != "Bob"'
    send: ["reply on my way"]
```

The grammar is deliberately small — comparisons (`< <= > >= == !=`),
`and`/`or`/`not`, parentheses, numbers, quoted strings, and `${...}`
terms. `${...}` reads the same things `send:` does: captures first, then
variables, then server data. Anything more involved than that is a
script's job (see [Scripting](#scripting)).

Two things to know:

- Numbers compare as numbers, text as text. `${hp} < 40` does the
  arithmetic you expect even though everything in the stores is a string.
- **A name that resolves to nothing makes the condition false** and the
  rule does not fire. That is the opposite of `send:`, which leaves a
  typo visible on screen — a guard has no way to show itself, so not
  firing is the safe failure.

A guarded-out rule does nothing at all: it doesn't `gag:` or `route:` the
line either, and for an alias a false guard is simply not a match, so the
next matching alias — or what you literally typed — still gets its turn.

Bad conditions are caught when the rules compile, the same as a bad
regex, so `when: '${hp} <'` fails at startup or on `/reload` rather than
becoming a rule that silently never fires. So does a bare term:
`when: '${combat}'` is an error, not a guess about what counts as true —
write the comparison out.

### Colouring what matters

A trigger can recolour the text it matched instead of — or as well as —
acting on it. That is the cheapest way to find your name in a wall of
chat:

```yaml
triggers:
  - pattern: '\bKestrel\b'
    highlight: {fg: bright_yellow, bold: true}
  - id: low-hp
    pattern: '^You are bleeding'
    highlight: {fg: white, bg: red, whole_line: true}
```

`fg` and `bg` take a colour name, `#rrggbb`, or a 0-255 palette index —
the same vocabulary as a profile's `color:`. The attributes are `bold`,
`italic`, `underline`, and `reverse`. All of them are optional, but a
`highlight:` that sets none of them is an error at startup rather than a
rule that quietly does nothing, and so is a colour name nobody has heard
of.

By default only the matched text is recoloured; `whole_line: true`
recolours the line. Matching a capture group isn't offered — narrow the
pattern instead.

The rest of the line keeps whatever colour the server gave it, so a
highlight inside a coloured region leaves the region looking untouched on
both sides. Where two highlights would overlap, the first one wins and
the second is dropped rather than nested, the same way `route:` picks a
channel. A rule that both gags and highlights just gags: the line isn't
there to colour.

### Driving another character

With more than one character connected, a rule can send commands to a
*different* session with `send_to:`, keyed by the name that addresses it
(the profile name, or `cleric-2` for a second copy):

```yaml
# tank profile — get healed when I drop below 40%
triggers:
  - pattern: '^HP: (?P<hp>\d+)%'
    when: '${hp} < 40'
    send_to:
      cleric: ["cast 'major heal' Grunk"]

# an alias that moves the whole group
aliases:
  - pattern: '^gn$'
    send: ["north"]
    send_to:
      '*': ["north"]        # `*` means every *other* session
```

The receiving pane echoes `[from tank] cast 'major heal' Grunk`, so
nothing happens invisibly. If the named session isn't connected, the
command is dropped with a warning in the *sending* pane.

By default an injected command is sent verbatim — the target's own
aliases do not expand it. That is the receiver's call to change, never
the sender's:

```yaml
# mudular.yaml — install-wide default
cross_session:
  expand_aliases: false   # injected commands are sent verbatim
  max_hops: 1             # how far a chain of injections may travel

# profiles/cleric.yaml — this character opts in
cross_session:
  expand_aliases: true
```

`max_hops` is the loop brake: rules that fire while a remote command is
being handled can only `send_to` again until the budget runs out, so two
characters' rules can't ping-pong forever.

#### Reading another character's state

The other direction is to watch instead of push. Every session publishes
its variables and its server data (GMCP/MSDP vitals, affects, room), and
any rule can read a peer's with `${@name.key}`:

```yaml
# cleric profile — the reaction lives with the character that acts
triggers:
  - pattern: '^You finish your prayer'
    when: '${@tank.Char.Vitals.hp} < 50'
    send: ["cast 'major heal' Grunk"]
```

`@` is its own namespace, so a peer name can never shadow one of your own
variables. An unknown peer or key resolves to nothing — which leaves a
`${...}` in `send:` visibly unexpanded and makes a `when:` guard false,
exactly like an unknown local name. Naming a character who hasn't
finished connecting is fine: you read an empty snapshot until there's
something to read.

Prefer this to `send_to:` when the reaction is really the other
character's job. The rule then lives with the character that acts, and it
keeps working no matter which session noticed first.

### Sharing rules between characters

Rules are layered, lowest precedence first:

1. `<config dir>/global.yaml` — always loaded
2. shared modules from `<config dir>/modules/<name>.yaml`, in the order
   the profile lists them under `modules:`
3. the profile's own inline `variables:`/`aliases:`/`triggers:`/`timers:`

A later layer can **override** an earlier rule instead of adding a second
one. Give a rule an `id:` and reuse that id in a later layer:

```yaml
# modules/combat.yaml — shared by every character
triggers:
  - id: autoloot
    pattern: 'is DEAD'
    send: ["get all corpse"]
```

```yaml
# profiles/thief.yaml — same module, different behaviour
modules: [combat]
triggers:
  - id: autoloot
    enabled: false        # turn it off, without repeating the pattern
```

An override only needs the fields it changes; everything else is
inherited, and the rule keeps its original position in the firing order.
Rules with no `id` are matched by their exact `pattern` instead. Aliases,
triggers, and timers each have their own id namespace.

### Reloading rules while connected

Edit a rule file, then type:

```
/reload
```

The rules are recompiled from disk and swapped into the running session —
no reconnect, no losing your place. If a file has an error, the reload
reports it and keeps the rules you already had.

Scripts come back with them: their files are re-read, the fresh VM has no
memory of the old one, and its `on_connect` hooks run, since as far as a
just-loaded script is concerned the connection is only now up. Timers
restart on the same reasoning.

Note that this reloads *rules and scripts* only. Changing a profile's
`host`, `port`, `tls`, or `charset` still needs a restart.

## Scripting

YAML covers the common cases; a script covers everything else — anything
that needs memory between lines, arithmetic, or more than one step.
Scripts are written in Lua, listed by file name in the module or profile
that owns them:

```yaml
# modules/uw-combat.yaml
name: uw-combat
scripts: [uw-combat.lua]     # beside this file
```

A name is a name, not a path: the file lives next to the YAML that
declares it, so a shared module is one directory to copy and a module you
downloaded can't reach the rest of your disk by naming `../`. Scripts
load in the same order as the rules (global, then modules, then the
profile), and they all share one VM per character — so a profile's script
can build on what a module's script defined, and both see the same
variables your rules do.

```lua
-- modules/uw-combat.lua
local potions = 0

mud.on_line(function(line)
  if line:match("^You quaff a %w+ potion") then
    potions = potions + 1
    mud.echo("** " .. potions .. " potions this session")
  end
end)

mud.on_gmcp(function(package, json)
  if package ~= "Char.Vitals" then return end
  local hp = tonumber(mud.data("Char.Vitals.hp"))
  local max = tonumber(mud.data("Char.Vitals.maxhp"))
  if hp and max and max > 0 and hp / max < 0.3 then
    mud.send("quaff heal")
  end
end)
```

Everything a script can do arrives through the `mud` table:

| Call | Does |
|---|---|
| `mud.send(cmd)` | Send a command, as if you had typed it |
| `mud.echo(text)` | Write a line into this character's pane only |
| `mud.gag()` / `mud.substitute(text)` | Hide the current line, or replace its text |
| `mud.get(name)` / `mud.set(name, value)` | Read and write the same variables `variables:` and a rule's `set:` use |
| `mud.data(key)` | Read server data by dotted key (`Char.Vitals.hp`) |
| `mud.timer(seconds, fn)` | Run `fn` once, later |
| `mud.on_line(fn)` / `mud.on_prompt(fn)` | Called with the line's text, colour codes already stripped |
| `mud.on_gmcp(fn)` | Called with the package name and its raw JSON — decode it with the JSON library you prefer |
| `mud.on_connect(fn)` / `mud.on_disconnect(fn)` | Called when the connection comes up or goes away |
| `mud.session(name)` | A handle on another character (below), or `nil` if no session has that name |
| `mud.on_peer(name, key, fn)` | Watch another character's server data (below) |

`mud.get`/`mud.set` and `mud.data` are the *same* stores the rules use,
not a parallel copy — a trigger's `set:` and a hook can hand work to each
other. On an inbound line the triggers run first and then the line hooks,
so a script sees what that line's rules just set, and has the last word
on gagging it.

`mud.timer` is one-shot. A heartbeat re-arms itself from inside its own
callback, which is also what stops a slow callback stacking up behind
itself:

```lua
local function tick()
  mud.send("save")
  mud.timer(300, tick)
end
mud.timer(300, tick)
```

### Calling a script from a rule

For a reaction that only needs a different *action*, leave the pattern in
the YAML where you can read and override it, and point the rule at a
function:

```yaml
triggers:
  - id: tally-kills
    pattern: '(?P<victim>.+) is DEAD!'
    script: {file: uw-combat.lua, fn: on_death}
```

```lua
local kills = 0

function on_death(line, caps)
  kills = kills + 1
  mud.echo("** " .. caps.victim .. " down (" .. kills .. " this session)")
end
```

The function gets the matched line and its captures — numbered groups at
`caps[1]`, `caps[2]`, …, named groups under their own names. Both halves
are checked when the rules compile: the file must be one a layer
declared, and the function must exist in it, so a mistyped `fn:` fails at
load rather than becoming a rule that never does anything. A `when:`
guard applies here like anywhere else — no fire, no call.

`script:` works on aliases and triggers. A timer that needs to do more
than `send:` should arm itself with `mud.timer` instead.

### Reaching other characters from a script

`mud.session(name)` is the scripting side of `send_to:` and
`${@name.key}`:

```lua
-- cleric script: rebuff the tank when his blessing drops
mud.on_peer("tank", "Char.Affects", function(key, value)
  if key == "Char.Affects.blessed" and value == "0" then
    mud.send("cast bless Grunk")
    mud.session("tank"):echo("blessing you now")
  end
end)
```

- `mud.on_peer(name, key, fn)` subscribes by dotted key *prefix*, so
  `"Char.Affects"` catches `Char.Affects.blessed` without naming it, and
  `fn(key, value)` is called once per key that changed.
- `mud.session("tank").vars` and `.data` are that character's state as it
  stood when you asked for the handle — so reading two keys shows you one
  moment of their life rather than two.
- `handle:send(cmd)` is routed exactly like `send_to:`, hop limit and
  all. `handle:echo(text)` only writes into their pane, so nothing runs at
  the far end and no limit applies. Both carry the `[from tank]` tag.
- `mud.session` answers `nil` for a name no session holds, so a script can
  ask rather than assume.

### What a script can't do

Scripts are sandboxed, because a shared module you downloaded may carry
one. There is no filesystem, no network, and no process access: `io`,
`os`, `package`, and `debug` are never created in the first place, and
`load`, `dofile`, `require`, and `print` are gone — a script cannot fetch
more code or write around the TUI.

Hooks run on their own character's task and are expected to be quick. One
that runs longer than 100ms is aborted, so a runaway loop can't stall the
character it belongs to (and never another one). An aborted or failing
hook says so in that character's scrollback — a script that quietly
stopped working is otherwise as invisible as a trigger that stopped
firing.

### JavaScript

Lua ships in the default build. JavaScript is available behind a feature
flag:

```sh
cargo build --features js
```

The `mud` API is identical — same calls, same hooks, same semantics,
spelled the way JavaScript spells things (`mud.session("tank").send(cmd)`
rather than `:send`). The file extension picks the engine per script, and
a character can run both at once; each language gets its own VM, and a
line reaches every script in both.

## TLS certificate verification

`--tls-verify` (or a profile's `tls.verify`) controls how much a TLS
connection is trusted:

| Mode | Behavior |
|---|---|
| `full` (default) | Standard certificate validation against public CAs. Use this for MUDs with a real certificate. |
| `pinned` | Trust-on-first-use: the certificate's fingerprint is recorded the first time you connect, under `<config dir>/known_certs`. If it ever changes later, the connection is refused rather than silently accepted — useful for the self-signed certificates many MUDs run. |
| `insecure` | No verification at all. The connection is clearly labeled and a warning is written into the pane every time — don't use this on a network you don't trust. |

## Charsets

Most modern MUDs speak UTF-8 and nothing else needs to be configured.
For older MUDs that predate UTF-8, set `charset: latin1` or
`charset: cp437` in the profile (or pass `--charset cp437` for a direct
`--host` connection). CP437 covers the classic MS-DOS-era box-drawing
characters (`╔═╗│║└┐`, etc.) many older MUD maps and walls use.

Mudular also negotiates the Telnet CHARSET option and will request UTF-8
if the server offers it — but most legacy MUDs never implemented that
option at all, which is exactly why the profile setting exists as a
fallback.

## Keys

| Key | Does | Remappable as |
|---|---|---|
| `Enter` | Send the input line. On an empty box it sends a bare return, for the "press return to continue" prompts many MUDs use at login. | — |
| `Up` / `Down` | Walk back and forward through the commands you've sent to the focused character. What you were part-way through typing comes back when you walk forward past the newest one. | — (built in) |
| `F1` | Show the help overlay: every key and client command, including any you've remapped. Any key closes it. | `help` |
| `Ctrl+C` | Quit | `quit` |
| `Alt+1` … `Alt+9` | Jump straight to session 1–9 | — (built in) |
| `Ctrl+Tab` | Cycle focus to the next pane, including channel panes | `focus_next` |
| `F2` | Toggle the raw GMCP inspector for the focused session — the messages the server is sending behind the scenes | `gmcp_inspector` |
| `F3` | Switch between the tabbed and side-by-side layouts | `cycle_layout` |
| `F4` | Show or hide the channel panes | `toggle_channels` |
| `PgUp` / `PgDn` | Scroll the focused pane back / forward through its scrollback | — (built in) |
| `Home` / `End` | Jump to the oldest / newest line in the focused pane | — (built in) |

`F1` shows this same list inside the client, built from your actual
config — so it stays right even if you remap something. `/help` prints it
into the pane if you'd rather not reach for a key.

`Ctrl+Tab` is the one to watch: terminals differ in whether they send it
at all, and some swallow it for their own tab switching. If it does
nothing in yours, remap it (`alt+tab` is the usual second choice).
`Alt+1..9` works everywhere and is unaffected.

## App settings and keybinds

Optional app-wide settings live at `<config dir>/mudular.yaml`. Every key
in the table above with a name in the last column can be remapped there:

```yaml
# ~/.config/mudular/mudular.yaml
keybinds:
  quit: ctrl+q
  focus_next: alt+tab
  gmcp_inspector: f2
  cycle_layout: f3
  toggle_channels: f4
  help: f1
```

`history_size:` (default 500) sets how many commands each character
remembers for `Up`/`Down`:

```yaml
history_size: 500
```

History is per character — the tank's commands never appear in the
cleric's input — and it records what you typed, before aliases expand, so
`k` recalls as `k`. Passwords are never recorded: while the server has
input masked, nothing is stored and `Up` does nothing, so an old command
can't be sent as your password. History is kept in memory only and is
gone when you quit; nothing is written to disk.

Keybindings are written as `modifier+modifier+key` — modifiers are
`ctrl`, `alt`, and `shift`; keys are a single character (`c`), a function
key (`f2`), or one of `esc`, `enter`, `tab`, `backspace`. Every entry is
optional: with no file, no `keybinds` section, or a section naming only
some of them, the rest keep the defaults above. An unknown modifier or key
name is rejected at startup with the offending string, rather than
silently leaving you without the binding.

## Recording a session

`--record <file>` writes every raw byte the server sends to a file, with
per-read timestamps — handy for filing a bug report or turning a tricky
server quirk into a test fixture:

```sh
mudular --host mud.example.org --port 4000 --record session.log
```

## All flags

```
mudular [PROFILE]... [OPTIONS]

  [PROFILE]...            Profile name(s) to connect with (each loads
                          <config dir>/profiles/<PROFILE>.yaml). Naming
                          several opens one session per character.

  --host <HOST>           Connect directly, bypassing profiles
  --port <PORT>           Port for --host (default: 23)
  --tls                   Use TLS for --host
  --tls-verify <MODE>     full | pinned | insecure (default: full)
  --charset <CHARSET>     utf-8 | latin1 | cp437 for --host (default: utf-8)
  --config-dir <PATH>     Override the config directory
  --set-password <PROFILE>
                          Store that profile's auto-login password in the
                          OS keyring (prompts; no echo), then exit
  --forget-password <PROFILE>
                          Delete that profile's stored password from the
                          OS keyring, then exit
  --record <PATH>         Record raw inbound bytes to a file
  --log <PATH>            Write diagnostic logs to a file (filtered via RUST_LOG)
```
