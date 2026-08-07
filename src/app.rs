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

use std::collections::VecDeque;
use std::path::PathBuf;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::DefaultTerminal;
use tokio::sync::mpsc;
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;

use ratatui::style::Color;

use crate::config::{Channel, CrossSession, Keybinds};
use crate::engine::Engine;
use crate::proto::charset::Charset;
use crate::session::{self, SessionCommand, SessionEvent};
use crate::ui;

/// Bounded so a chatty MUD can't grow the buffer without limit
/// (docs/ARCHITECTURE.md §8; a fuller ring buffer with disk logging is M9).
const SCROLLBACK_LIMIT: usize = 10_000;

/// Same rationale as `SCROLLBACK_LIMIT`, for the raw GMCP inspector log.
const GMCP_LOG_LIMIT: usize = 1_000;

/// The client-side commands. Everything else starting with `/` is left
/// alone, since plenty of MUDs use `/` for their own commands.
const RELOAD_COMMAND: &str = "/reload";
/// The same listing as the overlay, for players who never find the key
/// (docs/ARCHITECTURE.md §11.2).
const HELP_COMMAND: &str = "/help";

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
    cross: CrossSession,
    /// Last pane size reported to the server, so a redraw that did not
    /// change this pane sends no NAWS (docs/ARCHITECTURE.md §6.2).
    last_size: Option<(u16, u16)>,
}

impl SessionPane {
    fn push_line(&mut self, line: String) {
        self.scrollback.push_back(line);
        if self.scrollback.len() > SCROLLBACK_LIMIT {
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
}

impl ChannelPane {
    fn push(&mut self, line: String) {
        self.lines.push_back(line);
        if self.lines.len() > SCROLLBACK_LIMIT {
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
    /// Whether the focused session's pane shows `gmcp_log` instead of its
    /// scrollback.
    pub show_gmcp: bool,
    /// Whether the help overlay is up (§11.2).
    pub show_help: bool,
    /// The live bindings, so the input hint and the help overlay both read
    /// what the event loop actually matches against — never a second copy
    /// that can drift (§11.2).
    pub keybinds: Keybinds,
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
    fn route_send_to(
        &mut self,
        from: usize,
        target: &str,
        lines: Vec<String>,
        hops: u8,
    ) -> Vec<(usize, SessionCommand)> {
        let matched: Vec<usize> = if target == ALL_SESSIONS {
            (0..self.sessions.len()).filter(|&i| i != from).collect()
        } else {
            self.sessions
                .iter()
                .position(|session| session.name == target)
                .filter(|&index| index != from)
                .into_iter()
                .collect()
        };

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

/// `HH:MM:SS` for channel panes that ask for timestamps. Seconds since the
/// epoch is all `std::time` offers, so the clock arithmetic is done here
/// rather than pulling in a date library for one format (§2.1).
fn timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let day = secs % 86_400;
    format!("{:02}:{:02}:{:02}", day / 3600, (day % 3600) / 60, day % 60)
}

/// Applies one session event to `state`. Returns the injections the hub
/// should deliver, and whether the session ended (so the caller can stop
/// polling its receiver).
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
        SessionEvent::Prompt(text) => {
            state.sessions[index].prompt = text;
            (false, Vec::new())
        }
        SessionEvent::EchoMask(masked) => {
            state.sessions[index].masked = masked;
            (false, Vec::new())
        }
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
        SessionEvent::Ended(reason) => {
            let session = &mut state.sessions[index];
            session.status = format!("disconnected: {reason}");
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

pub async fn run(
    targets: Vec<ConnectTarget>,
    keybinds: Keybinds,
    channels: Vec<Channel>,
    history_size: usize,
) -> Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, targets, keybinds, channels, history_size).await;
    ratatui::restore();
    result
}

fn connect(target: ConnectTarget, history_limit: usize) -> SessionPane {
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
    );
    SessionPane {
        name: target.name,
        scrollback: VecDeque::new(),
        prompt: String::new(),
        input: Input::default(),
        status,
        masked: false,
        security: String::new(),
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
        cross: target.cross,
        last_size: None,
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
) -> Result<()> {
    let sessions: Vec<SessionPane> = targets
        .into_iter()
        .map(|target| connect(target, history_size))
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
            })
            .collect(),
        focus: Focus::Session(0),
        input_session: 0,
        layout: LayoutMode::Tabs,
        show_channels: !channels.is_empty(),
        show_gmcp: false,
        show_help: false,
        keybinds: keybinds.clone(),
    };

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
                if handle_key(&mut state, &keybinds, key.code, key.modifiers) {
                    report_pane_sizes(&mut state, terminal.get_frame().area()).await;
                } else if key.code == KeyCode::Enter {
                    submit_input(&mut state, &channels).await;
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
                report_pane_sizes(&mut state, area).await;
            }
            Wake::Terminal(Some(Ok(_))) => {}
            Wake::Terminal(Some(Err(err))) => return Err(err.into()),
            Wake::Terminal(None) => return Ok(()),
            Wake::Session(index, ev) => {
                let mut injections = Vec::new();
                match ev {
                    Some(ev) => {
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
fn handle_key(
    state: &mut AppState,
    keybinds: &Keybinds,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> bool {
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
                cross: CrossSession::default(),
                last_size: None,
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
                show_gmcp: false,
                show_help: false,
                keybinds: Keybinds::default(),
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
            &keybinds.help,
        ] {
            assert!(
                listing.contains(&binding.to_string()),
                "{binding} missing from:\n{listing}"
            );
        }
        // The built-ins and client commands are just as invisible otherwise.
        for text in ["Alt+1", "Up / Down", "/reload", "/help"] {
            assert!(listing.contains(text), "{text} missing from:\n{listing}");
        }
        // An unimplemented binding is shown as such rather than omitted.
        assert!(listing.contains("not implemented yet"), "{listing}");
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
        handle_key(state, &Keybinds::default(), code, modifiers)
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

    /// Hiding the channel column while a channel pane has focus must move
    /// focus back to a pane that is still drawn.
    #[test]
    fn hiding_the_channels_takes_focus_off_them() {
        let (mut state, _rx) = app(&["tank"]);
        state.channels.push(ChannelPane {
            config: channel("comms"),
            lines: VecDeque::new(),
            unread: 0,
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
}
