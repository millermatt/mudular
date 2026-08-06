//! Ratatui widgets: pane grid, status bar, input line, tabs.
//!
//! M1 is a single pane: scrollback, a prompt line pinned above the input
//! line, and a grapheme-aware input editor (`tui-input`) that masks its
//! contents while the server is echoing. ANSI/TrueColor SGR sequences are
//! rendered via `ansi-to-tui`; unknown/malformed escapes are dropped
//! rather than shown raw. Layout, focus, and unread-indicator state for
//! multiple sessions arrive in M7 (docs/ARCHITECTURE.md §11).

use ansi_to_tui::IntoText;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Paragraph, Wrap};

use crate::app::AppState;

/// Splits the frame into output pane, prompt row, and input box. The
/// prompt row stays reserved for the life of a session so the layout —
/// and the NAWS pane size derived from it — does not shift as prompts
/// come and go.
pub fn layout(area: Rect, reserve_prompt: bool) -> [Rect; 3] {
    Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(if reserve_prompt { 1 } else { 0 }),
        Constraint::Length(3),
    ])
    .areas(area)
}

/// The server-visible pane size for NAWS: the output pane inside its
/// border (docs/ARCHITECTURE.md §6.2 — per-pane, not the whole terminal).
pub fn output_pane_size(area: Rect, reserve_prompt: bool) -> (u16, u16) {
    let [main, _, _] = layout(area, reserve_prompt);
    (main.width.saturating_sub(2), main.height.saturating_sub(2))
}

