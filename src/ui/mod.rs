//! Ratatui widgets: pane grid, tab bar, channel panes, and the input line.
//!
//! Layout is either tabs (the focused session full-screen) or splits (every
//! session side by side), with channel panes docked in a column beside them
//! (docs/ARCHITECTURE.md §11/§11.1). One input line sits at the bottom bound
//! to the last focused *session*, showing that binding in its border, so
//! focusing a channel pane never redirects what you type. ANSI/TrueColor SGR
//! sequences are rendered via `ansi-to-tui`; unknown/malformed escapes are
//! dropped rather than shown raw.

use ansi_to_tui::IntoText;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Paragraph, Wrap};

use crate::app::{AppState, ChannelPane, Focus, LayoutMode};
use crate::config::Keybinds;

/// Width of the docked channel column, and the smallest main area worth
/// keeping beside it — below that the channels are simply not drawn.
const CHANNEL_WIDTH: u16 = 28;
const MIN_MAIN_WIDTH: u16 = 30;

/// Column the descriptions start at in the help listing.
const HELP_KEY_WIDTH: usize = 14;

/// Every binding and client command, built from the bindings the event loop
/// actually matches against (docs/ARCHITECTURE.md §11.2). Nothing here is a
/// hardcoded key name: a remapped binding documents itself, and the help
/// cannot drift out of step with what the client does.
pub fn help_lines(keybinds: &Keybinds) -> Vec<String> {
    fn row(key: impl std::fmt::Display, what: &str) -> String {
        format!("  {:HELP_KEY_WIDTH$}{what}", key.to_string())
    }

    vec![
        "Typing".to_string(),
        row("Enter", "send the line — on an empty box a bare"),
        row("", "return, for \"press return to continue\""),
        row("Up / Down", "walk this character's history"),
        row("PgUp / PgDn", "scroll the focused pane back / forward"),
        row("Home / End", "jump to the oldest / newest line"),
        String::new(),
        "Characters".to_string(),
        row("Alt+1 … Alt+9", "jump to character 1-9"),
        row(keybinds.focus_next, "cycle focus, channels included"),
        String::new(),
        "Views".to_string(),
        row(keybinds.cycle_layout, "tabs / side-by-side layout"),
        row(keybinds.toggle_channels, "show or hide channel panes"),
        row(keybinds.gmcp_inspector, "raw GMCP inspector"),
        row(keybinds.help, "this help"),
        String::new(),
        "Commands you can type".to_string(),
        row("/help", "print this help into the pane"),
        row("/reload", "recompile rules and scripts from disk"),
        String::new(),
        "Leaving".to_string(),
        row(keybinds.quit, "quit"),
        String::new(),
        "Keys other than Alt+N, Enter and the arrows are".to_string(),
        "remappable under `keybinds:` in mudular.yaml.".to_string(),
    ]
}

/// Where every pane lands this frame.
pub struct Panes {
    /// One rect per session. In tabs mode every session shares the same
    /// rect: a hidden pane would occupy exactly that space when focused, so
    /// its server-visible size never changes just by losing focus.
    pub sessions: Vec<Rect>,
    pub tab_bar: Option<Rect>,
    pub channels: Vec<Rect>,
    pub prompt: Option<Rect>,
    pub input: Rect,
}

/// Splits `area` into the panes `state` asks for.
pub fn layout(area: Rect, state: &AppState) -> Panes {
    let reserve_prompt = state.bound().is_some_and(|session| session.connected);
    let [body, prompt_area, input] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(if reserve_prompt { 1 } else { 0 }),
        Constraint::Length(3),
    ])
    .areas(area);

    let show_channels = state.show_channels
        && !state.channels.is_empty()
        && body.width >= MIN_MAIN_WIDTH + CHANNEL_WIDTH;
    let (main, channel_column) = if show_channels {
        let [main, column] = Layout::horizontal([
            Constraint::Min(MIN_MAIN_WIDTH),
            Constraint::Length(CHANNEL_WIDTH),
        ])
        .areas(body);
        (main, Some(column))
    } else {
        (body, None)
    };

    let (tab_bar, session_area) = match state.layout {
        LayoutMode::Tabs if state.sessions.len() > 1 => {
            let [bar, rest] =
                Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(main);
            (Some(bar), rest)
        }
        _ => (None, main),
    };

    let sessions = match state.layout {
        LayoutMode::Tabs => vec![session_area; state.sessions.len()],
        LayoutMode::Splits => even_split(session_area, state.sessions.len(), Direction::Horizontal),
    };
    let channels = match channel_column {
        Some(column) => even_split(column, state.channels.len(), Direction::Vertical),
        None => Vec::new(),
    };

    Panes {
        sessions,
        tab_bar,
        channels,
        prompt: reserve_prompt.then_some(prompt_area),
        input,
    }
}

