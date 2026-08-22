//! What the client *is*: the session model, the panes, and `AppState`.
//!
//! Split out of `app` so `ui` can render the model without importing the
//! module that owns the terminal and the event loop (#6,
//! docs/ARCHITECTURE.md §4). The rule the split encodes: here live the
//! state and the questions answerable from state alone; in `app` lives the
//! loop that changes it, and everything with an effect beyond it.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;

use crossterm::event::{Event, KeyCode, KeyModifiers};
use ratatui::style::Color;
use tokio::sync::mpsc;
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;

use crate::config::{self, Channel, CrossSession, Keybinds};
use crate::engine::Engine;
use crate::proto::charset::Charset;
use crate::scrollback::{Origin, RetainedLine};
use crate::session::{self, SessionCommand, SessionEvent};
use crate::ui;

/// Same rationale as `scrollback_size` (§8), for the raw server-data
/// inspector log (GMCP and/or MSDP, §6.3): bounded so a chatty MUD can't
/// grow it without limit.
pub(crate) const INSPECTOR_LOG_LIMIT: usize = 1_000;

/// Rows `PgUp`/`PgDn` move per press. Exact viewport-height paging would
/// need pane geometry threaded back from the UI layer into `AppState`; a
/// fixed step is the ordinary simplification most pagers make too
/// (docs/ARCHITECTURE.md §11.5).
pub(crate) const SCROLL_PAGE: usize = 10;

/// How many replies the empty client keeps on screen.
///
/// Sized for the largest thing that lands there rather than for a typical
/// error: `/help` works without a character (#7) and prints the whole
/// keybind listing, and a cap that clipped it to the last few rows would
/// make the one command a stuck player is most likely to reach for the
/// one that answers worst.
pub(crate) const SHELL_NOTICES: usize = 24;

/// How many warnings `/errors` keeps (#18). Enough that a session's worth
/// of them survives, bounded because nothing else prunes it.
pub(crate) const ERRORS_KEPT: usize = 200;

/// The client-side commands. Everything else starting with `/` is left
/// alone, since plenty of MUDs use `/` for their own commands.
pub(crate) const RELOAD_COMMAND: &str = "/reload";
/// The same listing as the overlay, for players who never find the key
/// (docs/ARCHITECTURE.md §11.2).
pub(crate) const HELP_COMMAND: &str = "/help";
/// Opens the in-client profile editor (§10.2) — the same thing `F5` does.
pub(crate) const CONFIG_COMMAND: &str = "/config";
/// Installs a newer release, having been told one exists (§15). Applying
/// is delegated to `mudular-update`, which knows how this copy was
/// installed; see `crate::update`.
pub(crate) const UPDATE_COMMAND: &str = "/update";
/// Opens the same form the first-run screen shows, reachable any time
/// rather than only at zero-profile startup (§15, UX_REVIEW.md B).
pub(crate) const NEWPROFILE_COMMAND: &str = "/newprofile";
/// Adds a character to this running instance (§7.5,
/// `ARCH_REVIEW.md` "Features that would break the architecture") — a
/// name follows, not a fixed string like the commands above.
pub(crate) const CONNECT_COMMAND: &str = "/connect";
/// Closes the character being typed at (#98). The other half of
/// `/connect`: a client that can add a pane but never remove one teaches
/// its user to relaunch, which is what multi-session support exists to
/// avoid. Takes no argument deliberately — the pane you are typing at is
/// the one you mean, and naming a character invites closing the wrong one
/// by typo.
pub(crate) const DISCONNECT_COMMAND: &str = "/disconnect";
/// Opens the panel of client warnings (#18). Client-generated, so it
/// never fills with server text: the point is a place where a failed
/// `/reload` cannot be mistaken for something the MUD said.
pub(crate) const ERRORS_COMMAND: &str = "/errors";
/// Walks toward a room already on the map, one step at a time (§16) — a
/// vnum, or a case-insensitive substring of a room name.
pub(crate) const GOTO_COMMAND: &str = "/goto";
/// Walks back to where a `corpse:` trigger last said the character died
/// (§16) — `/goto` with the target already remembered.
pub(crate) const CORPSE_COMMAND: &str = "/corpse";
/// Labels the room the character is standing in (§16) — the player's own
/// note about what a place is for, since no protocol tells us.
pub(crate) const MARK_COMMAND: &str = "/mark";
pub(crate) const MAP_COMMAND: &str = "/map";
/// Shows or hides the comms column (§11.1) — the same thing
/// `toggle_channels` does, for players who reach for a command before a
/// function key.
pub(crate) const COMMS_COMMAND: &str = "/comms";
/// Runs a command as another character without switching to their pane
/// (§7.5) — a character name, or `*`, then the command. The ad-hoc form of a
/// rule's `send_to:`, for the times the player did not decide in advance that
/// this was a thing they would want to do.
pub(crate) const SEND_COMMAND: &str = "/send";

/// `send_to` address meaning "every other session" (§7.5).
pub(crate) const ALL_SESSIONS: &str = "*";

/// Which field the new-profile wizard is currently collecting, in the
/// order it asks them (docs/ARCHITECTURE.md §15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WizardStep {
    Name,
    Host,
    Port,
    Tls,
}

impl WizardStep {
    pub fn prompt(self) -> &'static str {
        match self {
            WizardStep::Name => "Character/profile name",
            WizardStep::Host => "Host",
            WizardStep::Port => "Port (blank for 23)",
            WizardStep::Tls => "Use TLS? (y/N)",
        }
    }
}

/// What one key did to a [`NewProfileWizard`] in progress.
#[derive(Debug)]
pub(crate) enum WizardOutcome {
    /// More fields to go, or the last answer was rejected — the wizard's
    /// own state (`error`, in particular) says which.
    Continue,
    Cancelled,
    Done(config::NewProfile),
}

/// A character name a person would actually type, not paste — long past
/// this the wizard's dialog box would grow to fill the terminal and break
/// its own centered layout (UX_REVIEW.md, Adversarial findings, Low #6).
pub(crate) const MAX_PROFILE_NAME_LEN: usize = 32;

/// The "create a profile" form's state machine (docs/ARCHITECTURE.md §15),
/// shared by two homes: the first-run screen (`run_new_profile_wizard`,
/// which still needs its own terminal session, since there is no live
/// session yet for the form to overlay) and `/newprofile` reachable any
/// time after (an `AppState` field, drawn over the panes and keeping
/// sessions running behind it — the same "state, not layout" split the
/// config editor already uses).
pub struct NewProfileWizard {
    pub step: WizardStep,
    name: String,
    host: String,
    port: u16,
    pub answered: Vec<(&'static str, String)>,
    pub input: Input,
    pub error: Option<String>,
    config_dir: PathBuf,
}

impl NewProfileWizard {
    pub(crate) fn new(config_dir: PathBuf) -> Self {
        Self {
            step: WizardStep::Name,
            name: String::new(),
            host: String::new(),
            port: 23,
            answered: Vec::new(),
            input: Input::default(),
            error: None,
            config_dir,
        }
    }

    pub(crate) fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> WizardOutcome {
        if code == KeyCode::Esc {
            return WizardOutcome::Cancelled;
        }
        if code != KeyCode::Enter {
            self.input
                .handle_event(&Event::Key(crossterm::event::KeyEvent::new(
                    code, modifiers,
                )));
            return WizardOutcome::Continue;
        }

        let value = self.input.value().trim().to_string();
        self.error = match self.step {
            WizardStep::Name if value.is_empty() || value.contains(['/', '\\']) => {
                Some("a profile name can't be empty or contain a slash".to_string())
            }
            WizardStep::Name if value.chars().count() > MAX_PROFILE_NAME_LEN => Some(format!(
                "a profile name can't be longer than {MAX_PROFILE_NAME_LEN} characters"
            )),
            // Only reachable from `/newprofile`: the first-run form only
            // ever shows with zero profiles saved, so no name can collide
            // there.
            WizardStep::Name if config::profile_path(&self.config_dir, &value).exists() => {
                Some(format!("a profile named `{value}` already exists"))
            }
            WizardStep::Name => {
                self.name = value.clone();
                self.answered.push(("Name", value));
                self.step = WizardStep::Host;
                None
            }
            WizardStep::Host if value.is_empty() => Some("a host is required".to_string()),
            WizardStep::Host => {
                self.host = value.clone();
                self.answered.push(("Host", value));
                self.step = WizardStep::Port;
                None
            }
            WizardStep::Port if value.is_empty() => {
                self.answered.push(("Port", "23".to_string()));
                self.step = WizardStep::Tls;
                None
            }
            WizardStep::Port => match value.parse() {
                Ok(parsed) => {
                    self.port = parsed;
                    self.answered.push(("Port", value));
                    self.step = WizardStep::Tls;
                    None
                }
                Err(_) => Some("port must be a number from 1-65535".to_string()),
            },
            WizardStep::Tls => {
                let tls = matches!(value.to_ascii_lowercase().as_str(), "y" | "yes");
                return WizardOutcome::Done(config::NewProfile {
                    name: self.name.clone(),
                    host: self.host.clone(),
                    port: self.port,
                    tls,
                });
            }
        };
        self.input = Input::default();
        WizardOutcome::Continue
    }
}

/// Every client command, as one type (#7).
///
/// `submit_input` grew from three `line.trim() ==` comparisons to twelve,
/// and #108 added a second dispatcher beside it for the empty client —
/// two if-chains over the same vocabulary, which is exactly how a command
/// comes to work in one place and not the other. One enum, one parser, and
/// matches the compiler checks for exhaustiveness: a new command cannot be
/// added and then forgotten in the other dispatcher, because the code will
/// not build until every arm exists.
///
/// Not a table of function pointers. The bodies are `async`, take
/// different arguments, and half of them need `&mut AppState` across an
/// await — boxing a future per command to fit them into one signature
/// would cost more than the if-chain ever did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ClientCommand {
    Help,
    Reload,
    Update,
    Config,
    NewProfile,
    Connect(String),
    Disconnect,
    Errors,
    Map,
    Comms,
    Goto(String),
    Corpse,
    Mark(String),
    Send(String),
}

