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
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};

use crate::config::Keybinds;
use crate::scrollback::{Origin, RetainedLine};
use crate::state::{AppState, ChannelPane, Focus, LayoutMode};

pub mod config_editor;
mod map_render;
mod map_sixel;
pub(crate) use map_render::PendingImage;
mod sixel;

use map_render::MapRenderer as _;

/// The smallest main area worth keeping beside the docked column — below
/// that the channels are simply not drawn. Where the column *starts* is
/// `config::DEFAULT_CHANNEL_WIDTH`: a starting value is a setting's
/// default, and `config` cannot read it from here without depending on
/// the layer that draws (docs/ARCHITECTURE.md §4, §11.4).
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
/// resized to. The live width is `AppState::map_width` (§11.4, §16), and
/// where it starts is `config::DEFAULT_MAP_WIDTH`.
/// Narrower than the comms floor because a map row is glyphs and
/// connectors, not words — but not so narrow that the current room can
/// never be centred with a neighbour either side.
pub(crate) const MIN_MAP_WIDTH: u16 = 12;

/// As `clamp_channel_width`, for the map column.
pub(crate) fn clamp_map_width(width: u16, area_width: u16) -> u16 {
    let max = area_width.saturating_sub(MIN_MAIN_WIDTH).max(MIN_MAP_WIDTH);
    width.clamp(MIN_MAP_WIDTH, max)
}

/// The smallest terminal the client will draw its layout into; below this it
/// says so instead (#122). The panes collapse toward zero width, titles
/// truncate, and the map and comms columns compete for space that is not
/// there — a screen the player is left to diagnose. 80x24 is the conventional
/// floor and what a default terminal still opens at. That it is *enough* is
/// asserted from the layout rather than assumed: see
/// `the_minimum_width_holds_the_help_listing` and
/// `the_minimum_height_leaves_a_usable_session_pane`.
pub(crate) const MIN_COLS: u16 = 80;
pub(crate) const MIN_ROWS: u16 = 24;

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
        row("Tab", "accept the dimmed completion, if there is one"),
        row(
            keybinds.toggle_autocomplete,
            "turn that completion off or on",
        ),
        row("Up / Down", "walk this character's history"),
        row("PgUp / PgDn", "scroll the focused pane back / forward"),
        row("Home / End", "jump to the oldest / newest line"),
        String::new(),
        "Characters".to_string(),
        row("Alt+1 … Alt+9", "jump to character 1-9, then the map"),
        row(keybinds.focus_next, "cycle focus, comms included"),
        String::new(),
        "Views".to_string(),
        row(keybinds.cycle_layout, "tabs / side-by-side layout"),
        row(keybinds.toggle_channels, "show or hide comms"),
        // Paired on one row each: the map cursor needed a line and the
        // listing was already at the height of a small terminal, where two
        // rows to say "wider" and "narrower" is the cheapest thing to give
        // back.
        row(
            format!("{} / {}", keybinds.channel_wider, keybinds.channel_narrower),
            "widen / narrow the comms column",
        ),
        row(keybinds.toggle_map, "show or hide the map column"),
        row(keybinds.swap_columns, "swap the map and comms columns"),
        row(
            format!("{} / {}", keybinds.map_wider, keybinds.map_narrower),
            "widen / narrow the map column",
        ),
        row(
            keybinds.map_cursor,
            "focus the map: arrows read rooms, Enter walks there",
        ),
        row(keybinds.toggle_hud, "show or hide the party strip"),
        row(
            keybinds.toggle_timestamps,
            "show or hide the clock down the character panes",
        ),
        row(
            keybinds.who_needs_me,
            "jump to whoever is in the most trouble",
        ),
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
        row(
            "/config",
            "edit this profile — `/config charset` opens on a setting",
        ),
        row("/update", "install a newer Mudular, if one was announced"),
        row("/newprofile", "create another character's profile"),
        row("/connect", "add a character to this running instance"),
        row("/disconnect", "close the character you are typing at"),
        row("/send", "run one command as another character, or `*`"),
        row("/goto", "walk to a known room, one step at a time"),
        row("/corpse", "walk back to where you last died"),
        row(
            "/mark",
            "label this room on the map (`/mark` alone clears it)",
        ),
        row("/map", "show or hide the map, and describe this room"),
        row("/comms", "show or hide the comms column"),
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
    /// The party strip, when it is on and there is more than nothing to put
    /// in it (§11.6).
    pub hud: Option<Rect>,
    pub input: Rect,
    /// The status bar: the last row of the terminal, always present.
    pub status: Rect,
}

/// Splits `area` into the panes `state` asks for.
pub fn layout(area: Rect, state: &AppState) -> Panes {
    let reserve_prompt = state.bound().is_some_and(|session| session.view.connected);
    // Above the prompt rather than below it: the prompt and the input line
    // are one thing to look at while typing, and a strip between them would
    // split the pair the player's eye already treats as joined.
    let hud_rows = u16::from(state.show_hud && !state.sessions.is_empty());
    // The status bar is the very last row, below the input box — where a
    // terminal application's status line belongs, and where the eye can
    // find it without hunting. One row, always present: a bar that comes
    // and goes moves everything above it, and the client would appear to
    // jump whenever a warning arrived.
    let [body, hud_area, prompt_area, input, status] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(hud_rows),
        Constraint::Length(if reserve_prompt { 1 } else { 0 }),
        Constraint::Length(3),
        Constraint::Length(1),
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

    // Which side column sits next to the character panes is the player's
    // to choose (#111): the map is read *while* playing, so a multi-boxer
    // glancing between a pane and the map would rather not cross a column
    // of chatter to do it.
    //
    // Built as a list so the order is stated once. Pushing constraints and
    // then indexing `columns` separately is how the two could disagree,
    // and a map drawn in the comms column is not a subtle bug to find.
    let mut side: Vec<(bool, u16)> = Vec::new();
    if state.map_first {
        side.push((true, state.map_width));
        side.push((false, state.channel_width));
    } else {
        side.push((false, state.channel_width));
        side.push((true, state.map_width));
    }
    side.retain(|(is_map, _)| if *is_map { show_map } else { show_channels });

    let mut constraints = vec![Constraint::Min(MIN_MAIN_WIDTH)];
    constraints.extend(side.iter().map(|(_, width)| Constraint::Length(*width)));
    let columns = Layout::horizontal(constraints).split(body);
    let main = columns[0];
    let column_of = |wanted_map: bool| {
        side.iter()
            .position(|(is_map, _)| *is_map == wanted_map)
            .map(|at| columns[at + 1])
    };
    let channel_column = column_of(false);
    let map = column_of(true);

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
        hud: (hud_rows > 0).then_some(hud_area),
        input,
        status,
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

/// What a frame put on screen that a step after `Terminal::draw` needs to
/// know about: the picture to write once ratatui has flushed the cells
/// around it (§16), and the map pane's own area — real screen coordinates,
/// not a guess — so `--map-debug` can read back exactly what this frame
/// showed instead of re-rendering a separate approximation of it.
#[derive(Default)]
pub struct DrawnFrame {
    /// The map's picture this frame — present whenever one occupies the
    /// pane, whether freshly rasterised or reused from `MapImageCache`, so
    /// a consumer like `--map-debug` always has real glyphs to read.
    pub image: Option<map_render::PendingImage>,
    /// Whether `image` was actually rasterised this frame rather than
    /// reused unchanged. Only a fresh picture is worth the cost of writing
    /// to the terminal again — the one already on screen is still correct.
    pub image_is_fresh: bool,
    pub map_area: Option<Rect>,
    /// The grid the map was actually drawn into — the pane minus border,
    /// legend and caption. Reported rather than recomputed because those
    /// subtractions depend on the cursor's description and the legend's
    /// wrapped height, and a second copy of that arithmetic would drift
    /// (#58).
    pub map_grid: Option<Rect>,
}

/// Marks each border side that has undrawn rooms beyond it, with an arrow
/// pointing that way (#55, made legible in #113).
///
/// The first version restyled the border instead — brighter and bold, no
/// glyph, on the reasoning that #50 confined the map's marks to characters
/// present in every monospace font. That was true and it did not matter:
/// a slightly brighter run of `│` among a border made of `│` is not a
/// mark, it is a shade, and in use it read as nothing at all.
///
/// `<`, `>`, `^` and `v` are plain ASCII, so they clear the portability
/// bar by a wider margin than the box-drawing set ever did, and they say
/// which way rather than merely that something is there. Bright and bold
/// as well, since the border they replace is dim.
fn mark_edges(frame: &mut Frame, area: Rect, beyond: &map_render::Beyond) {
    if area.width < 3 || area.height < 3 || !beyond.any() {
        return;
    }
    let lit = Style::new()
        .fg(map_render::palette::PAPER)
        .add_modifier(Modifier::BOLD);
    // Half the side rather than a third: this is the pane saying part of
    // the world is missing, which is worth more than a hint.
    let run = |span: u16| {
        let length = (span / 2).max(1);
        let start = span.saturating_sub(length) / 2;
        (start, length)
    };

    if beyond.up || beyond.down {
        let (start, length) = run(area.width.saturating_sub(2));
        for offset in 0..length {
            let x = area.left() + 1 + start + offset;
            if beyond.up {
                frame.buffer_mut()[(x, area.top())]
                    .set_symbol("^")
                    .set_style(lit);
            }
            if beyond.down {
                frame.buffer_mut()[(x, area.bottom() - 1)]
                    .set_symbol("v")
                    .set_style(lit);
            }
        }
    }
    if beyond.left || beyond.right {
        let (start, length) = run(area.height.saturating_sub(2));
        for offset in 0..length {
            let y = area.top() + 1 + start + offset;
            if beyond.left {
                frame.buffer_mut()[(area.left(), y)]
                    .set_symbol("<")
                    .set_style(lit);
            }
            if beyond.right {
                frame.buffer_mut()[(area.right() - 1, y)]
                    .set_symbol(">")
                    .set_style(lit);
            }
        }
    }
}

/// Whether the map pane would draw the room at scene coordinate `at`, with
/// enough context around it to be worth keeping the view still (#58).
///
/// `app` owns the policy — when to re-centre — and this is the geometry
/// question it cannot answer without knowing how the renderer lays a scene
/// out. Takes the grid `draw` reported, not the pane rect.
pub fn map_shows_room(grid: Rect, at: (i32, i32)) -> bool {
    map_render::room_is_visible(grid, at)
}

/// What the map pane drew last frame, so a frame where nothing about the
/// scene changed can skip rasterising and RLE-encoding a fresh picture —
/// on a real map that took long enough to be felt on every single
/// keystroke, not just the ones that moved the character (see `render` in
/// `map_sixel.rs`).
#[derive(Default)]
pub struct MapImageCache {
    key: Option<MapImageCacheKey>,
    image: Option<map_render::PendingImage>,
}

impl MapImageCache {
    /// Drops what the cache believes is on screen.
    ///
    /// The cache does not really hold a picture, it holds the claim that
    /// the terminal is already showing one — which is why a reused
    /// picture is not written again. Anything that clears the terminal
    /// makes that claim false, so the caller has to say so, or the next
    /// frame reuses a picture that is no longer anywhere.
    pub fn forget(&mut self) {
        self.key = None;
        self.image = None;
    }
}

#[derive(PartialEq, Eq)]
struct MapImageCacheKey {
    scene: crate::map::Scene,
    cursor: Option<crate::map::RoomId>,
    area: Rect,
    cell: (u16, u16),
    /// Where the picture is panned to. The same scene at a different pan
    /// is a different picture, and leaving this out of the key is how the
    /// map would appear frozen after a character switch (#58).
    pan: (i32, i32),
}

/// Whether `area` can host the layout at all (#122).
///
/// Kept apart from [`draw`] rather than folded into it because `draw` is the
/// layout renderer and this is a policy about the terminal: the tests drive
/// `draw` at deliberately small sizes to exercise narrow panes, and a floor
/// inside it would make those sizes unreachable rather than merely unsupported.
/// The caller that owns a real terminal asks this first.
pub fn fits(area: Rect) -> bool {
    area.width >= MIN_COLS && area.height >= MIN_ROWS
}

/// One legible notice in place of a layout that cannot fit (#122).
///
/// No border and no vertical centring by `Layout`: at this size every row is
/// scarce, and a border would spend two of them saying nothing. The size it
/// reports is the live one, so a player dragging a window edge can watch the
/// numbers approach what is needed rather than guess how much further to go.
pub fn draw_too_small(frame: &mut Frame, no_color: bool) {
    let area = frame.area();
    let lines = vec![
        Line::from("Terminal too small".bold()),
        Line::from(format!(
            "{}x{} - need {MIN_COLS}x{MIN_ROWS}",
            area.width, area.height
        )),
    ];
    // Top-left rather than centred when there is no room to centre: the
    // arithmetic for centring is what would clip the first line away.
    let top = area.height.saturating_sub(lines.len() as u16) / 2;
    let where_to = Rect {
        y: area.y + top,
        height: area.height - top,
        ..area
    };
    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        where_to,
    );
    // The same last word the full frame gets: a notice the player cannot read
    // in monochrome is no better than the broken layout it replaced (#120).
    if no_color {
        strip_colour(frame.buffer_mut());
    }
}

/// What a real terminal shows: the layout, or a notice that it does not fit
/// (#122).
///
/// The split from [`draw`] is deliberate. `draw` renders the layout at
/// whatever size it is handed, which is what lets the tests exercise narrow
/// panes directly; the floor is a policy about the terminal, and belongs at
/// the one place that owns one. Callers driving a terminal want this.
pub fn draw_screen(
    frame: &mut Frame,
    state: &AppState,
    map_cache: &mut MapImageCache,
) -> DrawnFrame {
    if fits(frame.area()) {
        draw(frame, state, map_cache)
    } else {
        draw_too_small(frame, state.no_color);
        DrawnFrame::default()
    }
}

