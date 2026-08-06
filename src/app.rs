//! UI event loop: owns the terminal and, eventually, all session channels.
//!
//! Sessions never touch the terminal; they emit [`crate::session::SessionEvent`]s
//! that this loop consumes alongside terminal input (see docs/ARCHITECTURE.md §3).

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::DefaultTerminal;

use crate::ui;

pub async fn run(status: String) -> Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &status).await;
    ratatui::restore();
    result
}

async fn event_loop(terminal: &mut DefaultTerminal, status: &str) -> Result<()> {
    let mut events = EventStream::new();
    loop {
        terminal.draw(|frame| ui::draw_placeholder(frame, status))?;
        match events.next().await {
            Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                let ctrl_c =
                    key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
                if key.code == KeyCode::Char('q') || ctrl_c {
                    return Ok(());
                }
            }
            Some(Ok(_)) => {}
            Some(Err(err)) => return Err(err.into()),
            None => return Ok(()),
        }
    }
}