impl ClientCommand {
    /// The line, or `None` if it is not a client command at all — in which
    /// case it belongs to the MUD, which is why an unknown `/word` is left
    /// alone rather than refused: plenty of MUDs use `/` themselves.
    ///
    /// The argument is whatever follows the first space, trimmed. Commands
    /// that take none ignore it, which keeps `/map ` (a stray space) doing
    /// what `/map` does rather than falling through to the server.
    pub(crate) fn parse(line: &str) -> Option<Self> {
        let line = line.trim();
        let (word, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let rest = rest.trim().to_string();
        Some(match word {
            HELP_COMMAND => Self::Help,
            RELOAD_COMMAND => Self::Reload,
            UPDATE_COMMAND => Self::Update,
            CONFIG_COMMAND => Self::Config,
            NEWPROFILE_COMMAND => Self::NewProfile,
            CONNECT_COMMAND => Self::Connect(rest),
            DISCONNECT_COMMAND => Self::Disconnect,
            ERRORS_COMMAND => Self::Errors,
            MAP_COMMAND => Self::Map,
            COMMS_COMMAND => Self::Comms,
            GOTO_COMMAND => Self::Goto(rest),
            CORPSE_COMMAND => Self::Corpse,
            MARK_COMMAND => Self::Mark(rest),
            SEND_COMMAND => Self::Send(rest),
            _ => return None,
        })
    }

    /// Every command, for anything that needs to show them all (#43).
    ///
    /// Spelled out rather than derived, because a variant with an argument
    /// has no single value to enumerate — and because a list the compiler
    /// cannot check is exactly what this type exists to replace, the test
    /// `every_command_is_in_the_palette` walks `parse` over the command
    /// names to prove nothing is missing from it.
    pub(crate) fn all() -> Vec<Self> {
        vec![
            Self::Help,
            Self::Errors,
            Self::Connect(String::new()),
            Self::Disconnect,
            Self::NewProfile,
            Self::Map,
            Self::Comms,
            Self::Goto(String::new()),
            Self::Corpse,
            Self::Mark(String::new()),
            Self::Send(String::new()),
            Self::Config,
            Self::Reload,
            Self::Update,
        ]
    }

    /// The name as typed, without any argument.
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Help => HELP_COMMAND,
            Self::Reload => RELOAD_COMMAND,
            Self::Update => UPDATE_COMMAND,
            Self::Config => CONFIG_COMMAND,
            Self::NewProfile => NEWPROFILE_COMMAND,
            Self::Connect(_) => CONNECT_COMMAND,
            Self::Disconnect => DISCONNECT_COMMAND,
            Self::Errors => ERRORS_COMMAND,
            Self::Map => MAP_COMMAND,
            Self::Comms => COMMS_COMMAND,
            Self::Goto(_) => GOTO_COMMAND,
            Self::Corpse => CORPSE_COMMAND,
            Self::Mark(_) => MARK_COMMAND,
            Self::Send(_) => SEND_COMMAND,
        }
    }

    /// What it does, in the words a player would search for rather than
    /// the ones the code uses — someone hunting for the map types "map",
    /// but someone who wants to stop seeing chatter types "hide", and
    /// neither knows the pane is called a channel.
    fn describes(&self) -> &'static str {
        match self {
            Self::Help => "show every keybinding",
            Self::Reload => "reload rules and modules from disk",
            Self::Update => "install a newer release",
            Self::Config => "edit this profile",
            Self::NewProfile => "make a new character profile",
            Self::Connect(_) => "open another character",
            Self::Disconnect => "close the character you are typing at",
            Self::Errors => "show warnings the client kept",
            Self::Map => "show or hide the map column",
            Self::Comms => "show or hide the comms column",
            Self::Goto(_) => "walk to a room you have been to",
            Self::Corpse => "walk back to your corpse",
            Self::Mark(_) => "label this room on the map",
            Self::Send(_) => "run a command as another character",
        }
    }

    /// Whether the player has to type something after the name, so the
    /// palette can hand them a half-written line instead of running
    /// something incomplete.
    pub(crate) fn takes_an_argument(&self) -> bool {
        matches!(
            self,
            Self::Connect(_) | Self::Goto(_) | Self::Send(_) | Self::Mark(_)
        )
    }

    /// Whether this means anything with no character connected (#108).
    ///
    /// Exhaustive on purpose: the empty client used to have its own list
    /// of three commands, and the honest way to keep the two in step is to
    /// make the compiler ask about every command, once, here.
    pub(crate) fn needs_a_character(&self) -> bool {
        match self {
            // Opening or making one, and the listing that names them.
            // Warnings outlive the character they were about, and the
            // ones from startup never had one (#18).
            Self::Connect(_) | Self::NewProfile | Self::Help | Self::Errors => false,
            Self::Reload
            | Self::Update
            | Self::Config
            | Self::Disconnect
            | Self::Map
            | Self::Comms
            | Self::Goto(_)
            | Self::Corpse
            | Self::Mark(_)
            | Self::Send(_) => true,
        }
    }
}

/// What the palette can act on (#43).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteEntry {
    Command(ClientCommand),
    /// A profile on disk, offered as "connect this character" — the
    /// commonest thing a multi-boxer wants that is not a command at all.
    Profile(String),
}

impl PaletteEntry {
    pub fn label(&self) -> String {
        match self {
            Self::Command(command) => command.name().to_string(),
            Self::Profile(name) => format!("{CONNECT_COMMAND} {name}"),
        }
    }

    pub fn describes(&self) -> String {
        match self {
            Self::Command(command) => command.describes().to_string(),
            Self::Profile(name) => format!("open {name}"),
        }
    }
}

/// The command palette (#43).
pub struct Palette {
    pub input: Input,
    /// Which row is selected, as an index into the *filtered* list — the
    /// list is rebuilt on every keystroke, so an index into the full one
    /// would point somewhere else the moment a letter is typed.
    pub selected: usize,
}

/// Scores `text` against a query typed a few letters at a time.
///
/// Subsequence matching rather than substring: "dc" should find
/// `/disconnect`, which is the whole point of typing three letters instead
/// of eleven. Consecutive matches and matches at a word boundary score
/// higher, so `/map` beats `/mark` for "map" even though both contain
/// those letters in order.
///
/// Hand-rolled rather than a dependency: this is thirty lines against a
/// list of twenty entries, and a fuzzy-matching crate would be a build
/// dependency for the rest of the client's life (TAD §2.1 asks for
/// established crates where the work is real; here it is not).
/// How much a name match outranks a description match. Larger than any
/// score `fuzzy_score` can return, so the two never interleave.
pub(crate) const NAME_MATCH: i32 = 1_000_000;

pub(crate) fn fuzzy_score(text: &str, query: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let text_lower = text.to_lowercase();
    let mut score = 0;
    let mut last_match: Option<usize> = None;
    let mut from = 0;
    for needle in query.to_lowercase().chars() {
        if needle.is_whitespace() {
            continue;
        }
        let at = text_lower[from..].find(needle)? + from;
        // A letter that follows the previous one is worth more than one
        // found halfway across the string: "conn" naming `/connect` should
        // outrank the same letters scattered through a description.
        score += match last_match {
            Some(previous) if at == previous + 1 => 8,
            _ => 1,
        };
        // And a letter starting a word is what someone typing initials
        // means — "np" for `/newprofile`.
        let starts_word = at == 0
            || text_lower[..at]
                .chars()
                .next_back()
                .is_some_and(|before| before == ' ' || before == '/' || before == '-');
        if starts_word {
            score += 6;
        }
        last_match = Some(at);
        from = at + needle.len_utf8();
    }
    // Shorter names win ties: with "map" matching both, `/map` is what was
    // meant and `/mark` is not.
    Some(score * 100 - text.chars().count() as i32)
}

pub struct ConnectTarget {
    /// How other sessions address this one (§7.5): the profile name, with a
    /// numeric suffix when a profile is opened twice.
    pub name: String,
    pub host: String,
    pub port: u16,
    pub tls: Option<crate::net::TlsConfig>,
    pub record: Option<PathBuf>,
    pub charset: Charset,
    /// The compiled rule set, and how to rebuild it for `/reload`.
    pub rules: Rules,
    /// How this session treats commands injected into it (§7.5).
    pub cross: CrossSession,
    /// Tints this character's border and tab entry (§11).
    pub color: Option<Color>,
    /// Which world's map this character shares, when `host:port` is not
    /// the right answer (§16).
    pub world: Option<String>,
    /// Answers the server's opening prompts, if the profile asked for it.
    pub login: Option<session::login::Autologin>,
    /// Offer to keep the password the player types at the login prompt
    /// (docs/ARCHITECTURE.md §13), so a stored password does not have to be
    /// set up ahead of time.
    pub offer_password_save: bool,
    /// Where to append this session's transcript, if the profile's `log:`
    /// is set (§8, §12). `None` means disk logging is off.
    pub log_path: Option<PathBuf>,
}

/// Where a session's rules came from, so `/reload` can recompile them from
/// disk without reconnecting (docs/ARCHITECTURE.md §7.3).
pub struct Rules {
    pub engine: Engine,
    pub config_dir: PathBuf,
    pub profile: Option<String>,
}

