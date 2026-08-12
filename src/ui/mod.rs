//! Ratatui widgets: pane grid, tab bar, channel panes, and the input line.
//!
//! Layout is either tabs (the focused session full-screen) or splits (every
//! session side by side), with channel panes docked in a column beside them
//! (docs/ARCHITECTURE.md §11/§11.1). One input line sits at the bottom bound
//! to the last focused *session*, showing that binding in its border, so
//! focusing a channel pane never redirects what you type. ANSI/TrueColor SGR
//! sequences are rendered via `ansi-to-tui`; unknown/malformed escapes are
//! dropped rather than shown raw.

use std::collections::VecDeque;

use ansi_to_tui::IntoText;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Paragraph, Wrap};

use crate::app::{AppState, ChannelPane, Focus, LayoutMode};
use crate::config::Keybinds;
use crate::scrollback::{Origin, RetainedLine};

pub mod config_editor;
mod map_render;

use map_render::MapRenderer as _;

/// Default width of the docked channel column, and the smallest main area
/// worth keeping beside it — below that the channels are simply not drawn.
/// The live width is `AppState::channel_width`; this is only where it starts
/// (docs/ARCHITECTURE.md §11.4).
pub(crate) const CHANNEL_WIDTH: u16 = 28;
const MIN_MAIN_WIDTH: u16 = 30;
/// The narrowest the column may be resized to: enough for a channel name and
/// a couple of words inside the border. Shrinking a pane to nothing is a way
/// to lose a pane you cannot then find — hiding channels is what the toggle
/// key is for (§11.4).
pub(crate) const MIN_CHANNEL_WIDTH: u16 = 14;

/// Clamps a requested channel width to what a terminal `area_width` wide can
/// hold: never under `MIN_CHANNEL_WIDTH`, never so wide that the session area
/// falls below `MIN_MAIN_WIDTH`. On a terminal too narrow for both, the floor
/// wins and `layout` declines to draw the channels at all (§11.4).
pub(crate) fn clamp_channel_width(width: u16, area_width: u16) -> u16 {
    let max = area_width
        .saturating_sub(MIN_MAIN_WIDTH)
        .max(MIN_CHANNEL_WIDTH);
    width.clamp(MIN_CHANNEL_WIDTH, max)
}

/// Default width of the docked map column, and the narrowest it may be
/// resized to. The live width is `AppState::map_width` (§11.4, §16).
/// Narrower than the comms floor because a map row is glyphs and
/// connectors, not words — but not so narrow that the current room can
/// never be centred with a neighbour either side.
pub(crate) const MAP_WIDTH: u16 = 24;
pub(crate) const MIN_MAP_WIDTH: u16 = 12;