fn even_split(area: Rect, count: usize, direction: Direction) -> Vec<Rect> {
    if count == 0 {
        return Vec::new();
    }
    Layout::new(
        direction,
        std::iter::repeat_n(Constraint::Ratio(1, count as u32), count),
    )
    .split(area)
    .to_vec()
}

/// The server-visible size of each session's pane, inside its border — NAWS
/// is per-pane, not per-terminal (docs/ARCHITECTURE.md §6.2).
pub fn session_pane_sizes(area: Rect, state: &AppState) -> Vec<(usize, (u16, u16))> {
    layout(area, state)
        .sessions
        .into_iter()
        .enumerate()
        .map(|(index, rect)| {
            (
                index,
                (rect.width.saturating_sub(2), rect.height.saturating_sub(2)),
            )
        })
        .collect()
}

pub fn draw(frame: &mut Frame, state: &AppState) {
    let panes = layout(frame.area(), state);

    if state.sessions.is_empty() {
        let help = Paragraph::new(
            "no target — run with a profile name, or --host <mud> [--port N] [--tls]",
        )
        .block(Block::bordered().title(" Mudular ".bold()));
        frame.render_widget(
            help,
            panes.sessions.first().copied().unwrap_or(frame.area()),
        );
    }

    for (index, rect) in panes.sessions.iter().enumerate() {
        // In tabs mode only the focused session is on screen.
        if state.layout == LayoutMode::Tabs && !state.is_focused_session(index) {
            continue;
        }
        draw_session(frame, *rect, state, index);
    }

    if let Some(bar) = panes.tab_bar {
        frame.render_widget(Paragraph::new(tab_line(state)), bar);
    }

    for (index, rect) in panes.channels.iter().enumerate() {
        draw_channel(
            frame,
            *rect,
            &state.channels[index],
            state.focus == Focus::Channel(index),
        );
    }

    let bound = state.bound();
    if let (Some(area), Some(session)) = (panes.prompt, bound)
        && !session.prompt.is_empty()
    {
        // Indent by one column so the prompt lines up with the pane's
        // content rather than its border.
        let inset = Rect {
            x: area.x + 1,
            width: area.width.saturating_sub(1),
            ..area
        };
        frame.render_widget(
            Paragraph::new(Text::from(ansi_lines(&session.prompt))),
            inset,
        );
    }

    draw_input(frame, panes.input, state);

    if state.show_help {
        draw_help(frame, frame.area(), &state.keybinds);
    }
}

/// The help overlay: a box centred over the layout, sized to its content and
/// clipped to the terminal (docs/ARCHITECTURE.md §11.2).
fn draw_help(frame: &mut Frame, area: Rect, keybinds: &Keybinds) {
    let lines = help_lines(keybinds);
    let content_width = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    // +2 for the border on each axis.
    let width = (content_width as u16 + 4).min(area.width);
    let height = (lines.len() as u16 + 2).min(area.height);
    let overlay = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    let text = Text::from(lines.into_iter().map(Line::from).collect::<Vec<_>>());
    let block =
        Block::bordered().title(format!(" Help — {} or Esc to close ", keybinds.help).bold());
    // Clear first: the overlay sits on top of panes that already drew here.
    frame.render_widget(ratatui::widgets::Clear, overlay);
    frame.render_widget(Paragraph::new(text).block(block), overlay);
}