/// Everything one character's pane needs. Nothing here is shared: buffer
/// isolation between sessions is structural (docs/ARCHITECTURE.md §3).
/// What a session pane *shows*: the state `ui` reads to draw it, and
/// nothing else (#11, docs/ARCHITECTURE.md §4).
///
/// Split out of `SessionPane` because that struct had grown to 38 fields
/// holding a socket's command sender, an open transcript file and a
/// plaintext password alongside the scrollback — so every feature that drew
/// something new (the party HUD, the map column) reached past a password to
/// get at it. Nothing here is a handle, a file, or a secret: the type
/// carries only what a renderer needs, which is what makes it safe to hand
/// to one.
///
/// `SessionPane` still owns this and still writes it — `push_line` and the
/// history walk stay whole, on the far side of the boundary, because they
/// are also where masked input is kept out of the scrollback and the
/// transcript at one choke point (§13).
#[derive(Debug)]
pub struct SessionView {
    pub name: String,
    pub scrollback: VecDeque<RetainedLine>,
    /// Text pinned above the input line; empty means no prompt.
    pub prompt: String,
    pub input: Input,
    /// The rest of the word the input line would complete to — drawn as a
    /// ghost past the cursor, and appended to what is sent. Kept rather
    /// than recomputed where it is drawn, so what the player is looking at
    /// and what Enter sends cannot be two different answers.
    pub suggestion: Option<String>,
    pub status: String,
    /// Server took over echoing (Telnet ECHO): hide what we type.
    pub masked: bool,
    /// Transport security shown in the pane title ("TLS", "TLS pinned", …).
    pub security: String,
    /// Last measured round trip, formatted for the pane title ("42ms").
    /// Empty until the first one completes, and again whenever the
    /// connection stops carrying data — a stale number would read as a live
    /// one (§11).
    pub latency: String,
    /// Whether the session owns the prompt row. Keeping the row reserved for
    /// the whole session keeps the layout — and so the NAWS pane size —
    /// stable as prompts come and go.
    pub connected: bool,
    /// Raw server-data lines, newest last, each tagged `[GMCP]` or `[MSDP]`
    /// by origin — the inspector view (docs/ARCHITECTURE.md §6.3, §14 M6).
    pub inspector_log: VecDeque<String>,
    /// Lines that arrived while the pane was not focused (§11).
    pub unread: usize,
    /// How much trouble this character is in, if any — the fraction of
    /// health left, once it is low enough to be worth saying (§11.7).
    ///
    /// Kept here rather than derived where it is drawn, because the bell
    /// rings on the *edge* — the moment a character enters trouble — and
    /// only something that remembers the last pass can see an edge. The
    /// strip and the tab bar then read one answer instead of each
    /// recomputing their own.
    pub distress: Option<f64>,
    /// The profile's `color:`, if it set one — the pane border and this
    /// session's tab entry are drawn in it (§11).
    pub color: Option<Color>,
    /// Distance back from the tail, in wrapped rows: 0 is pinned to the
    /// newest content, larger is further back in history. Storing distance
    /// rather than an absolute position means a new line arriving needs no
    /// compensation to keep a scrolled reader's view stable — the buffer
    /// grows underneath the same offset (docs/ARCHITECTURE.md §11.5).
    pub back_offset: usize,
    /// Where on the world's map this character is standing (§16).
    ///
    /// The map itself is *not* here: it belongs to the world, and lives in
    /// `AppState::maps` keyed by [`SessionPane::map_key`]. Where a
    /// character stands is the part that is genuinely theirs.
    pub current_room: Option<crate::map::RoomId>,
    /// Where this character died last, for `/corpse` (§16). Set by a
    /// trigger's `corpse:`, never cleared: a corpse run that reaches the
    /// body leaves the mark behind rather than forgetting a room the
    /// player may still be walking back and forth to, and the next death
    /// overwrites it anyway.
    pub corpse: Option<crate::map::RoomId>,
}

/// A session's identity, fixed for as long as the session exists and
/// never reused (#94).
///
/// The panes live in a `Vec` and are drawn in that order, so an index is
/// the right way to say *where* a pane is. It is the wrong way to say
/// *which character* — removing one renumbers every pane above it, and
/// `Focus`/`input_session` would then quietly name a different person.
/// Position is a property of the list; identity is a property of the
/// session, and the two stopped agreeing the moment `/connect` shipped
/// without a way to disconnect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(u64);

impl SessionId {
    /// Handed out in order, never reused within a run: a removed session's
    /// id must not come back and make a stale `Focus` valid again.
    pub(crate) fn next() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }
}

pub struct SessionPane {
    /// Who this pane is, as against where it is (#94).
    pub id: SessionId,
    /// Everything `ui` draws from (#11).
    pub view: SessionView,
    /// Words this MUD has printed, ranked by recency (§11.3). Per session,
    /// because each character is standing somewhere else.
    pub(crate) vocabulary: crate::complete::Vocabulary,
    /// The input value Escape was pressed on. A dismissal lasts exactly as
    /// long as that line does: type another character and the suggestion is
    /// welcome again, because the question has changed.
    pub(crate) dismissed: Option<String>,
    /// `autocomplete:` — off makes this whole path inert (§11.3).
    pub(crate) autocomplete: bool,
    /// Whether a GMCP message has reached this session — drives the
    /// inspector's title (§6.3): a client that only ever names GMCP would
    /// look broken on an MSDP-only MUD, so the title says what actually
    /// showed up rather than what the client merely supports.
    pub(crate) gmcp_seen: bool,
    /// The MSDP twin of `gmcp_seen`.
    pub(crate) msdp_seen: bool,
    /// Commands typed here, oldest first, exactly as typed — before alias
    /// expansion and `;` splitting, because the alias is what the player is
    /// choosing to repeat (docs/ARCHITECTURE.md §11.3).
    pub(crate) history: VecDeque<String>,
    /// How far back through `history` the recall walk has gone. `None` means
    /// the input line is the player's own, not a recalled copy.
    pub(crate) history_pos: Option<usize>,
    /// What was typed when the walk began, restored on walking back past the
    /// newest entry. Losing a half-written line to a stray arrow key is what
    /// teaches people to distrust history.
    pub(crate) history_draft: String,
    /// How many entries `history` keeps (`history_size`, §11.3).
    pub(crate) history_limit: usize,
    /// The status line shown once the connection is up.
    pub(crate) connected_status: String,
    /// `None` once the session has ended, so its receiver stops being polled.
    pub(crate) events: Option<mpsc::Receiver<SessionEvent>>,
    pub(crate) commands: mpsc::Sender<SessionCommand>,
    /// Rule provenance for `/reload`.
    pub(crate) rules: (PathBuf, Option<String>),
    /// Whether a password typed here should still be offered to the keyring
    /// (§13). Cleared once the offer has been made, so it happens once.
    pub(crate) offer_password_save: bool,
    /// A password typed at a masked prompt, held only until the player
    /// answers the offer to save it. Never echoed, recalled, or logged —
    /// the same rule that keeps it out of scrollback and history.
    pub(crate) pending_password: Option<String>,
    pub(crate) cross: CrossSession,
    /// Last pane size reported to the server, so a redraw that did not
    /// change this pane sends no NAWS (docs/ARCHITECTURE.md §6.2).
    pub(crate) last_size: Option<(u16, u16)>,
    /// How many lines `scrollback` keeps (`scrollback_size`, §8).
    pub(crate) scrollback_limit: usize,
    /// The open transcript file, if the profile's `log:` is set (§8, §12).
    /// Every line that reaches `push_line` is appended here too — the same
    /// choke point that already keeps masked lines out of scrollback keeps
    /// them out of the transcript, for free (§13).
    pub(crate) log: Option<std::io::BufWriter<std::fs::File>>,
    /// Which world's map this session reads and writes (§16) — the key
    /// into `AppState::maps`. Held rather than recomputed because the pane
    /// outlives the target it came from.
    pub map_key: String,
    /// A `/goto` walk in progress (§16). `None` when nothing is walking.
    pub walk: Option<Walk>,
}

/// One world's map, and everything about writing it back (§16).
///
/// Shared by every character on that world rather than copied per session,
/// because a map *is* a property of the world: two characters on one MUD
/// walk the same rooms and read and write one file. Copying it per session
/// meant two in-memory maps drifting apart between saves — one character
/// unmarking a room while the other went on holding the label, one
/// character discovering a corridor the other could not see — and the file
/// reconciling them only at the next launch. Sharing makes that
/// divergence unrepresentable rather than something each writer has to
/// remember to fan out.
#[derive(Default)]
pub struct WorldMap {
    pub map: crate::map::Map,
    /// Whether this world has learned something not yet on disk (§16).
    /// Saving is a merge, so writing often is safe; writing when nothing
    /// has changed is just disk churn.
    pub dirty: bool,
    /// Whether the player has already been told that saving is failing.
    /// One notice per run of failures, not one per attempt (§16).
    pub save_failed: bool,
    /// Rooms whose mark was explicitly set or cleared since the last
    /// successful write (§16).
    ///
    /// A save merges into whatever is already on disk, and that merge must
    /// not erase a mark this run never touched — another *process*, or a
    /// `/connect` elsewhere, may have written one. But the same "never
    /// erase" rule means `None` can never win against an existing `Some`,
    /// so *removing* a mark would never reach disk at all. These are the
    /// rooms where the absence is not "I never knew", it is "I just
    /// cleared it", and `config::save_map` applies them authoritatively
    /// instead of merging them.
    pub explicit_marks: HashSet<crate::map::RoomId>,
}

/// The labels `/mark` offers when asked with no argument (§16).
///
/// Not an attempt to cover every MUD — a shortlist of what rooms are
/// commonly *for*, so the common case is two keystrokes instead of typing.
/// Anything not here is still `/mark <whatever you like>`; this is a
/// convenience, not a vocabulary. First letters are kept distinct, since
/// that letter is what the map has room to draw.
pub const MARK_SUGGESTIONS: &[&str] = &[
    "shop", "water", "rent", "bank", "healer", "trainer", "forge", "portal", "quest",
];

