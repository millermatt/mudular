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

use crate::map::{PlacedRoom, RoomId, RoomRole, Scene};

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
        pan: (i32, i32),
    ) -> Option<PendingImage>;
}

/// A room occupies three columns and a corridor the gap beside it, so a
/// grid step is four columns across and two rows down. In pixels that is
/// close to square on a typical cell, which is what keeps a diagonal
/// looking like a diagonal.
pub(super) const STEP_X: i32 = 4;
pub(super) const STEP_Y: i32 = 2;

/// Which sides of the pane have rooms beyond them that are not drawn
/// (#55).
///
/// A column showing four rooms looks the same whether the area has four
/// rooms or forty, which is the same dishonesty #47 fixed for a room whose
/// exits could not be drawn: the map should not look complete when it is
/// showing part of something.
///
/// A room counts as beyond only when *no* part of it is on the pane. One
/// clipped to a single column is still visible, and marking its side would
/// point at something the player can already see.
///
/// Deliberately says nothing about "the area ends here". Absence of a mark
/// means no known rooms that way — whether because the area stops or
/// because nobody has walked there yet is a distinction the map cannot
/// honestly draw, since an unexplored exit and a wall look identical until
/// someone tries.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Beyond {
    pub left: bool,
    pub right: bool,
    pub up: bool,
    pub down: bool,
}

impl Beyond {
    pub fn any(&self) -> bool {
        self.left || self.right || self.up || self.down
    }
}

pub(super) fn rooms_beyond(grid: Rect, scene: &Scene, pan: (i32, i32)) -> Beyond {
    let width = grid.width as i32;
    let height = grid.height as i32;
    let mut beyond = Beyond::default();
    for room in &scene.rooms {
        // The same arithmetic the renderer uses, including the three
        // columns a room occupies — a room is off to the left only when
        // its rightmost column is.
        let col = width / 2 - 1 + (pan.0 + room.at.0) * STEP_X;
        let row = height / 2 + (pan.1 + room.at.1) * STEP_Y;
        beyond.left |= col + 2 < 0;
        beyond.right |= col >= width;
        beyond.up |= row < 0;
        beyond.down |= row >= height;
    }
    beyond
}

