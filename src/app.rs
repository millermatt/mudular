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

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::DefaultTerminal;
use tokio::sync::{mpsc, watch};
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;

use ratatui::style::Color;

use crate::config::{self, Channel, CrossSession, Keybinds};
use crate::engine::{Engine, PeerSnapshot};
use crate::proto::charset::Charset;
use crate::session::{self, PeerLinks, SessionCommand, SessionEvent};
use crate::ui;

/// Same rationale as `scrollback_size` (§8), for the raw GMCP inspector log:
/// bounded so a chatty MUD can't grow it without limit.
const GMCP_LOG_LIMIT: usize = 1_000;

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
    pub scrollback: VecDeque<String>,
    /// Text pinned above the input line; empty means no prompt.
    pub prompt: String,
    pub input: Input,
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
    /// Raw `Package payload` lines, newest last — the GMCP inspector view
    /// (docs/ARCHITECTURE.md §14 M6).
    pub gmcp_log: VecDeque<String>,
    /// Lines that arrived while the pane was not focused (§11).
    pub unread: usize,
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
}

impl SessionPane {
    fn push_line(&mut self, line: String) {
        if let Some(log) = &mut self.log {
            use std::io::Write as _;
            // Best-effort: a full disk or a yanked log file must not take
            // the session down. Silently stop trying rather than repeat a
            // failing write every line.
            if writeln!(log, "{line}").and_then(|()| log.flush()).is_err() {
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

    fn push_gmcp(&mut self, package: String, payload: Option<String>) {
        let line = match payload {
            Some(payload) => format!("{package} {payload}"),
            None => package,
        };
        self.gmcp_log.push_back(line);
        if self.gmcp_log.len() > GMCP_LOG_LIMIT {
            self.gmcp_log.pop_front();
        }
    }
}

/// A channel pane: app-level, aggregating matching lines from every session
/// that isn't excluded by `session:` (docs/ARCHITECTURE.md §11.1).
pub struct ChannelPane {
    pub config: Channel,
    pub lines: VecDeque<String>,
    pub unread: usize,
    /// How many lines `lines` keeps (`scrollback_size`, §8).
    pub scrollback_limit: usize,
    /// Same "distance from the tail" scroll state as `SessionPane`, and the
    /// same sticky-bottom behaviour (§11.5).
    pub back_offset: usize,
}

impl ChannelPane {
    fn push(&mut self, line: String) {
        self.lines.push_back(line);
        if self.lines.len() > self.scrollback_limit {
            self.lines.pop_front();
        }
    }
}

/// Which pane has focus. Channel panes are focusable, but the input line
/// stays bound to `input_session` — reading comms must never silently
/// change which character your commands go to (§11.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Session(usize),
    Channel(usize),
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
    /// Whether the focused session's pane shows `gmcp_log` instead of its
    /// scrollback.
    pub show_gmcp: bool,
    /// Whether the help overlay is up (§11.2).
    pub show_help: bool,
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
    /// A scrollback line-cursor is active on the focused pane (§10.2/§11.5):
    /// `Some(back_offset)` is the line it currently highlights, measured the
    /// same way `SessionPane::back_offset` is — distance from the tail.
    pub line_cursor: Option<usize>,
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
        let total = sessions + channels;
        if total == 0 {
            return;
        }
        let current = match self.focus {
            Focus::Session(index) => index,
            Focus::Channel(index) => sessions + index,
        };
        let next = (current + 1) % total;
        self.focus_pane(if next < sessions {
            Focus::Session(next)
        } else {
            Focus::Channel(next - sessions)
        });
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
            Focus::Channel(_) => self.input_session == index,
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
    /// when more than one character is active (§11.1).
    fn push_routed(&mut self, from: usize, channel: &str, text: String) {
        let tag = self.aggregating().then(|| self.sessions[from].name.clone());
        let Some(index) = self.channel_index(channel) else {
            return;
        };
        let focused = self.focus == Focus::Channel(index);
        let pane = &mut self.channels[index];
        let mut line = String::new();
        if pane.config.timestamps {
            line.push_str(&timestamp());
            line.push(' ');
        }
        if let Some(tag) = tag {
            line.push_str(&format!("[{tag}] "));
        }
        line.push_str(&text);
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
            self.sessions[from].push_line(notice);
            return;
        }

        let name = self.sessions[from].name.clone();
        for index in matched {
            self.sessions[index].push_line(format!("[from {name}] {text}"));
        }
    }

    fn route_send_to(
        &mut self,
        from: usize,
        target: &str,
        lines: Vec<String>,
        hops: u8,
    ) -> Vec<(usize, SessionCommand)> {
        let matched = self.addressed(from, target);

        if matched.is_empty() {
            let notice = format!("** send_to: no session named `{target}`");
            self.sessions[from].push_line(notice);
            return Vec::new();
        }

        let name = self.sessions[from].name.clone();
        let mut out = Vec::new();
        let mut notices = Vec::new();
        for index in matched {
            let session = &self.sessions[index];
            if !session.connected {
                notices.push(format!(
                    "** send_to `{}`: not connected, dropped {} command(s)",
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
                    "** send_to `{}`: hop limit ({}) reached, dropped",
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
            self.sessions[from].push_line(notice);
        }
        out
    }
}

/// `HH:MM:SS` in the player's local timezone for channel panes that ask
/// for timestamps — a UTC clock silently mislabeled as local would be
/// wrong for most players, every day, all year.
fn timestamp() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
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
/// kitty, foot, …), an OSC 9 desktop notification. Best-effort: a write
/// failure here is not worth ending the session over.
fn notify_unfocused(session_name: &str) {
    use std::io::Write as _;
    let mut stdout = std::io::stdout();
    let _ = write!(stdout, "\x07\x1b]9;{session_name} has new output\x07");
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
        SessionEvent::Line(line) => {
            let session = &mut state.sessions[index];
            session.push_line(line);
            if !focused {
                session.unread += 1;
            }
            (false, Vec::new())
        }
        SessionEvent::Route { channel, text } => {
            state.push_routed(index, &channel, text);
            (false, Vec::new())
        }
        SessionEvent::SendTo {
            target,
            lines,
            hops,
        } => (false, state.route_send_to(index, &target, lines, hops)),
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
                session.push_line(
                    "** not saved: that password was rejected, so there is \
                     nothing worth keeping"
                        .to_string(),
                );
            }
            (false, Vec::new())
        }
        // The hub decides whether to actually ring it, from the caller's
        // own focus check — this function stays a pure state update.
        SessionEvent::Bell => (false, Vec::new()),
        SessionEvent::Gmcp { package, payload } => {
            state.sessions[index].push_gmcp(package, payload);
            (false, Vec::new())
        }
        SessionEvent::Security(security) => {
            let session = &mut state.sessions[index];
            session.security = security.label;
            // §13 requires an insecure connection (or a newly pinned
            // certificate) to be visible, not just implied by a label.
            if let Some(warning) = security.warning {
                session.push_line(format!("** {warning}"));
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
            session.push_line(format!("** disconnected: {reason}"));
            session.latency.clear();
            session.masked = false;
            session.connected = false;
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
            session.push_line(
                "** /config needs a profile session — this one was started with --host".to_string(),
            );
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
                session.push_line(format!("** could not open the profile editor: {err:#}"));
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
    let pattern = regex::escape(&ui::plain_text(raw));
    let (dir, profile) = session.rules.clone();
    let Some(name) = profile else {
        if let Some(session) = state.bound_mut() {
            session.push_line(
                "** picking a trigger needs a profile session — this one was started with \
                 --host"
                    .to_string(),
            );
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
                session.push_line(format!("** could not open the profile editor: {err:#}"));
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
                    state.sessions[index].push_line(text);
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
enum WizardStep {
    Name,
    Host,
    Port,
    Tls,
}

impl WizardStep {
    fn prompt(self) -> &'static str {
        match self {
            WizardStep::Name => "Character/profile name",
            WizardStep::Host => "Host",
            WizardStep::Port => "Port (blank for 23)",
            WizardStep::Tls => "Use TLS? (y/N)",
        }
    }
}

/// The first-run "create a profile" form (docs/ARCHITECTURE.md §15): shown
/// when nothing was given on the command line and no profile exists yet,
/// so a first connection needs no hand-edited YAML. Runs its own terminal
/// session before any of `event_loop`'s — there is no session to drive it
/// yet, and threading a would-be session through a loop built to manage
/// live ones would be the tail wagging the dog. `Ok(None)` means the
/// player cancelled with Esc.
pub async fn run_new_profile_wizard() -> Result<Option<config::NewProfile>> {
    let mut terminal = ratatui::init();
    let result = new_profile_event_loop(&mut terminal).await;
    ratatui::restore();
    result
}

async fn new_profile_event_loop(
    terminal: &mut DefaultTerminal,
) -> Result<Option<config::NewProfile>> {
    let mut step = WizardStep::Name;
    let mut name = String::new();
    let mut host = String::new();
    let mut port: u16 = 23;
    let mut answered: Vec<(&str, String)> = Vec::new();
    let mut input = Input::default();
    let mut error: Option<String> = None;
    let mut term_events = EventStream::new();

    loop {
        terminal.draw(|frame| {
            ui::draw_new_profile_wizard(
                frame,
                &answered,
                step.prompt(),
                input.value(),
                input.visual_cursor(),
                error.as_deref(),
            )
        })?;

        let Some(Ok(Event::Key(key))) = term_events.next().await else {
            return Ok(None);
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if key.code == KeyCode::Esc {
            return Ok(None);
        }
        if key.code != KeyCode::Enter {
            input.handle_event(&Event::Key(key));
            continue;
        }

        let value = input.value().trim().to_string();
        error = match step {
            WizardStep::Name if value.is_empty() || value.contains(['/', '\\']) => {
                Some("a profile name can't be empty or contain a slash".to_string())
            }
            WizardStep::Name => {
                name = value.clone();
                answered.push(("Name", value));
                step = WizardStep::Host;
                None
            }
            WizardStep::Host if value.is_empty() => Some("a host is required".to_string()),
            WizardStep::Host => {
                host = value.clone();
                answered.push(("Host", value));
                step = WizardStep::Port;
                None
            }
            WizardStep::Port if value.is_empty() => {
                answered.push(("Port", "23".to_string()));
                step = WizardStep::Tls;
                None
            }
            WizardStep::Port => match value.parse() {
                Ok(parsed) => {
                    port = parsed;
                    answered.push(("Port", value));
                    step = WizardStep::Tls;
                    None
                }
                Err(_) => Some("port must be a number from 1-65535".to_string()),
            },
            WizardStep::Tls => {
                let tls = matches!(value.to_ascii_lowercase().as_str(), "y" | "yes");
                return Ok(Some(config::NewProfile {
                    name,
                    host,
                    port,
                    tls,
                }));
            }
        };
        input = Input::default();
    }
}

pub async fn run(
    targets: Vec<ConnectTarget>,
    keybinds: Keybinds,
    channels: Vec<Channel>,
    history_size: usize,
    scrollback_size: usize,
    channel_width: u16,
) -> Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(
        &mut terminal,
        targets,
        keybinds,
        channels,
        history_size,
        scrollback_size,
        channel_width,
    )
    .await;
    ratatui::restore();
    result
}

fn connect(
    target: ConnectTarget,
    history_limit: usize,
    scrollback_limit: usize,
    peers: PeerLinks,
) -> SessionPane {
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
    SessionPane {
        name: target.name,
        scrollback: VecDeque::new(),
        prompt: String::new(),
        input: Input::default(),
        status,
        masked: false,
        security: String::new(),
        latency: String::new(),
        connected: true,
        gmcp_log: VecDeque::new(),
        unread: 0,
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
    }
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
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    targets: Vec<ConnectTarget>,
    keybinds: Keybinds,
    channels: Vec<Channel>,
    history_size: usize,
    scrollback_size: usize,
    channel_width: u16,
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
            connect(target, history_size, scrollback_size, links)
        })
        .collect();
    let has_sessions = !sessions.is_empty();

    let mut state = AppState {
        sessions,
        channels: channels
            .iter()
            .map(|config| ChannelPane {
                config: config.clone(),
                lines: VecDeque::new(),
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
        show_gmcp: false,
        show_help: false,
        keybinds: keybinds.clone(),
        config_editor: None,
        config_editor_save: None,
        line_cursor: None,
    };
    // A config value wider than this terminal is clamped before the first
    // frame, the same as a `Resize` clamps it later (§11.4).
    state.channel_width =
        ui::clamp_channel_width(state.channel_width, terminal.get_frame().area().width);

    if !has_sessions {
        // Nothing to drive the loop but the terminal; the empty-state help
        // lives in the UI, which has no pane to draw it in otherwise.
        state.show_channels = false;
    }

    report_pane_sizes(&mut state, terminal.get_frame().area()).await;

    let mut term_events = EventStream::new();

    loop {
        terminal.draw(|frame| ui::draw(frame, &state))?;

        let wake = tokio::select! {
            ev = term_events.next() => Wake::Terminal(ev),
            (index, ev) = next_session_event(&mut state.sessions) => Wake::Session(index, ev),
        };

        match wake {
            Wake::Terminal(Some(Ok(Event::Key(key)))) if key.kind == KeyEventKind::Press => {
                if keybinds.quit.matches(key.code, key.modifiers) {
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
                    report_pane_sizes(&mut state, terminal.get_frame().area()).await;
                } else if key.code == KeyCode::Enter {
                    submit_input(&mut state, &channels).await;
                } else if is_scroll_key(key.code, key.modifiers) {
                    // Scrollback keys are built-in and unremappable, like
                    // Up/Down below — and they act on the *focused* pane,
                    // not the input-bound session, so a focused channel pane
                    // scrolls even while typing stays bound elsewhere
                    // (§11.1, §11.5). This changes pane content, not size,
                    // so no NAWS report follows.
                    state.scroll_focused(key.code);
                } else if let Some(session) = state.bound_mut() {
                    // Up/Down are built-in and unremappable (§11.3): on a
                    // single-line input they have no other meaning, and
                    // they are the one binding every user arrives knowing.
                    let walked = match key.code {
                        KeyCode::Up if key.modifiers.is_empty() => session.walk_history(true),
                        KeyCode::Down if key.modifiers.is_empty() => session.walk_history(false),
                        _ => false,
                    };
                    if !walked {
                        session.input.handle_event(&Event::Key(key));
                    }
                }
            }
            Wake::Terminal(Some(Ok(Event::Resize(cols, rows)))) => {
                let area = ratatui::layout::Rect::new(0, 0, cols, rows);
                // A narrower terminal can leave the column wider than the
                // session area can spare, so the width re-clamps on every
                // resize before the sizes are reported (§11.4).
                state.channel_width = ui::clamp_channel_width(state.channel_width, area.width);
                report_pane_sizes(&mut state, area).await;
            }
            Wake::Terminal(Some(Ok(_))) => {}
            Wake::Terminal(Some(Err(err))) => return Err(err.into()),
            Wake::Terminal(None) => return Ok(()),
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
                    notify_unfocused(&state.sessions[index].name);
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
    // The scrollback line-cursor also owns the keyboard while active: it is
    // only a few keys (§10.2/§11.5), so they're handled directly here rather
    // than through a second state-machine type. `back_offset` is kept in
    // step with the cursor so the highlighted line (drawn in `ui::draw`)
    // never scrolls out of view — the two count different things in
    // general (wrapped rows vs. raw scrollback entries), but line-cursor
    // moves one raw entry at a time, so pinning `back_offset` to the same
    // number keeps the picked line on screen for the ordinary case of
    // unwrapped MUD output.
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
    if state.show_help {
        state.show_help = false;
        return true;
    }
    if keybinds.help.matches(code, modifiers) {
        state.show_help = true;
        return true;
    }
    if keybinds.config_editor.matches(code, modifiers) {
        open_config_editor(state, channels);
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
    if keybinds.gmcp_inspector.matches(code, modifiers) {
        state.show_gmcp = !state.show_gmcp;
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
    if keybinds.toggle_channels.matches(code, modifiers) && !state.channels.is_empty() {
        state.show_channels = !state.show_channels;
        // Focus must never rest on a pane that is no longer drawn.
        if !state.show_channels && matches!(state.focus, Focus::Channel(_)) {
            state.focus_pane(Focus::Session(state.input_session));
        }
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
    // Alt+1..9 jumps straight to a session (§11).
    if modifiers.contains(KeyModifiers::ALT)
        && let KeyCode::Char(c) = code
        && let Some(n) = c.to_digit(10).filter(|&n| n >= 1)
    {
        let index = n as usize - 1;
        if index < state.sessions.len() {
            state.focus_pane(Focus::Session(index));
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
    session.push_line(line);
}

/// Sends the bound session's input line. Always the bound session, never
/// the focused pane: focusing comms must not redirect commands (§11.1).
async fn submit_input(state: &mut AppState, channels: &[Channel]) {
    let Some(session) = state.bound_mut() else {
        return;
    };
    let line = session.input.value().to_string();
    session.input.reset();
    session.push_history(&line);
    // A bare Enter is a keystroke in its own right — MUD login flows and
    // pagers ask for one ("press return to continue") — so it goes to the
    // server rather than being swallowed as "nothing typed". It is not
    // echoed: a lone `>` in the scrollback would be noise, and the server's
    // own response is the feedback that matters.
    if !line.is_empty() && !session.masked {
        // Never echo what the server is masking.
        session.push_line(format!("> {line}"));
    }
    // A masked line at a login the profile wants automated is the password
    // it is missing (§13). Offer to keep it rather than making the player
    // find `--set-password`; it is held in memory only until they answer.
    if session.offer_password_save && session.masked && !line.is_empty() {
        session.offer_password_save = false;
        session.pending_password = Some(line.clone());
        let profile = session.rules.1.clone().unwrap_or_default();
        session.push_line(format!(
            "** Save this password in the OS keyring for `{profile}`, \
             so it logs you in next time? (y/n)"
        ));
    }
    if line.trim() == HELP_COMMAND {
        let lines = ui::help_lines(&state.keybinds);
        if let Some(session) = state.bound_mut() {
            for line in lines {
                session.push_line(line);
            }
        }
        return;
    }
    if line.trim() == RELOAD_COMMAND {
        let notice = reload_rules(state, channels).await;
        if let Some(session) = state.bound_mut() {
            session.push_line(notice);
        }
        return;
    }
    if line.trim() == CONFIG_COMMAND {
        open_config_editor(state, channels);
        return;
    }
    let _ = session.commands.send(SessionCommand::SendLine(line)).await;
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
                gmcp_log: VecDeque::new(),
                unread: 0,
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
                channels: Vec::new(),
                focus: Focus::Session(0),
                input_session: 0,
                layout: LayoutMode::Tabs,
                show_channels: false,
                channel_width: crate::ui::CHANNEL_WIDTH,
                show_gmcp: false,
                show_help: false,
                keybinds: Keybinds::default(),
                config_editor: None,
                config_editor_save: None,
                line_cursor: None,
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
            matches: Vec::new(),
            keep_in_main: false,
            timestamps: false,
            session: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{app_with_receivers as app, channel};
    use super::*;
    use crate::net::Security;

    fn scrollback(session: &SessionPane) -> String {
        session
            .scrollback
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// `timestamp()` must stay `HH:MM:SS` — a regression back to hand-rolled
    /// arithmetic on Unix-epoch seconds (as opposed to `chrono::Local`)
    /// would silently mislabel a UTC clock as local time again.
    #[test]
    fn timestamp_is_hh_mm_ss() {
        let stamp = timestamp();
        let parts: Vec<&str> = stamp.split(':').collect();
        assert_eq!(parts.len(), 3, "{stamp}");
        for part in parts {
            assert_eq!(part.len(), 2, "{stamp}");
            assert!(part.chars().all(|c| c.is_ascii_digit()), "{stamp}");
        }
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
        assert!(!wants_bell(
            &state,
            1,
            &SessionEvent::Line("hi".to_string())
        ));
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
            session.push_line(format!("> {line}"));
        }
        assert!(session.scrollback.is_empty());

        session.masked = false;
        session.push_line("> look".to_string());
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
            session.push_line(format!("> {line}"));
        }

        session.masked = false;
        session.push_line("> look".to_string());
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

        session.push_line("You see a rat.".to_string());
        session.push_line("You swing your sword.".to_string());
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
        assert_eq!(state.sessions[0].gmcp_log.len(), 1);
        assert_eq!(state.sessions[0].gmcp_log[0], r#"Char.Vitals {"hp":100}"#);
        assert!(
            state.sessions[0].scrollback.is_empty(),
            "GMCP must not reach scrollback"
        );
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

        apply_session_event(&mut state, 0, SessionEvent::Line("tank sees this".into()));
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
        apply_session_event(&mut state, 1, SessionEvent::Line("cleric sees this".into()));

        assert_eq!(scrollback(&state.sessions[0]), "tank sees this");
        assert_eq!(scrollback(&state.sessions[1]), "cleric sees this");
        assert_eq!(state.sessions[0].prompt, "HP:100>");
        assert!(state.sessions[1].prompt.is_empty());
        assert!(state.sessions[0].masked);
        assert!(!state.sessions[1].masked, "echo masking must not spread");
        assert_eq!(state.sessions[0].gmcp_log.len(), 1);
        assert!(state.sessions[1].gmcp_log.is_empty());
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

        apply_session_event(&mut state, 1, SessionEvent::Line("a tell arrives".into()));
        apply_session_event(&mut state, 1, SessionEvent::Line("and another".into()));
        apply_session_event(&mut state, 0, SessionEvent::Line("focused output".into()));

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

        assert_eq!(state.channels[0].lines[0], "[cleric] Bob tells you hi");
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

        assert_eq!(state.channels[0].lines[0], "Bob tells you hi");
    }

    #[test]
    fn a_timestamped_channel_prefixes_the_clock() {
        let (mut state, _rx) = app(&["tank"]);
        with_channel(&mut state, "comms", true);

        apply_session_event(
            &mut state,
            0,
            SessionEvent::Route {
                channel: "comms".into(),
                text: "Bob tells you hi".into(),
            },
        );

        let line = &state.channels[0].lines[0];
        assert!(line.ends_with("Bob tells you hi"), "{line}");
        assert_eq!(line.len(), "00:00:00 Bob tells you hi".len(), "{line}");
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
        assert_eq!(
            state.sessions[1].scrollback.back().map(String::as_str),
            Some("[from tank] he is about to fall")
        );
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
        assert!(notice.contains("ghost"), "{notice}");
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
            &keybinds.gmcp_inspector,
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

        let printed = state.sessions[0]
            .scrollback
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
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
            format!("name: combat\naliases:\n  - pattern: '^x$'\n    send: [\"{send}\"]\n"),
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

    #[tokio::test]
    async fn reload_picks_up_edited_rules() {
        let dir = config_with_alias("old");
        let (state, mut receivers) = app_with_rules(dir.path());

        // Edit the module on disk, exactly as a player would.
        std::fs::write(
            dir.path().join("modules/combat.yaml"),
            "name: combat\naliases:\n  - pattern: '^x$'\n    send: [\"new\"]\n",
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
            "name: combat\naliases:\n  - pattern: '('\n",
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
            state.sessions[0].push_line(line.to_string());
        }

        assert_eq!(
            state.sessions[0].scrollback.iter().collect::<Vec<_>>(),
            ["two", "three"]
        );
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

        assert_eq!(
            state.channels[0].lines.iter().collect::<Vec<_>>(),
            ["two", "three"]
        );
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

        apply_session_event(&mut state, 0, SessionEvent::Line("more output".into()));

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

        apply_session_event(&mut state, 0, SessionEvent::Line("more output".into()));

        assert_eq!(state.sessions[0].back_offset, 0);
    }
}
