//! Session manager and UI event loop: owns the terminal, every session's
//! channels, and the layout/focus state.
//!
//! Session tasks never touch the terminal; they emit
//! [`crate::session::SessionEvent`]s that this loop consumes alongside
//! terminal input (see docs/ARCHITECTURE.md §3). Each session keeps its own
//! scrollback, prompt, and input buffer here, so nothing one character sees
//! or types can reach another's pane. The only cross-session path is
//! [`SessionEvent::SendTo`], which this hub turns into an explicit
//! [`SessionCommand::Inject`] for the addressed session (§7.5).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::DefaultTerminal;
use tokio::sync::{mpsc, watch};
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;

use ratatui::style::Color;

use crate::config::{self, Channel, CrossSession, Keybinds};
use crate::engine::{self, Engine, PeerSnapshot};
use crate::proto::charset::Charset;
use crate::scrollback::{Origin, RetainedLine};
use crate::session::{self, PeerLinks, SessionCommand, SessionEvent};
use crate::ui;

/// Same rationale as `scrollback_size` (§8), for the raw server-data
/// inspector log (GMCP and/or MSDP, §6.3): bounded so a chatty MUD can't
/// grow it without limit.
const INSPECTOR_LOG_LIMIT: usize = 1_000;

/// Columns the channel column grows or shrinks per press (§11.4). One column
/// a press makes a real adjustment a dozen keystrokes; two lands on a usable
/// width in a few.
const CHANNEL_WIDTH_STEP: u16 = 2;

/// Rows `PgUp`/`PgDn` move per press. Exact viewport-height paging would
/// need pane geometry threaded back from the UI layer into `AppState`; a
/// fixed step is the ordinary simplification most pagers make too
/// (docs/ARCHITECTURE.md §11.5).
const SCROLL_PAGE: usize = 10;

/// The client-side commands. Everything else starting with `/` is left
/// alone, since plenty of MUDs use `/` for their own commands.
const RELOAD_COMMAND: &str = "/reload";
/// The same listing as the overlay, for players who never find the key
/// (docs/ARCHITECTURE.md §11.2).
const HELP_COMMAND: &str = "/help";
/// Opens the in-client profile editor (§10.2) — the same thing `F5` does.
const CONFIG_COMMAND: &str = "/config";
/// Installs a newer release, having been told one exists (§15). Applying
/// is delegated to `mudular-update`, which knows how this copy was
/// installed; see `crate::update`.
const UPDATE_COMMAND: &str = "/update";
/// Opens the same form the first-run screen shows, reachable any time
/// rather than only at zero-profile startup (§15, UX_REVIEW.md B).
const NEWPROFILE_COMMAND: &str = "/newprofile";
/// Adds a character to this running instance (§7.5,
/// `ARCH_REVIEW.md` "Features that would break the architecture") — a
/// name follows, not a fixed string like the commands above.
const CONNECT_COMMAND: &str = "/connect";
/// Walks toward a room already on the map, one step at a time (§16) — a
/// vnum, or a case-insensitive substring of a room name.
const GOTO_COMMAND: &str = "/goto";
/// Walks back to where a `corpse:` trigger last said the character died
/// (§16) — `/goto` with the target already remembered.
const CORPSE_COMMAND: &str = "/corpse";
/// Labels the room the character is standing in (§16) — the player's own
/// note about what a place is for, since no protocol tells us.
const MARK_COMMAND: &str = "/mark";
const MAP_COMMAND: &str = "/map";
/// Shows or hides the comms column (§11.1) — the same thing
/// `toggle_channels` does, for players who reach for a command before a
/// function key.
const COMMS_COMMAND: &str = "/comms";
/// Runs a command as another character without switching to their pane
/// (§7.5) — a character name, or `*`, then the command. The ad-hoc form of a
/// rule's `send_to:`, for the times the player did not decide in advance that
/// this was a thing they would want to do.
const SEND_COMMAND: &str = "/send";

