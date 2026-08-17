//! Drawing the map with pixels, where the terminal can take them
//! (docs/ARCHITECTURE.md §16).
//!
//! The second implementor of [`MapRenderer`], and the reason that trait
//! exists: it consumes the same [`Scene`] the character renderer does, so
//! nothing above it knows which one is running.
//!
//! ## What is pixels and what is not
//!
//! Corridors and room bodies are drawn as pixels, because that is where the
//! character grid was actually losing something — a diagonal at 45° rather
//! than a `\` leaning at 63°, and a corridor that meets the room it joins.
//!
//! The letters are *not*. A `@` or a shop's `S` needs a font, and a bitmap
//! font small enough to embed would draw them worse than the terminal's own
//! font already does, which is the one part of the character renderer that
//! was never the problem. So the map keeps the cells it covers, paints the
//! background as one image, and then writes the letters onto those cells
//! itself. Both halves land on the same grid, so they line up by
//! construction.

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};

use super::map_render::{PendingImage, STEP_X, STEP_Y, exit_tick, palette, room_style};
use super::sixel;
use crate::map::{RoomId, Scene};

/// A room is three cells wide and one tall, as it is on the character grid.
/// Keeping the same geometry is what lets the letters written afterwards
/// land on the cells the image was drawn under.
const ROOM_CELLS_W: i32 = 3;