pub fn draw(frame: &mut Frame, state: &AppState, map_cache: &mut MapImageCache) -> DrawnFrame {
    let mut pending = None;
    let mut image_is_fresh = true;
    let mut map_area = None;
    let mut map_grid = None;
    let panes = layout(frame.area(), state);

    if state.sessions.is_empty() {
        // The hint, then whatever the client has been told since — a
        // `/connect` that named a profile nobody has heard of has nowhere
        // else to say so (#108).
        let mut lines = vec![Line::from(
            "no target — /connect <profile> opens a character, /newprofile makes one",
        )];
        if !state.shell_notices.is_empty() {
            lines.push(Line::from(""));
            lines.extend(
                state
                    .shell_notices
                    .iter()
                    .map(|notice| Line::from(notice.as_str().dim())),
            );
        }
        let help = Paragraph::new(lines)
            .wrap(Wrap { trim: true })
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

    if let Some(rect) = panes.hud {
        draw_hud(frame, rect, state);
    }

    if let Some(rect) = panes.map {
        let drawn;
        (pending, image_is_fresh, drawn) = draw_map(frame, rect, state, map_cache);
        map_area = Some(rect);
        map_grid = drawn;
    }

    let bound = state.bound();
    if let (Some(area), Some(session)) = (panes.prompt, bound)
        && !session.view.prompt.is_empty()
    {
        // Indent by one column so the prompt lines up with the pane's
        // content rather than its border.
        let inset = Rect {
            x: area.x + 1,
            width: area.width.saturating_sub(1),
            ..area
        };
        frame.render_widget(
            Paragraph::new(Text::from(ansi_lines(&session.view.prompt))),
            inset,
        );
    }

    if state.config_editor.is_none() {
        draw_input(frame, panes.input, state);
    }
    draw_status(frame, panes.status, state);

    if state.show_help {
        draw_help(frame, frame.area(), state);
    }

    if state.show_errors {
        draw_errors(frame, frame.area(), state);
    }

    if state.palette.is_some() {
        draw_palette(frame, frame.area(), state);
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

    // Last, so it catches every overlay above as well as the panes below:
    // an overlay drawn after the strip would keep its colour (#120).
    if state.no_color {
        strip_colour(frame.buffer_mut());
    }

    // The picture is written by the caller once the frame has been
    // flushed: an image is not made of cells, so ratatui cannot carry it.
    DrawnFrame {
        image: pending,
        image_is_fresh,
        map_area,
        map_grid,
    }
}

/// The help overlay: a box centred over the layout, sized to its content and
/// clipped to the terminal (docs/ARCHITECTURE.md §11.2).
/// The command palette (#43).
///
/// A query line and the matches under it, best first. Near the top of the
/// terminal rather than centred: the list grows downward as you type, and
/// a box that grows from the middle of the screen moves its own first row
/// out from under the eye already reading it.
fn draw_palette(frame: &mut Frame, area: Rect, state: &AppState) {
    let Some(palette) = &state.palette else {
        return;
    };
    let entries = state.palette_entries(palette.input.value());

    let width = (area.width * 2 / 3).max(24).min(area.width);
    let rows = (entries.len() as u16).min(10);
    let height = (rows + 3).min(area.height);
    let overlay = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + area.height / 8,
        width,
        height,
    };

    let selected = palette.selected.min(entries.len().saturating_sub(1));
    // The window follows the selection instead of being pinned to the top
    // of the list: with more matches than rows, arrowing down moved a
    // highlight nobody could see, because the same first ten entries were
    // drawn every frame.
    let first = selected.saturating_sub(rows.saturating_sub(1) as usize);
    let mut lines = vec![Line::from(vec![
        Span::styled("> ", Style::new().dim()),
        Span::raw(palette.input.value().to_string()),
    ])];
    for (offset, entry) in entries.iter().skip(first).take(rows as usize).enumerate() {
        let index = first + offset;
        // The selected row is reversed rather than recoloured: the labels
        // are already the client's own commands in the client's own
        // colours, and a second hue here would be one more thing to learn.
        let style = match index == selected {
            true => Style::new().add_modifier(Modifier::REVERSED),
            false => Style::new(),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{:<14}", entry.label()), style),
            Span::styled(format!(" {}", entry.describes()), style.dim()),
        ]));
    }
    if entries.is_empty() {
        lines.push(Line::from("nothing matches".dim()));
    }

    frame.render_widget(Clear, overlay);
    // The count says there is more below than fits, which a window that
    // scrolled silently would otherwise hide.
    let title = match entries.len() {
        0 => " run a command (Esc to close) ".to_string(),
        total => format!(" run a command — {} of {total} (Esc) ", selected + 1),
    };
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(title.bold())),
        overlay,
    );
    // The cursor belongs in the query, which is the only thing being typed
    // into — without it the palette reads as a list that happens to have a
    // line of text above it.
    frame.set_cursor_position((
        overlay.x + 3 + palette.input.visual_cursor() as u16,
        overlay.y + 1,
    ));
}

/// The warnings panel (#18).
///
/// Newest last, like scrollback, because that is the order the player read
/// them in the first place. Only the tail is shown when there are more
/// than fit: the recent ones are what a player who just saw the badge is
/// looking for, and an old warning nobody chased is not worth pushing a
/// new one off the screen for.
fn draw_errors(frame: &mut Frame, area: Rect, state: &AppState) {
    let width = (area.width * 3 / 4).max(20).min(area.width);
    let height = (area.height * 3 / 4).max(3).min(area.height);
    let overlay = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    let rows = overlay.height.saturating_sub(2) as usize;
    let lines: Vec<Line<'static>> = match state.errors.is_empty() {
        true => vec![Line::from("nothing has gone wrong yet".dim())],
        false => state
            .errors
            .iter()
            .rev()
            .take(rows)
            .rev()
            .map(|text| Line::from(text.clone()))
            .collect(),
    };

    let title = format!(" warnings ({}) ", state.errors.len());
    frame.render_widget(Clear, overlay);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(Block::bordered().title(title.bold())),
        overlay,
    );
}

fn draw_help(frame: &mut Frame, area: Rect, state: &AppState) {
    let keybinds = &state.keybinds;
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

    // The listing is longer than a short terminal, and used to be simply
    // cut off at the bottom — the part a newcomer most needs being the part
    // that did not fit (§11.2).
    let rows = lines.len() as u16;
    let viewport = overlay.height.saturating_sub(2);
    let hidden = rows.saturating_sub(viewport);
    let scroll = state.help_scroll.min(hidden);

    let title = match hidden {
        0 => format!(" Help — {} or Esc to close ", keybinds.help),
        // The keys are only worth naming when there is somewhere to go.
        _ => format!(" Help — ↑↓ PgUp/PgDn, {} or Esc to close ", keybinds.help),
    };
    let text = Text::from(lines.into_iter().map(Line::from).collect::<Vec<_>>());
    let block = Block::bordered().title(title.bold());
    // Clear first: the overlay sits on top of panes that already drew here.
    frame.render_widget(ratatui::widgets::Clear, overlay);
    frame.render_widget(
        Paragraph::new(text).block(block).scroll((scroll, 0)),
        overlay,
    );

    if hidden > 0 {
        // Says how much is out of sight, and where in it you are — the
        // title alone cannot, and a listing that silently had more was the
        // whole defect.
        let mut bar = ScrollbarState::new(rows as usize)
            .position(scroll as usize)
            .viewport_content_length(viewport as usize);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            overlay.inner(ratatui::layout::Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut bar,
        );
    }
}

