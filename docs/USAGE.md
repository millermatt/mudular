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

Type a line and press Enter to send it. `Ctrl+C` quits (remappable, see
below). If the server masks input (a password prompt), the input box
shows `(hidden)` and what you type isn't echoed or kept in scrollback.

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
```

Then just:

```sh
mudular kestrel
```

See [`examples/config/profiles/kestrel.yaml`](../examples/config/profiles/kestrel.yaml)
for a fuller example.

### Where the config directory is

By default, the platform config directory:

- Linux: `~/.config/mudular/`
- macOS: `~/Library/Application Support/mudular/`
- Windows: `%APPDATA%\mudular\`

Override it with `--config-dir <path>` (useful for testing, or running
multiple isolated configs).

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
  named group like `(?P<who>...)`) or a variable. A name that resolves to
  neither is left as-is, so a typo is visible rather than silently blank.
- **`set:`** on any rule updates a variable, so a trigger can record
  something (say, your current target) for a later alias to use.
- **Typing several commands at once**: `north; k; look` is split on `;`
  and each part expanded separately. Alias output is never re-expanded,
  so aliases cannot loop.
- **Durations** need a unit: `500ms`, `30s`, `5m`, `2h`.

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

Note that this reloads *rules* only. Changing a profile's `host`, `port`,
`tls`, or `charset` still needs a restart.

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
  --record <PATH>         Record raw inbound bytes to a file
  --log <PATH>            Write diagnostic logs to a file (filtered via RUST_LOG)
```