/// What taking the highlighted row of the `/mark` chooser means.
pub enum MarkChoice {
    /// Write this label.
    Label(String),
    /// Ask for one the list does not have.
    Custom,
    /// Take off the label that is there.
    Remove,
}

/// The chooser `/mark` opens when it is not told what to write (§16).
pub struct MarkMenu {
    /// The room being labelled, captured when the menu opened — walking
    /// away with it up must not relabel wherever you ended up.
    pub at: crate::map::RoomId,
    pub selected: usize,
    /// `Some` when the room already has a label, so the list can offer to
    /// take it off again.
    pub existing: Option<String>,
    /// What the player is typing, once they have asked for a label of
    /// their own. `None` while the list is being browsed.
    pub typing: Option<String>,
}

/// The row that asks for a label the list does not offer.
pub(crate) const MARK_CUSTOM_ROW: &str = "something else…";

impl MarkMenu {
    /// The list as shown: the suggestions, a way to write your own, and —
    /// when there is one — a way to take the current label off.
    pub fn entries(&self) -> Vec<String> {
        let mut entries: Vec<String> = MARK_SUGGESTIONS.iter().map(|s| s.to_string()).collect();
        entries.push(MARK_CUSTOM_ROW.to_string());
        if let Some(existing) = &self.existing {
            entries.push(format!("remove `{existing}`"));
        }
        entries
    }

    /// What picking the highlighted row means.
    pub fn choice(&self) -> MarkChoice {
        match MARK_SUGGESTIONS.get(self.selected) {
            Some(label) => MarkChoice::Label((*label).to_string()),
            None if self.selected == MARK_SUGGESTIONS.len() => MarkChoice::Custom,
            None => MarkChoice::Remove,
        }
    }
}

/// One `/goto` walk in progress. Exactly one step is ever in flight: the
/// rest of `remaining` waits for `expecting` to be confirmed before the next
/// one is sent, because a route learned earlier can be stale by the time it
/// is walked — a gate open when the path was found can be locked now, and
/// firing the rest of the path from wherever a failed step actually lands
/// would walk the character somewhere they never asked to go.
pub struct Walk {
    /// Directions still to send once the step in flight is confirmed.
    pub(crate) remaining: VecDeque<String>,
    /// The room the step currently in flight should land in.
    pub(crate) expecting: crate::map::RoomId,
    /// Where the walk is headed, for the arrival message.
    pub(crate) destination: crate::map::RoomId,
}

impl SessionPane {
    /// Adds a line's words to what this session can complete from, and
    /// re-answers the input line's question in their light — a name that
    /// arrives while you are half-way through typing it is exactly the
    /// case worth catching.
    ///
    /// Only what the *server* said. The client's own notices, a rule's
    /// echo, and our own commands coming back are not names the MUD
    /// knows, and a warning's vocabulary completing into a command is
    /// noise at best.
    pub(crate) fn learn_words(&mut self, text: &str, origin: &Origin) {
        if !self.autocomplete || *origin != Origin::Server {
            return;
        }
        self.vocabulary.learn(&crate::scrollback::strip_ansi(text));
        self.refresh_suggestion();
    }

    /// Recomputes what the input line would complete to (§11.3).
    ///
    /// Called after anything that could change the answer — a keystroke, a
    /// history walk, or a line arriving that teaches a new name — rather
    /// than at render time, because the same answer has to serve the ghost
    /// on screen and the text Enter actually sends.
    pub(crate) fn refresh_suggestion(&mut self) {
        self.view.suggestion = self.completion();
    }

    fn completion(&self) -> Option<String> {
        if !self.autocomplete {
            return None;
        }
        // Never into a password. The server is hiding this line, the
        // scrollback never saw it, and a ghost drawn past the asterisks
        // would be guessing at it out loud (§13).
        if self.view.masked {
            return None;
        }
        let value = self.view.input.value();
        if self.dismissed.as_deref() == Some(value) {
            return None;
        }
        // Only at the end of the line. Completing a word in the middle
        // would have to decide what happens to the text after it, and
        // there is no answer to that which is obvious enough to do silently.
        if self.view.input.cursor() != value.chars().count() {
            return None;
        }
        // The trailing run of non-space, which is empty when the line ends
        // in a space — a word not yet begun is not a prefix.
        let word = value.rsplit(char::is_whitespace).next()?;
        self.vocabulary.suggest(word)
    }

    /// Tab: take the guess — put it in the line for real, cursor after it.
    ///
    /// Enter already sends the ghost (§11.3), so this changes nothing about
    /// what a completed line means; what it changes is that you can *keep
    /// typing*. Until now the only way to extend a guessed word was to type
    /// the rest of it yourself, because the ghost was not text you had a
    /// cursor in.
    ///
    /// Says whether it accepted anything, so a Tab with no guess can fall
    /// through to meaning whatever it meant before.
    pub(crate) fn accept_suggestion(&mut self) -> bool {
        let Some(rest) = self.view.suggestion.take() else {
            return false;
        };
        // `with_value` leaves the cursor at the end, which is where accepting
        // a completion has to put it.
        self.view.input = Input::default().with_value(format!("{}{rest}", self.view.input.value()));
        // A word completed can still be the prefix of a longer one, so ask
        // again rather than assuming this was the last word it could be.
        self.refresh_suggestion();
        true
    }

    /// Escape: send what I typed, not what you guessed.
    pub(crate) fn dismiss_suggestion(&mut self) {
        self.dismissed = Some(self.view.input.value().to_string());
        self.view.suggestion = None;
    }

    /// What the input line means, ghost included — what Enter sends.
    pub(crate) fn completed_input(&self) -> String {
        match &self.view.suggestion {
            Some(rest) => format!("{}{rest}", self.view.input.value()),
            None => self.view.input.value().to_string(),
        }
    }

    pub(crate) fn push_line(&mut self, line: RetainedLine) {
        if let Some(log) = &mut self.log {
            use std::io::Write as _;
            // Best-effort: a full disk or a yanked log file must not take
            // the session down. Silently stop trying rather than repeat a
            // failing write every line.
            if writeln!(log, "{}", line.text)
                .and_then(|()| log.flush())
                .is_err()
            {
                self.log = None;
            }
        }
        self.view.scrollback.push_back(line);
        if self.view.scrollback.len() > self.scrollback_limit {
            self.view.scrollback.pop_front();
        }
    }

    /// Records a submitted command for recall, and ends any walk in progress
    /// so the next `Up` starts from the newest entry again (§11.3).
    pub(crate) fn push_history(&mut self, line: &str) {
        self.history_pos = None;
        self.history_draft.clear();
        // Nothing to recall from an empty line, and a masked line is a
        // password: it stays out of history for the same reason it stays out
        // of scrollback (§13).
        if line.is_empty() || self.view.masked || self.history_limit == 0 {
            return;
        }
        // Only *consecutive* duplicates collapse: a spammed `look` costs one
        // slot, but a repeat further back keeps its place, since where a
        // command sits in the sequence is information.
        if self.history.back().is_some_and(|last| last == line) {
            return;
        }
        self.history.push_back(line.to_string());
        while self.history.len() > self.history_limit {
            self.history.pop_front();
        }
    }

    /// Walks the recall list, `back` towards older entries. Returns whether
    /// anything moved, so the caller can leave the key to the input widget.
    pub(crate) fn walk_history(&mut self, back: bool) -> bool {
        // Recall into a masked prompt would send an old command as the
        // password, in the clear, to whatever is listening.
        if self.view.masked || self.history.is_empty() {
            return false;
        }
        let next = match (self.history_pos, back) {
            // Starting a walk: stash the live line before it is overwritten.
            (None, true) => {
                self.history_draft = self.view.input.value().to_string();
                Some(self.history.len() - 1)
            }
            (None, false) => return false,
            (Some(0), true) => return false,
            (Some(pos), true) => Some(pos - 1),
            (Some(pos), false) if pos + 1 < self.history.len() => Some(pos + 1),
            // Past the newest entry: back to what the player was typing.
            (Some(_), false) => None,
        };
        let line = match next {
            Some(pos) => self.history[pos].clone(),
            None => std::mem::take(&mut self.history_draft),
        };
        self.history_pos = next;
        // A recalled entry is a copy — editing it must never rewrite history.
        self.view.input = Input::new(line);
        true
    }

    pub(crate) fn push_gmcp(&mut self, package: String, payload: Option<String>) {
        let line = match payload {
            Some(payload) => format!("{package} {payload}"),
            None => package,
        };
        self.push_inspector_line("GMCP", &line);
        self.gmcp_seen = true;
    }

    /// One line per flattened key/value pair, mirroring GMCP's one
    /// package-per-line grain — MSDP's own wire form has no equivalent of
    /// a package payload to show as a single line (§6.3).
    pub(crate) fn push_msdp(&mut self, pairs: Vec<(String, String)>) {
        for (key, value) in pairs {
            self.push_inspector_line("MSDP", &format!("{key} {value}"));
        }
        self.msdp_seen = true;
    }

    fn push_inspector_line(&mut self, tag: &str, line: &str) {
        // Straight from the wire: neither protocol's raw value passed
        // through the line pipeline's control-byte filter, and the
        // inspector renders it with `Line::raw` rather than through the
        // ANSI parser — so nothing else stands between a server's escape
        // sequence and the terminal (§13). Shown, not stripped: this view
        // exists to reveal exactly what arrived.
        let line = crate::scrollback::escape_controls(&format!("[{tag}] {line}"));
        self.view.inspector_log.push_back(line);
        if self.view.inspector_log.len() > INSPECTOR_LOG_LIMIT {
            self.view.inspector_log.pop_front();
        }
    }