/// The `/mark` chooser (§16): what is this room *for*?
///
/// Offered because nothing on the wire answers that — a Diku MUD's MSDP
/// carries no shop flag and no terrain — so the label is always the
/// player's, and the least a client can do is not make them remember what
/// they called the last one. Numbered, because nine rows is short enough
/// that counting beats arrowing.
fn draw_mark_menu(frame: &mut Frame, area: Rect, menu: &crate::state::MarkMenu) {
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

    // 24 was wide enough for the list, whose longest row is a fixed
    // `{:<16}` field — but not for "Enter to mark, Esc to go back", which
    // this box also has to show once a custom label is being typed. Sized
    // to the actual content instead, so the day a longer prompt is added
    // here it grows with it rather than clipping silently.
    let content_width = lines.iter().map(Line::width).max().unwrap_or(0) as u16;
    let width = (content_width + 2).max(24).min(area.width);
    let height = (lines.len() as u16 + 2).min(area.height);
    let overlay = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    let block = Block::bordered().title(format!(" mark #{} ", menu.at.0).bold());
    frame.render_widget(ratatui::widgets::Clear, overlay);
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .block(block),
        overlay,
    );
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
        // Says what is about to happen, and how long it will take. The old
        // copy here advertised "no YAML required", which only means anything
        // to somebody who already knew there was YAML to be afraid of — the
        // one reader this screen is not for.
        "Four questions and you're playing.".to_string(),
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
    let picked_line = (!showing_inspector && state.is_bound(index))
        .then_some(state.line_cursor)
        .flatten()
        .and_then(|cursor| session.view.scrollback.len().checked_sub(1 + cursor));

    let content_width = area.width.saturating_sub(2);

    // Where the connection stands lives in the status bar now, not in
    // every pane title — it was in both, which is one place too many.
    //
    // What stays here is a session that is *not* connected. The bar can
    // only speak for the character being typed at, so a second character
    // dropping while you play the first would otherwise say so nowhere.
    // Trouble is per-pane; routine is global.
    let title = if showing_inspector {
        format!(" {} — {} ", session.view.name, session.inspector_title())
    } else {
        let picking = if picked_line.is_some() {
            " ↑↓ pick a line, Enter for a trigger, Esc to cancel"
        } else {
            scroll_indicator(session.view.back_offset)
        };
        let state_note = match session.view.connected {
            true => String::new(),
            false => format!(" — {}", session.view.status),
        };
        format!(
            "{}{state_note}{picking} ",
            pane_title(
                &session.view.name,
                session.view.unread,
                session.view.distress.is_some()
            ),
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
        session.view.back_offset
    };

    let (lines, extent) = if showing_inspector {
        visible_window(
            &session.view.inspector_log,
            area.height,
            content_width,
            back_offset,
            |_, raw| vec![Line::raw(raw.to_string())],
        )
    } else {
        visible_window(
            &session.view.scrollback,
            area.height,
            content_width,
            back_offset,
            |i, line| {
                // Composed here from the line's own `at`, exactly as a
                // channel pane composes its own (§8): the toggle decides at
                // render time, and the stored line stays the text the MUD
                // sent — so turning the clock off gives back the pane that
                // was there before, rather than a pane with the stamps
                // spliced in and no way out.
                let stamp = match state.show_timestamps {
                    true => vec![timestamp(line)],
                    false => Vec::new(),
                };
                let rendered = prefixed(ansi_lines(&line.text), stamp);
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

    render_scrollback(
        frame,
        area,
        lines,
        title,
        focused,
        session.view.color,
        extent,
    );
}

/// Every character's vitals on one row (§11.6).
///
/// Multiboxers do this in their head, flicking between panes to find who is
/// in trouble; the numbers are already in the merged server-data store that
/// rules read, so the client can simply say. Each character keeps the
/// colour that tints their pane border and tab (§11), so the strip is read
/// by *who* before it is read by *what*.
fn draw_hud(frame: &mut Frame, area: Rect, state: &AppState) {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (index, session) in state.sessions.iter().enumerate() {
        // Read out of the peer snapshot rather than reaching into the
        // session task: the snapshot is a value already in memory and
        // borrowing it costs no lock (§7.5), which matters on a strip
        // redrawn every frame.
        let vitals = state
            .peer_registry
            .get(&session.view.name)
            .map(
                |peer: &tokio::sync::watch::Receiver<crate::engine::PeerSnapshot>| {
                    crate::vitals::from_server_data(&peer.borrow().data)
                },
            )
            .unwrap_or_default();

        if index > 0 {
            spans.push(Span::raw("  "));
        }
        let name = Style::default().fg(session.view.color.unwrap_or(Color::Gray));
        let focused = state.is_bound(index);
        spans.push(Span::styled(
            match focused {
                // The character the input line is bound to, marked the way
                // the tab bar marks it — the strip is another place to
                // answer "who am I typing to".
                true => format!("▸{}", session.view.name),
                false => format!(" {}", session.view.name),
            },
            match focused {
                true => name.add_modifier(Modifier::BOLD),
                false => name,
            },
        ));

        if vitals.is_empty() {
            // A MUD that reports nothing is said to report nothing, rather
            // than being drawn as a character at zero health.
            spans.push(Span::styled(
                " —".to_string(),
                Style::default().add_modifier(Modifier::DIM),
            ));
            continue;
        }
        for (label, gauge) in [
            ("hp", vitals.health),
            ("mp", vitals.mana),
            ("mv", vitals.movement),
        ] {
            let Some(gauge) = gauge else { continue };
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                format!("{label} {}", gauge.now),
                gauge_style(gauge),
            ));
        }

        // Experience is last, and reads differently from the three above
        // it in both of the ways that matter.
        //
        // A **percentage**, where they show the number itself: `27` hp is
        // a fact a player acts on, where experience-within-a-level is a
        // six-digit number whose meaning is entirely in how far along it
        // is. Percent is the short way to say that, and the only reason
        // the bar is drawn at all.
        //
        // And **never `gauge_style`**, whose whole vocabulary is danger —
        // red at a quarter, amber at a half. A character 10% into a level
        // is not in trouble, they have just levelled; painting that red
        // would say the opposite of what happened, in a strip whose red
        // means "go and help them" (§11.7).
        if let Some(experience) = vitals.experience {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                format!("xp {}%", (experience.fraction() * 100.0).floor() as u8),
                Style::default().fg(Color::Rgb(0x64, 0x74, 0x8B)),
            ));
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The one colour for "this needs you now": a gauge under
/// [`crate::vitals::ALARM`] and the tab of the character it belongs to
/// (§11.6, §11.7). Named once so the strip and the tab bar cannot come to
/// mean different things by the same red.
const ALARM: Color = Color::Rgb(0xE1, 0x1D, 0x48);

/// How alarming a gauge is. Colour rather than a number alone, because the
/// point of the strip is noticing trouble without reading it — and bold on
/// top of colour, so the warning survives a terminal that renders these
/// hues too close together.
fn gauge_style(gauge: crate::vitals::Gauge) -> Style {
    let filled = gauge.fraction();
    let style = Style::default();
    match filled {
        f if f <= crate::vitals::ALARM => style.fg(ALARM).add_modifier(Modifier::BOLD),
        f if f <= 0.5 => style.fg(Color::Rgb(0xE6, 0x9F, 0x00)),
        _ => style.fg(Color::Rgb(0x64, 0x74, 0x8B)),
    }
}

/// Draws the area around the character, from the scene the map builds.
///
/// Only a *view*: every fact shown comes from `Map::scene`, and the prose
/// form of the same knowledge is `Map::describe` (§16). This function owns
/// the pane — its border, its title, and what to say when there is nothing
/// to draw — and hands the picture itself to a [`MapRenderer`].
fn draw_map(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    map_cache: &mut MapImageCache,
) -> (Option<map_render::PendingImage>, bool, Option<Rect>) {
    let session = state.bound();
    let title = match session.and_then(|session| session.view.current_room) {
        Some(at) => state
            .bound_map()
            .and_then(|map| map.rooms.get(&at))
            .and_then(|room| room.area.clone())
            .unwrap_or_else(|| "map".to_string()),
        None => "map".to_string(),
    };
    // The same three keys the scrollback line picker advertises, for the
    // same reason: a selection nobody can see how to leave is a trap.
    let focused = state.focus == crate::state::Focus::Map;
    let title = match focused {
        true => format!(" {}:{title} — ↑↓←→ Enter Esc ", state.sessions.len() + 1),
        // The number it answers to, so the way in is discoverable from the
        // pane itself rather than only from the help listing.
        false => format!(" {}:{title} ", state.sessions.len() + 1),
    };
    let block = Block::bordered()
        .title(if focused { title.bold() } else { title.into() })
        .border_style(if focused {
            Style::new()
        } else {
            Style::new().dim()
        });
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return (None, true, None);
    }

    // Reserved before anything else claims the bottom of the pane, so it
    // stays put whether the pane is empty, mid-walk, or showing a room's
    // description — chrome rather than content, the same either way.
    let (inner, legend_area) = split_legend(inner);
    frame.render_widget(
        // Already broken to this width, so no wrapping: `Paragraph`'s
        // would split entries apart again.
        Paragraph::new(map_render::legend(legend_area.width)),
        legend_area,
    );

    let (Some(session), Some(current)) = (session, session.and_then(|s| s.view.current_room))
    else {
        frame.render_widget(
            Paragraph::new("no room data yet").wrap(Wrap { trim: true }),
            inner,
        );
        return (None, true, None);
    };

    let Some(map) = state.bound_map() else {
        return (None, true, None);
    };
    // Everyone on this world, not only the character being typed at
    // (§16) — the map is of the world, and they are all on it.
    let party = state
        .bound_index()
        .map(|i| state.party_of(i))
        .unwrap_or_default();
    let scene = map.scene(current, session.view.corpse, &party);
    // Where the picture sits relative to the character, in room steps
    // (#58). Zero is the historical behaviour — character at the middle,
    // world sliding under them as they walk — and it stays zero unless a
    // switch between characters has deliberately held the view still.
    let pan = state.map_pan;

    // Pixels where the terminal takes them, cells everywhere else — the
    // same scene either way (§16).
    //
    // And cells, whatever the terminal can do, while an overlay is open.
    // A sixel picture is written to the terminal *after* the frame, so it
    // lands on top of whatever ratatui drew — including a panel the player
    // is reading. Drawing the map as glyphs for those frames puts it back
    // in the buffer, where the overlay's `Clear` covers it like any other
    // pane, and the cell path clears the image cache on its way through so
    // the picture returns freshly rasterised once the overlay closes.
    if let Some(cell) = state.map_cell_px.filter(|_| !overlay_covers_map(state)) {
        let described_rows = described_height(state, inner);
        let (grid, caption) = split_caption(inner, described_rows);
        let key = MapImageCacheKey {
            scene: scene.clone(),
            cursor: state.map_cursor,
            area: grid,
            cell,
            // Part of the key: the same scene panned elsewhere is a
            // different picture, and reusing the cached one would leave
            // the map visibly stuck (#58).
            pan,
        };
        // Rasterising and RLE-encoding a real map costs tens of
        // milliseconds — fine once, on the frame that moved the player,
        // but `draw` runs on every keystroke, map-related or not. Reusing
        // the last picture whenever nothing the key covers has changed is
        // what keeps typing from paying that cost on every character.
        let cached = (map_cache.key.as_ref() == Some(&key))
            .then(|| map_cache.image.clone())
            .flatten();
        let (image, fresh) = match cached {
            Some(image) => (Some(image), false),
            None => {
                let image = map_sixel::render(grid, &scene, state.map_cursor, cell, pan);
                map_cache.key = Some(key);
                map_cache.image = image.clone();
                (image, true)
            }
        };
        if let Some(image) = image {
            // Skipped only while the terminal is already showing this
            // picture. Then the cells under it belong to it, and left to
            // ratatui they would be painted over the pixels every frame.
            //
            // On the frame it is *newly* written they are deliberately
            // left alone, so ratatui's ordinary diff repaints the region
            // — which erases whatever the picture is landing on, the "no
            // room data yet" line or the letters of the previous scene,
            // in the few cells that actually changed. That used to need
            // clearing the whole terminal and repainting every pane,
            // which is a visible blank on the frame a map first appears.
            if !fresh {
                for y in grid.top()..grid.bottom() {
                    for x in grid.left()..grid.right() {
                        frame.buffer_mut()[(x, y)]
                            .set_diff_option(ratatui::buffer::CellDiffOption::Skip);
                    }
                }
            }
            draw_caption(frame, caption, state);
            mark_edges(frame, area, &map_render::rooms_beyond(grid, &scene, pan));
            return (Some(image), fresh, Some(grid));
        }
    }

    // With the cursor up, the bottom of the column says what it is sitting
    // on — `Map::describe`, the same prose `/map` prints. It goes in the
    // pane rather than the scrollback because arrowing across a dozen rooms
    // would otherwise bury the session's own output under a dozen
    // descriptions nobody asked to keep.
    let described: Vec<String> = state
        .map_cursor
        .zip(state.bound_map())
        .map(|(at, map)| map.describe(at))
        .unwrap_or_default();
    let (grid, caption) = match described.is_empty() {
        true => (inner, None),
        false => {
            // Wrapped, so a long room name is not silently cut in half, but
            // never more than half the column — the map is still the point.
            let wanted = described
                .iter()
                .map(|line| line.len().div_ceil(inner.width.max(1) as usize) as u16)
                .sum::<u16>();
            let rows = wanted.clamp(1, inner.height / 2);
            let split = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(0), Constraint::Length(rows)])
                .split(inner);
            (split[0], Some(split[1]))
        }
    };

    map_render::CharRenderer.draw(frame, grid, &scene, state.map_cursor, pan);

    if let Some(rect) = caption {
        frame.render_widget(
            Paragraph::new(described.join("\n"))
                .wrap(Wrap { trim: true })
                .style(Style::default().add_modifier(Modifier::DIM)),
            rect,
        );
    }
    // No picture occupies the pane this frame, so nothing in the cache is
    // still on screen to reuse.
    mark_edges(frame, area, &map_render::rooms_beyond(grid, &scene, pan));
    map_cache.key = None;
    map_cache.image = None;
    (None, true, Some(grid))
}

/// How many rows the cursor's description wants, so both renderers leave
/// it the same room.
fn described_height(state: &AppState, inner: Rect) -> u16 {
    let Some(at) = state.map_cursor else {
        return 0;
    };
    let Some(map) = state.bound_map() else {
        return 0;
    };
    let lines = map.describe(at);
    if lines.is_empty() {
        return 0;
    }
    let wanted: u16 = lines
        .iter()
        .map(|line| line.len().div_ceil(inner.width.max(1) as usize) as u16)
        .sum();
    wanted.clamp(1, inner.height / 2)
}

/// Splits the legend's row(s) off the bottom of the map pane.
///
/// Wrapped rather than truncated, so a narrow pane spends more rows on
/// the legend instead of cutting entries off the end of a line — but
/// never more than half the pane, because the map is still the point. A
/// pane short enough that the legend wants more than that shows as much
/// as fits and loses the tail; the alternative is a legend that crowds
/// out the thing it is explaining.
fn split_legend(inner: Rect) -> (Rect, Rect) {
    let rows = (map_render::legend(inner.width).len() as u16).clamp(1, (inner.height / 2).max(1));
    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(rows)])
        .split(inner);
    (split[0], split[1])
}

fn split_caption(inner: Rect, rows: u16) -> (Rect, Option<Rect>) {
    if rows == 0 {
        return (inner, None);
    }
    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(rows)])
        .split(inner);
    (split[0], Some(split[1]))
}

fn draw_caption(frame: &mut Frame, caption: Option<Rect>, state: &AppState) {
    let (Some(rect), Some(at)) = (caption, state.map_cursor) else {
        return;
    };
    frame.render_widget(
        Paragraph::new(
            state
                .bound_map()
                .map(|map| map.describe(at).join("\n"))
                .unwrap_or_default(),
        )
        .wrap(Wrap { trim: true })
        .style(Style::default().add_modifier(Modifier::DIM)),
        rect,
    );
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
            .find(|session| session.view.name == name)
            .and_then(|session| session.view.color)
    };
    let (lines, extent) = visible_window(
        &channel.lines,
        area.height,
        area.width.saturating_sub(2),
        channel.back_offset,
        |_, line| channel_line(line, timestamps, &tint),
    );
    let title = format!(
        "{}{} ",
        pane_title(&channel.config.name, channel.unread, false),
        scroll_indicator(channel.back_offset)
    );
    // Channels aggregate across characters, so no one profile's colour
    // could stand for the pane.
    render_scrollback(frame, area, lines, title, focused, None, extent);
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
        prefix.push(timestamp(line));
    }
    if let Origin::Session(names) = &line.origin
        && let Some((first, rest)) = names.split_first()
    {
        // The character's own colour, the same one that tints their pane
        // border and tab (§11), so a channel that several characters talk
        // into can be read by who is talking rather than by squinting at
        // names. Uncoloured profiles keep the plain tag.
        //
        // Several names when one broadcast reached several characters. They
        // are named rather than counted because *which* characters heard it
        // is occasionally the information: a character outside the clan
        // does not hear clan chat, and `[2]` would hide that.
        let name = |name: &str| match tint(name) {
            Some(color) => Span::styled(name.to_string(), Style::default().fg(color)),
            None => Span::raw(name.to_string()),
        };
        prefix.push(Span::raw("["));
        prefix.push(name(first));
        for also in rest {
            prefix.push(Span::raw("+"));
            prefix.push(name(also));
        }
        prefix.push(Span::raw("] "));
    }
    prefixed(ansi_lines(&line.text), prefix)
}

/// The `HH:MM:SS ` a pane showing timestamps puts in front of a line.
///
/// Local, not UTC: a clock silently mislabeled as local would be wrong for
/// most players, every day, all year.
fn timestamp(line: &RetainedLine) -> Span<'static> {
    Span::raw(line.at.format("%H:%M:%S ").to_string())
}

/// Puts `prefix` in front of a rendered line's **first row only**. A
/// wrapped continuation is the same line, and repeating the stamp down its
/// rows would read as several.
fn prefixed(mut rendered: Vec<Line<'static>>, prefix: Vec<Span<'static>>) -> Vec<Line<'static>> {
    if prefix.is_empty() {
        return rendered;
    }
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
/// Where a pane's viewport sits in its buffer, for the scrollbar.
///
/// Counted in buffer *entries* rather than wrapped rows. `visible_window`
/// already knows the index of the topmost entry it kept, so this costs
/// nothing, and both numbers are then in the same unit — where mixing
/// wrapped rows against entry counts would put the thumb in the wrong
/// place on any pane with wrapped output.
pub(crate) struct Extent {
    pub total: usize,
    pub top: usize,
    /// Rows to scroll the rendered paragraph by, so a partly-visible line
    /// at the top of the window is cut rather than pushing the tail off.
    pub scroll: u16,
}

