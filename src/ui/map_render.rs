//! Painting a [`Scene`] into the map pane (§16).
//!
//! The seam is the [`MapRenderer`] trait: everything above it is the map's
//! own knowledge in grid space, everything below is one way of putting that
//! on screen. [`CharRenderer`] is the character-cell implementation, and the
//! reason the trait exists rather than being inlined is that a terminal with
//! a graphics protocol (Sixel, or kitty's) could paint the same scene as
//! pixels — true diagonals, arbitrary marks — while every other terminal,
//! every tmux session and every ssh login keeps the character version.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::map::{RoomRole, Scene};

/// One way of drawing a map scene into a pane.
pub(crate) trait MapRenderer {
    fn draw(&self, frame: &mut Frame, area: Rect, scene: &Scene);
}

/// A room occupies three columns and a corridor the gap beside it, so a
/// grid step is four columns across and two rows down. In pixels that is
/// close to square on a typical cell, which is what keeps a diagonal
/// looking like a diagonal.
const STEP_X: i32 = 4;
const STEP_Y: i32 = 2;

/// What each role paints. Colour carries the meaning here because a room is
/// three cells of solid ground with one slot on it — shape alone cannot say
/// "shop" at this size, and a letter plus a hue says it twice, which is what
/// survives a monochrome terminal or a colour-blind reader.
fn role_style(role: RoomRole) -> (Style, Option<char>) {
    match role {
        RoomRole::Here => (
            Style::default()
                .bg(Color::Cyan)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
            Some('@'),
        ),
        RoomRole::Corpse => (
            Style::default()
                .bg(Color::Red)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
            Some('X'),
        ),
        RoomRole::Known => (Style::default().bg(Color::DarkGray), None),
    }
}

/// Draws the scene as filled cells on a character grid.
///
/// Rooms are a *background* fill rather than a glyph: a filled cell is the
/// most ink a terminal can put in one place, it needs no character at all so
/// no font can fail to have it, and it leaves the foreground free to carry a
/// letter. `▓` would be only three-quarters ink and would let the pane
/// ground show through.
pub(crate) struct CharRenderer;

impl MapRenderer for CharRenderer {
    fn draw(&self, frame: &mut Frame, area: Rect, scene: &Scene) {
        let width = area.width as i32;
        let height = area.height as i32;
        // The current room sits in the middle, so walking scrolls the world
        // past a fixed marker rather than sliding a dot at an edge it then
        // falls off (§16).
        let origin_col = width / 2 - 1;
        let origin_row = height / 2;

        let mut cells: Vec<Vec<(char, Style)>> =
            vec![vec![(' ', Style::default()); area.width as usize]; area.height as usize];
        let mut put = |col: i32, row: i32, ch: char, style: Style| {
            if col >= 0 && row >= 0 && col < width && row < height {
                cells[row as usize][col as usize] = (ch, style);
            }
        };

        // Corridors first, so a room's fill wins wherever the two want the
        // same cell — the room is the thing being connected.
        let corridor = Style::default().fg(Color::DarkGray);
        for link in &scene.links {
            let col = origin_col + link.from.0 * STEP_X;
            let row = origin_row + link.from.1 * STEP_Y;
            // ASCII `\` and `/` rather than `╲` and `╱`: the box-drawing
            // diagonals are missing from Ubuntu Mono and Liberation Mono,
            // which are live defaults on real machines, where `─` and `│`
            // are in every monospace font measured.
            match link.step {
                (1, 0) => put(col + 3, row, '─', corridor),
                (-1, 0) => put(col - 1, row, '─', corridor),
                (0, 1) => put(col + 1, row + 1, '│', corridor),
                (0, -1) => put(col + 1, row - 1, '│', corridor),
                (1, 1) => put(col + 3, row + 1, '\\', corridor),
                (-1, -1) => put(col - 1, row - 1, '\\', corridor),
                (1, -1) => put(col + 3, row - 1, '/', corridor),
                (-1, 1) => put(col - 1, row + 1, '/', corridor),
                _ => {}
            }
        }

        for room in &scene.rooms {
            let col = origin_col + room.at.0 * STEP_X;
            let row = origin_row + room.at.1 * STEP_Y;
            let (style, letter) = role_style(room.role);
            // Three slots: a tick for exits off the grid, the room's own
            // mark, and ground. All on one fill, so none of it crowds.
            let tick = match (room.up, room.down) {
                (true, true) => '↕',
                (true, false) => '^',
                (false, true) => 'v',
                (false, false) => ' ',
            };
            put(col, row, tick, style);
            put(col + 1, row, letter.unwrap_or(' '), style);
            put(col + 2, row, ' ', style);
        }

        let lines: Vec<Line> = cells
            .into_iter()
            .map(|row| {
                // Runs of one style become one span, so a row of map is a
                // handful of spans rather than one per cell.
                let mut spans: Vec<Span> = Vec::new();
                for (ch, style) in row {
                    match spans.last_mut() {
                        Some(last) if last.style == style => last.content.to_mut().push(ch),
                        _ => spans.push(Span::styled(ch.to_string(), style)),
                    }
                }
                Line::from(spans)
            })
            .collect();

        frame.render_widget(Paragraph::new(lines), area);
    }
}