/// Rasterises a scene, given how many pixels a cell is.
///
/// `None` when the terminal has not told us its cell size — without it an
/// image would span the wrong number of cells and shove the panes beside it
/// sideways, which is worse than not drawing one.
pub(crate) fn render(
    area: Rect,
    scene: &Scene,
    cursor: Option<RoomId>,
    cell: (u16, u16),
    pan: (i32, i32),
) -> Option<PendingImage> {
    let (cw, ch) = (cell.0 as i32, cell.1 as i32);
    if cw == 0 || ch == 0 {
        return None;
    }
    let (width, height) = (area.width as i32 * cw, area.height as i32 * ch);
    if width <= 0 || height <= 0 {
        return None;
    }

    // Index 0 is plain black, and every pixel starts there — a real,
    // transmitted colour rather than a gap left for the terminal to fill
    // in on its own. `P2=2` (`sixel::encode`) says to paint the background
    // over anything the image leaves unpainted, but "background" there is
    // the *terminal's* idea of one, and WezTerm's default theme is a pale
    // blue-grey rather than black — light enough to wash out the corridor
    // colour drawn on top of it. Owning the colour ourselves means every
    // terminal draws the same void, whatever its theme says background
    // means.
    let mut colours: Vec<(u8, u8, u8)> = vec![(0, 0, 0)];
    let mut pixels = vec![u8::MAX; (width * height) as usize];
    let mut glyphs: Vec<(u16, u16, char, Style)> = Vec::new();

    let index_of = |colour: Color, colours: &mut Vec<(u8, u8, u8)>| -> u8 {
        let Color::Rgb(r, g, b) = colour else {
            return u8::MAX;
        };
        match colours.iter().position(|c| *c == (r, g, b)) {
            Some(at) => at as u8,
            None => {
                colours.push((r, g, b));
                (colours.len() - 1) as u8
            }
        }
    };

    // Same pan as the cell renderer, in the same units: the two draw the
    // same scene and must agree about where it sits (#58).
    let origin_col = area.width as i32 / 2 - 1 + pan.0 * STEP_X;
    let origin_row = area.height as i32 / 2 + pan.1 * STEP_Y;
    // Centre of a room in pixels, which is what corridors are drawn between.
    let centre = |at: (i32, i32)| {
        let col = origin_col + at.0 * STEP_X;
        let row = origin_row + at.1 * STEP_Y;
        (col * cw + (ROOM_CELLS_W * cw) / 2, row * ch + ch / 2)
    };

    // Black, but only under the rooms — plus one room-step of margin, not
    // the whole canvas. The map is always centred on the current room, so
    // an ordinary step shifts every room's position by exactly one step;
    // a step's worth of margin is exactly enough to cover whatever the
    // previous frame drew that this one does not. Filling the *whole*
    // pane instead was tried first, to fix the ghosting `P2=2` alone
    // could not: it fixed the ghosting, but a canvas that is mostly
    // unbroken black no longer compresses the way a mostly-empty one did,
    // and paying that cost on every frame — every keystroke, whether or
    // not it had anything to do with the map — made typing sluggish.
    // Bounding the fill to where rooms actually are keeps both fixes at
    // the cost of neither.
    if let Some((min_x, max_x, min_y, max_y)) =
        scene
            .rooms
            .iter()
            .fold(None::<(i32, i32, i32, i32)>, |bounds, room| {
                Some(match bounds {
                    None => (room.at.0, room.at.0, room.at.1, room.at.1),
                    Some((min_x, max_x, min_y, max_y)) => (
                        min_x.min(room.at.0),
                        max_x.max(room.at.0),
                        min_y.min(room.at.1),
                        max_y.max(room.at.1),
                    ),
                })
            })
    {
        let px0 = ((origin_col + (min_x - 1) * STEP_X) * cw).clamp(0, width);
        let py0 = ((origin_row + (min_y - 1) * STEP_Y) * ch).clamp(0, height);
        let px1 = ((origin_col + (max_x + 1) * STEP_X + ROOM_CELLS_W) * cw).clamp(0, width);
        let py1 = ((origin_row + (max_y + 1) * STEP_Y + 1) * ch).clamp(0, height);
        for y in py0..py1 {
            for x in px0..px1 {
                pixels[(y * width + x) as usize] = 0;
            }
        }
    }

    let corridor = index_of(palette::CORRIDOR, &mut colours);
    for link in &scene.links {
        let from = centre(link.from);
        let to = centre((link.from.0 + link.step.0, link.from.1 + link.step.1));
        // A real line between the two rooms, at whatever angle they lie —
        // the diagonals the character grid could only lean at.
        line(
            &mut pixels,
            width,
            height,
            from,
            to,
            ch.max(2) / 3,
            corridor,
        );
    }

    for room in &scene.rooms {
        let (style, letter) = room_style(room);
        let fill = index_of(style.bg.unwrap_or(palette::KNOWN), &mut colours);

        let col = origin_col + room.at.0 * STEP_X;
        let row = origin_row + room.at.1 * STEP_Y;
        let (x0, y0) = (col * cw, row * ch);
        rect(
            &mut pixels,
            width,
            height,
            (x0, y0),
            (ROOM_CELLS_W * cw, ch),
            fill,
        );
        if cursor == Some(room.id) {
            // An outline rather than a fill: the room's own colour still has
            // to read through it, since that is what says what the room is.
            outline(
                &mut pixels,
                width,
                height,
                (x0, y0),
                (ROOM_CELLS_W * cw, ch),
                index_of(palette::PAPER, &mut colours),
            );
        }

        // The cells this room covers are ours now, so the letters go on
        // afterwards in the terminal's own font.
        let tick = exit_tick(room.up, room.down).unwrap_or(' ');
        let cells = [
            (col, tick),
            (col + 1, letter.unwrap_or(' ')),
            (col + 2, if room.hidden_exits { '·' } else { ' ' }),
        ];
        for (at, ch) in cells {
            if ch != ' '
                && at >= 0
                && row >= 0
                && at < area.width as i32
                && row < area.height as i32
            {
                glyphs.push((
                    area.x + at as u16,
                    area.y + row as u16,
                    ch,
                    // The room's colour again, rather than left to the
                    // image underneath. A terminal that attaches images to
                    // cells — WezTerm and kitty both — drops the image
                    // from any cell it writes a character into, so a
                    // transparent glyph punches a hole in the very room it
                    // is naming. Painting what the image painted makes the
                    // two halves indistinguishable.
                    Style::default()
                        .fg(style.fg.unwrap_or(palette::INK))
                        .bg(style.bg.unwrap_or(palette::KNOWN)),
                ));
            }
        }
    }

    Some(PendingImage {
        area,
        sixel: sixel::encode(width as usize, height as usize, &colours, &pixels),
        glyphs,
    })
}