fn draw_session(frame: &mut Frame, area: Rect, state: &AppState, index: usize) {
    let session = &state.sessions[index];
    let focused = state.is_focused_session(index);
    let showing_gmcp = state.show_gmcp && focused;

    let mut lines: Vec<Line> = Vec::new();
    if showing_gmcp {
        for raw in &session.gmcp_log {
            lines.push(Line::raw(raw.clone()));
        }
    } else {
        for raw in &session.scrollback {
            lines.extend(ansi_lines(raw));
        }
    }

    let title = if showing_gmcp {
        format!(" {} — GMCP inspector ", session.name)
    } else if session.security.is_empty() {
        format!(
            "{} — {}{} ",
            pane_title(&session.name, session.unread),
            session.status,
            scroll_indicator(session.back_offset)
        )
    } else {
        format!(
            "{} — {} [{}]{} ",
            pane_title(&session.name, session.unread),
            session.status,
            session.security,
            scroll_indicator(session.back_offset)
        )
    };

    // The inspector is a different buffer (`gmcp_log`, not `scrollback`) —
    // a scroll position set for one has nothing to say about the other, so
    // toggling into it always shows its own tail rather than silently
    // inheriting an offset that would land somewhere unrelated with no
    // indicator to explain why (§11.5 scopes navigation to scrollback).
    let back_offset = if showing_gmcp { 0 } else { session.back_offset };

    render_scrollback(
        frame,
        area,
        lines,
        title,
        focused,
        session.color,
        back_offset,
    );
}

fn draw_channel(frame: &mut Frame, area: Rect, channel: &ChannelPane, focused: bool) {
    let lines: Vec<Line> = channel
        .lines
        .iter()
        .flat_map(|raw| ansi_lines(raw))
        .collect();
    let title = format!(
        "{}{} ",
        pane_title(&channel.config.name, channel.unread),
        scroll_indicator(channel.back_offset)
    );
    // Channels aggregate across characters, so no one profile's colour
    // could stand for the pane.
    render_scrollback(
        frame,
        area,
        lines,
        title,
        focused,
        None,
        channel.back_offset,
    );
}

/// `↑ scrolled` when a pane isn't pinned to the tail — distinct from the
/// unread badge (`●N`), and shown even on a focused pane, which the unread
/// badge deliberately never marks: unread means "you haven't looked",
/// scrolled means "you're looking at something old right now"
/// (docs/ARCHITECTURE.md §11.5).
fn scroll_indicator(back_offset: usize) -> &'static str {
    if back_offset == 0 {
        ""
    } else {
        " ↑ scrolled"
    }
}

/// Renders a bordered pane, tailed to the newest content unless `back_offset`
/// says otherwise. Clamping happens here, not at key-press time: this is
/// where `wrapped_rows` — ratatui's real wrap algorithm, not an estimate —
/// and the viewport height are both already known (docs/ARCHITECTURE.md
/// §11.5).
fn render_scrollback(
    frame: &mut Frame,
    area: Rect,
    lines: Vec<Line>,
    title: String,
    focused: bool,
    color: Option<Color>,
    back_offset: usize,
) {
    // Content width matches the Paragraph's own wrapping width (area minus
    // borders). `line_count` runs ratatui's real wrap algorithm rather than
    // an estimate, so a long line's tail is never silently truncated.
    let content_width = area.width.saturating_sub(2);
    let text = Text::from(lines);
    let wrapped_rows = Paragraph::new(text.clone())
        .wrap(Wrap { trim: false })
        .line_count(content_width) as u16;

    // A profile's colour tints the border; dimming still marks the pane as
    // unfocused, so colour identifies the character and brightness
    // identifies focus — two signals that don't compete (§11).
    let mut border = match color {
        Some(color) => Style::new().fg(color),
        None => Style::new(),
    };
    if !focused {
        border = border.dim();
    }
    let block = Block::bordered()
        .title(if focused { title.bold() } else { title.into() })
        .border_style(border);
    let body = Paragraph::new(text)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((scroll_offset(area.height, wrapped_rows, back_offset), 0));
    frame.render_widget(body, area);
}