fn visible_window<T>(
    raws: &VecDeque<T>,
    area_height: u16,
    content_width: u16,
    back_offset: usize,
    parse: impl Fn(usize, &T) -> Vec<Line<'static>>,
) -> (Vec<Line<'static>>, Extent) {
    let viewport = area_height.saturating_sub(2) as usize; // borders
    let needed = viewport.saturating_add(back_offset);

    let mut window: Vec<Line<'static>> = Vec::new();
    let mut rows = 0usize;
    // Distinguishes "stopped because the viewport is covered" from "ran out
    // of buffer" — only the latter knows the true total, and only the
    // latter therefore has to clamp `back_offset` (below).
    let mut exhausted = true;
    let mut top = raws.len();

    for (i, raw) in raws.iter().enumerate().rev() {
        if rows >= needed {
            exhausted = false;
            break;
        }
        let mut parsed = parse(i, raw);
        rows += wrapped_rows(&parsed, content_width);
        parsed.append(&mut window);
        window = parsed;
        top = i;
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

    (
        window,
        Extent {
            total: raws.len(),
            top,
            scroll: scroll as u16,
        },
    )
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
    extent: Extent,
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
        .scroll((extent.scroll, 0));
    frame.render_widget(body, area);

    // Only when something is out of sight. A bar on a pane holding four
    // lines is furniture saying nothing, and every column it takes is one
    // the MUD's own output does not get.
    let shown = extent.total.saturating_sub(extent.top);
    if extent.total > shown {
        let mut bar = ScrollbarState::new(extent.total)
            .position(extent.top)
            .viewport_content_length(shown);
        frame.render_stateful_widget(
            // No arrows on the ends: they read as buttons, and nothing here
            // is clickable — this says how much there is and where you are,
            // and is not a control.
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None),
            area.inner(ratatui::layout::Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut bar,
        );
    }
}

/// The status bar (#18, and the home #23 and #40 will want).
///
/// Dark on light, which is what makes it read as chrome rather than as
/// one more line of the game: everything else in the client is light text
/// on the terminal's own background, so the inversion says "this row is
/// the client talking about itself".
///
/// Left is what the client is doing — the character commands go to, and
/// how its connection is holding up. Right is what wants attention, which
/// today is the unread warning count and nothing else. Right-aligned
/// because a badge that moves as the left side changes length is a badge
/// the eye has to search for.
/// Whether anything is drawn on top of the map pane this frame.
///
/// Every overlay here is centred on the whole terminal, so any of them
/// being open is enough — none can be open and *not* cover a docked map
/// column of any useful width.
fn overlay_covers_map(state: &AppState) -> bool {
    state.show_help
        || state.show_errors
        || state.palette.is_some()
        || state.mark_menu.is_some()
        || state.config_editor.is_some()
        || state.new_profile_wizard.is_some()
}

/// Removes colour from a finished frame, leaving bold, dim and reverse to
/// carry the hierarchy (#120, no-color.org).
///
/// Done to the whole buffer at the end of the frame rather than at each of
/// the places a style is built, because colour is not centralised: the
/// chrome, a profile's tint, the alarm hue, rule highlights and the MUD's
/// own ANSI (through `ansi_to_tui`) reach the screen by five different
/// routes. This is the one point they have all arrived at, so it is the
/// only place a single change can be complete — and the server's colour is
/// the half a per-call-site fix would silently miss.
fn strip_colour(buffer: &mut ratatui::buffer::Buffer) {
    for cell in &mut buffer.content {
        cell.fg = Color::Reset;
        cell.bg = Color::Reset;
        cell.underline_color = Color::Reset;
    }
}

fn draw_status(frame: &mut Frame, area: Rect, state: &AppState) {
    if area.width == 0 {
        return;
    }
    let bar = Style::new()
        .bg(map_render::palette::CHROME)
        .fg(map_render::palette::INK);

    let left = match state.bound() {
        Some(session) => {
            // The whole connection, in one place: who you are typing at,
            // where they are connected, whether it is encrypted, and how
            // the round trip is holding up. Each part is absent until it
            // is known, so the bar never shows an empty bracket or a
            // placeholder latency.
            let mut text = format!(" {}", session.view.name);
            if !session.view.status.is_empty() {
                text.push_str(&format!("  {}", session.view.status));
            }
            if !session.view.security.is_empty() {
                text.push_str(&format!("  [{}]", session.view.security));
            }
            if !session.view.latency.is_empty() {
                text.push_str(&format!("  {}", session.view.latency));
            }
            text
        }
        None => " no character".to_string(),
    };
    // Never "0 errors": a count of nothing is not news, and a bar that
    // always says something teaches the eye to stop reading it.
    let right = match state.errors_unread {
        0 => String::new(),
        1 => format!("!1 error ({}) ", crate::state::ERRORS_COMMAND),
        n => format!("!{n} errors ({}) ", crate::state::ERRORS_COMMAND),
    };

    let gap = (area.width as usize)
        .saturating_sub(left.chars().count() + right.chars().count())
        .max(1);
    let line = Line::from(vec![
        Span::styled(left, bar),
        Span::styled(" ".repeat(gap), bar),
        Span::styled(right, bar.add_modifier(Modifier::BOLD)),
    ]);
    frame.render_widget(Paragraph::new(line).style(bar), area);
}

/// The input pane's title: who the line goes to, and the only key hints the
/// client shows without being asked.
///
/// The palette (#121) is advertised here rather than in the status bar
/// because this is where a key hint already lived, and because the last row
/// is spent (#116). One hint, not the five a full hint row would carry: the
/// palette is the key that opens every other command, so it is the one worth
/// a permanent column.
///
/// Both keys are rendered from the binding, so a player who remapped either
/// is told their own key rather than the default.
fn input_title(name: Option<&str>, masked: bool, keybinds: &Keybinds, width: u16) -> String {
    let who = match name {
        Some(name) => format!(" input → {name}"),
        None => " input".to_string(),
    };
    if masked {
        // A password prompt is the wrong moment for chrome, which is why
        // this form never carried the quit hint either.
        return format!("{who} (hidden) ");
    }
    let full = format!(
        "{who} ({} cmds · {} quit) ",
        keybinds.palette, keybinds.quit
    );
    // ratatui truncates a title wider than its block without saying so, so
    // the choice on a narrow pane is not "cramped" but "the quit key silently
    // cut in half". The new hint yields; the one players already rely on
    // stays.
    if full.chars().count() <= width.saturating_sub(2) as usize {
        full
    } else {
        format!("{who} ({} to quit) ", keybinds.quit)
    }
}

fn draw_input(frame: &mut Frame, area: Rect, state: &AppState) {
    let Some(session) = state.bound() else {
        // No character, but still a line: `/connect` has to be typeable in
        // the one state a player most needs it (#108). Drawn with a real
        // cursor for the same reason — an input box without one reads as
        // decoration, which is exactly how this looked when it swallowed
        // every keystroke.
        let shell = Paragraph::new(state.shell_input.value()).block(
            Block::bordered()
                .title(input_title(None, false, &state.keybinds, area.width))
                .border_style(Style::new().dim()),
        );
        frame.render_widget(shell, area);
        frame.set_cursor_position((
            area.x + 1 + state.shell_input.visual_cursor() as u16,
            area.y + 1,
        ));
        return;
    };

    let (value, cursor) = if session.view.masked {
        (
            "*".repeat(session.view.input.value().chars().count()),
            session.view.input.cursor() as u16,
        )
    } else {
        (
            session.view.input.value().to_string(),
            session.view.input.visual_cursor() as u16,
        )
    };
    // The border names the session commands go to: with several characters
    // open, and focus possibly on a channel pane, that must never be a guess.
    let title = input_title(
        Some(&session.view.name),
        session.view.masked,
        &state.keybinds,
        area.width,
    );
    // The completion is drawn past the cursor rather than inserted (§11.3):
    // dim, so it reads as the client's guess rather than as something you
    // typed, and behind the cursor, so the cursor still sits where the next
    // character will land.
    let mut spans = vec![Span::raw(value)];
    if let Some(rest) = &session.view.suggestion {
        spans.push(Span::styled(rest.clone(), Style::new().dim()));
    }
    let input_line = Paragraph::new(Line::from(spans)).block(
        Block::bordered()
            .title(title)
            .border_style(Style::new().dim()),
    );
    frame.render_widget(input_line, area);

    frame.set_cursor_position((area.x + 1 + cursor, area.y + 1));
}

/// `! name ● 3` — the unread badge for a pane that isn't focused (§11),
/// and the mark for a character in trouble (§11.7). The space before the
/// count matters: `●` commonly renders wider than the single cell it's
/// measured as, and packed straight against a digit it visually collides
/// with it in most terminal fonts.
///
/// The `!` is shown on a focused pane too, which the unread badge never is:
/// unread means "you have not looked", so looking clears it, while a
/// character at a tenth of their health is in trouble whether or not
/// anybody is watching.
fn pane_title(name: &str, unread: usize, in_trouble: bool) -> String {
    let name = match in_trouble {
        true => format!("! {name}"),
        false => name.to_string(),
    };
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
        let in_trouble = session.view.distress.is_some();
        let label = format!(
            "{}:{}",
            index + 1,
            pane_title(&session.view.name, session.view.unread, in_trouble)
        );
        let mut style = match session.view.color {
            Some(color) => Style::new().fg(color),
            None => Style::new(),
        };
        style = if state.is_focused_session(index) {
            style.bold().reversed()
        } else if in_trouble {
            // Never dim, whatever else this tab is. Dimming is exactly the
            // treatment that would hide the one tab the player needs to
            // see, and an unfocused character is the case the alarm exists
            // for (§11.7).
            style.fg(ALARM).bold()
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
    /// `ui` draws the model; it must not reach into the module that owns
    /// the terminal and the event loop (#6, docs/ARCHITECTURE.md §4). Left
    /// to review this drifted back twice — `AppState` and `Focus` were
    /// imported straight from `app` for three milestones before a review
    /// caught it — so the boundary is checked by reading the source rather
    /// than by remembering. The needle is assembled rather than written
    /// out, so the check does not trip over its own source.
    #[test]
    fn ui_names_the_model_not_the_event_loop() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ui");
        for entry in std::fs::read_dir(&dir).expect("src/ui is readable") {
            let path = entry.expect("a readable entry").path();
            if path.extension().is_none_or(|ext| ext != "rs") {
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("a readable source file");
            let forbidden = concat!("crate", "::app");
            assert!(
                !source.contains(forbidden),
                "{} names `{forbidden}`; the model it wants lives in `crate::state`",
                path.display()
            );
        }
    }

    use std::collections::VecDeque;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::config::Channel;
    use crate::state::{ChannelPane, test_support};
    use tui_input::Input;

    /// A 30x10 terminal splits into a 6-tall output pane (4 content rows
    /// inside its border), a 1-row prompt, and a 3-tall input box. Content
    /// rows are 1..=4; row 0 and row 5 are the pane's own borders, so
    /// asserting `│` there would test the border, not corruption.
    /// The rows inside the session pane's border, derived from the layout
    /// rather than counted: this was `1..=4` until the status bar took a
    /// row, at which point row 4 became the bottom border and the failure
    /// read as "left border corrupted".
    fn content_rows(state: &AppState) -> std::ops::RangeInclusive<u16> {
        let pane = panes_of(state).sessions[0];
        (pane.y + 1)..=(pane.y + pane.height.saturating_sub(2))
    }

    fn state() -> AppState {
        test_support::app(&["kestrel"])
    }

    /// A terminal tall enough to leave the session pane `rows` of content.
    ///
    /// Derived rather than counted, because chrome has now changed height
    /// twice — the prompt row, then the status bar — and each time a
    /// hand-counted terminal height in a test silently started meaning
    /// something else. `overlay_fits` records the same lesson for width.
    fn terminal_height_for(state: &AppState, width: u16, rows: u16) -> u16 {
        (rows + 2..rows + 12)
            .find(|height| {
                session_pane_sizes(Rect::new(0, 0, width, *height), state)
                    .first()
                    .is_some_and(|(_, (_, tall))| *tall >= rows)
            })
            .expect("some terminal height gives the pane that many rows")
    }

    /// Where a pane actually lands, for tests that used to count rows from
    /// the bottom of a 30x10 buffer. Adding the status bar moved every one
    /// of them by a row, which is the same lesson `overlay_fits` records:
    /// a hand-counted offset is a number that silently stops being true.
    fn panes_of(state: &AppState) -> Panes {
        layout(Rect::new(0, 0, 30, 10), state)
    }

    fn prompt_row(state: &AppState) -> u16 {
        panes_of(state).prompt.expect("a prompt row").y
    }

    fn input_top(state: &AppState) -> u16 {
        panes_of(state).input.y
    }

    fn render(state: &AppState) -> ratatui::buffer::Buffer {
        render_sized(state, 30, 10)
    }

    /// As `render_sized`, but through the entry a real terminal uses — so the
    /// size floor applies (#122).
    fn screen_sized(state: &AppState, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut cache = MapImageCache::default();
        terminal
            .draw(|frame| {
                draw_screen(frame, state, &mut cache);
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn render_sized(state: &AppState, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut cache = MapImageCache::default();
        terminal
            .draw(|frame| {
                draw(frame, state, &mut cache);
            })
            .unwrap();
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
    /// Cells with a background of their own — room tiles, legend swatches
    /// — which is how the map tests count what was drawn.
    ///
    /// The status bar is excluded: it paints its whole row, so counting it
    /// would swamp the thing being measured. It is always the last row,
    /// which is the one property of it these tests need to know.
    fn filled_cells(buffer: &ratatui::buffer::Buffer) -> usize {
        (0..buffer.area.height.saturating_sub(1))
            .flat_map(|y| (0..buffer.area.width).map(move |x| (x, y)))
            .filter(|(x, y)| buffer.cell((*x, *y)).unwrap().bg != ratatui::style::Color::Reset)
            .count()
    }

    /// The legend's own contribution to `filled_cells` — one coloured
    /// swatch per entry, however many rows it wraps to — so a count meant
    /// to be about room tiles can subtract the legend rather than being
    /// broken by it.
    fn legend_swatches() -> usize {
        map_render::legend(u16::MAX)
            .iter()
            .flat_map(|line| line.spans.iter())
            .filter(|span| span.style.bg.is_some())
            .count()
    }

    fn row(buffer: &ratatui::buffer::Buffer, y: u16) -> String {
        let width = buffer.area.width;
        (0..width)
            .map(|x| buffer.cell((x, y)).unwrap().symbol())
            .collect()
    }

    fn assert_left_border_intact(state: &AppState, buffer: &ratatui::buffer::Buffer) {
        for y in content_rows(state) {
            assert_eq!(
                buffer.cell((0, y)).unwrap().symbol(),
                "│",
                "row {y} left border corrupted: {:?}",
                row(buffer, y)
            );
        }
    }

    // ---- the help overlay (§11.2) ----

    /// The listing outgrew a short terminal and was simply cut off, so the
    /// bottom of it — the commands, and how to leave — could not be read at
    /// all. Adding one binding was enough to do it.
    #[test]
    fn a_help_listing_taller_than_the_terminal_can_be_scrolled_to_its_end() {
        let mut state = state();
        state.show_help = true;

        let top = rows(&render_sized(&state, 70, 20));
        state.help_scroll = u16::MAX;
        let bottom = rows(&render_sized(&state, 70, 20));

        assert_ne!(top, bottom, "a short terminal has somewhere to scroll to");
        assert!(
            bottom.contains("Leaving"),
            "the end of the listing has to be reachable: {bottom}"
        );
    }

    /// And says so, rather than leaving the player to guess there is more.
    #[test]
    fn a_clipped_help_listing_shows_a_scrollbar_and_says_how_to_move() {
        let mut state = state();
        state.show_help = true;

        let clipped = rows(&render_sized(&state, 70, 20));

        // The listing itself has a `PgUp / PgDn` row, so match the title's
        // own spacing rather than the word.
        assert!(clipped.contains("PgUp/PgDn"), "{clipped}");
        assert!(
            clipped.contains('█') || clipped.contains('▐') || clipped.contains('║'),
            "a scrollbar should be drawn: {clipped}"
        );
    }

    /// A terminal with room for all of it gets neither, since there is
    /// nowhere to go and the hint would be noise.
    #[test]
    fn a_help_listing_that_fits_has_no_scrollbar() {
        let mut state = state();
        state.show_help = true;

        let roomy = rows(&render_sized(&state, 70, 60));

        assert!(roomy.contains("Leaving"), "all of it is on screen: {roomy}");
        assert!(
            !roomy.contains("PgUp/PgDn"),
            "no hint when there is nowhere to go: {roomy}"
        );
        assert!(!roomy.contains('█'), "and no scrollbar: {roomy}");
    }

    /// Scrolling past the end stops at the end rather than running the
    /// listing off the top of its own box.
    #[test]
    fn scrolling_the_help_stops_at_the_bottom() {
        let mut state = state();
        state.show_help = true;
        state.help_scroll = u16::MAX;
        let pinned = rows(&render_sized(&state, 70, 20));

        state.help_scroll = u16::MAX / 2;
        assert_eq!(rows(&render_sized(&state, 70, 20)), pinned);
    }

    // ---- the party strip (§11.6) ----    // ---- the party strip (§11.6) ----

    /// Publishes vitals for a session the way its own task would, so the
    /// strip reads them the way it will in play.
    fn publish_vitals(state: &mut AppState, name: &str, pairs: &[(&str, &str)]) {
        let (tx, rx) = tokio::sync::watch::channel(crate::engine::PeerSnapshot {
            vars: std::collections::HashMap::new(),
            data: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        });
        // The sender goes out of scope here on purpose: a `watch::Receiver`
        // still reads the last value published after its sender is gone,
        // which is exactly what the strip does between updates.
        drop(tx);
        state.peer_registry.insert(name.to_string(), rx);
    }

    #[test]
    fn the_strip_shows_every_character_at_once() {
        let mut state = test_support::app(&["tank", "cleric"]);
        state.show_hud = true;
        publish_vitals(
            &mut state,
            "tank",
            &[("HEALTH", "27"), ("HEALTH_MAX", "30")],
        );
        publish_vitals(
            &mut state,
            "cleric",
            &[("Char.Vitals.mp", "12"), ("Char.Vitals.maxmp", "40")],
        );

        let screen = rows(&render_sized(&state, 70, 12));

        assert!(screen.contains("tank"), "{screen}");
        assert!(screen.contains("hp 27"), "{screen}");
        assert!(screen.contains("cleric"), "{screen}");
        assert!(screen.contains("mp 12"), "{screen}");
    }

    #[test]
    fn the_strip_is_off_until_asked_for() {
        let mut state = test_support::app(&["tank"]);
        publish_vitals(
            &mut state,
            "tank",
            &[("HEALTH", "27"), ("HEALTH_MAX", "30")],
        );

        let screen = rows(&render_sized(&state, 70, 12));

        assert!(!screen.contains("hp 27"), "{screen}");
    }

    /// A MUD that reports nothing is said to report nothing — drawing it as
    /// a character at zero health would be a lie about someone who is
    /// probably fine.
    #[test]
    fn a_character_with_no_vitals_is_not_drawn_as_a_dying_one() {
        let mut state = test_support::app(&["tank"]);
        state.show_hud = true;
        publish_vitals(&mut state, "tank", &[("Room.Info.num", "1")]);

        let screen = rows(&render_sized(&state, 70, 12));

        assert!(screen.contains("tank"), "{screen}");
        assert!(!screen.contains("hp 0"), "{screen}");
    }

    /// The point of the strip is noticing trouble without reading it.
    /// Experience reads as a percentage rather than the raw number the
    /// other gauges show: `4500` means nothing without the ceiling beside
    /// it, and how far along the level is what the number is for.
    #[test]
    fn the_strip_shows_experience_as_progress_through_the_level() {
        let mut state = test_support::app(&["tank"]);
        state.show_hud = true;
        publish_vitals(
            &mut state,
            "tank",
            &[("EXPERIENCE", "4500"), ("EXPERIENCE_MAX", "9000")],
        );

        let screen = rows(&render_sized(&state, 70, 12));

        assert!(screen.contains("xp 50%"), "{screen}");
    }

    /// The bug this pins: `gauge_style` speaks danger — red at a quarter,
    /// amber at a half — and a character 10% into a level has just
    /// levelled, not nearly died. Painting that the colour the strip uses
    /// for "go and help them" (§11.7) would say the opposite of what
    /// happened.
    #[test]
    fn a_barely_started_level_is_never_painted_as_danger() {
        let mut state = test_support::app(&["tank"]);
        state.show_hud = true;
        publish_vitals(
            &mut state,
            "tank",
            &[
                ("HEALTH", "100"),
                ("HEALTH_MAX", "100"),
                ("EXPERIENCE", "1"),
                ("EXPERIENCE_MAX", "9000"),
            ],
        );

        let buffer = render_sized(&state, 70, 12);
        let xp = (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| (x, y)))
            .find(|(x, y)| {
                buffer.cell((*x, *y)).unwrap().symbol() == "x"
                    && buffer.cell((x + 1, *y)).unwrap().symbol() == "p"
            })
            .map(|(x, y)| buffer.cell((x, y)).unwrap().fg)
            .expect("the strip should show experience");

        assert_ne!(xp, ALARM, "1% of a level is not a character in trouble");
    }

    #[test]
    fn a_character_in_trouble_is_coloured_for_it() {
        let mut state = test_support::app(&["tank"]);
        state.show_hud = true;
        publish_vitals(
            &mut state,
            "tank",
            &[("HEALTH", "3"), ("HEALTH_MAX", "100")],
        );
        let hurt = render_sized(&state, 70, 12);

        publish_vitals(
            &mut state,
            "tank",
            &[("HEALTH", "95"), ("HEALTH_MAX", "100")],
        );
        let well = render_sized(&state, 70, 12);

        let colour_of = |buffer: &ratatui::buffer::Buffer| {
            (0..buffer.area.height)
                .flat_map(|y| (0..buffer.area.width).map(move |x| (x, y)))
                .find(|(x, y)| buffer.cell((*x, *y)).unwrap().symbol() == "h")
                .map(|(x, y)| buffer.cell((x, y)).unwrap().fg)
        };
        assert_ne!(
            colour_of(&hurt),
            colour_of(&well),
            "3 of 100 and 95 of 100 must not look the same"
        );
    }

    // ---- the map column (§16) ----    // ---- the map column (§16) ----

    /// Drops a room into a session's map with the exits it should have.
    fn map_room(state: &mut AppState, id: i64, exits: &[(&str, i64)]) {
        state.map_of_mut(0).rooms.insert(
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
        state.sessions[0].view.current_room = Some(crate::map::RoomId(1));

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
        state.sessions[0].view.current_room = Some(crate::map::RoomId(1));

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
        state.sessions[0].view.current_room = Some(crate::map::RoomId(1));

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
            .view
            .scrollback
            .push_back(RetainedLine::server("You are in a forest."));
        state.sessions[0].view.prompt = "HP:100 MP:50>".to_string();

        let buffer = render(&state);
        assert!(
            row(&buffer, prompt_row(&state)).contains("HP:100 MP:50>"),
            "prompt row: {:?}",
            row(&buffer, prompt_row(&state))
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
            .view
            .scrollback
            .push_back(RetainedLine::server("You are in a forest."));
        state.sessions[0]
            .view
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

    /// The status bar carries the connection — where you are, whether it
    /// is encrypted, and the round trip — and carries nothing where a
    /// number is not known yet, so it never shows a placeholder.
    ///
    /// It used to be the pane title's job, and for a while it was both,
    /// which is one place too many for the same fact.
    #[test]
    fn the_status_bar_shows_the_connection_once_there_is_one() {
        let mut state = state();
        state.sessions[0].view.status = "connected".to_string();
        state.sessions[0].view.security = "TLS".to_string();

        let status_row = |state: &AppState| {
            let area = Rect::new(0, 0, 60, 12);
            row(&render_sized(state, 60, 12), layout(area, state).status.y)
        };

        let before = status_row(&state);
        assert!(before.contains("[TLS]"), "status bar: {before:?}");
        assert!(before.contains("connected"), "status bar: {before:?}");
        assert!(!before.contains("ms"), "status bar: {before:?}");

        state.sessions[0].view.latency = "42ms".to_string();
        let after = status_row(&state);
        assert!(after.contains("42ms"), "status bar: {after:?}");

        // And no longer in two places at once.
        let title = row(&render_sized(&state, 60, 12), 0);
        assert!(!title.contains("42ms"), "still in the title: {title:?}");
        assert!(!title.contains("[TLS]"), "still in the title: {title:?}");
    }

    /// #121: the palette shipped in #118 with nothing on screen naming its
    /// key, and the first question after it merged was how to open it. The
    /// input title is where the client already advertises a key, so it is
    /// where the palette gets advertised too — and by rendering the binding
    /// rather than the string `Ctrl+P`, so a player who remapped it is told
    /// their own key rather than ours.
    /// #120, no-color.org: an environment that says "no colour" gets none.
    ///
    /// Asserted over every cell of a screen deliberately full of it, because
    /// colour is not centralised here — chrome, a profile's tint, the alarm
    /// hue and the MUD's own ANSI all arrive by different routes, and a test
    /// that checked one route would pass while the others still painted.
    #[test]
    fn nothing_is_coloured_when_the_environment_asked_for_none() {
        let mut state = state();
        state.sessions[0].view.color = Some(Color::Red);
        state.sessions[0].view.distress = Some(0.09);
        state.sessions[0].view.security = "TLS".to_string();

        // The control: without it, this screen is colourful. Without this
        // half the test would pass just as well against a blank frame.
        let colourful = render_sized(&state, 80, 24);
        assert!(
            colourful
                .content
                .iter()
                .any(|c| c.fg != Color::Reset || c.bg != Color::Reset),
            "the fixture should be colourful, or the assertion below proves nothing"
        );

        state.no_color = true;
        let plain = render_sized(&state, 80, 24);
        let coloured: Vec<_> = plain
            .content
            .iter()
            .filter(|c| c.fg != Color::Reset || c.bg != Color::Reset)
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(
            coloured.is_empty(),
            "these cells kept their colour: {coloured:?}"
        );

        // Colour goes; hierarchy stays. Bold, dim and reverse are not colour
        // and are what the interface still reads by.
        assert!(
            plain
                .content
                .iter()
                .any(|c| c.modifier != ratatui::style::Modifier::empty()),
            "stripping colour must not flatten the emphasis with it"
        );
    }

    #[test]
    fn the_input_title_names_the_key_that_opens_the_palette() {
        let mut state = state();
        let title = row(
            &render_sized(&state, 80, 12),
            layout(Rect::new(0, 0, 80, 12), &state).input.y,
        );
        assert!(
            title.contains(&state.keybinds.palette.to_string()),
            "the input title should name the palette key: {title:?}"
        );
        assert!(
            title.contains(&state.keybinds.quit.to_string()),
            "and must not have lost the quit key it already had: {title:?}"
        );

        // Remapped, the hint follows: the point is the player's binding, not
        // a hard-coded one.
        state.keybinds.palette = "alt+k".parse().unwrap();
        let title = row(
            &render_sized(&state, 80, 12),
            layout(Rect::new(0, 0, 80, 12), &state).input.y,
        );
        assert!(
            title.contains("Alt+K"),
            "a remapped palette key should advertise itself: {title:?}"
        );
        assert!(
            !title.contains("Ctrl+P"),
            "and the old default should be gone: {title:?}"
        );
    }

    /// Too narrow for both hints, the palette one goes and the quit one
    /// stays. ratatui truncates an over-wide block title silently, so the
    /// alternative is not a cramped title but a `Ctrl+Q` that players
    /// already rely on being quietly cut in half.
    ///
    /// The width is derived from the title itself, per `overlay_fits`: a
    /// hand-picked number stops meaning "one column too narrow" the moment
    /// a hint is reworded, and the test would still pass while testing
    /// nothing.
    #[test]
    fn a_title_too_narrow_for_both_hints_keeps_the_one_that_was_already_there() {
        let state = state();
        let full = input_title(Some("kestrel"), false, &state.keybinds, u16::MAX);
        let too_narrow = full.chars().count() as u16 - 1;

        let title = input_title(Some("kestrel"), false, &state.keybinds, too_narrow);
        assert!(
            title.contains(&state.keybinds.quit.to_string()),
            "the quit key must survive a narrow pane: {title:?}"
        );
        assert!(
            !title.contains(&state.keybinds.palette.to_string()),
            "the palette hint is the one that yields: {title:?}"
        );
        assert!(
            title.chars().count() <= too_narrow as usize,
            "the fallback must itself fit {too_narrow} columns: {title:?}"
        );
    }

    /// A password prompt is the wrong moment for chrome. The masked title
    /// already dropped the quit hint deliberately; it does not gain one.
    #[test]
    fn a_masked_input_advertises_nothing() {
        let mut state = state();
        state.sessions[0].view.masked = true;
        let title = row(
            &render_sized(&state, 80, 12),
            layout(Rect::new(0, 0, 80, 12), &state).input.y,
        );
        assert!(
            !title.contains(&state.keybinds.palette.to_string()),
            "no palette hint while typing a password: {title:?}"
        );
        assert!(title.contains("hidden"), "still says hidden: {title:?}");
    }

    /// A profile's colour has to reach both places a character is named,
    /// or the pane and its tab look like different characters (§11).
    #[test]
    fn a_profiles_color_tints_its_border_and_its_tab() {
        let mut state = test_support::app(&["tank", "cleric"]);
        state.sessions[0].view.color = Some(Color::Magenta);

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
        state.sessions[1].view.color = Some(Color::Magenta);
        state.sessions[1].view.unread = 3;

        let buffer = render_sized(&state, 40, 12);
        let badge = buffer
            .content()
            .iter()
            .take(40)
            .find(|cell| cell.symbol() == "●")
            .expect("the unread badge should render in the tab bar");
        assert_eq!(badge.fg, Color::Magenta);
    }

    /// A character in trouble is marked wherever their name is written, and
    /// the mark is not dimmed — dimming is precisely the treatment that
    /// would hide the one tab worth looking at (§11.7).
    #[test]
    fn a_character_in_trouble_is_marked_in_the_tab_bar() {
        let mut state = test_support::app(&["tank", "cleric"]);
        state.sessions[1].view.color = Some(Color::Magenta);
        state.sessions[1].view.distress = Some(0.1);

        let buffer = render_sized(&state, 40, 12);
        let bar = row(&buffer, 0);
        assert!(bar.contains("! cleric"), "{bar}");

        let mark = buffer
            .content()
            .iter()
            .take(40)
            .find(|cell| cell.symbol() == "!")
            .expect("the tab bar marks the character");
        assert_eq!(mark.fg, ALARM);
        assert!(!mark.modifier.contains(Modifier::DIM), "{mark:?}");
    }

    /// Unread means "you have not looked", so looking clears it. Trouble is
    /// a fact about the character, so looking does not — the mark stays on
    /// the pane being read (§11.7).
    #[test]
    fn the_mark_stays_on_the_pane_being_looked_at() {
        let mut state = test_support::app(&["tank", "cleric"]);
        state.sessions[0].view.distress = Some(0.1);

        let screen = rows(&render_sized(&state, 40, 12));

        assert!(screen.contains("! tank"), "{screen}");
    }

    /// The tab bar only exists in Tabs mode (`>1` session). In Splits, an
    /// unfocused pane's own unread badge is the only place the count shows
    /// at all — it should carry the same colour identity, for the same
    /// reason (UX_REVIEW.md H).
    #[test]
    fn splits_mode_unread_badge_matches_the_pane_color() {
        let mut state = test_support::app(&["tank", "cleric"]);
        state.layout = LayoutMode::Splits;
        state.sessions[1].view.color = Some(Color::Magenta);
        state.sessions[1].view.unread = 3;

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
        state.sessions[1].view.color = Some(Color::Green);
        state.layout = LayoutMode::Splits;

        let buffer = render_sized(&state, 60, 12);
        let border = buffer.cell((59, 1)).unwrap();
        assert_eq!(border.fg, Color::Green);
        assert!(border.modifier.contains(ratatui::style::Modifier::DIM));
    }

    /// Reported live with map graphics on: closing the warnings panel left
    /// the part of it that had been over the map still on screen.
    ///
    /// A reused sixel picture marks its cells `Skip` so ratatui will not
    /// paint over the pixels — which also stops it repainting them after
    /// an overlay drawn on top goes away. The cache key treats "an overlay
    /// was covering this" as part of what is on those cells, so opening
    /// and closing each force one fresh write.
    #[test]
    fn an_open_overlay_draws_the_map_as_cells_not_pixels() {
        use crate::map::{RoomId, RoomInfo};
        use std::collections::BTreeMap;

        let mut state = state();
        let mut map = crate::map::Map::default();
        map.observe(&RoomInfo {
            id: RoomId(1),
            name: Some("Town Square".to_string()),
            area: Some("Midgaard".to_string()),
            exits: BTreeMap::new(),
        });
        state.world_mut(0).map = map;
        state.sessions[0].view.current_room = Some(RoomId(1));
        state.show_map = true;
        state.map_width = crate::config::DEFAULT_MAP_WIDTH;
        // A terminal that reports a cell size is one that can draw pixels.
        state.map_cell_px = Some((8, 16));
        let mut cache = MapImageCache::default();

        let drew_a_picture = |state: &AppState, cache: &mut MapImageCache| {
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            let mut had_image = false;
            terminal
                .draw(|frame| had_image = draw(frame, state, cache).image.is_some())
                .unwrap();
            had_image
        };

        assert!(
            drew_a_picture(&state, &mut cache),
            "with nothing in the way the map is a picture"
        );

        state.show_errors = true;
        assert!(
            !drew_a_picture(&state, &mut cache),
            "a picture is written after the frame, so it would land on top \
             of the panel — the map has to be glyphs while one is open"
        );

        state.show_errors = false;
        assert!(
            drew_a_picture(&state, &mut cache),
            "and the picture comes back once the panel is gone"
        );

        // Every overlay, because each is centred on the whole terminal and
        // each would be painted over the same way.
        state.show_help = true;
        assert!(!drew_a_picture(&state, &mut cache));
    }

    /// The picture is cached against the rect it was drawn into, so
    /// moving the column has to invalidate it (#111). If it did not, the
    /// map would be redrawn from a cache keyed to where it used to be —
    /// which is the failure most likely to look like a rendering bug
    /// rather than a layout one.
    #[test]
    fn moving_the_map_column_does_not_reuse_the_old_picture() {
        let mut state = crate::state::test_support::app(&["tank"]);
        state.channels.push(ChannelPane {
            config: test_support::channel("chat"),
            lines: VecDeque::new(),
            unread: 0,
            scrollback_limit: 10_000,
            back_offset: 0,
        });
        state.show_channels = true;
        state.show_map = true;
        let area = Rect::new(0, 0, 120, 30);

        let before = layout(area, &state).map.expect("a map column");
        state.map_first = true;
        let after = layout(area, &state).map.expect("a map column");

        assert_ne!(
            before, after,
            "the map moved, so anything keyed on its rect is a different key"
        );
    }

    /// #111. Which side column sits next to the character panes is the
    /// player's to choose, so both orders have to actually come out of
    /// `layout` — and the two columns must not land on the same rect,
    /// which is what a disagreement between the constraint order and the
    /// index lookups would produce.
    #[test]
    fn the_map_and_comms_columns_swap_on_request() {
        let mut state = crate::state::test_support::app(&["tank"]);
        state.channels.push(ChannelPane {
            config: test_support::channel("chat"),
            lines: VecDeque::new(),
            unread: 0,
            scrollback_limit: 10_000,
            back_offset: 0,
        });
        state.show_channels = true;
        state.show_map = true;
        let area = Rect::new(0, 0, 120, 30);

        let comms_first = layout(area, &state);
        state.map_first = true;
        let map_first = layout(area, &state);

        let (comms_a, map_a) = (
            comms_first
                .channels
                .first()
                .copied()
                .expect("a comms column"),
            comms_first.map.expect("a map column"),
        );
        let (comms_b, map_b) = (
            map_first.channels.first().copied().expect("a comms column"),
            map_first.map.expect("a map column"),
        );

        assert!(comms_a.x < map_a.x, "comms is inboard by default");
        assert!(map_b.x < comms_b.x, "and outboard once swapped");
        assert_ne!(comms_b, map_b, "the two columns are never the same rect");
        assert_eq!(
            comms_a.width, comms_b.width,
            "swapping moves the columns, it does not resize them"
        );
        assert_eq!(map_a.width, map_b.width);
    }

    /// With only one side column shown there is nothing to swap with, and
    /// the order must not leave a hole where the missing one would be.
    #[test]
    fn swapping_with_only_one_column_shown_still_fills_the_space() {
        let mut state = crate::state::test_support::app(&["tank"]);
        state.show_channels = false;
        state.show_map = true;
        state.map_first = true;
        let area = Rect::new(0, 0, 120, 30);

        let panes = layout(area, &state);
        let map = panes.map.expect("a map column");
        assert!(panes.channels.is_empty(), "no comms column was asked for");
        assert_eq!(
            map.right(),
            area.right(),
            "the map takes the edge rather than leaving a gap where comms would have been"
        );
    }

    /// #55's mark has to reach the screen, not just the arithmetic. The
    /// border is chrome the map does not own, so this is the one test that
    /// says the two meet.
    #[test]
    fn a_side_with_more_map_beyond_it_is_lit_on_the_border() {
        use map_render::Beyond;
        let area = Rect::new(0, 0, 20, 9);
        let lit = |beyond: Beyond| {
            let mut terminal = Terminal::new(TestBackend::new(20, 9)).unwrap();
            terminal
                .draw(|frame| {
                    frame.render_widget(Block::bordered(), area);
                    mark_edges(frame, area, &beyond);
                })
                .unwrap();
            let buffer = terminal.backend().buffer().clone();
            // The middle of the right-hand border, which is where a run
            // lands on a nine-row pane.
            buffer[(area.right() - 1, area.top() + 4)].symbol() == ">"
        };

        assert!(
            lit(Beyond {
                right: true,
                ..Beyond::default()
            }),
            "rooms beyond the east edge should light it"
        );
        assert!(
            !lit(Beyond::default()),
            "a map with nothing beyond it must leave the border alone"
        );
        assert!(
            !lit(Beyond {
                left: true,
                ..Beyond::default()
            }),
            "the west edge lighting the east border would be a transposition"
        );
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

    /// A terminal the whole help listing fits inside, with a few rows and
    /// columns to spare so there are still panes visible around it. Mirrors
    /// `draw_help`'s own sizing, so adding a row or a longer description
    /// cannot silently push part of the listing out of a test's view.
    fn overlay_fits(keybinds: &Keybinds) -> (u16, u16) {
        let lines = help_lines(keybinds);
        let widest = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
        (widest as u16 + 4 + 10, lines.len() as u16 + 2 + 6)
    }

    /// The declared floor has to hold the help listing, which is the widest
    /// thing the client draws. Derived from the listing rather than restated:
    /// a row long enough to need more than `MIN_COLS` fails here, instead of
    /// silently truncating every row for anyone at the floor.
    #[test]
    fn the_minimum_width_holds_the_help_listing() {
        let lines = help_lines(&Keybinds::default());
        let widest = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
        // +4 for a border column and a space either side, as `draw_help` adds.
        assert!(
            (widest + 4) as u16 <= MIN_COLS,
            "the help listing needs {} columns but the floor is {MIN_COLS}",
            widest + 4
        );
    }

    /// A floor that leaves no scrollback would be a floor in name only. What
    /// the chrome costs is asked of `layout` rather than counted here — the
    /// prompt row and the status bar each changed that answer once already,
    /// and a hand-counted height would have gone quietly wrong both times.
    #[test]
    fn the_minimum_height_leaves_a_usable_session_pane() {
        /// Rows of scrollback the floor promises. Not a layout constant: it is
        /// the bar `MIN_ROWS` is chosen to clear, so a new row of chrome fails
        /// this test rather than eating the player's view unannounced.
        const PROMISED_ROWS: u16 = 16;

        let state = state();
        let needed = terminal_height_for(&state, MIN_COLS, PROMISED_ROWS);
        assert!(
            needed <= MIN_ROWS,
            "a {PROMISED_ROWS}-row pane needs a {needed}-row terminal, over the {MIN_ROWS} floor"
        );
    }

    /// Below the floor the player gets one legible sentence, not a layout
    /// squeezed into a space it cannot occupy (#122).
    #[test]
    fn a_terminal_under_the_floor_is_told_so_instead_of_drawn_into() {
        let drawn = rows(&screen_sized(&state(), MIN_COLS - 1, MIN_ROWS - 1));

        assert!(
            drawn.contains("too small"),
            "the notice should say what is wrong:\n{drawn}"
        );
        assert!(
            drawn.contains(&format!("{MIN_COLS}x{MIN_ROWS}")),
            "the notice should say what size is needed:\n{drawn}"
        );
        assert!(
            !drawn.contains("kestrel"),
            "no pane should be drawn under the notice:\n{drawn}"
        );
    }

    /// And gets its client back when the window grows, with no state to
    /// reset: the check is per frame, so recovery is simply the next one.
    #[test]
    fn a_terminal_grown_back_over_the_floor_draws_the_client_again() {
        let state = state();
        // Under the floor first, so this is recovery rather than a fresh start.
        let _ = screen_sized(&state, MIN_COLS - 1, MIN_ROWS - 1);
        let drawn = rows(&screen_sized(&state, MIN_COLS, MIN_ROWS));

        assert!(
            !drawn.contains("too small"),
            "the notice should be gone once there is room:\n{drawn}"
        );
        assert!(
            drawn.contains("kestrel"),
            "the character pane should be back:\n{drawn}"
        );
    }

    /// A notice that panics is worse than the layout it replaced. Every size
    /// down to 1x1 has to render, including ones too small for the text.
    #[test]
    fn the_too_small_notice_survives_any_terminal_it_can_be_shown_on() {
        let state = state();
        for (w, h) in [(1, 1), (2, 1), (1, 40), (40, 1), (9, 2), (79, 23), (20, 5)] {
            let drawn = rows(&screen_sized(&state, w, h));
            assert_eq!(
                drawn.lines().count(),
                h as usize,
                "a {w}x{h} terminal should still produce {h} rows"
            );
        }
    }

    /// The notice obeys `--no-color` like every other frame (#120).
    #[test]
    fn the_too_small_notice_is_stripped_under_no_color() {
        let mut state = state();
        state.no_color = true;
        let buffer = screen_sized(&state, MIN_COLS - 1, MIN_ROWS - 1);
        // Colour only: bold is emphasis, not colour, and NO_COLOR does not
        // ask for it to go.
        assert!(
            buffer
                .content
                .iter()
                .all(|cell| cell.fg == Color::Reset && cell.bg == Color::Reset),
            "no cell should carry colour under no_color"
        );
    }

    /// The overlay is as wide as its longest row, capped at the terminal —
    /// so one over-long description does not just look cramped, it silently
    /// truncates *every* row for anyone on a narrow terminal. 80 columns is
    /// the width to hold, being the one a default terminal still opens at.
    #[test]
    fn no_help_row_is_too_long_for_an_eighty_column_terminal() {
        let lines = help_lines(&Keybinds::default());
        let widest = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
        let longest = lines
            .iter()
            .max_by_key(|l| l.chars().count())
            .map(String::as_str)
            .unwrap_or("");
        // +4: a border column and a space either side, as `draw_help` adds.
        assert!(
            widest + 4 <= 80,
            "the help listing needs {} columns; shorten this row:\n{longest}",
            widest + 4
        );
    }

    /// The overlay covers what is under it — a listing rendered over live
    /// scrollback would be unreadable (docs/ARCHITECTURE.md §11.2).
    #[test]
    fn the_help_overlay_draws_over_the_panes() {
        let mut state = state();
        for _ in 0..40 {
            state.sessions[0]
                .view
                .scrollback
                .push_back(RetainedLine::server("You are in a forest."));
        }

        // Big enough for the whole listing plus room around it, *derived*
        // from the listing rather than tuned to it. Hand-tuned numbers here
        // were bumped five separate times — once per row the help grew (the
        // party alarm's key, the panes' clock, `/comms`, `/update`, `/send`)
        // — and each time the failure looked like a rendering bug rather
        // than "the overlay outgrew the test's terminal".
        let (width, height) = overlay_fits(&state.keybinds);
        let without = render_sized(&state, width, height);
        assert!(rows(&without).contains("forest"));

        state.show_help = true;
        let with = render_sized(&state, width, height);
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
        state.sessions[0].view.input =
            tui_input::Input::default().with_value("hunter2".to_string());

        let visible = render(&state);
        let value_row = input_top(&state) + 1;
        assert!(row(&visible, value_row).contains("hunter2"));

        state.sessions[0].view.masked = true;
        let hidden = render(&state);
        assert!(
            !row(&hidden, value_row).contains("hunter2"),
            "password leaked: {:?}",
            row(&hidden, value_row)
        );
        assert!(row(&hidden, value_row).contains("*******"));
    }

    /// NAWS must describe the output pane inside its border, not the whole
    /// terminal (docs/ARCHITECTURE.md §6.2).
    #[test]
    fn reports_pane_size_excluding_chrome() {
        let mut state = state();
        let area = Rect::new(0, 0, 30, 10);
        // 30 wide - 2 border columns; 10 tall - 1 prompt - 3 input - 1
        // status bar - 2 border. The status bar's row comes out of the
        // scrollback, which is the price of it being always present.
        assert_eq!(session_pane_sizes(area, &state), vec![(0, (28, 3))]);

        state.sessions[0].view.connected = false;
        assert_eq!(session_pane_sizes(area, &state), vec![(0, (28, 4))]);
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
            .view
            .scrollback
            .push_back(RetainedLine::server("tankline"));
        state.sessions[1]
            .view
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
            .view
            .scrollback
            .push_back(RetainedLine::server("tankline"));
        state.sessions[1]
            .view
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
        state.sessions[1].view.unread = 3;

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
        state.focus_pane(Focus::Session(state.id_at(1).unwrap()));
        state.focus_pane(Focus::Channel(0));

        let buffer = render_sized(&state, 70, 12);
        let input_row = row(&buffer, layout(Rect::new(0, 0, 70, 12), &state).input.y);
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

    /// The completion is shown before it is sent, dim and past the cursor:
    /// it is the client's guess, and a player who does not want it has to
    /// be able to see it is there (§11.3).
    #[test]
    fn the_input_line_shows_the_completion_as_a_dim_ghost() {
        let mut state = state();
        state.sessions[0].view.input = Input::default().with_value("look bull".into());
        state.sessions[0].view.suggestion = Some("ywug".to_string());

        let buffer = render_sized(&state, 40, 8);
        let screen: String = (0..8).map(|y| row(&buffer, y)).collect();
        assert!(screen.contains("look bullywug"), "{screen}");

        let ghost = (0..8)
            .flat_map(|y| (0..40).map(move |x| (x, y)))
            .find(|(x, y)| {
                buffer.cell((*x, *y)).unwrap().symbol() == "y"
                    && buffer.cell((x + 1, *y)).unwrap().symbol() == "w"
            })
            .map(|(x, y)| buffer.cell((x, y)).unwrap().modifier);

        assert_eq!(
            ghost.map(|m| m.contains(Modifier::DIM)),
            Some(true),
            "the guessed half is dim: {screen}"
        );
    }

    /// The cursor stays where the next character lands — in front of the
    /// ghost, not past it.
    #[test]
    fn the_cursor_sits_before_the_ghost() {
        let mut state = state();
        state.sessions[0].view.input = Input::default().with_value("look bull".into());
        state.sessions[0].view.suggestion = Some("ywug".to_string());

        let mut terminal = Terminal::new(TestBackend::new(40, 8)).unwrap();
        let mut cache = MapImageCache::default();
        terminal
            .draw(|frame| {
                draw(frame, &state, &mut cache);
            })
            .unwrap();

        // One for the border, nine for `look bull`.
        assert_eq!(terminal.get_cursor_position().unwrap().x, 1 + 9);
    }

    /// A comms pane aggregates several characters, so the `[name]` tag is
    /// the only thing saying who spoke — in that character's own colour,
    /// the same one tinting their pane border and tab (§11), so the pane
    /// can be read by who is talking rather than by squinting at names.
    #[test]
    fn a_channel_tags_each_line_in_the_speaker_s_own_colour() {
        let mut state = state();
        state.sessions[0].view.color = Some(Color::Magenta);
        let mut channel = ChannelPane {
            config: test_support::channel("comms"),
            lines: VecDeque::new(),
            unread: 0,
            scrollback_limit: 10_000,
            back_offset: 0,
        };
        channel.lines.push_back(RetainedLine::from_session(
            state.sessions[0].view.name.clone(),
            "Bob tells you hi",
        ));
        state.channels.push(channel);
        state.show_channels = true;

        let buffer = render_sized(&state, 70, 12);

        assert_eq!(
            tag_colour(&buffer, 70, 12),
            Some(Color::Magenta),
            "the tag should carry the character's colour: {}",
            (0..12).map(|y| row(&buffer, y)).collect::<String>()
        );
    }

    /// The colour is on the name, not on the brackets around it — with
    /// several names in one tag they are each their own character's colour,
    /// so the separators cannot belong to any of them.
    fn tag_colour(buffer: &ratatui::buffer::Buffer, width: u16, height: u16) -> Option<Color> {
        (0..height)
            .flat_map(|y| (0..width).map(move |x| (x, y)))
            .find(|(x, y)| buffer.cell((*x, *y)).unwrap().symbol() == "[")
            .map(|(x, y)| buffer.cell((x + 1, y)).unwrap().fg)
    }

    /// A profile with no colour set keeps a plain tag rather than picking
    /// one for it.
    #[test]
    fn an_uncoloured_profile_keeps_a_plain_tag() {
        let mut state = state();
        state.sessions[0].view.color = None;
        let mut channel = ChannelPane {
            config: test_support::channel("comms"),
            lines: VecDeque::new(),
            unread: 0,
            scrollback_limit: 10_000,
            back_offset: 0,
        };
        channel.lines.push_back(RetainedLine::from_session(
            state.sessions[0].view.name.clone(),
            "Bob tells you hi",
        ));
        state.channels.push(channel);
        state.show_channels = true;

        let buffer = render_sized(&state, 70, 12);

        assert_eq!(tag_colour(&buffer, 70, 12), Some(Color::Reset));
    }

    /// A broadcast several characters heard is one entry naming all of them
    /// (#57) — the sentence is not repeated once per character, and each
    /// name keeps its own colour so the tag can still be read at a glance.
    #[test]
    fn a_line_several_characters_heard_names_them_all_in_one_tag() {
        let mut state = state();
        state.sessions[0].view.color = Some(Color::Magenta);
        let mut channel = ChannelPane {
            config: test_support::channel("comms"),
            lines: VecDeque::new(),
            unread: 0,
            scrollback_limit: 10_000,
            back_offset: 0,
        };
        channel.lines.push_back(RetainedLine::with_origin(
            "Bob gossips hi",
            Origin::Session(vec![
                state.sessions[0].view.name.clone(),
                "cleric".to_string(),
            ]),
        ));
        state.channels.push(channel);
        state.show_channels = true;

        let buffer = render_sized(&state, 70, 12);
        let screen: String = (0..12).map(|y| row(&buffer, y)).collect();

        assert!(
            screen.contains("[kestrel+cleric] Bob"),
            "both names on one entry: {screen}"
        );
        assert_eq!(
            tag_colour(&buffer, 70, 12),
            Some(Color::Magenta),
            "the first name is still the first character's colour"
        );
    }

    /// A character pane's clock is composed the same way a channel's is,
    /// from the line's own arrival time — so it can be turned on for a
    /// buffer that filled up before anybody asked for it, and turned off
    /// again without leaving anything spliced into the text.
    #[test]
    fn a_character_pane_stamps_its_lines_when_the_toggle_is_on() {
        let mut state = state();
        let line = RetainedLine::server("Bob tells you hi");
        let at = line.at.format("%H:%M:%S").to_string();
        state.sessions[0].view.scrollback.push_back(line);

        let plain: String = {
            let buffer = render_sized(&state, 60, 10);
            (0..10).map(|y| row(&buffer, y)).collect()
        };
        assert!(plain.contains("Bob tells you hi"), "{plain}");
        assert!(!plain.contains(&at), "no clock until asked for: {plain}");

        state.show_timestamps = true;
        let stamped: String = {
            let buffer = render_sized(&state, 60, 10);
            (0..10).map(|y| row(&buffer, y)).collect()
        };
        assert!(
            stamped.contains(&format!("{at} Bob tells you hi")),
            "{stamped}"
        );
    }

    /// The stamp belongs to the line, not to each row it wraps onto:
    /// repeating it down a wrapped line would read as several lines.
    #[test]
    fn only_the_first_row_of_a_wrapped_line_is_stamped() {
        let mut state = state();
        state.show_timestamps = true;
        let line = RetainedLine::server("word ".repeat(20).trim_end());
        let at = line.at.format("%H:%M:%S").to_string();
        state.sessions[0].view.scrollback.push_back(line);

        let buffer = render_sized(&state, 40, 10);
        let screen: String = (0..10).map(|y| row(&buffer, y)).collect();

        assert_eq!(screen.matches(&at).count(), 1, "{screen}");
    }

    /// The timestamp is not stored in the line's text (§8): the pane owns
    /// the `timestamps:` setting, so the clock is composed here, from the
    /// line's own arrival time, and only on the pane that asked for one.
    #[test]
    fn a_timestamped_channel_composes_the_clock_from_the_lines_arrival_time() {
        let stored = "Bob tells you hi";
        // One line, reused: building a second one to render would take its
        // own `Local::now()`, and a second ticking over between the two
        // made this fail at random.
        let line = RetainedLine::server(stored);
        let at = line.at.format("%H:%M:%S").to_string();

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
            channel.lines.push_back(line.clone());
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

        state.world_mut(0).map = map;
        state.sessions[0].view.current_room = Some(RoomId(1));
        state.show_map = true;
        state.map_width = crate::config::DEFAULT_MAP_WIDTH;

        let tall = terminal_height_for(&state, 80, 26);
        let drawn = render_sized(&state, 80, tall);
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
            screen.contains('↓'),
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
            filled_cells(&drawn) - legend_swatches(),
            9,
            "three rooms, three cells each, drawn as filled ground: {screen}"
        );

        // With the column off, none of it is on screen — the description
        // is what remains, and that goes to the scrollback, not here.
        state.show_map = false;
        let hidden = rows(&render_sized(&state, 80, 20));
        assert!(!hidden.contains("Midgaard"), "{hidden}");
    }

    /// The bug this pins: rasterising and RLE-encoding a real map costs
    /// tens of milliseconds, and `draw` used to redo it on every frame —
    /// every keystroke, map-related or not — which was slow enough to be
    /// felt while typing. A second frame with nothing about the scene
    /// changed has to reuse the picture rather than rebuild it.
    #[test]
    fn a_second_frame_with_nothing_changed_reuses_the_map_picture() {
        use crate::map::{RoomId, RoomInfo};
        use std::collections::BTreeMap;

        let mut state = state();
        let mut map = crate::map::Map::default();
        map.observe(&RoomInfo {
            id: RoomId(1),
            name: Some("Town Square".to_string()),
            area: Some("Midgaard".to_string()),
            exits: BTreeMap::new(),
        });
        state.world_mut(0).map = map;
        state.sessions[0].view.current_room = Some(RoomId(1));
        state.show_map = true;
        state.map_width = crate::config::DEFAULT_MAP_WIDTH;
        state.map_cell_px = Some((8, 16));

        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        let mut cache = MapImageCache::default();

        let mut first = DrawnFrame {
            image: None,
            image_is_fresh: true,
            map_area: None,
            map_grid: None,
        };
        terminal
            .draw(|frame| first = draw(frame, &state, &mut cache))
            .unwrap();
        assert!(first.image.is_some(), "a sixel image should have rendered");
        assert!(first.image_is_fresh, "the first frame always has to render");

        let mut second = DrawnFrame {
            image: None,
            image_is_fresh: true,
            map_area: None,
            map_grid: None,
        };
        terminal
            .draw(|frame| second = draw(frame, &state, &mut cache))
            .unwrap();
        assert!(
            second.image.is_some(),
            "the picture must still be there to write, or the pane goes blank"
        );
        assert!(
            !second.image_is_fresh,
            "nothing about the scene changed, so this frame must reuse the cache"
        );

        // Moving invalidates it: the picture drawn afterward has to be a
        // different render, not the stale one from the old room.
        state.sessions[0].view.current_room = Some(RoomId(1));
        state.map_cursor = Some(RoomId(1));
        let mut third = DrawnFrame {
            image: None,
            image_is_fresh: true,
            map_area: None,
            map_grid: None,
        };
        terminal
            .draw(|frame| third = draw(frame, &state, &mut cache))
            .unwrap();
        assert!(
            third.image_is_fresh,
            "moving the cursor changes the scene the cache key covers"
        );
    }

    /// The bug this pins: `image_is_fresh` is what the event loop uses to
    /// decide whether to write the picture to the terminal, so it has to
    /// mean "the terminal needs this", not merely "we re-rasterised".
    /// Clearing the screen — which the loop does the moment a picture
    /// appears, to erase the "no room data yet" underneath it — takes the
    /// picture with it. Reusing the cache across that clear reported the
    /// picture as unchanged, nothing wrote it back, and the map stayed
    /// blank until a move forced a fresh one: the map appeared a move
    /// late.
    #[test]
    fn a_cleared_terminal_makes_the_next_frame_write_the_picture_again() {
        use crate::map::{RoomId, RoomInfo};

        let mut state = state();
        let mut map = crate::map::Map::default();
        map.observe(&RoomInfo {
            id: RoomId(1),
            name: None,
            area: Some("Midgaard".to_string()),
            exits: Default::default(),
        });
        state.world_mut(0).map = map;
        state.sessions[0].view.current_room = Some(RoomId(1));
        state.show_map = true;
        state.map_width = crate::config::DEFAULT_MAP_WIDTH;
        state.map_cell_px = Some((8, 16));

        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        let mut cache = MapImageCache::default();
        let mut draw_once = |cache: &mut MapImageCache| {
            let mut drawn = DrawnFrame {
                image: None,
                image_is_fresh: true,
                map_area: None,
                map_grid: None,
            };
            terminal
                .draw(|frame| drawn = draw(frame, &state, cache))
                .unwrap();
            drawn
        };

        assert!(draw_once(&mut cache).image_is_fresh, "the first frame");
        assert!(
            !draw_once(&mut cache).image_is_fresh,
            "an unchanged frame reuses it, which is the point of the cache"
        );

        cache.forget();
        let after_clear = draw_once(&mut cache);
        assert!(after_clear.image.is_some(), "there is still a picture");
        assert!(
            after_clear.image_is_fresh,
            "and the terminal no longer has it, so it must be written again"
        );
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

        state.world_mut(0).map = map;
        state.sessions[0].view.current_room = Some(RoomId(2));
        state.show_map = true;
        state.map_width = crate::config::DEFAULT_MAP_WIDTH;

        let tall = terminal_height_for(&state, 80, 26);
        let drawn = render_sized(&state, 80, tall);
        let screen = rows(&drawn);
        assert!(screen.contains('@'), "the current room is drawn: {screen}");
        assert_eq!(
            filled_cells(&drawn) - legend_swatches(),
            6,
            "and so is the one to its west: {screen}"
        );
    }

    /// The point of the legend: a player who has never seen the map before
    /// can tell what a colour means without leaving the pane, whether or
    /// not the server has placed them anywhere yet.
    #[test]
    fn the_map_pane_always_shows_the_legend() {
        let mut state = state();
        state.show_map = true;
        state.map_width = crate::config::DEFAULT_MAP_WIDTH;

        let empty = rows(&render_sized(&state, 80, 20));
        assert!(
            empty.contains("shop") && empty.contains("corpse"),
            "the legend should show even with no room data yet: {empty}"
        );

        use crate::map::{RoomId, RoomInfo};
        let mut map = crate::map::Map::default();
        map.observe(&RoomInfo {
            id: RoomId(1),
            name: Some("Town Square".to_string()),
            area: Some("Midgaard".to_string()),
            exits: Default::default(),
        });
        state.world_mut(0).map = map;
        state.sessions[0].view.current_room = Some(RoomId(1));

        let populated = rows(&render_sized(&state, 80, 20));
        assert!(
            populated.contains("shop") && populated.contains("you"),
            "the legend should stay put once the map has something to show: {populated}"
        );
    }

    /// The legend explained every colour but neither of the glyphs beside
    /// the room's own letter, so `^`, `v`, `↕` and `·` were on the map
    /// with nothing anywhere saying what they meant.
    #[test]
    fn the_legend_explains_the_exit_glyphs_too() {
        let mut state = state();
        state.show_map = true;
        state.map_width = crate::config::DEFAULT_MAP_WIDTH;

        let screen = rows(&render_sized(&state, 80, 24));

        for (glyph, meaning) in [
            ('↑', "up"),
            ('↓', "down"),
            ('↕', "up+down"),
            ('·', "more exits"),
        ] {
            // The glyph as well as the words: with no room data there is
            // no map drawn, so a `^` on screen can only have come from the
            // legend — which is what makes this more than a spellcheck.
            assert!(
                screen.contains(glyph),
                "`{glyph}` should appear in the legend: {screen}"
            );
            assert!(
                screen.contains(meaning),
                "`{glyph}` should be explained as `{meaning}`: {screen}"
            );
        }
    }

    /// The bug this pins: the box was a fixed 24 columns, sized for the
    /// list view's longest row — but typing a custom label swaps in
    /// "Enter to mark, Esc to go back", which is longer than that and
    /// nothing wrapped it, so it clipped silently at the border.
    #[test]
    fn the_mark_menus_typing_prompt_is_not_clipped() {
        let mut state = state();
        state.mark_menu = Some(crate::state::MarkMenu {
            at: crate::map::RoomId(1),
            selected: 0,
            existing: None,
            typing: Some("a shrine to".to_string()),
        });

        let screen = rows(&render_sized(&state, 80, 20));

        assert!(
            screen.contains("Enter to mark, Esc to go back"),
            "the full prompt should be on screen, not cut off: {screen}"
        );
        assert!(screen.contains("what is this room for?"), "{screen}");
    }

    /// The whole point, drawn: switching characters used to be the only
    /// way to find out where the other one was, and now both are on the
    /// one map at once.
    #[test]
    fn the_map_draws_the_other_character_too() {
        use crate::map::{RoomId, RoomInfo};
        use std::collections::BTreeMap;

        let mut state = test_support::app(&["mathias", "saihtam"]);
        let mut map = crate::map::Map::default();
        for id in [1, 2] {
            map.observe(&RoomInfo {
                id: RoomId(id),
                name: None,
                area: Some("Midgaard".to_string()),
                exits: BTreeMap::new(),
            });
        }
        map.connect(RoomId(1), "e", RoomId(2));
        map.connect(RoomId(2), "w", RoomId(1));
        for session in &mut state.sessions {
            session.map_key = "hercmud.net".to_string();
        }
        state.world_mut(0).map = map;
        state.sessions[0].view.current_room = Some(RoomId(1));
        state.sessions[1].view.current_room = Some(RoomId(2));
        state.show_map = true;
        state.map_width = crate::config::DEFAULT_MAP_WIDTH;

        let screen = rows(&render_sized(&state, 80, 20));

        assert!(screen.contains('@'), "you are still on it: {screen}");
        assert!(
            screen.contains('S'),
            "and saihtam's initial marks where they are: {screen}"
        );
    }

    /// Before the server has placed the character there is nothing to draw,
    /// and an empty bordered box reads as a broken pane.
    #[test]
    fn the_map_pane_says_when_it_has_no_room_data() {
        let mut state = state();
        state.show_map = true;
        state.map_width = crate::config::DEFAULT_MAP_WIDTH;

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
            .view
            .scrollback
            .push_back(RetainedLine::server(format!(
                "{} {} {}",
                "a".repeat(4),
                "b".repeat(24),
                "c".repeat(4)
            )));
        for i in 0..10 {
            state.sessions[0]
                .view
                .scrollback
                .push_back(RetainedLine::server(format!("line {i}")));
        }
        state.sessions[0].view.prompt = "By what name do you wish to be known?".to_string();

        assert_left_border_intact(&state, &render(&state));
    }

    /// Redraws on every event, exactly like the real event loop: a banner
    /// arriving line by line, a prompt, then more server lines — reusing
    /// one `Terminal` (and its diff buffer) across draws.
    #[test]
    fn repeated_draws_never_corrupt_the_left_border() {
        let mut state = state();
        let tall = terminal_height_for(&state, 30, 4);
        let mut terminal = Terminal::new(TestBackend::new(30, tall)).unwrap();
        let mut cache = MapImageCache::default();

        let banner = [
            "Welcome to FakeMUD",
            "",
            "A tale of two cities.",
            "It was the best of times, it was the worst of times.",
        ];
        for line in banner {
            state.sessions[0]
                .view
                .scrollback
                .push_back(RetainedLine::server(line));
            terminal
                .draw(|frame| {
                    draw(frame, &state, &mut cache);
                })
                .unwrap();
            assert_left_border_intact(&state, terminal.backend().buffer());
        }

        state.sessions[0].view.prompt = "By what name do you wish to be known?".to_string();
        terminal
            .draw(|frame| {
                draw(frame, &state, &mut cache);
            })
            .unwrap();
        assert_left_border_intact(&state, terminal.backend().buffer());

        state.sessions[0].view.prompt.clear();
        state.sessions[0]
            .view
            .scrollback
            .push_back(RetainedLine::server("> crazy-foo"));
        for line in ["Password:", "Reconnecting.", "", "i107 >"] {
            state.sessions[0]
                .view
                .scrollback
                .push_back(RetainedLine::server(line));
            terminal
                .draw(|frame| {
                    draw(frame, &state, &mut cache);
                })
                .unwrap();
            assert_left_border_intact(&state, terminal.backend().buffer());
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
                .view
                .scrollback
                .push_back(RetainedLine::server(format!("line {i}")));
        }

        let at_tail = rows(&render_sized(&state, 40, 12));
        assert!(
            !at_tail.contains("scrolled"),
            "a tailed pane must show no indicator: {at_tail}"
        );

        state.sessions[0].view.back_offset = 5;
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
                .view
                .scrollback
                .push_back(RetainedLine::server(format!("line {i}")));
        }
        state.sessions[0].view.back_offset = usize::MAX;

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
    /// buffer reads exactly as the same tail does on its own.
    ///
    /// The scrollbar is excluded on purpose — it is the one thing that
    /// *should* differ, since one of these has five thousand lines above
    /// the view and the other has none. Saying so here is cheaper than a
    /// reader wondering why the comparison is narrowed.
    #[test]
    fn a_huge_buffer_shows_the_same_tail_as_a_small_one() {
        let mut big = state();
        for i in 0..5_000 {
            big.sessions[0]
                .view
                .scrollback
                .push_back(RetainedLine::server(format!("line {i}")));
        }
        let mut small = state();
        for i in 4_980..5_000 {
            small.sessions[0]
                .view
                .scrollback
                .push_back(RetainedLine::server(format!("line {i}")));
        }

        let text = |state: &AppState| {
            let buffer = render_sized(state, 40, 12);
            (0..buffer.area.height)
                .map(|y| {
                    // Drop the last column, where the bar is drawn.
                    let line = row(&buffer, y);
                    line.chars().take(39).collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        assert_eq!(text(&big), text(&small));
    }

    /// Scrolled back into a large buffer, the window has to start further
    /// up — the offset must still land on the right lines rather than
    /// silently pinning to the tail.
    #[test]
    fn scrolling_back_into_a_large_buffer_lands_on_the_right_lines() {
        let mut state = state();
        for i in 0..5_000 {
            state.sessions[0]
                .view
                .scrollback
                .push_back(RetainedLine::server(format!("line {i}")));
        }
        state.sessions[0].view.back_offset = 100;

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
                .view
                .inspector_log
                .push_back(format!("gmcp-{i}"));
        }
        // A large offset set while looking at the scrollback, carried
        // along when the player then hits F2.
        state.sessions[0].view.back_offset = usize::MAX;
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
