//! UI event loop: owns the terminal and the session channels.
//!
//! Session tasks never touch the terminal; they emit
//! [`crate::session::SessionEvent`]s that this loop consumes alongside
//! terminal input (see docs/ARCHITECTURE.md §3). Exactly one session is
//! wired for now; multiplexing several arrives in M7.

use std::collections::VecDeque;
use std::path::PathBuf;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind};
use futures::StreamExt;
use ratatui::DefaultTerminal;
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;

use crate::config::Keybinds;
use crate::engine::Engine;
use crate::proto::charset::Charset;
use crate::session::{self, SessionCommand, SessionEvent};
use crate::ui;

/// Bounded so a chatty MUD can't grow the buffer without limit
/// (docs/ARCHITECTURE.md §8; a fuller ring buffer with disk logging is M9).
const SCROLLBACK_LIMIT: usize = 10_000;

/// Same rationale as `SCROLLBACK_LIMIT`, for the raw GMCP inspector log.
const GMCP_LOG_LIMIT: usize = 1_000;

/// The one client-side command M4 defines. Everything else starting with
/// `/` is left alone, since plenty of MUDs use `/` for their own commands.
const RELOAD_COMMAND: &str = "/reload";

/// Recompile the rule set from disk and hand it to the running session.
/// Returns the line to show the player either way — a broken rule file
/// must report itself rather than silently leaving the old rules in place.
async fn reload_rules(
    rules: Option<&(PathBuf, Option<String>)>,
    cmd_tx: Option<&tokio::sync::mpsc::Sender<SessionCommand>>,
) -> String {
    let (Some((config_dir, profile)), Some(tx)) = (rules, cmd_tx) else {
        return "** /reload needs a connected session".to_string();
    };

    match crate::config::load_rules(config_dir, profile.as_deref())
        .and_then(|layers| Ok(Engine::compile(&layers)?))
    {
        Ok(engine) => {
            let _ = tx.send(SessionCommand::SetRules(Box::new(engine))).await;
            "** rules reloaded".to_string()
        }
        Err(err) => format!("** reload failed, keeping the current rules: {err:#}"),
    }
}

pub struct ConnectTarget {
    pub host: String,
    pub port: u16,
    pub tls: Option<crate::net::TlsConfig>,
    pub record: Option<PathBuf>,
    pub charset: Charset,
    /// The compiled rule set, and how to rebuild it for `/reload`.
    pub rules: Rules,
}

/// Where a session's rules came from, so `/reload` can recompile them from
/// disk without reconnecting (docs/ARCHITECTURE.md §7.3).
pub struct Rules {
    pub engine: Engine,
    pub config_dir: PathBuf,
    pub profile: Option<String>,
}

/// Everything the UI needs to render a frame.
pub struct AppState {
    pub scrollback: VecDeque<String>,
    /// Text pinned above the input line; empty means no prompt.
    pub prompt: String,
    pub input: Input,
    pub status: String,
    /// Server took over echoing (Telnet ECHO): hide what we type.
    pub masked: bool,
    /// Transport security shown in the title bar ("TLS", "TLS pinned", …).
    pub security: String,
    /// Whether a session owns the prompt row. Keeping the row reserved for
    /// the whole session keeps the layout — and so the NAWS pane size —
    /// stable as prompts come and go.
    pub connected: bool,
    /// The configured quit binding, for the input box's hint text.
    pub quit_hint: String,
    /// Raw `Package payload` lines, newest last — the GMCP inspector view
    /// (docs/ARCHITECTURE.md §14 M6).
    pub gmcp_log: VecDeque<String>,
    /// Whether the output pane is currently showing `gmcp_log` instead of
    /// the scrollback.
    pub show_gmcp: bool,
}

