//! UI event loop: owns the terminal and the session channels.
//!
//! Session tasks never touch the terminal; they emit
//! [`crate::session::SessionEvent`]s that this loop consumes alongside
//! terminal input (see docs/ARCHITECTURE.md §3). M0 wires exactly one
//! session; multiplexing several arrives in M7.

use std::collections::VecDeque;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::DefaultTerminal;
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;

use crate::session::{self, SessionCommand, SessionEvent};
use crate::ui;

/// Bounded so a chatty MUD can't grow the buffer without limit
/// (docs/ARCHITECTURE.md §8; a fuller ring buffer with disk logging is M9).
const SCROLLBACK_LIMIT: usize = 10_000;

pub struct ConnectTarget {
    pub host: String,
    pub port: u16,
    pub tls: bool,
}

/// Everything the UI needs to render a frame.
pub struct AppState {
    pub scrollback: VecDeque<String>,
    /// The current unterminated line (e.g. a login prompt with no `\n`).
    pub prompt: String,
    pub input: Input,
    pub status: String,
}

impl AppState {
    fn push_line(&mut self, line: String) {
        self.scrollback.push_back(line);
        if self.scrollback.len() > SCROLLBACK_LIMIT {
            self.scrollback.pop_front();
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
            state.prompt.clear();
            state.push_line(line);
            false
        }
        SessionEvent::Prompt(text) => {
            state.prompt = text;
            false
        }
        SessionEvent::EchoMask(_) => {
            // Password masking lands in M1.
            false
        }
        SessionEvent::Ended(reason) => {
            state.status = format!("disconnected: {reason}");
            true
        }
    }
}

pub async fn run(target: Option<ConnectTarget>) -> Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, target).await;
    ratatui::restore();
    result
}

async fn event_loop(terminal: &mut DefaultTerminal, target: Option<ConnectTarget>) -> Result<()> {
    let (mut session_events, cmd_tx, status, connected_status) = match target {
        Some(t) => {
            let status = format!("connecting to {}:{}...", t.host, t.port);
            let connected_status = format!("connected to {}:{}", t.host, t.port);
            let (rx, tx) = session::spawn(t.host, t.port, t.tls);
            (Some(rx), Some(tx), status, connected_status)
        }
        None => (
            None,
            None,
            "no target — run with --host <mud> [--port N] [--tls]".to_string(),
            String::new(),
        ),
    };

    let mut state = AppState {
        scrollback: VecDeque::new(),
        prompt: String::new(),
        input: Input::default(),
        status,
    };

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
                        let ctrl_c = key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL);
                        if ctrl_c {
                            return Ok(());
                        }
                        if key.code == KeyCode::Enter {
                            let line = state.input.value().to_string();
                            state.input.reset();
                            if !line.is_empty() {
                                state.push_line(format!("> {line}"));
                                if let Some(tx) = &cmd_tx {
                                    let _ = tx.send(SessionCommand::SendLine(line)).await;
                                }
                            }
                        } else {
                            state.input.handle_event(&Event::Key(key));
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