    /// The inspector title's protocol descriptor: says which protocol(s)
    /// actually produced data, not which the client merely supports, so an
    /// MSDP-only MUD doesn't leave the pane looking like a broken GMCP view
    /// (§6.3).
    pub(crate) fn inspector_title(&self) -> &'static str {
        match (self.gmcp_seen, self.msdp_seen) {
            (true, true) => "GMCP + MSDP inspector",
            (true, false) => "GMCP inspector",
            (false, true) => "MSDP inspector",
            (false, false) => "server data — nothing received yet",
        }
    }
}

/// A channel pane: app-level, aggregating matching lines from every session
/// that isn't excluded by `session:` (docs/ARCHITECTURE.md §11.1).
pub struct ChannelPane {
    pub config: Channel,
    pub lines: VecDeque<RetainedLine>,
    pub unread: usize,
    /// How many lines `lines` keeps (`scrollback_size`, §8).
    pub scrollback_limit: usize,
    /// Same "distance from the tail" scroll state as `SessionPane`, and the
    /// same sticky-bottom behaviour (§11.5).
    pub back_offset: usize,
}

/// How far back a copy of a broadcast may lag its siblings and still be
/// recognised as the same message. The copies are one server write fanned
/// out over several sockets, so they arrive milliseconds apart; the seconds
/// here are headroom for a session that stalled, not a guess at the gap.
pub(crate) const HEARD_TOGETHER: chrono::TimeDelta = chrono::TimeDelta::seconds(2);

impl ChannelPane {
    fn push(&mut self, line: RetainedLine) {
        self.lines.push_back(line);
        if self.lines.len() > self.scrollback_limit {
            self.lines.pop_front();
        }
    }

    /// Records `name` on an entry this line is a duplicate of, if there is
    /// one, and reports whether it did (§11.1). One gossip heard by three
    /// characters is one message: three sessions each parse their own copy,
    /// and appending all three says the same sentence three times.
    ///
    /// Sameness is the plain-text projection — the copies may be coloured
    /// differently per character, and that must not make them different
    /// messages. A MUD that substitutes the recipient's name into the text
    /// defeats this, and correctly so: those copies genuinely do not read
    /// the same, and guessing which differences are cosmetic would collapse
    /// lines that are not duplicates at all.
    ///
    /// Requiring a *different* character is what keeps a genuine repeat
    /// apart — someone saying "hi" twice reaches the same session twice, so
    /// the second copy finds itself already listed and lands as its own
    /// entry. That, rather than the width of the window, is what does most
    /// of the work here.
    fn collapse_into_recent(&mut self, line: &RetainedLine, name: &str) -> bool {
        for existing in self.lines.iter_mut().rev() {
            if line.at.signed_duration_since(existing.at) > HEARD_TOGETHER {
                return false;
            }
            if existing.plain() != line.plain() {
                continue;
            }
            let Origin::Session(heard_by) = &mut existing.origin else {
                continue;
            };
            if heard_by.iter().any(|who| who == name) {
                return false;
            }
            heard_by.push(name.to_string());
            return true;
        }
        false
    }
}

/// Which pane has focus. Channel panes are focusable, but the input line
/// stays bound to `input_session` — reading comms must never silently
/// change which character your commands go to (§11.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Session(SessionId),
    Channel(usize),
    /// The map column. Unlike the others it holds no buffer, so focusing
    /// it moves neither the input line nor the scroll keys — what it takes
    /// is the arrows, which drive the map cursor (§16). That is the whole
    /// reason it is a focus stop rather than only a mode: reaching the map
    /// is the same gesture as reaching any other pane.
    Map,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutMode {
    /// One session full-screen, with a tab bar.
    Tabs,
    /// Every session side by side.
    Splits,
}

/// Everything the UI needs to render a frame.
/// Whether the environment asked for no colour (no-color.org).
///
/// The variable's *presence* is the signal, not its value: `NO_COLOR=0`
/// still means no colour, which is the part of the convention that is easy
/// to get wrong by reaching for a bool parser. Empty is the one documented
/// exception, and means nothing was asked.
pub fn no_color_requested(value: Option<&str>) -> bool {
    matches!(value, Some(value) if !value.is_empty())
}

pub struct AppState {
    /// The environment asked for no colour (#120, no-color.org). Read once
    /// at startup and carried, rather than consulted where colour is drawn:
    /// an env lookup per cell would be absurd, and a test that had to set a
    /// process-wide variable could not run beside its neighbours.
    pub no_color: bool,
    pub sessions: Vec<SessionPane>,
    pub channels: Vec<ChannelPane>,
    pub focus: Focus,
    /// The session the input line is bound to — the last focused session.
    pub input_session: SessionId,
    pub layout: LayoutMode,
    pub show_channels: bool,
    /// Live width of the channel column, in columns. State, not layout:
    /// `ui::layout` recomputes every rect from it each frame, and the keys
    /// only move this number (docs/ARCHITECTURE.md §11.4).
    pub channel_width: u16,
    /// Whether the docked map column is up (§16).
    pub show_map: bool,
    /// Live width of the map column, on the same terms as `channel_width`.
    pub map_width: u16,
    /// Whether the focused session's pane shows `inspector_log` instead of
    /// its scrollback.
    pub show_inspector: bool,
    /// The party strip: every character's vitals side by side (§11.6).
    pub show_hud: bool,
    /// Whether the character panes stamp each line with the time it
    /// arrived (§11.3). App-level, not per pane: "when did that happen"
    /// is a question about the evening, not about one character.
    pub show_timestamps: bool,
    /// Draw the map with pixels, and how big a cell is in them (§16).
    /// `None` where the player has not asked, or the terminal never said
    /// how big its cells are — an image sized against a guess would span
    /// the wrong number of cells and shove the panes beside it sideways.
    pub map_cell_px: Option<(u16, u16)>,
    /// Whether the help overlay is up (§11.2).
    pub show_help: bool,
    /// How far down the help listing has been scrolled, in rows. The
    /// listing outgrew a short terminal (§11.2), so it has to be reachable
    /// rather than merely clipped.
    pub help_scroll: u16,
    /// The live bindings, so the input hint and the help overlay both read
    /// what the event loop actually matches against — never a second copy
    /// that can drift (§11.2).
    pub keybinds: Keybinds,
    /// The profile editor, when it is open (§10.2). `Some` means it owns
    /// the keyboard and paints over every pane, like the help overlay —
    /// but sessions keep running behind it.
    pub config_editor: Option<ui::config_editor::ConfigEditorState>,
    /// A save the editor asked for on the last keypress, drained by
    /// `event_loop` right after `handle_key` returns — saving needs async
    /// IO that `handle_key` itself, being sync, cannot do.
    pub(crate) config_editor_save: Option<config::SaveMode>,
    /// The reload keybind was pressed, drained the same way: `reload_rules`
    /// sends a `SessionCommand` over an async channel, which `handle_key`
    /// cannot do either (UX_REVIEW.md F — the same gap `/reload` typed as
    /// a command doesn't have, since `submit_input` is already async).
    pub(crate) reload_requested: bool,
    /// A scrollback line-cursor is active on the focused pane (§10.2/§11.5):
    /// `Some(back_offset)` is the line it currently highlights, measured the
    /// same way `SessionPane::back_offset` is — distance from the tail.
    pub line_cursor: Option<usize>,
    /// The room the map cursor is on, when it is up (§16). A room id rather
    /// than a grid position: the layout is rebuilt every frame, and a
    /// coordinate would quietly come to mean a different room the moment
    /// anything moved.
    pub map_cursor: Option<crate::map::RoomId>,
    /// Whether the map column sits inboard of comms — next to the
    /// character panes rather than out past them (#111). Persisted with
    /// the rest of the layout, so the toggle *is* the preference.
    pub map_first: bool,
    /// Client-generated warnings, oldest first (#18).
    ///
    /// App-level rather than per-session: a rejected keybind at startup or
    /// a profile that would not load belongs to no character, and the ones
    /// that do belong to one are still worth reading after that character
    /// has been disconnected.
    pub errors: VecDeque<String>,
    /// How many have arrived since the panel was last looked at. The count
    /// is the whole point of keeping it — a warning nobody sees is the
    /// thing #18 is about, and a panel nobody knows to open is the same
    /// failure wearing a hat.
    pub errors_unread: usize,
    pub show_errors: bool,
    /// The command palette, when it is open (#43).
    pub palette: Option<Palette>,
    /// Set when the palette filled the input line with something complete,
    /// so the event loop submits it on the next turn. The palette runs
    /// inside `handle_key`, which is not async and so cannot submit.
    pub palette_submit: bool,
    /// The input line when no session is bound (#108).
    ///
    /// `SessionView` owns the ordinary one, which is right — it is that
    /// character's line, with that character's history. But the client can
    /// legitimately have no character at all: `mudular` run bare with a
    /// profile saved, or the last one closed with `/disconnect`. Without a
    /// line of its own that state swallowed every keystroke, so the one
    /// command that could leave it — `/connect` — was untypeable.
    pub shell_input: Input,
    /// What the empty client has been told, since it has no scrollback to
    /// say it in. Bounded because nothing prunes it: this is a waiting
    /// room, not a log.
    pub shell_notices: Vec<String>,
    /// Where the map picture sits relative to the bound character, in room
    /// steps (#58). `(0, 0)` is the historical behaviour: the character in
    /// the middle, the world sliding under them as they walk.
    ///
    /// A pan rather than a centre room, so that walking keeps whatever
    /// offset a character switch established instead of paging: the scene
    /// is rebuilt around wherever the character now stands, and holding
    /// the offset constant is exactly a one-room pan per step.
    pub map_pan: (i32, i32),
    /// The character and room `map_pan` was last computed against, so a
    /// switch can be told from a step by the same character.
    pub(crate) map_pan_for: Option<(SessionId, crate::map::RoomId)>,
    /// The grid the map pane drew into last frame, as `ui` reported it.
    /// One frame stale by construction: a pane that changed size since is
    /// re-judged on the next frame, and the cost of being wrong for one
    /// frame is a view that held still when it should have moved.
    pub map_grid: Option<ratatui::layout::Rect>,
    /// A room `Enter` on the map cursor asked to walk to. Serviced by the
    /// event loop rather than acted on in `handle_key`, which is not async
    /// — the same hand-off `reload_requested` uses.
    pub walk_requested: Option<crate::map::RoomId>,
    /// One map per world, keyed by `SessionPane::map_key` (§16). Every
    /// character on a MUD shares the entry, so what one of them learns or
    /// labels is what all of them see.
    pub maps: HashMap<String, WorldMap>,
    /// Where profiles live, so `/newprofile` knows where to save one
    /// without depending on which session happens to be bound (§15).
    /// Maps are written here too, for the same reason: a world's file
    /// belongs to the client, not to whichever character noticed a room.
    pub(crate) config_dir: PathBuf,
    /// `/newprofile`'s form, when it is open — reachable any time, not
    /// just at first run (§15, UX_REVIEW.md B). `Some` owns the keyboard
    /// and paints over every pane, like the config editor, and sessions
    /// keep running behind it the same way.
    /// The `/mark` chooser, when it is open (§16).
    pub mark_menu: Option<MarkMenu>,
    pub new_profile_wizard: Option<NewProfileWizard>,
    /// Every live session's own publish receiver, by name — the hub's half
    /// of the peer mesh (§7.5). Built once at startup from the same
    /// channels each session gets, and grown by `/connect`: a session
    /// added later hands its receiver here so the *next* `/connect` (or
    /// this one, reciprocally) can find it, and broadcasts it to every
    /// session already running via `SessionCommand::AddPeer` — the gap
    /// `ARCH_REVIEW.md` "Features that would break the architecture"
    /// named (a session added after startup was invisible to every
    /// existing one and vice versa, since the mesh was built once before
    /// `event_loop` and never revisited).
    pub(crate) peer_registry: crate::engine::Peers,
    /// Per-session limits new sessions need too, not just the ones spawned
    /// at startup (§8, §11.3) — `/connect` builds a session the same way
    /// `main.rs` does, so it needs the same numbers `event_loop` was
    /// handed once at launch.
    pub(crate) history_size: usize,
    pub(crate) scrollback_size: usize,
    /// `autocomplete:` — on the same terms, and read by `connect` (§11.3).
    pub(crate) autocomplete: bool,
    /// The install-wide `cross_session:` default, before a profile's own
    /// override — `/connect` needs it for the same reason it needs
    /// `history_size`/`scrollback_size`.
    pub(crate) cross_session_default: CrossSession,
}

