//! Ratatui widgets: pane grid, status bar, input line, tabs.
//!
//! M0 is a single pane: scrollback + prompt above a grapheme-aware input
//! line (`tui-input`). ANSI/TrueColor SGR sequences are rendered as-is via
//! `ansi-to-tui`; unknown/malformed escapes are dropped rather than shown
//! raw. Layout, focus, and unread-indicator state for multiple sessions
//! arrive in M7 (docs/ARCHITECTURE.md §11).

use ansi_to_tui::IntoText;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Paragraph, Wrap};

use crate::app::AppState;

pub fn draw(frame: &mut Frame, state: &AppState) {
    let [main, input_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).areas(frame.area());

    let mut lines: Vec<Line> = Vec::new();
    for raw in &state.scrollback {
        lines.extend(ansi_lines(raw));
    }
    if !state.prompt.is_empty() {
        lines.extend(ansi_lines(&state.prompt));
    }

    // Content width matches the Paragraph's own wrapping width (area minus
    // borders). `line_count` runs ratatui's real wrap algorithm rather than
    // an estimate, so a long line's tail is never silently truncated.
    let content_width = main.width.saturating_sub(2);
    let text = Text::from(lines);
    let wrapped_rows = Paragraph::new(text.clone())
        .wrap(Wrap { trim: false })
        .line_count(content_width) as u16;

    let title = format!(" Mudular — {} ", state.status);
    let body = Paragraph::new(text)
        .block(Block::bordered().title(title.bold()))
        .wrap(Wrap { trim: false })
        .scroll((scroll_offset(main.height, wrapped_rows), 0));
    frame.render_widget(body, main);

    let input_line = Paragraph::new(state.input.value()).block(
        Block::bordered()
            .title(" input (Ctrl+C to quit) ")
            .border_style(Style::new().dim()),
    );
    frame.render_widget(input_line, input_area);

    let cursor_x = input_area.x + 1 + state.input.visual_cursor() as u16;
    let cursor_y = input_area.y + 1;
    frame.set_cursor_position((cursor_x, cursor_y));
}

/// Keep the view tailed to the bottom, like a terminal — scrollback
/// navigation (PgUp/PgDn) is a later milestone.
fn scroll_offset(area_height: u16, content_lines: u16) -> u16 {
    let viewport = area_height.saturating_sub(2); // borders
    content_lines.saturating_sub(viewport)
}

fn ansi_lines(raw: &str) -> Vec<Line<'static>> {
    match raw.into_text() {
        Ok(text) => text.lines,
        Err(_) => vec![Line::raw(raw.to_string())],
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::app::AppState;

    /// A 30-wide terminal splits into a 7-tall output pane (5 content rows
    /// between its own top/bottom border) and a 3-tall input pane, matching
    /// the `Layout` in `draw`. Content rows are 1..=5; row 0 is the output
    /// pane's top border and row 6 is its bottom border — asserting `│` on
    /// those would be checking the widget's own legitimate border, not
    /// corruption.
    const CONTENT_ROWS: std::ops::RangeInclusive<u16> = 1..=5;

    fn assert_left_border_intact(buffer: &ratatui::buffer::Buffer) {
        for y in CONTENT_ROWS {
            let cell = buffer.cell((0, y)).unwrap();
            assert_eq!(
                cell.symbol(),
                "│",
                "row {y} left border corrupted: {:?}",
                (0..30)
                    .map(|x| buffer.cell((x, y)).unwrap().symbol())
                    .collect::<String>()
            );
        }
    }

    /// One draw of a static, already-overflowing state: exercises the
    /// scroll-offset math (`wrapped_rows` vs viewport height) in isolation,
    /// including a line ("aaaa bbbb...b cccc") whose real word-wrapped row
    /// count (3, at this 28-wide content area) diverges from a naive
    /// width/content_width division (2) — the exact class of mismatch that
    /// previously drove the scroll offset to the wrong row.
    #[test]
    fn scroll_offset_matches_real_wrap_count() {
        let backend = TestBackend::new(30, 10);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut scrollback = VecDeque::new();
        scrollback.push_back(format!(
            "{} {} {}",
            "a".repeat(4),
            "b".repeat(24),
            "c".repeat(4)
        ));
        for i in 0..10 {
            scrollback.push_back(format!("line {i}"));
        }

        let state = AppState {
            scrollback,
            prompt: "By what name do you wish to be known?".to_string(),
            input: tui_input::Input::default(),
            status: "connected".to_string(),
        };

        terminal.draw(|frame| draw(frame, &state)).unwrap();
        assert_left_border_intact(terminal.backend().buffer());
    }

    /// Redraws on every event, exactly like the real event loop
    /// (docs/ARCHITECTURE.md §3 `app.rs`): a login banner arriving line by
    /// line, an unterminated prompt, then several short server lines each
    /// triggering their own `terminal.draw()` — reusing the same `Terminal`
    /// (and its diff buffer) across calls, unlike a single one-shot draw of
    /// the final state. This is the closest headless reproduction of the
    /// reported session.
    #[test]
    fn repeated_draws_never_corrupt_the_left_border() {
        let backend = TestBackend::new(30, 10);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut state = AppState {
            scrollback: VecDeque::new(),
            prompt: String::new(),
            input: tui_input::Input::default(),
            status: "connected".to_string(),
        };

        let banner = [
            "Welcome to FakeMUD",
            "",
            "A tale of two cities.",
            "It was the best of times, it was the worst of times.",
        ];
        for line in banner {
            state.scrollback.push_back(line.to_string());
            terminal.draw(|frame| draw(frame, &state)).unwrap();
            assert_left_border_intact(terminal.backend().buffer());
        }

        state.prompt = "By what name do you wish to be known?".to_string();
        terminal.draw(|frame| draw(frame, &state)).unwrap();
        assert_left_border_intact(terminal.backend().buffer());

        state.prompt.clear();
        state.scrollback.push_back("> crazy-foo".to_string());
        terminal.draw(|frame| draw(frame, &state)).unwrap();
        assert_left_border_intact(terminal.backend().buffer());

        for line in ["Password:", "Reconnecting.", "", "i107 >"] {
            state.scrollback.push_back(line.to_string());
            terminal.draw(|frame| draw(frame, &state)).unwrap();
            assert_left_border_intact(terminal.backend().buffer());
        }
    }
}
