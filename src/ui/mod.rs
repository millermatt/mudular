//! Ratatui widgets: pane grid, status bar, input line, tabs.
//!
//! Layout, focus, and unread-indicator state will live in an `AppState`
//! owned by the UI task (docs/ARCHITECTURE.md §11).

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Style, Stylize};
use ratatui::widgets::{Block, Paragraph};

pub fn draw_placeholder(frame: &mut Frame, status: &str) {
    let [main, input] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).areas(frame.area());

    let body = Paragraph::new(format!("{status}\n\npress q to quit"))
        .block(Block::bordered().title(" Mudular ".bold()));
    frame.render_widget(body, main);

    let input_line = Paragraph::new("").block(
        Block::bordered()
            .title(" input ")
            .border_style(Style::new().dim()),
    );
    frame.render_widget(input_line, input);
}