/// `send_to` address meaning "every other session" (§7.5).
const ALL_SESSIONS: &str = "*";

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
pub struct SessionPane {
    pub name: String,
    pub scrollback: VecDeque<RetainedLine>,
    /// Text pinned above the input line; empty means no prompt.
    pub prompt: String,
    pub input: Input,
    /// Words this MUD has printed, ranked by recency (§11.3). Per session,
    /// because each character is standing somewhere else.
    vocabulary: crate::complete::Vocabulary,
    /// The rest of the word the input line would complete to — drawn as a
    /// ghost past the cursor, and appended to what is sent. Kept rather
    /// than recomputed where it is drawn, so what the player is looking at
    /// and what Enter sends cannot be two different answers.
    pub suggestion: Option<String>,
    /// The input value Escape was pressed on. A dismissal lasts exactly as
    /// long as that line does: type another character and the suggestion is
    /// welcome again, because the question has changed.
    dismissed: Option<String>,
    /// `autocomplete:` — off makes this whole path inert (§11.3).
    autocomplete: bool,
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
    /// Whether a GMCP message has reached this session — drives the
    /// inspector's title (§6.3): a client that only ever names GMCP would
    /// look broken on an MSDP-only MUD, so the title says what actually
    /// showed up rather than what the client merely supports.
    gmcp_seen: bool,
    /// The MSDP twin of `gmcp_seen`.
    msdp_seen: bool,
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
    /// Commands typed here, oldest first, exactly as typed — before alias
    /// expansion and `;` splitting, because the alias is what the player is
    /// choosing to repeat (docs/ARCHITECTURE.md §11.3).
    history: VecDeque<String>,
    /// How far back through `history` the recall walk has gone. `None` means
    /// the input line is the player's own, not a recalled copy.
    history_pos: Option<usize>,
    /// What was typed when the walk began, restored on walking back past the
    /// newest entry. Losing a half-written line to a stray arrow key is what
    /// teaches people to distrust history.
    history_draft: String,
    /// How many entries `history` keeps (`history_size`, §11.3).
    history_limit: usize,
    /// The status line shown once the connection is up.
    connected_status: String,
    /// `None` once the session has ended, so its receiver stops being polled.
    events: Option<mpsc::Receiver<SessionEvent>>,
    commands: mpsc::Sender<SessionCommand>,
    /// Rule provenance for `/reload`.
    rules: (PathBuf, Option<String>),
    /// Whether a password typed here should still be offered to the keyring
    /// (§13). Cleared once the offer has been made, so it happens once.
    offer_password_save: bool,
    /// A password typed at a masked prompt, held only until the player
    /// answers the offer to save it. Never echoed, recalled, or logged —
    /// the same rule that keeps it out of scrollback and history.
    pending_password: Option<String>,
    cross: CrossSession,
    /// Last pane size reported to the server, so a redraw that did not
    /// change this pane sends no NAWS (docs/ARCHITECTURE.md §6.2).
    last_size: Option<(u16, u16)>,
    /// How many lines `scrollback` keeps (`scrollback_size`, §8).
    scrollback_limit: usize,
    /// The open transcript file, if the profile's `log:` is set (§8, §12).
    /// Every line that reaches `push_line` is appended here too — the same
    /// choke point that already keeps masked lines out of scrollback keeps
    /// them out of the transcript, for free (§13).
    log: Option<std::io::BufWriter<std::fs::File>>,
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
const MARK_CUSTOM_ROW: &str = "something else…";

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
    remaining: VecDeque<String>,
    /// The room the step currently in flight should land in.
    expecting: crate::map::RoomId,
    /// Where the walk is headed, for the arrival message.
    destination: crate::map::RoomId,
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
    fn learn_words(&mut self, text: &str, origin: &Origin) {
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
    fn refresh_suggestion(&mut self) {
        self.suggestion = self.completion();
    }

    fn completion(&self) -> Option<String> {
        if !self.autocomplete {
            return None;
        }
        // Never into a password. The server is hiding this line, the
        // scrollback never saw it, and a ghost drawn past the asterisks
        // would be guessing at it out loud (§13).
        if self.masked {
            return None;
        }
        let value = self.input.value();
        if self.dismissed.as_deref() == Some(value) {
            return None;
        }
        // Only at the end of the line. Completing a word in the middle
        // would have to decide what happens to the text after it, and
        // there is no answer to that which is obvious enough to do silently.
        if self.input.cursor() != value.chars().count() {
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
    fn accept_suggestion(&mut self) -> bool {
        let Some(rest) = self.suggestion.take() else {
            return false;
        };
        // `with_value` leaves the cursor at the end, which is where accepting
        // a completion has to put it.
        self.input = Input::default().with_value(format!("{}{rest}", self.input.value()));
        // A word completed can still be the prefix of a longer one, so ask
        // again rather than assuming this was the last word it could be.
        self.refresh_suggestion();
        true
    }

    /// Escape: send what I typed, not what you guessed.
    fn dismiss_suggestion(&mut self) {
        self.dismissed = Some(self.input.value().to_string());
        self.suggestion = None;
    }

    /// What the input line means, ghost included — what Enter sends.
    fn completed_input(&self) -> String {
        match &self.suggestion {
            Some(rest) => format!("{}{rest}", self.input.value()),
            None => self.input.value().to_string(),
        }
    }

    fn push_line(&mut self, line: RetainedLine) {
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
        self.scrollback.push_back(line);
        if self.scrollback.len() > self.scrollback_limit {
            self.scrollback.pop_front();
        }
    }

    /// Records a submitted command for recall, and ends any walk in progress
    /// so the next `Up` starts from the newest entry again (§11.3).
    fn push_history(&mut self, line: &str) {
        self.history_pos = None;
        self.history_draft.clear();
        // Nothing to recall from an empty line, and a masked line is a
        // password: it stays out of history for the same reason it stays out
        // of scrollback (§13).
        if line.is_empty() || self.masked || self.history_limit == 0 {
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
    fn walk_history(&mut self, back: bool) -> bool {
        // Recall into a masked prompt would send an old command as the
        // password, in the clear, to whatever is listening.
        if self.masked || self.history.is_empty() {
            return false;
        }
        let next = match (self.history_pos, back) {
            // Starting a walk: stash the live line before it is overwritten.
            (None, true) => {
                self.history_draft = self.input.value().to_string();
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
        self.input = Input::new(line);
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
        self.inspector_log.push_back(line);
        if self.inspector_log.len() > INSPECTOR_LOG_LIMIT {
            self.inspector_log.pop_front();
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
const HEARD_TOGETHER: chrono::TimeDelta = chrono::TimeDelta::seconds(2);

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
    Session(usize),
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
pub struct AppState {
    pub sessions: Vec<SessionPane>,
    pub channels: Vec<ChannelPane>,
    pub focus: Focus,
    /// The session the input line is bound to — the last focused session.
    pub input_session: usize,
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
    config_editor_save: Option<config::SaveMode>,
    /// The reload keybind was pressed, drained the same way: `reload_rules`
    /// sends a `SessionCommand` over an async channel, which `handle_key`
    /// cannot do either (UX_REVIEW.md F — the same gap `/reload` typed as
    /// a command doesn't have, since `submit_input` is already async).
    reload_requested: bool,
    /// A scrollback line-cursor is active on the focused pane (§10.2/§11.5):
    /// `Some(back_offset)` is the line it currently highlights, measured the
    /// same way `SessionPane::back_offset` is — distance from the tail.
    pub line_cursor: Option<usize>,
    /// The room the map cursor is on, when it is up (§16). A room id rather
    /// than a grid position: the layout is rebuilt every frame, and a
    /// coordinate would quietly come to mean a different room the moment
    /// anything moved.
    pub map_cursor: Option<crate::map::RoomId>,
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
    config_dir: PathBuf,
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
    history_size: usize,
    scrollback_size: usize,
    /// `autocomplete:` — on the same terms, and read by `connect` (§11.3).
    autocomplete: bool,
    /// The install-wide `cross_session:` default, before a profile's own
    /// override — `/connect` needs it for the same reason it needs
    /// `history_size`/`scrollback_size`.
    cross_session_default: CrossSession,
}

/// `PgUp`/`PgDn`/`Home`/`End`, unmodified — built-in and unremappable, like
/// `Up`/`Down` (§11.3, §11.5). A modified chord (`Ctrl+PageUp`, etc.) is
/// left alone rather than swallowed, in case a terminal or a later binding
/// wants it.
fn is_scroll_key(code: KeyCode, modifiers: KeyModifiers) -> bool {
    matches!(
        code,
        KeyCode::PageUp | KeyCode::PageDown | KeyCode::Home | KeyCode::End
    ) && modifiers.is_empty()
}

impl AppState {
    /// The session the input line types into. Sessions are never removed
    /// from the list, so this index always resolves.
    pub fn bound(&self) -> Option<&SessionPane> {
        self.sessions.get(self.input_session)
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
            .filter_map(|other| Some((other.current_room?, other.name.clone())))
            .collect()
    }

    /// The bound session's map — what the map pane draws.
    pub fn bound_map(&self) -> Option<&crate::map::Map> {
        self.map_of(self.input_session)
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

    fn bound_mut(&mut self) -> Option<&mut SessionPane> {
        self.sessions.get_mut(self.input_session)
    }

    /// Origin tags only appear once more than one character is in play, so
    /// a single-session channel pane reads exactly like the main scrollback.
    fn aggregating(&self) -> bool {
        self.sessions.len() > 1
    }

    pub fn focus_pane(&mut self, focus: Focus) {
        self.focus = focus;
        match focus {
            Focus::Session(index) => {
                if let Some(session) = self.sessions.get_mut(index) {
                    session.unread = 0;
                }
                self.input_session = index;
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
                self.map_cursor = self.bound().and_then(|session| session.current_room);
            }
        }
    }

    /// Panes in cycle order: sessions, then any visible channel panes.
    fn focus_next(&mut self) {
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
            Focus::Session(index) => index,
            Focus::Channel(index) => sessions + index,
            Focus::Map => sessions + channels,
        };
        let next = (current + 1) % total;
        if next >= sessions + channels {
            self.focus_pane(Focus::Map);
            return;
        }
        self.focus_pane(if next < sessions {
            Focus::Session(next)
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
    fn refresh_distress(&mut self) -> Vec<String> {
        let mut newly_in_trouble = Vec::new();
        for index in 0..self.sessions.len() {
            let now = self
                .peer_registry
                .get(&self.sessions[index].name)
                .and_then(|peer| crate::vitals::from_server_data(&peer.borrow().data).distress());
            let was = self.sessions[index].distress.is_some();
            // Not while the player is already looking at them: the pane
            // they are reading needs no bell to point at itself.
            let unwatched = !self.is_focused(index);
            self.sessions[index].distress = now;
            if now.is_some() && !was && unwatched {
                newly_in_trouble.push(self.sessions[index].name.clone());
            }
        }
        newly_in_trouble
    }

    /// Whoever is in the most trouble — the character the "who needs me?"
    /// key jumps to. Least health left first, because with two characters
    /// under a quarter the answer to "who needs me?" is the worse of them.
    fn neediest(&self) -> Option<usize> {
        self.sessions
            .iter()
            .enumerate()
            .filter_map(|(index, session)| session.distress.map(|left| (index, left)))
            .min_by(|(_, a), (_, b)| a.total_cmp(b))
            .map(|(index, _)| index)
    }

    /// Strictly focused: what unread counting keys off.
    fn is_focused(&self, index: usize) -> bool {
        self.focus == Focus::Session(index)
    }

    /// Which session pane the UI draws as active. With focus on a channel
    /// pane the bound session stays highlighted — it is still the character
    /// being played (§11.1).
    pub fn is_focused_session(&self, index: usize) -> bool {
        match self.focus {
            Focus::Session(focused) => focused == index,
            // With focus elsewhere the bound session stays highlighted — it
            // is still the character being played (§11.1).
            Focus::Channel(_) | Focus::Map => self.input_session == index,
        }
    }

    /// Moves the scroll position of the *visually focused* pane
    /// (`self.focus`), not the input-bound session — a focused channel pane
    /// has to scroll on its own, even though typing still goes to whichever
    /// session `input_session` names (§11.1, §11.5). Exact clamping happens
    /// at render time, where the real wrapped-row count is known; this only
    /// adjusts the stored distance from the tail.
    fn scroll_focused(&mut self, code: KeyCode) {
        let back_offset = match self.focus {
            Focus::Session(index) => self.sessions.get_mut(index).map(|s| &mut s.back_offset),
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
    fn push_routed(&mut self, from: usize, channel: &str, text: String) {
        let tag = self.aggregating().then(|| self.sessions[from].name.clone());
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
                .position(|session| session.name == target)
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
    fn route_echo_to(&mut self, from: usize, target: &str, text: String) {
        let matched = self.addressed(from, target);
        if matched.is_empty() {
            let notice = format!("** echo_to: no session named `{target}`");
            self.sessions[from].push_line(RetainedLine::client(notice));
            return;
        }

        let name = self.sessions[from].name.clone();
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
    fn route_send_to(
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
            self.sessions[from].push_line(RetainedLine::client(notice));
            return Vec::new();
        }

        let name = self.sessions[from].name.clone();
        let mut out = Vec::new();
        let mut notices = Vec::new();
        for index in matched {
            let session = &self.sessions[index];
            if !session.connected {
                notices.push(format!(
                    "** {label} `{}`: not connected, dropped {} command(s)",
                    session.name,
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
                    session.name, session.cross.max_hops
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

/// Applies one session event to `state`. Returns the injections the hub
/// should deliver, and whether the session ended (so the caller can stop
/// polling its receiver).
/// Whether a `SessionEvent::Bell` from this session should actually ring
/// (§14 M9): only when its pane isn't the one the player is looking at — a
/// focused session's own bell would just be noise.
fn wants_bell(state: &AppState, index: usize, ev: &SessionEvent) -> bool {
    matches!(ev, SessionEvent::Bell) && !state.is_focused(index)
}

/// Rings the terminal bell and, on terminals that turn it into one (iTerm2,
/// kitty, foot, …), an OSC 9 desktop notification saying what happened.
/// Best-effort: a write failure here is not worth ending the session over.
fn notify(message: &str) {
    use std::io::Write as _;
    let mut stdout = std::io::stdout();
    let _ = write!(stdout, "\x07\x1b]9;{message}\x07");
    let _ = stdout.flush();
}

fn apply_session_event(
    state: &mut AppState,
    index: usize,
    ev: SessionEvent,
) -> (bool, Vec<(usize, SessionCommand)>) {
    let focused = state.is_focused(index);
    {
        let connected_status = state.sessions[index].connected_status.clone();
        let session = &mut state.sessions[index];
        if session.status != connected_status && !matches!(ev, SessionEvent::Ended(_)) {
            session.status = connected_status;
        }
    }

    match ev {
        SessionEvent::Line { text, origin } => {
            let session = &mut state.sessions[index];
            session.learn_words(&text, &origin);
            session.push_line(RetainedLine::with_origin(text, origin));
            if !focused {
                session.unread += 1;
            }
            (false, Vec::new())
        }
        SessionEvent::Route { channel, text } => {
            // Before routing, which moves the line out of this session's
            // scrollback: a name is worth completing wherever the line it
            // was in ended up (§11.3).
            state.sessions[index].learn_words(&text, &Origin::Server);
            state.push_routed(index, &channel, text);
            (false, Vec::new())
        }
        SessionEvent::SendTo {
            target,
            lines,
            hops,
        } => (
            false,
            state.route_send_to(index, &target, lines, hops, "send_to"),
        ),
        SessionEvent::EchoTo { target, text } => {
            state.route_echo_to(index, &target, text);
            (false, Vec::new())
        }
        SessionEvent::Prompt(text) => {
            state.sessions[index].prompt = text;
            (false, Vec::new())
        }
        SessionEvent::EchoMask(masked) => {
            let session = &mut state.sessions[index];
            session.masked = masked;
            // Being asked to hide typing again, with a password still
            // waiting on an answer, means the server re-prompted: the one
            // it was just given was wrong. Withdraw the offer rather than
            // let the player save a password that doesn't work (§13).
            if masked && session.pending_password.take().is_some() {
                session.push_line(RetainedLine::client(
                    "** not saved: that password was rejected, so there is \
                     nothing worth keeping",
                ));
            }
            (false, Vec::new())
        }
        // The hub decides whether to actually ring it, from the caller's
        // own focus check — this function stays a pure state update.
        SessionEvent::Bell => (false, Vec::new()),
        SessionEvent::Corpse => {
            let session = &mut state.sessions[index];
            // Whatever room the character is in as this arrives is where
            // they were standing when the server announced their death:
            // the room they get sent to comes down the same stream, behind
            // this. A death on a MUD the mapper knows nothing about has no
            // room to remember, and says so rather than silently recording
            // nothing for a `/corpse` that will later claim no death ever
            // happened.
            match session.current_room {
                Some(at) => {
                    session.corpse = Some(at);
                    let name = state
                        .map_of(index)
                        .and_then(|map| map.rooms.get(&at))
                        .and_then(|room| room.name.clone())
                        .unwrap_or_else(|| "somewhere unnamed".to_string());
                    let session = &mut state.sessions[index];
                    session.push_line(RetainedLine::client(format!(
                        "** corpse marked at #{} {name} — /corpse walks back",
                        at.0
                    )));
                }
                None => session.push_line(RetainedLine::client(
                    "** corpse: the map doesn't know where you died",
                )),
            }
            (false, Vec::new())
        }
        SessionEvent::Gmcp { package, payload } => {
            state.sessions[index].push_gmcp(package, payload);
            (false, Vec::new())
        }
        SessionEvent::Msdp { pairs } => {
            state.sessions[index].push_msdp(pairs);
            (false, Vec::new())
        }
        SessionEvent::Room { info, arrived_via } => {
            let session = &mut state.sessions[index];
            // A walk carries the room each step should reach, so an arrival
            // that is not that room is unattributable: it may be something
            // that moved the character without asking (a summon, a wimpy
            // auto-flee), or an exit that simply does not lead where it led
            // last time, as in a maze that reshuffles. The client cannot
            // tell those apart, and both make the arrival a bad teacher —
            // so a surprise during a walk teaches nothing. Exploring on
            // foot is unaffected: with no walk running there is no
            // prediction to contradict, and edges are learned as before.
            let surprising = session
                .walk
                .as_ref()
                .is_some_and(|walk| walk.expecting != info.id);
            // Only the hub can close an edge: the session knows which way
            // the character went, and the hub is what remembers where they
            // were when they went.
            let walked = match surprising {
                false => session
                    .current_room
                    .zip(arrived_via.as_deref().map(str::to_string)),
                true => None,
            };
            session.current_room = Some(info.id);
            // The world learns the room and the edge; the session only
            // learns where it now stands (§16).
            let world = state.world_mut(index);
            if let Some((from, direction)) = walked {
                world.map.connect(from, &direction, info.id);
            }
            world.map.observe(&info);
            world.dirty = true;
            let session = &mut state.sessions[index];

            // `/goto` (§16) never has more than one step outstanding, so a
            // room change while it is running is the answer to that step —
            // whether it landed where the route predicted or somewhere the
            // route knows nothing about.
            let Some(expecting) = session.walk.as_ref().map(|walk| walk.expecting) else {
                return (false, Vec::new());
            };
            if info.id != expecting {
                // The route was learned earlier and the world has since
                // changed underneath it — a locked gate, a closed door,
                // anything that turns one step into a different room than
                // the map predicted. Guessing the rest of the path from
                // here could walk the character further from where they
                // meant to go, so the walk stops rather than continuing on
                // a route that has just been proven wrong.
                session.walk = None;
                let via = arrived_via.as_deref().unwrap_or("an unrecognised step");
                session.push_line(RetainedLine::client(format!(
                    "** walk stopped: `{via}` led to #{}, not the #{} the map expected",
                    info.id.0, expecting.0
                )));
                return (false, Vec::new());
            }

            let destination = session.walk.as_ref().unwrap().destination;
            match session.walk.as_mut().unwrap().remaining.pop_front() {
                None => {
                    session.walk = None;
                    session.push_line(RetainedLine::client(format!(
                        "** arrived at #{}",
                        destination.0
                    )));
                    (false, Vec::new())
                }
                Some(direction) => {
                    // `Map::path` only follows edges with a known
                    // destination, but it said so about the map as it stood
                    // when the route was planned. `observe` has run since,
                    // one event ago, and the step in flight takes its next
                    // expectation from the *live* map — so a server that
                    // re-points an exit mid-walk can land the character in a
                    // room the route never planned for, whose own exits need
                    // not include the next direction at all. Indexing here
                    // took the whole client down with it.
                    let next = state
                        .map_of(index)
                        .and_then(|map| map.rooms.get(&info.id))
                        .and_then(|room| room.exits.get(&direction))
                        .copied()
                        .flatten();
                    let session = &mut state.sessions[index];
                    match next {
                        Some(next_expecting) => {
                            session.walk.as_mut().unwrap().expecting = next_expecting;
                            (false, vec![(index, SessionCommand::SendLine(direction))])
                        }
                        None => {
                            // Same verdict as a step that lands somewhere
                            // unexpected: the route has been proven wrong,
                            // so stop rather than guess the rest of it.
                            session.walk = None;
                            session.push_line(RetainedLine::client(format!(
                                "** walk stopped: the map no longer knows where `{direction}` \
                                 leads from #{}",
                                info.id.0
                            )));
                            (false, Vec::new())
                        }
                    }
                }
            }
        }
        SessionEvent::Mark(label) => {
            let session = &mut state.sessions[index];
            // Same ordering as `Corpse`: whatever room the character is in
            // as this arrives is the one the line was about.
            match session.current_room {
                Some(at) => {
                    // Through the same door as `/mark`, so a trigger's
                    // label reaches this world's other characters too.
                    if set_mark(state, index, at, Some(label.clone())) {
                        state.sessions[index].push_line(RetainedLine::client(format!(
                            "** marked #{} as `{label}`",
                            at.0
                        )));
                    }
                }
                None => session.push_line(RetainedLine::client(format!(
                    "** mark `{label}`: the map doesn't know where you are"
                ))),
            }
            (false, Vec::new())
        }
        SessionEvent::Security(security) => {
            let session = &mut state.sessions[index];
            session.security = security.label;
            // §13 requires an insecure connection (or a newly pinned
            // certificate) to be visible, not just implied by a label.
            if let Some(warning) = security.warning {
                session.push_line(RetainedLine::client(format!("** {warning}")));
            }
            (false, Vec::new())
        }
        SessionEvent::Latency(rtt) => {
            state.sessions[index].latency = format!("{}ms", rtt.as_millis());
            (false, Vec::new())
        }
        SessionEvent::Reconnecting {
            attempt,
            delay,
            reason,
        } => {
            let session = &mut state.sessions[index];
            session.status = format!(
                "reconnecting in {}s (attempt {attempt}): {reason}",
                delay.as_secs()
            );
            session.latency.clear();
            // Whatever the server had asked us to hide, it is not asking
            // any more.
            session.masked = false;
            (false, Vec::new())
        }
        SessionEvent::Ended(reason) => {
            let session = &mut state.sessions[index];
            session.status = format!("disconnected: {reason}");
            // The status line is one row wide and gets cut off mid-word on
            // anything longer than a short reason (a TLS pin mismatch's
            // full fingerprint comparison, say) — exactly the moment a
            // player most needs the complete explanation, not a fragment.
            // Scrollback wraps and never truncates, so the full reason
            // always lands there too, the same way §13 already puts a
            // security warning in both places.
            session.push_line(RetainedLine::client(format!("** disconnected: {reason}")));
            session.latency.clear();
            session.masked = false;
            session.connected = false;
            // Nothing will confirm the outstanding step now — leaving it set
            // would have the next `/goto` compare against a room from a
            // connection that no longer exists.
            session.walk = None;
            let key = session.map_key.clone();
            save_world_map(state, &key);
            (true, Vec::new())
        }
    }
}

/// Recompile the rule set from disk and hand it to the bound session.
/// Returns the line to show the player either way — a broken rule file
/// must report itself rather than silently leaving the old rules in place.
async fn reload_rules(state: &AppState, channels: &[Channel]) -> String {
    let Some(session) = state.bound() else {
        return "** /reload needs a connected session".to_string();
    };
    let (config_dir, profile) = &session.rules;

    match crate::config::load_rules(config_dir, profile.as_deref(), channels)
        .and_then(|layers| Ok(Engine::compile(&layers)?))
    {
        Ok(engine) => {
            let _ = session
                .commands
                .send(SessionCommand::SetRules(Box::new(engine)))
                .await;
            "** rules reloaded".to_string()
        }
        Err(err) => format!("** reload failed, keeping the current rules: {err:#}"),
    }
}

/// Opens the in-client profile editor (§10.2) over the bound session's
/// profile. Requires a profile session — an ad-hoc `--host` session has no
/// file to edit — and a profile that at least parses; a broken file is
/// reported rather than edited from a blank default, which would risk
/// overwriting whatever is actually wrong with it.
fn open_config_editor(state: &mut AppState, channels: &[Channel]) {
    let Some(session) = state.bound() else {
        return;
    };
    let (dir, profile) = session.rules.clone();
    let Some(name) = profile else {
        if let Some(session) = state.bound_mut() {
            session.push_line(RetainedLine::client(
                "** /config needs a profile session — this one was started with --host",
            ));
        }
        return;
    };

    let path = config::profile_path(&dir, &name);
    let had_comments = std::fs::read_to_string(&path)
        .map(|text| text.lines().any(|line| line.trim_start().starts_with('#')))
        .unwrap_or(false);

    match config::load_profile_file(&path) {
        Ok(file) => {
            let known_modules = known_module_names(&dir);
            state.config_editor = Some(ui::config_editor::ConfigEditorState::open(
                file,
                dir,
                name,
                channels,
                known_modules,
                had_comments,
            ));
        }
        Err(err) => {
            if let Some(session) = state.bound_mut() {
                session.push_line(RetainedLine::client(format!(
                    "** could not open the profile editor: {err:#}"
                )));
            }
        }
    }
}

/// The module names that actually exist under `modules/`, so the editor can
/// flag a profile's `modules:` entry that doesn't resolve without treating
/// it as a hard error — the module may simply not be written yet.
fn known_module_names(dir: &Path) -> HashSet<String> {
    std::fs::read_dir(dir.join("modules"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "yaml") {
                return None;
            }
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_string)
        })
        .collect()
}

/// The scrollback line-cursor's `Enter` (§10.2/§11.5): opens the profile
/// editor straight into a new trigger, its pattern prefilled from the
/// picked line. `back_offset` counts from the newest line, matching
/// `SessionPane::back_offset`'s convention.
fn open_new_trigger_from_line(state: &mut AppState, channels: &[Channel], back_offset: usize) {
    let Some(session) = state.bound() else {
        return;
    };
    let Some(raw) = session
        .scrollback
        .len()
        .checked_sub(1 + back_offset)
        .and_then(|index| session.scrollback.get(index))
    else {
        return;
    };
    let pattern = engine::pattern_escape(raw.plain());
    let (dir, profile) = session.rules.clone();
    let Some(name) = profile else {
        if let Some(session) = state.bound_mut() {
            session.push_line(RetainedLine::client(
                "** picking a trigger needs a profile session — this one was started with \
                 --host",
            ));
        }
        return;
    };

    let path = config::profile_path(&dir, &name);
    let had_comments = std::fs::read_to_string(&path)
        .map(|text| text.lines().any(|line| line.trim_start().starts_with('#')))
        .unwrap_or(false);

    match config::load_profile_file(&path) {
        Ok(file) => {
            let known_modules = known_module_names(&dir);
            state.config_editor =
                Some(ui::config_editor::ConfigEditorState::open_with_new_trigger(
                    file,
                    dir,
                    name,
                    channels,
                    known_modules,
                    had_comments,
                    pattern,
                ));
        }
        Err(err) => {
            if let Some(session) = state.bound_mut() {
                session.push_line(RetainedLine::client(format!(
                    "** could not open the profile editor: {err:#}"
                )));
            }
        }
    }
}

/// Validates and saves the editor's draft (§10.2), called once `handle_key`
/// has returned an `EditorAction::Save` — the one piece of the save flow
/// that needs async IO the editor's own (sync) key handling cannot do.
/// Every session bound to the saved profile gets its rules reloaded live,
/// not just the one that opened the editor: two panes can share a profile.
async fn service_config_save(state: &mut AppState, channels: &[Channel], mode: config::SaveMode) {
    let Some(editor) = &mut state.config_editor else {
        return;
    };

    let dir = editor.dir().to_path_buf();
    let name = editor.name().to_string();
    let draft = editor.draft().clone();

    if let Err(err) = config::validate_profile_rules(&dir, &name, &draft, channels) {
        editor.set_notice_error(format!("{err:#}"));
        return;
    }

    let connection_changed = {
        let before = editor.original_profile();
        before.host != draft.host
            || before.port != draft.port
            || before.tls != draft.tls
            || before.charset != draft.charset
            || before.login != draft.login
    };

    match config::save_profile(editor.file(), &draft, mode) {
        Ok(saved) => {
            editor.note_saved();
            let mut notice = "saved".to_string();
            if let Some(backup) = &saved.backup {
                notice.push_str(&format!(
                    " — previous version backed up to {}",
                    backup.display()
                ));
            }
            if connection_changed {
                notice.push_str("; host/port/TLS/charset/login changes apply next connection");
            }
            editor.set_notice_info(notice);

            let mut lines_for = Vec::new();
            for session in &state.sessions {
                if session.rules.1.as_deref() == Some(name.as_str()) {
                    lines_for.push(session.name.clone());
                }
            }
            for target_name in lines_for {
                if let Some(index) = state.sessions.iter().position(|s| s.name == target_name) {
                    let session = &state.sessions[index];
                    let (config_dir, profile) = &session.rules;
                    let text =
                        match crate::config::load_rules(config_dir, profile.as_deref(), channels)
                            .and_then(|layers| Ok(Engine::compile(&layers)?))
                        {
                            Ok(engine) => {
                                let _ = session
                                    .commands
                                    .send(SessionCommand::SetRules(Box::new(engine)))
                                    .await;
                                "** config saved; rules reloaded".to_string()
                            }
                            Err(err) => {
                                format!("** config saved, but rules failed to reload: {err:#}")
                            }
                        };
                    state.sessions[index].push_line(RetainedLine::client(text));
                }
            }
        }
        Err(config::SaveError::Conflict { .. }) => {
            let at = std::fs::metadata(&editor.file().path)
                .and_then(|m| m.modified())
                .map(|t| {
                    let secs = t
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    format!("{secs}s since epoch")
                })
                .unwrap_or_else(|_| "unknown time".to_string());
            editor.prompt_conflict(&at);
        }
        Err(config::SaveError::Vanished { .. }) => editor.prompt_vanished(),
        Err(err) => editor.set_notice_error(format!("{err}")),
    }
}

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
enum WizardOutcome {
    /// More fields to go, or the last answer was rejected — the wizard's
    /// own state (`error`, in particular) says which.
    Continue,
    Cancelled,
    Done(config::NewProfile),
}

/// A character name a person would actually type, not paste — long past
/// this the wizard's dialog box would grow to fill the terminal and break
/// its own centered layout (UX_REVIEW.md, Adversarial findings, Low #6).
const MAX_PROFILE_NAME_LEN: usize = 32;

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
    fn new(config_dir: PathBuf) -> Self {
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

    fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> WizardOutcome {
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

/// The first-run "create a profile" form (docs/ARCHITECTURE.md §15): shown
/// when nothing was given on the command line and no profile exists yet,
/// so a first connection needs no hand-edited YAML. Runs its own terminal
/// session before any of `event_loop`'s — there is no session to drive it
/// yet, and threading a would-be session through a loop built to manage
/// live ones would be the tail wagging the dog. `Ok(None)` means the
/// player cancelled with Esc.
pub async fn run_new_profile_wizard(
    config_dir: &std::path::Path,
) -> Result<Option<config::NewProfile>> {
    let mut terminal = ratatui::init();
    let result = new_profile_event_loop(&mut terminal, config_dir).await;
    ratatui::restore();
    result
}

async fn new_profile_event_loop(
    terminal: &mut DefaultTerminal,
    config_dir: &std::path::Path,
) -> Result<Option<config::NewProfile>> {
    let mut wizard = NewProfileWizard::new(config_dir.to_path_buf());
    let mut term_events = EventStream::new();
    let mut map_saves = tokio::time::interval(MAP_SAVE_INTERVAL);
    // `interval` fires once immediately; nothing is dirty yet, so the tick
    // would be a no-op, but skipping it keeps the first real save a full
    // period away rather than at startup.
    map_saves.tick().await;

    loop {
        terminal.draw(|frame| {
            ui::draw_new_profile_wizard(
                frame,
                &wizard.answered,
                wizard.step.prompt(),
                wizard.input.value(),
                wizard.input.visual_cursor(),
                wizard.error.as_deref(),
            )
        })?;

        let Some(Ok(Event::Key(key))) = term_events.next().await else {
            return Ok(None);
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match wizard.handle_key(key.code, key.modifiers) {
            WizardOutcome::Continue => {}
            WizardOutcome::Cancelled => return Ok(None),
            WizardOutcome::Done(profile) => return Ok(Some(profile)),
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    config_dir: PathBuf,
    targets: Vec<ConnectTarget>,
    keybinds: Keybinds,
    channels: Vec<Channel>,
    history_size: usize,
    scrollback_size: usize,
    channel_width: u16,
    map_width: u16,
    map_graphics: bool,
    autocomplete: bool,
    cross_session_default: CrossSession,
    first_run_hint: bool,
    map_debug: Option<PathBuf>,
    update_check: Option<tokio::sync::oneshot::Receiver<Option<crate::update::Available>>>,
) -> Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(
        &mut terminal,
        config_dir,
        targets,
        keybinds,
        channels,
        history_size,
        scrollback_size,
        channel_width,
        map_width,
        map_graphics,
        autocomplete,
        cross_session_default,
        first_run_hint,
        map_debug,
        update_check,
    )
    .await;
    ratatui::restore();
    result
}

/// Builds everything one profile needs to connect, shared by the CLI
/// startup path and `/connect <profile>` (§7.5, ARCH_REVIEW.md "Features
/// that would break the architecture") — the same construction either way,
/// so a session added later behaves identically to one named on the
/// command line. `taken` is the names already in use (so a repeated
/// profile still gets its `-2` suffix); `record` is `None` for `/connect`,
/// since `--record` is a startup-only flag with no session to apply to
/// after the fact.
pub fn build_profile_target(
    dir: &Path,
    name: &str,
    taken: &mut Vec<String>,
    channels: &[Channel],
    cross_default: CrossSession,
    record: Option<PathBuf>,
) -> Result<ConnectTarget> {
    let path = config::profile_path(dir, name);
    let profile = config::load_profile(&path)?;
    let tls = profile.tls.enabled.then(|| crate::net::TlsConfig {
        verify: profile.tls.verify,
        pin_store: crate::net::pins::PinStore::new(dir.join("known_certs")),
    });
    let charset: Charset = profile
        .charset
        .parse()
        .map_err(|err: String| anyhow::anyhow!(err))
        .with_context(|| format!("charset in {}", path.display()))?;
    let layers = config::load_rules(dir, Some(name), channels)?;
    let (login, offer_password_save) = autologin(profile.login.as_ref(), name, dir)?;
    let session_name_str = session_name(name, taken);
    let log_path = profile
        .log
        .then(|| config::log_dir(dir).join(format!("{session_name_str}.log")));
    Ok(ConnectTarget {
        world: profile.world.clone(),
        name: session_name_str,
        host: profile.host,
        port: profile.port,
        tls,
        record,
        charset,
        rules: Rules {
            engine: Engine::compile(&layers)?,
            config_dir: dir.to_path_buf(),
            profile: Some(name.to_string()),
        },
        cross: cross_default.with_override(profile.cross_session),
        color: profile.color,
        login,
        offer_password_save,
        log_path,
    })
}

/// Builds the auto-login machine for a profile, reading its password from
/// the OS keyring (docs/ARCHITECTURE.md §10). A profile with no `login:`
/// block gets `None` and the ordinary hand-typed login.
///
/// Also reports whether the session should offer to save the password the
/// player types: only for a profile that wants auto-login, has nothing
/// stored, and has not already turned the offer down (§13).
fn autologin(
    login: Option<&config::Login>,
    profile: &str,
    dir: &Path,
) -> Result<(Option<session::login::Autologin>, bool)> {
    let Some(login) = login else {
        return Ok((None, false));
    };
    // Read once, at connect time: the keyring may prompt, and doing that
    // from inside the session task would block the pipeline mid-connection.
    let password = config::stored_password(profile)?;
    let offer_save = password.is_none() && !config::password_save_declined(dir, profile);
    let machine = session::login::Autologin::new(
        login.name.clone(),
        password,
        login.name_prompt.as_deref(),
        login.password_prompt.as_deref(),
    )
    .with_context(|| format!("auto-login for {profile}"))?;
    Ok((Some(machine), offer_save))
}

/// Sessions are addressed by profile name; a second session on the same
/// profile gets a numeric suffix (`cleric-2`, docs/ARCHITECTURE.md §7.5).
fn session_name(base: &str, taken: &mut Vec<String>) -> String {
    let mut name = base.to_string();
    let mut suffix = 1;
    while taken.contains(&name) {
        suffix += 1;
        name = format!("{base}-{suffix}");
    }
    taken.push(name.clone());
    name
}

fn connect(
    target: ConnectTarget,
    history_limit: usize,
    scrollback_limit: usize,
    autocomplete: bool,
    peers: PeerLinks,
) -> SessionPane {
    // Taken before the target is consumed by `spawn` below.
    let map_key = config::map_key(target.world.as_deref(), &target.host, target.port);
    let (target_host, target_port) = (target.host.clone(), target.port);
    let status = format!("connecting to {}:{}...", target.host, target.port);
    let connected_status = format!("connected to {}:{}", target.host, target.port);
    let Rules {
        engine,
        config_dir,
        profile,
    } = target.rules;
    let (events, commands) = session::spawn(
        target.host,
        target.port,
        target.tls,
        target.record,
        target.charset,
        engine,
        target.cross.expand_aliases,
        target.login,
        peers,
    );
    // Keyed by the world, not the character (§16): two characters on one
    // MUD walk the same rooms, so they share what they have walked. This is
    // also why a `--host` session now keeps a map where it used to get
    // none — it has no profile, but it does have a host and a port.
    // Both older schemes: the original per-character file, and the
    // briefly-lived host-and-port one.
    let mut legacy_keys: Vec<String> = profile.iter().cloned().collect();
    legacy_keys.push(format!("{}_{}", target_host, target_port));
    if let Err(err) = config::adopt_legacy_map(&config_dir, &legacy_keys, &map_key) {
        tracing::warn!("could not adopt an older map: {err:#}");
    }
    SessionPane {
        map_key,
        name: target.name,
        scrollback: VecDeque::new(),
        prompt: String::new(),
        input: Input::default(),
        status,
        masked: false,
        security: String::new(),
        latency: String::new(),
        connected: true,
        inspector_log: VecDeque::new(),
        gmcp_seen: false,
        msdp_seen: false,
        unread: 0,
        distress: None,
        color: target.color,
        history: VecDeque::new(),
        history_pos: None,
        history_draft: String::new(),
        history_limit,
        connected_status,
        events: Some(events),
        commands,
        rules: (config_dir, profile),
        offer_password_save: target.offer_password_save,
        pending_password: None,
        cross: target.cross,
        last_size: None,
        scrollback_limit,
        log: target.log_path.as_deref().and_then(open_log),
        back_offset: 0,
        current_room: None,
        corpse: None,
        walk: None,
        vocabulary: crate::complete::Vocabulary::default(),
        suggestion: None,
        dismissed: None,
        autocomplete,
    }
}

/// A keystroke that reached the bare input line — nothing above it wanted
/// it — and what the completion makes of the result (§11.3).
///
/// Its own function so it can be tested: everything else on this path is
/// inside `event_loop`, which owns a real terminal and cannot be driven
/// from a test.
fn type_into_input(session: &mut SessionPane, key: crossterm::event::KeyEvent) {
    // Escape means "send what I typed, not what you guessed" here, which
    // it is free to mean only because every overlay that wants Escape has
    // already had its chance at this key.
    if key.code == KeyCode::Esc && key.modifiers.is_empty() {
        session.dismiss_suggestion();
        return;
    }
    // Tab takes the guess. Only when there is one: with nothing to accept it
    // falls through, where `tui-input` ignores it — a literal tab in a line
    // bound for the MUD is never what was meant. `Ctrl+Tab` is a different
    // key here (it cycles panes), which the modifier check keeps out.
    if key.code == KeyCode::Tab && key.modifiers.is_empty() && session.accept_suggestion() {
        return;
    }
    // Up/Down are built-in and unremappable (§11.3): on a single-line
    // input they have no other meaning, and they are the one binding every
    // user arrives knowing.
    let walked = match key.code {
        KeyCode::Up if key.modifiers.is_empty() => session.walk_history(true),
        KeyCode::Down if key.modifiers.is_empty() => session.walk_history(false),
        _ => false,
    };
    if !walked {
        session.input.handle_event(&Event::Key(key));
    }
    session.refresh_suggestion();
}

/// Whether a picture that was on screen has just gone, leaving pixels
/// ratatui will not paint over.
///
/// Only this direction needs the terminal cleared. A picture *arriving*
/// used to need it too — the cells under it were skipped from the first
/// frame, so nothing erased what it landed on — but they are only skipped
/// once the terminal already has the picture now, which leaves the
/// arriving frame to ratatui's ordinary diff and costs one pane's worth
/// of repaint instead of the whole screen's.
fn image_vanished(had_image: bool, has_image: bool) -> bool {
    had_image && !has_image
}

/// Everything `--map-debug` writes about one moment: which session's map is
/// showing, the room it is centred on, and the scene itself — both as a
/// picture sized to the whole thing (not clipped to the pane) and as a
/// listing exact enough to catch what the picture can't, like which room a
/// stray gap or a collision-dropped neighbour actually was.
/// Reads back the exact cells [`ui::draw`] just put in the map pane, from
/// ratatui's own buffer — real screen content, not a re-render — plus the
/// glyphs `write_map_image` writes straight to the terminal afterward. The
/// cells under a Sixel picture are marked skipped so ratatui's buffer never
/// learns what covers them; the glyphs are as much of that region as a text
/// snapshot can show at all, since the pixels underneath have no text form.
fn capture_map_area(
    buffer: &ratatui::buffer::Buffer,
    area: Option<ratatui::layout::Rect>,
    image: Option<&ui::PendingImage>,
) -> String {
    let Some(area) = area else {
        return "(map pane not shown this frame)".to_string();
    };
    let mut rows: Vec<Vec<char>> = (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| {
                    buffer[(area.x + x, area.y + y)]
                        .symbol()
                        .chars()
                        .next()
                        .unwrap_or(' ')
                })
                .collect()
        })
        .collect();
    if let Some(image) = image {
        for (x, y, ch, _) in &image.glyphs {
            if *x >= area.x && *x < area.x + area.width && *y >= area.y && *y < area.y + area.height
            {
                rows[(*y - area.y) as usize][(*x - area.x) as usize] = *ch;
            }
        }
    }
    let note = if image.is_some() {
        "(sixel pixels have no text form — only the letters written on top \
         of them show here; the room listing below says what the pixels \
         themselves would be)\n"
    } else {
        ""
    };
    let picture = rows
        .into_iter()
        .map(|row| row.into_iter().collect::<String>().trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    format!("{note}{picture}")
}

/// Everything `--map-debug` writes about one moment: the header facts, the
/// pane exactly as it was captured off screen (`capture_map_area`), and the
/// full room/link listing — the off-screen half of the picture, since a
/// screen capture only ever shows what fit.
fn map_debug_snapshot(state: &AppState, has_image: bool, screen_text: &str) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let Some(session) = state.bound() else {
        out.push_str("no session bound\n");
        return out;
    };
    let _ = writeln!(out, "session: {}", session.name);
    let _ = writeln!(out, "sixel image this frame: {has_image}");
    let _ = writeln!(out, "map cursor: {:?}", state.map_cursor);
    let _ = writeln!(out, "corpse: {:?}", session.corpse);
    let current = session.current_room;
    let _ = writeln!(
        out,
        "current room: {}",
        match current
            .zip(state.map_of(state.input_session))
            .and_then(|(id, map)| map.rooms.get(&id).map(|room| (id, room)))
        {
            Some((id, room)) => format!("{} {:?} area {:?}", id.0, room.name, room.area),
            None => "none (no room data yet)".to_string(),
        }
    );
    out.push('\n');
    out.push_str(screen_text);

    let Some(current) = current else {
        return out;
    };
    let Some(map) = state.map_of(state.input_session) else {
        return out;
    };
    let scene = map.scene(current, session.corpse, &[]);
    out.push_str("\n\nrooms:\n");
    for room in &scene.rooms {
        let _ = writeln!(
            out,
            "  {:>6} at {:>4},{:<4} role={:?} up={} down={} mark={:?} hidden_exits={}",
            room.id.0,
            room.at.0,
            room.at.1,
            room.role,
            room.up,
            room.down,
            room.mark,
            room.hidden_exits
        );
    }
    out.push_str("\nlinks:\n");
    for link in &scene.links {
        let _ = writeln!(out, "  {:?} step {:?}", link.from, link.step);
    }
    out
}

/// Enough to say whether the map pane could have changed, without doing
/// any of the work of finding out what it now shows. `record` is called
/// every frame — every keystroke, in any pane — and `map_debug_snapshot`
/// lays out the whole area from scratch and writes a line per room and
/// per link; on a busy area that is real work to redo on every character
/// typed into an unrelated session.
///
/// A room discovered without the current room changing (a passage
/// revealed while standing still) will not flip this key, so a snapshot
/// for it can be missed — a session that only walks to see the picture
/// change never hits that gap, and it is a small loss for a debug tool
/// against a real per-keystroke cost.
#[derive(Clone, PartialEq, Eq)]
struct MapDebugKey {
    session: String,
    current_room: Option<crate::map::RoomId>,
    corpse: Option<crate::map::RoomId>,
    cursor: Option<crate::map::RoomId>,
    has_image: bool,
}

impl MapDebugKey {
    fn of(state: &AppState, has_image: bool) -> Option<Self> {
        let session = state.bound()?;
        Some(Self {
            session: session.name.clone(),
            current_room: session.current_room,
            corpse: session.corpse,
            cursor: state.map_cursor,
            has_image,
        })
    }
}

/// Writes a `map_debug_snapshot` to disk whenever the map pane could have
/// changed — otherwise every keystroke in an unrelated pane would write
/// one.
struct MapDebugWriter {
    dir: PathBuf,
    count: u32,
    last_key: Option<MapDebugKey>,
}

impl MapDebugWriter {
    fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            count: 0,
            last_key: None,
        }
    }

    /// `screen_text` is lazy: building it means walking the pane's whole
    /// buffer, and `map_debug_snapshot` does real work with it besides —
    /// work worth skipping outright when `MapDebugKey` already says
    /// nothing worth snapshotting has changed, rather than doing it and
    /// throwing the result away.
    fn record(
        &mut self,
        state: &AppState,
        has_image: bool,
        screen_text: impl FnOnce() -> String,
    ) -> Result<()> {
        let key = MapDebugKey::of(state, has_image);
        if key == self.last_key {
            return Ok(());
        }
        self.last_key = key;
        let snapshot = map_debug_snapshot(state, has_image, &screen_text());
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(format!(
            "{:05}_{}.txt",
            self.count,
            chrono::Local::now().format("%Y%m%d-%H%M%S%.3f")
        ));
        std::fs::write(&path, &snapshot)?;
        self.count += 1;
        Ok(())
    }
}

/// Puts the map's picture on the terminal, after ratatui has flushed the
/// cells around it (§16).
///
/// Written here rather than through a widget because an image is not made
/// of cells and ratatui has nowhere to put one. The cells underneath were
/// marked skipped while the frame was built, so nothing has painted over
/// the space and nothing will until the next frame.
fn write_map_image(image: &ui::PendingImage) -> Result<()> {
    use crossterm::{cursor, queue, style};
    use std::io::Write as _;

    let mut out = std::io::stdout();
    queue!(out, cursor::SavePosition)?;
    queue!(out, cursor::MoveTo(image.area.x, image.area.y))?;
    out.write_all(image.sixel.as_bytes())?;
    // The letters go on last, in the terminal's own font, onto the cells
    // the image covers — a bitmap font small enough to embed would draw
    // them worse than this, and the map's grid is shared by both halves so
    // they line up without being told to.
    for (x, y, ch, glyph_style) in &image.glyphs {
        queue!(out, cursor::MoveTo(*x, *y))?;
        if let Some(ratatui::style::Color::Rgb(r, g, b)) = glyph_style.fg {
            queue!(
                out,
                style::SetForegroundColor(style::Color::Rgb { r, g, b })
            )?;
        }
        // The cell's own colour, because writing the character costs us the
        // image in that cell on every terminal that keeps images per cell.
        if let Some(ratatui::style::Color::Rgb(r, g, b)) = glyph_style.bg {
            queue!(
                out,
                style::SetBackgroundColor(style::Color::Rgb { r, g, b })
            )?;
        }
        queue!(out, style::Print(ch))?;
    }
    queue!(out, style::ResetColor, cursor::RestorePosition)?;
    out.flush()?;
    Ok(())
}

/// How many pixels a cell is, if the terminal has said.
///
/// Asked of the kernel rather than the terminal: `TIOCGWINSZ` carries pixel
/// dimensions beside the row and column counts, so this needs no escape
/// sequence, no reply to wait for, and no timeout to get wrong. A terminal
/// that leaves them zero — and a pty, which is why the live-test driver
/// never takes this path — simply has not said.
fn cell_pixels() -> Option<(u16, u16)> {
    let size = crossterm::terminal::window_size().ok()?;
    if size.width == 0 || size.height == 0 || size.columns == 0 || size.rows == 0 {
        return None;
    }
    Some((size.width / size.columns, size.height / size.rows))
}

/// The pane layout as it stands.
fn current_layout(state: &AppState) -> config::UiState {
    config::UiState {
        show_channels: state.show_channels,
        show_map: state.show_map,
        show_inspector: state.show_inspector,
        show_hud: Some(state.show_hud),
        show_timestamps: state.show_timestamps,
        channel_width: state.channel_width,
        map_width: state.map_width,
    }
}

/// Writes the layout when a key actually moved something (§11.4).
///
/// Compared rather than written unconditionally, so holding a resize key
/// against its clamp — which changes nothing — writes nothing, and an
/// ordinary keystroke costs no disk at all.
fn remember_layout_if_changed(state: &AppState, before: config::UiState) {
    let now = current_layout(state);
    if now == before {
        return;
    }
    if let Err(err) = config::save_ui_state(&state.config_dir, &now) {
        tracing::warn!("could not save the pane layout: {err:#}");
    }
}

/// Saves a session's map to its profile, merged with whatever is already on
/// disk (`config::save_map`). An ad-hoc `--host` session has no profile and
/// so nothing to save to — the same rule `/config` and disk logging already
/// apply. Best-effort: a save failure must never block whatever the player
/// was already doing when it happened.
/// How often exploration is flushed to disk (§16). The map is only ever
/// written when it has actually changed, so this is the most recent
/// exploration a crash can cost, not a fixed write rate.
const MAP_SAVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Saves every world that has learned something since the last write.
///
/// Per world, not per session: the characters on one MUD share the file,
/// so saving each of them in turn wrote it several times over — and the
/// last writer's copy was the one that stuck, which is how an unmark made
/// through one character used to be undone by another.
///
/// Kept out of the quit and disconnect paths, which save unconditionally:
/// those are the last chance to write anything, so "nothing has changed"
/// is not worth being clever about at the moment the client is going away.
fn save_dirty_maps(state: &mut AppState) {
    let dirty: Vec<String> = state
        .maps
        .iter()
        .filter(|(_, world)| world.dirty)
        .map(|(key, _)| key.clone())
        .collect();
    for key in dirty {
        save_world_map(state, &key);
    }
}

/// Writes every world, dirty or not — the way out, where the question is
/// not whether it is worth writing but whether there is a last chance to.
fn save_all_maps(state: &mut AppState) {
    let keys: Vec<String> = state.maps.keys().cloned().collect();
    for key in keys {
        save_world_map(state, &key);
    }
}

/// Returns whether the map actually reached disk, so a caller can tell
/// "written" from "there was nowhere to write it" as well as from a
/// failure.
/// Reads a world's map off disk, the first time a character arrives on
/// it. Later characters on the same MUD join the entry that is already
/// there rather than loading a second copy of it (§16).
///
/// Only on first open: re-reading for a later arrival would merge the
/// file back over what this run has since changed, and `Map::merge`'s
/// never-erase rule would put a just-removed mark back. A legacy file
/// adopted by a profile that `/connect`s into an already-open world is
/// therefore picked up at the next launch rather than immediately —
/// one-time migration against a rare ordering, and it does not lose
/// anything.
fn open_world(maps: &mut HashMap<String, WorldMap>, config_dir: &Path, key: &str) {
    if key.is_empty() || maps.contains_key(key) {
        return;
    }
    maps.insert(
        key.to_string(),
        WorldMap {
            map: config::load_map(config_dir, key),
            ..WorldMap::default()
        },
    );
}

/// A channel pane's starting contents: what the last run left, where the
/// channel persists at all (§11.1).
///
/// The restored block is closed with a marker naming when it ends. Without
/// one, a tell from last Tuesday sits directly above tonight's first line
/// and reads as new — the pane draws arrival order, not elapsed time, and
/// a channel with `timestamps: false` shows no clock to correct the
/// impression.
fn restored_comms(
    config_dir: &Path,
    channel: &Channel,
    scrollback_size: usize,
) -> VecDeque<RetainedLine> {
    if !channel.persist {
        return VecDeque::new();
    }
    let mut lines = config::load_comms(config_dir, &channel.name);
    // The file's bound and the pane's are set independently, so the file
    // may hold more than this pane is willing to keep.
    let over = lines.len().saturating_sub(scrollback_size);
    let mut lines: VecDeque<RetainedLine> = lines.drain(over..).collect();
    if let Some(last) = lines.back().map(|line| line.at) {
        lines.push_back(RetainedLine::client(format!(
            "** end of saved comms — the line above arrived {}",
            last.format("%Y-%m-%d %H:%M")
        )));
    }
    lines
}

/// Writes every persisting channel pane on the way out (§11.1). Only on
/// the way out: a busy gossip channel would otherwise rewrite the file on
/// every line, and unlike a map — which is exploration that cost someone an
/// evening — a lost tail of chat costs the session it was in.
fn save_comms_panes(state: &AppState) {
    for pane in &state.channels {
        if !pane.config.persist {
            continue;
        }
        if let Err(err) = config::save_comms(&state.config_dir, &pane.config.name, &pane.lines) {
            // Nowhere to show a notice — the client is already leaving.
            tracing::warn!("could not save the {} pane: {err}", pane.config.name);
        }
    }
}

fn save_world_map(state: &mut AppState, key: &str) -> bool {
    // A `--host` session saves too: it has no profile, but the world it is
    // in has a name all the same (§16). An empty key is a session with no
    // world at all, which is nothing to write.
    if key.is_empty() {
        return false;
    }
    let Some(world) = state.maps.get(key) else {
        return false;
    };
    let outcome = config::save_map(&state.config_dir, key, &world.map, &world.explicit_marks);

    let notice = match outcome {
        Ok(()) => {
            let Some(world) = state.maps.get_mut(key) else {
                return false;
            };
            // Working again, so a later failure is news once more.
            world.save_failed = false;
            world.dirty = false;
            // Reached disk, authoritatively — the next save is a plain
            // merge again until something is explicitly marked or cleared.
            world.explicit_marks.clear();
            return true;
        }
        Err(err) => {
            tracing::warn!("could not save map for {key}: {err:#}");
            let Some(world) = state.maps.get_mut(key) else {
                return false;
            };
            // Said once, not every tick. Saving runs every 30s, and a
            // problem that persists would otherwise fill the scrollback
            // with the same line for the rest of the session — which is
            // worse than silence, and would bury the first one.
            //
            // Once is enough because of that same 30s tick: anything that
            // stops maps reaching disk surfaces within half a minute,
            // while the player can still act on it. A failure that only
            // ever happens on the way out has at most the last tick's
            // exploration to lose, and still reaches the log.
            let first = !world.save_failed;
            world.save_failed = true;
            match first {
                true => format!(
                    "** could not save the map: {err:#} — exploration since the last save is not on disk"
                ),
                false => return false,
            }
        }
    };
    // Every character on this world, since it is every character's
    // exploration that is not reaching disk.
    for session in state
        .sessions
        .iter_mut()
        .filter(|session| session.map_key == key)
    {
        session.push_line(RetainedLine::client(notice.clone()));
    }
    false
}

/// Opens a session's transcript file for append, creating its directory if
/// needed. `None` on any failure — a session that can't log still connects
/// (docs/ARCHITECTURE.md §3: one session's trouble stays local to it).
fn open_log(path: &std::path::Path) -> Option<std::io::BufWriter<std::fs::File>> {
    if let Some(dir) = path.parent()
        && let Err(err) = std::fs::create_dir_all(dir)
    {
        tracing::warn!("could not create log directory {}: {err}", dir.display());
        return None;
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    // Transcripts can hold anything a server echoes back, including a MUD
    // account's own login exchange — owner-only, like the config it sits
    // beside, rather than left at the process umask.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(file) => Some(std::io::BufWriter::new(file)),
        Err(err) => {
            tracing::warn!("could not open log file {}: {err}", path.display());
            None
        }
    }
}

/// Waits for the next event from any live session. Parks forever when every
/// session has ended, so the loop keeps serving the terminal.
async fn next_session_event(sessions: &mut [SessionPane]) -> (usize, Option<SessionEvent>) {
    let pending: Vec<_> = sessions
        .iter_mut()
        .enumerate()
        .filter(|(_, session)| session.events.is_some())
        .map(|(index, session)| {
            Box::pin(async move {
                let events = session.events.as_mut().expect("filtered to live sessions");
                (index, events.recv().await)
            })
        })
        .collect();
    if pending.is_empty() {
        return std::future::pending().await;
    }
    futures::future::select_all(pending).await.0
}

/// Send each session the size of its own pane, and only when it changed —
/// per-pane NAWS, not the terminal's size (docs/ARCHITECTURE.md §6.2).
async fn report_pane_sizes(state: &mut AppState, area: ratatui::layout::Rect) {
    for (index, size) in ui::session_pane_sizes(area, state) {
        let session = &mut state.sessions[index];
        if session.last_size == Some(size) {
            continue;
        }
        session.last_size = Some(size);
        let _ = session
            .commands
            .send(SessionCommand::Resize {
                cols: size.0,
                rows: size.1,
            })
            .await;
    }
}

/// What woke the event loop.
enum Wake {
    Terminal(Option<std::io::Result<Event>>),
    Session(usize, Option<SessionEvent>),
    /// Time to flush any exploration that isn't on disk yet.
    SaveMaps,
    /// The startup update check came back. `None` means "no news", which
    /// includes every way it could have failed (§15).
    Update(Option<crate::update::Available>),
}

#[allow(clippy::too_many_arguments)]
async fn event_loop(
    terminal: &mut DefaultTerminal,
    config_dir: PathBuf,
    targets: Vec<ConnectTarget>,
    keybinds: Keybinds,
    channels: Vec<Channel>,
    history_size: usize,
    scrollback_size: usize,
    channel_width: u16,
    map_width: u16,
    map_graphics: bool,
    autocomplete: bool,
    cross_session_default: CrossSession,
    first_run_hint: bool,
    map_debug: Option<PathBuf>,
    mut update_check: Option<tokio::sync::oneshot::Receiver<Option<crate::update::Available>>>,
) -> Result<()> {
    // Every session publishes a snapshot of its state and reads every
    // other session's (§7.5). The channels are made up front, so a session
    // can watch a peer that has not finished connecting yet — it simply
    // sees that peer's empty snapshot until it has something to say.
    let published: Vec<(
        String,
        watch::Sender<PeerSnapshot>,
        watch::Receiver<PeerSnapshot>,
    )> = targets
        .iter()
        .map(|target| {
            let (tx, rx) = watch::channel(PeerSnapshot::default());
            (target.name.clone(), tx, rx)
        })
        .collect();
    let receivers: Vec<(String, watch::Receiver<PeerSnapshot>)> = published
        .iter()
        .map(|(name, _, rx)| (name.clone(), rx.clone()))
        .collect();

    let multiple_characters = targets.len() > 1;
    let sessions: Vec<SessionPane> = targets
        .into_iter()
        .zip(published)
        .map(|(target, (_, publish, _))| {
            // `*` in a rule means "every other session", and so does this:
            // a session never watches itself, where its own variables are
            // already the live ones.
            let others = receivers
                .iter()
                .filter(|(name, _)| *name != target.name)
                .map(|(name, rx)| (name.clone(), rx.clone()))
                .collect();
            let links = PeerLinks {
                publish: Some(publish),
                others,
            };
            connect(target, history_size, scrollback_size, autocomplete, links)
        })
        .collect();
    let has_sessions = !sessions.is_empty();

    let mut state = AppState {
        sessions,
        maps: HashMap::new(),
        channels: channels
            .iter()
            .map(|config| ChannelPane {
                lines: restored_comms(&config_dir, config, scrollback_size),
                config: config.clone(),
                unread: 0,
                scrollback_limit: scrollback_size,
                back_offset: 0,
            })
            .collect(),
        focus: Focus::Session(0),
        input_session: 0,
        layout: LayoutMode::Tabs,
        show_channels: !channels.is_empty(),
        channel_width,
        // Off until asked for: on a MUD that sends no room data it would
        // be a permanently empty column, and the player has not said yet
        // whether they want it.
        show_map: false,
        map_width,
        show_inspector: false,
        // One character has nothing to compare, and the strip would be a
        // row spent restating the prompt. Two or more is the case it exists
        // for — the numbers a multiboxer was holding in their head.
        show_hud: multiple_characters,
        // Off until asked for: a MUD's own output is dense enough, and a
        // clock down every line is a cost paid on every line by someone
        // who wanted it on one.
        show_timestamps: false,
        map_cell_px: map_graphics.then(cell_pixels).flatten(),
        show_help: false,
        help_scroll: 0,
        keybinds: keybinds.clone(),
        config_editor: None,
        config_editor_save: None,
        reload_requested: false,
        line_cursor: None,
        map_cursor: None,
        walk_requested: None,
        config_dir,
        mark_menu: None,
        new_profile_wizard: None,
        peer_registry: receivers.into_iter().collect(),
        history_size,
        scrollback_size,
        autocomplete,
        cross_session_default,
    };
    // Each world read off disk once, by whichever character reaches it
    // first — the rest join the entry that is already there (§16).
    for index in 0..state.sessions.len() {
        let key = state.sessions[index].map_key.clone();
        open_world(&mut state.maps, &state.config_dir, &key);
    }

    // A config value wider than this terminal is clamped before the first
    // frame, the same as a `Resize` clamps it later (§11.4).
    state.channel_width =
        ui::clamp_channel_width(state.channel_width, terminal.get_frame().area().width);
    state.map_width = ui::clamp_map_width(state.map_width, terminal.get_frame().area().width);
    // Config says where a fresh install starts; this says where the player
    // left off, so it goes on top (§11.4). Widths are re-clamped because
    // the terminal may be narrower than it was last time.
    if let Some(saved) = config::load_ui_state(&state.config_dir) {
        let width = terminal.get_frame().area().width;
        state.show_channels = saved.show_channels;
        state.show_map = saved.show_map;
        state.show_inspector = saved.show_inspector;
        state.show_timestamps = saved.show_timestamps;
        // Only where the player has actually said. A saved layout that
        // predates the strip, or one from before they ever touched it,
        // leaves the count to decide.
        if let Some(shown) = saved.show_hud {
            state.show_hud = shown;
        }
        state.channel_width = ui::clamp_channel_width(saved.channel_width, width);
        state.map_width = ui::clamp_map_width(saved.map_width, width);
    }

    if !has_sessions {
        // Nothing to drive the loop but the terminal; the empty-state help
        // lives in the UI, which has no pane to draw it in otherwise.
        state.show_channels = false;
    }

    // The wizard always connects exactly one character, so it's this one
    // (UX_REVIEW.md C) — a newcomer who just answered it has no other way
    // to know F1 exists.
    if first_run_hint && let Some(session) = state.sessions.first_mut() {
        session.push_line(RetainedLine::client("press F1 for the full key list"));
    }

    report_pane_sizes(&mut state, terminal.get_frame().area()).await;

    let mut term_events = EventStream::new();
    let mut map_saves = tokio::time::interval(MAP_SAVE_INTERVAL);
    // `interval` fires once immediately; nothing is dirty yet, so the tick
    // would be a no-op, but skipping it keeps the first real save a full
    // period away rather than at startup.
    map_saves.tick().await;

    let mut map_debug = map_debug.map(MapDebugWriter::new);
    let mut map_image_cache = ui::MapImageCache::default();

    let mut had_image = false;
    loop {
        // Before the frame that will draw the answer: a character who has
        // just dropped into trouble is worth saying out loud once, for the
        // player whose eyes are on another pane entirely (§11.7).
        for name in state.refresh_distress() {
            notify(&format!("{name} needs help"));
        }
        let mut drawn = ui::DrawnFrame {
            image: None,
            image_is_fresh: true,
            map_area: None,
        };
        let mut completed =
            terminal.draw(|frame| drawn = ui::draw(frame, &state, &mut map_image_cache))?;
        if image_vanished(had_image, drawn.image.is_some()) {
            // The cells under a picture are marked skipped while it is up,
            // and a skipped cell is excluded from ratatui's diff no matter
            // which side of the transition it is on — so the frame where a
            // picture appears leaves ratatui's diff nothing to repaint over
            // whatever was genuinely there before (a "no room data yet"
            // placeholder, a room's description), and the frame where one
            // disappears leaves the pixels on screen over whatever replaced
            // them. Clearing drops the known state, so the next frame
            // repaints the lot.
            //
            // Not `Terminal::clear()`: on this backend it saves the cursor
            // position first, which means asking the terminal where the
            // cursor is and blocking on its answer — and a real terminal
            // that is slow, or busy, or simply quiet about it left this
            // hanging for two seconds before erroring out the whole
            // client. `resize` to the unchanged area clears the same way
            // without that round trip, because a fullscreen viewport has
            // no cursor position worth asking for.
            let area = terminal.get_frame().area();
            terminal.resize(area)?;
            // The clear took the picture off the screen with everything
            // else, so the cache's claim that the terminal already has it
            // is now false. Without this the redraw below reuses it,
            // reports it as unchanged, and nothing writes it back — the
            // map stayed blank until something moved and forced a fresh
            // one, which read as "the map needs two moves to appear".
            map_image_cache.forget();
            completed =
                terminal.draw(|frame| drawn = ui::draw(frame, &state, &mut map_image_cache))?;
        }
        if drawn.image_is_fresh
            && let Some(image) = &drawn.image
        {
            write_map_image(image)?;
        }
        if let Some(writer) = &mut map_debug {
            writer.record(&state, drawn.image.is_some(), || {
                capture_map_area(completed.buffer, drawn.map_area, drawn.image.as_ref())
            })?;
        }
        had_image = drawn.image.is_some();

        let wake = tokio::select! {
            ev = term_events.next() => Wake::Terminal(ev),
            (index, ev) = next_session_event(&mut state.sessions) => Wake::Session(index, ev),
            _ = map_saves.tick() => Wake::SaveMaps,
            // Guarded: a oneshot that has already produced its value resolves
            // instantly on every later poll, which would spin the loop. The
            // guard retires the branch after it fires.
            found = async { update_check.as_mut().expect("guarded").await },
                if update_check.is_some() =>
            {
                update_check = None;
                Wake::Update(found.ok().flatten())
            }
        };

        match wake {
            Wake::Terminal(Some(Ok(Event::Key(key)))) if key.kind == KeyEventKind::Press => {
                // Deliberately only around keys. A *terminal* resize also
                // clamps the columns, and remembering that would throw away
                // the width the player chose the moment they shrank the
                // window.
                let layout_before = current_layout(&state);
                if keybinds.quit.matches(key.code, key.modifiers) {
                    save_all_maps(&mut state);
                    save_comms_panes(&state);
                    return Ok(());
                }
                let area_width = terminal.get_frame().area().width;
                if handle_key(
                    &mut state,
                    &keybinds,
                    key.code,
                    key.modifiers,
                    area_width,
                    &channels,
                ) {
                    if let Some(mode) = state.config_editor_save.take() {
                        service_config_save(&mut state, &channels, mode).await;
                    }
                    if state.reload_requested {
                        state.reload_requested = false;
                        let notice = reload_rules(&state, &channels).await;
                        if let Some(session) = state.bound_mut() {
                            session.push_line(RetainedLine::client(notice));
                        }
                    }
                    if let Some(target) = state.walk_requested.take() {
                        walk_to_room(&mut state, target).await;
                    }
                    report_pane_sizes(&mut state, terminal.get_frame().area()).await;
                } else if key.code == KeyCode::Enter {
                    submit_input(&mut state, &channels).await;
                    // Usually a no-op (`report_pane_sizes` only sends a
                    // session a size it hasn't already been told), but
                    // `/connect` just changed how many panes there are —
                    // in Splits layout every existing one just resized,
                    // and the new one needs telling for the first time.
                    report_pane_sizes(&mut state, terminal.get_frame().area()).await;
                } else if is_scroll_key(key.code, key.modifiers) {
                    // Scrollback keys are built-in and unremappable, like
                    // Up/Down below — and they act on the *focused* pane,
                    // not the input-bound session, so a focused channel pane
                    // scrolls even while typing stays bound elsewhere
                    // (§11.1, §11.5). This changes pane content, not size,
                    // so no NAWS report follows.
                    state.scroll_focused(key.code);
                } else if let Some(session) = state.bound_mut() {
                    type_into_input(session, key);
                }
                remember_layout_if_changed(&state, layout_before);
            }
            Wake::Terminal(Some(Ok(Event::Resize(cols, rows)))) => {
                let area = ratatui::layout::Rect::new(0, 0, cols, rows);
                // A narrower terminal can leave the column wider than the
                // session area can spare, so the width re-clamps on every
                // resize before the sizes are reported (§11.4).
                state.channel_width = ui::clamp_channel_width(state.channel_width, area.width);
                state.map_width = ui::clamp_map_width(state.map_width, area.width);
                report_pane_sizes(&mut state, area).await;
            }
            Wake::Terminal(Some(Ok(_))) => {}
            Wake::Terminal(Some(Err(err))) => return Err(err.into()),
            Wake::Terminal(None) => return Ok(()),
            Wake::Update(Some(available)) => {
                // Into the focused pane, the way the first-run F1 hint goes
                // into the first one: it is a client notice, not something a
                // MUD said, and it must not look like one.
                let version = available.version;
                if let Some(session) = state.sessions.first_mut() {
                    session.push_line(RetainedLine::client(format!(
                        "Mudular {version} is available — type /update to install it                          (set check_for_updates: false in mudular.yaml to stop looking)"
                    )));
                }
            }
            // No news, or the check failed. Either way there is nothing to say
            // — an error here is not the player's problem.
            Wake::Update(None) => {}
            Wake::SaveMaps => {
                // Blocking I/O on the loop thread, as the quit path already
                // does: a map is a few tens of kilobytes of JSON, and the
                // alternative is cloning it to hand to a blocking task on a
                // timer that mostly finds nothing to do.
                save_dirty_maps(&mut state);
            }
            Wake::Session(index, ev) => {
                let mut injections = Vec::new();
                let mut ring_bell = false;
                match ev {
                    Some(ev) => {
                        ring_bell |= wants_bell(&state, index, &ev);
                        let (ended, out) = apply_session_event(&mut state, index, ev);
                        injections.extend(out);
                        if ended {
                            state.sessions[index].events = None;
                        } else {
                            // Drain any further already-buffered events without
                            // blocking, so a burst of server output (e.g. a banner
                            // arriving across many small reads) triggers one
                            // redraw instead of one per event.
                            while let Some(ev) = try_recv(&mut state, index) {
                                ring_bell |= wants_bell(&state, index, &ev);
                                let (ended, out) = apply_session_event(&mut state, index, ev);
                                injections.extend(out);
                                if ended {
                                    state.sessions[index].events = None;
                                    break;
                                }
                            }
                        }
                    }
                    None => state.sessions[index].events = None,
                }
                if ring_bell {
                    notify(&format!("{} has new output", state.sessions[index].name));
                }
                for (target, command) in injections {
                    let _ = state.sessions[target].commands.send(command).await;
                }
            }
        }
    }
}

fn try_recv(state: &mut AppState, index: usize) -> Option<SessionEvent> {
    state.sessions[index]
        .events
        .as_mut()
        .and_then(|rx| rx.try_recv().ok())
}

/// Handles the layout/focus keys. Returns `true` when the key changed which
/// panes are on screen, so the caller can re-report NAWS sizes.
/// `area_width` is the terminal's current width, which bounds how wide the
/// channel column may grow (§11.4).
fn handle_key(
    state: &mut AppState,
    keybinds: &Keybinds,
    code: KeyCode,
    modifiers: KeyModifiers,
    area_width: u16,
    channels: &[Channel],
) -> bool {
    // An unanswered save-password offer owns `y` and `n`, and nothing else:
    // any other key drops the held password and goes on to the input line,
    // so a stray keystroke costs the offer rather than trapping the player.
    if let Some(session) = state.bound_mut()
        && session.pending_password.is_some()
    {
        match code {
            // `true` only to consume the key, as the help overlay does:
            // the answer must not also land in the input line.
            KeyCode::Char('y' | 'Y') => {
                answer_password_offer(session, true);
                return true;
            }
            KeyCode::Char('n' | 'N') => {
                answer_password_offer(session, false);
                return true;
            }
            _ => session.pending_password = None,
        }
    }
    // The mark chooser owns the keyboard while it is up, and answers to the
    // same three keys the scrollback line picker does (§11.3) — one small
    // list, moved with the arrows, taken with Enter, dropped with Esc.
    if let Some(menu) = state.mark_menu.as_mut() {
        let last = menu.entries().len().saturating_sub(1);
        // Typing a label of your own: the keyboard is text now, so the
        // digit shortcuts below would eat the digits.
        if let Some(typed) = menu.typing.as_mut() {
            match code {
                KeyCode::Char(ch) => typed.push(ch),
                KeyCode::Backspace => {
                    typed.pop();
                }
                KeyCode::Enter => apply_mark_menu(state),
                // Back to the list rather than out of the chooser: one Esc
                // undoes one step, as the profile editor's input does.
                KeyCode::Esc => menu.typing = None,
                _ => {}
            }
            return true;
        }
        match code {
            KeyCode::Esc => {
                state.mark_menu = None;
            }
            KeyCode::Up => menu.selected = menu.selected.saturating_sub(1),
            KeyCode::Down => menu.selected = (menu.selected + 1).min(last),
            KeyCode::Enter => apply_mark_menu(state),
            // A digit takes its row outright: nine entries is short enough
            // that counting beats arrowing.
            KeyCode::Char(digit @ '1'..='9') => {
                let row = digit as usize - '1' as usize;
                if row <= last {
                    menu.selected = row;
                    apply_mark_menu(state);
                }
            }
            // Any other character starts a label of its own, keeping the
            // one just typed. Someone wanting a word the list has not got
            // types the word — which is what happened, and it used to be
            // swallowed silently, leaving `Enter` to apply whatever row was
            // highlighted. Typing `mail` marked the room `shop`.
            //
            // Digits are the row shortcuts above, so a label starting with
            // one goes through "something else…" first. No label worth
            // having starts with a digit, and losing the shortcuts would
            // cost more than that.
            KeyCode::Char(ch) => menu.typing = Some(ch.to_string()),
            _ => {}
        }
        return true;
    }
    // The profile editor owns the keyboard while it's open, same as the
    // help overlay below — but it routes keys into its own state machine
    // rather than closing on any key (§10.2).
    if let Some(editor) = state.config_editor.as_mut() {
        match editor.handle_key(code, modifiers) {
            ui::config_editor::EditorAction::Consumed => {}
            ui::config_editor::EditorAction::Close => state.config_editor = None,
            ui::config_editor::EditorAction::Save { force } => {
                state.config_editor_save = Some(if force {
                    config::SaveMode::Overwrite
                } else {
                    config::SaveMode::Guarded
                });
            }
        }
        return true;
    }
    // The new-profile form owns the keyboard while it's open, same as the
    // config editor above — reachable any time via `/newprofile`, not just
    // at first run (§15, UX_REVIEW.md B). It only ever writes the file:
    // connecting it live would need the dynamic peer registry `/connect`
    // does (tracked separately), so the confirmation says how to use it
    // next launch rather than pretending it joined this one.
    if let Some(wizard) = state.new_profile_wizard.as_mut() {
        match wizard.handle_key(code, modifiers) {
            WizardOutcome::Continue => {}
            WizardOutcome::Cancelled => state.new_profile_wizard = None,
            WizardOutcome::Done(profile) => {
                state.new_profile_wizard = None;
                let notice = match config::save_new_profile(&state.config_dir, &profile) {
                    Ok(()) => format!(
                        "** saved profiles/{}.yaml — run `mudular {}` alongside \
                         this session next time to play it too",
                        profile.name, profile.name
                    ),
                    Err(err) => format!("** could not save the new profile: {err:#}"),
                };
                if let Some(session) = state.bound_mut() {
                    session.push_line(RetainedLine::client(notice));
                }
            }
        }
        return true;
    }
    // The scrollback line-cursor also owns the keyboard while active: it is
    // only a few keys (§10.2/§11.5), so they're handled directly here rather
    // than through a second state-machine type. `back_offset` is kept in
    // step with the cursor so the highlighted line (drawn in `ui::draw`)
    // never scrolls out of view — the two count different things in
    // general (wrapped rows vs. raw scrollback entries), but line-cursor
    // moves one raw entry at a time, so pinning `back_offset` to the same
    // number keeps the picked line on screen for the ordinary case of
    // unwrapped MUD output.
    // The map cursor owns the arrows while it is up, the same way the
    // scrollback line picker below owns them: one small selection, moved
    // with the arrows, taken with Enter, dropped with Esc (§11.3).
    if let Some(cursor) = state.map_cursor {
        let step = match code {
            KeyCode::Esc => {
                state.focus_pane(Focus::Session(state.input_session));
                state.map_cursor = None;
                return true;
            }
            KeyCode::Enter => {
                state.focus_pane(Focus::Session(state.input_session));
                state.map_cursor = None;
                state.walk_requested = Some(cursor);
                return true;
            }
            KeyCode::Up => (0, -1),
            KeyCode::Down => (0, 1),
            KeyCode::Left => (-1, 0),
            KeyCode::Right => (1, 0),
            // Anything else puts the cursor away and then *does its
            // ordinary job*: closing it must not also cost the keystroke.
            // Swallowing it meant `Alt+2` did nothing the first time it was
            // pressed with the cursor up — the character it was meant to
            // switch to stayed where it was, and the key had to be pressed
            // again.
            _ => {
                state.map_cursor = None;
                return handle_key(state, keybinds, code, modifiers, area_width, channels);
            }
        };
        // A nudge into empty space leaves the cursor where it is, rather
        // than dropping it — the map is full of gaps and losing your place
        // to one would make it tiring to steer.
        if let Some(next) = map_cursor_step(state, cursor, step) {
            state.map_cursor = Some(next);
        }
        return true;
    }
    if keybinds.toggle_timestamps.matches(code, modifiers) {
        state.show_timestamps = !state.show_timestamps;
        return true;
    }
    if keybinds.toggle_hud.matches(code, modifiers) {
        state.show_hud = !state.show_hud;
        return true;
    }
    if keybinds.who_needs_me.matches(code, modifiers) {
        match state.neediest() {
            Some(index) => state.focus_pane(Focus::Session(index)),
            // Said rather than ignored, the same way the map cursor says
            // why it did nothing: a key that silently does nothing is a
            // key the player assumes is broken.
            None => {
                if let Some(session) = state.bound_mut() {
                    session.push_line(RetainedLine::client("** nobody is in trouble"));
                }
            }
        }
        return true;
    }
    if keybinds.map_cursor.matches(code, modifiers) {
        // The same place `Alt+<map>` and `focus_next` arrive at — one key
        // for players who reach for a shortcut rather than counting panes.
        if state
            .bound()
            .and_then(|session| session.current_room)
            .is_none()
        {
            if let Some(session) = state.bound_mut() {
                session.push_line(RetainedLine::client(
                    "** the map doesn't know where you are yet",
                ));
            }
            return true;
        }
        // No use steering a map nobody can see.
        state.show_map = true;
        state.focus_pane(Focus::Map);
        return true;
    }
    if let Some(cursor) = state.line_cursor {
        let len = state.bound().map_or(0, |s| s.scrollback.len());
        let new_cursor = match code {
            KeyCode::Esc => {
                state.line_cursor = None;
                return true;
            }
            KeyCode::Up => (cursor + 1).min(len.saturating_sub(1)),
            KeyCode::Down => cursor.saturating_sub(1),
            KeyCode::PageUp => (cursor + SCROLL_PAGE).min(len.saturating_sub(1)),
            KeyCode::PageDown => cursor.saturating_sub(SCROLL_PAGE),
            KeyCode::Enter => {
                state.line_cursor = None;
                open_new_trigger_from_line(state, channels, cursor);
                return true;
            }
            _ => cursor,
        };
        state.line_cursor = Some(new_cursor);
        if let Some(session) = state.bound_mut() {
            session.back_offset = new_cursor;
        }
        return true;
    }
    // While the overlay is up it owns the keyboard: any key dismisses it and
    // goes no further. Typing blind into an input line hidden behind the
    // help is worse than the extra keystroke to reopen it (§11.2).
    //
    // Except the scroll keys, which now move the listing instead. It is
    // longer than a short terminal can show, and a key that dismissed the
    // help would make the bottom of it unreachable — the one part a
    // newcomer most needs is the part that scrolled off.
    if state.show_help {
        match code {
            KeyCode::Up => state.help_scroll = state.help_scroll.saturating_sub(1),
            KeyCode::Down => state.help_scroll = state.help_scroll.saturating_add(1),
            KeyCode::PageUp => {
                state.help_scroll = state.help_scroll.saturating_sub(SCROLL_PAGE as u16)
            }
            KeyCode::PageDown => {
                state.help_scroll = state.help_scroll.saturating_add(SCROLL_PAGE as u16)
            }
            KeyCode::Home => state.help_scroll = 0,
            KeyCode::End => state.help_scroll = u16::MAX,
            _ => state.show_help = false,
        }
        return true;
    }
    if keybinds.help.matches(code, modifiers) {
        state.show_help = true;
        // Always from the top: reopening to wherever it was left reads as
        // the overlay having lost its place.
        state.help_scroll = 0;
        return true;
    }
    if keybinds.config_editor.matches(code, modifiers) {
        open_config_editor(state, channels);
        return true;
    }
    if keybinds.toggle_map.matches(code, modifiers) {
        state.show_map = !state.show_map;
        describe_current_room(state);
        return true;
    }
    if keybinds.reload.matches(code, modifiers) {
        state.reload_requested = true;
        return true;
    }
    if keybinds.line_picker.matches(code, modifiers) {
        if let Some(session) = state.bound()
            && !session.scrollback.is_empty()
        {
            // Starts wherever the pane is already scrolled to, rather than
            // always jumping to the newest line — if you scrolled up to
            // look at something before reaching for this, that's the line
            // you meant to pick.
            state.line_cursor = Some(
                session
                    .back_offset
                    .min(session.scrollback.len().saturating_sub(1)),
            );
        }
        return true;
    }
    if keybinds.server_data_inspector.matches(code, modifiers) {
        state.show_inspector = !state.show_inspector;
        return true;
    }
    if keybinds.focus_next.matches(code, modifiers) {
        state.focus_next();
        return true;
    }
    if keybinds.cycle_layout.matches(code, modifiers) {
        state.layout = match state.layout {
            LayoutMode::Tabs => LayoutMode::Splits,
            LayoutMode::Splits => LayoutMode::Tabs,
        };
        return true;
    }
    if keybinds.toggle_channels.matches(code, modifiers) {
        toggle_comms(state);
        return true;
    }
    // Resizing the column resizes the session panes beside it, so both keys
    // report `true` and the caller re-reports NAWS (§6.2, §11.4). A press
    // that is already at a limit stops there rather than hiding a pane.
    if keybinds.channel_wider.matches(code, modifiers) {
        state.channel_width = ui::clamp_channel_width(
            state.channel_width.saturating_add(CHANNEL_WIDTH_STEP),
            area_width,
        );
        return true;
    }
    if keybinds.channel_narrower.matches(code, modifiers) {
        state.channel_width = ui::clamp_channel_width(
            state.channel_width.saturating_sub(CHANNEL_WIDTH_STEP),
            area_width,
        );
        return true;
    }
    if keybinds.map_wider.matches(code, modifiers) {
        state.map_width = ui::clamp_map_width(
            state.map_width.saturating_add(CHANNEL_WIDTH_STEP),
            area_width,
        );
        return true;
    }
    if keybinds.map_narrower.matches(code, modifiers) {
        state.map_width = ui::clamp_map_width(
            state.map_width.saturating_sub(CHANNEL_WIDTH_STEP),
            area_width,
        );
        return true;
    }
    // Alt+1..9 jumps straight to a session (§11), and to the map column on
    // the number after the last one — the sessions keep the numbers they
    // always had, so nothing anybody has learned moves.
    if modifiers.contains(KeyModifiers::ALT)
        && let KeyCode::Char(c) = code
        && let Some(n) = c.to_digit(10).filter(|&n| n >= 1)
    {
        let index = n as usize - 1;
        if index < state.sessions.len() {
            state.focus_pane(Focus::Session(index));
            return true;
        }
        if index == state.sessions.len() && state.show_map {
            state.focus_pane(Focus::Map);
            return true;
        }
    }
    false
}

/// Acts on the answer to the save-password offer: `y` stores the held
/// password in the keyring, `n` records the refusal so the offer is made
/// once per profile rather than once per login (§13). A keyring or file
/// error is reported in the pane — the player is logged in either way, and
/// ending the session over it would help nobody.
fn answer_password_offer(session: &mut SessionPane, save: bool) {
    let Some(password) = session.pending_password.take() else {
        return;
    };
    // The offer is only armed for profile sessions, which always have one.
    let Some(profile) = session.rules.1.clone() else {
        return;
    };
    let outcome = if save {
        config::store_password(&profile, &password)
            .map(|()| format!("** password saved in the keyring for `{profile}`"))
    } else {
        config::decline_password_save(&session.rules.0, &profile).map(|()| {
            format!("** not saved; run `mudular --set-password {profile}` to change that")
        })
    };
    let line = outcome.unwrap_or_else(|err| format!("** {err:#}"));
    session.push_line(RetainedLine::client(line));
}

/// Sends the bound session's input line. Always the bound session, never
/// the focused pane: focusing comms must not redirect commands (§11.1).
async fn submit_input(state: &mut AppState, channels: &[Channel]) {
    let Some(session) = state.bound_mut() else {
        return;
    };
    // The ghost is part of the line: what is on screen is what gets sent,
    // which is the whole bargain of an inline completion (§11.3).
    let line = session.completed_input();
    session.input.reset();
    session.suggestion = None;
    session.dismissed = None;
    // Player intervention always wins over an in-flight `/goto` (§16): a
    // gate the walk hasn't discovered yet, or the player simply changing
    // their mind, is their call, not a queued route's — checked before the
    // line is otherwise handled so nothing below can race an outstanding
    // step against a fresh command.
    if session.walk.take().is_some() {
        session.push_line(RetainedLine::client("** walk cancelled"));
    }
    session.push_history(&line);
    // A bare Enter is a keystroke in its own right — MUD login flows and
    // pagers ask for one ("press return to continue") — so it goes to the
    // server rather than being swallowed as "nothing typed". It is not
    // echoed: a lone `>` in the scrollback would be noise, and the server's
    // own response is the feedback that matters.
    if !line.is_empty() && !session.masked {
        // Never echo what the server is masking.
        session.push_line(RetainedLine::echo(format!("> {line}")));
    }
    // A masked line at a login the profile wants automated is the password
    // it is missing (§13). Offer to keep it rather than making the player
    // find `--set-password`; it is held in memory only until they answer.
    if session.offer_password_save && session.masked && !line.is_empty() {
        session.offer_password_save = false;
        session.pending_password = Some(line.clone());
        let profile = session.rules.1.clone().unwrap_or_default();
        session.push_line(RetainedLine::client(format!(
            "** Save this password in the OS keyring for `{profile}`, \
             so it logs you in next time? (y/n)"
        )));
    }
    if line.trim() == HELP_COMMAND {
        let lines = ui::help_lines(&state.keybinds);
        if let Some(session) = state.bound_mut() {
            for line in lines {
                session.push_line(RetainedLine::client(line));
            }
        }
        return;
    }
    if line.trim() == RELOAD_COMMAND {
        let notice = reload_rules(state, channels).await;
        if let Some(session) = state.bound_mut() {
            session.push_line(RetainedLine::client(notice));
        }
        return;
    }
    if line.trim() == UPDATE_COMMAND {
        // Runs off the loop thread, but awaited here, so the pane stops
        // redrawing until the updater finishes — a few seconds of download.
        // Accepted deliberately: the player typed this, a progress bar would
        // need state on AppState for a command used once a month, and sessions
        // keep reading their sockets throughout because they are separate
        // tasks. Nothing is lost, it just looks still.
        let report = tokio::task::spawn_blocking(crate::update::apply)
            .await
            .unwrap_or_else(|_| vec!["the updater could not be started".to_string()]);
        if let Some(session) = state.bound_mut() {
            for text in report {
                session.push_line(RetainedLine::client(text));
            }
        }
        return;
    }
    if line.trim() == CONFIG_COMMAND {
        open_config_editor(state, channels);
        return;
    }
    if line.trim() == NEWPROFILE_COMMAND {
        state.new_profile_wizard = Some(NewProfileWizard::new(state.config_dir.clone()));
        return;
    }
    let trimmed = line.trim();
    if trimmed == CONNECT_COMMAND || trimmed.starts_with("/connect ") {
        // Not `trimmed.strip_prefix(CONNECT_COMMAND_PREFIX)`: trimming the
        // whole line first already ate a lone trailing space, so
        // "/connect " (no name) would otherwise miss its own prefix check
        // and fall through to the server as literal text instead of
        // reporting the missing name below.
        let name = trimmed.strip_prefix(CONNECT_COMMAND).unwrap_or("").trim();
        if name.is_empty() {
            if let Some(session) = state.bound_mut() {
                session.push_line(RetainedLine::client("** /connect needs a profile name"));
            }
            return;
        }
        connect_new_session(state, channels, name).await;
        return;
    }
    if trimmed == MAP_COMMAND {
        state.show_map = !state.show_map;
        describe_current_room(state);
        return;
    }
    if trimmed == COMMS_COMMAND {
        toggle_comms(state);
        return;
    }
    if trimmed == GOTO_COMMAND || trimmed.starts_with("/goto ") {
        let arg = trimmed.strip_prefix(GOTO_COMMAND).unwrap_or("").trim();
        start_goto(state, arg).await;
        return;
    }
    if trimmed == CORPSE_COMMAND {
        start_corpse_run(state).await;
        return;
    }
    if trimmed == MARK_COMMAND || trimmed.starts_with("/mark ") {
        let label = trimmed.strip_prefix(MARK_COMMAND).unwrap_or("").trim();
        mark_current_room(state, label);
        return;
    }
    if trimmed == SEND_COMMAND || trimmed.starts_with("/send ") {
        let args = trimmed.strip_prefix(SEND_COMMAND).unwrap_or("").trim();
        send_as_other_session(state, args).await;
        return;
    }
    let _ = session.commands.send(SessionCommand::SendLine(line)).await;
}

/// `/send <character> <command>` — run something as another character without
/// leaving this pane (§7.5).
///
/// The routing is a rule's `send_to:`, reached from the keyboard instead of
/// from YAML: the same addressing (a name, or `*` for everyone else), the same
/// reporting when it goes nowhere, and the same rule about the far end. What
/// arrives there is verbatim unless *that* character's config asked for
/// `cross_session: expand_aliases`, so `/send` cannot come to mean something
/// different depending on who it is aimed at.
///
/// Semicolons split here rather than at the far end, because this is a line
/// the player typed and that is what typing `;` does everywhere else in the
/// client (`Engine::expand_input`). Without it `/send * wake;stand` would
/// arrive as one nonsense command on a config that expands nothing.
async fn send_as_other_session(state: &mut AppState, args: &str) {
    let (target, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
    let lines: Vec<String> = rest
        .split(';')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect();
    if target.is_empty() || lines.is_empty() {
        if let Some(session) = state.bound_mut() {
            session.push_line(RetainedLine::client(
                "** /send needs a character and a command, as in \
                 `/send cleric drink well` (or `*` for everyone else)",
            ));
        }
        return;
    }

    let from = state.input_session;
    // Aiming it at the pane you are already typing in is not a delivery
    // failure, and must not be reported as one — `addressed` excludes the
    // sender, so without this it would come back "no session named `tank`"
    // about a character plainly on screen. Not silently sent locally either:
    // a local send would run through this session's aliases when the same
    // command aimed anywhere else would not, which is exactly the "means
    // something different depending on the target" trap above.
    if state.sessions[from].name == target {
        let notice = format!("** /send: `{target}` is the character you are typing as");
        state.sessions[from].push_line(RetainedLine::client(notice));
        return;
    }

    let injections = state.route_send_to(from, target, lines, crate::session::FIRST_HOP, "/send");
    for (index, command) in injections {
        let _ = state.sessions[index].commands.send(command).await;
    }
}

/// Shows or hides the comms column, from either the key or `/comms`
/// (§11.1).
///
/// Says so when there is nothing to show. An install with no `channels:`
/// block has no comms pane to reveal, and a command — or a key — that
/// silently does nothing is how a player concludes the client is broken;
/// the map cursor already answers the same way for the same reason.
fn toggle_comms(state: &mut AppState) {
    if state.channels.is_empty() {
        if let Some(session) = state.bound_mut() {
            session.push_line(RetainedLine::client(
                "** no comms panes to show — declare one under `channels:` in mudular.yaml",
            ));
        }
        return;
    }
    state.show_channels = !state.show_channels;
    // Focus must never rest on a pane that is no longer drawn.
    if !state.show_channels && matches!(state.focus, Focus::Channel(_)) {
        state.focus_pane(Focus::Session(state.input_session));
    }
}

/// Prints where the character is standing and what leads out of it, as
/// ordinary scrollback lines (§16).
///
/// Runs on every `/map` and every toggle key, in both directions, rather
/// than only when the column is coming up. The drawn column is glyphs at
/// coordinates in the alternate screen, which a screen reader cannot read
/// at all; these lines are the form of the map that everyone can reach, so
/// they are not a consolation for having the pane off. They also scroll,
/// copy, and land in the disk log, which the pane does none of.
fn describe_current_room(state: &mut AppState) {
    let described = state
        .bound()
        .and_then(|session| session.current_room)
        .zip(state.map_of(state.input_session))
        .map(|(at, map)| map.describe(at))
        .filter(|lines| !lines.is_empty());
    let Some(session) = state.bound_mut() else {
        return;
    };
    match described {
        Some(lines) => {
            for line in lines {
                session.push_line(RetainedLine::client(line));
            }
        }
        // Same plain report `/goto` gives: a command that answers nothing
        // reads as a broken command.
        None => session.push_line(RetainedLine::client(
            "** the map doesn't know where you are yet",
        )),
    }
}

/// `/goto <vnum | name substring>` (§16): walks toward a room the map
/// already knows, one direction at a time. Never the whole path in one
/// burst — see `Walk`'s doc comment for why a mid-route surprise has to stop
/// the walk rather than keep firing steps planned for a world that has since
/// changed.
async fn start_goto(state: &mut AppState, arg: &str) {
    let Some(current) = state.bound().and_then(|session| session.current_room) else {
        if let Some(session) = state.bound_mut() {
            session.push_line(RetainedLine::client(
                "** /goto doesn't know where you are yet",
            ));
        }
        return;
    };

    let target = if let Ok(vnum) = arg.parse::<i64>() {
        let id = crate::map::RoomId(vnum);
        if !state
            .bound_map()
            .is_some_and(|map| map.rooms.contains_key(&id))
        {
            let session = &mut state.sessions[state.input_session];
            session.push_line(RetainedLine::client(format!(
                "** /goto: no room #{vnum} on the map"
            )));
            return;
        }
        id
    } else {
        let needle = arg.to_ascii_lowercase();
        let Some(map) = state.bound_map() else {
            return;
        };
        let matches: Vec<&crate::map::Room> = map
            .rooms
            .values()
            .filter(|room| {
                room.name
                    .as_deref()
                    .is_some_and(|name| name.to_ascii_lowercase().contains(&needle))
            })
            .collect();
        // Decided while the map is still borrowed, acted on after: the
        // refusals want to write to the session, which the borrow rules
        // will not allow until this is an owned answer.
        let chosen: Result<crate::map::RoomId, String> = match matches.as_slice() {
            [] => Err(format!("** /goto: no room matches `{arg}`")),
            [room] => Ok(room.id),
            many => {
                // Silently picking one would walk the character somewhere
                // they did not ask for — a vnum is unambiguous, so that is
                // what the player is asked to name instead.
                let mut candidates: Vec<String> = many
                    .iter()
                    .take(5)
                    .map(|room| format!("#{} {}", room.id.0, room.name.as_deref().unwrap_or("")))
                    .collect();
                if many.len() > 5 {
                    candidates.push("…".to_string());
                }
                Err(format!(
                    "** /goto: `{arg}` matches more than one room, say which vnum: {}",
                    candidates.join(", ")
                ))
            }
        };
        match chosen {
            Ok(id) => id,
            Err(notice) => {
                if let Some(session) = state.bound_mut() {
                    session.push_line(RetainedLine::client(notice));
                }
                return;
            }
        }
    };

    let index = state.input_session;
    walk_to(state, index, current, target, GOTO_COMMAND).await;
}

/// Walks to a room the map cursor picked, reusing `/goto`'s route and its
/// one-step-at-a-time walk (§16) — which is the point of the cursor: the
/// map stops being a picture and becomes somewhere to say "there".
async fn walk_to_room(state: &mut AppState, target: crate::map::RoomId) {
    let Some(session) = state.bound_mut() else {
        return;
    };
    let Some(current) = session.current_room else {
        return;
    };
    let index = state.input_session;
    walk_to(state, index, current, target, GOTO_COMMAND).await;
}

/// Where the map cursor lands when nudged one step in `step`, or `None` if
/// nothing is drawn that way.
///
/// Moves room to room rather than cell to cell: the grid is mostly empty
/// space, and a cursor that could sit on a blank cell would need the player
/// to steer around gaps to reach a room two along. Only rooms the picture
/// actually shows are reachable, which is the same rule the pane draws by.
fn map_cursor_step(
    state: &AppState,
    from: crate::map::RoomId,
    step: (i32, i32),
) -> Option<crate::map::RoomId> {
    let session = state.bound()?;
    let scene = state
        .map_of(state.input_session)?
        // No party: stepping the cursor only reads where rooms are, and
        // who is standing in one has no bearing on which room is next.
        .scene(session.current_room?, session.corpse, &[]);
    let at = scene.rooms.iter().find(|room| room.id == from)?.at;
    let wanted = (at.0 + step.0, at.1 + step.1);
    scene
        .rooms
        .iter()
        .find(|room| room.at == wanted)
        .map(|room| room.id)
}

/// `/mark <label>` (§16): writes the player's own note onto the room they
/// are standing in, and `/mark` alone rubs it out again.
///
/// The client never guesses the label. A Diku MUD's MSDP carries vnum,
/// name, area and exits and nothing about what a room is *for* — no shop
/// flag, no terrain — so the only honest source for "this is the baker's"
/// is the person who walked in and read the sign.
/// Writes a mark into this session's world and puts it on disk. Returns
/// whether anything actually changed.
///
/// One map per world (§16), so there is nothing to fan out: every
/// character on this MUD is already reading the map being written here.
/// That is the point of sharing it — the version that copied the map per
/// session had to remember to update every sibling, and a sibling left
/// holding an old label wrote it straight back on its next save.
///
/// Written immediately rather than left to the next periodic tick or a
/// clean quit, since a process that dies first takes both of those with
/// it, and a mark is one keystroke the player is not walking back into.
fn set_mark(
    state: &mut AppState,
    from: usize,
    at: crate::map::RoomId,
    mark: Option<String>,
) -> bool {
    let world = state.world_mut(from);
    if !world.map.set_mark(at, mark) {
        return false;
    }
    world.explicit_marks.insert(at);

    let key = state
        .sessions
        .get(from)
        .map(|session| session.map_key.clone())
        .unwrap_or_default();
    // Dirty exactly when the write did not happen — a failure, or a
    // session with no world to write to — so the periodic tick still owes
    // it a retry in both cases and in neither other.
    let wrote = save_world_map(state, &key);
    state.world_mut(from).dirty = !wrote;
    true
}

fn mark_current_room(state: &mut AppState, label: &str) {
    let Some(session) = state.bound_mut() else {
        return;
    };
    let Some(at) = session.current_room else {
        session.push_line(RetainedLine::client(
            "** /mark doesn't know where you are yet",
        ));
        return;
    };

    let previous = state
        .map_of(state.input_session)
        .and_then(|map| map.rooms.get(&at))
        .and_then(|room| room.mark.clone());

    // Asked with nothing to write, offer the usual answers rather than
    // making the player remember what they called the last one. Typing a
    // label outright still works and is the only way to say something the
    // list does not.
    if label.is_empty() {
        state.mark_menu = Some(MarkMenu {
            at,
            selected: 0,
            existing: previous,
            typing: None,
        });
        return;
    }

    let notice = match previous {
        Some(old) if old == label => format!("** #{} is already marked `{label}`", at.0),
        _ => format!("** marked #{} as `{label}`", at.0),
    };
    // Every character on this world, not only the one who typed it — see
    // `set_mark_across_world`.
    let from = state.input_session;
    set_mark(state, from, at, Some(label.to_string()));
    if let Some(session) = state.bound_mut() {
        session.push_line(RetainedLine::client(notice));
    }
}

/// Writes what the `/mark` chooser landed on, and closes it.
fn apply_mark_menu(state: &mut AppState) {
    let Some(menu) = state.mark_menu.as_mut() else {
        return;
    };
    // Asking for a label of your own does not close the chooser — it turns
    // it into somewhere to type one.
    if let (None, MarkChoice::Custom) = (&menu.typing, menu.choice()) {
        menu.typing = Some(String::new());
        return;
    }
    let chosen = match menu.typing.take() {
        // A custom label typed and then left empty means "never mind",
        // rather than silently removing whatever was there.
        Some(typed) if typed.trim().is_empty() => {
            state.mark_menu = None;
            return;
        }
        Some(typed) => Some(typed.trim().to_string()),
        None => match menu.choice() {
            MarkChoice::Label(label) => Some(label),
            MarkChoice::Remove => None,
            MarkChoice::Custom => unreachable!("handled above"),
        },
    };
    let menu = state.mark_menu.take().expect("just checked");
    if state.bound().is_none() {
        return;
    }
    let notice = match &chosen {
        Some(label) => format!("** marked #{} as `{label}`", menu.at.0),
        None => format!("** unmarked #{}", menu.at.0),
    };
    // Removing is exactly the case `set_mark_across_world` exists for: a
    // sibling still holding the old label would otherwise write it back.
    let from = state.input_session;
    set_mark(state, from, menu.at, chosen);
    if let Some(session) = state.bound_mut() {
        session.push_line(RetainedLine::client(notice));
    }
}

/// `/corpse` (§16): retraces the way back to the room a `corpse:` trigger
/// last marked. The whole feature is `/goto` with the target remembered
/// for you — the miserable part of a corpse run was never the walking, it
/// was having to recall where you were when you died.
async fn start_corpse_run(state: &mut AppState) {
    let Some(session) = state.bound_mut() else {
        return;
    };
    let Some(corpse) = session.corpse else {
        // Distinguished from "no route" deliberately: a player whose death
        // was never marked needs to be told to write the trigger, not to
        // go explore.
        session.push_line(RetainedLine::client(
            "** /corpse: no death recorded — a trigger needs `corpse: true` on this MUD's death message",
        ));
        return;
    };
    let Some(current) = session.current_room else {
        session.push_line(RetainedLine::client(
            "** /corpse doesn't know where you are yet",
        ));
        return;
    };
    let index = state.input_session;
    walk_to(state, index, current, corpse, CORPSE_COMMAND).await;
}

/// Starts a one-step-at-a-time walk from `current` to `target`, or says
/// plainly why it can't (§16). Shared by `/goto` and `/corpse`, which
/// differ only in how they arrive at a target room — `command` is the name
/// each refusal should blame, since a message naming the wrong command
/// reads as a bug in whichever one the player actually typed.
async fn walk_to(
    state: &mut AppState,
    index: usize,
    current: crate::map::RoomId,
    target: crate::map::RoomId,
    command: &str,
) {
    if target == current {
        state.sessions[index]
            .push_line(RetainedLine::client(format!("** {command}: already there")));
        return;
    }

    let Some(path) = state
        .map_of(index)
        .and_then(|map| map.path(current, target))
    else {
        state.sessions[index].push_line(RetainedLine::client(format!(
            "** {command}: no known route there"
        )));
        return;
    };

    let mut remaining: VecDeque<String> = path.into();
    // `path` never returns an empty list when `current != target` — it
    // returns `None` instead when there's no route — so this is always one
    // real direction, not a placeholder.
    let direction = remaining.pop_front().expect("path is non-empty here");
    // Sound today — `path` ran against this same map a few lines up — but
    // not worth asserting: the version of this in the arrival handler made
    // exactly this argument and panicked the client when the map moved
    // underneath it. A route that cannot be started is a route that cannot
    // be walked, and saying so costs nothing.
    let Some(expecting) = state
        .map_of(index)
        .and_then(|map| map.rooms.get(&current))
        .and_then(|room| room.exits.get(&direction))
        .copied()
        .flatten()
    else {
        state.sessions[index].push_line(RetainedLine::client(format!(
            "** {command}: the map no longer knows where `{direction}` leads from here"
        )));
        return;
    };

    let steps = remaining.len() + 1;
    let session = &mut state.sessions[index];
    session.walk = Some(Walk {
        remaining,
        expecting,
        destination: target,
    });
    session.push_line(RetainedLine::client(format!(
        "** walking to #{} ({steps} step{})",
        target.0,
        if steps == 1 { "" } else { "s" }
    )));
    let _ = session
        .commands
        .send(SessionCommand::SendLine(direction))
        .await;
}

/// `/connect <profile>` (§7.5, `ARCH_REVIEW.md` "Features that would break
/// the architecture"): adds a character to this running instance rather
/// than requiring a relaunch with every profile named up front. Builds the
/// target the same way `main.rs` does at startup (`build_profile_target`),
/// then does the two things the peer mesh being built once before
/// `event_loop` and never revisited had made impossible — hands the new
/// session everyone else's receivers, and everyone else the new session's,
/// so `${@name.var}`/`mud.on_peer` work in both directions immediately
/// rather than only for characters named on the command line. `--record`
/// never applies here: it is a startup flag with no CLI invocation to read
/// from at this point.
async fn connect_new_session(state: &mut AppState, channels: &[Channel], name: &str) {
    let mut taken: Vec<String> = state.sessions.iter().map(|s| s.name.clone()).collect();
    let target = match build_profile_target(
        &state.config_dir,
        name,
        &mut taken,
        channels,
        state.cross_session_default,
        None,
    ) {
        Ok(target) => target,
        Err(err) => {
            if let Some(session) = state.bound_mut() {
                session.push_line(RetainedLine::client(format!(
                    "** could not connect `{name}`: {err:#}"
                )));
            }
            return;
        }
    };

    let (publish_tx, publish_rx) = watch::channel(PeerSnapshot::default());
    let others = state.peer_registry.clone();
    for session in &state.sessions {
        let _ = session
            .commands
            .send(SessionCommand::AddPeer {
                name: target.name.clone(),
                rx: publish_rx.clone(),
            })
            .await;
    }
    state.peer_registry.insert(target.name.clone(), publish_rx);

    let connected_name = target.name.clone();
    let links = PeerLinks {
        publish: Some(publish_tx),
        others,
    };
    let session = connect(
        target,
        state.history_size,
        state.scrollback_size,
        state.autocomplete,
        links,
    );
    let key = session.map_key.clone();
    state.sessions.push(session);
    open_world(&mut state.maps, &state.config_dir, &key);

    // Focused immediately, the same as a freshly launched character would
    // be — the confirmation below lands in the pane it describes.
    let index = state.sessions.len() - 1;
    state.focus = Focus::Session(index);
    state.input_session = index;

    if let Some(session) = state.bound_mut() {
        session.push_line(RetainedLine::client(format!(
            "** connected {connected_name}"
        )));
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
                gmcp_seen: false,
                msdp_seen: false,
                unread: 0,
                distress: None,
                color: None,
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
                back_offset: 0,
                current_room: None,
                corpse: None,
                // Empty on purpose: a test pane saves nothing unless it
                // says which world it belongs to, so tests that never
                // mention maps do not write files or report failing to.
                map_key: String::new(),
                walk: None,
                vocabulary: crate::complete::Vocabulary::default(),
                suggestion: None,
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
        (
            AppState {
                sessions,
                maps: HashMap::new(),
                channels: Vec::new(),
                focus: Focus::Session(0),
                input_session: 0,
                layout: LayoutMode::Tabs,
                show_channels: false,
                channel_width: crate::ui::CHANNEL_WIDTH,
                show_map: false,
                map_width: crate::ui::MAP_WIDTH,
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

#[cfg(test)]
mod tests {
    use super::test_support::{app_with_receivers as app, channel};
    use super::*;
    use crate::net::Security;
    use crate::scrollback::Origin;

    /// Only a picture *going* strands pixels ratatui will not paint over.
    /// One arriving is handled where it lands — the cells under it are not
    /// skipped until the terminal already has it — so clearing the whole
    /// screen for that, and blanking every pane for a frame, bought
    /// nothing.
    #[test]
    fn only_a_vanished_picture_needs_the_screen_cleared() {
        assert!(image_vanished(true, false), "gone: its pixels are stranded");
        assert!(
            !image_vanished(false, true),
            "arriving: the diff repaints the region it lands on"
        );
        assert!(!image_vanished(false, false));
        assert!(!image_vanished(true, true));
    }

    /// `--map-debug`'s snapshot has to carry enough to find the room again
    /// in the saved map file, not just what a screenshot already shows.
    #[test]
    fn a_map_debug_snapshot_names_the_room_and_pictures_the_scene() {
        let mut state = test_support::app(&["tank"]);
        put_room(state.map_of_mut(0), 1, Some("Temple"), &[("n", 2)]);
        put_room(state.map_of_mut(0), 2, None, &[]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));

        let snapshot = map_debug_snapshot(&state, true, "@ captured screen text @");

        assert!(snapshot.contains("session: tank"), "{snapshot}");
        assert!(snapshot.contains("current room: 1"), "{snapshot}");
        assert!(snapshot.contains("Temple"), "{snapshot}");
        assert!(
            snapshot.contains("sixel image this frame: true"),
            "{snapshot}"
        );
        assert!(
            snapshot.contains("@ captured screen text @"),
            "the captured screen text should be carried through verbatim: {snapshot}"
        );
        assert!(snapshot.contains("rooms:"), "{snapshot}");
        assert!(snapshot.contains("links:"), "{snapshot}");
    }

    /// The snapshot's picture has to be what the real terminal actually
    /// shows — ratatui's own buffer for the pane, not a separate re-render
    /// sized and laid out differently, which was the bug report that led
    /// here.
    #[test]
    fn capture_map_area_reads_ratatuis_own_buffer() {
        let mut buffer = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 10, 3));
        buffer[(2, 1)].set_symbol("@");
        let area = ratatui::layout::Rect::new(1, 1, 4, 1);

        let text = capture_map_area(&buffer, Some(area), None);

        assert_eq!(
            text, " @",
            "only the requested area, right-trimmed: {text:?}"
        );
    }

    /// The cells under a Sixel picture are marked skipped, so ratatui's own
    /// buffer never learns what the terminal actually shows there — the
    /// glyphs `write_map_image` writes afterward are the only ground truth
    /// a text snapshot has for that region, and have to be overlaid rather
    /// than left to whatever stale content the buffer is still holding.
    #[test]
    fn capture_map_area_overlays_the_glyphs_a_skipped_region_would_otherwise_hide() {
        let buffer = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 10, 3));
        let area = ratatui::layout::Rect::new(0, 0, 5, 1);
        let image = ui::PendingImage {
            area,
            sixel: String::new(),
            glyphs: vec![(2, 0, '@', ratatui::style::Style::default())],
        };

        let text = capture_map_area(&buffer, Some(area), Some(&image));

        assert!(text.contains('@'), "{text}");
    }

    /// No map pane on screen this frame — the map hidden, or no room to
    /// centre on — has to say so rather than silently showing a blank
    /// picture that reads the same as a bug.
    #[test]
    fn capture_map_area_says_so_when_there_is_no_pane_to_read() {
        let buffer = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 10, 3));
        assert_eq!(
            capture_map_area(&buffer, None, None),
            "(map pane not shown this frame)"
        );
    }

    /// A character who has connected but not yet had a room described is
    /// the exact case the stale-Sixel bug hit — the snapshot has to say so
    /// plainly rather than rendering an empty picture that looks the same
    /// as a bug.
    #[test]
    fn a_map_debug_snapshot_says_when_there_is_no_room_yet() {
        let state = test_support::app(&["tank"]);
        let snapshot = map_debug_snapshot(&state, false, "");
        assert!(snapshot.contains("no room data yet"), "{snapshot}");
    }

    /// The point of dedup: a `MapDebugWriter` running for a whole session
    /// would otherwise write one file per frame, most of them identical.
    #[test]
    fn a_map_debug_writer_only_writes_when_the_snapshot_changes() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let mut writer = MapDebugWriter::new(dir.path().to_path_buf());
        let mut state = test_support::app(&["tank"]);
        put_room(state.map_of_mut(0), 1, None, &[]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));

        writer
            .record(&state, false, String::new)
            .expect("first write");
        writer
            .record(&state, false, String::new)
            .expect("unchanged write");
        writer
            .record(&state, true, String::new)
            .expect("changed write (image now present)");

        let written: Vec<_> = std::fs::read_dir(dir.path())
            .expect("the debug dir should exist")
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            written.len(),
            2,
            "the unchanged repeat should not have written a second file"
        );
    }

    /// The bug this pins: `record` used to lay out the whole area and
    /// build the room/link listing on every call, keystrokes included, and
    /// only decide afterward whether the result was worth keeping. On a
    /// busy area that made typing sluggish. `MapDebugKey` has to reject an
    /// unchanged frame before `screen_text` — the expensive part — is ever
    /// called at all.
    #[test]
    fn an_unchanged_frame_never_pays_for_the_screen_text() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let mut writer = MapDebugWriter::new(dir.path().to_path_buf());
        let mut state = test_support::app(&["tank"]);
        put_room(state.map_of_mut(0), 1, None, &[]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));

        let calls = std::cell::Cell::new(0);
        let screen_text = || {
            calls.set(calls.get() + 1);
            String::new()
        };

        writer.record(&state, false, screen_text).unwrap();
        assert_eq!(calls.get(), 1, "the first call always has to do the work");

        writer.record(&state, false, screen_text).unwrap();
        assert_eq!(
            calls.get(),
            1,
            "an unchanged key must skip screen_text entirely, not just skip the write"
        );
    }

    fn scrollback(session: &SessionPane) -> String {
        session
            .scrollback
            .iter()
            .map(|line| line.text.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The client's own voice has to stay distinguishable from the MUD's
    /// after the fact (docs/UX_REVIEW.md D): a `**` in the text is a
    /// convention anyone can type, `origin` is what the pane actually knows.
    #[tokio::test]
    async fn a_pane_records_who_wrote_each_line() {
        let (mut state, _rx) = app(&["tank"]);
        apply_session_event(&mut state, 0, SessionEvent::server("You see a rat."));
        state.sessions[0].input = Input::default().with_value("kill rat".into());
        submit_input(&mut state, &[]).await;
        apply_session_event(&mut state, 0, SessionEvent::Ended("host went away".into()));

        let origins: Vec<&Origin> = state.sessions[0]
            .scrollback
            .iter()
            .map(|line| &line.origin)
            .collect();
        assert_eq!(
            origins,
            [&Origin::Server, &Origin::Echo, &Origin::Client],
            "{}",
            scrollback(&state.sessions[0])
        );
    }

    /// §13: an unverified connection must be visible in the pane, not just
    /// implied by a status label the player may never look at.
    #[test]
    fn surfaces_a_security_warning_in_the_pane() {
        let (mut state, _rx) = app(&["tank"]);
        apply_session_event(
            &mut state,
            0,
            SessionEvent::Security(Security {
                label: "TLS insecure".to_string(),
                warning: Some("certificate NOT verified".to_string()),
            }),
        );

        assert_eq!(state.sessions[0].security, "TLS insecure");
        assert!(
            scrollback(&state.sessions[0]).contains("certificate NOT verified"),
            "warning missing from the pane: {:?}",
            scrollback(&state.sessions[0])
        );
    }

    #[test]
    fn a_verified_connection_adds_no_noise() {
        let (mut state, _rx) = app(&["tank"]);
        apply_session_event(
            &mut state,
            0,
            SessionEvent::Security(Security {
                label: "TLS".to_string(),
                warning: None,
            }),
        );

        assert_eq!(state.sessions[0].security, "TLS");
        assert!(state.sessions[0].scrollback.is_empty());
    }

    /// A round trip is only worth showing while one is possible: a pane
    /// that has lost its connection would otherwise keep advertising the
    /// last good number as though data were still flowing (§11).
    #[test]
    fn latency_shows_once_measured_and_goes_with_the_connection() {
        let (mut state, _rx) = app(&["tank"]);
        assert!(state.sessions[0].latency.is_empty());

        apply_session_event(
            &mut state,
            0,
            SessionEvent::Latency(std::time::Duration::from_millis(42)),
        );
        assert_eq!(state.sessions[0].latency, "42ms");

        apply_session_event(&mut state, 0, SessionEvent::Ended("closed".to_string()));
        assert!(state.sessions[0].latency.is_empty());
    }

    /// The status line is one row and cuts a long reason off mid-word —
    /// exactly what a TLS pin mismatch's full fingerprint comparison does
    /// not survive. Scrollback wraps and never truncates, so the complete
    /// reason must land there too, the same way a §13 security warning
    /// already does.
    #[test]
    fn a_disconnect_reason_reaches_scrollback_uncut() {
        let (mut state, _rx) = app(&["tank"]);
        let reason = "TLS handshake with 127.0.0.1:5577: unexpected error: \
                       pinned certificate mismatch: expected SHA-256 aaaa, \
                       server offered bbbb";

        apply_session_event(&mut state, 0, SessionEvent::Ended(reason.to_string()));

        assert!(
            scrollback(&state.sessions[0]).contains(reason),
            "full disconnect reason missing from the pane: {:?}",
            scrollback(&state.sessions[0])
        );
    }

    /// A focused session's own bell is not worth ringing — the player is
    /// already looking at it. An unfocused one is exactly the case the
    /// notification exists for (§14 M9).
    #[test]
    fn a_bell_rings_only_for_an_unfocused_session() {
        let (state, _rx) = app(&["tank", "cleric"]);
        assert!(state.is_focused(0));
        assert!(!wants_bell(&state, 0, &SessionEvent::Bell));
        assert!(wants_bell(&state, 1, &SessionEvent::Bell));
        assert!(!wants_bell(&state, 1, &SessionEvent::server("hi")));
    }

    /// A password typed while the server is echoing must not be written to
    /// the scrollback the player can scroll back through.
    #[test]
    fn masked_input_is_never_echoed_to_scrollback() {
        let (mut state, _rx) = app(&["tank"]);
        state.sessions[0].masked = true;
        // Mirrors the Enter branch of the event loop.
        let session = &mut state.sessions[0];
        let line = "hunter2".to_string();
        if !session.masked {
            session.push_line(RetainedLine::echo(format!("> {line}")));
        }
        assert!(session.scrollback.is_empty());

        session.masked = false;
        session.push_line(RetainedLine::echo("> look"));
        assert_eq!(scrollback(session), "> look");
    }

    /// The same guard that keeps a password out of scrollback must keep it
    /// out of the transcript file — disk logging must never be the one
    /// place a masked line survives.
    #[test]
    fn masked_input_is_never_written_to_the_log_file() {
        let (mut session, _rx) = test_support::pane("tank");
        let path = std::env::temp_dir().join(format!(
            "mudular-test-masked-{}-{:?}.log",
            std::process::id(),
            std::thread::current().id()
        ));
        session.log = open_log(&path);
        assert!(session.log.is_some(), "log file failed to open");

        session.masked = true;
        let line = "hunter2".to_string();
        // Mirrors the Enter branch of the event loop: a masked line never
        // reaches push_line at all.
        if !session.masked {
            session.push_line(RetainedLine::echo(format!("> {line}")));
        }

        session.masked = false;
        session.push_line(RetainedLine::echo("> look"));
        drop(session);

        let logged = std::fs::read_to_string(&path).unwrap();
        assert_eq!(logged, "> look\n");
        assert!(!logged.contains("hunter2"));
        std::fs::remove_file(&path).unwrap();
    }

    /// A transcript can hold anything a server echoes back, including a MUD
    /// account's own login exchange — owner-only on disk, not left at the
    /// process umask.
    #[cfg(unix)]
    #[test]
    fn open_log_creates_the_file_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!(
            "mudular-test-perms-{}-{:?}.log",
            std::process::id(),
            std::thread::current().id()
        ));
        let log = open_log(&path);
        assert!(log.is_some(), "log file failed to open");
        drop(log);

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
    }

    /// Disk logging (§8, §12) writes through the same choke point as
    /// scrollback, so anything that reaches the pane reaches the transcript
    /// too — and, by the same token, a masked line reaches neither.
    #[test]
    fn push_line_appends_to_the_open_log_file() {
        let (mut session, _rx) = test_support::pane("tank");
        let path = std::env::temp_dir().join(format!(
            "mudular-test-{}-{:?}.log",
            std::process::id(),
            std::thread::current().id()
        ));
        session.log = open_log(&path);
        assert!(session.log.is_some(), "log file failed to open");

        session.push_line(RetainedLine::server("You see a rat."));
        session.push_line(RetainedLine::server("You swing your sword."));
        drop(session); // flushes the BufWriter

        let logged = std::fs::read_to_string(&path).unwrap();
        assert_eq!(logged, "You see a rat.\nYou swing your sword.\n");
        std::fs::remove_file(&path).unwrap();
    }

    /// The inspector view's data source (§14 M6): raw GMCP messages land in
    /// their own log, not the scrollback.
    #[test]
    fn a_gmcp_message_is_logged_for_the_inspector_view() {
        let (mut state, _rx) = app(&["tank"]);
        apply_session_event(
            &mut state,
            0,
            SessionEvent::Gmcp {
                package: "Char.Vitals".to_string(),
                payload: Some(r#"{"hp":100}"#.to_string()),
            },
        );
        assert_eq!(state.sessions[0].inspector_log.len(), 1);
        assert_eq!(
            state.sessions[0].inspector_log[0],
            r#"[GMCP] Char.Vitals {"hp":100}"#
        );
        assert!(
            state.sessions[0].scrollback.is_empty(),
            "GMCP must not reach scrollback"
        );
    }

    /// The MSDP twin of `a_gmcp_message_is_logged_for_the_inspector_view`:
    /// both protocols share one log, tagged so a line's origin is never
    /// ambiguous (§6.3).
    #[test]
    fn an_msdp_update_is_logged_for_the_inspector_view() {
        let (mut state, _rx) = app(&["tank"]);
        apply_session_event(
            &mut state,
            0,
            SessionEvent::Msdp {
                pairs: vec![("HP".to_string(), "100".to_string())],
            },
        );
        assert_eq!(state.sessions[0].inspector_log.len(), 1);
        assert_eq!(state.sessions[0].inspector_log[0], "[MSDP] HP 100");
        assert!(
            state.sessions[0].scrollback.is_empty(),
            "MSDP must not reach scrollback"
        );
    }

    /// A server that speaks both protocols interleaves both tags in the one
    /// log, in arrival order (§6.3) — the whole point of tagging lines
    /// rather than keeping separate buffers.
    #[test]
    fn gmcp_and_msdp_lines_share_one_tagged_log() {
        let (mut state, _rx) = app(&["tank"]);
        apply_session_event(
            &mut state,
            0,
            SessionEvent::Gmcp {
                package: "Char.Vitals".to_string(),
                payload: Some(r#"{"hp":100}"#.to_string()),
            },
        );
        apply_session_event(
            &mut state,
            0,
            SessionEvent::Msdp {
                pairs: vec![("HP".to_string(), "100".to_string())],
            },
        );
        let log = &state.sessions[0].inspector_log;
        assert_eq!(log.len(), 2);
        assert_eq!(log[0], r#"[GMCP] Char.Vitals {"hp":100}"#);
        assert_eq!(log[1], "[MSDP] HP 100");
    }

    /// Control bytes in MSDP keys/values must be escaped before they reach
    /// the log, the same as GMCP's — the inspector renders with `Line::raw`,
    /// bypassing the ANSI parser that would otherwise interpret them (§13).
    #[test]
    fn msdp_control_bytes_are_escaped_in_the_inspector_log() {
        let (mut session, _rx) = test_support::pane("tank");
        session.push_msdp(vec![("K\x1b[31mEY".to_string(), "V\x07AL".to_string())]);
        let line = &session.inspector_log[0];
        assert!(!line.contains('\x1b'));
        assert!(!line.contains('\x07'));
    }

    /// The inspector title tells the player which protocol(s) actually
    /// showed up, not which the client merely supports (§6.3) — an
    /// MSDP-only MUD must not look like a broken GMCP view.
    #[test]
    fn inspector_title_reflects_which_protocols_have_produced_data() {
        let (mut session, _rx) = test_support::pane("tank");
        assert_eq!(
            session.inspector_title(),
            "server data — nothing received yet"
        );

        session.push_gmcp("Char.Vitals".to_string(), None);
        assert_eq!(session.inspector_title(), "GMCP inspector");

        session.push_msdp(vec![("HP".to_string(), "100".to_string())]);
        assert_eq!(session.inspector_title(), "GMCP + MSDP inspector");

        let (mut msdp_only, _rx2) = test_support::pane("cleric");
        msdp_only.push_msdp(vec![("HP".to_string(), "100".to_string())]);
        assert_eq!(msdp_only.inspector_title(), "MSDP inspector");
    }

    #[test]
    fn ending_a_session_clears_masking_and_connected_state() {
        let (mut state, _rx) = app(&["tank"]);
        state.sessions[0].masked = true;
        let (ended, _) = apply_session_event(
            &mut state,
            0,
            SessionEvent::Ended("connection closed".to_string()),
        );

        assert!(ended);
        assert!(
            !state.sessions[0].masked,
            "a dead session must not leave input hidden"
        );
        assert!(!state.sessions[0].connected);
        assert!(state.sessions[0].status.contains("connection closed"));
    }

    // ---- per-session isolation audit (docs/ARCHITECTURE.md §14 M7) ----

    /// The M7 acceptance criterion's first half: two characters played at
    /// once with no cross-talk. Every kind of per-session state — output,
    /// prompt, echo masking, security, GMCP — must land in exactly one pane.
    #[test]
    fn no_session_state_leaks_into_another_pane() {
        let (mut state, _rx) = app(&["tank", "cleric"]);

        apply_session_event(&mut state, 0, SessionEvent::server("tank sees this"));
        apply_session_event(&mut state, 0, SessionEvent::Prompt("HP:100>".into()));
        apply_session_event(&mut state, 0, SessionEvent::EchoMask(true));
        apply_session_event(
            &mut state,
            0,
            SessionEvent::Gmcp {
                package: "Char.Vitals".into(),
                payload: None,
            },
        );
        apply_session_event(&mut state, 1, SessionEvent::server("cleric sees this"));

        assert_eq!(scrollback(&state.sessions[0]), "tank sees this");
        assert_eq!(scrollback(&state.sessions[1]), "cleric sees this");
        assert_eq!(state.sessions[0].prompt, "HP:100>");
        assert!(state.sessions[1].prompt.is_empty());
        assert!(state.sessions[0].masked);
        assert!(!state.sessions[1].masked, "echo masking must not spread");
        assert_eq!(state.sessions[0].inspector_log.len(), 1);
        assert!(state.sessions[1].inspector_log.is_empty());
    }

    /// Input buffers are per-session: switching focus never mixes them
    /// (docs/ARCHITECTURE.md §11).
    #[test]
    fn input_buffers_stay_with_their_session() {
        let (mut state, _rx) = app(&["tank", "cleric"]);
        state.sessions[0].input = Input::default().with_value("kill rat".into());

        state.focus_pane(Focus::Session(1));
        assert_eq!(state.bound().unwrap().input.value(), "");

        state.focus_pane(Focus::Session(0));
        assert_eq!(state.bound().unwrap().input.value(), "kill rat");
    }

    /// A session ending must not disturb the others (§12).
    #[test]
    fn one_session_ending_leaves_the_others_alone() {
        let (mut state, _rx) = app(&["tank", "cleric"]);
        apply_session_event(&mut state, 0, SessionEvent::Ended("socket dropped".into()));

        assert!(!state.sessions[0].connected);
        assert!(state.sessions[1].connected);
        assert!(state.sessions[1].scrollback.is_empty());
    }

    // ---- unread indicators (§11) ----

    #[test]
    fn unfocused_sessions_count_unread_lines_until_focused() {
        let (mut state, _rx) = app(&["tank", "cleric"]);

        apply_session_event(&mut state, 1, SessionEvent::server("a tell arrives"));
        apply_session_event(&mut state, 1, SessionEvent::server("and another"));
        apply_session_event(&mut state, 0, SessionEvent::server("focused output"));

        assert_eq!(state.sessions[1].unread, 2);
        assert_eq!(state.sessions[0].unread, 0, "the focused pane is read");

        state.focus_pane(Focus::Session(1));
        assert_eq!(state.sessions[1].unread, 0);
    }

    // ---- focus (§11) ----

    #[test]
    fn cycling_focus_wraps_through_sessions_and_visible_channels() {
        let (mut state, _rx) = app(&["tank", "cleric"]);
        state.channels.push(ChannelPane {
            config: channel("comms"),
            lines: VecDeque::new(),
            unread: 0,
            scrollback_limit: 10_000,
            back_offset: 0,
        });
        state.show_channels = true;

        state.focus_next();
        assert_eq!(state.focus, Focus::Session(1));
        state.focus_next();
        assert_eq!(state.focus, Focus::Channel(0));
        state.focus_next();
        assert_eq!(state.focus, Focus::Session(0));
    }

    /// Focusing a channel pane must leave the input bound to the last
    /// focused session — reading comms never redirects your commands.
    #[test]
    fn focusing_a_channel_leaves_input_bound_to_the_session() {
        let (mut state, _rx) = app(&["tank", "cleric"]);
        state.channels.push(ChannelPane {
            config: channel("comms"),
            lines: VecDeque::new(),
            unread: 0,
            scrollback_limit: 10_000,
            back_offset: 0,
        });
        state.show_channels = true;

        state.focus_pane(Focus::Session(1));
        state.focus_pane(Focus::Channel(0));

        assert_eq!(state.focus, Focus::Channel(0));
        assert_eq!(state.input_session, 1, "input stays with the last session");
        assert_eq!(state.bound().unwrap().name, "cleric");
    }

    // ---- channel panes (§11.1) ----

    fn with_channel(state: &mut AppState, name: &str, timestamps: bool) {
        state.channels.push(ChannelPane {
            config: Channel {
                timestamps,
                ..channel(name)
            },
            lines: VecDeque::new(),
            unread: 0,
            scrollback_limit: 10_000,
            back_offset: 0,
        });
        state.show_channels = true;
    }

    /// The M7 criterion's third half: tells land in a comms pane. With more
    /// than one session they carry an origin tag.
    #[test]
    fn routed_lines_land_in_the_channel_with_an_origin_tag() {
        let (mut state, _rx) = app(&["tank", "cleric"]);
        with_channel(&mut state, "comms", false);

        apply_session_event(
            &mut state,
            1,
            SessionEvent::Route {
                channel: "comms".into(),
                text: "Bob tells you hi".into(),
            },
        );

        // The tag is provenance, not text: `ui::draw_channel` composes the
        // `[cleric]` from it (§8).
        assert_eq!(state.channels[0].lines[0].text, "Bob tells you hi");
        assert_eq!(
            state.channels[0].lines[0].origin,
            Origin::Session(vec!["cleric".to_string()])
        );
        assert_eq!(state.channels[0].unread, 1);
        assert!(
            state.sessions[1].scrollback.is_empty(),
            "a routed line reaches the pane only via Line, which the gag suppresses"
        );
    }

    /// One session means no ambiguity about where a line came from, so the
    /// tag would just be noise.
    #[test]
    fn a_single_session_channel_line_carries_no_origin_tag() {
        let (mut state, _rx) = app(&["tank"]);
        with_channel(&mut state, "comms", false);

        apply_session_event(
            &mut state,
            0,
            SessionEvent::Route {
                channel: "comms".into(),
                text: "Bob tells you hi".into(),
            },
        );

        assert_eq!(state.channels[0].lines[0].text, "Bob tells you hi");
        assert_eq!(state.channels[0].lines[0].origin, Origin::Server);
    }

    fn route(state: &mut AppState, from: usize, text: &str) {
        apply_session_event(
            state,
            from,
            SessionEvent::Route {
                channel: "comms".into(),
                text: text.into(),
            },
        );
    }

    /// One gossip heard by two characters is one message (#57). Both parsed
    /// it out of their own stream, so the pane is handed it twice — and
    /// saying the same sentence twice is exactly what the aggregating pane
    /// exists to avoid.
    #[test]
    fn one_broadcast_heard_by_two_characters_is_one_line() {
        let (mut state, _rx) = app(&["tank", "cleric"]);
        with_channel(&mut state, "comms", false);

        route(&mut state, 0, "Bob gossips hi");
        route(&mut state, 1, "Bob gossips hi");

        assert_eq!(state.channels[0].lines.len(), 1);
        assert_eq!(
            state.channels[0].lines[0].origin,
            Origin::Session(vec!["tank".to_string(), "cleric".to_string()]),
            "the entry names everyone who heard it, in arrival order"
        );
        assert_eq!(
            state.channels[0].unread, 1,
            "one message is one unread, however many characters heard it"
        );
    }

    /// Colour is the difference most likely to separate two copies of one
    /// broadcast — a MUD may tint a channel per character — so sameness is
    /// the plain projection, not the bytes.
    #[test]
    fn copies_that_differ_only_in_colour_still_collapse() {
        let (mut state, _rx) = app(&["tank", "cleric"]);
        with_channel(&mut state, "comms", false);

        route(&mut state, 0, "Bob gossips hi");
        route(&mut state, 1, "\x1b[36mBob gossips hi\x1b[0m");

        assert_eq!(state.channels[0].lines.len(), 1);
        assert_eq!(
            state.channels[0].lines[0].text, "Bob gossips hi",
            "the first copy to arrive is the one kept"
        );
    }

    /// Someone saying the same thing twice is two messages. The second copy
    /// reaches the same character, which is what tells it apart from a
    /// sibling's copy of the first.
    #[test]
    fn a_character_hearing_the_same_line_twice_keeps_both() {
        let (mut state, _rx) = app(&["tank", "cleric"]);
        with_channel(&mut state, "comms", false);

        route(&mut state, 0, "Bob gossips hi");
        route(&mut state, 0, "Bob gossips hi");

        assert_eq!(state.channels[0].lines.len(), 2);
        assert_eq!(state.channels[0].unread, 2);
    }

    /// Both characters hearing both of a repeated pair leaves two entries,
    /// each naming both — the collapse must pair the copies up, not fold
    /// all four into one.
    #[test]
    fn a_repeat_both_characters_heard_stays_two_entries() {
        let (mut state, _rx) = app(&["tank", "cleric"]);
        with_channel(&mut state, "comms", false);

        route(&mut state, 0, "Bob gossips hi");
        route(&mut state, 1, "Bob gossips hi");
        route(&mut state, 0, "Bob gossips hi");
        route(&mut state, 1, "Bob gossips hi");

        let both = Origin::Session(vec!["tank".to_string(), "cleric".to_string()]);
        assert_eq!(state.channels[0].lines.len(), 2);
        assert_eq!(state.channels[0].lines[0].origin, both);
        assert_eq!(state.channels[0].lines[1].origin, both);
    }

    /// The window is what stops an identical line said an hour later from
    /// being folded into a stale entry no one is looking at any more.
    #[test]
    fn an_old_entry_is_not_collapsed_into() {
        let (mut state, _rx) = app(&["tank", "cleric"]);
        with_channel(&mut state, "comms", false);

        route(&mut state, 0, "Bob gossips hi");
        state.channels[0].lines[0].at -= chrono::TimeDelta::hours(1);
        route(&mut state, 1, "Bob gossips hi");

        assert_eq!(state.channels[0].lines.len(), 2);
    }

    /// Only the aggregating case duplicates: one character parses one copy,
    /// so a repeat there is genuinely a repeat.
    #[test]
    fn a_single_session_never_collapses_its_own_repeats() {
        let (mut state, _rx) = app(&["tank"]);
        with_channel(&mut state, "comms", false);

        route(&mut state, 0, "Bob gossips hi");
        route(&mut state, 0, "Bob gossips hi");

        assert_eq!(state.channels[0].lines.len(), 2);
    }

    // ---- comms that survive a restart (§11.1) ----

    fn persisting(name: &str) -> Channel {
        Channel {
            persist: true,
            ..channel(name)
        }
    }

    /// A tell you got ten minutes before quitting is still there when you
    /// come back (#56), closed by a marker saying where it ends: the pane
    /// draws arrival order, not elapsed time, so without one a week-old
    /// tell sits above tonight's first line and reads as new.
    #[test]
    fn a_restored_pane_says_where_the_saved_lines_end() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let mut saved = VecDeque::new();
        saved.push_back(RetainedLine::server("Bob tells you hi"));
        config::save_comms(dir.path(), "comms", &saved).unwrap();

        let lines = restored_comms(dir.path(), &persisting("comms"), 10_000);

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "Bob tells you hi");
        assert_eq!(lines[1].origin, Origin::Client);
        assert!(
            lines[1].text.starts_with("** end of saved comms"),
            "{}",
            lines[1].text
        );
    }

    /// Nothing saved is not the same as something saved and empty: an
    /// unused channel opens as it always did, with no marker announcing
    /// history that isn't there.
    #[test]
    fn a_pane_with_nothing_saved_opens_empty() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        assert!(restored_comms(dir.path(), &persisting("comms"), 10_000).is_empty());
    }

    /// `persist: false` is how a channel carrying private conversation
    /// stays out of the config dir — including on the way back in, where a
    /// file written before the setting changed must not be read anyway.
    #[test]
    fn a_channel_that_does_not_persist_starts_empty() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let mut saved = VecDeque::new();
        saved.push_back(RetainedLine::server("Bob tells you hi"));
        config::save_comms(dir.path(), "comms", &saved).unwrap();

        assert!(restored_comms(dir.path(), &channel("comms"), 10_000).is_empty());
    }

    /// The file's bound and the pane's are set independently, so restoring
    /// must not hand a pane more lines than it is willing to hold.
    #[test]
    fn restoring_respects_the_panes_own_scrollback_bound() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let saved: VecDeque<RetainedLine> = (0..50)
            .map(|n| RetainedLine::server(format!("line {n}")))
            .collect();
        config::save_comms(dir.path(), "comms", &saved).unwrap();

        let lines = restored_comms(dir.path(), &persisting("comms"), 10);

        assert_eq!(lines.len(), 11, "ten lines and the marker");
        assert_eq!(lines[0].text, "line 40", "the newest ten, not the oldest");
    }

    /// The other end of the round trip: what the quit path writes is what
    /// the next launch reads, and a channel that opted out is not written
    /// at all.
    #[test]
    fn quitting_writes_the_persisting_panes_and_only_those() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let (mut state, _rx) = app(&["tank", "cleric"]);
        state.config_dir = dir.path().to_path_buf();
        with_channel(&mut state, "comms", false);
        with_channel(&mut state, "private", false);
        state.channels[0].config.persist = true;
        route(&mut state, 0, "Bob gossips hi");
        state.push_routed(1, "private", "Bob tells you a secret".to_string());

        save_comms_panes(&state);

        let comms = config::load_comms(dir.path(), "comms");
        assert_eq!(comms.len(), 1);
        assert_eq!(comms[0].text, "Bob gossips hi");
        assert!(
            config::load_comms(dir.path(), "private").is_empty(),
            "a pane that opted out leaves nothing behind"
        );
        assert!(
            !config::comms_path(dir.path(), "private").exists(),
            "not even an empty file"
        );
    }

    /// A line routed to a channel that isn't declared must not panic or
    /// vanish into a new pane the layout doesn't know about.
    #[test]
    fn a_route_to_an_unknown_channel_is_dropped() {
        let (mut state, _rx) = app(&["tank"]);
        apply_session_event(
            &mut state,
            0,
            SessionEvent::Route {
                channel: "nope".into(),
                text: "hi".into(),
            },
        );
        assert!(state.channels.is_empty());
    }

    // ---- cross-session echo_to (§7.5) ----

    /// A script's `mud.session("cleric"):echo(text)` reaches that pane and
    /// no other, tagged with the character it came from.
    #[test]
    fn echo_to_writes_only_the_named_session_pane() {
        let (mut state, _rx) = app(&["tank", "cleric", "mage"]);

        let (_, commands) = apply_session_event(
            &mut state,
            0,
            SessionEvent::EchoTo {
                target: "cleric".into(),
                text: "he is about to fall".into(),
            },
        );

        // Display-only: nothing is asked of the target session.
        assert!(commands.is_empty(), "{commands:?}");
        let line = state.sessions[1].scrollback.back().expect("the echo");
        assert_eq!(line.text, "[from tank] he is about to fall");
        assert_eq!(line.origin, Origin::Session(vec!["tank".to_string()]));
        assert!(state.sessions[0].scrollback.is_empty(), "not the sender");
        assert!(state.sessions[2].scrollback.is_empty(), "not a bystander");
    }

    #[test]
    fn echo_to_an_unknown_session_warns_in_the_origin_pane() {
        let (mut state, _rx) = app(&["tank"]);

        apply_session_event(
            &mut state,
            0,
            SessionEvent::EchoTo {
                target: "ghost".into(),
                text: "anyone there".into(),
            },
        );

        let notice = state.sessions[0].scrollback.back().expect("a notice");
        assert!(notice.text.contains("ghost"), "{}", notice.text);
        assert_eq!(notice.origin, Origin::Client);
    }

    // ---- cross-session send_to (§7.5) ----

    /// The M7 criterion's second half: a tank trigger fires a heal in the
    /// cleric session.
    #[test]
    fn send_to_reaches_the_named_session_only() {
        let (mut state, _rx) = app(&["tank", "cleric", "mage"]);

        let (_, injections) = apply_session_event(
            &mut state,
            0,
            SessionEvent::SendTo {
                target: "cleric".into(),
                lines: vec!["cast 'major heal' Grunk".into()],
                hops: 1,
            },
        );

        assert_eq!(injections.len(), 1);
        let (target, command) = &injections[0];
        assert_eq!(*target, 1);
        match command {
            SessionCommand::Inject { from, lines, hops } => {
                assert_eq!(from, "tank");
                assert_eq!(lines, &["cast 'major heal' Grunk".to_string()]);
                assert_eq!(*hops, 1);
            }
            other => panic!("expected Inject, got {other:?}"),
        }
    }

    /// `*` is "everyone else" — never a loop back to the sender.
    #[test]
    fn a_star_target_reaches_every_other_session() {
        let (mut state, _rx) = app(&["tank", "cleric", "mage"]);

        let (_, injections) = apply_session_event(
            &mut state,
            0,
            SessionEvent::SendTo {
                target: ALL_SESSIONS.into(),
                lines: vec!["follow tank".into()],
                hops: 1,
            },
        );

        let targets: Vec<usize> = injections.iter().map(|(index, _)| *index).collect();
        assert_eq!(targets, vec![1, 2]);
    }

    /// If the target isn't connected the action is dropped with a warning in
    /// the *originating* pane (§7.5).
    #[test]
    fn a_disconnected_target_warns_in_the_origin_pane() {
        let (mut state, _rx) = app(&["tank", "cleric"]);
        state.sessions[1].connected = false;

        let (_, injections) = apply_session_event(
            &mut state,
            0,
            SessionEvent::SendTo {
                target: "cleric".into(),
                lines: vec!["heal me".into()],
                hops: 1,
            },
        );

        assert!(injections.is_empty());
        assert!(
            scrollback(&state.sessions[0]).contains("not connected"),
            "{:?}",
            scrollback(&state.sessions[0])
        );
        assert!(state.sessions[1].scrollback.is_empty());
    }

    #[test]
    fn an_unknown_target_warns_in_the_origin_pane() {
        let (mut state, _rx) = app(&["tank"]);

        let (_, injections) = apply_session_event(
            &mut state,
            0,
            SessionEvent::SendTo {
                target: "cleric".into(),
                lines: vec!["heal me".into()],
                hops: 1,
            },
        );

        assert!(injections.is_empty());
        assert!(scrollback(&state.sessions[0]).contains("no session named `cleric`"));
    }

    /// Loop safety: the hop count is checked against the *receiver's* limit,
    /// so two sessions' rules cannot ping-pong forever (§7.5).
    #[test]
    fn an_exhausted_hop_count_is_dropped_with_a_warning() {
        let (mut state, _rx) = app(&["tank", "cleric"]);
        assert_eq!(state.sessions[1].cross.max_hops, 1);

        let (_, injections) = apply_session_event(
            &mut state,
            0,
            SessionEvent::SendTo {
                target: "cleric".into(),
                lines: vec!["heal me".into()],
                hops: 2,
            },
        );

        assert!(injections.is_empty());
        assert!(
            scrollback(&state.sessions[0]).contains("hop limit"),
            "{:?}",
            scrollback(&state.sessions[0])
        );
    }

    /// A rule addressing its own session would be a trivial infinite loop.
    #[test]
    fn a_session_cannot_send_to_itself() {
        let (mut state, _rx) = app(&["tank"]);

        let (_, injections) = apply_session_event(
            &mut state,
            0,
            SessionEvent::SendTo {
                target: "tank".into(),
                lines: vec!["look".into()],
                hops: 1,
            },
        );

        assert!(injections.is_empty());
        assert!(scrollback(&state.sessions[0]).contains("no session named"));
    }

    // ---- /send (§7.5, issue #68) ----

    /// Helper: type a line and press Enter, as the bound session.
    async fn enter_line(state: &mut AppState, line: &str) {
        state.sessions[state.input_session].input = Input::default().with_value(line.to_string());
        submit_input(state, &[]).await;
    }

    /// The whole point: a command runs as another character, from the pane the
    /// player is already in, without switching to theirs.
    #[tokio::test]
    async fn send_runs_a_command_as_the_named_character() {
        let (mut state, mut receivers) = app(&["tank", "cleric"]);

        enter_line(&mut state, "/send cleric drink well").await;

        match receivers[1].try_recv().expect("the cleric is asked") {
            SessionCommand::Inject { from, lines, hops } => {
                assert_eq!(from, "tank");
                assert_eq!(lines, &["drink well".to_string()]);
                assert_eq!(hops, crate::session::FIRST_HOP);
            }
            other => panic!("expected Inject, got {other:?}"),
        }
        // Nothing goes to the MUD this character is talking to.
        assert!(receivers[0].try_recv().is_err());
        // And the player can see what they typed.
        assert!(
            scrollback(&state.sessions[0]).contains("> /send cleric drink well"),
            "{:?}",
            scrollback(&state.sessions[0])
        );
    }

    /// `*` is the TinTin `#all` idiom players arrive looking for, and a
    /// semicolon means what it means everywhere else in the client — so
    /// `wake;stand` is two commands at each of them, not one nonsense one.
    #[tokio::test]
    async fn send_to_a_star_reaches_everyone_else_and_splits_on_semicolons() {
        let (mut state, mut receivers) = app(&["tank", "cleric", "mage"]);

        enter_line(&mut state, "/send * wake;stand").await;

        for index in [1, 2] {
            match receivers[index].try_recv().expect("delivered") {
                SessionCommand::Inject { lines, .. } => {
                    assert_eq!(lines, &["wake".to_string(), "stand".to_string()]);
                }
                other => panic!("expected Inject, got {other:?}"),
            }
        }
        assert!(receivers[0].try_recv().is_err(), "never back to the sender");
    }

    /// A name that is not there has to say so. Silence would leave the player
    /// believing the drink happened.
    #[tokio::test]
    async fn send_to_an_unknown_character_says_so() {
        let (mut state, _rx) = app(&["tank"]);

        enter_line(&mut state, "/send athos drink well").await;

        let notice = scrollback(&state.sessions[0]);
        assert!(notice.contains("no session named `athos`"), "{notice}");
        // Reported as the command that was typed, not as the YAML field that
        // shares its plumbing.
        assert!(!notice.contains("send_to"), "{notice}");
    }

    /// Aiming it at yourself is a mistake worth naming, not a delivery
    /// failure — `addressed` excludes the sender, so the untreated case
    /// reports "no session named" about a character on screen.
    #[tokio::test]
    async fn send_to_yourself_says_what_is_wrong_with_it() {
        let (mut state, mut receivers) = app(&["tank"]);

        enter_line(&mut state, "/send tank look").await;

        let notice = scrollback(&state.sessions[0]);
        assert!(
            notice.contains("the character you are typing as"),
            "{notice}"
        );
        assert!(!notice.contains("no session named"), "{notice}");
        assert!(receivers[0].try_recv().is_err(), "and it is not sent");
    }

    /// A disconnected target already reports through the shared path; what
    /// matters here is that it is reported as `/send`.
    #[tokio::test]
    async fn send_to_a_disconnected_character_says_so_as_send() {
        let (mut state, _rx) = app(&["tank", "cleric"]);
        state.sessions[1].connected = false;

        enter_line(&mut state, "/send cleric drink well").await;

        let notice = scrollback(&state.sessions[0]);
        assert!(notice.contains("/send `cleric`"), "{notice}");
        assert!(notice.contains("not connected"), "{notice}");
    }

    /// A name with no command, or no name at all, must not reach the MUD as
    /// literal text — the player would see the server reject `/send` and have
    /// no idea the client owns that word.
    #[tokio::test]
    async fn send_without_a_command_explains_itself() {
        for line in ["/send", "/send cleric", "/send cleric   "] {
            let (mut state, mut receivers) = app(&["tank", "cleric"]);

            enter_line(&mut state, line).await;

            let notice = scrollback(&state.sessions[0]);
            assert!(notice.contains("/send needs"), "{line}: {notice}");
            assert!(receivers[0].try_recv().is_err(), "{line} reached the MUD");
            assert!(
                receivers[1].try_recv().is_err(),
                "{line} reached the cleric"
            );
        }
    }

    // ---- input submission ----

    /// Pressing Enter on an empty box must still send: MUD login flows ask
    /// for a bare return. It is not echoed, though — a lone `>` line would
    /// be noise in the scrollback.
    #[tokio::test]
    async fn a_bare_enter_is_sent_but_not_echoed() {
        let (mut state, mut receivers) = app(&["tank"]);

        submit_input(&mut state, &[]).await;

        match receivers[0].try_recv().expect("a bare Enter is dispatched") {
            SessionCommand::SendLine(line) => assert_eq!(line, ""),
            other => panic!("expected SendLine, got {other:?}"),
        }
        assert!(
            state.sessions[0].scrollback.is_empty(),
            "a bare Enter must not leave a stray prompt line: {:?}",
            state.sessions[0].scrollback
        );
    }

    #[tokio::test]
    async fn a_typed_line_is_still_echoed() {
        let (mut state, _rx) = app(&["tank"]);
        state.sessions[0].input = Input::default().with_value("look".into());

        submit_input(&mut state, &[]).await;

        assert_eq!(scrollback(&state.sessions[0]), "> look");
        assert_eq!(state.sessions[0].input.value(), "", "input is cleared");
    }

    // ---- help (docs/ARCHITECTURE.md §11.2) ----

    #[test]
    fn the_help_key_opens_the_overlay_and_any_key_closes_it() {
        let (mut state, _rx) = app(&["tank"]);

        assert!(press(&mut state, KeyCode::F(1), KeyModifiers::NONE));
        assert!(state.show_help);

        assert!(press(&mut state, KeyCode::Esc, KeyModifiers::NONE));
        assert!(!state.show_help);

        press(&mut state, KeyCode::F(1), KeyModifiers::NONE);
        // A key with no other meaning still dismisses, rather than being
        // typed into an input line hidden behind the overlay.
        assert!(press(&mut state, KeyCode::Char('k'), KeyModifiers::NONE));
        assert!(!state.show_help);
    }

    /// The overlay must never become a second, stale copy of the bindings
    /// (§11.2) — the listing is generated from the ones in force.
    #[test]
    fn help_shows_remapped_keys_not_the_defaults() {
        let keybinds = Keybinds {
            toggle_channels: "alt+t".parse().unwrap(),
            ..Keybinds::default()
        };
        let listing = ui::help_lines(&keybinds).join("\n");

        assert!(listing.contains("Alt+T"), "{listing}");
        assert!(
            !listing.contains("F4"),
            "the default it replaced must be gone: {listing}"
        );
    }

    #[test]
    fn help_lists_every_configurable_binding() {
        let keybinds = Keybinds::default();
        let listing = ui::help_lines(&keybinds).join("\n");

        for binding in [
            &keybinds.quit,
            &keybinds.server_data_inspector,
            &keybinds.focus_next,
            &keybinds.cycle_layout,
            &keybinds.toggle_channels,
            &keybinds.channel_wider,
            &keybinds.channel_narrower,
            &keybinds.help,
        ] {
            assert!(
                listing.contains(&binding.to_string()),
                "{binding} missing from:\n{listing}"
            );
        }
        // The built-ins and client commands are just as invisible otherwise.
        for text in [
            "Alt+1",
            "Up / Down",
            "PgUp / PgDn",
            "Home / End",
            "/reload",
            "/help",
        ] {
            assert!(listing.contains(text), "{text} missing from:\n{listing}");
        }
    }

    #[tokio::test]
    async fn the_help_command_prints_the_same_listing_into_the_pane() {
        let (mut state, mut receivers) = app(&["tank"]);
        state.sessions[0].input = Input::default().with_value("/help".into());

        submit_input(&mut state, &[]).await;

        let printed = scrollback(&state.sessions[0]);
        for line in ui::help_lines(&state.keybinds) {
            assert!(printed.contains(&line), "{line:?} missing from:\n{printed}");
        }
        assert!(
            receivers[0].try_recv().is_err(),
            "/help is a client command and must not reach the server"
        );
    }

    // ---- command history (docs/ARCHITECTURE.md §11.3) ----

    /// Submits `line` as if typed into the bound session.
    async fn submit(state: &mut AppState, line: &str) {
        if let Some(session) = state.bound_mut() {
            session.input = Input::default().with_value(line.into());
        }
        submit_input(state, &[]).await;
    }

    #[tokio::test]
    async fn up_recalls_the_last_command_and_down_returns_to_the_draft() {
        let (mut state, _rx) = app(&["tank"]);
        submit(&mut state, "kill rat").await;
        submit(&mut state, "look").await;

        let session = &mut state.sessions[0];
        session.input = Input::default().with_value("half typ".into());

        assert!(session.walk_history(true));
        assert_eq!(session.input.value(), "look");
        assert!(session.walk_history(true));
        assert_eq!(session.input.value(), "kill rat");
        // Nothing older to reach; the oldest entry stays put.
        assert!(!session.walk_history(true));
        assert_eq!(session.input.value(), "kill rat");

        assert!(session.walk_history(false));
        assert_eq!(session.input.value(), "look");
        assert!(session.walk_history(false));
        assert_eq!(
            session.input.value(),
            "half typ",
            "walking back past the newest entry restores what was being typed"
        );
    }

    #[tokio::test]
    async fn editing_a_recalled_line_leaves_the_stored_one_alone() {
        let (mut state, _rx) = app(&["tank"]);
        submit(&mut state, "kill rat").await;

        let session = &mut state.sessions[0];
        session.walk_history(true);
        session.input = Input::default().with_value("kill wolf".into());
        submit(&mut state, "kill wolf").await;

        let session = &mut state.sessions[0];
        assert!(session.walk_history(true));
        assert_eq!(session.input.value(), "kill wolf");
        assert!(session.walk_history(true));
        assert_eq!(
            session.input.value(),
            "kill rat",
            "the edited recall was appended, not written over its source"
        );
    }

    #[tokio::test]
    async fn consecutive_duplicates_collapse_but_earlier_repeats_stay() {
        let (mut state, _rx) = app(&["tank"]);
        for line in ["look", "look", "north", "look"] {
            submit(&mut state, line).await;
        }

        let session = &mut state.sessions[0];
        assert_eq!(
            session.history.iter().collect::<Vec<_>>(),
            ["look", "north", "look"]
        );
    }

    #[tokio::test]
    async fn a_bare_enter_is_not_worth_recalling() {
        let (mut state, _rx) = app(&["tank"]);
        submit(&mut state, "look").await;
        submit(&mut state, "").await;

        let session = &mut state.sessions[0];
        assert!(session.walk_history(true));
        assert_eq!(session.input.value(), "look");
    }

    /// A password typed under server ECHO stays out of history for the same
    /// reason it stays out of scrollback (§13).
    #[tokio::test]
    async fn a_masked_line_is_never_recorded_or_recalled() {
        let (mut state, _rx) = app(&["tank"]);
        submit(&mut state, "look").await;

        state.sessions[0].masked = true;
        submit(&mut state, "hunter2").await;

        let session = &mut state.sessions[0];
        assert!(
            !session.history.contains(&"hunter2".to_string()),
            "history: {:?}",
            session.history
        );
        assert!(
            !session.walk_history(true),
            "recall into a masked prompt would send an old command as the password"
        );
        assert_eq!(session.input.value(), "");

        // Unmasking restores ordinary recall, minus the password.
        state.sessions[0].masked = false;
        let session = &mut state.sessions[0];
        assert!(session.walk_history(true));
        assert_eq!(session.input.value(), "look");
    }

    // ---- offering to keep a typed password (docs/ARCHITECTURE.md §13) ----

    /// A session for a profile that wants auto-login but has nothing stored,
    /// sitting at a masked prompt.
    fn armed(dir: &std::path::Path) -> (AppState, Vec<mpsc::Receiver<SessionCommand>>) {
        let (mut state, receivers) = app(&["kestrel"]);
        let session = &mut state.sessions[0];
        session.rules = (dir.to_path_buf(), Some("kestrel".to_string()));
        session.offer_password_save = true;
        session.masked = true;
        (state, receivers)
    }

    #[tokio::test]
    async fn a_masked_login_password_is_offered_to_the_keyring() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let (mut state, mut receivers) = armed(dir.path());

        submit(&mut state, "hunter2").await;

        assert!(
            scrollback(&state.sessions[0]).contains("(y/n)"),
            "{:?}",
            scrollback(&state.sessions[0])
        );
        assert!(
            matches!(receivers[0].try_recv(), Ok(SessionCommand::SendLine(line)) if line == "hunter2"),
            "the password must still reach the server; the offer is only an offer"
        );

        // Once per session: a second masked line is not another question.
        state.sessions[0].pending_password = None;
        state.sessions[0].scrollback.clear();
        submit(&mut state, "again").await;
        assert!(state.sessions[0].pending_password.is_none());
    }

    /// Only the masked prompt is a password. Ordinary commands must never be
    /// offered to the keyring.
    #[tokio::test]
    async fn an_unmasked_line_is_never_taken_for_a_password() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let (mut state, _rx) = armed(dir.path());
        state.sessions[0].masked = false;

        submit(&mut state, "look").await;

        assert!(state.sessions[0].pending_password.is_none());
        assert!(state.sessions[0].offer_password_save);
    }

    /// "No" is remembered, so the offer costs one question per profile
    /// rather than one per login.
    #[tokio::test]
    async fn refusing_is_recorded_against_the_profile() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let (mut state, _rx) = armed(dir.path());
        submit(&mut state, "hunter2").await;

        assert!(press(&mut state, KeyCode::Char('n'), KeyModifiers::NONE));

        assert!(state.sessions[0].pending_password.is_none());
        assert!(crate::config::password_save_declined(dir.path(), "kestrel"));
    }

    /// The server hiding input again means it re-prompted, which means the
    /// password it was just given was wrong. Saving that would break the
    /// next login rather than automate it.
    #[tokio::test]
    async fn a_rejected_password_is_never_offered_to_the_keyring() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let (mut state, _rx) = armed(dir.path());
        submit(&mut state, "wrong").await;

        // The password prompt ends, then the MUD asks for one again.
        apply_session_event(&mut state, 0, SessionEvent::EchoMask(false));
        apply_session_event(&mut state, 0, SessionEvent::EchoMask(true));

        assert!(state.sessions[0].pending_password.is_none());
        assert!(
            scrollback(&state.sessions[0]).contains("rejected"),
            "{:?}",
            scrollback(&state.sessions[0])
        );
        // A withdrawn offer is not a refusal: nothing was answered.
        assert!(!crate::config::password_save_declined(
            dir.path(),
            "kestrel"
        ));
    }

    /// The offer only exists to fill an empty keyring. A profile whose
    /// password was already stored with `--set-password` is never armed,
    /// so nothing asks about the password it types at a masked prompt.
    #[tokio::test]
    async fn a_profile_with_a_stored_password_is_never_asked() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let (mut state, _rx) = armed(dir.path());
        // What `autologin` produces when the keyring already has one.
        state.sessions[0].offer_password_save = false;

        submit(&mut state, "hunter2").await;

        assert!(state.sessions[0].pending_password.is_none());
        assert!(state.sessions[0].scrollback.is_empty());
    }

    /// A stray keystroke must not answer the question, and must not be read
    /// as a refusal either — it just drops the held password.
    #[tokio::test]
    async fn a_stray_key_drops_the_offer_without_recording_a_refusal() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let (mut state, _rx) = armed(dir.path());
        submit(&mut state, "hunter2").await;

        assert!(
            !press(&mut state, KeyCode::Char('k'), KeyModifiers::NONE),
            "an unrelated key still belongs to the input line"
        );

        assert!(state.sessions[0].pending_password.is_none());
        assert!(!crate::config::password_save_declined(
            dir.path(),
            "kestrel"
        ));
    }

    #[tokio::test]
    async fn history_is_per_session() {
        let (mut state, _rx) = app(&["tank", "cleric"]);
        submit(&mut state, "kill rat").await;

        state.input_session = 1;
        state.focus_pane(Focus::Session(1));
        let cleric = &mut state.sessions[1];

        assert!(
            !cleric.walk_history(true),
            "the tank's commands must not be recallable in the cleric's input"
        );
        assert_eq!(cleric.input.value(), "");
    }

    #[tokio::test]
    async fn history_is_bounded_and_discards_oldest_first() {
        let (mut state, _rx) = app(&["tank"]);
        state.sessions[0].history_limit = 2;
        for line in ["one", "two", "three"] {
            submit(&mut state, line).await;
        }

        assert_eq!(
            state.sessions[0].history.iter().collect::<Vec<_>>(),
            ["two", "three"]
        );
    }

    #[tokio::test]
    async fn submitting_restarts_the_walk_from_the_newest_entry() {
        let (mut state, _rx) = app(&["tank"]);
        submit(&mut state, "one").await;
        submit(&mut state, "two").await;

        state.sessions[0].walk_history(true);
        state.sessions[0].walk_history(true);
        assert_eq!(state.sessions[0].input.value(), "one");
        submit(&mut state, "one").await;

        let session = &mut state.sessions[0];
        assert!(session.walk_history(true));
        assert_eq!(
            session.input.value(),
            "one",
            "a fresh walk starts at the newest entry, not where the last one stopped"
        );
    }

    // ---- keys (docs/ARCHITECTURE.md §11) ----

    fn press(state: &mut AppState, code: KeyCode, modifiers: KeyModifiers) -> bool {
        // Wide enough that the channel-width clamp never binds by accident;
        // the tests that care about the ceiling pass their own width.
        handle_key(state, &Keybinds::default(), code, modifiers, 120, &[])
    }

    // ---- completing from what the MUD said (§11.3) ----

    fn typed(session: &mut SessionPane, text: &str) {
        for c in text.chars() {
            type_into_input(
                session,
                crossterm::event::KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            );
        }
    }

    fn heard(session: &mut SessionPane, line: &str) {
        session.learn_words(line, &Origin::Server);
    }

    /// The ask: with a bullywug in the room, typing `look bull` sends
    /// `look bullywug` — no completion key, the guess is simply part of
    /// the line by the time Enter is pressed.
    #[test]
    fn typing_a_prefix_completes_from_what_the_mud_just_said() {
        let (mut session, _rx) = test_support::pane("tank");
        heard(&mut session, "A bullywug is here.");

        typed(&mut session, "look bull");

        assert_eq!(session.suggestion.as_deref(), Some("ywug"));
        assert_eq!(session.completed_input(), "look bullywug");
    }

    /// A word arriving mid-keystroke is the case worth catching: you start
    /// typing at what you can see, and the room finishes loading.
    #[test]
    fn a_name_that_arrives_while_you_type_completes_what_is_already_there() {
        let (mut session, _rx) = test_support::pane("tank");
        typed(&mut session, "look bull");
        assert_eq!(session.suggestion, None);

        heard(&mut session, "A bullywug is here.");

        assert_eq!(session.suggestion.as_deref(), Some("ywug"));
    }

    /// Tab takes the guess and puts it in the line for real, with the cursor
    /// after it — so the next thing typed continues the word rather than
    /// landing in the middle of it.
    #[test]
    fn tab_accepts_the_guess_and_leaves_the_cursor_at_the_end() {
        let (mut session, _rx) = test_support::pane("tank");
        heard(&mut session, "A bullywug is here.");
        typed(&mut session, "look bull");

        press_tab(&mut session);

        assert_eq!(session.input.value(), "look bullywug");
        assert_eq!(
            session.input.cursor(),
            "look bullywug".chars().count(),
            "the cursor belongs after what was just accepted"
        );
        // The ghost has become the line, so there is nothing left to draw
        // dimmed after it.
        assert_eq!(session.suggestion, None);
        assert_eq!(session.completed_input(), "look bullywug");
    }

    /// Typing on after Tab extends the accepted word instead of overwriting
    /// it, which is the whole point of where the cursor ends up.
    #[test]
    fn typing_after_tab_continues_the_accepted_word() {
        let (mut session, _rx) = test_support::pane("tank");
        heard(&mut session, "A bullywug is here.");
        typed(&mut session, "look bull");
        press_tab(&mut session);

        typed(&mut session, "s");

        assert_eq!(session.input.value(), "look bullywugs");
    }

    /// With nothing to accept, Tab does what it did before this existed:
    /// nothing. Never a literal tab character in a line bound for the MUD.
    #[test]
    fn tab_with_no_guess_leaves_the_line_alone() {
        let (mut session, _rx) = test_support::pane("tank");
        typed(&mut session, "look bull");
        assert_eq!(session.suggestion, None, "nothing heard, nothing to guess");

        press_tab(&mut session);

        assert_eq!(session.input.value(), "look bull");
    }

    /// A dismissed guess stays dismissed. Escape said "send what I typed",
    /// and Tab must not resurrect what that ruled out.
    #[test]
    fn tab_after_escape_has_nothing_to_accept() {
        let (mut session, _rx) = test_support::pane("tank");
        heard(&mut session, "A bullywug is here.");
        typed(&mut session, "look bull");
        type_into_input(
            &mut session,
            crossterm::event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        );

        press_tab(&mut session);

        assert_eq!(session.input.value(), "look bull");
    }

    fn press_tab(session: &mut SessionPane) {
        type_into_input(
            session,
            crossterm::event::KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
        );
    }

    /// Escape is the way out for one line: what you typed is what is sent.
    #[test]
    fn escape_dismisses_the_guess_for_that_line() {
        let (mut session, _rx) = test_support::pane("tank");
        heard(&mut session, "A bullywug is here.");
        typed(&mut session, "look bull");

        type_into_input(
            &mut session,
            crossterm::event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        );

        assert_eq!(session.suggestion, None);
        assert_eq!(session.completed_input(), "look bull");
    }

    /// ...and only for that line. Typing on changes the question, so the
    /// dismissal does not silently outlive what it was about.
    #[test]
    fn typing_after_a_dismissal_asks_again() {
        let (mut session, _rx) = test_support::pane("tank");
        heard(&mut session, "A bullywug is here.");
        typed(&mut session, "look bull");
        type_into_input(
            &mut session,
            crossterm::event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        );

        typed(&mut session, "y");

        assert_eq!(session.suggestion.as_deref(), Some("wug"));
    }

    /// Never into a password. The server is hiding this line and the
    /// scrollback never saw it; a ghost past the asterisks would be
    /// guessing at it out loud (§13).
    #[test]
    fn a_masked_line_is_never_completed() {
        let (mut session, _rx) = test_support::pane("tank");
        heard(&mut session, "A bullywug is here.");
        session.masked = true;

        typed(&mut session, "bull");

        assert_eq!(session.suggestion, None);
    }

    /// Only what the MUD said. Our own echo and the client's notices are
    /// not names the server knows.
    #[test]
    fn only_the_servers_own_words_are_learned() {
        let (mut session, _rx) = test_support::pane("tank");
        session.learn_words("** the bullywug warning", &Origin::Client);
        session.learn_words("> look bullywug", &Origin::Echo);

        typed(&mut session, "bull");

        assert_eq!(session.suggestion, None);
    }

    /// Colour is not part of a name: a MUD that tints the mob's name must
    /// not teach an escape sequence as a word.
    #[test]
    fn colour_is_stripped_before_words_are_learned() {
        let (mut session, _rx) = test_support::pane("tank");
        heard(&mut session, "A \x1b[1;32mbullywug\x1b[0m is here.");

        typed(&mut session, "bull");

        assert_eq!(session.suggestion.as_deref(), Some("ywug"));
    }

    /// Completing a word in the middle of a line would have to decide what
    /// happens to the text after it, and no answer to that is obvious
    /// enough to do silently.
    #[test]
    fn nothing_is_completed_away_from_the_end_of_the_line() {
        let (mut session, _rx) = test_support::pane("tank");
        heard(&mut session, "A bullywug is here.");
        typed(&mut session, "look bull");

        type_into_input(
            &mut session,
            crossterm::event::KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
        );

        assert_eq!(session.suggestion, None);
    }

    /// `autocomplete: false` makes the whole path inert — nothing learned,
    /// nothing guessed.
    #[test]
    fn the_switch_turns_it_off() {
        let (mut session, _rx) = test_support::pane("tank");
        session.autocomplete = false;
        heard(&mut session, "A bullywug is here.");

        typed(&mut session, "look bull");

        assert_eq!(session.suggestion, None);
        assert_eq!(session.completed_input(), "look bull");
    }

    /// The end of the bargain: what is on screen is what gets sent, and
    /// what is remembered is what was sent — recalling the command must
    /// give back the one the MUD was asked, not the half of it that was
    /// typed.
    #[tokio::test]
    async fn enter_sends_the_line_the_ghost_completed() {
        let (mut state, _rx) = app(&["tank"]);
        heard(&mut state.sessions[0], "A bullywug is here.");
        typed(&mut state.sessions[0], "look bull");

        submit_input(&mut state, &[]).await;

        let echoed = &state.sessions[0].scrollback[0];
        assert_eq!(echoed.text, "> look bullywug");
        assert_eq!(state.sessions[0].suggestion, None, "and it is spent");
        state.sessions[0].walk_history(true);
        assert_eq!(state.sessions[0].input.value(), "look bullywug");
    }

    // ---- who needs me? (docs/ARCHITECTURE.md §11.7) ----

    /// Publishes vitals for a session the way its own task would, so the
    /// alarm reads them the way it will in play.
    fn publish_vitals(state: &mut AppState, name: &str, pairs: &[(&str, &str)]) {
        let (tx, rx) = tokio::sync::watch::channel(crate::engine::PeerSnapshot {
            vars: std::collections::HashMap::new(),
            data: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        });
        // Dropped on purpose: a `watch::Receiver` still reads the last
        // value published after its sender is gone, which is what the
        // client does between updates.
        drop(tx);
        state.peer_registry.insert(name.to_string(), rx);
    }

    /// The bell is for the moment trouble starts. Ringing it again on every
    /// pass of the event loop while a character sits at low health would
    /// make the sound mean "someone is hurt", which the player already
    /// knows, instead of "someone just got hurt".
    #[test]
    fn a_character_dropping_into_trouble_is_announced_once() {
        let (mut state, _rx) = app(&["tank", "cleric"]);
        publish_vitals(
            &mut state,
            "cleric",
            &[("HEALTH", "9"), ("HEALTH_MAX", "100")],
        );

        assert_eq!(state.refresh_distress(), vec!["cleric".to_string()]);
        assert_eq!(state.refresh_distress(), Vec::<String>::new());

        publish_vitals(
            &mut state,
            "cleric",
            &[("HEALTH", "90"), ("HEALTH_MAX", "100")],
        );
        assert_eq!(state.refresh_distress(), Vec::<String>::new());
        assert_eq!(state.sessions[1].distress, None, "healed, and said so");

        publish_vitals(
            &mut state,
            "cleric",
            &[("HEALTH", "9"), ("HEALTH_MAX", "100")],
        );
        assert_eq!(
            state.refresh_distress(),
            vec!["cleric".to_string()],
            "hurt again is news again"
        );
    }

    /// The pane the player is reading needs no bell to point at itself —
    /// but it is still marked, because the mark is a fact about the
    /// character rather than about what has been looked at.
    #[test]
    fn the_character_being_watched_is_marked_but_not_rung_for() {
        let (mut state, _rx) = app(&["tank"]);
        publish_vitals(
            &mut state,
            "tank",
            &[("HEALTH", "9"), ("HEALTH_MAX", "100")],
        );

        assert_eq!(state.refresh_distress(), Vec::<String>::new());
        assert!(state.sessions[0].distress.is_some());
    }

    #[test]
    fn the_key_jumps_to_whoever_is_worst() {
        let (mut state, _rx) = app(&["tank", "cleric", "mage"]);
        publish_vitals(
            &mut state,
            "cleric",
            &[("HEALTH", "20"), ("HEALTH_MAX", "100")],
        );
        publish_vitals(
            &mut state,
            "mage",
            &[("HEALTH", "5"), ("HEALTH_MAX", "100")],
        );
        state.refresh_distress();

        assert!(press(&mut state, KeyCode::F(10), KeyModifiers::NONE));

        assert_eq!(state.input_session, 2, "the mage is closer to dying");
        assert_eq!(state.focus, Focus::Session(2));
    }

    /// A key that silently does nothing is a key the player assumes is
    /// broken.
    #[test]
    fn the_key_says_so_when_nobody_needs_you() {
        let (mut state, _rx) = app(&["tank", "cleric"]);
        publish_vitals(
            &mut state,
            "cleric",
            &[("HEALTH", "90"), ("HEALTH_MAX", "100")],
        );
        state.refresh_distress();

        assert!(press(&mut state, KeyCode::F(10), KeyModifiers::NONE));

        assert_eq!(state.input_session, 0, "nobody to jump to");
        let said = state.sessions[0]
            .scrollback
            .iter()
            .any(|line| line.text.contains("nobody is in trouble"));
        assert!(said, "{:?}", state.sessions[0].scrollback);
    }

    #[test]
    fn alt_n_jumps_straight_to_a_session() {
        let (mut state, _rx) = app(&["tank", "cleric", "mage"]);

        assert!(press(&mut state, KeyCode::Char('3'), KeyModifiers::ALT));
        assert_eq!(state.focus, Focus::Session(2));

        assert!(press(&mut state, KeyCode::Char('1'), KeyModifiers::ALT));
        assert_eq!(state.focus, Focus::Session(0));
    }

    /// Alt+N past the last session must do nothing rather than focus a pane
    /// that isn't there.
    #[test]
    fn alt_n_beyond_the_open_sessions_is_ignored() {
        let (mut state, _rx) = app(&["tank"]);
        assert!(!press(&mut state, KeyCode::Char('9'), KeyModifiers::ALT));
        assert_eq!(state.focus, Focus::Session(0));
    }

    #[test]
    fn the_layout_key_switches_between_tabs_and_splits() {
        let (mut state, _rx) = app(&["tank", "cleric"]);

        assert!(press(&mut state, KeyCode::F(3), KeyModifiers::NONE));
        assert_eq!(state.layout, LayoutMode::Splits);
        assert!(press(&mut state, KeyCode::F(3), KeyModifiers::NONE));
        assert_eq!(state.layout, LayoutMode::Tabs);
    }

    /// The width keys move `AppState`'s number and report `true`, so the
    /// caller re-reports NAWS: the sessions beside the column are told their
    /// new width (§6.2, §11.4).
    #[test]
    fn the_channel_width_keys_resize_the_column_and_ask_for_a_size_report() {
        let (mut state, _rx) = app(&["tank"]);
        let start = state.channel_width;

        assert!(press(&mut state, KeyCode::Char('-'), KeyModifiers::ALT));
        assert_eq!(state.channel_width, start + CHANNEL_WIDTH_STEP);
        assert!(press(&mut state, KeyCode::Char('='), KeyModifiers::ALT));
        assert_eq!(state.channel_width, start);
    }

    /// Narrowing stops at the floor rather than shrinking the column to
    /// nothing, and widening stops before the session area goes under
    /// `MIN_MAIN_WIDTH` — hiding channels is the toggle key's job (§11.4).
    #[test]
    fn the_channel_width_keys_stop_at_both_limits() {
        let (mut state, _rx) = app(&["tank"]);
        let keybinds = Keybinds::default();

        state.channel_width = ui::MIN_CHANNEL_WIDTH;
        assert!(handle_key(
            &mut state,
            &keybinds,
            KeyCode::Char('='),
            KeyModifiers::ALT,
            120,
            &[],
        ));
        assert_eq!(state.channel_width, ui::MIN_CHANNEL_WIDTH);

        // 60 columns wide leaves 30 for the column once the main area keeps
        // its minimum; a press at 30 must not take a 31st.
        state.channel_width = 30;
        assert!(handle_key(
            &mut state,
            &keybinds,
            KeyCode::Char('-'),
            KeyModifiers::ALT,
            60,
            &[],
        ));
        assert_eq!(state.channel_width, 30);
    }

    /// Hiding the channel column while a channel pane has focus must move
    /// focus back to a pane that is still drawn.
    #[test]
    fn hiding_the_channels_takes_focus_off_them() {
        let (mut state, _rx) = app(&["tank"]);
        state.channels.push(ChannelPane {
            config: channel("comms"),
            lines: VecDeque::new(),
            unread: 0,
            scrollback_limit: 10_000,
            back_offset: 0,
        });
        state.show_channels = true;
        state.focus_pane(Focus::Channel(0));

        assert!(press(&mut state, KeyCode::F(4), KeyModifiers::NONE));
        assert!(!state.show_channels);
        assert_eq!(state.focus, Focus::Session(0));
    }

    /// The default binding (F6, UX_REVIEW.md F) sets the deferred flag
    /// `event_loop` drains — `handle_key` is sync and `reload_rules` needs
    /// async IO, the same reason `config_editor_save` is deferred rather
    /// than run inline.
    #[test]
    fn the_reload_keybind_requests_a_reload() {
        let (mut state, _rx) = app(&["tank"]);

        assert!(press(&mut state, KeyCode::F(6), KeyModifiers::NONE));
        assert!(state.reload_requested);
    }

    // ---- /reload (docs/ARCHITECTURE.md §7.3) ----

    /// Writes a config dir with one module the profile pulls in.
    fn config_with_alias(send: &str) -> crate::net::pins::tests::tempdir::TempDir {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        std::fs::create_dir_all(dir.path().join("profiles")).unwrap();
        std::fs::create_dir_all(dir.path().join("modules")).unwrap();
        std::fs::write(
            dir.path().join("profiles/tank.yaml"),
            "name: tank\nhost: h\nport: 1\nmodules: [combat]\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("modules/combat.yaml"),
            format!("name: combat\naliases:\n  - regex: '^x$'\n    send: [\"{send}\"]\n"),
        )
        .unwrap();
        dir
    }

    /// Points the app's single session at `dir`'s `tank` profile.
    fn app_with_rules(dir: &std::path::Path) -> (AppState, Vec<mpsc::Receiver<SessionCommand>>) {
        let (mut state, receivers) = app(&["tank"]);
        state.sessions[0].rules = (dir.to_path_buf(), Some("tank".to_string()));
        (state, receivers)
    }

    /// `Alt+V` builds a trigger pattern out of a picked line, so it has to
    /// use the projection triggers actually match against (§7.1). It used a
    /// second implementation, over the render path's parser, which disagrees
    /// with that one about escapes a MUD really sends — an `ESC M` here — and
    /// so prefilled a pattern that could never match the line it came from.
    #[test]
    fn a_picked_line_prefills_the_pattern_a_trigger_would_match() {
        let dir = config_with_alias("old");
        let (mut state, _rx) = app_with_rules(dir.path());
        let line = "The kobold\x1bM is DEAD!";
        state.sessions[0]
            .scrollback
            .push_back(RetainedLine::server(line));

        open_new_trigger_from_line(&mut state, &[], 0);

        let editor = state.config_editor.as_ref().expect("the editor opened");
        let pattern = editor
            .draft()
            .triggers
            .last()
            .and_then(|trigger| trigger.pattern.clone())
            .expect("a new trigger was added, with a pattern");
        assert_eq!(
            pattern,
            engine::pattern_escape(&crate::scrollback::strip_ansi(line)),
            "the pattern must be built from the projection triggers match"
        );
        // A plain pattern only escapes its own syntax, so what the player is
        // handed to edit is the line they picked, not a thicket of
        // backslashes.
        assert_eq!(pattern, "The kobold is DEAD!");
    }

    #[tokio::test]
    async fn reload_picks_up_edited_rules() {
        let dir = config_with_alias("old");
        let (state, mut receivers) = app_with_rules(dir.path());

        // Edit the module on disk, exactly as a player would.
        std::fs::write(
            dir.path().join("modules/combat.yaml"),
            "name: combat\naliases:\n  - regex: '^x$'\n    send: [\"new\"]\n",
        )
        .unwrap();

        let notice = reload_rules(&state, &[]).await;
        assert!(notice.contains("reloaded"), "{notice}");

        match receivers[0].try_recv().expect("a new rule set was sent") {
            SessionCommand::SetRules(mut engine) => {
                assert_eq!(engine.expand_input("x").sends, vec!["new"]);
            }
            other => panic!("expected SetRules, got {other:?}"),
        }
    }

    /// A broken rule file must report itself and leave the running session
    /// on the rules it already had, rather than dropping to no rules.
    #[tokio::test]
    async fn reload_reports_a_broken_file_and_sends_nothing() {
        let dir = config_with_alias("old");
        let (state, mut receivers) = app_with_rules(dir.path());

        std::fs::write(
            dir.path().join("modules/combat.yaml"),
            "name: combat\naliases:\n  - regex: '('\n",
        )
        .unwrap();

        let notice = reload_rules(&state, &[]).await;
        assert!(notice.contains("reload failed"), "{notice}");
        assert!(
            notice.contains("keeping the current rules"),
            "the player must be told the old rules still apply: {notice}"
        );
        assert!(
            receivers[0].try_recv().is_err(),
            "a failed reload must not replace the session's rules"
        );
    }

    #[tokio::test]
    async fn reload_without_a_session_explains_itself() {
        let (state, _rx) = app(&[]);
        let notice = reload_rules(&state, &[]).await;
        assert!(notice.contains("needs a connected session"), "{notice}");
    }

    // ---- scrollback navigation (§11.5) ----

    /// `scrollback_size` (§8) has to actually bound the buffer, the same
    /// property `history_is_bounded_and_discards_oldest_first` already
    /// covers for history — a config knob nobody discards against is a
    /// setting that doesn't do anything.
    #[test]
    fn session_scrollback_is_bounded_and_discards_oldest_first() {
        let (mut state, _rx) = app(&["tank"]);
        state.sessions[0].scrollback_limit = 2;
        for line in ["one", "two", "three"] {
            state.sessions[0].push_line(RetainedLine::server(line));
        }

        assert_eq!(scrollback(&state.sessions[0]), "two\nthree");
    }

    /// Same property, the channel-pane code path (`ChannelPane::push`) —
    /// structurally separate from `SessionPane::push_line`, so nothing
    /// above proves this one trims too.
    #[test]
    fn channel_scrollback_is_bounded_and_discards_oldest_first() {
        let (mut state, _rx) = app(&["tank"]);
        state.channels.push(ChannelPane {
            config: channel("comms"),
            lines: VecDeque::new(),
            unread: 0,
            scrollback_limit: 2,
            back_offset: 0,
        });
        state.show_channels = true;
        for line in ["one", "two", "three"] {
            state.push_routed(0, "comms", line.to_string());
        }

        let kept: Vec<&str> = state.channels[0]
            .lines
            .iter()
            .map(|line| line.text.as_str())
            .collect();
        assert_eq!(kept, ["two", "three"]);
    }

    /// The scroll keys are unmodified `PgUp`/`PgDn`/`Home`/`End` only — a
    /// chord like `Ctrl+PageUp` must fall through rather than being
    /// silently swallowed as a scroll.
    #[test]
    fn a_modified_scroll_key_is_not_a_scroll_key() {
        assert!(is_scroll_key(KeyCode::PageUp, KeyModifiers::NONE));
        assert!(!is_scroll_key(KeyCode::PageUp, KeyModifiers::CONTROL));
        assert!(!is_scroll_key(KeyCode::Home, KeyModifiers::SHIFT));
    }

    /// Two presses move twice as far as one — `back_offset` accumulates
    /// rather than being reset or capped per keypress.
    #[test]
    fn repeated_pgup_presses_accumulate() {
        let (mut state, _rx) = app(&["tank"]);
        state.scroll_focused(KeyCode::PageUp);
        state.scroll_focused(KeyCode::PageUp);

        assert_eq!(state.sessions[0].back_offset, SCROLL_PAGE * 2);
    }

    /// The channel-pane half of `scrolling_up_then_new_output_does_not_move_the_reader`
    /// / `a_pane_at_the_tail_stays_pinned_on_new_output` below: `push_routed`
    /// is a structurally separate path from `apply_session_event`, and
    /// nothing else proves it respects a reader's scroll position too.
    #[test]
    fn a_scrolled_channel_pane_is_not_moved_by_new_output() {
        let (mut state, _rx) = app(&["tank"]);
        state.channels.push(ChannelPane {
            config: channel("comms"),
            lines: VecDeque::new(),
            unread: 0,
            scrollback_limit: 10_000,
            back_offset: 0,
        });
        state.show_channels = true;
        state.focus_pane(Focus::Channel(0));
        state.scroll_focused(KeyCode::PageUp);
        let offset_before = state.channels[0].back_offset;
        assert_ne!(offset_before, 0);

        state.push_routed(0, "comms", "more chatter".to_string());

        assert_eq!(
            state.channels[0].back_offset, offset_before,
            "a scrolled channel reader's position must not move"
        );
    }

    /// `PgUp` acts on the *focused* pane, not the input-bound session — the
    /// single most likely bug in this feature (§11.1). A channel pane
    /// focused while a different session is bound must scroll the channel.
    #[test]
    fn pgup_scrolls_the_focused_pane_not_the_bound_session() {
        let (mut state, _rx) = app(&["tank", "cleric"]);
        state.channels.push(ChannelPane {
            config: channel("comms"),
            lines: VecDeque::new(),
            unread: 0,
            scrollback_limit: 10_000,
            back_offset: 0,
        });
        state.show_channels = true;
        // Bind input to "tank" (index 0), then focus the channel pane.
        state.focus_pane(Focus::Session(0));
        state.focus_pane(Focus::Channel(0));
        assert_eq!(state.input_session, 0, "input stays bound to tank");

        state.scroll_focused(KeyCode::PageUp);

        assert_eq!(
            state.channels[0].back_offset, SCROLL_PAGE,
            "the focused channel pane must scroll"
        );
        assert_eq!(
            state.sessions[0].back_offset, 0,
            "the bound-but-unfocused session must not scroll"
        );
    }

    /// `PgDn` at the tail must not go negative — saturating arithmetic, not
    /// a panic or wraparound.
    #[test]
    fn pgdn_at_the_tail_stays_put() {
        let (mut state, _rx) = app(&["tank"]);
        assert_eq!(state.sessions[0].back_offset, 0);

        state.scroll_focused(KeyCode::PageDown);

        assert_eq!(state.sessions[0].back_offset, 0);
    }

    /// `End` resets to the tail from anywhere, including `Home`'s
    /// `usize::MAX` sentinel that the render-time clamp resolves to the
    /// true top.
    #[test]
    fn home_and_end_jump_to_the_ends() {
        let (mut state, _rx) = app(&["tank"]);

        state.scroll_focused(KeyCode::Home);
        assert_eq!(state.sessions[0].back_offset, usize::MAX);

        state.scroll_focused(KeyCode::End);
        assert_eq!(state.sessions[0].back_offset, 0);
    }

    /// New output must not yank a reader who has scrolled away from the
    /// tail — the offset is stored as distance from the tail, so leaving it
    /// unchanged is exactly what keeps the reader's relative position
    /// stable as the buffer grows underneath it (§11.5).
    #[test]
    fn scrolling_up_then_new_output_does_not_move_the_reader() {
        let (mut state, _rx) = app(&["tank"]);
        state.scroll_focused(KeyCode::PageUp);
        let offset_before = state.sessions[0].back_offset;
        assert_ne!(offset_before, 0);

        apply_session_event(&mut state, 0, SessionEvent::server("more output"));

        assert_eq!(
            state.sessions[0].back_offset, offset_before,
            "a scrolled reader's position must not move on new output"
        );
    }

    /// The complementary case: a pane already at the tail stays pinned as
    /// new lines arrive — the ordinary "sticky bottom" default (§11.5).
    #[test]
    fn a_pane_at_the_tail_stays_pinned_on_new_output() {
        let (mut state, _rx) = app(&["tank"]);
        assert_eq!(state.sessions[0].back_offset, 0);

        apply_session_event(&mut state, 0, SessionEvent::server("more output"));

        assert_eq!(state.sessions[0].back_offset, 0);
    }

    // ---- /newprofile (§15, UX_REVIEW.md B) ----

    fn fill_wizard(wizard: &mut NewProfileWizard, value: &str) -> WizardOutcome {
        for c in value.chars() {
            assert!(matches!(
                wizard.handle_key(KeyCode::Char(c), KeyModifiers::NONE),
                WizardOutcome::Continue
            ));
        }
        wizard.handle_key(KeyCode::Enter, KeyModifiers::NONE)
    }

    #[test]
    fn the_wizard_collects_all_four_fields_then_reports_done() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let mut wizard = NewProfileWizard::new(dir.path().to_path_buf());

        assert!(matches!(
            fill_wizard(&mut wizard, "kestrel"),
            WizardOutcome::Continue
        ));
        assert!(matches!(
            fill_wizard(&mut wizard, "mud.example.org"),
            WizardOutcome::Continue
        ));
        assert!(matches!(
            fill_wizard(&mut wizard, "4000"),
            WizardOutcome::Continue
        ));
        match fill_wizard(&mut wizard, "y") {
            WizardOutcome::Done(profile) => {
                assert_eq!(profile.name, "kestrel");
                assert_eq!(profile.host, "mud.example.org");
                assert_eq!(profile.port, 4000);
                assert!(profile.tls);
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    /// Reusing the wizard mid-session (unlike first-run, where zero
    /// profiles are guaranteed) means a name can collide with one already
    /// on disk — caught at the same step as an empty name, not left to
    /// silently overwrite the existing file.
    #[test]
    fn the_wizard_rejects_a_name_that_already_exists() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        config::save_new_profile(
            dir.path(),
            &config::NewProfile {
                name: "kestrel".to_string(),
                host: "old.example.org".to_string(),
                port: 23,
                tls: false,
            },
        )
        .unwrap();

        let mut wizard = NewProfileWizard::new(dir.path().to_path_buf());
        assert!(matches!(
            fill_wizard(&mut wizard, "kestrel"),
            WizardOutcome::Continue
        ));
        assert_eq!(
            wizard.error.as_deref(),
            Some("a profile named `kestrel` already exists")
        );
        assert_eq!(
            wizard.step,
            WizardStep::Name,
            "must not advance past the collision"
        );
    }

    /// A name well past what anyone would actually type is rejected rather
    /// than saved and left to grow the wizard's own dialog box to the
    /// terminal's full width (UX_REVIEW.md, Adversarial findings, Low #6).
    #[test]
    fn the_wizard_rejects_a_name_longer_than_the_cap() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let mut wizard = NewProfileWizard::new(dir.path().to_path_buf());

        let too_long = "x".repeat(MAX_PROFILE_NAME_LEN + 1);
        assert!(matches!(
            fill_wizard(&mut wizard, &too_long),
            WizardOutcome::Continue
        ));
        assert_eq!(
            wizard.error.as_deref(),
            Some("a profile name can't be longer than 32 characters")
        );
        assert_eq!(wizard.step, WizardStep::Name);

        // Exactly at the cap is fine.
        assert!(matches!(
            fill_wizard(&mut wizard, &"x".repeat(MAX_PROFILE_NAME_LEN)),
            WizardOutcome::Continue
        ));
        assert_eq!(wizard.step, WizardStep::Host);
    }

    #[tokio::test]
    async fn newprofile_command_opens_the_form_and_saving_writes_the_file() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let (mut state, _rx) = app(&["tank"]);
        state.config_dir = dir.path().to_path_buf();

        submit(&mut state, "/newprofile").await;
        assert!(state.new_profile_wizard.is_some());

        for c in "kestrel".chars() {
            press(&mut state, KeyCode::Char(c), KeyModifiers::NONE);
        }
        press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
        for c in "mud.example.org".chars() {
            press(&mut state, KeyCode::Char(c), KeyModifiers::NONE);
        }
        press(&mut state, KeyCode::Enter, KeyModifiers::NONE);
        press(&mut state, KeyCode::Enter, KeyModifiers::NONE); // blank port -> 23
        press(&mut state, KeyCode::Char('n'), KeyModifiers::NONE);
        press(&mut state, KeyCode::Enter, KeyModifiers::NONE);

        assert!(state.new_profile_wizard.is_none());
        assert!(config::has_profiles(dir.path()));
        assert!(
            scrollback(&state.sessions[0]).contains("saved profiles/kestrel.yaml"),
            "{:?}",
            scrollback(&state.sessions[0])
        );
    }

    #[tokio::test]
    async fn escape_cancels_the_newprofile_form_without_saving() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let (mut state, _rx) = app(&["tank"]);
        state.config_dir = dir.path().to_path_buf();

        submit(&mut state, "/newprofile").await;
        for c in "kestrel".chars() {
            press(&mut state, KeyCode::Char(c), KeyModifiers::NONE);
        }
        assert!(press(&mut state, KeyCode::Esc, KeyModifiers::NONE));

        assert!(state.new_profile_wizard.is_none());
        assert!(!config::has_profiles(dir.path()));
    }

    /// The whole point of putting this in `AppState` instead of a second
    /// blocking terminal loop, like the first-run wizard's own: other
    /// sessions keep receiving and rendering while the form is up (§15).
    #[tokio::test]
    async fn other_sessions_keep_running_while_the_form_is_open() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let (mut state, _rx) = app(&["tank", "cleric"]);
        state.config_dir = dir.path().to_path_buf();

        submit(&mut state, "/newprofile").await;
        assert!(state.new_profile_wizard.is_some());

        apply_session_event(&mut state, 1, SessionEvent::server("a rat scurries past"));

        assert!(
            scrollback(&state.sessions[1]).contains("a rat scurries past"),
            "the other session must keep updating while the form is open"
        );
        assert!(
            state.new_profile_wizard.is_some(),
            "the form must still be open"
        );
    }

    // ---- the room graph (§16) ----

    fn room(id: i64, arrived_via: Option<&str>) -> SessionEvent {
        SessionEvent::Room {
            info: Box::new(crate::map::RoomInfo {
                id: crate::map::RoomId(id),
                name: None,
                area: Some("Test".to_string()),
                exits: std::collections::BTreeMap::new(),
            }),
            arrived_via: arrived_via.map(str::to_string),
        }
    }

    /// A gate standing open when a route was learned can be locked when it
    /// is next walked, so a speedwalk's middle step simply fails and every
    /// room after it is reached by a different step than the one that was
    /// queued for it. The session reports those rooms with no movement
    /// credited (§16), and the map must take it at its word: recording
    /// where the character has been, and inventing no edge between the
    /// places a failed walk happened to visit.
    #[test]
    fn a_walk_broken_part_way_records_rooms_but_invents_no_edges() {
        let (mut state, _rx) = app(&["tank"]);

        // Four rooms reached during a five-step walk whose third step hit
        // the locked gate — the session could not tell which step brought
        // which room, so none of them carry a direction.
        for id in [2, 3, 4, 5] {
            apply_session_event(&mut state, 0, room(id, None));
        }

        let session = &state.sessions[0];
        assert_eq!(
            session.current_room,
            Some(crate::map::RoomId(5)),
            "position still tracks, even with nothing learned about the way there"
        );
        assert!(
            state
                .map_of(0)
                .unwrap()
                .rooms
                .values()
                .all(|r| r.exits.is_empty()),
            "a broken walk must leave no edge behind: {:?}",
            state.map_of(0).unwrap().rooms
        );
        assert_eq!(
            state.map_of(0).unwrap().rooms.len(),
            4,
            "but the rooms are still known"
        );
    }

    /// A `--host` session has no profile to key a map on, the same reason
    /// it gets no `/config` and no disk log. It must leave nothing behind
    /// A `--host` session used to get no map at all, having no profile to
    /// key one by. Keying by world instead means it has an answer — it
    /// knows its host and port — so exploring without a profile is no
    /// longer exploring into nothing.
    #[test]
    fn an_ad_hoc_session_now_keeps_its_map() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let (mut state, _rx) = app(&["adhoc"]);
        state.sessions[0].rules = (dir.path().to_path_buf(), None);
        state.config_dir = dir.path().to_path_buf();
        state.sessions[0].map_key = config::map_key(None, "mud.example.org", 4000);
        apply_session_event(&mut state, 0, room(1, None));

        let key = state.sessions[0].map_key.clone();
        save_world_map(&mut state, &key);

        let reloaded = config::load_map(dir.path(), "mud.example.org");
        assert!(reloaded.rooms.contains_key(&crate::map::RoomId(1)));
    }