pub fn draw(frame: &mut Frame, state: &AppState) {
    let [main, prompt_area, input_area] = layout(frame.area(), state.connected);

    let mut lines: Vec<Line> = Vec::new();
    for raw in &state.scrollback {
        lines.extend(ansi_lines(raw));
    }

    // Content width matches the Paragraph's own wrapping width (area minus
    // borders). `line_count` runs ratatui's real wrap algorithm rather than
    // an estimate, so a long line's tail is never silently truncated.
    let content_width = main.width.saturating_sub(2);
    let text = Text::from(lines);
    let wrapped_rows = Paragraph::new(text.clone())
        .wrap(Wrap { trim: false })
        .line_count(content_width) as u16;

    let title = if state.security.is_empty() {
        format!(" Mudular — {} ", state.status)
    } else {
        format!(" Mudular — {} [{}] ", state.status, state.security)
    };
    let body = Paragraph::new(text)
        .block(Block::bordered().title(title.bold()))
        .wrap(Wrap { trim: false })
        .scroll((scroll_offset(main.height, wrapped_rows), 0));
    frame.render_widget(body, main);

    if state.connected && !state.prompt.is_empty() {
        // Indent by one column so the prompt lines up with the pane's
        // content rather than its border.
        let inset = Rect {
            x: prompt_area.x + 1,
            width: prompt_area.width.saturating_sub(1),
            ..prompt_area
        };
        let prompt = Paragraph::new(Text::from(ansi_lines(&state.prompt)));
        frame.render_widget(prompt, inset);
    }

    let (value, cursor) = if state.masked {
        (
            "*".repeat(state.input.value().chars().count()),
            state.input.cursor() as u16,
        )
    } else {
        (
            state.input.value().to_string(),
            state.input.visual_cursor() as u16,
        )
    };
    let title = if state.masked {
        " input (hidden) ".to_string()
    } else {
        format!(" input ({} to quit) ", state.quit_hint)
    };
    let input_line = Paragraph::new(value).block(
        Block::bordered()
            .title(title)
            .border_style(Style::new().dim()),
    );
    frame.render_widget(input_line, input_area);

    frame.set_cursor_position((input_area.x + 1 + cursor, input_area.y + 1));
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

    /// A 30x10 terminal splits into a 6-tall output pane (4 content rows
    /// inside its border), a 1-row prompt, and a 3-tall input box. Content
    /// rows are 1..=4; row 0 and row 5 are the pane's own borders, so
    /// asserting `│` there would test the border, not corruption.
    const CONTENT_ROWS: std::ops::RangeInclusive<u16> = 1..=4;

    fn state() -> AppState {
        AppState {
            scrollback: VecDeque::new(),
            prompt: String::new(),
            input: tui_input::Input::default(),
            status: "connected".to_string(),
            masked: false,
            security: String::new(),
            connected: true,
            quit_hint: "Ctrl+C".to_string(),
        }
    }

    fn render(state: &AppState) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(30, 10)).unwrap();
        terminal.draw(|frame| draw(frame, state)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn row(buffer: &ratatui::buffer::Buffer, y: u16) -> String {
        (0..30)
            .map(|x| buffer.cell((x, y)).unwrap().symbol())
            .collect()
    }

    fn assert_left_border_intact(buffer: &ratatui::buffer::Buffer) {
        for y in CONTENT_ROWS {
            assert_eq!(
                buffer.cell((0, y)).unwrap().symbol(),
                "│",
                "row {y} left border corrupted: {:?}",
                row(buffer, y)
            );
        }
    }

    /// The prompt belongs on its own row above the input box, not in the
    /// scrollback — "prompts render as prompts" (docs/ARCHITECTURE.md §14).
    #[test]
    fn pins_the_prompt_above_the_input_line() {
        let mut state = state();
        state
            .scrollback
            .push_back("You are in a forest.".to_string());
        state.prompt = "HP:100 MP:50>".to_string();

        let buffer = render(&state);
        assert!(
            row(&buffer, 6).contains("HP:100 MP:50>"),
            "prompt row: {:?}",
            row(&buffer, 6)
        );
        assert!(
            row(&buffer, 1).contains("You are in a forest."),
            "scrollback stays in the pane: {:?}",
            row(&buffer, 1)
        );
    }

    #[test]
    fn masks_input_when_the_server_is_echoing() {
        let mut state = state();
        state.input = tui_input::Input::default().with_value("hunter2".to_string());

        let visible = render(&state);
        assert!(row(&visible, 8).contains("hunter2"));

        state.masked = true;
        let hidden = render(&state);
        assert!(
            !row(&hidden, 8).contains("hunter2"),
            "password leaked: {:?}",
            row(&hidden, 8)
        );
        assert!(row(&hidden, 8).contains("*******"));
    }

    /// NAWS must describe the output pane inside its border, not the whole
    /// terminal (docs/ARCHITECTURE.md §6.2).
    #[test]
    fn reports_pane_size_excluding_chrome() {
        let area = Rect::new(0, 0, 30, 10);
        // 30 wide - 2 border columns; 10 tall - 1 prompt - 3 input - 2 border.
        assert_eq!(output_pane_size(area, true), (28, 4));
        assert_eq!(output_pane_size(area, false), (28, 5));
    }

    /// One draw of a static, already-overflowing state: exercises the
    /// scroll-offset math (`wrapped_rows` vs viewport height) in isolation,
    /// including a line whose real word-wrapped row count diverges from a
    /// naive width division.
    #[test]
    fn scroll_offset_matches_real_wrap_count() {
        let mut state = state();
        state.scrollback.push_back(format!(
            "{} {} {}",
            "a".repeat(4),
            "b".repeat(24),
            "c".repeat(4)
        ));
        for i in 0..10 {
            state.scrollback.push_back(format!("line {i}"));
        }
        state.prompt = "By what name do you wish to be known?".to_string();

        assert_left_border_intact(&render(&state));
    }

    /// Redraws on every event, exactly like the real event loop: a banner
    /// arriving line by line, a prompt, then more server lines — reusing
    /// one `Terminal` (and its diff buffer) across draws.
    #[test]
    fn repeated_draws_never_corrupt_the_left_border() {
        let mut terminal = Terminal::new(TestBackend::new(30, 10)).unwrap();
        let mut state = state();

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
        for line in ["Password:", "Reconnecting.", "", "i107 >"] {
            state.scrollback.push_back(line.to_string());
            terminal.draw(|frame| draw(frame, &state)).unwrap();
            assert_left_border_intact(terminal.backend().buffer());
        }
    }
}
