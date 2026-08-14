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

use crate::map::{RoomId, RoomRole, Scene};

/// A picture the terminal has to be told about directly, because it is not
/// made of cells: the escape sequence, the pane it covers, and the glyphs
/// to write on top once it is drawn (§16).
///
/// `Clone` so `MapImageCache` can hand out a copy of the last one rendered
/// on a frame where nothing about the scene changed, without re-rasterising.
#[derive(Clone)]
pub(crate) struct PendingImage {
    pub area: Rect,
    pub sixel: String,
    pub glyphs: Vec<(u16, u16, char, Style)>,
}

/// One way of drawing a map scene into a pane.
pub(crate) trait MapRenderer {
    /// `cursor` is view state, not map knowledge — where the player is
    /// *looking*, which the map has no opinion about — so it arrives here
    /// rather than in the [`Scene`].
    /// Returns a picture for the caller to write after the frame, for a
    /// renderer that paints pixels. A cell renderer draws into `frame` and
    /// returns `None`.
    fn draw(
        &self,
        frame: &mut Frame,
        area: Rect,
        scene: &Scene,
        cursor: Option<RoomId>,
    ) -> Option<PendingImage>;
}

/// A room occupies three columns and a corridor the gap beside it, so a
/// grid step is four columns across and two rows down. In pixels that is
/// close to square on a typical cell, which is what keeps a diagonal
/// looking like a diagonal.
pub(super) const STEP_X: i32 = 4;
pub(super) const STEP_Y: i32 = 2;

/// What each role paints. Colour carries the meaning here because a room is
/// three cells of solid ground with one slot on it — shape alone cannot say
/// "shop" at this size, and a letter plus a hue says it twice, which is what
/// survives a monochrome terminal or a colour-blind reader.
pub(super) fn role_style(role: RoomRole) -> (Style, Option<char>) {
    match role {
        RoomRole::Here => (
            Style::default()
                .bg(palette::HERE)
                .fg(ink_for(palette::HERE))
                .add_modifier(Modifier::BOLD),
            Some('@'),
        ),
        RoomRole::Corpse => (
            Style::default()
                .bg(palette::CORPSE)
                .fg(ink_for(palette::CORPSE))
                .add_modifier(Modifier::BOLD),
            Some('X'),
        ),
        RoomRole::Known => (
            Style::default()
                .bg(palette::KNOWN)
                .fg(ink_for(palette::KNOWN)),
            None,
        ),
    }
}

/// The map's palette, given in RGB rather than the sixteen ANSI colours.
///
/// The ANSI names are whatever the player's terminal theme says they are —
/// one person's `Yellow` is mustard and another's is near-white — so a set
/// chosen to be told apart cannot be, and the pane ends up looking like
/// whatever the theme happened to do to it. Naming the colours outright is
/// the only way the map looks like one designed thing.
///
/// The label colours are the **Okabe-Ito** qualitative palette, which was
/// built to stay distinguishable under all three common kinds of
/// colour-vision deficiency — the same reason the letter is drawn on top
/// of the colour rather than instead of it. Its eighth entry is black,
/// which cannot be a background here, so seven remain.
pub(super) mod palette {
    use ratatui::style::Color;

    pub const HERE: Color = Color::Rgb(0x38, 0xBD, 0xF8); // sky
    pub const CORPSE: Color = Color::Rgb(0xE1, 0x1D, 0x48); // rose
    pub const KNOWN: Color = Color::Rgb(0x3F, 0x46, 0x51); // slate
    pub const CORRIDOR: Color = Color::Rgb(0x64, 0x74, 0x8B); // lighter slate
    pub const INK: Color = Color::Rgb(0x0B, 0x0E, 0x11); // near-black
    pub const PAPER: Color = Color::Rgb(0xF8, 0xFA, 0xFC); // near-white

    /// Okabe-Ito, minus its black.
    pub const LABELS: [Color; 7] = [
        Color::Rgb(0xE6, 0x9F, 0x00), // orange
        Color::Rgb(0x56, 0xB4, 0xE9), // sky blue
        Color::Rgb(0x00, 0x9E, 0x73), // bluish green
        Color::Rgb(0xF0, 0xE4, 0x42), // yellow
        Color::Rgb(0x00, 0x72, 0xB2), // blue
        Color::Rgb(0xD5, 0x5E, 0x00), // vermillion
        Color::Rgb(0xCC, 0x79, 0xA7), // reddish purple
    ];
}

/// Dark ink on a light colour, light ink on a dark one, by relative
/// luminance rather than by listing which is which — the palette can grow
/// without anyone remembering to update a second table.
fn ink_for(background: Color) -> Color {
    let Color::Rgb(r, g, b) = background else {
        return palette::INK;
    };
    // Rec. 601 luma, which is close enough for deciding light from dark.
    let luma = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
    match luma > 140.0 {
        true => palette::INK,
        false => palette::PAPER,
    }
}