fn draw_input(frame: &mut Frame, area: Rect, state: &AppState) {
    let Some(session) = state.bound() else {
        let empty = Paragraph::new("").block(
            Block::bordered()
                .title(format!(" input ({} to quit) ", state.keybinds.quit))
                .border_style(Style::new().dim()),
        );
        frame.render_widget(empty, area);
        return;
    };

    let (value, cursor) = if session.masked {
        (
            "*".repeat(session.input.value().chars().count()),
            session.input.cursor() as u16,
        )
    } else {
        (
            session.input.value().to_string(),
            session.input.visual_cursor() as u16,
        )
    };
    // The border names the session commands go to: with several characters
    // open, and focus possibly on a channel pane, that must never be a guess.
    let title = if session.masked {
        format!(" input → {} (hidden) ", session.name)
    } else {
        format!(
            " input → {} ({} to quit) ",
            session.name, state.keybinds.quit
        )
    };
    let input_line = Paragraph::new(value).block(
        Block::bordered()
            .title(title)
            .border_style(Style::new().dim()),
    );
    frame.render_widget(input_line, area);

    frame.set_cursor_position((area.x + 1 + cursor, area.y + 1));
}

/// `name ●3` — the unread badge for a pane that isn't focused (§11).
fn pane_title(name: &str, unread: usize) -> String {
    if unread == 0 {
        format!(" {name}")
    } else {
        format!(" {name} ●{unread}")
    }
}

fn tab_line(state: &AppState) -> Line<'static> {
    let mut spans = Vec::new();
    for (index, session) in state.sessions.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(" │"));
        }
        let label = format!(
            "{}:{}",
            index + 1,
            pane_title(&session.name, session.unread)
        );
        let mut style = match session.color {
            Some(color) => Style::new().fg(color),
            None => Style::new(),
        };
        style = if state.is_focused_session(index) {
            style.bold().reversed()
        } else {
            style.dim()
        };
        spans.push(Span::styled(label, style));
    }
    Line::from(spans)
}