/// `PgUp`/`PgDn`/`Home`/`End`, unmodified — built-in and unremappable, like
/// `Up`/`Down` (§11.3, §11.5). A modified chord (`Ctrl+PageUp`, etc.) is
/// left alone rather than swallowed, in case a terminal or a later binding
/// wants it.
pub(crate) fn is_scroll_key(code: KeyCode, modifiers: KeyModifiers) -> bool {
    matches!(
        code,
        KeyCode::PageUp | KeyCode::PageDown | KeyCode::Home | KeyCode::End
    ) && modifiers.is_empty()
}

impl AppState {
    /// The session the input line types into. Sessions are never removed
    /// from the list, so this index always resolves.
    pub fn bound(&self) -> Option<&SessionPane> {
        self.index_of(self.input_session)
            .and_then(|i| self.sessions.get(i))
    }

    /// The world this session belongs to, opened if this is the first
    /// character to reach it. Sessions never outlive their entry, so a
    /// caller with a valid index always gets one.
    pub fn world_mut(&mut self, index: usize) -> &mut WorldMap {
        let key = self
            .sessions
            .get(index)
            .map(|session| session.map_key.clone())
            .unwrap_or_default();
        self.maps.entry(key).or_default()
    }

    /// The map this session reads and writes, or an empty one for a
    /// session whose world has not been opened — the drawing path asks
    /// before there is anything to draw, and must not panic there.
    pub fn world(&self, index: usize) -> Option<&WorldMap> {
        self.maps.get(&self.sessions.get(index)?.map_key)
    }

    /// Where every *other* character on this session's world is standing
    /// (§16), for a map that shows the whole party rather than only the
    /// character being typed at.
    ///
    /// Characters with no room yet are absent rather than placed
    /// anywhere: a session that has not been told where it is cannot be
    /// drawn, and guessing would put an ally on a room they are not in.
    pub fn party_of(&self, index: usize) -> Vec<(crate::map::RoomId, String)> {
        let Some(session) = self.sessions.get(index) else {
            return Vec::new();
        };
        self.sessions
            .iter()
            .enumerate()
            .filter(|(other, _)| *other != index)
            .map(|(_, other)| other)
            .filter(|other| !other.map_key.is_empty() && other.map_key == session.map_key)
            .filter_map(|other| Some((other.view.current_room?, other.view.name.clone())))
            .collect()
    }

    /// The bound session's map — what the map pane draws.
    pub fn bound_map(&self) -> Option<&crate::map::Map> {
        self.index_of(self.input_session)
            .and_then(|i| self.map_of(i))
    }

    /// Shorthand for the map itself, which is what most callers want.
    pub fn map_of(&self, index: usize) -> Option<&crate::map::Map> {
        self.world(index).map(|world| &world.map)
    }

    /// Only tests reach for the map without also wanting the rest of the
    /// world's state; production goes through `world_mut`.
    #[cfg(test)]
    pub fn map_of_mut(&mut self, index: usize) -> &mut crate::map::Map {
        &mut self.world_mut(index).map
    }

    pub(crate) fn bound_mut(&mut self) -> Option<&mut SessionPane> {
        self.index_of(self.input_session)
            .and_then(|i| self.sessions.get_mut(i))
    }

    /// Origin tags only appear once more than one character is in play, so
    /// a single-session channel pane reads exactly like the main scrollback.
    fn aggregating(&self) -> bool {
        self.sessions.len() > 1
    }

    /// Where a session currently sits in the list, or `None` if it has
    /// been removed. The one place identity is translated into position;
    /// everything that draws or cycles panes goes on using indices, which
    /// is what they are good for.
    pub fn index_of(&self, id: SessionId) -> Option<usize> {
        self.sessions.iter().position(|session| session.id == id)
    }

    /// The id of the pane at `index` — for the call sites that legitimately
    /// start from a position, such as a numbered jump key.
    pub fn id_at(&self, index: usize) -> Option<SessionId> {
        self.sessions.get(index).map(|session| session.id)
    }

    /// Where the character being typed at currently sits in the list.
    pub fn bound_index(&self) -> Option<usize> {
        self.index_of(self.input_session)
    }

    /// Whether the pane at `index` is the one the input line is bound to —
    /// the question `ui` actually has, asked positionally so that drawing a
    /// list of panes never has to know what a `SessionId` is.
    pub fn is_bound(&self, index: usize) -> bool {
        self.id_at(index) == Some(self.input_session)
    }

    /// Closes a session and leaves the survivors addressable (#94).
    ///
    /// The removal itself is one line; everything else is the reason this
    /// could not be done while identity was an index. Focus and the input
    /// binding are carried by id, so they need no fixing up — but if the
    /// session they named is the one that just went, they have to land on a
    /// survivor rather than on a position that now holds a stranger.
    pub fn remove_session(&mut self, id: SessionId) {
        let Some(index) = self.index_of(id) else {
            return;
        };
        let departing = self.sessions.remove(index);
        // A peer snapshot outliving its session would leave `${@tank.hp}`
        // resolving against a character who is no longer connected (§7.5).
        self.peer_registry.remove(&departing.view.name);

        let Some(fallback) = self.sessions.first().map(|session| session.id) else {
            // Nothing left to bind to. Leaving the stale id is better than
            // inventing one; `bound()` answers `None` either way.
            return;
        };
        if self.input_session == id {
            self.input_session = fallback;
        }
        if self.focus == Focus::Session(id) {
            self.focus_pane(Focus::Session(self.input_session));
        }
    }