/// A room the player has labelled: its initial, on a colour picked from the
/// label itself.
///
/// Derived rather than configured, so `shop` is the same colour on every
/// map, in every session, without anyone having to say so — and `shop` and
/// `smith`, which share an initial, do not share a colour.
///
/// The hash is written out rather than taken from `DefaultHasher`, whose
/// `RandomState` is seeded per process: the same label would land on a
/// different colour every launch, which is worse than one colour for
/// everything, because it would look like the map meant something by it.
pub(super) fn marked_style(label: &str) -> (Style, Option<char>) {
    let label = label.trim();
    let folded = label.to_ascii_lowercase();
    // The labels `/mark` offers get a colour each, by position, so the
    // common ones are all told apart. Hashing alone put three of the nine
    // on one colour and two more pairs on another — fine for arbitrary
    // labels, poor for the list the client itself suggests.
    let slot = crate::app::MARK_SUGGESTIONS
        .iter()
        .position(|known| *known == folded)
        .unwrap_or_else(|| {
            // Anything else: derived from the label, so it is at least
            // stable and usually distinct.
            let mut hash: u32 = 0x811c_9dc5;
            for byte in folded.bytes() {
                hash ^= byte as u32;
                hash = hash.wrapping_mul(0x0100_0193);
            }
            hash as usize
        });
    let background = palette::LABELS[slot % palette::LABELS.len()];
    let style = Style::default()
        .bg(background)
        .fg(ink_for(background))
        .add_modifier(Modifier::BOLD);
    // The initial is what fits, and a letter is the glyph a font is
    // actually built to render at this size, where a pictogram is a smudge.
    // The whole label is in `Map::describe`.
    (
        style,
        label.chars().next().map(|first| first.to_ascii_uppercase()),
    )
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
    fn draw(
        &self,
        frame: &mut Frame,
        area: Rect,
        scene: &Scene,
        cursor: Option<RoomId>,
    ) -> Option<PendingImage> {
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
        let corridor = Style::default().fg(palette::CORRIDOR);
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
            let (mut style, mut letter) = role_style(room.role);
            // Where the player is, and where their corpse is, outrank a
            // label: both are things they are looking for right now, and a
            // room has one slot to say it with.
            if room.role == RoomRole::Known
                && let Some(mark) = room.mark.as_deref()
            {
                (style, letter) = marked_style(mark);
            }
            // Three slots: a tick for exits off the grid, the room's own
            // mark, and ground. All on one fill, so none of it crowds.
            let tick = match (room.up, room.down) {
                (true, true) => '↕',
                (true, false) => '^',
                (false, true) => 'v',
                (false, false) => ' ',
            };
            // The third slot is the one that was ground, so nothing has to
            // give way for this: the tick and the mark both keep their
            // cell, and a room with nothing left out still draws blank
            // there. `·` rather than a louder glyph because the dot is
            // saying "there is more here than fits", not "look at me" — it
            // must never outshout the `@` or an `X` a cell away. It is also
            // in every monospace font measured, unlike the dashed and
            // diagonal box-drawing sets, which Ubuntu Mono and Liberation
            // Mono are both missing.
            let elided = if room.hidden_exits { '·' } else { ' ' };
            // Reversed rather than recoloured: every colour here already
            // means something — where you are, your corpse, a label — and
            // the cursor has to read on top of any of them without
            // pretending to be another kind of room.
            if cursor == Some(room.id) {
                style = style.add_modifier(Modifier::REVERSED);
            }
            put(col, row, tick, style);
            put(col + 1, row, letter.unwrap_or(' '), style);
            put(col + 2, row, elided, style);
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
        None
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::map::{Map, RoomId, RoomInfo};

    /// Every label used to draw the same yellow, so a shop, a well and a
    /// smith were one colour told apart only by an initial — and `shop` and
    /// `smith` do not differ there either.
    #[test]
    fn different_labels_take_different_colours() {
        assert_ne!(marked_style("shop").0.bg, marked_style("water").0.bg);
        assert_ne!(marked_style("bank").0.bg, marked_style("healer").0.bg);
    }

    /// The property that makes a derived colour usable at all: `shop` is
    /// the same colour on every map, in every session, on every launch.
    /// `DefaultHasher` would have broken this silently — its `RandomState`
    /// is seeded per process, so the colours would reshuffle on every
    /// start, which is worse than one colour for everything because it
    /// would look like the map meant something by the change.
    #[test]
    fn a_label_keeps_its_colour_between_runs() {
        // Pinned, not compared with itself: a self-comparison passes
        // happily under a per-process seed.
        assert_eq!(marked_style("shop").0.bg, Some(palette::LABELS[0]));
        assert_eq!(marked_style("water").0.bg, Some(palette::LABELS[1]));
        // An arbitrary label goes through the hash rather than the table
        // and has to be just as steady.
        assert_eq!(marked_style("grocer").0.bg, marked_style("grocer").0.bg);
    }

    /// The labels the client offers are the ones most likely to share a
    /// map, so they take a colour each by position, spreading across the
    /// whole palette. Hashing alone put three of the nine on one colour.
    #[test]
    fn the_offered_labels_spread_across_the_palette() {
        let used: std::collections::HashSet<Option<Color>> = crate::app::MARK_SUGGESTIONS
            .iter()
            .map(|label| marked_style(label).0.bg)
            .collect();

        assert_eq!(
            used.len(),
            palette::LABELS.len(),
            "every palette colour should be in play: {used:?}"
        );
    }

    /// Okabe-Ito is seven colours and there are nine offered labels, so two
    /// pairs share a hue. They must at least differ by letter, which is the
    /// whole reason the letter is drawn on top of the colour rather than
    /// instead of it.
    #[test]
    fn labels_sharing_a_colour_do_not_share_a_letter() {
        let mut seen: std::collections::HashMap<(Option<Color>, Option<char>), &str> =
            std::collections::HashMap::new();
        for label in crate::app::MARK_SUGGESTIONS {
            let (style, letter) = marked_style(label);
            if let Some(other) = seen.insert((style.bg, letter), label) {
                panic!("`{label}` and `{other}` are indistinguishable");
            }
        }
    }

    /// Ink is chosen by luminance, so a dark swatch takes light text and a
    /// light one takes dark. A fixed foreground made two of the seven
    /// unreadable.
    #[test]
    fn every_colour_in_the_pane_is_legible() {
        let contrast = |bg: Color| {
            let (Color::Rgb(r, g, b), Some(Color::Rgb(ir, ig, ib))) = (bg, Some(ink_for(bg)))
            else {
                panic!("the pane's palette is RGB throughout");
            };
            let luma = |r: u8, g: u8, b: u8| 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
            (luma(r, g, b) - luma(ir, ig, ib)).abs()
        };
        for bg in
            palette::LABELS
                .iter()
                .copied()
                .chain([palette::HERE, palette::CORPSE, palette::KNOWN])
        {
            assert!(
                contrast(bg) > 60.0,
                "{bg:?} and its ink are too close to read"
            );
        }
    }

    /// A label may never impersonate the colours that already mean
    /// something else in this pane.
    #[test]
    fn no_label_colour_collides_with_a_role_colour() {
        for label in crate::app::MARK_SUGGESTIONS {
            let bg = marked_style(label).0.bg;
            assert_ne!(bg, Some(palette::HERE), "{label} would read as the player");
            assert_ne!(bg, Some(palette::CORPSE), "{label} would read as a corpse");
            assert_ne!(bg, Some(palette::KNOWN), "{label} would read as unlabelled");
        }
    }

    /// Scenes are built through the map rather than by hand:    /// Scenes are built through the map rather than by hand: `PlacedRoom`
    /// is the map's own vocabulary and the renderer only ever reads it.
    fn scene_of(edges: &[(i64, &str, i64)]) -> Scene {
        let mut map = Map::default();
        for id in [1, 2, 4] {
            map.observe(&RoomInfo {
                id: RoomId(id),
                name: None,
                area: Some("Test".to_string()),
                exits: BTreeMap::new(),
            });
        }
        for (from, dir, to) in edges {
            map.connect(RoomId(*from), dir, RoomId(*to));
        }
        map.scene(RoomId(1), None)
    }

    fn render(scene: &Scene) -> String {
        let mut terminal = Terminal::new(TestBackend::new(11, 5)).unwrap();
        terminal
            .draw(|frame| {
                CharRenderer.draw(frame, frame.area(), scene, None);
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|row| {
                (0..buffer.area.width)
                    .map(|col| buffer[(col, row)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The picture, not just the scene, has to distinguish "known but not
    /// shown" from "nothing there" — the same fixture as
    /// `map::scene::a_room_whose_exit_could_not_be_drawn_says_so`, drawn.
    #[test]
    fn a_room_with_undrawable_exits_is_visibly_different_from_one_without() {
        let drawn = render(&scene_of(&[(1, "e", 2)]));
        let gapped = render(&scene_of(&[(1, "e", 2), (1, "n", 4), (2, "s", 4)]));

        assert!(
            !drawn.contains('·'),
            "a map with nothing left out is left clean:\n{drawn}"
        );
        assert!(
            gapped.contains('·'),
            "and #2's undrawable `s` puts a dot on #2:\n{gapped}"
        );
    }
}