/// As `clamp_channel_width`, for the map column.
pub(crate) fn clamp_map_width(width: u16, area_width: u16) -> u16 {
    let max = area_width.saturating_sub(MIN_MAIN_WIDTH).max(MIN_MAP_WIDTH);
    width.clamp(MIN_MAP_WIDTH, max)
}

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
        row(keybinds.focus_next, "cycle focus, comms included"),
        String::new(),
        "Views".to_string(),
        row(keybinds.cycle_layout, "tabs / side-by-side layout"),
        row(keybinds.toggle_channels, "show or hide comms"),
        row(keybinds.channel_wider, "widen the comms column"),
        row(keybinds.channel_narrower, "narrow the comms column"),
        row(keybinds.toggle_map, "show or hide the map column"),
        row(keybinds.map_wider, "widen the map column"),
        row(keybinds.map_narrower, "narrow the map column"),
        row(
            keybinds.server_data_inspector,
            "raw server-data inspector (GMCP/MSDP)",
        ),
        row(keybinds.help, "this help"),
        row(keybinds.config_editor, "edit this character's profile"),
        row(keybinds.reload, "recompile rules and scripts from disk"),
        row(
            keybinds.line_picker,
            "pick a scrollback line for a new trigger",
        ),
        String::new(),
        "Commands you can type".to_string(),
        row("/help", "print this help into the pane"),
        row("/reload", "recompile rules and scripts from disk"),
        row("/config", "edit this character's profile"),
        row("/newprofile", "create another character's profile"),
        row("/connect", "add a character to this running instance"),
        row("/goto", "walk to a known room, one step at a time"),
        row("/corpse", "walk back to where you last died"),
        row(
            "/mark",
            "label this room on the map (`/mark` alone clears it)",
        ),
        row("/map", "show or hide the map, and describe this room"),
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
    /// The docked map column, when the player has it on and the terminal
    /// has room for it beside everything already there (§16).
    pub map: Option<Rect>,
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
        && body.width >= MIN_MAIN_WIDTH + state.channel_width;
    // Comms wins a tie: it was already on screen, and dropping it to make
    // room for a column the player just asked for would move furniture
    // they did not touch.
    let channel_cost = if show_channels {
        state.channel_width
    } else {
        0
    };
    let show_map = state.show_map && body.width >= MIN_MAIN_WIDTH + channel_cost + state.map_width;

    let mut constraints = vec![Constraint::Min(MIN_MAIN_WIDTH)];
    if show_channels {
        constraints.push(Constraint::Length(state.channel_width));
    }
    if show_map {
        constraints.push(Constraint::Length(state.map_width));
    }
    let columns = Layout::horizontal(constraints).split(body);
    let main = columns[0];
    let channel_column = show_channels.then(|| columns[1]);
    let map = show_map.then(|| columns[columns.len() - 1]);

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
        map,
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
            state,
        );
    }

    if let Some(rect) = panes.map {
        draw_map(frame, rect, state);
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

    if state.config_editor.is_none() {
        draw_input(frame, panes.input, state);
    }

    if state.show_help {
        draw_help(frame, frame.area(), &state.keybinds);
    }

    if let Some(menu) = &state.mark_menu {
        draw_mark_menu(frame, frame.area(), menu);
    }

    if let Some(editor) = &state.config_editor {
        editor.draw(frame, frame.area());
    }

    if let Some(wizard) = &state.new_profile_wizard {
        draw_new_profile_wizard(
            frame,
            &wizard.answered,
            wizard.step.prompt(),
            wizard.input.value(),
            wizard.input.visual_cursor(),
            wizard.error.as_deref(),
        );
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

/// The `/mark` chooser (§16): what is this room *for*?
///
/// Offered because nothing on the wire answers that — a Diku MUD's MSDP
/// carries no shop flag and no terrain — so the label is always the
/// player's, and the least a client can do is not make them remember what
/// they called the last one. Numbered, because nine rows is short enough
/// that counting beats arrowing.
fn draw_mark_menu(frame: &mut Frame, area: Rect, menu: &crate::app::MarkMenu) {
    // Typing a label of your own replaces the list: the choice has been
    // made, and what is left is one field, so the list would only be
    // something to read past.
    let lines: Vec<Line> = match &menu.typing {
        Some(typed) => vec![
            Line::raw(" what is this room for?"),
            Line::styled(
                format!(" {typed}▏"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Line::styled(
                " Enter to mark, Esc to go back",
                Style::default().add_modifier(Modifier::DIM),
            ),
        ],
        None => menu
            .entries()
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                let row = format!(" {}  {entry:<16}", i + 1);
                match i == menu.selected {
                    true => Line::styled(row, Style::default().add_modifier(Modifier::REVERSED)),
                    false => Line::raw(row),
                }
            })
            .collect(),
    };

    let width = 24.min(area.width);
    let height = (lines.len() as u16 + 2).min(area.height);
    let overlay = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    let block = Block::bordered().title(format!(" mark #{} ", menu.at.0).bold());
    frame.render_widget(ratatui::widgets::Clear, overlay);
    frame.render_widget(Paragraph::new(Text::from(lines)).block(block), overlay);
}

/// The "new profile" wizard (docs/ARCHITECTURE.md §15): one field at a
/// time, with what's already been answered shown above it, so filling it
/// in never risks fat-fingering a form whose other fields are out of
/// sight. Shared by two callers — the first-run screen, which draws over
/// an empty terminal since no session exists yet, and `/newprofile`
/// (`AppState::new_profile_wizard`), which draws this same overlay on top
/// of live panes via `ratatui::widgets::Clear`.
pub fn draw_new_profile_wizard(
    frame: &mut Frame,
    answered: &[(&str, String)],
    prompt: &str,
    value: &str,
    cursor: usize,
    error: Option<&str>,
) {
    let area = frame.area();
    let mut lines = vec![
        "Let's connect to a MUD — no YAML required.".to_string(),
        String::new(),
    ];
    for (label, value) in answered {
        lines.push(format!("{label}: {value}"));
    }
    if let Some(error) = error {
        lines.push(String::new());
        lines.push(format!("** {error}"));
    }
    lines.push(String::new());
    // The input line is always last, so the cursor row is just its index —
    // no separate bookkeeping to keep in step with what's above it.
    let input_row = lines.len() as u16;
    lines.push(format!("{prompt}: {value}"));

    let content_width = lines
        .iter()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(0)
        .max(44);
    let width = (content_width as u16 + 4).min(area.width);
    let height = (lines.len() as u16 + 2).min(area.height);
    let overlay = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    let text = Text::from(lines.into_iter().map(Line::from).collect::<Vec<_>>());
    let block = Block::bordered().title(" New profile — Esc to cancel ".bold());
    frame.render_widget(ratatui::widgets::Clear, overlay);
    frame.render_widget(Paragraph::new(text).block(block), overlay);

    let max_x = overlay.x + overlay.width.saturating_sub(1);
    let cursor_x = overlay.x + 1 + prompt.chars().count() as u16 + 2 + cursor as u16;
    frame.set_cursor_position((cursor_x.min(max_x), overlay.y + 1 + input_row));
}

fn draw_session(frame: &mut Frame, area: Rect, state: &AppState, index: usize) {
    let session = &state.sessions[index];
    let focused = state.is_focused_session(index);
    let showing_inspector = state.show_inspector && focused;

    // The scrollback line-cursor (docs/ARCHITECTURE.md §10.2/§11.5) only
    // ever picks from the input-bound session, regardless of which pane is
    // visually focused — same scoping as `Alt+V` itself in `app.rs`.
    let picked_line = (!showing_inspector && index == state.input_session)
        .then_some(state.line_cursor)
        .flatten()
        .and_then(|cursor| session.scrollback.len().checked_sub(1 + cursor));

    let content_width = area.width.saturating_sub(2);

    // Security and latency are each absent until they are known, so a pane
    // never shows an empty bracket or a placeholder round trip.
    let security = match session.security.is_empty() {
        true => String::new(),
        false => format!(" [{}]", session.security),
    };
    let latency = match session.latency.is_empty() {
        true => String::new(),
        false => format!(" {}", session.latency),
    };
    let title = if showing_inspector {
        format!(" {} — {} ", session.name, session.inspector_title())
    } else {
        let picking = if picked_line.is_some() {
            " ↑↓ pick a line, Enter for a trigger, Esc to cancel"
        } else {
            scroll_indicator(session.back_offset)
        };
        format!(
            "{} — {}{security}{latency}{picking} ",
            pane_title(&session.name, session.unread),
            session.status,
        )
    };

    // The inspector is a different buffer (`inspector_log`, not
    // `scrollback`) — a scroll position set for one has nothing to say
    // about the other, so toggling into it always shows its own tail
    // rather than silently inheriting an offset that would land somewhere
    // unrelated with no indicator to explain why (§11.5 scopes navigation
    // to scrollback).
    let back_offset = if showing_inspector {
        0
    } else {
        session.back_offset
    };

    let (lines, scroll) = if showing_inspector {
        visible_window(
            &session.inspector_log,
            area.height,
            content_width,
            back_offset,
            |_, raw| vec![Line::raw(raw.to_string())],
        )
    } else {
        visible_window(
            &session.scrollback,
            area.height,
            content_width,
            back_offset,
            |i, line| {
                let rendered = ansi_lines(&line.text);
                if Some(i) != picked_line {
                    return rendered;
                }
                // `REVERSED` rather than a fixed colour: it reads clearly
                // over whatever ANSI colours the server's own line already
                // has, instead of clashing with or hiding them.
                rendered
                    .into_iter()
                    .map(|line| line.patch_style(Style::default().add_modifier(Modifier::REVERSED)))
                    .collect()
            },
        )
    };

    render_scrollback(frame, area, lines, title, focused, session.color, scroll);
}

/// Draws the area around the character, from the scene the map builds.
///
/// Only a *view*: every fact shown comes from `Map::scene`, and the prose
/// form of the same knowledge is `Map::describe` (§16). This function owns
/// the pane — its border, its title, and what to say when there is nothing
/// to draw — and hands the picture itself to a [`MapRenderer`].
fn draw_map(frame: &mut Frame, area: Rect, state: &AppState) {
    let session = state.bound();
    let title = match session.and_then(|session| session.current_room) {
        Some(at) => session
            .and_then(|session| session.map.rooms.get(&at))
            .and_then(|room| room.area.clone())
            .unwrap_or_else(|| "map".to_string()),
        None => "map".to_string(),
    };
    let block = Block::bordered().title(format!(" {title} "));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let (Some(session), Some(current)) = (session, session.and_then(|s| s.current_room)) else {
        frame.render_widget(
            Paragraph::new("no room data yet").wrap(Wrap { trim: true }),
            inner,
        );
        return;
    };

    let scene = session.map.scene(current, session.corpse);
    map_render::CharRenderer.draw(frame, inner, &scene);
}

fn draw_channel(
    frame: &mut Frame,
    area: Rect,
    channel: &ChannelPane,
    focused: bool,
    state: &AppState,
) {
    let timestamps = channel.config.timestamps;
    // Which character said it, in that character's own colour — the pane
    // itself has no single owner, but every line in it does.
    let tint = |name: &str| {
        state
            .sessions
            .iter()
            .find(|session| session.name == name)
            .and_then(|session| session.color)
    };
    let (lines, scroll) = visible_window(
        &channel.lines,
        area.height,
        area.width.saturating_sub(2),
        channel.back_offset,
        |_, line| channel_line(line, timestamps, &tint),
    );
    let title = format!(
        "{}{} ",
        pane_title(&channel.config.name, channel.unread),
        scroll_indicator(channel.back_offset)
    );
    // Channels aggregate across characters, so no one profile's colour
    // could stand for the pane.
    render_scrollback(frame, area, lines, title, focused, None, scroll);
}

/// A channel line as the pane shows it. The `HH:MM:SS` and the `[character]`
/// tag are composed here from the line's own `at` and `origin`, rather than
/// spliced into its text when it was routed (§8): the pane's `timestamps:`
/// setting then decides at render time, and the stored line stays the text
/// the MUD actually sent.
fn channel_line(
    line: &RetainedLine,
    timestamps: bool,
    tint: &dyn Fn(&str) -> Option<Color>,
) -> Vec<Line<'static>> {
    let mut prefix: Vec<Span<'static>> = Vec::new();
    if timestamps {
        // Local, not UTC: a clock silently mislabeled as local would be
        // wrong for most players, every day, all year.
        prefix.push(Span::raw(line.at.format("%H:%M:%S ").to_string()));
    }
    if let Origin::Session(name) = &line.origin {
        // The character's own colour, the same one that tints their pane
        // border and tab (§11), so a channel that several characters talk
        // into can be read by who is talking rather than by squinting at
        // names. Uncoloured profiles keep the plain tag.
        let tag = format!("[{name}] ");
        prefix.push(match tint(name) {
            Some(color) => Span::styled(tag, Style::default().fg(color)),
            None => Span::raw(tag),
        });
    }
    if prefix.is_empty() {
        return ansi_lines(&line.text);
    }
    // Only the first row is prefixed; a wrapped continuation is the same
    // line, and repeating the stamp down its rows would read as several.
    let mut rendered = ansi_lines(&line.text);
    match rendered.first_mut() {
        Some(first) => {
            for span in prefix.into_iter().rev() {
                first.spans.insert(0, span);
            }
        }
        None => rendered.push(Line::from(prefix)),
    }
    rendered
}

/// `↑ scrolled` when a pane isn't pinned to the tail — distinct from the
/// unread badge (`● N`), and shown even on a focused pane, which the unread
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

/// How many rows `lines` occupy once wrapped at `width`. Uses ratatui's
/// real wrap algorithm rather than an estimate, for the same reason the
/// renderer always has: a long line's tail must never be silently
/// truncated.
///
/// Safe to call per source line and sum, because `Wrap` splits a `Line`
/// into rows but never joins two `Line`s — so the rows of a slice are
/// exactly the rows of its parts. `window_is_row_exact_against_the_full_buffer`
/// pins that.
fn wrapped_rows(lines: &[Line<'static>], width: u16) -> usize {
    if width == 0 || lines.is_empty() {
        return lines.len();
    }
    Paragraph::new(Text::from(lines.to_vec()))
        .wrap(Wrap { trim: false })
        .line_count(width)
}

/// Parses and returns only the tail of `raws` that the viewport can
/// actually show at `back_offset`, plus the scroll offset to apply within
/// that window.
///
/// Rendering used to parse and wrap the *entire* buffer every frame, which
/// made per-frame cost O(buffered lines × panes) — and every inbound line
/// is a frame, so a spammy MUD with full 10,000-line buffers paid tens of
/// thousands of ANSI parses and a whole-`Text` clone per arriving line.
/// That is the worst-case latency profile §2.1 chose this stack to avoid,
/// and §11.1's "cost scales with the viewport" only becomes true with this
/// walk: stop as soon as enough rows are covered, newest first.
///
/// Cost is O(visible rows) while a pane is tailing, and O(visible rows +
/// `back_offset`) while scrolled back — proportional to what the player
/// asked to see, rather than to everything they have ever seen.
fn visible_window<T>(
    raws: &VecDeque<T>,
    area_height: u16,
    content_width: u16,
    back_offset: usize,
    parse: impl Fn(usize, &T) -> Vec<Line<'static>>,
) -> (Vec<Line<'static>>, u16) {
    let viewport = area_height.saturating_sub(2) as usize; // borders
    let needed = viewport.saturating_add(back_offset);

    let mut window: Vec<Line<'static>> = Vec::new();
    let mut rows = 0usize;
    // Distinguishes "stopped because the viewport is covered" from "ran out
    // of buffer" — only the latter knows the true total, and only the
    // latter therefore has to clamp `back_offset` (below).
    let mut exhausted = true;

    for (i, raw) in raws.iter().enumerate().rev() {
        if rows >= needed {
            exhausted = false;
            break;
        }
        let mut parsed = parse(i, raw);
        rows += wrapped_rows(&parsed, content_width);
        parsed.append(&mut window);
        window = parsed;
    }

    let scroll = if exhausted {
        // The whole buffer was walked, so `rows` is the exact total and
        // this is the original clamp verbatim: a `back_offset` past the top
        // pins to the top rather than scrolling into blank space (§11.5).
        let max_scroll = rows.saturating_sub(viewport);
        max_scroll.saturating_sub(back_offset.min(max_scroll))
    } else {
        // More lines exist above the window, so `back_offset` is in range
        // and the window's own top is the only reference needed.
        rows.saturating_sub(viewport).saturating_sub(back_offset)
    };

    (window, scroll as u16)
}

/// Renders a bordered pane over an already-windowed `lines` and the
/// `scroll` [`visible_window`] computed for it.
fn render_scrollback(
    frame: &mut Frame,
    area: Rect,
    lines: Vec<Line>,
    title: String,
    focused: bool,
    color: Option<Color>,
    scroll: u16,
) {
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
    let body = Paragraph::new(Text::from(lines))
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
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

/// `name ● 3` — the unread badge for a pane that isn't focused (§11). The
/// space before the count matters: `●` commonly renders wider than the
/// single cell it's measured as, and packed straight against a digit it
/// visually collides with it in most terminal fonts.
fn pane_title(name: &str, unread: usize) -> String {
    if unread == 0 {
        format!(" {name}")
    } else {
        format!(" {name} ● {unread}")
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
    use crate::config::Channel;

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

    /// How many cells the map painted as room ground. Rooms are a
    /// background fill rather than a glyph, so counting ink means counting
    /// backgrounds, not characters.
    fn filled_cells(buffer: &ratatui::buffer::Buffer) -> usize {
        (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| (x, y)))
            .filter(|(x, y)| buffer.cell((*x, *y)).unwrap().bg != ratatui::style::Color::Reset)
            .count()
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

    // ---- the map column (§16) ----

    /// Drops a room into a session's map with the exits it should have.
    fn map_room(state: &mut AppState, id: i64, exits: &[(&str, i64)]) {
        state.sessions[0].map.rooms.insert(
            crate::map::RoomId(id),
            crate::map::Room {
                id: crate::map::RoomId(id),
                mark: None,
                name: None,
                area: Some("Test".to_string()),
                exits: exits
                    .iter()
                    .map(|(dir, dest)| (dir.to_string(), Some(crate::map::RoomId(*dest))))
                    .collect(),
            },
        );
    }

    /// The bug this guards: on a MUD with only n/s/e/w, the map column used
    /// to sprout diagonal corridors. `layout_area` sites each room once, so
    /// a second exit into an already-placed room can have any delta — and
    /// the renderer was reading that delta as if it were the direction.
    #[test]
    fn a_six_direction_mud_never_draws_a_diagonal_corridor() {
        let mut state = state();
        state.show_map = true;
        map_room(&mut state, 1, &[("e", 2), ("n", 4)]);
        map_room(&mut state, 2, &[("s", 4)]);
        map_room(&mut state, 4, &[]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));

        let buffer = render_sized(&state, 60, 14);
        let text = rows(&buffer);

        assert!(
            !text.contains('\\') && !text.contains('/'),
            "no exit here is diagonal, so no diagonal may be drawn:\n{text}"
        );
    }

    /// The complementary half, so the guard above is not mistaken for the
    /// renderer having stopped drawing corridors: an exit whose rooms *did*
    /// land where it says they lie is still connected.
    #[test]
    fn an_exit_the_layout_honoured_is_still_drawn() {
        let mut state = state();
        state.show_map = true;
        map_room(&mut state, 1, &[("e", 2)]);
        map_room(&mut state, 2, &[]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));

        let buffer = render_sized(&state, 60, 14);
        let text = rows(&buffer);

        assert!(
            text.contains('─'),
            "an honoured east exit should still draw its corridor:\n{text}"
        );
    }

    /// A genuine diagonal on a MUD that has them is drawn as one — the fix
    /// is about trusting the direction, not about refusing diagonals.
    #[test]
    fn a_real_diagonal_exit_is_still_drawn_diagonally() {
        let mut state = state();
        state.show_map = true;
        map_room(&mut state, 1, &[("se", 2)]);
        map_room(&mut state, 2, &[]);
        state.sessions[0].current_room = Some(crate::map::RoomId(1));

        let buffer = render_sized(&state, 60, 14);
        let text = rows(&buffer);

        assert!(
            text.contains('\\'),
            "a real southeast exit still gets its diagonal:\n{text}"
        );
    }

    /// The prompt belongs on its own row above the input box, not in the
    /// scrollback — "prompts render as prompts" (docs/ARCHITECTURE.md §14).
    #[test]
    fn pins_the_prompt_above_the_input_line() {
        let mut state = state();
        state.sessions[0]
            .scrollback
            .push_back(RetainedLine::server("You are in a forest."));
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
    /// server-data log while toggled on.
    #[test]
    fn shows_the_inspector_log_instead_of_scrollback_when_toggled() {
        let mut state = state();
        state.sessions[0]
            .scrollback
            .push_back(RetainedLine::server("You are in a forest."));
        state.sessions[0]
            .inspector_log
            .push_back(r#"[GMCP] Char.Vitals {"hp":100}"#.to_string());
        state.show_inspector = true;

        let buffer = render(&state);
        assert!(row(&buffer, 1).contains("Char.Vitals"));
        assert!(!row(&buffer, 1).contains("forest"));
    }

    /// §13: server data is untrusted, and the inspector is the one view
    /// that shows it verbatim, for both protocols. A payload carrying
    /// escape sequences must be *shown*, not executed — ratatui writes a
    /// cell's symbol to the terminal unfiltered, so an ESC that reaches a
    /// cell is a real escape injection out of a subnegotiation the player
    /// only has to press a key to look at.
    #[test]
    fn the_gmcp_inspector_never_writes_a_control_byte_to_the_terminal() {
        let mut state = state();
        state.sessions[0].push_gmcp(
            "Char.Vitals".to_string(),
            Some("{\"hp\":\x1b[2J\x1b]0;pwned\x07100}".to_string()),
        );
        state.show_inspector = true;

        let buffer = render(&state);
        let offenders: Vec<&str> = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .filter(|symbol| symbol.chars().any(|c| c.is_control()))
            .collect();
        assert!(
            offenders.is_empty(),
            "control bytes reached the terminal: {offenders:?}"
        );
        // Shown, not silently dropped: the inspector exists to reveal what
        // the server actually sent.
        assert!(rows(&buffer).contains("x1b"), "{}", rows(&buffer));
    }

    /// The MSDP twin of the test above: MSDP keys and values are just as
    /// untrusted as a GMCP payload, and share the same render path (§13).
    #[test]
    fn the_msdp_inspector_never_writes_a_control_byte_to_the_terminal() {
        let mut state = state();
        state.sessions[0].push_msdp(vec![(
            "hp".to_string(),
            "\x1b[2J\x1b]0;pwned\x07100".to_string(),
        )]);
        state.show_inspector = true;

        let buffer = render(&state);
        let offenders: Vec<&str> = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .filter(|symbol| symbol.chars().any(|c| c.is_control()))
            .collect();
        assert!(
            offenders.is_empty(),
            "control bytes reached the terminal: {offenders:?}"
        );
        assert!(rows(&buffer).contains("x1b"), "{}", rows(&buffer));
    }

    /// §11: the pane title carries the latency, and carries nothing where
    /// it would be before the first round trip has been measured.
    #[test]
    fn the_pane_title_shows_latency_once_there_is_one() {
        let mut state = state();
        state.sessions[0].status = "connected".to_string();
        state.sessions[0].security = "TLS".to_string();

        let before = row(&render_sized(&state, 60, 12), 0);
        assert!(before.contains("[TLS]"), "title: {before:?}");
        assert!(!before.contains("ms"), "title: {before:?}");

        state.sessions[0].latency = "42ms".to_string();
        let after = row(&render_sized(&state, 60, 12), 0);
        assert!(after.contains("[TLS] 42ms"), "title: {after:?}");
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

    /// The unread badge is the one place the eye jumps to first when
    /// something happens elsewhere — it should carry the same
    /// per-character colour identity the rest of the tab does, not a
    /// fixed colour layered on top of it (UX_REVIEW.md H).
    #[test]
    fn the_unread_badge_matches_the_pane_color() {
        let mut state = test_support::app(&["tank", "cleric"]);
        state.sessions[1].color = Some(Color::Magenta);
        state.sessions[1].unread = 3;

        let buffer = render_sized(&state, 40, 12);
        let badge = buffer
            .content()
            .iter()
            .take(40)
            .find(|cell| cell.symbol() == "●")
            .expect("the unread badge should render in the tab bar");
        assert_eq!(badge.fg, Color::Magenta);
    }

    /// The tab bar only exists in Tabs mode (`>1` session). In Splits, an
    /// unfocused pane's own unread badge is the only place the count shows
    /// at all — it should carry the same colour identity, for the same
    /// reason (UX_REVIEW.md H).
    #[test]
    fn splits_mode_unread_badge_matches_the_pane_color() {
        let mut state = test_support::app(&["tank", "cleric"]);
        state.layout = LayoutMode::Splits;
        state.sessions[1].color = Some(Color::Magenta);
        state.sessions[1].unread = 3;

        let buffer = render_sized(&state, 60, 12);
        let badge = buffer
            .content()
            .iter()
            .find(|cell| cell.symbol() == "●")
            .expect("the unread badge should render in cleric's own border title");
        assert_eq!(badge.fg, Color::Magenta);
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

    /// `help_lines`'s own doc comment promises "every binding" — a
    /// keybind added without a matching `row(...)` here is exactly the
    /// gap that let `reload` (§14 M9, UX_REVIEW.md F) ship discoverable
    /// only by reading the source, not by pressing F1.
    #[test]
    fn help_lines_lists_the_reload_keybind() {
        let lines = help_lines(&Keybinds::default());
        assert!(
            lines.iter().any(|line| line.contains("F6")),
            "the reload keybind must be listed in the help overlay: {lines:?}"
        );
    }

    /// The overlay covers what is under it — a listing rendered over live
    /// scrollback would be unreadable (docs/ARCHITECTURE.md §11.2).
    #[test]
    fn the_help_overlay_draws_over_the_panes() {
        let mut state = state();
        for _ in 0..40 {
            state.sessions[0]
                .scrollback
                .push_back(RetainedLine::server("You are in a forest."));
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

    // ---- new-profile wizard (§15) ----

    fn render_wizard(
        answered: &[(&str, String)],
        prompt: &str,
        value: &str,
        cursor: usize,
        error: Option<&str>,
    ) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        terminal
            .draw(|frame| draw_new_profile_wizard(frame, answered, prompt, value, cursor, error))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn the_wizard_shows_prior_answers_above_the_current_prompt() {
        let answered = vec![("Name", "kestrel".to_string())];
        let buffer = render_wizard(&answered, "Host", "underworld", 10, None);
        let listing = rows(&buffer);
        assert!(listing.contains("Name: kestrel"), "{listing}");
        assert!(listing.contains("Host: underworld"), "{listing}");
    }

    #[test]
    fn the_wizard_shows_a_rejected_answer_as_an_error() {
        let buffer = render_wizard(&[], "Host", "", 0, Some("a host is required"));
        assert!(rows(&buffer).contains("a host is required"));
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
            .push_back(RetainedLine::server("tankline"));
        state.sessions[1]
            .scrollback
            .push_back(RetainedLine::server("clericline"));

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
            .push_back(RetainedLine::server("tankline"));
        state.sessions[1]
            .scrollback
            .push_back(RetainedLine::server("clericline"));

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
        assert!(row(&buffer, 0).contains("● 3"), "{:?}", row(&buffer, 0));
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
        channel
            .lines
            .push_back(RetainedLine::server("Bob tells you hi"));
        state.channels.push(channel);
        state.show_channels = true;

        let buffer = render_sized(&state, 70, 12);
        let joined: String = (0..12).map(|y| row(&buffer, y)).collect();
        assert!(joined.contains("comms ● 2"), "{joined}");
        assert!(joined.contains("Bob tells you hi"), "{joined}");
    }

    /// A comms pane aggregates several characters, so the `[name]` tag is
    /// the only thing saying who spoke — in that character's own colour,
    /// the same one tinting their pane border and tab (§11), so the pane
    /// can be read by who is talking rather than by squinting at names.
    #[test]
    fn a_channel_tags_each_line_in_the_speaker_s_own_colour() {
        let mut state = state();
        state.sessions[0].color = Some(Color::Magenta);
        let mut channel = ChannelPane {
            config: test_support::channel("comms"),
            lines: VecDeque::new(),
            unread: 0,
            scrollback_limit: 10_000,
            back_offset: 0,
        };
        channel.lines.push_back(RetainedLine::from_session(
            state.sessions[0].name.clone(),
            "Bob tells you hi",
        ));
        state.channels.push(channel);
        state.show_channels = true;

        let buffer = render_sized(&state, 70, 12);
        let tagged = (0..12)
            .flat_map(|y| (0..70).map(move |x| (x, y)))
            .find(|(x, y)| buffer.cell((*x, *y)).unwrap().symbol() == "[")
            .map(|(x, y)| buffer.cell((x, y)).unwrap().fg);

        assert_eq!(
            tagged,
            Some(Color::Magenta),
            "the tag should carry the character's colour: {}",
            (0..12).map(|y| row(&buffer, y)).collect::<String>()
        );
    }

    /// A profile with no colour set keeps a plain tag rather than picking
    /// one for it.
    #[test]
    fn an_uncoloured_profile_keeps_a_plain_tag() {
        let mut state = state();
        state.sessions[0].color = None;
        let mut channel = ChannelPane {
            config: test_support::channel("comms"),
            lines: VecDeque::new(),
            unread: 0,
            scrollback_limit: 10_000,
            back_offset: 0,
        };
        channel.lines.push_back(RetainedLine::from_session(
            state.sessions[0].name.clone(),
            "Bob tells you hi",
        ));
        state.channels.push(channel);
        state.show_channels = true;

        let buffer = render_sized(&state, 70, 12);
        let tagged = (0..12)
            .flat_map(|y| (0..70).map(move |x| (x, y)))
            .find(|(x, y)| buffer.cell((*x, *y)).unwrap().symbol() == "[")
            .map(|(x, y)| buffer.cell((x, y)).unwrap().fg);

        assert_eq!(tagged, Some(Color::Reset));
    }

    /// The timestamp is not stored in the line's text (§8): the pane owns
    /// the `timestamps:` setting, so the clock is composed here, from the
    /// line's own arrival time, and only on the pane that asked for one.
    #[test]
    fn a_timestamped_channel_composes_the_clock_from_the_lines_arrival_time() {
        let stored = "Bob tells you hi";
        let at = RetainedLine::server(stored)
            .at
            .format("%H:%M:%S")
            .to_string();

        for timestamps in [false, true] {
            let mut state = state();
            let mut channel = ChannelPane {
                config: Channel {
                    timestamps,
                    ..test_support::channel("comms")
                },
                lines: VecDeque::new(),
                unread: 0,
                scrollback_limit: 10_000,
                back_offset: 0,
            };
            channel.lines.push_back(RetainedLine::server(stored));
            state.channels.push(channel);
            state.show_channels = true;

            let buffer = render_sized(&state, 70, 12);
            let joined: String = (0..12).map(|y| row(&buffer, y)).collect();
            assert_eq!(
                joined.contains(&format!("{at} {stored}")),
                timestamps,
                "timestamps: {timestamps}\n{joined}"
            );
        }
    }

    /// With several characters open a routed line says which one it came
    /// from — composed from the line's `origin`, not spliced into its text
    /// when it was routed (§11.1).
    #[test]
    fn a_channel_line_from_another_character_is_tagged_with_its_name() {
        let mut state = state();
        let mut channel = ChannelPane {
            config: test_support::channel("comms"),
            lines: VecDeque::new(),
            unread: 0,
            scrollback_limit: 10_000,
            back_offset: 0,
        };
        channel
            .lines
            .push_back(RetainedLine::from_session("cleric", "Bob tells you hi"));
        state.channels.push(channel);
        state.show_channels = true;

        let buffer = render_sized(&state, 70, 12);
        let joined: String = (0..12).map(|y| row(&buffer, y)).collect();
        assert!(joined.contains("[cleric] Bob tells you hi"), "{joined}");
    }

    /// The column's width comes from `AppState`, not the constant — that is
    /// the whole point of §11.4's "state, not layout", and the session panes
    /// beside it give up exactly what the column takes.
    #[test]
    fn the_channel_column_takes_its_width_from_state() {
        let mut state = state();
        state.channels.push(ChannelPane {
            config: test_support::channel("comms"),
            lines: VecDeque::new(),
            unread: 0,
            scrollback_limit: 10_000,
            back_offset: 0,
        });
        state.show_channels = true;
        let area = Rect::new(0, 0, 80, 12);

        state.channel_width = 40;
        let panes = layout(area, &state);
        assert_eq!(panes.channels[0].width, 40);
        assert_eq!(
            panes.sessions[0].width, 40,
            "the main area gives up the rest"
        );

        state.channel_width = MIN_CHANNEL_WIDTH;
        let panes = layout(area, &state);
        assert_eq!(panes.channels[0].width, MIN_CHANNEL_WIDTH);
    }

    /// Clamps, not disappearances (§11.4): the width never falls below the
    /// floor nor squeezes the session area under `MIN_MAIN_WIDTH`, and a
    /// terminal too narrow for both drops the channels entirely rather than
    /// drawing a sliver of each.
    #[test]
    fn the_channel_width_clamps_and_the_column_vanishes_on_a_narrow_terminal() {
        assert_eq!(clamp_channel_width(2, 200), MIN_CHANNEL_WIDTH);
        assert_eq!(clamp_channel_width(500, 100), 100 - MIN_MAIN_WIDTH);
        assert_eq!(
            clamp_channel_width(28, 100),
            28,
            "a fitting width is left alone"
        );
        // Narrower than the floor plus a usable main area: the floor wins,
        // and `layout` is what declines to draw the column.
        assert_eq!(clamp_channel_width(28, 20), MIN_CHANNEL_WIDTH);

        let mut state = state();
        state.channels.push(ChannelPane {
            config: test_support::channel("comms"),
            lines: VecDeque::new(),
            unread: 0,
            scrollback_limit: 10_000,
            back_offset: 0,
        });
        state.show_channels = true;
        state.channel_width = MIN_CHANNEL_WIDTH;

        let wide = layout(
            Rect::new(0, 0, MIN_MAIN_WIDTH + MIN_CHANNEL_WIDTH, 12),
            &state,
        );
        assert_eq!(wide.channels.len(), 1);
        let narrow = layout(
            Rect::new(0, 0, MIN_MAIN_WIDTH + MIN_CHANNEL_WIDTH - 1, 12),
            &state,
        );
        assert!(narrow.channels.is_empty(), "channels are not drawn at all");
    }

    /// The map column gets the same discipline as the comms column (§11.4)
    /// and yields to it: comms was there first and the player did not ask
    /// for it to move, so a terminal with room for only one column keeps
    /// the one already on screen.
    #[test]
    fn the_map_width_clamps_and_the_column_yields_to_comms() {
        assert_eq!(clamp_map_width(2, 200), MIN_MAP_WIDTH);
        assert_eq!(clamp_map_width(500, 100), 100 - MIN_MAIN_WIDTH);
        assert_eq!(
            clamp_map_width(24, 100),
            24,
            "a fitting width is left alone"
        );
        assert_eq!(clamp_map_width(24, 20), MIN_MAP_WIDTH);

        let mut state = state();
        state.show_map = true;
        state.map_width = MIN_MAP_WIDTH;

        let wide = layout(Rect::new(0, 0, MIN_MAIN_WIDTH + MIN_MAP_WIDTH, 12), &state);
        assert!(wide.map.is_some());
        let narrow = layout(
            Rect::new(0, 0, MIN_MAIN_WIDTH + MIN_MAP_WIDTH - 1, 12),
            &state,
        );
        assert!(narrow.map.is_none(), "the map is not drawn at all");

        // Both columns asked for, only one fits: comms keeps its place.
        state.channels.push(ChannelPane {
            config: test_support::channel("comms"),
            lines: VecDeque::new(),
            unread: 0,
            scrollback_limit: 10_000,
            back_offset: 0,
        });
        state.show_channels = true;
        state.channel_width = MIN_CHANNEL_WIDTH;
        let cramped = layout(
            Rect::new(
                0,
                0,
                MIN_MAIN_WIDTH + MIN_CHANNEL_WIDTH + MIN_MAP_WIDTH - 1,
                12,
            ),
            &state,
        );
        assert_eq!(cramped.channels.len(), 1);
        assert!(cramped.map.is_none());

        let roomy = layout(
            Rect::new(0, 0, MIN_MAIN_WIDTH + MIN_CHANNEL_WIDTH + MIN_MAP_WIDTH, 12),
            &state,
        );
        assert_eq!(roomy.channels.len(), 1);
        let map = roomy.map.expect("both columns fit");
        assert_eq!(map.width, MIN_MAP_WIDTH);
        assert!(
            map.x > roomy.channels[0].x,
            "map sits outboard of comms: [main | comms | map]"
        );
    }

    /// The pane draws the area around the character: the current room in
    /// the middle, its mapped neighbours placed by `layout_area`, and a
    /// connector only where both ends are actually on screen (§16).
    #[test]
    fn the_map_pane_draws_the_area_around_the_current_room() {
        use crate::map::{RoomId, RoomInfo};
        use std::collections::BTreeMap;

        let mut state = state();
        let mut map = crate::map::Map::default();
        for (id, name) in [(1, "Town Square"), (2, "North Road"), (3, "East Gate")] {
            map.observe(&RoomInfo {
                id: RoomId(id),
                name: Some(name.to_string()),
                area: Some("Midgaard".to_string()),
                exits: BTreeMap::new(),
            });
        }
        map.connect(RoomId(1), "n", RoomId(2));
        map.connect(RoomId(2), "s", RoomId(1));
        map.connect(RoomId(1), "e", RoomId(3));
        map.connect(RoomId(3), "w", RoomId(1));
        // A vertical exit the flat grid cannot place, so the glyph says so.
        map.connect(RoomId(1), "d", RoomId(9));

        state.sessions[0].map = map;
        state.sessions[0].current_room = Some(RoomId(1));
        state.show_map = true;
        state.map_width = MAP_WIDTH;

        let drawn = render_sized(&state, 80, 20);
        let screen = rows(&drawn);
        assert!(
            screen.contains("Midgaard"),
            "the column is titled with the area: {screen}"
        );
        assert!(
            screen.contains('@'),
            "the current room carries its own mark: {screen}"
        );
        assert!(
            screen.contains('v'),
            "the current room marks its vertical exit: {screen}"
        );
        assert!(
            screen.contains('─'),
            "the east neighbour is connected: {screen}"
        );
        assert!(
            screen.contains('│'),
            "the north neighbour is connected: {screen}"
        );
        assert_eq!(
            filled_cells(&drawn),
            9,
            "three rooms, three cells each, drawn as filled ground: {screen}"
        );

        // With the column off, none of it is on screen — the description
        // is what remains, and that goes to the scrollback, not here.
        state.show_map = false;
        let hidden = rows(&render_sized(&state, 80, 20));
        assert!(!hidden.contains("Midgaard"), "{hidden}");
    }

    /// The room glyph's default marker is `·`, which is two bytes wide and
    /// one column wide. Highlighting the current room by byte-slicing the
    /// assembled row therefore panicked the moment any room was drawn to
    /// its *left* — found live on the first westward walk, not by any
    /// buffer assertion (§13: never index text by byte offsets you derived
    /// from columns).
    #[test]
    fn a_room_west_of_the_current_one_does_not_panic_the_highlight() {
        use crate::map::{RoomId, RoomInfo};
        use std::collections::BTreeMap;

        let mut state = state();
        let mut map = crate::map::Map::default();
        for id in [1, 2] {
            map.observe(&RoomInfo {
                id: RoomId(id),
                name: Some(format!("Room {id}")),
                area: Some("Midgaard".to_string()),
                exits: BTreeMap::new(),
            });
        }
        // The current room is the *east* one, so the neighbour's `·` sits
        // ahead of it in the row.
        map.connect(RoomId(2), "w", RoomId(1));
        map.connect(RoomId(1), "e", RoomId(2));

        state.sessions[0].map = map;
        state.sessions[0].current_room = Some(RoomId(2));
        state.show_map = true;
        state.map_width = MAP_WIDTH;

        let drawn = render_sized(&state, 80, 20);
        let screen = rows(&drawn);
        assert!(screen.contains('@'), "the current room is drawn: {screen}");
        assert_eq!(
            filled_cells(&drawn),
            6,
            "and so is the one to its west: {screen}"
        );
    }

    /// Before the server has placed the character there is nothing to draw,
    /// and an empty bordered box reads as a broken pane.
    #[test]
    fn the_map_pane_says_when_it_has_no_room_data() {
        let mut state = state();
        state.show_map = true;
        state.map_width = MAP_WIDTH;

        let screen = rows(&render_sized(&state, 80, 20));
        assert!(screen.contains("no room data yet"), "{screen}");
    }

    /// One draw of a static, already-overflowing state: exercises the
    /// scroll-offset math (`wrapped_rows` vs viewport height) in isolation,
    /// including a line whose real word-wrapped row count diverges from a
    /// naive width division.
    #[test]
    fn scroll_offset_matches_real_wrap_count() {
        let mut state = state();
        state.sessions[0]
            .scrollback
            .push_back(RetainedLine::server(format!(
                "{} {} {}",
                "a".repeat(4),
                "b".repeat(24),
                "c".repeat(4)
            )));
        for i in 0..10 {
            state.sessions[0]
                .scrollback
                .push_back(RetainedLine::server(format!("line {i}")));
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
            state.sessions[0]
                .scrollback
                .push_back(RetainedLine::server(line));
            terminal.draw(|frame| draw(frame, &state)).unwrap();
            assert_left_border_intact(terminal.backend().buffer());
        }

        state.sessions[0].prompt = "By what name do you wish to be known?".to_string();
        terminal.draw(|frame| draw(frame, &state)).unwrap();
        assert_left_border_intact(terminal.backend().buffer());

        state.sessions[0].prompt.clear();
        state.sessions[0]
            .scrollback
            .push_back(RetainedLine::server("> crazy-foo"));
        for line in ["Password:", "Reconnecting.", "", "i107 >"] {
            state.sessions[0]
                .scrollback
                .push_back(RetainedLine::server(line));
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
            state.sessions[0]
                .scrollback
                .push_back(RetainedLine::server(format!("line {i}")));
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
            state.sessions[0]
                .scrollback
                .push_back(RetainedLine::server(format!("line {i}")));
        }
        state.sessions[0].back_offset = usize::MAX;

        let buffer = render_sized(&state, 40, 12);
        assert!(
            rows(&buffer).contains("line 0"),
            "the oldest line must be reachable, not scrolled past: {}",
            rows(&buffer)
        );
    }

    /// The load-bearing assumption behind windowing: `Wrap` splits a
    /// `Line` into rows but never joins two of them, so the rows of a
    /// window equal the rows of those same lines inside the whole buffer.
    /// If ratatui ever reflowed across `Line` boundaries, every scroll
    /// position computed from a window would drift.
    #[test]
    fn window_is_row_exact_against_the_full_buffer() {
        let width = 20;
        let raws: Vec<String> = vec![
            "short".to_string(),
            "x".repeat(75), // wraps to several rows
            "\x1b[31mcoloured\x1b[0m".to_string(),
            "y".repeat(21), // wraps to exactly two
            String::new(),  // empty line still occupies one row
        ];
        let parsed: Vec<Line<'static>> = raws.iter().flat_map(|r| ansi_lines(r)).collect();

        let whole = wrapped_rows(&parsed, width);
        let sum_of_parts: usize = raws
            .iter()
            .map(|r| wrapped_rows(&ansi_lines(r), width))
            .sum();

        assert_eq!(whole, sum_of_parts, "wrapping must not join lines");
    }

    /// The point of the windowed renderer (§2.1): per-frame cost must
    /// scale with the viewport, not the buffer. Before this, `draw_session`
    /// ANSI-parsed every line in the deque and cloned the whole wrapped
    /// `Text` on every frame — and every inbound line is a frame.
    #[test]
    fn a_tailing_pane_parses_only_what_it_can_show() {
        let mut raws: VecDeque<String> = VecDeque::new();
        for i in 0..10_000 {
            raws.push_back(format!("line {i}"));
        }

        let parsed = std::cell::Cell::new(0usize);
        let (window, _) = visible_window(&raws, 22, 40, 0, |_, raw| {
            parsed.set(parsed.get() + 1);
            ansi_lines(raw)
        });

        // 22 rows of pane = 20 of viewport, so ~20 single-row lines cover
        // it. The bound is deliberately loose; what matters is that it is
        // a function of the viewport and not of the 10,000 buffered lines.
        assert!(
            parsed.get() <= 24,
            "parsed {} lines to fill a 20-row viewport",
            parsed.get()
        );
        assert!(window.len() <= 24, "window was {} lines", window.len());
    }

    /// Windowing must not change what the player sees: the tail of a huge
    /// buffer renders exactly as the same tail does on its own.
    #[test]
    fn a_huge_buffer_shows_the_same_tail_as_a_small_one() {
        let mut big = state();
        for i in 0..5_000 {
            big.sessions[0]
                .scrollback
                .push_back(RetainedLine::server(format!("line {i}")));
        }
        let mut small = state();
        for i in 4_980..5_000 {
            small.sessions[0]
                .scrollback
                .push_back(RetainedLine::server(format!("line {i}")));
        }

        assert_eq!(
            rows(&render_sized(&big, 40, 12)),
            rows(&render_sized(&small, 40, 12))
        );
    }

    /// Scrolled back into a large buffer, the window has to start further
    /// up — the offset must still land on the right lines rather than
    /// silently pinning to the tail.
    #[test]
    fn scrolling_back_into_a_large_buffer_lands_on_the_right_lines() {
        let mut state = state();
        for i in 0..5_000 {
            state.sessions[0]
                .scrollback
                .push_back(RetainedLine::server(format!("line {i}")));
        }
        state.sessions[0].back_offset = 100;

        // `back_offset` is a distance from the tail in rows, so with 5,000
        // single-row lines the newest visible one is 5000 - 100 - 1.
        let shown = rows(&render_sized(&state, 40, 12));
        assert!(shown.contains("line 4899"), "{shown}");
        assert!(
            !shown.contains("line 4900"),
            "scrolled too far forward: {shown}"
        );
        assert!(
            !shown.contains("line 4999"),
            "tail must be scrolled off: {shown}"
        );
    }

    /// The inspector (§14 M6) is a different buffer from the scrollback a
    /// `back_offset` was set against — toggling into it must always show
    /// its own tail, not silently inherit an offset that belongs to
    /// unrelated content with no indicator to explain why.
    #[test]
    fn toggling_the_server_data_inspector_ignores_the_scrollbacks_scroll_position() {
        let mut state = state();
        // More entries than the 4 content rows `render` gives (see
        // `CONTENT_ROWS`), so a wrongly-applied offset would visibly hide
        // the tail rather than just failing to matter.
        for i in 0..8 {
            state.sessions[0]
                .inspector_log
                .push_back(format!("gmcp-{i}"));
        }
        // A large offset set while looking at the scrollback, carried
        // along when the player then hits F2.
        state.sessions[0].back_offset = usize::MAX;
        state.show_inspector = true;

        let buffer = render(&state);
        assert!(
            rows(&buffer).contains("gmcp-7"),
            "the inspector must show its own tail: {}",
            rows(&buffer)
        );
    }

    /// The inspector title (§6.3) says what has actually been seen, in each
    /// of the three states — including the empty one, which must read as
    /// self-explanatory rather than as a broken GMCP-only view.
    #[test]
    fn the_inspector_title_reflects_what_has_been_seen() {
        let mut state = state();
        state.show_inspector = true;

        // Wide enough for the longest title ("GMCP + MSDP inspector"),
        // unlike the default 30-column `render`.
        let buffer = render_sized(&state, 60, 10);
        assert!(
            rows(&buffer).contains("server data — nothing received yet"),
            "{}",
            rows(&buffer)
        );

        state.sessions[0].push_gmcp("Char.Vitals".to_string(), None);
        let buffer = render_sized(&state, 60, 10);
        assert!(
            rows(&buffer).contains("GMCP inspector"),
            "{}",
            rows(&buffer)
        );

        state.sessions[0].push_msdp(vec![("hp".to_string(), "100".to_string())]);
        let buffer = render_sized(&state, 60, 10);
        assert!(
            rows(&buffer).contains("GMCP + MSDP inspector"),
            "{}",
            rows(&buffer)
        );
    }
}