    /// Chooses what the map is centred on, before a frame is drawn (#58).
    ///
    /// Re-centring on every frame is what made `Alt+<n>` between two
    /// characters standing near each other slide the whole world, even
    /// though both were already on screen and the picture either way is
    /// the same one shifted. So the rule is: keep the current centre while
    /// the bound character is comfortably drawn in it, and move only when
    /// they are not.
    ///
    /// "Not" covers three cases and they all want the same answer —
    /// nothing has been drawn yet, the character walked out of view, or
    /// they are somewhere this centre cannot show at all (another area, or
    /// a group it cannot reach, where `layout_area` gives them no
    /// coordinate). In each, centring on them is the only thing that puts
    /// them in front of the player.
    pub fn update_map_pan(&mut self) {
        let (Some(id), Some(current)) = (
            self.bound_index().and_then(|i| self.id_at(i)),
            self.bound().and_then(|session| session.view.current_room),
        ) else {
            return;
        };

        self.map_pan = match self.map_pan_for {
            // The same character, a step later. Holding the offset is what
            // makes walking a pan rather than a page: the scene moved, the
            // picture moves with it, and where they sit on screen does not
            // change.
            Some((was, _)) if was == id => self.map_pan,
            // A different character. The picture should not move at all,
            // so the new pan is the old one plus however far apart the two
            // of them are — which `layout_area` gives directly, since it
            // puts the room it is asked about at the origin.
            Some((_, from)) => match self
                .bound_map()
                .map(|map| map.layout_area(from))
                .and_then(|coords| coords.get(&current).copied())
            {
                Some((dx, dy)) => (self.map_pan.0 + dx, self.map_pan.1 + dy),
                // Nowhere this layout can show: another area, or a group
                // the old room cannot reach. Nothing to preserve.
                None => (0, 0),
            },
            None => (0, 0),
        };

        // With the cursor up the view belongs to it, not to the character
        // (#103). Browsing away from where you are standing is the whole
        // point of the cursor, so the character-centred rule below must not
        // fight it — and a cursor steered off the pane is a highlight the
        // player cannot see, on the room `Enter` would walk them to.
        //
        // `layout_area` is asked about the character's room, so the
        // coordinate it returns for the cursor is already relative to the
        // scene being drawn; the room lands at `pan + at`.
        let cursor_at = self.map_cursor.and_then(|cursor| {
            self.bound_map()
                .map(|map| map.layout_area(current))
                .and_then(|coords| coords.get(&cursor).copied())
        });
        match (cursor_at, self.map_grid) {
            (Some(at), Some(grid)) => {
                let drawn =
                    crate::ui::map_shows_room(grid, (self.map_pan.0 + at.0, self.map_pan.1 + at.1));
                if !drawn {
                    // Centred rather than nudged to the edge: the cursor is
                    // being steered, and the next arrow press should have
                    // somewhere to go in every direction.
                    self.map_pan = (-at.0, -at.1);
                }
            }
            // No cursor, or nothing drawn yet to judge against. The
            // character being played has to be on screen with room to see
            // where they can go next; a pane too small to hold the margin
            // therefore never holds a view still, which is the old
            // behaviour and the right thing to fall back to.
            _ => {
                if !self
                    .map_grid
                    .is_some_and(|grid| crate::ui::map_shows_room(grid, self.map_pan))
                {
                    self.map_pan = (0, 0);
                }
            }
        }
        self.map_pan_for = Some((id, current));
    }

    /// Says something to the player wherever they can see it: the bound
    /// character's scrollback, or the empty client's notice area when
    /// there is no character at all (#108).
    ///
    /// Every `if let Some(session) = state.bound_mut()` before a
    /// `push_line` was a silent drop waiting for the day nothing was
    /// bound — which `/connect`'s own "no such profile" reply was, in
    /// exactly the state a player would be typing `/connect` in.
    /// Everything the palette can act on, best match first (#43).
    ///
    /// Commands whose meaning needs a character are dropped when there is
    /// none, rather than offered and then refused — the palette is a list
    /// of what you can do, and a list that lies about that is worse than
    /// no list. Profiles come from disk each time it opens, because a
    /// `/newprofile` a minute ago should be findable now.
    pub fn palette_entries(&self, query: &str) -> Vec<PaletteEntry> {
        let bound = self.bound().is_some();
        let mut entries: Vec<PaletteEntry> = ClientCommand::all()
            .into_iter()
            .filter(|command| bound || !command.needs_a_character())
            .map(PaletteEntry::Command)
            .collect();
        entries.extend(
            config::profile_names(&self.config_dir)
                .into_iter()
                .map(PaletteEntry::Profile),
        );

        let mut scored: Vec<(i32, PaletteEntry)> = entries
            .into_iter()
            .filter_map(|entry| {
                // The name and what it does are both searchable: someone
                // hunting for the map types "map", and someone after a
                // newer version types "release" — a word that appears
                // nowhere in `/update`.
                //
                // Scored apart, though, and a name always wins. Run
                // together, "dc" found `/map` — through "hi(d)e the map
                // (c)olumn" — while `/disconnect` sat below it, which is
                // the opposite of what typing initials means.
                let by_name = fuzzy_score(&entry.label(), query);
                let by_meaning = fuzzy_score(&entry.describes(), query);
                match (by_name, by_meaning) {
                    (Some(score), _) => Some((score + NAME_MATCH, entry)),
                    (None, Some(score)) => Some((score, entry)),
                    (None, None) => None,
                }
            })
            .collect();
        scored.sort_by(|(a, left), (b, right)| {
            b.cmp(a).then_with(|| left.label().cmp(&right.label()))
        });
        scored.into_iter().map(|(_, entry)| entry).collect()
    }

    /// Says something *and* keeps it (#18).
    ///
    /// For failures only. A warning shown as one more scrollback line is
    /// indistinguishable from server text a minute later and gone entirely
    /// once it scrolls off, which is how a failed `/reload` or a dropped
    /// `send_to:` comes to be missed. Successes and running commentary
    /// stay on `tell_player`: a panel that keeps everything is a scrollback
    /// with extra steps.
    pub fn warn_player(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.record_warning(text.clone());
        self.tell_player(text);
    }

    /// Keeps a warning without printing it — for the sites that already
    /// put it in front of the player themselves, usually because they know
    /// which pane it belongs in.
    pub fn record_warning(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.errors.push_back(text);
        while self.errors.len() > ERRORS_KEPT {
            self.errors.pop_front();
        }
        // Already looking at the panel means it is not unread.
        if !self.show_errors {
            self.errors_unread += 1;
        }
    }

    pub fn tell_player(&mut self, text: impl Into<String>) {
        let text = text.into();
        match self.bound_mut() {
            Some(session) => session.push_line(RetainedLine::client(text)),
            None => {
                self.shell_notices.push(text);
                // Oldest first out: the reply to what was just typed is
                // the one worth keeping.
                while self.shell_notices.len() > SHELL_NOTICES {
                    self.shell_notices.remove(0);
                }
            }
        }
    }

    pub fn focus_pane(&mut self, focus: Focus) {
        self.focus = focus;
        match focus {
            Focus::Session(id) => {
                if let Some(session) = self
                    .index_of(id)
                    .and_then(|index| self.sessions.get_mut(index))
                {
                    session.view.unread = 0;
                }
                self.input_session = id;
            }
            Focus::Channel(index) => {
                if let Some(channel) = self.channels.get_mut(index) {
                    channel.unread = 0;
                }
            }
            // Focusing the map puts the cursor on the character, which is
            // what the arrows will move from. Typing stays bound where it
            // was, as it does for a comms pane.
            Focus::Map => {
                self.map_cursor = self.bound().and_then(|session| session.view.current_room);
            }
        }
    }

    /// Panes in cycle order: sessions, then any visible channel panes.
    pub(crate) fn focus_next(&mut self) {
        let sessions = self.sessions.len();
        let channels = if self.show_channels {
            self.channels.len()
        } else {
            0
        };
        let map = usize::from(self.show_map);
        let total = sessions + channels + map;
        if total == 0 {
            return;
        }
        let current = match self.focus {
            // Cycling is a walk along the list, so identity becomes a
            // position here and turns back into one below.
            Focus::Session(id) => self.index_of(id).unwrap_or(0),
            Focus::Channel(index) => sessions + index,
            Focus::Map => sessions + channels,
        };
        let next = (current + 1) % total;
        if next >= sessions + channels {
            self.focus_pane(Focus::Map);
            return;
        }
        self.focus_pane(if next < sessions {
            match self.id_at(next) {
                Some(id) => Focus::Session(id),
                None => return,
            }
        } else {
            Focus::Channel(next - sessions)
        });
    }

    /// Recomputes who is in trouble, and answers with the characters who
    /// have just got into it while the player was looking elsewhere —
    /// the ones worth ringing about (§11.7).
    ///
    /// Read from the peer snapshots for the reason §11.6 gives for the
    /// strip reading them: the numbers are already in memory and borrowing
    /// them takes no lock, so asking every pass of the event loop costs
    /// nothing worth avoiding.
    pub(crate) fn refresh_distress(&mut self) -> Vec<String> {
        let mut newly_in_trouble = Vec::new();
        for index in 0..self.sessions.len() {
            let now = self
                .peer_registry
                .get(&self.sessions[index].view.name)
                .and_then(|peer| crate::vitals::from_server_data(&peer.borrow().data).distress());
            let was = self.sessions[index].view.distress.is_some();
            // Not while the player is already looking at them: the pane
            // they are reading needs no bell to point at itself.
            let unwatched = !self.is_focused(index);
            self.sessions[index].view.distress = now;
            if now.is_some() && !was && unwatched {
                newly_in_trouble.push(self.sessions[index].view.name.clone());
            }
        }
        newly_in_trouble
    }

    /// Whoever is in the most trouble — the character the "who needs me?"
    /// key jumps to. Least health left first, because with two characters
    /// under a quarter the answer to "who needs me?" is the worse of them.
    pub(crate) fn neediest(&self) -> Option<usize> {
        self.sessions
            .iter()
            .enumerate()
            .filter_map(|(index, session)| session.view.distress.map(|left| (index, left)))
            .min_by(|(_, a), (_, b)| a.total_cmp(b))
            .map(|(index, _)| index)
    }

    /// Strictly focused: what unread counting keys off.
    pub(crate) fn is_focused(&self, index: usize) -> bool {
        self.id_at(index)
            .is_some_and(|id| self.focus == Focus::Session(id))
    }

    /// Which session pane the UI draws as active. With focus on a channel
    /// pane the bound session stays highlighted — it is still the character
    /// being played (§11.1).
    pub fn is_focused_session(&self, index: usize) -> bool {
        match self.focus {
            Focus::Session(focused) => self.id_at(index) == Some(focused),
            // With focus elsewhere the bound session stays highlighted — it
            // is still the character being played (§11.1).
            Focus::Channel(_) | Focus::Map => self.id_at(index) == Some(self.input_session),
        }
    }

