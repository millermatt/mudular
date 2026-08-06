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
for a fuller example, including the trigger/alias fields profiles accept
today (rule *scoping* across profiles and shared modules lands in a later
milestone — see the roadmap in ARCHITECTURE.md §14).

### Where the config directory is

By default, the platform config directory:

- Linux: `~/.config/mudular/`
- macOS: `~/Library/Application Support/mudular/`
- Windows: `%APPDATA%\mudular\`

Override it with `--config-dir <path>` (useful for testing, or running
multiple isolated configs).

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

## App settings and keybinds

Optional app-wide settings live at `<config dir>/mudular.yaml`. Today
this covers the quit key:

```yaml
# ~/.config/mudular/mudular.yaml
keybinds:
  quit: ctrl+q
```

Keybindings are written as `modifier+modifier+key`, e.g. `ctrl+c`,
`ctrl+shift+q`, `esc`. With no file (or no `keybinds` section), quit
defaults to `ctrl+c`.

## Recording a session

`--record <file>` writes every raw byte the server sends to a file, with
per-read timestamps — handy for filing a bug report or turning a tricky
server quirk into a test fixture:

```sh
mudular --host mud.example.org --port 4000 --record session.log
```

## All flags

```
mudular [PROFILE] [OPTIONS]

  [PROFILE]              Profile name to connect with (loads
                          <config dir>/profiles/<PROFILE>.yaml)

  --host <HOST>           Connect directly, bypassing profiles
  --port <PORT>           Port for --host (default: 23)
  --tls                   Use TLS for --host
  --tls-verify <MODE>     full | pinned | insecure (default: full)
  --charset <CHARSET>     utf-8 | latin1 | cp437 for --host (default: utf-8)
  --config-dir <PATH>     Override the config directory
  --record <PATH>         Record raw inbound bytes to a file
  --log <PATH>            Write diagnostic logs to a file (filtered via RUST_LOG)
```