/// Turns a pane's `back_offset` (distance from the tail) into the ratatui
/// scroll row, clamped to `[0, max_scroll]` here — the one place both the
/// real wrapped-row count and the viewport height are known
/// (docs/ARCHITECTURE.md §11.5).
fn scroll_offset(area_height: u16, content_lines: u16, back_offset: usize) -> u16 {
    let viewport = area_height.saturating_sub(2); // borders
    let max_scroll = content_lines.saturating_sub(viewport);
    let back = back_offset.min(max_scroll as usize) as u16;
    max_scroll - back
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
    use crate::app::{ChannelPane, test_support};

    /// A 30x10 terminal splits into a 6-tall output pane (4 content rows
    /// inside its border), a 1-row prompt, and a 3-tall input box. Content
    /// rows are 1..=4; row 0 and row 5 are the pane's own borders, so
    /// asserting `│` there would test the border, not corruption.
    const CONTENT_ROWS: std::ops::RangeInclusive<u16> = 1..=4;

    fn state() -> AppState {
        test_support::app(&["kestrel"])
    }

    fn render(state: &AppState) -> ratatui::buffer::Buffer {
        render_sized(state, 30, 10)
    }

    fn render_sized(state: &AppState, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, state)).unwrap();
        terminal.backend().buffer().clone()
    }

    /// The whole buffer as text, for assertions that don't care which row.
    fn rows(buffer: &ratatui::buffer::Buffer) -> String {
        (0..buffer.area.height)
            .map(|y| row(buffer, y))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn row(buffer: &ratatui::buffer::Buffer, y: u16) -> String {
        let width = buffer.area.width;
        (0..width)
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
        state.sessions[0]
            .scrollback
            .push_back("You are in a forest.".to_string());
        state.sessions[0].prompt = "HP:100 MP:50>".to_string();

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

    /// The inspector view (§14 M6) replaces the scrollback with the raw
    /// GMCP log while toggled on.
    #[test]
    fn shows_the_gmcp_log_instead_of_scrollback_when_toggled() {
        let mut state = state();
        state.sessions[0]
            .scrollback
            .push_back("You are in a forest.".to_string());
        state.sessions[0]
            .gmcp_log
            .push_back(r#"Char.Vitals {"hp":100}"#.to_string());
        state.show_gmcp = true;

        let buffer = render(&state);
        assert!(row(&buffer, 1).contains("Char.Vitals"));
        assert!(!row(&buffer, 1).contains("forest"));
    }

    /// A profile's colour has to reach both places a character is named,
    /// or the pane and its tab look like different characters (§11).
    #[test]
    fn a_profiles_color_tints_its_border_and_its_tab() {
        let mut state = test_support::app(&["tank", "cleric"]);
        state.sessions[0].color = Some(Color::Magenta);

        let buffer = render_sized(&state, 40, 12);
        // Row 0 is the tab bar; row 1 is the focused pane's top border.
        let tab = buffer
            .content()
            .iter()
            .take(40)
            .find(|cell| cell.symbol() == "t")
            .expect("the tab bar names the session");
        assert_eq!(tab.fg, Color::Magenta);
        assert_eq!(buffer.cell((0, 1)).unwrap().fg, Color::Magenta);
    }

    /// Focus is shown by brightness and colour by profile, so an unfocused
    /// coloured pane must keep both signals rather than losing one.
    #[test]
    fn an_unfocused_colored_pane_stays_colored() {
        let mut state = test_support::app(&["tank", "cleric"]);
        state.sessions[1].color = Some(Color::Green);
        state.layout = LayoutMode::Splits;

        let buffer = render_sized(&state, 60, 12);
        let border = buffer.cell((59, 1)).unwrap();
        assert_eq!(border.fg, Color::Green);
        assert!(border.modifier.contains(ratatui::style::Modifier::DIM));
    }

    /// The overlay covers what is under it — a listing rendered over live
    /// scrollback would be unreadable (docs/ARCHITECTURE.md §11.2).
    #[test]
    fn the_help_overlay_draws_over_the_panes() {
        let mut state = state();
        for _ in 0..40 {
            state.sessions[0]
                .scrollback
                .push_back("You are in a forest.".to_string());
        }

        let without = render_sized(&state, 70, 40);
        assert!(rows(&without).contains("forest"));

        state.show_help = true;
        let with = render_sized(&state, 70, 40);
        let listing = rows(&with);
        assert!(listing.contains("Help"), "{listing}");
        assert!(listing.contains("Ctrl+C"), "{listing}");

        let covered = (0..with.area.height)
            .map(|y| row(&with, y))
            .find(|line| line.contains("Alt+1"))
            .expect("the listing is on screen");
        assert!(
            !covered.contains("forest"),
            "the overlay must clear the cells it covers: {covered}"
        );
    }

    /// Small terminals are the ones most likely to have the overlay, and a
    /// box larger than the screen would panic in ratatui rather than clip.
    #[test]
    fn the_help_overlay_fits_a_terminal_smaller_than_itself() {
        let mut state = state();
        state.show_help = true;

        let buffer = render_sized(&state, 20, 6);
        assert_eq!(buffer.area.width, 20);
    }

    #[test]
    fn masks_input_when_the_server_is_echoing() {
        let mut state = state();
        state.sessions[0].input = tui_input::Input::default().with_value("hunter2".to_string());

        let visible = render(&state);
        assert!(row(&visible, 8).contains("hunter2"));

        state.sessions[0].masked = true;
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
        let mut state = state();
        let area = Rect::new(0, 0, 30, 10);
        // 30 wide - 2 border columns; 10 tall - 1 prompt - 3 input - 2 border.
        assert_eq!(session_pane_sizes(area, &state), vec![(0, (28, 4))]);

        state.sessions[0].connected = false;
        assert_eq!(session_pane_sizes(area, &state), vec![(0, (28, 5))]);
    }

    /// Split panes each report their own width, not the terminal's — two
    /// characters side by side must not both think they have 80 columns
    /// (docs/ARCHITECTURE.md §6.2).
    #[test]
    fn split_panes_report_their_own_widths() {
        let mut state = test_support::app(&["tank", "cleric"]);
        state.layout = LayoutMode::Splits;

        let sizes = session_pane_sizes(Rect::new(0, 0, 80, 20), &state);
        assert_eq!(sizes.len(), 2);
        assert_eq!(sizes[0].1.0, 38, "half of 80, minus borders");
        assert_eq!(sizes[1].1.0, 38);
    }

    /// Tabs mode hides the other sessions, but their size is what they would
    /// occupy on focus — losing focus must not renegotiate NAWS.
    #[test]
    fn tabbed_panes_all_report_the_full_pane_size() {
        let state = test_support::app(&["tank", "cleric"]);
        let sizes = session_pane_sizes(Rect::new(0, 0, 80, 20), &state);
        assert_eq!(sizes[0].1, sizes[1].1);
    }

    /// Splits put both characters on screen at once — the M7 design point.
    #[test]
    fn splits_draw_every_session_at_once() {
        let mut state = test_support::app(&["tank", "cleric"]);
        state.layout = LayoutMode::Splits;
        state.sessions[0]
            .scrollback
            .push_back("tankline".to_string());
        state.sessions[1]
            .scrollback
            .push_back("clericline".to_string());

        let buffer = render_sized(&state, 60, 10);
        let joined: String = (0..10).map(|y| row(&buffer, y)).collect();
        assert!(joined.contains("tankline"), "{joined}");
        assert!(joined.contains("clericline"), "{joined}");
    }

    /// Tabs show one session and a tab bar naming them all.
    #[test]
    fn tabs_show_only_the_focused_session() {
        let mut state = test_support::app(&["tank", "cleric"]);
        state.sessions[0]
            .scrollback
            .push_back("tankline".to_string());
        state.sessions[1]
            .scrollback
            .push_back("clericline".to_string());

        let buffer = render_sized(&state, 60, 12);
        let joined: String = (0..12).map(|y| row(&buffer, y)).collect();
        assert!(joined.contains("tankline"), "{joined}");
        assert!(
            !joined.contains("clericline"),
            "an unfocused tab must not draw: {joined}"
        );
        assert!(row(&buffer, 0).contains("1: tank"), "{:?}", row(&buffer, 0));
        assert!(
            row(&buffer, 0).contains("2: cleric"),
            "{:?}",
            row(&buffer, 0)
        );
    }

    /// Unread output on a background session is visible without switching to
    /// it (§11).
    #[test]
    fn an_unfocused_session_shows_an_unread_badge() {
        let mut state = test_support::app(&["tank", "cleric"]);
        state.sessions[1].unread = 3;

        let buffer = render_sized(&state, 60, 12);
        assert!(row(&buffer, 0).contains("●3"), "{:?}", row(&buffer, 0));
    }

    /// Focusing a channel pane must not change where typing goes, and the
    /// input border has to say so (§11.1).
    #[test]
    fn the_input_border_names_the_bound_session() {
        let mut state = test_support::app(&["tank", "cleric"]);
        state.channels.push(ChannelPane {
            config: test_support::channel("comms"),
            lines: VecDeque::new(),
            unread: 0,
            scrollback_limit: 10_000,
            back_offset: 0,
        });
        state.show_channels = true;
        state.focus_pane(Focus::Session(1));
        state.focus_pane(Focus::Channel(0));

        let buffer = render_sized(&state, 70, 12);
        let input_row = row(&buffer, 9);
        assert!(input_row.contains("input → cleric"), "{input_row:?}");
    }

    /// Channel panes dock beside the session panes and carry their own
    /// unread badge (§11.1).
    #[test]
    fn channel_panes_dock_beside_the_sessions() {
        let mut state = state();
        let mut channel = ChannelPane {
            config: test_support::channel("comms"),
            lines: VecDeque::new(),
            unread: 2,
            scrollback_limit: 10_000,
            back_offset: 0,
        };
        channel.lines.push_back("Bob tells you hi".to_string());
        state.channels.push(channel);
        state.show_channels = true;

        let buffer = render_sized(&state, 70, 12);
        let joined: String = (0..12).map(|y| row(&buffer, y)).collect();
        assert!(joined.contains("comms ●2"), "{joined}");
        assert!(joined.contains("Bob tells you hi"), "{joined}");
    }

    /// One draw of a static, already-overflowing state: exercises the
    /// scroll-offset math (`wrapped_rows` vs viewport height) in isolation,
    /// including a line whose real word-wrapped row count diverges from a
    /// naive width division.
    #[test]
    fn scroll_offset_matches_real_wrap_count() {
        let mut state = state();
        state.sessions[0].scrollback.push_back(format!(
            "{} {} {}",
            "a".repeat(4),
            "b".repeat(24),
            "c".repeat(4)
        ));
        for i in 0..10 {
            state.sessions[0].scrollback.push_back(format!("line {i}"));
        }
        state.sessions[0].prompt = "By what name do you wish to be known?".to_string();

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
            state.sessions[0].scrollback.push_back(line.to_string());
            terminal.draw(|frame| draw(frame, &state)).unwrap();
            assert_left_border_intact(terminal.backend().buffer());
        }

        state.sessions[0].prompt = "By what name do you wish to be known?".to_string();
        terminal.draw(|frame| draw(frame, &state)).unwrap();
        assert_left_border_intact(terminal.backend().buffer());

        state.sessions[0].prompt.clear();
        state.sessions[0]
            .scrollback
            .push_back("> crazy-foo".to_string());
        for line in ["Password:", "Reconnecting.", "", "i107 >"] {
            state.sessions[0].scrollback.push_back(line.to_string());
            terminal.draw(|frame| draw(frame, &state)).unwrap();
            assert_left_border_intact(terminal.backend().buffer());
        }
    }

    // ---- scrollback navigation (§11.5) ----

    /// The scrolled indicator is distinct from the unread badge and shows
    /// only when the pane isn't pinned to the tail — including on a
    /// focused pane, which the unread badge deliberately never marks.
    #[test]
    fn a_scrolled_pane_shows_an_indicator_the_tail_does_not() {
        let mut state = state();
        for i in 0..20 {
            state.sessions[0].scrollback.push_back(format!("line {i}"));
        }

        let at_tail = rows(&render_sized(&state, 40, 12));
        assert!(
            !at_tail.contains("scrolled"),
            "a tailed pane must show no indicator: {at_tail}"
        );

        state.sessions[0].back_offset = 5;
        let scrolled = rows(&render_sized(&state, 40, 12));
        assert!(
            scrolled.contains("scrolled"),
            "a scrolled pane must show an indicator: {scrolled}"
        );
    }

    /// `back_offset` is clamped at render time to `[0, max_scroll]`: an
    /// offset far past the top must not panic and must land on the oldest
    /// line, not scroll past it into blank space.
    #[test]
    fn an_oversized_back_offset_clamps_to_the_true_top() {
        let mut state = state();
        for i in 0..20 {
            state.sessions[0].scrollback.push_back(format!("line {i}"));
        }
        state.sessions[0].back_offset = usize::MAX;

        let buffer = render_sized(&state, 40, 12);
        assert!(
            rows(&buffer).contains("line 0"),
            "the oldest line must be reachable, not scrolled past: {}",
            rows(&buffer)
        );
    }

    /// The GMCP inspector (§14 M6) is a different buffer from the
    /// scrollback a `back_offset` was set against — toggling into it must
    /// always show its own tail, not silently inherit an offset that
    /// belongs to unrelated content with no indicator to explain why.
    #[test]
    fn toggling_the_gmcp_inspector_ignores_the_scrollbacks_scroll_position() {
        let mut state = state();
        // More entries than the 4 content rows `render` gives (see
        // `CONTENT_ROWS`), so a wrongly-applied offset would visibly hide
        // the tail rather than just failing to matter.
        for i in 0..8 {
            state.sessions[0].gmcp_log.push_back(format!("gmcp-{i}"));
        }
        // A large offset set while looking at the scrollback, carried
        // along when the player then hits F2.
        state.sessions[0].back_offset = usize::MAX;
        state.show_gmcp = true;

        let buffer = render(&state);
        assert!(
            rows(&buffer).contains("gmcp-7"),
            "the inspector must show its own tail: {}",
            rows(&buffer)
        );
    }
}