/// Whether a room at scene coordinate `at` is drawn in `grid` with enough
/// around it to be worth not re-centring for (#58).
///
/// The arithmetic mirrors `draw` exactly — same origin, same `STEP_*`, and
/// the room's own cell is `col + 1`, the middle of its three slots — so
/// this cannot drift from what the renderer actually puts on screen
/// without a test noticing.
///
/// The margin is one room step on every side, and it is the point rather
/// than a safety fudge: centring exists so a player can see where they can
/// go next, and a character pinned against the pane edge has lost that
/// even though their letter is technically visible. A pane too small to
/// hold the margin never reports anything as visible, so it re-centres on
/// every switch — the old behaviour, which is the right degradation.
pub(super) fn room_is_visible(grid: Rect, at: (i32, i32)) -> bool {
    let width = grid.width as i32;
    let height = grid.height as i32;
    let col = width / 2 - 1 + at.0 * STEP_X + 1;
    let row = height / 2 + at.1 * STEP_Y;
    col - STEP_X >= 0 && col + STEP_X < width && row - STEP_Y >= 0 && row + STEP_Y < height
}

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
    /// A mark that is not one of `MARK_SUGGESTIONS` — a player's own
    /// "sewer" or "stable" — outside the Okabe-Ito set on purpose, so it
    /// can never land on the same colour a curated label uses and read as
    /// one at a glance.
    pub const OTHER: Color = Color::Rgb(0x8A, 0x8D, 0x91); // neutral grey
    /// Another character on this world. Violet because nothing else in
    /// the pane is: `HERE` is the sky blue that means *you*, and telling
    /// "me" from "one of mine" at a glance is the whole point of drawing
    /// them at all.
    pub const PARTY: Color = Color::Rgb(0xA7, 0x8B, 0xFA); // violet
    pub const INK: Color = Color::Rgb(0x0B, 0x0E, 0x11); // near-black
    /// The status bar's ground. Grey rather than the near-white `PAPER`:
    /// the bar is on screen all the time, and a full-width white strip at
    /// the bottom of a dark terminal pulls the eye away from the game it
    /// is supposed to be reporting on.
    pub const CHROME: Color = Color::Rgb(0x9C, 0xA3, 0xAF); // slate grey
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
/// map, in every session, without anyone having to say so — `MARK_SUGGESTIONS`
/// gets a colour each, by position, so the common ones are all told apart.
///
/// Anything else — a label the client did not offer — gets `palette::OTHER`
/// rather than a colour of its own. A hashed colour was tried first, and
/// the hash could land on the *same* colour a curated label already uses:
/// a player's own "stable" could read as an actual shop at a glance, purely
/// by the coincidence of where its hash fell. One shared colour for
/// "something the player wrote, not a category the client knows" cannot
/// collide with a curated label's meaning, which is the property that
/// matters — it can still collide with *another* custom label sharing a
/// first letter, but the full label is always a look at `Map::describe`
/// away, and that ambiguity already existed for two curated labels sharing
/// a hue (`labels_sharing_a_colour_do_not_share_a_letter`).
pub(super) fn marked_style(label: &str) -> (Style, Option<char>) {
    let label = label.trim();
    let folded = label.to_ascii_lowercase();
    let background = match crate::app::MARK_SUGGESTIONS
        .iter()
        .position(|known| *known == folded)
    {
        Some(slot) => palette::LABELS[slot % palette::LABELS.len()],
        None => palette::OTHER,
    };
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

/// What a room's first cell says about exits that leave the flat grid
/// (§16), or `None` when there are none.
///
/// Real arrows rather than `^` and `v`, which is what this was: `↕`
/// already came from the Arrows block for the both-case, so ASCII for
/// the other two was inconsistent as well as harder to read — `v` is a
/// letter, and the cell beside it draws letters (`W` for water, `S` for
/// a shop, a character's initial), so a `v` tick read as text. `↑` and
/// `↓` cannot be mistaken for one, and are in at least as many fonts as
/// the `↕` this file already relies on.
pub(super) fn exit_tick(up: bool, down: bool) -> Option<char> {
    match (up, down) {
        (true, true) => Some('↕'),
        (true, false) => Some('↑'),
        (false, true) => Some('↓'),
        (false, false) => None,
    }
}

/// Another character standing in a room (§16): their initial, on a colour
/// that is neither yours nor any label's.
///
/// The initial is a stand-in, not a decision — a per-profile marker would
/// slot in here and nowhere else, beside the `color:` a profile already
/// sets.
pub(super) fn party_style(name: &str) -> (Style, Option<char>) {
    (
        Style::default()
            .bg(palette::PARTY)
            .fg(ink_for(palette::PARTY))
            .add_modifier(Modifier::BOLD),
        name.trim()
            .chars()
            .next()
            .map(|first| first.to_ascii_uppercase()),
    )
}

/// What a placed room draws as — every rule about which fact wins, in one
/// place, so the two renderers cannot drift apart about it.
///
/// Where you are outranks everything: a map you cannot find yourself on is
/// not a map. Your corpse comes next, because there is only ever one and
/// it is the thing a corpse run is steering toward. Then whoever else is
/// standing there, which changes minute to minute and is what a
/// multiboxer is actually watching. A label loses to all three: it is
/// still true when they have moved on, and `Map::describe` still has it.
pub(super) fn room_style(room: &PlacedRoom) -> (Style, Option<char>) {
    match room.role {
        RoomRole::Here | RoomRole::Corpse => role_style(room.role),
        RoomRole::Known => match (room.party.first(), room.mark.as_deref()) {
            (Some(who), _) => party_style(who),
            (None, Some(mark)) => marked_style(mark),
            (None, None) => role_style(room.role),
        },
    }
}

/// What every colour and letter the map draws actually means, in the same
/// styles the map itself uses — a swatch is only honest if it is the exact
/// style a room would be drawn in, not a description of one (§16).
///
/// Static rather than scene-derived: it lists what `/mark` offers and the
/// two roles every scene can show, not what happens to be on screen right
/// now, so it reads the same whether the pane is empty or full.
pub(super) fn legend(width: u16) -> Vec<Line<'static>> {
    // The third field groups entries that answer the same question, so a
    // row break can be forced between groups: who is on the map, what a
    // room's exits do, what a room is for. Packed as one long run they
    // interleave — the three arrows would split across a line boundary —
    // and a legend is read by scanning, which wants the answer to one
    // question in one place.
    let mut entries: Vec<(Style, Option<char>, &str, u8)> = Vec::new();
    let (here_style, here_letter) = role_style(RoomRole::Here);
    entries.push((here_style, here_letter, "you", 0));
    let (corpse_style, corpse_letter) = role_style(RoomRole::Corpse);
    entries.push((corpse_style, corpse_letter, "corpse", 0));
    let (party, party_letter) = party_style("party");
    entries.push((party, party_letter, "another char", 0));

    // The two slots either side of the room's own letter. Drawn on plain
    // room ground rather than a colour of their own, because that is
    // exactly how they appear on the map — they say something about the
    // room's *exits*, not about what kind of place it is, and giving them
    // a hue would imply otherwise.
    let (ground, _) = role_style(RoomRole::Known);
    for (up, down, meaning) in [
        (true, false, "up"),
        (false, true, "down"),
        (true, true, "up+down"),
    ] {
        entries.push((ground, exit_tick(up, down), meaning, 1));
    }
    entries.push((ground, Some('·'), "more exits", 1));

    for label in crate::app::MARK_SUGGESTIONS {
        let (style, letter) = marked_style(label);
        entries.push((style, letter, label, 2));
    }
    // Whatever a custom `/mark` actually says is never `?` — this row is
    // the one place that glyph is honest, standing in for "some label of
    // your own" rather than any real room's first letter.
    entries.push((
        Style::default()
            .bg(palette::OTHER)
            .fg(ink_for(palette::OTHER))
            .add_modifier(Modifier::BOLD),
        Some('?'),
        "other",
        2,
    ));

    // The edge mark (#55). Its own group, because it answers neither "who
    // is on the map" nor "what is this room" but "is there more of it" —
    // and it is drawn the way the border is drawn, since that is where the
    // player will see it rather than on any room.
    entries.push((
        Style::new().fg(palette::PAPER).add_modifier(Modifier::BOLD),
        Some('>'),
        "more map",
        3,
    ));

    // Broken at entry boundaries rather than left to `Paragraph`'s word
    // wrap, which knows nothing about which words belong together: it
    // would end a line on a swatch and start the next with the word that
    // swatch names, so `S` sat alone above `shop`. An entry is one thing
    // and now wraps as one, whatever the column is doing.
    const GAP: &str = "  ";
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    let mut group = None;
    for (style, letter, label, entry_group) in entries {
        // A swatch, a space, and the word: the swatch is drawn in the
        // style the map would draw it, the word beside it dim so the
        // colours stay the loudest thing in the row.
        let entry = letter.map_or(0, |_| 1) + 1 + label.chars().count();
        let new_group = group
            .replace(entry_group)
            .is_some_and(|was| was != entry_group);
        let wrapping =
            !spans.is_empty() && (new_group || used + GAP.len() + entry > width.max(1) as usize);
        if wrapping {
            lines.push(Line::from(std::mem::take(&mut spans)));
            used = 0;
        } else if !spans.is_empty() {
            spans.push(Span::raw(GAP));
            used += GAP.len();
        }
        if let Some(letter) = letter {
            spans.push(Span::styled(letter.to_string(), style));
        }
        spans.push(Span::styled(
            format!(" {label}"),
            Style::default().add_modifier(Modifier::DIM),
        ));
        used += entry;
    }
    if !spans.is_empty() {
        lines.push(Line::from(spans));
    }
    lines
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
        pan: (i32, i32),
    ) -> Option<PendingImage> {
        let width = area.width as i32;
        let height = area.height as i32;
        // The current room sits in the middle, so walking scrolls the world
        // past a fixed marker rather than sliding a dot at an edge it then
        // falls off (§16).
        // `pan` moves the whole picture, character included, in room
        // steps — the character is the scene's (0,0), so panning is the
        // only way anything other than them can sit at the middle (#58).
        let origin_col = width / 2 - 1 + pan.0 * STEP_X;
        let origin_row = height / 2 + pan.1 * STEP_Y;

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
            let (mut style, letter) = room_style(room);
            // Three slots: a tick for exits off the grid, the room's own
            // mark, and ground. All on one fill, so none of it crowds.
            let tick = exit_tick(room.up, room.down).unwrap_or(' ');
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

    /// The property that makes a fixed colour usable at all: `shop` is the
    /// same colour on every map, in every session, on every launch.
    #[test]
    fn a_curated_label_keeps_its_colour_between_runs() {
        assert_eq!(marked_style("shop").0.bg, Some(palette::LABELS[0]));
        assert_eq!(marked_style("water").0.bg, Some(palette::LABELS[1]));
    }

    /// The bug this pins: a hashed colour for anything outside
    /// `MARK_SUGGESTIONS` was tried first, and the hash could land on the
    /// *same* colour a curated label already uses — a player's own
    /// "smith" could read as an actual `shop` at a glance, purely by where
    /// its hash fell, sharing both the colour and (since both start with
    /// `s`) the letter. `palette::OTHER` cannot collide with a curated
    /// label's meaning no matter what the player types.
    #[test]
    fn an_uncurated_label_never_collides_with_a_curated_one() {
        assert_eq!(marked_style("smith").0.bg, Some(palette::OTHER));
        assert_ne!(marked_style("smith").0.bg, marked_style("shop").0.bg);
        // Any label at all — not just one that happens to share a letter.
        for custom in ["grocer", "stable", "sewer", "well"] {
            assert_eq!(
                marked_style(custom).0.bg,
                Some(palette::OTHER),
                "{custom} is not offered, so it should not borrow a curated colour"
            );
        }
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
        for bg in palette::LABELS.iter().copied().chain([
            palette::HERE,
            palette::CORPSE,
            palette::KNOWN,
            palette::OTHER,
            palette::PARTY,
        ]) {
            assert!(
                contrast(bg) > 60.0,
                "{bg:?} and its ink are too close to read"
            );
        }
    }

    /// The bug this pins: the legend was one long `Line` left to
    /// `Paragraph`'s word wrap, which knows nothing about which words
    /// belong together — it ended lines on a swatch and began the next
    /// with the word that swatch named, so `S` sat alone above `shop`.
    /// An entry is one thing; a group of them answers one question.
    #[test]
    fn the_legend_never_splits_an_entry_or_mixes_two_groups() {
        let who = ["you", "corpse", "another char"];
        let exits = ["up", "down", "up+down", "more exits"];

        for width in [18u16, 22, 30, 42, 60, 200] {
            for line in legend(width) {
                let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                assert!(
                    line.width() <= width.max(1) as usize,
                    "`{text}` overruns {width}"
                );

                let mut groups = std::collections::HashSet::new();
                for entry in text.trim_end().split("  ") {
                    let (glyph, label) = entry
                        .split_once(' ')
                        .unwrap_or_else(|| panic!("`{entry}` lost its swatch or its word: {text}"));
                    assert_eq!(
                        glyph.chars().count(),
                        1,
                        "`{entry}` should be one swatch and its word: {text}"
                    );
                    assert!(!label.is_empty(), "`{entry}` names nothing: {text}");
                    groups.insert(match label {
                        l if who.contains(&l) => 0,
                        l if exits.contains(&l) => 1,
                        _ => 2,
                    });
                }
                assert_eq!(
                    groups.len(),
                    1,
                    "one row should answer one question, not {}: {text}",
                    groups.len()
                );
            }
        }
    }

    /// The tick was written out twice, once per renderer, and the legend
    /// named its glyphs a third time — three places to keep in step. They
    /// read one function now, so a changed arrow cannot leave the legend
    /// describing the old one.
    #[test]
    fn the_legend_names_the_arrows_the_map_actually_draws() {
        let legend: String = legend(u16::MAX).iter().map(ToString::to_string).collect();
        for (up, down) in [(true, false), (false, true), (true, true)] {
            let drawn = exit_tick(up, down).expect("an exit that leaves the grid has a tick");
            assert!(
                legend.contains(drawn),
                "the map draws `{drawn}` for up={up} down={down}, so the legend must name it: {legend}"
            );
        }
        assert_eq!(exit_tick(false, false), None, "a flat room has no tick");
    }

    /// `v` is a letter, and the cell beside the tick draws letters — a
    /// mark's initial, a character's. An arrow cannot be read as one.
    #[test]
    fn the_exit_ticks_are_not_letters() {
        for (up, down) in [(true, false), (false, true), (true, true)] {
            let tick = exit_tick(up, down).unwrap();
            assert!(
                !tick.is_ascii_alphabetic(),
                "`{tick}` would read as text beside a room's letter"
            );
        }
    }

    /// The precedence both renderers now share, stated once. Where you
    /// are and where your corpse is are singular and are what you steer
    /// by; who else is standing somewhere changes minute to minute and is
    /// what a multiboxer watches; a label is still true tomorrow.
    #[test]
    fn a_party_marker_beats_a_label_but_not_you_or_your_corpse() {
        let placed = |role, party: &[&str], mark: Option<&str>| PlacedRoom {
            id: RoomId(1),
            at: (0, 0),
            role,
            up: false,
            down: false,
            mark: mark.map(str::to_string),
            party: party.iter().map(|name| name.to_string()).collect(),
            hidden_exits: false,
        };

        assert_eq!(
            room_style(&placed(RoomRole::Known, &["saihtam"], Some("shop"))).1,
            Some('S'),
            "an ally's initial, not the shop's"
        );
        assert_eq!(
            room_style(&placed(RoomRole::Known, &["saihtam"], Some("shop")))
                .0
                .bg,
            Some(palette::PARTY),
            "and unmistakably not a shop's colour"
        );
        assert_eq!(
            room_style(&placed(RoomRole::Here, &["saihtam"], None)).0.bg,
            Some(palette::HERE),
            "standing together still shows you where *you* are"
        );
        assert_eq!(
            room_style(&placed(RoomRole::Corpse, &["saihtam"], None))
                .0
                .bg,
            Some(palette::CORPSE),
            "and an ally passing your corpse does not hide it"
        );
        assert_eq!(
            room_style(&placed(RoomRole::Known, &[], Some("shop"))).0.bg,
            marked_style("shop").0.bg,
            "with nobody there, a label is drawn as it always was"
        );
    }

    /// Violet is not any label's colour and not yours, so "one of mine" is
    /// never mistaken for "a shop" or for "me".
    #[test]
    fn the_party_colour_is_its_own() {
        for label in crate::app::MARK_SUGGESTIONS {
            assert_ne!(marked_style(label).0.bg, Some(palette::PARTY), "{label}");
        }
        assert_ne!(palette::PARTY, palette::HERE);
        assert_ne!(palette::PARTY, palette::OTHER);
        assert_ne!(palette::PARTY, palette::CORPSE);
    }

    /// A label may never impersonate the colours that already mean
    /// something else in this pane.
    #[test]
    fn no_label_colour_collides_with_a_role_colour() {
        for label in crate::app::MARK_SUGGESTIONS
            .iter()
            .chain(["anything"].iter())
        {
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
        map.scene(RoomId(1), None, &[])
    }

    fn render(scene: &Scene) -> String {
        render_panned(scene, (0, 0))
    }

    fn render_panned(scene: &Scene, pan: (i32, i32)) -> String {
        let mut terminal = Terminal::new(TestBackend::new(11, 5)).unwrap();
        terminal
            .draw(|frame| {
                CharRenderer.draw(frame, frame.area(), scene, None, pan);
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

    /// #55. A column showing four rooms looked the same whether the area
    /// had four rooms or forty. The sides with something beyond them are
    /// exactly the ones the renderer was already discarding.
    #[test]
    fn a_room_off_the_pane_marks_the_side_it_is_off() {
        let scene = scene_of(&[(1, "e", 2)]);
        // Wide enough for both rooms: room 2 sits one step east of centre.
        let roomy = rooms_beyond(Rect::new(0, 0, 40, 20), &scene, (0, 0));
        assert_eq!(roomy, Beyond::default(), "both rooms fit, so no marks");

        // Eleven columns puts room 1 at columns 4-6 and room 2 at 8-10,
        // both on. Panning one step east slides room 1 to 8-10 and room 2
        // past the right-hand edge.
        let panned = rooms_beyond(Rect::new(0, 0, 11, 5), &scene, (1, 0));
        assert!(panned.right, "room 2 is off to the east");
        assert!(!panned.left && !panned.up && !panned.down);
    }

    /// A room clipped to part of its width is still something the player
    /// can see, so marking its side would point at what is already on
    /// screen. Only a room with no cell at all on the pane counts.
    #[test]
    fn a_partly_drawn_room_is_not_beyond_the_edge() {
        let scene = scene_of(&[(1, "e", 2)]);
        // 9 columns: centre at 3, room 2 at columns 7, 8, 9 — the last of
        // those is off, the first two are not.
        let clipped = rooms_beyond(Rect::new(0, 0, 9, 5), &scene, (0, 0));
        assert_eq!(
            clipped,
            Beyond::default(),
            "part of the room is drawn, so nothing is hidden beyond the edge"
        );
    }

    /// Every side, so the marks cannot be wired up transposed — the bug
    /// this test exists to catch is `up` lighting the bottom border.
    #[test]
    fn each_direction_marks_its_own_side() {
        for (dir, expected) in [
            (
                "e",
                Beyond {
                    right: true,
                    ..Beyond::default()
                },
            ),
            (
                "w",
                Beyond {
                    left: true,
                    ..Beyond::default()
                },
            ),
            (
                "s",
                Beyond {
                    down: true,
                    ..Beyond::default()
                },
            ),
            (
                "n",
                Beyond {
                    up: true,
                    ..Beyond::default()
                },
            ),
        ] {
            let scene = scene_of(&[(1, dir, 2)]);
            // Small enough that one step in any direction is off the pane.
            assert_eq!(
                rooms_beyond(Rect::new(0, 0, 5, 3), &scene, (0, 0)),
                expected,
                "going {dir} marked the wrong side"
            );
        }
    }

    /// The pan is only real if it moves the picture (#58). Everything else
    /// about #58 is arithmetic on `AppState`; this is the one test that
    /// says the arithmetic reaches the screen.
    #[test]
    fn panning_moves_the_whole_picture_by_whole_room_steps() {
        let scene = scene_of(&[(1, "e", 2)]);
        let centred = render_panned(&scene, (0, 0));
        let panned = render_panned(&scene, (1, 0));

        assert_ne!(centred, panned, "a pan has to change what is drawn");
        // One room step east is `STEP_X` columns, and every row moves
        // together — the picture is shifted, not re-laid-out.
        for (before, after) in centred.lines().zip(panned.lines()) {
            let shifted: String = " ".repeat(STEP_X as usize) + before;
            assert!(
                shifted.starts_with(after.trim_end()) || after.trim().is_empty(),
                "row moved by something other than one whole room step:\n\
                 before: {before:?}\nafter:  {after:?}"
            );
        }
    }

    /// Panning far enough takes the character off the pane entirely. The
    /// renderer simply clips — deciding that this must not happen is
    /// `AppState::update_map_pan`'s job, and it has its own tests.
    #[test]
    fn a_pan_beyond_the_pane_draws_nothing_rather_than_wrapping() {
        let drawn = render_panned(&scene_of(&[(1, "e", 2)]), (99, 0));
        assert!(
            drawn.trim().is_empty(),
            "expected an empty pane, got:\n{drawn}"
        );
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