fn put(pixels: &mut [u8], width: i32, height: i32, x: i32, y: i32, colour: u8) {
    if x >= 0 && y >= 0 && x < width && y < height {
        pixels[(y * width + x) as usize] = colour;
    }
}

fn rect(pixels: &mut [u8], width: i32, height: i32, at: (i32, i32), size: (i32, i32), colour: u8) {
    for y in at.1..at.1 + size.1 {
        for x in at.0..at.0 + size.0 {
            put(pixels, width, height, x, y, colour);
        }
    }
}

fn outline(
    pixels: &mut [u8],
    width: i32,
    height: i32,
    at: (i32, i32),
    size: (i32, i32),
    colour: u8,
) {
    for x in at.0..at.0 + size.0 {
        put(pixels, width, height, x, at.1, colour);
        put(pixels, width, height, x, at.1 + size.1 - 1, colour);
    }
    for y in at.1..at.1 + size.1 {
        put(pixels, width, height, at.0, y, colour);
        put(pixels, width, height, at.0 + size.0 - 1, y, colour);
    }
}

/// A line of the given thickness between two points, Bresenham with a
/// square brush. Thickness because a one-pixel corridor disappears on a
/// high-density display, where a room is forty pixels tall.
fn line(
    pixels: &mut [u8],
    width: i32,
    height: i32,
    from: (i32, i32),
    to: (i32, i32),
    thickness: i32,
    colour: u8,
) {
    let (mut x, mut y) = from;
    let (dx, dy) = ((to.0 - x).abs(), -(to.1 - y).abs());
    let (sx, sy) = (if x < to.0 { 1 } else { -1 }, if y < to.1 { 1 } else { -1 });
    let mut err = dx + dy;
    let half = thickness.max(1) / 2;
    loop {
        for oy in -half..=half {
            for ox in -half..=half {
                put(pixels, width, height, x + ox, y + oy, colour);
            }
        }
        if x == to.0 && y == to.1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::{Map, RoomInfo};
    use std::collections::BTreeMap;

    fn scene_of(edges: &[(i64, &str, i64)]) -> Scene {
        let mut map = Map::default();
        for id in [1, 2, 3] {
            map.observe(&RoomInfo {
                id: RoomId(id),
                name: None,
                area: Some("T".to_string()),
                exits: BTreeMap::new(),
            });
        }
        for (from, dir, to) in edges {
            map.connect(RoomId(*from), dir, RoomId(*to));
        }
        map.scene(RoomId(1), None, &[])
    }

    /// Without a cell size an image would span the wrong number of cells and
    /// shove the panes beside it sideways — worse than drawing nothing.
    #[test]
    fn no_cell_size_means_no_image() {
        let area = Rect::new(0, 0, 20, 10);
        assert!(render(area, &scene_of(&[]), None, (0, 0), (0, 0)).is_none());
    }

    #[test]
    fn the_image_covers_exactly_the_pane() {
        let area = Rect::new(3, 4, 20, 10);
        let image = render(area, &scene_of(&[]), None, (8, 16), (0, 0)).expect("an image");

        assert_eq!(image.area, area);
        assert!(
            image.sixel.contains("\"1;1;160;160"),
            "20x8 by 10x16 pixels"
        );
    }

    /// The bug this pins: the void behind the map used to be left for the
    /// terminal to fill in on its own (`P2=2`'s "background"), and WezTerm
    /// paints that as its own pale theme colour rather than black — light
    /// enough to wash out the corridor colour drawn on top of it. Every
    /// pixel the renderer does not otherwise touch has to be an explicit,
    /// transmitted black, not a gap.
    #[test]
    fn the_void_behind_the_map_is_an_explicit_black_not_left_to_the_terminal() {
        let image = render(
            Rect::new(0, 0, 20, 10),
            &scene_of(&[]),
            None,
            (8, 16),
            (0, 0),
        )
        .expect("an image");

        assert!(
            image.sixel.contains("#0;2;0;0;0"),
            "index 0 should be transmitted as black: {}",
            image.sixel
        );
    }

    /// The bug painting the *whole* canvas black caused: on a pane much
    /// bigger than the explored map — the ordinary case, since the pane is
    /// sized to the terminal and the map to what has been walked — a full
    /// fill turns a mostly-empty image into a mostly-solid one, which
    /// costs real bytes and real time to encode on every single frame.
    /// One room's picture on a huge pane has to stay cheap regardless of
    /// how big the pane is.
    #[test]
    fn the_payload_stays_small_on_a_pane_much_bigger_than_the_map() {
        let image = render(
            Rect::new(0, 0, 200, 150),
            &scene_of(&[]),
            None,
            (8, 16),
            (0, 0),
        )
        .expect("an image");

        assert!(
            image.sixel.len() < 4_000,
            "a single room on a 1600x2400 canvas should stay a small fraction of a full fill: {} bytes",
            image.sixel.len()
        );
    }

    /// The point of the whole renderer: a diagonal corridor is drawn at the
    /// angle the two rooms actually lie at, rather than leaning at whatever
    /// a `\` in a tall cell happens to be.
    #[test]
    fn a_diagonal_corridor_is_a_real_line() {
        let plain = render(
            Rect::new(0, 0, 20, 10),
            &scene_of(&[(1, "e", 2)]),
            None,
            (8, 16),
            (0, 0),
        )
        .expect("an image");
        let diagonal = render(
            Rect::new(0, 0, 20, 10),
            &scene_of(&[(1, "se", 2)]),
            None,
            (8, 16),
            (0, 0),
        )
        .expect("an image");

        assert_ne!(plain.sixel, diagonal.sixel);
    }

    /// Letters are the terminal's job, on the cells the image covers.
    #[test]
    fn the_character_is_written_as_a_glyph_not_drawn_as_pixels() {
        let image = render(
            Rect::new(0, 0, 20, 10),
            &scene_of(&[]),
            None,
            (8, 16),
            (0, 0),
        )
        .expect("an image");

        assert!(
            image.glyphs.iter().any(|(_, _, ch, _)| *ch == '@'),
            "{:?}",
            image.glyphs
        );
    }

    /// A glyph carries the room's own colour as its background, because a
    /// terminal that attaches images to cells drops the image from any cell
    /// it writes a character into. Left transparent, the letter punches a
    /// hole in the room it is naming.
    #[test]
    fn a_glyph_repaints_the_room_colour_it_covers() {
        let image = render(
            Rect::new(0, 0, 20, 10),
            &scene_of(&[]),
            None,
            (8, 16),
            (0, 0),
        )
        .expect("an image");

        let (_, _, _, style) = image
            .glyphs
            .iter()
            .find(|(_, _, ch, _)| *ch == '@')
            .expect("the here-room's glyph");
        assert_eq!(style.bg, Some(palette::HERE));
    }

    /// A glyph must sit inside the pane it belongs to, or it would be
    /// written over whatever is beside the map.
    #[test]
    fn glyphs_stay_inside_the_pane() {
        let area = Rect::new(5, 2, 20, 10);
        let image = render(
            area,
            &scene_of(&[(1, "e", 2), (2, "e", 3)]),
            None,
            (8, 16),
            (0, 0),
        )
        .expect("an image");

        for (x, y, ch, _) in &image.glyphs {
            assert!(
                *x >= area.x && *x < area.right() && *y >= area.y && *y < area.bottom(),
                "`{ch}` at {x},{y} is outside {area:?}"
            );
        }
    }
}