    /// The other half: a profile session's exploration outlives it, which
    /// is the whole point of keeping the graph on disk.
    #[test]
    fn a_profile_session_saves_its_map_where_it_can_be_loaded_again() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let (mut state, _rx) = app(&["tank"]);
        state.sessions[0].rules = (dir.path().to_path_buf(), Some("tank".to_string()));
        state.config_dir = dir.path().to_path_buf();
        state.sessions[0].map_key = "tank".to_string();
        apply_session_event(&mut state, 0, room(1, None));
        apply_session_event(&mut state, 0, room(2, Some("n")));

        let key = state.sessions[0].map_key.clone();
        save_world_map(&mut state, &key);

        let reloaded = config::load_map(dir.path(), "tank");
        assert_eq!(
            reloaded.path(crate::map::RoomId(1), crate::map::RoomId(2)),
            Some(vec!["n".to_string()]),
            "the walked edge should survive the round trip, not just the rooms"
        );
    }

    /// The complementary case, so the conservatism above is not mistaken
    /// for the map never learning anything: one step, unambiguously
    /// credited, is exactly how exploring on foot fills the graph in.
    #[test]
    fn a_single_credited_step_records_the_edge() {
        let (mut state, _rx) = app(&["tank"]);

        apply_session_event(&mut state, 0, room(1, None));
        apply_session_event(&mut state, 0, room(2, Some("n")));

        assert_eq!(
            state.map_of(0).unwrap().rooms[&crate::map::RoomId(1)]
                .exits
                .get("n"),
            Some(&Some(crate::map::RoomId(2)))
        );
        assert_eq!(
            state
                .map_of(0)
                .unwrap()
                .path(crate::map::RoomId(1), crate::map::RoomId(2)),
            Some(vec!["n".to_string()])
        );
    }

    // ---- /goto (§16) ----

    /// Drops a room straight into the map, bypassing `SessionEvent::Room`
    /// (and so `arrived_via`'s single-outstanding-move rule) — `/goto`'s
    /// tests are about walking a route the map already claims to know, not
    /// about how that route was learned.
    fn put_room(map: &mut crate::map::Map, id: i64, name: Option<&str>, exits: &[(&str, i64)]) {
        map.rooms.insert(
            crate::map::RoomId(id),
            crate::map::Room {
                id: crate::map::RoomId(id),
                mark: None,
                name: name.map(str::to_string),
                area: None,
                exits: exits
                    .iter()
                    .map(|(dir, dest)| (dir.to_string(), Some(crate::map::RoomId(*dest))))
                    .collect(),
            },
        );
    }

    #[tokio::test]
    async fn goto_without_a_known_position_says_so() {
        let (mut state, _receivers) = app(&["tank"]);

        submit(&mut state, "/goto 1").await;

        assert!(
            scrollback(&state.sessions[0]).contains("doesn't know where you are"),
            "{}",
            scrollback(&state.sessions[0])
        );
    }

    /// The property the whole design hinges on: only the step in flight is
    /// ever on the wire, so a route that turns out to be wrong is caught
    /// after one step, not after all of them.
    #[tokio::test]
    async fn goto_by_vnum_walks_one_step_at_a_time() {
        let (mut state, mut receivers) = app(&["tank"]);
        put_room(state.map_of_mut(0), 1, None, &[("n", 2)]);
        put_room(state.map_of_mut(0), 2, None, &[("n", 3)]);
        put_room(state.map_of_mut(0), 3, None, &[]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));

        submit(&mut state, "/goto 3").await;

        assert!(
            matches!(receivers[0].try_recv(), Ok(SessionCommand::SendLine(line)) if line == "n"),
            "the first step should be sent immediately"
        );
        assert!(
            receivers[0].try_recv().is_err(),
            "the second step must wait for the first to be confirmed, not fire alongside it"
        );
        let walk = state.sessions[0]
            .walk
            .as_ref()
            .expect("a walk should be running");
        assert_eq!(walk.remaining, VecDeque::from(["n".to_string()]));
        assert_eq!(walk.expecting, crate::map::RoomId(2));
        assert!(
            scrollback(&state.sessions[0]).contains("2 steps"),
            "{}",
            scrollback(&state.sessions[0])
        );
    }

    #[tokio::test]
    async fn goto_by_name_substring() {
        let (mut state, mut receivers) = app(&["tank"]);
        put_room(state.map_of_mut(0), 1, Some("Town Square"), &[("e", 2)]);
        put_room(state.map_of_mut(0), 2, Some("Temple of the Sun"), &[]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));

        submit(&mut state, "/goto temple").await;

        assert!(
            matches!(receivers[0].try_recv(), Ok(SessionCommand::SendLine(line)) if line == "e")
        );
        assert_eq!(
            state.sessions[0].walk.as_ref().map(|w| w.destination),
            Some(crate::map::RoomId(2))
        );
    }

    /// `/map` does two things on purpose (§16): it toggles the column, and
    /// it prints the description into the scrollback. The printed form is
    /// the only one a screen reader can read — a grid of glyphs in the
    /// alternate screen is not — so it is not a fallback for when the pane
    /// is off, it happens every time.
    #[tokio::test]
    async fn map_toggles_the_column_and_always_prints_the_description() {
        let (mut state, _receivers) = app(&["tank"]);
        put_room(state.map_of_mut(0), 1, Some("Town Square"), &[("e", 2)]);
        put_room(state.map_of_mut(0), 2, Some("Temple of the Sun"), &[]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));

        submit(&mut state, "/map").await;
        assert!(state.show_map, "the column comes up");
        let printed = scrollback(&state.sessions[0]);
        assert!(printed.contains("Town Square (#1)"), "{printed}");
        assert!(
            printed.contains("east to Temple of the Sun"),
            "the description names where exits lead: {printed}"
        );

        submit(&mut state, "/map").await;
        assert!(!state.show_map, "and goes back down");
        let printed = scrollback(&state.sessions[0]);
        assert_eq!(
            printed.matches("Town Square (#1)").count(),
            2,
            "printed on the way down too, not only on the way up: {printed}"
        );
    }

    /// `/comms` is the typed way to the column the key already toggles —
    /// same state, so the two cannot disagree about whether it is up.
    #[tokio::test]
    async fn comms_toggles_the_column() {
        let (mut state, _receivers) = app(&["tank"]);
        with_channel(&mut state, "comms", false);
        state.show_channels = false;

        submit(&mut state, "/comms").await;
        assert!(state.show_channels, "the column comes up");

        submit(&mut state, "/comms").await;
        assert!(!state.show_channels, "and goes back down");
    }

    /// Hiding the column must take focus with it: focus resting on a pane
    /// that is no longer drawn is what sends the next keystroke nowhere.
    #[tokio::test]
    async fn hiding_comms_takes_focus_off_it() {
        let (mut state, _receivers) = app(&["tank"]);
        with_channel(&mut state, "comms", false);
        state.focus_pane(Focus::Channel(0));

        submit(&mut state, "/comms").await;

        assert!(!state.show_channels);
        assert_eq!(state.focus, Focus::Session(0));
    }

    /// An install with no `channels:` block has no column to reveal.
    /// Saying so beats toggling a flag nothing draws — a command that
    /// silently does nothing is how a player concludes it is broken.
    #[tokio::test]
    async fn comms_says_so_when_there_are_none() {
        let (mut state, _receivers) = app(&["tank"]);

        submit(&mut state, "/comms").await;

        assert!(!state.show_channels);
        assert!(
            scrollback(&state.sessions[0]).contains("no comms panes to show"),
            "{}",
            scrollback(&state.sessions[0])
        );
    }

    /// The map has nothing to say before the server has placed you, and
    /// saying nothing at all reads as a broken command (§16's plain-report
    /// rule, as `/goto` already follows).
    #[tokio::test]
    async fn map_says_so_when_it_does_not_know_where_you_are() {
        let (mut state, _receivers) = app(&["tank"]);

        submit(&mut state, "/map").await;

        assert!(state.show_map, "the column still toggles");
        assert!(
            scrollback(&state.sessions[0]).contains("doesn't know where you are"),
            "{}",
            scrollback(&state.sessions[0])
        );
    }

    /// Silently picking one of several matches would walk the character
    /// somewhere they never named — a vnum is unambiguous, so that is what
    /// the player is asked for instead.
    #[tokio::test]
    async fn goto_ambiguous_name_lists_candidates_and_starts_no_walk() {
        let (mut state, mut receivers) = app(&["tank"]);
        put_room(state.map_of_mut(0), 1, Some("Start"), &[]);
        put_room(state.map_of_mut(0), 10, Some("North Gate"), &[]);
        put_room(state.map_of_mut(0), 11, Some("South Gate"), &[]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));

        submit(&mut state, "/goto gate").await;

        assert!(state.sessions[0].walk.is_none());
        assert!(
            receivers[0].try_recv().is_err(),
            "nothing should be sent while the match is still ambiguous"
        );
        let text = scrollback(&state.sessions[0]);
        assert!(text.contains("#10") && text.contains("#11"), "{text}");
    }

    #[tokio::test]
    async fn goto_reports_a_name_with_no_match() {
        let (mut state, _receivers) = app(&["tank"]);
        put_room(state.map_of_mut(0), 1, Some("Start"), &[]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));

        submit(&mut state, "/goto nowhere").await;

        assert!(state.sessions[0].walk.is_none());
        assert!(scrollback(&state.sessions[0]).contains("no room matches"));
    }

    #[tokio::test]
    async fn goto_reports_an_unknown_vnum() {
        let (mut state, _receivers) = app(&["tank"]);
        put_room(state.map_of_mut(0), 1, None, &[]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));

        submit(&mut state, "/goto 999").await;

        assert!(state.sessions[0].walk.is_none());
        assert!(scrollback(&state.sessions[0]).contains("no room #999"));
    }

    #[tokio::test]
    async fn goto_reports_no_known_route() {
        let (mut state, _receivers) = app(&["tank"]);
        put_room(state.map_of_mut(0), 1, None, &[]);
        put_room(state.map_of_mut(0), 2, None, &[]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));

        submit(&mut state, "/goto 2").await;

        assert!(state.sessions[0].walk.is_none());
        assert!(scrollback(&state.sessions[0]).contains("no known route"));
    }

    #[tokio::test]
    async fn goto_reports_already_there() {
        let (mut state, _receivers) = app(&["tank"]);
        put_room(state.map_of_mut(0), 1, None, &[]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));

        submit(&mut state, "/goto 1").await;

        assert!(scrollback(&state.sessions[0]).contains("already there"));
    }

    /// A step that lands anywhere other than the room the map predicted
    /// means the route is stale — the walk has to stop rather than guess
    /// the remaining steps from an unplanned-for room.
    #[test]
    fn a_step_landing_somewhere_unexpected_stops_the_walk() {
        let (mut state, _rx) = app(&["tank"]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));
        state.sessions[0].walk = Some(Walk {
            remaining: VecDeque::new(),
            expecting: crate::map::RoomId(2),
            destination: crate::map::RoomId(5),
        });

        let (_, injections) = apply_session_event(&mut state, 0, room(99, Some("n")));

        assert!(injections.is_empty(), "a broken walk sends nothing further");
        assert!(state.sessions[0].walk.is_none());
        let text = scrollback(&state.sessions[0]);
        assert!(
            text.contains("walk stopped") && text.contains("#99") && text.contains("#2"),
            "{text}"
        );
    }

    /// Because `/goto` never has more than one step outstanding, every room
    /// update it causes has exactly one credited movement — so a wrong edge
    /// the walk crosses gets overwritten with the truth by the very same
    /// `SessionEvent::Room` handling that stops the walk. Walking repairs
    /// the map.
    #[test]
    fn a_surprise_during_a_walk_teaches_the_map_nothing() {
        let (mut state, _rx) = app(&["tank"]);
        // The route believes `n` from room 1 reaches room 99.
        put_room(state.map_of_mut(0), 1, None, &[("n", 99)]);
        let session = &mut state.sessions[0];
        session.current_room = Some(crate::map::RoomId(1));
        session.walk = Some(Walk {
            remaining: VecDeque::new(),
            expecting: crate::map::RoomId(99),
            destination: crate::map::RoomId(99),
        });

        // Room 2 turns up instead. That could be a summon, a wimpy
        // auto-flee, or an exit that simply does not lead where it did last
        // time — nothing here can tell those apart, so none of them get to
        // rewrite the map.
        apply_session_event(&mut state, 0, room(2, Some("n")));

        let session = &state.sessions[0];
        assert!(
            session.walk.is_none(),
            "arriving somewhere the route didn't predict must stop the walk"
        );
        assert_eq!(
            state.map_of(0).unwrap().rooms[&crate::map::RoomId(1)]
                .exits
                .get("n"),
            Some(&Some(crate::map::RoomId(99))),
            "and must not overwrite the edge with an arrival it cannot attribute"
        );
        assert_eq!(
            session.current_room,
            Some(crate::map::RoomId(2)),
            "where the character actually is is never in doubt: the server said so"
        );
    }

    /// The capability that guard costs us, kept where it is safe. Walking
    /// on foot has no route to contradict, so a step still teaches the map
    /// — including correcting an edge that has gone stale. Noticing a
    /// `/goto` stop and walking the leg by hand is how a changed world gets
    /// written back (§16).
    #[test]
    fn walking_on_foot_still_corrects_a_stale_edge() {
        let (mut state, _rx) = app(&["tank"]);
        put_room(state.map_of_mut(0), 1, None, &[("n", 99)]);
        let session = &mut state.sessions[0];
        session.current_room = Some(crate::map::RoomId(1));
        assert!(session.walk.is_none(), "no route running");

        apply_session_event(&mut state, 0, room(2, Some("n")));

        assert_eq!(
            state.map_of(0).unwrap().rooms[&crate::map::RoomId(1)]
                .exits
                .get("n"),
            Some(&Some(crate::map::RoomId(2))),
            "an unprompted step is attributable, so it may correct the map"
        );
    }

    /// Player intervention wins over a route in progress: a gate the walk
    /// hasn't discovered yet, or a change of mind, is the player's call.
    #[tokio::test]
    async fn a_typed_line_cancels_an_active_walk() {
        let (mut state, mut receivers) = app(&["tank"]);
        state.sessions[0].walk = Some(Walk {
            remaining: VecDeque::from(["n".to_string()]),
            expecting: crate::map::RoomId(2),
            destination: crate::map::RoomId(3),
        });

        submit(&mut state, "look").await;

        assert!(state.sessions[0].walk.is_none());
        assert!(scrollback(&state.sessions[0]).contains("cancelled"));
        assert!(
            matches!(receivers[0].try_recv(), Ok(SessionCommand::SendLine(line)) if line == "look")
        );
    }

    /// Walking a step of your own mid-`/goto` — impatience, or spotting
    /// something the route did not know about. Two things have to hold:
    /// the walk is off, and the step it already put on the wire must not
    /// quietly resume it when its room lands a moment later. A route that
    /// carries on after the player has taken the wheel is the same failure
    /// as one that carries on through a locked gate (§16).
    #[tokio::test]
    async fn a_typed_step_mid_walk_cancels_it_and_a_late_arrival_does_not_resume_it() {
        let (mut state, mut receivers) = app(&["tank"]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));
        state.sessions[0].walk = Some(Walk {
            remaining: VecDeque::from(["e".to_string()]),
            expecting: crate::map::RoomId(2),
            destination: crate::map::RoomId(3),
        });

        submit(&mut state, "w").await;

        assert!(state.sessions[0].walk.is_none(), "the walk is off");
        assert!(
            matches!(receivers[0].try_recv(), Ok(SessionCommand::SendLine(line)) if line == "w"),
            "and the step the player typed still reaches the server"
        );

        // The walk's own last step lands now, after the cancellation.
        let (_, out) = apply_session_event(&mut state, 0, room(2, Some("n")));

        assert!(
            out.is_empty(),
            "a cancelled walk must not send another step: {out:?}"
        );
        assert!(state.sessions[0].walk.is_none());
    }

    // ---- periodic map saving (§16) ----

    /// Exploration used to reach disk only on a clean quit or a disconnect,
    /// so a crash cost the whole session's mapping.
    #[test]
    fn arriving_somewhere_new_marks_the_map_for_saving() {
        let (mut state, _rx) = app(&["tank"]);
        assert!(!state.world_mut(0).dirty, "nothing learned yet");

        apply_session_event(&mut state, 0, room(1, None));

        assert!(state.world_mut(0).dirty);
    }

    #[test]
    fn a_periodic_save_writes_the_map_and_marks_it_clean() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let (mut state, _rx) = app(&["tank"]);
        state.sessions[0].rules = (dir.path().to_path_buf(), Some("tank".to_string()));
        state.config_dir = dir.path().to_path_buf();
        state.sessions[0].map_key = "tank".to_string();
        apply_session_event(&mut state, 0, room(1, None));
        apply_session_event(&mut state, 0, room(2, Some("n")));

        save_dirty_maps(&mut state);

        assert!(!state.world_mut(0).dirty, "written, so no longer dirty");
        let reloaded = config::load_map(dir.path(), "tank");
        assert_eq!(
            reloaded.path(crate::map::RoomId(1), crate::map::RoomId(2)),
            Some(vec!["n".to_string()]),
            "the walked edge should be on disk without quitting first"
        );
    }

    /// The end-to-end version of the bug report: mark a room, save, unmark
    /// it, save again — the second save used to lose the race against its
    /// own earlier one, because merging the session's "no mark" into a
    /// file that still had the old mark always kept the old mark.
    /// `explicit_marks` is what makes an intentional removal actually
    /// survive a restart rather than quietly reviving on the next launch.
    #[test]
    fn unmarking_a_room_survives_a_reload_after_it_was_already_saved_once() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let (mut state, _rx) = app(&["tank"]);
        state.sessions[0].rules = (dir.path().to_path_buf(), Some("tank".to_string()));
        state.config_dir = dir.path().to_path_buf();
        state.sessions[0].map_key = "tank".to_string();
        put_room(state.map_of_mut(0), 1, None, &[]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));

        mark_current_room(&mut state, "shop");
        save_dirty_maps(&mut state);
        assert_eq!(
            config::load_map(dir.path(), "tank").rooms[&crate::map::RoomId(1)]
                .mark
                .as_deref(),
            Some("shop"),
            "the mark should have reached disk"
        );

        mark_current_room(&mut state, "");
        let remove_row = state.mark_menu.as_ref().unwrap().entries().len() - 1;
        state.mark_menu.as_mut().unwrap().selected = remove_row;
        apply_mark_menu(&mut state);
        save_dirty_maps(&mut state);

        assert_eq!(
            config::load_map(dir.path(), "tank").rooms[&crate::map::RoomId(1)].mark,
            None,
            "the removal should also have reached disk, not been merged away"
        );
    }

    /// The point of the flag: a tick that finds nothing new must not
    /// rewrite the file. Otherwise idling in one room churns the disk
    /// every interval, forever.
    #[test]
    fn a_periodic_save_with_nothing_new_writes_nothing() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let (mut state, _rx) = app(&["tank"]);
        state.sessions[0].rules = (dir.path().to_path_buf(), Some("tank".to_string()));
        state.config_dir = dir.path().to_path_buf();
        state.sessions[0].map_key = "tank".to_string();
        apply_session_event(&mut state, 0, room(1, None));
        save_dirty_maps(&mut state);

        // Overwrite the file behind the client's back: if the next tick
        // rewrites it, this sentinel is gone.
        let path = dir.path().join("maps").join("tank.json");
        std::fs::write(&path, b"{\"rooms\":[]}").unwrap();

        save_dirty_maps(&mut state);

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"rooms\":[]}",
            "an unchanged map must not be rewritten"
        );
    }

    /// A `--host` session has no profile to save under, and must not
    /// invent one just because the timer fired.
    #[test]
    fn a_periodic_save_skips_a_session_with_no_world() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let (mut state, _rx) = app(&["adhoc"]);
        state.sessions[0].rules = (dir.path().to_path_buf(), None);
        state.config_dir = dir.path().to_path_buf();
        state.sessions[0].map_key = String::new();
        apply_session_event(&mut state, 0, room(1, None));

        save_dirty_maps(&mut state);

        assert!(!dir.path().join("maps").exists());
    }

    /// An adversarial review found this taking the whole client down: the
    /// walk sets its next expectation from the live map, so a server that
    /// re-points an exit mid-route lands the character in a room the route
    /// never planned for, and the next step indexed an exit that room does
    /// not have.
    #[tokio::test]
    async fn a_repointed_exit_mid_walk_stops_the_walk_instead_of_panicking() {
        let (mut state, mut receivers) = app(&["tank"]);
        put_room(state.map_of_mut(0), 1, None, &[("n", 2)]);
        put_room(state.map_of_mut(0), 2, None, &[("e", 3)]);
        put_room(state.map_of_mut(0), 3, None, &[("e", 4)]);
        put_room(state.map_of_mut(0), 4, None, &[]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));
        submit(&mut state, "/goto 4").await;
        let _ = receivers[0].try_recv();

        // Arriving in #2 as planned, but the server now says #2's `e` leads
        // somewhere else entirely.
        let mut moved = crate::map::RoomInfo {
            id: crate::map::RoomId(2),
            name: None,
            area: None,
            exits: std::collections::BTreeMap::new(),
        };
        moved
            .exits
            .insert("e".to_string(), Some(crate::map::RoomId(9)));
        apply_session_event(
            &mut state,
            0,
            SessionEvent::Room {
                info: Box::new(moved),
                arrived_via: Some("n".to_string()),
            },
        );
        let _ = receivers[0].try_recv();

        // …which lands the walk in #9, a room the route knows nothing about
        // and which has no `e` of its own.
        let mut elsewhere = crate::map::RoomInfo {
            id: crate::map::RoomId(9),
            name: None,
            area: None,
            exits: std::collections::BTreeMap::new(),
        };
        elsewhere.exits.insert("n".to_string(), None);
        let (_, out) = apply_session_event(
            &mut state,
            0,
            SessionEvent::Room {
                info: Box::new(elsewhere),
                arrived_via: Some("e".to_string()),
            },
        );

        assert!(state.sessions[0].walk.is_none(), "the walk stops");
        assert!(out.is_empty(), "and sends nothing further: {out:?}");
        assert!(
            scrollback(&state.sessions[0]).contains("walk stopped"),
            "and says so: {}",
            scrollback(&state.sessions[0])
        );
    }

    /// The same hazard where the exit exists but its destination does not —
    /// the bare `direction` form most servers send.
    #[tokio::test]
    async fn a_destinationless_exit_mid_walk_stops_the_walk() {
        let (mut state, mut receivers) = app(&["tank"]);
        put_room(state.map_of_mut(0), 1, None, &[("n", 2)]);
        put_room(state.map_of_mut(0), 2, None, &[("e", 3)]);
        put_room(state.map_of_mut(0), 3, None, &[]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));
        submit(&mut state, "/goto 3").await;
        let _ = receivers[0].try_recv();

        // #2 re-reports its `e` with no destination at all.
        let mut vague = crate::map::RoomInfo {
            id: crate::map::RoomId(2),
            name: None,
            area: None,
            exits: std::collections::BTreeMap::new(),
        };
        vague.exits.insert("e".to_string(), None);
        state
            .map_of_mut(0)
            .rooms
            .get_mut(&crate::map::RoomId(2))
            .unwrap()
            .exits
            .insert("e".to_string(), None);
        let (_, out) = apply_session_event(
            &mut state,
            0,
            SessionEvent::Room {
                info: Box::new(vague),
                arrived_via: Some("n".to_string()),
            },
        );

        assert!(state.sessions[0].walk.is_none());
        assert!(out.is_empty(), "{out:?}");
    }

    // ---- a failing map save (§16) ----

    /// Saving refuses to overwrite a map file it could not read, which
    /// stopped one bad file replacing a whole explored world — but the
    /// refusal only reached the log, so a player could explore for an hour
    /// with every save failing and find out at quit.
    #[test]
    fn a_map_save_that_fails_tells_the_player() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let maps = dir.path().join("maps");
        std::fs::create_dir_all(&maps).unwrap();
        std::fs::write(maps.join("tank.json"), b"{ not json").unwrap();
        let (mut state, _rx) = app(&["tank"]);
        state.sessions[0].rules = (dir.path().to_path_buf(), Some("tank".to_string()));
        state.config_dir = dir.path().to_path_buf();
        state.sessions[0].map_key = "tank".to_string();
        apply_session_event(&mut state, 0, room(1, None));

        save_dirty_maps(&mut state);

        assert!(
            scrollback(&state.sessions[0]).contains("could not save the map"),
            "{}",
            scrollback(&state.sessions[0])
        );
    }

    /// Once, not once every 30s. A problem that persists would otherwise
    /// fill the scrollback with the same line and bury the first one.
    #[test]
    fn a_failing_map_save_says_so_once_not_every_tick() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let maps = dir.path().join("maps");
        std::fs::create_dir_all(&maps).unwrap();
        std::fs::write(maps.join("tank.json"), b"{ not json").unwrap();
        let (mut state, _rx) = app(&["tank"]);
        state.sessions[0].rules = (dir.path().to_path_buf(), Some("tank".to_string()));
        state.config_dir = dir.path().to_path_buf();
        state.sessions[0].map_key = "tank".to_string();

        for room_id in 1..=4 {
            apply_session_event(&mut state, 0, room(room_id, None));
            save_dirty_maps(&mut state);
        }

        let complaints = scrollback(&state.sessions[0])
            .matches("could not save the map")
            .count();
        assert_eq!(complaints, 1, "{}", scrollback(&state.sessions[0]));
    }

    /// And if it starts working again, a later failure is news once more.
    #[test]
    fn a_save_that_recovers_can_complain_again_later() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let maps = dir.path().join("maps");
        std::fs::create_dir_all(&maps).unwrap();
        let path = maps.join("tank.json");
        std::fs::write(&path, b"{ not json").unwrap();
        let (mut state, _rx) = app(&["tank"]);
        state.sessions[0].rules = (dir.path().to_path_buf(), Some("tank".to_string()));
        state.config_dir = dir.path().to_path_buf();
        state.sessions[0].map_key = "tank".to_string();
        apply_session_event(&mut state, 0, room(1, None));
        save_dirty_maps(&mut state);

        // The file becomes readable, one save succeeds, then it breaks again.
        std::fs::remove_file(&path).unwrap();
        apply_session_event(&mut state, 0, room(2, Some("n")));
        save_dirty_maps(&mut state);
        assert!(!state.world_mut(0).save_failed, "recovered");
        std::fs::write(&path, b"{ not json either").unwrap();
        apply_session_event(&mut state, 0, room(3, Some("n")));
        save_dirty_maps(&mut state);

        let complaints = scrollback(&state.sessions[0])
            .matches("could not save the map")
            .count();
        assert_eq!(complaints, 2, "{}", scrollback(&state.sessions[0]));
    }

    // ---- remembered pane layout (§11.4) ----

    #[test]
    fn a_pane_key_that_changes_nothing_writes_nothing() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let (mut state, _rx) = app(&["tank"]);
        state.config_dir = dir.path().to_path_buf();
        state.map_width = crate::ui::MIN_MAP_WIDTH;

        // Narrowing a column already at its floor clamps to the same value.
        let before = current_layout(&state);
        let keys = crate::config::Keybinds::default();
        handle_key(
            &mut state,
            &keys,
            KeyCode::Char('.'),
            KeyModifiers::ALT,
            100,
            &[],
        );
        remember_layout_if_changed(&state, before);

        assert!(
            !dir.path().join("ui_state.json").exists(),
            "holding a key against its clamp must not write on every repeat"
        );
    }

    #[test]
    fn toggling_a_pane_is_remembered_at_once() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let (mut state, _rx) = app(&["tank"]);
        state.config_dir = dir.path().to_path_buf();

        let before = current_layout(&state);
        let keys = crate::config::Keybinds::default();
        handle_key(
            &mut state,
            &keys,
            KeyCode::F(7),
            KeyModifiers::NONE,
            100,
            &[],
        );
        remember_layout_if_changed(&state, before);

        let saved = config::load_ui_state(dir.path()).expect("written on the spot");
        assert_eq!(saved.show_map, state.show_map);
        assert!(saved.show_map, "F7 turned the map column on");
    }

    /// The clock down the character panes is layout, so it is remembered
    /// where the rest of the layout is — a player who wants timestamps
    /// wants them tomorrow too, and re-pressing a key every launch is what
    /// `ui_state.json` exists to spare them (§11.4).
    #[test]
    fn the_character_pane_clock_toggles_and_is_remembered() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let (mut state, _rx) = app(&["tank"]);
        state.config_dir = dir.path().to_path_buf();
        assert!(!state.show_timestamps, "off until asked for");

        let before = current_layout(&state);
        assert!(press(&mut state, KeyCode::Char('t'), KeyModifiers::ALT));
        remember_layout_if_changed(&state, before);

        assert!(state.show_timestamps);
        assert!(
            config::load_ui_state(dir.path())
                .expect("written on the spot")
                .show_timestamps
        );

        assert!(press(&mut state, KeyCode::Char('t'), KeyModifiers::ALT));
        assert!(!state.show_timestamps, "and off again");
    }

    /// Reported from play: marking a room `mail` put an `S` on the map.
    /// Typing into the open list was swallowed silently, so `Enter` applied
    /// whatever row was highlighted — the first one, `shop`. Not a wrong
    /// letter but a wrong label.
    #[tokio::test]
    async fn typing_a_word_into_the_chooser_writes_that_word() {
        let (mut state, _rx) = app(&["tank"]);
        put_room(state.map_of_mut(0), 1, None, &[]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));
        submit(&mut state, "/mark").await;
        let keys = crate::config::Keybinds::default();

        for ch in "mail".chars() {
            handle_key(
                &mut state,
                &keys,
                KeyCode::Char(ch),
                KeyModifiers::NONE,
                100,
                &[],
            );
        }
        handle_key(
            &mut state,
            &keys,
            KeyCode::Enter,
            KeyModifiers::NONE,
            100,
            &[],
        );

        assert_eq!(
            state.map_of(0).unwrap().rooms[&crate::map::RoomId(1)]
                .mark
                .as_deref(),
            Some("mail")
        );
    }

    /// The digit shortcuts have to survive it — they are the fast path the
    /// list exists for.
    #[tokio::test]
    async fn a_digit_still_takes_its_row_rather_than_starting_a_label() {
        let (mut state, _rx) = app(&["tank"]);
        put_room(state.map_of_mut(0), 1, None, &[]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));
        submit(&mut state, "/mark").await;
        let keys = crate::config::Keybinds::default();

        handle_key(
            &mut state,
            &keys,
            KeyCode::Char('2'),
            KeyModifiers::NONE,
            100,
            &[],
        );

        assert!(state.mark_menu.is_none(), "taking a row closes the chooser");
        assert_eq!(
            state.map_of(0).unwrap().rooms[&crate::map::RoomId(1)]
                .mark
                .as_deref(),
            Some(MARK_SUGGESTIONS[1])
        );
    }

    // ---- the map cursor (§16) ----

    fn keys() -> crate::config::Keybinds {
        crate::config::Keybinds::default()
    }

    /// A three-room row, so left and right have somewhere to go.
    fn cursor_state() -> (AppState, Vec<mpsc::Receiver<SessionCommand>>) {
        let (mut state, rx) = app(&["tank"]);
        put_room(state.map_of_mut(0), 1, Some("West Gate"), &[("e", 2)]);
        put_room(state.map_of_mut(0), 2, Some("Town Square"), &[("e", 3)]);
        put_room(state.map_of_mut(0), 3, Some("East Road"), &[]);
        state.sessions[0].current_room = Some(crate::map::RoomId(2));
        (state, rx)
    }

    #[tokio::test]
    async fn the_cursor_opens_on_the_character_and_shows_the_map() {
        let (mut state, _rx) = cursor_state();
        state.show_map = false;

        handle_key(
            &mut state,
            &keys(),
            KeyCode::F(8),
            KeyModifiers::NONE,
            100,
            &[],
        );

        assert_eq!(state.map_cursor, Some(crate::map::RoomId(2)));
        assert!(state.show_map, "no use steering a map nobody can see");
    }

    #[tokio::test]
    async fn the_cursor_moves_room_to_room() {
        let (mut state, _rx) = cursor_state();
        handle_key(
            &mut state,
            &keys(),
            KeyCode::F(8),
            KeyModifiers::NONE,
            100,
            &[],
        );

        handle_key(
            &mut state,
            &keys(),
            KeyCode::Left,
            KeyModifiers::NONE,
            100,
            &[],
        );
        assert_eq!(state.map_cursor, Some(crate::map::RoomId(1)));
        handle_key(
            &mut state,
            &keys(),
            KeyCode::Right,
            KeyModifiers::NONE,
            100,
            &[],
        );
        handle_key(
            &mut state,
            &keys(),
            KeyCode::Right,
            KeyModifiers::NONE,
            100,
            &[],
        );
        assert_eq!(state.map_cursor, Some(crate::map::RoomId(3)));
    }

    /// The grid is mostly gaps; losing your place to one would make it
    /// tiring to steer.
    #[tokio::test]
    async fn a_nudge_into_empty_space_leaves_the_cursor_where_it_is() {
        let (mut state, _rx) = cursor_state();
        handle_key(
            &mut state,
            &keys(),
            KeyCode::F(8),
            KeyModifiers::NONE,
            100,
            &[],
        );

        handle_key(
            &mut state,
            &keys(),
            KeyCode::Up,
            KeyModifiers::NONE,
            100,
            &[],
        );

        assert_eq!(state.map_cursor, Some(crate::map::RoomId(2)));
    }

    /// The point of the whole thing: the map stops being a picture and
    /// becomes somewhere to say "there".
    #[tokio::test]
    async fn enter_asks_to_walk_to_the_room_under_the_cursor() {
        let (mut state, mut receivers) = cursor_state();
        handle_key(
            &mut state,
            &keys(),
            KeyCode::F(8),
            KeyModifiers::NONE,
            100,
            &[],
        );
        handle_key(
            &mut state,
            &keys(),
            KeyCode::Right,
            KeyModifiers::NONE,
            100,
            &[],
        );

        handle_key(
            &mut state,
            &keys(),
            KeyCode::Enter,
            KeyModifiers::NONE,
            100,
            &[],
        );

        assert!(state.map_cursor.is_none(), "and closes behind itself");
        assert_eq!(state.walk_requested, Some(crate::map::RoomId(3)));

        // The event loop is what services it, being the half that can await.
        let target = state.walk_requested.take().unwrap();
        walk_to_room(&mut state, target).await;
        assert!(
            matches!(receivers[0].try_recv(), Ok(SessionCommand::SendLine(line)) if line == "e"),
            "the first step goes out"
        );
    }

    #[tokio::test]
    async fn esc_closes_the_cursor_and_walks_nowhere() {
        let (mut state, mut receivers) = cursor_state();
        handle_key(
            &mut state,
            &keys(),
            KeyCode::F(8),
            KeyModifiers::NONE,
            100,
            &[],
        );

        handle_key(
            &mut state,
            &keys(),
            KeyCode::Esc,
            KeyModifiers::NONE,
            100,
            &[],
        );

        assert!(state.map_cursor.is_none());
        assert_eq!(state.walk_requested, None);
        assert!(receivers[0].try_recv().is_err());
    }

    #[tokio::test]
    async fn the_cursor_needs_to_know_where_you_are() {
        let (mut state, _rx) = app(&["tank"]);

        handle_key(
            &mut state,
            &keys(),
            KeyCode::F(8),
            KeyModifiers::NONE,
            100,
            &[],
        );

        assert!(state.map_cursor.is_none());
        assert!(scrollback(&state.sessions[0]).contains("doesn't know where you are"));
    }

    /// Reported from play. The cursor swallowed every key it did not use,
    /// so `Alt+2` with the map cursor up closed the cursor and went no
    /// further — the character it was meant to switch to stayed put, and
    /// the key had to be pressed twice.
    #[tokio::test]
    async fn a_key_the_cursor_ignores_still_does_its_own_job() {
        let (mut state, _rx) = app(&["tank", "cleric"]);
        put_room(state.map_of_mut(0), 1, None, &[]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));
        let keys = crate::config::Keybinds::default();
        handle_key(
            &mut state,
            &keys,
            KeyCode::F(8),
            KeyModifiers::NONE,
            100,
            &[],
        );
        assert!(state.map_cursor.is_some(), "the cursor is up");

        handle_key(
            &mut state,
            &keys,
            KeyCode::Char('2'),
            KeyModifiers::ALT,
            100,
            &[],
        );

        assert!(state.map_cursor.is_none(), "the cursor is put away");
        assert_eq!(state.input_session, 1, "and the jump still happened");
    }

    /// The same for a binding rather than a jump, so this is not a special
    /// case for Alt+N.
    #[tokio::test]
    async fn a_binding_pressed_with_the_cursor_up_still_fires() {
        let (mut state, _rx) = app(&["tank"]);
        put_room(state.map_of_mut(0), 1, None, &[]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));
        let keys = crate::config::Keybinds::default();
        handle_key(
            &mut state,
            &keys,
            KeyCode::F(8),
            KeyModifiers::NONE,
            100,
            &[],
        );
        let before = state.show_hud;

        // F9 toggles the party strip.
        handle_key(
            &mut state,
            &keys,
            KeyCode::F(9),
            KeyModifiers::NONE,
            100,
            &[],
        );

        assert!(state.map_cursor.is_none());
        assert_ne!(state.show_hud, before, "the binding fired too");
    }

    /// Asked for after the map cursor landed: reaching the map should be
    /// the same gesture as reaching any other pane, not a mode of its own.
    /// The sessions keep the numbers they always had and the map takes the
    /// next one.
    #[tokio::test]
    async fn alt_n_reaches_the_map_pane() {
        let (mut state, _rx) = app(&["tank", "cleric"]);
        put_room(state.map_of_mut(0), 1, None, &[]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));
        state.show_map = true;
        let keys = crate::config::Keybinds::default();

        handle_key(
            &mut state,
            &keys,
            KeyCode::Char('3'),
            KeyModifiers::ALT,
            100,
            &[],
        );

        assert_eq!(state.focus, Focus::Map);
        assert_eq!(
            state.map_cursor,
            Some(crate::map::RoomId(1)),
            "and the cursor starts on the character"
        );
        assert_eq!(
            state.input_session, 0,
            "typing stays with the character, as it does for a comms pane"
        );
    }

    #[tokio::test]
    async fn the_existing_session_numbers_do_not_move() {
        let (mut state, _rx) = app(&["tank", "cleric"]);
        state.show_map = true;
        let keys = crate::config::Keybinds::default();

        handle_key(
            &mut state,
            &keys,
            KeyCode::Char('2'),
            KeyModifiers::ALT,
            100,
            &[],
        );

        assert_eq!(state.focus, Focus::Session(1));
    }

    /// With the column hidden there is no pane to reach, so the number
    /// answers to nothing rather than focusing something invisible.
    #[tokio::test]
    async fn alt_n_does_not_reach_a_hidden_map() {
        let (mut state, _rx) = app(&["tank"]);
        state.show_map = false;
        let keys = crate::config::Keybinds::default();

        handle_key(
            &mut state,
            &keys,
            KeyCode::Char('2'),
            KeyModifiers::ALT,
            100,
            &[],
        );

        assert_eq!(state.focus, Focus::Session(0));
    }

    #[tokio::test]
    async fn cycling_focus_visits_the_map_when_it_is_shown() {
        let (mut state, _rx) = app(&["tank"]);
        put_room(state.map_of_mut(0), 1, None, &[]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));
        state.show_map = true;
        state.show_channels = false;

        state.focus_next();

        assert_eq!(state.focus, Focus::Map);
        state.focus_next();
        assert_eq!(state.focus, Focus::Session(0), "and comes back round");
    }

    /// Leaving the map hands the keyboard back rather than stranding focus
    /// on a pane that does not take typing.
    #[tokio::test]
    async fn esc_returns_focus_to_the_character() {
        let (mut state, _rx) = app(&["tank"]);
        put_room(state.map_of_mut(0), 1, None, &[]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));
        state.show_map = true;
        let keys = crate::config::Keybinds::default();
        handle_key(
            &mut state,
            &keys,
            KeyCode::F(8),
            KeyModifiers::NONE,
            100,
            &[],
        );
        assert_eq!(state.focus, Focus::Map);

        handle_key(
            &mut state,
            &keys,
            KeyCode::Esc,
            KeyModifiers::NONE,
            100,
            &[],
        );

        assert_eq!(state.focus, Focus::Session(0));
        assert!(state.map_cursor.is_none());
    }

    // ---- /mark (§16) ----    // ---- /mark (§16) ----    // ---- /mark (§16) ----

    #[tokio::test]
    async fn mark_labels_the_room_and_shows_on_the_map() {
        let (mut state, _rx) = app(&["tank"]);
        put_room(state.map_of_mut(0), 1, Some("Bakers Shop"), &[]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));

        submit(&mut state, "/mark shop").await;

        assert_eq!(
            state.map_of(0).unwrap().rooms[&crate::map::RoomId(1)]
                .mark
                .as_deref(),
            Some("shop")
        );
        let scene = state
            .map_of(0)
            .unwrap()
            .scene(crate::map::RoomId(1), None, &[]);
        assert_eq!(
            scene.rooms[0].mark.as_deref(),
            Some("shop"),
            "and the map carries the label to draw it from"
        );
        assert!(scrollback(&state.sessions[0]).contains("marked #1 as `shop`"));
    }

    /// The exact bug report this closes: mark a room, save reaches disk,
    /// unmark it and walk away — then the process dies (killed, crashed,
    /// or simply raced by a driver that stops it before its own exit
    /// handlers finish) with no periodic tick and no clean quit ever
    /// running. The removal still has to be there, because it was written
    /// the moment it happened, not queued for later.
    #[tokio::test]
    async fn a_removed_mark_is_on_disk_even_if_the_process_dies_right_after() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let (mut state, _rx) = app(&["tank"]);
        state.sessions[0].rules = (dir.path().to_path_buf(), Some("tank".to_string()));
        state.config_dir = dir.path().to_path_buf();
        state.sessions[0].map_key = "tank".to_string();
        put_room(state.map_of_mut(0), 1, None, &[]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));

        submit(&mut state, "/mark shop").await;
        assert_eq!(
            config::load_map(dir.path(), "tank").rooms[&crate::map::RoomId(1)]
                .mark
                .as_deref(),
            Some("shop")
        );

        submit(&mut state, "/mark").await;
        let remove_row = state.mark_menu.as_ref().unwrap().entries().len() - 1;
        state.mark_menu.as_mut().unwrap().selected = remove_row;
        apply_mark_menu(&mut state);
        state.sessions[0].current_room = Some(crate::map::RoomId(2));

        // No `save_dirty_maps`, no quit key — reading straight from disk,
        // as if the process had just been killed.
        assert_eq!(
            config::load_map(dir.path(), "tank").rooms[&crate::map::RoomId(1)].mark,
            None,
            "the removal must already be on disk with no save step run here"
        );
    }

    /// Who goes on the map beside you (§16): the others on this world,
    /// never yourself — you are already `RoomRole::Here` — and never a
    /// character on a different MUD, whose room numbers mean nothing here.
    #[test]
    fn the_party_is_the_others_on_this_world_only() {
        let (mut state, _rx) = app(&["mathias", "saihtam", "elsewhere"]);
        for (index, key) in [(0, "hercmud.net"), (1, "hercmud.net"), (2, "other.mud")] {
            state.sessions[index].map_key = key.to_string();
            state.sessions[index].current_room = Some(crate::map::RoomId(index as i64 + 1));
        }

        let party = state.party_of(0);

        assert_eq!(
            party,
            vec![(crate::map::RoomId(2), "saihtam".to_string())],
            "only the other character on this world"
        );
    }

    /// A character who has connected but has not been placed yet cannot be
    /// drawn anywhere, and guessing would put them in a room they are not
    /// in.
    #[test]
    fn a_character_with_no_room_yet_is_not_on_the_map() {
        let (mut state, _rx) = app(&["mathias", "saihtam"]);
        for session in &mut state.sessions {
            session.map_key = "hercmud.net".to_string();
        }
        state.sessions[0].current_room = Some(crate::map::RoomId(1));

        assert!(state.party_of(0).is_empty());
    }

    /// The property the shared map exists for, stated directly: what one
    /// character learns, every character on that MUD knows at once. The
    /// per-session copies this replaced only reconciled through the file,
    /// so until the next launch two panes could show different amounts of
    /// the same world — quieter than the mark bug, and the same cause.
    #[test]
    fn what_one_character_explores_the_other_sees_immediately() {
        let (mut state, _rx) = app(&["mathias", "saihtam"]);
        for session in &mut state.sessions {
            session.map_key = "hercmud.net".to_string();
        }

        // Only mathias walks.
        apply_session_event(&mut state, 0, room(1, None));
        apply_session_event(&mut state, 0, room(2, Some("n")));

        assert_eq!(
            state
                .map_of(1)
                .unwrap()
                .path(crate::map::RoomId(1), crate::map::RoomId(2)),
            Some(vec!["n".to_string()]),
            "saihtam reads the same map, so the edge is already there"
        );
        assert_eq!(
            state.sessions[1].current_room, None,
            "but standing somewhere is still each character's own business"
        );
    }

    /// The bug two earlier fixes both missed, because both looked at one
    /// session in isolation: a map belongs to the *world*, so two
    /// characters on one MUD share the file, and each holds its own
    /// in-memory copy. Unmarking through one pane left the other still
    /// holding the label, and `Map::merge`'s never-erase rule meant that
    /// sibling's next save — guaranteed on quit, where every session is
    /// written unconditionally — put the mark straight back. The removal
    /// really did reach disk; the other character undid it on the way out,
    /// so it was back at the next launch.
    #[tokio::test]
    async fn unmarking_is_not_undone_by_another_character_on_the_same_world() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();

        // A room labelled in some earlier run, already on disk — so both
        // characters load it at connect time, which is how they really
        // both come to be holding it.
        let mut saved = crate::map::Map::default();
        put_room(&mut saved, 1, None, &[]);
        saved.set_mark(crate::map::RoomId(1), Some("shop".to_string()));
        config::save_map(
            dir.path(),
            "hercmud.net",
            &saved,
            &std::collections::HashSet::new(),
        )
        .unwrap();

        let (mut state, _rx) = app(&["mathias", "saihtam"]);
        state.config_dir = dir.path().to_path_buf();
        for session in &mut state.sessions {
            session.rules = (dir.path().to_path_buf(), None);
            session.map_key = "hercmud.net".to_string();
            session.current_room = Some(crate::map::RoomId(1));
        }
        state.world_mut(0).map = config::load_map(dir.path(), "hercmud.net");
        assert_eq!(
            state.map_of(1).unwrap().rooms[&crate::map::RoomId(1)]
                .mark
                .as_deref(),
            Some("shop"),
            "both characters read the one map, so both see the label"
        );

        // One of them takes it off.
        submit(&mut state, "/mark").await;
        let remove_row = state.mark_menu.as_ref().unwrap().entries().len() - 1;
        state.mark_menu.as_mut().unwrap().selected = remove_row;
        apply_mark_menu(&mut state);

        assert_eq!(
            state.map_of(1).unwrap().rooms[&crate::map::RoomId(1)].mark,
            None,
            "the other character reads the same map, so it cannot still hold it"
        );

        // Quitting: every session is written, siblings included. This is
        // the write that used to resurrect it.
        save_all_maps(&mut state);

        assert_eq!(
            config::load_map(dir.path(), "hercmud.net").rooms[&crate::map::RoomId(1)].mark,
            None,
            "the removal must survive the other character's save on the way out"
        );
    }

    /// A mark is something the player typed, so losing it to a crash would
    /// be worse than losing a room they can walk back into — worse still,
    /// a process that dies before the *next* periodic tick or a clean quit
    /// would lose it under the old deferred-save design. Marking now
    /// writes to disk immediately, so the map is clean again right away
    /// rather than merely flagged for later.
    #[tokio::test]
    async fn marking_a_room_saves_immediately_rather_than_waiting() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let (mut state, _rx) = app(&["tank"]);
        state.sessions[0].rules = (dir.path().to_path_buf(), Some("tank".to_string()));
        state.config_dir = dir.path().to_path_buf();
        state.sessions[0].map_key = "tank".to_string();
        put_room(state.map_of_mut(0), 1, None, &[]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));

        submit(&mut state, "/mark well").await;

        assert!(
            !state.world_mut(0).dirty,
            "the write already happened, so there is nothing left owed to a later tick"
        );
        assert_eq!(
            config::load_map(dir.path(), "tank").rooms[&crate::map::RoomId(1)]
                .mark
                .as_deref(),
            Some("well"),
            "the mark should be on disk without a periodic tick or a quit"
        );
    }

    /// The half of the fix that lives in `app.rs`: `config::save_map` can
    /// only apply a removal authoritatively if it is told which room to
    /// apply it to, and this is where that room gets named.
    #[tokio::test]
    async fn marking_and_removing_both_record_the_room_as_explicitly_touched() {
        let (mut state, _rx) = app(&["tank"]);
        put_room(state.map_of_mut(0), 1, None, &[]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));

        submit(&mut state, "/mark shop").await;
        assert!(
            state
                .world_mut(0)
                .explicit_marks
                .contains(&crate::map::RoomId(1)),
            "setting a mark should record the room"
        );

        // A save happened in between — the set is cleared once the mark
        // has actually reached disk.
        state.world_mut(0).explicit_marks.clear();

        state.mark_menu = Some(MarkMenu {
            at: crate::map::RoomId(1),
            selected: 0,
            existing: Some("shop".to_string()),
            typing: None,
        });
        // The last row is "remove" whenever the room already has a label.
        let remove_row = state.mark_menu.as_ref().unwrap().entries().len() - 1;
        state.mark_menu.as_mut().unwrap().selected = remove_row;
        apply_mark_menu(&mut state);

        assert_eq!(
            state.map_of(0).unwrap().rooms[&crate::map::RoomId(1)].mark,
            None,
            "the mark should be gone from the in-memory map"
        );
        assert!(
            state
                .world_mut(0)
                .explicit_marks
                .contains(&crate::map::RoomId(1)),
            "removing a mark should record the room just as setting one does"
        );
    }

    /// Opening the chooser is not itself a change: a stray `/mark` on a
    /// room you already labelled must not rub it out.
    #[tokio::test]
    async fn opening_the_chooser_changes_nothing_by_itself() {
        let (mut state, _rx) = app(&["tank"]);
        put_room(state.map_of_mut(0), 1, None, &[]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));
        submit(&mut state, "/mark well").await;

        submit(&mut state, "/mark").await;

        assert_eq!(
            state.map_of(0).unwrap().rooms[&crate::map::RoomId(1)]
                .mark
                .as_deref(),
            Some("well"),
            "the label survives until something is actually picked"
        );
        assert!(
            state
                .mark_menu
                .as_ref()
                .unwrap()
                .entries()
                .iter()
                .any(|e| e.contains("remove `well`")),
            "and taking it off is one of the offers"
        );
    }

    #[tokio::test]
    async fn mark_without_a_known_room_says_so() {
        let (mut state, _rx) = app(&["tank"]);

        submit(&mut state, "/mark shop").await;

        assert!(scrollback(&state.sessions[0]).contains("doesn't know where you are"));
    }

    /// `/mark` with nothing to write offers the usual answers rather than
    /// making the player remember what they called the last one.
    #[tokio::test]
    async fn bare_mark_opens_the_chooser() {
        let (mut state, _rx) = app(&["tank"]);
        put_room(state.map_of_mut(0), 1, None, &[]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));

        submit(&mut state, "/mark").await;

        let menu = state.mark_menu.as_ref().expect("the chooser is up");
        assert_eq!(menu.at, crate::map::RoomId(1));
        assert!(menu.entries().contains(&"shop".to_string()));
        assert!(
            menu.entries().iter().any(|e| e.contains("something else")),
            "and offers a label the list does not have: {:?}",
            menu.entries()
        );
        assert!(
            !menu.entries().iter().any(|e| e.starts_with("remove")),
            "nothing to remove from an unmarked room"
        );
    }

    #[tokio::test]
    async fn picking_a_row_marks_the_room() {
        let (mut state, _rx) = app(&["tank"]);
        put_room(state.map_of_mut(0), 1, None, &[]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));
        submit(&mut state, "/mark").await;

        // `2` is the second suggestion.
        let keys = crate::config::Keybinds::default();
        handle_key(
            &mut state,
            &keys,
            KeyCode::Char('2'),
            KeyModifiers::NONE,
            80,
            &[],
        );

        assert!(state.mark_menu.is_none(), "and closes behind itself");
        assert_eq!(
            state.map_of(0).unwrap().rooms[&crate::map::RoomId(1)]
                .mark
                .as_deref(),
            Some(MARK_SUGGESTIONS[1])
        );
    }

    /// The list cannot know every MUD, so it has a way out of itself.
    #[tokio::test]
    async fn the_chooser_takes_a_label_of_your_own() {
        let (mut state, _rx) = app(&["tank"]);
        put_room(state.map_of_mut(0), 1, None, &[]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));
        submit(&mut state, "/mark").await;
        let keys = crate::config::Keybinds::default();

        // Down to the "something else" row and take it.
        for _ in 0..MARK_SUGGESTIONS.len() {
            handle_key(
                &mut state,
                &keys,
                KeyCode::Down,
                KeyModifiers::NONE,
                80,
                &[],
            );
        }
        handle_key(
            &mut state,
            &keys,
            KeyCode::Enter,
            KeyModifiers::NONE,
            80,
            &[],
        );
        assert!(
            state.mark_menu.as_ref().unwrap().typing.is_some(),
            "asking for your own label opens a field rather than closing"
        );

        for ch in "grocer".chars() {
            handle_key(
                &mut state,
                &keys,
                KeyCode::Char(ch),
                KeyModifiers::NONE,
                80,
                &[],
            );
        }
        handle_key(
            &mut state,
            &keys,
            KeyCode::Enter,
            KeyModifiers::NONE,
            80,
            &[],
        );

        assert!(state.mark_menu.is_none());
        assert_eq!(
            state.map_of(0).unwrap().rooms[&crate::map::RoomId(1)]
                .mark
                .as_deref(),
            Some("grocer")
        );
    }

    /// Digits are text while a label is being typed, not row numbers.
    #[tokio::test]
    async fn digits_typed_into_a_custom_label_stay_in_it() {
        let (mut state, _rx) = app(&["tank"]);
        put_room(state.map_of_mut(0), 1, None, &[]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));
        submit(&mut state, "/mark").await;
        let keys = crate::config::Keybinds::default();
        state.mark_menu.as_mut().unwrap().typing = Some(String::new());

        for ch in "shop2".chars() {
            handle_key(
                &mut state,
                &keys,
                KeyCode::Char(ch),
                KeyModifiers::NONE,
                80,
                &[],
            );
        }

        assert_eq!(
            state.mark_menu.as_ref().unwrap().typing.as_deref(),
            Some("shop2")
        );
    }

    #[tokio::test]
    async fn the_chooser_offers_to_take_an_existing_mark_off() {
        let (mut state, _rx) = app(&["tank"]);
        put_room(state.map_of_mut(0), 1, None, &[]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));
        submit(&mut state, "/mark well").await;
        submit(&mut state, "/mark").await;
        let keys = crate::config::Keybinds::default();

        let last = state.mark_menu.as_ref().unwrap().entries().len() - 1;
        for _ in 0..last {
            handle_key(
                &mut state,
                &keys,
                KeyCode::Down,
                KeyModifiers::NONE,
                80,
                &[],
            );
        }
        handle_key(
            &mut state,
            &keys,
            KeyCode::Enter,
            KeyModifiers::NONE,
            80,
            &[],
        );

        assert_eq!(
            state.map_of(0).unwrap().rooms[&crate::map::RoomId(1)].mark,
            None
        );
    }

    #[tokio::test]
    async fn esc_leaves_the_room_as_it_was() {
        let (mut state, _rx) = app(&["tank"]);
        put_room(state.map_of_mut(0), 1, None, &[]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));
        submit(&mut state, "/mark").await;
        let keys = crate::config::Keybinds::default();

        handle_key(&mut state, &keys, KeyCode::Esc, KeyModifiers::NONE, 80, &[]);

        assert!(state.mark_menu.is_none());
        assert_eq!(
            state.map_of(0).unwrap().rooms[&crate::map::RoomId(1)].mark,
            None
        );
    }

    /// A trigger can write the label too, so a shop recognises itself the
    /// second time — the same shape as `corpse:`.
    #[tokio::test]
    async fn a_mark_trigger_labels_the_room_the_line_arrived_in() {
        let (mut state, _rx) = app(&["tank"]);
        apply_session_event(&mut state, 0, room(1, None));

        apply_session_event(&mut state, 0, SessionEvent::Mark("shop".to_string()));

        assert_eq!(
            state.map_of(0).unwrap().rooms[&crate::map::RoomId(1)]
                .mark
                .as_deref(),
            Some("shop")
        );
        assert!(state.world_mut(0).dirty);
    }

    // ---- /corpse (§16) ----

    /// The ordering the whole feature rests on: the death line arrives
    /// while the character is still standing in the room they died in, and
    /// the room death sends them to comes down the stream *behind* it. So
    /// the mark is taken from where they are now, and surviving the
    /// relocation that follows is the point.
    #[tokio::test]
    async fn a_corpse_trigger_marks_the_room_the_character_died_in() {
        let (mut state, _receivers) = app(&["tank"]);
        apply_session_event(&mut state, 0, room(1, None));

        apply_session_event(&mut state, 0, SessionEvent::Corpse);
        // Death drops them in the temple, which the mapper records as
        // usual — an arrival no movement predicted.
        apply_session_event(&mut state, 0, room(99, None));

        assert_eq!(state.sessions[0].corpse, Some(crate::map::RoomId(1)));
        assert_eq!(
            state.sessions[0].current_room,
            Some(crate::map::RoomId(99)),
            "the relocation still lands; only the mark is taken from before it"
        );
        assert!(
            scrollback(&state.sessions[0]).contains("corpse marked at #1"),
            "{}",
            scrollback(&state.sessions[0])
        );
    }

    /// A death on a MUD with no room data has nothing to remember. Saying
    /// so beats recording nothing and letting `/corpse` later claim no
    /// death ever happened.
    #[tokio::test]
    async fn a_corpse_trigger_with_no_known_room_says_so() {
        let (mut state, _receivers) = app(&["tank"]);

        apply_session_event(&mut state, 0, SessionEvent::Corpse);

        assert!(state.sessions[0].corpse.is_none());
        assert!(
            scrollback(&state.sessions[0]).contains("doesn't know where you died"),
            "{}",
            scrollback(&state.sessions[0])
        );
    }

    /// The payoff: one command, no remembering, and the same one-step-at-a-
    /// time walk `/goto` does — a corpse run through a world that has moved
    /// on is exactly where firing a whole route blind goes wrong.
    #[tokio::test]
    async fn corpse_walks_back_to_the_marked_room_one_step_at_a_time() {
        let (mut state, mut receivers) = app(&["tank"]);
        put_room(state.map_of_mut(0), 1, Some("Dark Alley"), &[]);
        put_room(state.map_of_mut(0), 99, Some("Temple"), &[("s", 2)]);
        put_room(state.map_of_mut(0), 2, Some("Street"), &[("w", 1)]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));
        apply_session_event(&mut state, 0, SessionEvent::Corpse);
        state.sessions[0].current_room = Some(crate::map::RoomId(99));

        submit(&mut state, "/corpse").await;

        assert!(
            matches!(receivers[0].try_recv(), Ok(SessionCommand::SendLine(line)) if line == "s"),
            "the first step should be sent immediately"
        );
        assert!(
            receivers[0].try_recv().is_err(),
            "the second step waits for the first to be confirmed"
        );
        assert_eq!(
            state.sessions[0].walk.as_ref().map(|walk| walk.destination),
            Some(crate::map::RoomId(1))
        );
    }

    /// Distinguished from "no known route" on purpose: this player needs to
    /// be told to write the trigger, not to go explore.
    #[tokio::test]
    async fn corpse_without_a_recorded_death_says_no_death_was_recorded() {
        let (mut state, mut receivers) = app(&["tank"]);
        put_room(state.map_of_mut(0), 1, None, &[]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));

        submit(&mut state, "/corpse").await;

        assert!(state.sessions[0].walk.is_none());
        assert!(receivers[0].try_recv().is_err());
        let text = scrollback(&state.sessions[0]);
        assert!(
            text.contains("no death recorded") && text.contains("corpse: true"),
            "{text}"
        );
    }

    /// The refusals `walk_to` shares with `/goto` have to blame the command
    /// the player actually typed.
    #[tokio::test]
    async fn corpse_with_no_route_back_blames_corpse_and_not_goto() {
        let (mut state, _receivers) = app(&["tank"]);
        put_room(state.map_of_mut(0), 1, None, &[]);
        put_room(state.map_of_mut(0), 99, None, &[]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));
        apply_session_event(&mut state, 0, SessionEvent::Corpse);
        state.sessions[0].current_room = Some(crate::map::RoomId(99));

        submit(&mut state, "/corpse").await;

        assert!(state.sessions[0].walk.is_none());
        assert!(
            scrollback(&state.sessions[0]).contains("/corpse: no known route there"),
            "{}",
            scrollback(&state.sessions[0])
        );
    }

    /// Reaching the body leaves the mark in place: a player still ferrying
    /// loot back and forth should not have to die again to get it back.
    #[tokio::test]
    async fn walking_back_to_the_corpse_leaves_the_mark_in_place() {
        let (mut state, _receivers) = app(&["tank"]);
        put_room(state.map_of_mut(0), 1, None, &[]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));
        apply_session_event(&mut state, 0, SessionEvent::Corpse);

        submit(&mut state, "/corpse").await;

        assert_eq!(state.sessions[0].corpse, Some(crate::map::RoomId(1)));
        assert!(
            scrollback(&state.sessions[0]).contains("/corpse: already there"),
            "{}",
            scrollback(&state.sessions[0])
        );
    }

    // ---- /connect (§7.5, ARCH_REVIEW.md "Features that would break the architecture") ----

    fn write_profile(dir: &std::path::Path, name: &str) {
        std::fs::create_dir_all(dir.join("profiles")).unwrap();
        std::fs::write(
            dir.join("profiles").join(format!("{name}.yaml")),
            format!("name: {name}\nhost: 127.0.0.1\nport: 1\n"),
        )
        .unwrap();
    }

    /// The two directions the peer mesh being built once before
    /// `event_loop` and never revisited had made impossible: the new
    /// session is reachable by name (`peer_registry`), and every session
    /// already running is told about it.
    #[tokio::test]
    async fn connect_adds_a_session_and_notifies_existing_ones() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        write_profile(dir.path(), "cleric");

        let (mut state, mut receivers) = app(&["tank"]);
        state.config_dir = dir.path().to_path_buf();

        submit(&mut state, "/connect cleric").await;

        assert_eq!(state.sessions.len(), 2);
        assert_eq!(state.sessions[1].name, "cleric");
        assert_eq!(
            state.focus,
            Focus::Session(1),
            "a connected character is focused immediately, like a launched one"
        );
        assert_eq!(state.input_session, 1);
        assert!(state.peer_registry.contains_key("cleric"));

        match receivers[0].try_recv() {
            Ok(SessionCommand::AddPeer { name, .. }) => assert_eq!(name, "cleric"),
            other => panic!("expected AddPeer for tank, got {other:?}"),
        }
    }

    /// A repeated profile name still gets the same `-2` suffix a second
    /// `mudular tank tank` on the command line would (§7.5).
    #[tokio::test]
    async fn connect_to_a_taken_name_gets_a_suffix() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        write_profile(dir.path(), "tank");

        let (mut state, _rx) = app(&["tank"]);
        state.config_dir = dir.path().to_path_buf();

        submit(&mut state, "/connect tank").await;

        assert_eq!(state.sessions.len(), 2);
        assert_eq!(state.sessions[1].name, "tank-2");
    }

    #[tokio::test]
    async fn connect_to_an_unknown_profile_shows_an_error_and_adds_nothing() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        std::fs::create_dir_all(dir.path().join("profiles")).unwrap();

        let (mut state, _rx) = app(&["tank"]);
        state.config_dir = dir.path().to_path_buf();

        submit(&mut state, "/connect ghost").await;

        assert_eq!(state.sessions.len(), 1, "no session should have been added");
        assert!(
            scrollback(&state.sessions[0]).contains("could not connect"),
            "{:?}",
            scrollback(&state.sessions[0])
        );
    }

    #[tokio::test]
    async fn connect_with_an_empty_name_shows_an_error() {
        let (mut state, _rx) = app(&["tank"]);

        submit(&mut state, "/connect ").await;

        assert_eq!(state.sessions.len(), 1);
        assert!(
            scrollback(&state.sessions[0]).contains("needs a profile name"),
            "{:?}",
            scrollback(&state.sessions[0])
        );
    }
}