    /// Moves the scroll position of the *visually focused* pane
    /// (`self.focus`), not the input-bound session — a focused channel pane
    /// has to scroll on its own, even though typing still goes to whichever
    /// session `input_session` names (§11.1, §11.5). Exact clamping happens
    /// at render time, where the real wrapped-row count is known; this only
    /// adjusts the stored distance from the tail.
    pub(crate) fn scroll_focused(&mut self, code: KeyCode) {
        let back_offset = match self.focus {
            Focus::Session(id) => self
                .index_of(id)
                .and_then(|index| self.sessions.get_mut(index))
                .map(|s| &mut s.view.back_offset),
            Focus::Channel(index) => self.channels.get_mut(index).map(|c| &mut c.back_offset),
            // The map holds no buffer: it is a view on the world recomputed
            // each frame, and it moves by walking rather than scrolling.
            Focus::Map => None,
        };
        let Some(back_offset) = back_offset else {
            return;
        };
        match code {
            KeyCode::PageUp => *back_offset = back_offset.saturating_add(SCROLL_PAGE),
            KeyCode::PageDown => *back_offset = back_offset.saturating_sub(SCROLL_PAGE),
            KeyCode::Home => *back_offset = usize::MAX,
            KeyCode::End => *back_offset = 0,
            _ => {}
        }
    }

    /// Appends a routed line to its channel pane, tagging the origin session
    /// when more than one character is active (§11.1). The tag and the
    /// timestamp are the line's `origin` and `at`, not text spliced onto the
    /// front of it — `ui::draw_channel` composes both from the pane's own
    /// `timestamps:` setting at render time (§8).
    pub(crate) fn push_routed(&mut self, from: usize, channel: &str, text: String) {
        let tag = self
            .aggregating()
            .then(|| self.sessions[from].view.name.clone());
        let Some(index) = self.channel_index(channel) else {
            return;
        };
        let focused = self.focus == Focus::Channel(index);
        let pane = &mut self.channels[index];
        let line = match &tag {
            Some(tag) => RetainedLine::from_session(tag.clone(), text),
            None => RetainedLine::server(text),
        };
        // A broadcast the other characters have already delivered is not a
        // new line, so it neither appends nor counts: three sessions hearing
        // one gossip must leave the pane one unread, not three.
        if let Some(tag) = &tag
            && pane.collapse_into_recent(&line, tag)
        {
            return;
        }
        pane.push(line);
        if !focused {
            pane.unread += 1;
        }
    }

    fn channel_index(&self, name: &str) -> Option<usize> {
        self.channels
            .iter()
            .position(|pane| pane.config.name == name)
    }

    /// Resolves a `send_to` address and produces the injections to deliver.
    /// Anything undeliverable is reported in the *originating* pane, so a
    /// rule that quietly does nothing is impossible (§7.5).
    /// The sessions an address names (§7.5): one by name, or every other
    /// session for `*`. Never the sender — `*` means "the others", and a
    /// session addressing itself by name would be talking to the pane it
    /// is already writing to.
    fn addressed(&self, from: usize, target: &str) -> Vec<usize> {
        if target == ALL_SESSIONS {
            (0..self.sessions.len()).filter(|&i| i != from).collect()
        } else {
            self.sessions
                .iter()
                .position(|session| session.view.name == target)
                .filter(|&index| index != from)
                .into_iter()
                .collect()
        }
    }

    /// Writes a script's cross-session echo straight into the addressed
    /// panes (§7.5). Nothing runs at the far end, so there is no hop limit
    /// to apply and no reason to care whether that session is still
    /// connected — its pane is still on screen, and text is the one thing
    /// it can still show. The `[from x]` tag is the same one an injected
    /// command carries: nothing another session does to this one may
    /// happen anonymously.
    pub(crate) fn route_echo_to(&mut self, from: usize, target: &str, text: String) {
        let matched = self.addressed(from, target);
        if matched.is_empty() {
            let notice = format!("** echo_to: no session named `{target}`");
            self.sessions[from].push_line(RetainedLine::client(notice.clone()));
            self.record_warning(notice);
            return;
        }

        let name = self.sessions[from].view.name.clone();
        for index in matched {
            self.sessions[index].push_line(RetainedLine::from_session(
                &name,
                format!("[from {name}] {text}"),
            ));
        }
    }

    /// `label` names whatever asked for the delivery — `send_to` for a rule,
    /// `/send` for the player — because the pane reports failures back and a
    /// notice about `send_to:` after typing `/send` sends the player looking
    /// through their config for a rule that does not exist.
    pub(crate) fn route_send_to(
        &mut self,
        from: usize,
        target: &str,
        lines: Vec<String>,
        hops: u8,
        label: &str,
    ) -> Vec<(usize, SessionCommand)> {
        let matched = self.addressed(from, target);

        if matched.is_empty() {
            let notice = format!("** {label}: no session named `{target}`");
            self.sessions[from].push_line(RetainedLine::client(notice.clone()));
            self.record_warning(notice);
            return Vec::new();
        }

        let name = self.sessions[from].view.name.clone();
        let mut out = Vec::new();
        let mut notices = Vec::new();
        for index in matched {
            let session = &self.sessions[index];
            if !session.view.connected {
                notices.push(format!(
                    "** {label} `{}`: not connected, dropped {} command(s)",
                    session.view.name,
                    lines.len()
                ));
                continue;
            }
            // The limit is the receiver's: it is the session whose rules
            // would run, so it is the one that gets to say how far a chain
            // may reach it.
            if hops > session.cross.max_hops {
                notices.push(format!(
                    "** {label} `{}`: hop limit ({}) reached, dropped",
                    session.view.name, session.cross.max_hops
                ));
                continue;
            }
            out.push((
                index,
                SessionCommand::Inject {
                    from: name.clone(),
                    lines: lines.clone(),
                    hops,
                },
            ));
        }
        for notice in notices {
            self.sessions[from].push_line(RetainedLine::client(notice));
        }
        out
    }
}

/// Panes and app state without live session tasks behind them, so both the
/// hub's own tests and the widget tests can build a realistic app.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub(crate) fn pane(name: &str) -> (SessionPane, mpsc::Receiver<SessionCommand>) {
        let (tx, rx) = mpsc::channel(16);
        (
            SessionPane {
                id: SessionId::next(),
                view: SessionView {
                    name: name.to_string(),
                    scrollback: VecDeque::new(),
                    prompt: String::new(),
                    input: Input::default(),
                    status: "connecting".to_string(),
                    masked: false,
                    security: String::new(),
                    latency: String::new(),
                    connected: true,
                    inspector_log: VecDeque::new(),
                    unread: 0,
                    distress: None,
                    color: None,
                    back_offset: 0,
                    current_room: None,
                    corpse: None,
                    suggestion: None,
                },

                gmcp_seen: false,
                msdp_seen: false,
                history: VecDeque::new(),
                history_pos: None,
                history_draft: String::new(),
                history_limit: 500,
                connected_status: "connected".to_string(),
                events: None,
                commands: tx,
                rules: (PathBuf::from("/cfg"), None),
                offer_password_save: false,
                pending_password: None,
                cross: CrossSession::default(),
                last_size: None,
                scrollback_limit: 10_000,
                log: None,
                // Empty on purpose: a test pane saves nothing unless it
                // says which world it belongs to, so tests that never
                // mention maps do not write files or report failing to.
                map_key: String::new(),
                walk: None,
                vocabulary: crate::complete::Vocabulary::default(),
                dismissed: None,
                // On, so a test that means to exercise completion only has
                // to teach the pane a word.
                autocomplete: true,
            },
            rx,
        )
    }

    pub(crate) fn app_with_receivers(
        names: &[&str],
    ) -> (AppState, Vec<mpsc::Receiver<SessionCommand>>) {
        let mut sessions = Vec::new();
        let mut receivers = Vec::new();
        for name in names {
            let (session, rx) = pane(name);
            sessions.push(session);
            receivers.push(rx);
        }
        let first_id = sessions
            .first()
            .map(|session: &SessionPane| session.id)
            .unwrap_or_else(SessionId::next);
        (
            AppState {
                // Off in tests: a fixture that stripped colour would make
                // every colour assertion in this module vacuous.
                no_color: false,
                sessions,
                maps: HashMap::new(),
                channels: Vec::new(),
                focus: Focus::Session(first_id),
                input_session: first_id,
                layout: LayoutMode::Tabs,
                show_channels: false,
                channel_width: crate::config::DEFAULT_CHANNEL_WIDTH,
                show_map: false,
                map_width: crate::config::DEFAULT_MAP_WIDTH,
                show_inspector: false,
                show_hud: false,
                show_timestamps: false,
                map_cell_px: None,
                show_help: false,
                help_scroll: 0,
                keybinds: Keybinds::default(),
                config_editor: None,
                config_editor_save: None,
                reload_requested: false,
                line_cursor: None,
                map_cursor: None,
                map_first: false,
                shell_input: Input::default(),
                shell_notices: Vec::new(),
                errors: VecDeque::new(),
                errors_unread: 0,
                show_errors: false,
                palette: None,
                palette_submit: false,
                map_pan: (0, 0),
                map_pan_for: None,
                map_grid: None,
                walk_requested: None,
                config_dir: PathBuf::from("/cfg"),
                mark_menu: None,
                new_profile_wizard: None,
                peer_registry: crate::engine::Peers::new(),
                history_size: 500,
                scrollback_size: 10_000,
                autocomplete: true,
                cross_session_default: CrossSession::default(),
            },
            receivers,
        )
    }

    /// The command receivers are dropped: callers that only render never
    /// send anything to a session.
    pub(crate) fn app(names: &[&str]) -> AppState {
        app_with_receivers(names).0
    }

    pub(crate) fn channel(name: &str) -> Channel {
        Channel {
            name: name.to_string(),
            patterns: Vec::new(),
            regexes: Vec::new(),
            keep_in_main: false,
            timestamps: false,
            session: None,
            // Off unless a test says otherwise: nothing here should reach
            // the real config dir, and a test that means to exercise
            // persistence sets it and points at a temp dir.
            persist: false,
        }
    }
}