impl AppState {
    fn push_line(&mut self, line: String) {
        self.scrollback.push_back(line);
        if self.scrollback.len() > SCROLLBACK_LIMIT {
            self.scrollback.pop_front();
        }
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

/// Applies one session event to `state`. Returns `true` if the session
/// ended (`Ended`), so the caller can stop polling its receiver.
fn apply_session_event(state: &mut AppState, connected_status: &str, ev: SessionEvent) -> bool {
    if state.status != connected_status && !matches!(ev, SessionEvent::Ended(_)) {
        state.status = connected_status.to_string();
    }
    match ev {
        SessionEvent::Line(line) => {
            state.push_line(line);
            false
        }
        SessionEvent::Prompt(text) => {
            state.prompt = text;
            false
        }
        SessionEvent::EchoMask(masked) => {
            state.masked = masked;
            false
        }
        SessionEvent::Gmcp { package, payload } => {
            state.push_gmcp(package, payload);
            false
        }
        SessionEvent::Security(security) => {
            state.security = security.label;
            // §13 requires an insecure connection (or a newly pinned
            // certificate) to be visible, not just implied by a label.
            if let Some(warning) = security.warning {
                state.push_line(format!("** {warning}"));
            }
            false
        }
        SessionEvent::Ended(reason) => {
            state.status = format!("disconnected: {reason}");
            state.masked = false;
            state.connected = false;
            true
        }
    }
}

pub async fn run(target: Option<ConnectTarget>, keybinds: Keybinds) -> Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, target, keybinds).await;
    ratatui::restore();
    result
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    target: Option<ConnectTarget>,
    keybinds: Keybinds,
) -> Result<()> {
    let (mut session_events, cmd_tx, status, connected_status, rules) = match target {
        Some(t) => {
            let status = format!("connecting to {}:{}...", t.host, t.port);
            let connected_status = format!("connected to {}:{}", t.host, t.port);
            let Rules {
                engine,
                config_dir,
                profile,
            } = t.rules;
            let (rx, tx) = session::spawn(t.host, t.port, t.tls, t.record, t.charset, engine);
            (
                Some(rx),
                Some(tx),
                status,
                connected_status,
                Some((config_dir, profile)),
            )
        }
        None => (
            None,
            None,
            "no target — run with --host <mud> [--port N] [--tls]".to_string(),
            String::new(),
            None,
        ),
    };

    let mut state = AppState {
        scrollback: VecDeque::new(),
        prompt: String::new(),
        input: Input::default(),
        status,
        masked: false,
        security: String::new(),
        connected: cmd_tx.is_some(),
        quit_hint: keybinds.quit.to_string(),
        gmcp_log: VecDeque::new(),
        show_gmcp: false,
    };

    // Tell the server our pane size up front; NAWS is sent once it agrees.
    if let Some(tx) = &cmd_tx {
        let (cols, rows) = ui::output_pane_size(terminal.get_frame().area(), state.connected);
        let _ = tx.send(SessionCommand::Resize { cols, rows }).await;
    }

    let mut term_events = EventStream::new();

    loop {
        terminal.draw(|frame| ui::draw(frame, &state))?;

        let next_session_event = async {
            match session_events.as_mut() {
                Some(rx) => rx.recv().await,
                None => std::future::pending().await,
            }
        };

        tokio::select! {
            ev = term_events.next() => {
                match ev {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        if keybinds.quit.matches(key.code, key.modifiers) {
                            return Ok(());
                        }
                        if keybinds.gmcp_inspector.matches(key.code, key.modifiers) {
                            state.show_gmcp = !state.show_gmcp;
                        } else if key.code == KeyCode::Enter {
                            let line = state.input.value().to_string();
                            state.input.reset();
                            if !line.is_empty() {
                                // Never echo what the server is masking.
                                if !state.masked {
                                    state.push_line(format!("> {line}"));
                                }
                                if line.trim() == RELOAD_COMMAND {
                                    let notice = reload_rules(rules.as_ref(), cmd_tx.as_ref()).await;
                                    state.push_line(notice);
                                } else if let Some(tx) = &cmd_tx {
                                    let _ = tx.send(SessionCommand::SendLine(line)).await;
                                }
                            }
                        } else {
                            state.input.handle_event(&Event::Key(key));
                        }
                    }
                    Some(Ok(Event::Resize(cols, rows))) => {
                        if let Some(tx) = &cmd_tx {
                            let area = ratatui::layout::Rect::new(0, 0, cols, rows);
                            let (cols, rows) = ui::output_pane_size(area, state.connected);
                            let _ = tx.send(SessionCommand::Resize { cols, rows }).await;
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(err)) => return Err(err.into()),
                    None => return Ok(()),
                }
            }
            ev = next_session_event => {
                let ended = match ev {
                    Some(ev) => apply_session_event(&mut state, &connected_status, ev),
                    None => true,
                };
                if ended {
                    session_events = None;
                } else if let Some(rx) = session_events.as_mut() {
                    // Drain any further already-buffered events without
                    // blocking, so a burst of server output (e.g. a banner
                    // arriving across many small reads) triggers one
                    // redraw instead of one per event.
                    while let Ok(ev) = rx.try_recv() {
                        if apply_session_event(&mut state, &connected_status, ev) {
                            session_events = None;
                            break;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::Security;

    fn state() -> AppState {
        AppState {
            scrollback: VecDeque::new(),
            prompt: String::new(),
            input: Input::default(),
            status: "connecting".to_string(),
            masked: false,
            security: String::new(),
            connected: true,
            quit_hint: "Ctrl+C".to_string(),
            gmcp_log: VecDeque::new(),
            show_gmcp: false,
        }
    }

    fn scrollback(state: &AppState) -> String {
        state
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
        let mut state = state();
        apply_session_event(
            &mut state,
            "connected",
            SessionEvent::Security(Security {
                label: "TLS insecure".to_string(),
                warning: Some("certificate NOT verified".to_string()),
            }),
        );

        assert_eq!(state.security, "TLS insecure");
        assert!(
            scrollback(&state).contains("certificate NOT verified"),
            "warning missing from the pane: {:?}",
            scrollback(&state)
        );
    }

    #[test]
    fn a_verified_connection_adds_no_noise() {
        let mut state = state();
        apply_session_event(
            &mut state,
            "connected",
            SessionEvent::Security(Security {
                label: "TLS".to_string(),
                warning: None,
            }),
        );

        assert_eq!(state.security, "TLS");
        assert!(state.scrollback.is_empty());
    }

    /// A password typed while the server is echoing must not be written to
    /// the scrollback the player can scroll back through.
    #[test]
    fn masked_input_is_never_echoed_to_scrollback() {
        let mut state = state();
        state.masked = true;
        // Mirrors the Enter branch of the event loop.
        let line = "hunter2".to_string();
        if !state.masked {
            state.push_line(format!("> {line}"));
        }
        assert!(state.scrollback.is_empty());

        state.masked = false;
        state.push_line("> look".to_string());
        assert_eq!(scrollback(&state), "> look");
    }

    /// The inspector view's data source (§14 M6): raw GMCP messages land in
    /// their own log, not the scrollback.
    #[test]
    fn a_gmcp_message_is_logged_for_the_inspector_view() {
        let mut state = state();
        apply_session_event(
            &mut state,
            "connected",
            SessionEvent::Gmcp {
                package: "Char.Vitals".to_string(),
                payload: Some(r#"{"hp":100}"#.to_string()),
            },
        );
        assert_eq!(state.gmcp_log.len(), 1);
        assert_eq!(state.gmcp_log[0], r#"Char.Vitals {"hp":100}"#);
        assert!(
            state.scrollback.is_empty(),
            "GMCP must not reach scrollback"
        );
    }

    #[test]
    fn ending_a_session_clears_masking_and_connected_state() {
        let mut state = state();
        state.masked = true;
        let ended = apply_session_event(
            &mut state,
            "connected",
            SessionEvent::Ended("connection closed".to_string()),
        );

        assert!(ended);
        assert!(!state.masked, "a dead session must not leave input hidden");
        assert!(!state.connected);
        assert!(state.status.contains("connection closed"));
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

    #[tokio::test]
    async fn reload_picks_up_edited_rules() {
        let dir = config_with_alias("old");
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let rules = (dir.path().to_path_buf(), Some("tank".to_string()));

        // Edit the module on disk, exactly as a player would.
        std::fs::write(
            dir.path().join("modules/combat.yaml"),
            "name: combat\naliases:\n  - pattern: '^x$'\n    send: [\"new\"]\n",
        )
        .unwrap();

        let notice = reload_rules(Some(&rules), Some(&tx)).await;
        assert!(notice.contains("reloaded"), "{notice}");

        match rx.try_recv().expect("a new rule set was sent") {
            SessionCommand::SetRules(mut engine) => {
                assert_eq!(engine.expand_input("x"), vec!["new"]);
            }
            other => panic!("expected SetRules, got {other:?}"),
        }
    }

    /// A broken rule file must report itself and leave the running session
    /// on the rules it already had, rather than dropping to no rules.
    #[tokio::test]
    async fn reload_reports_a_broken_file_and_sends_nothing() {
        let dir = config_with_alias("old");
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let rules = (dir.path().to_path_buf(), Some("tank".to_string()));

        std::fs::write(
            dir.path().join("modules/combat.yaml"),
            "name: combat\naliases:\n  - pattern: '('\n",
        )
        .unwrap();

        let notice = reload_rules(Some(&rules), Some(&tx)).await;
        assert!(notice.contains("reload failed"), "{notice}");
        assert!(
            notice.contains("keeping the current rules"),
            "the player must be told the old rules still apply: {notice}"
        );
        assert!(
            rx.try_recv().is_err(),
            "a failed reload must not replace the session's rules"
        );
    }

    #[tokio::test]
    async fn reload_without_a_session_explains_itself() {
        let notice = reload_rules(None, None).await;
        assert!(notice.contains("needs a connected session"), "{notice}");
    }
}
