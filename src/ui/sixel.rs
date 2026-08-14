//! Encoding an indexed bitmap as Sixel, for terminals that can draw one
//! (docs/ARCHITECTURE.md §16).
//!
//! Sixel is a palette-indexed format from the DEC VT240, and that is what
//! makes it a good fit here rather than a burden: the map already draws
//! from a fixed palette of about a dozen colours, so the hard part of
//! general Sixel encoding — reducing an arbitrary image to 256 colours —
//! simply does not arise. What is left is the band encoder.
//!
//! Written out rather than taken from a crate because §15 promises a static
//! binary with no C dependencies, which rules out the `libsixel` bindings,
//! and because encoding *our* case is small enough that a dependency would
//! be the larger commitment.
//!
//! ## The format
//!
//! A sixel is six vertical pixels in one column, packed into one character:
//! bit 0 is the top row, bit 5 the bottom, and the value is offset by `?`
//! (0x3F) so every byte is printable. An image is therefore drawn in bands
//! six pixels tall. Within a band each colour is written as its own pass —
//! select it, draw the columns where it appears, carriage-return to the
//! start of the band — and `-` moves to the next band.

/// Wraps a bitmap in the escape sequence a terminal will draw.
///
/// `pixels` is one palette index per pixel, row-major, `width * height` of
/// them. Indices outside `palette` are painted as the terminal's background,
/// which is what lets a room that leaves the scene — the map recentres on
/// every move — actually disappear.
pub(crate) fn encode(
    width: usize,
    height: usize,
    palette: &[(u8, u8, u8)],
    pixels: &[u8],
) -> String {
    debug_assert_eq!(pixels.len(), width * height, "one index per pixel");
    // `ESC P q` opens the data; the `0;2;0` selects the ordinary pixel
    // aspect ratio and paints unset pixels as the background rather than
    // leaving them transparent.
    //
    // Transparent (`P2=1`) sounds like the right choice for a canvas that
    // is mostly empty, and it was tried first — but "transparent" in Sixel
    // means the terminal leaves whatever was already there alone, not that
    // it paints nothing. Every frame draws the same fixed-size canvas, so a
    // room that fell out of the picture (the map is always centred on the
    // player, so nearly every move shifts every room) left its old pixels
    // on screen forever: rooms the player had long since walked away from
    // kept accumulating, session over session, because nothing ever told
    // the terminal to erase them. Painting the background instead means
    // every frame is a real overwrite of the whole canvas, at the cost of
    // the pane's own background no longer showing through the gaps — a
    // black rectangle is a small enough price for a map that stops lying.
    let mut out = String::from("\x1bP0;2;0q");
    // Raster attributes: square pixels, and the size up front so a terminal
    // can reserve the room before it starts drawing.
    out.push_str(&format!("\"1;1;{width};{height}"));

    for (index, (r, g, b)) in palette.iter().enumerate() {
        // Sixel colour components are percentages, not bytes. Rounding
        // rather than truncating keeps `0xFF` at 100 instead of 99.
        let pc = |v: u8| (v as u32 * 100 + 127) / 255;
        out.push_str(&format!("#{index};2;{};{};{}", pc(*r), pc(*g), pc(*b)));
    }

    for band in 0..height.div_ceil(6) {
        let top = band * 6;
        let rows = 6.min(height - top);

        // Only the colours actually in this band get a pass; on a map most
        // bands are one or two colours and the rest would be empty passes.
        let mut present: Vec<u8> = Vec::new();
        for y in top..top + rows {
            for x in 0..width {
                let index = pixels[y * width + x];
                if (index as usize) < palette.len() && !present.contains(&index) {
                    present.push(index);
                }
            }
        }
        present.sort_unstable();

        for (pass, colour) in present.iter().enumerate() {
            if pass > 0 {
                // Back to the start of the band for the next colour.
                out.push('$');
            }
            out.push_str(&format!("#{colour}"));

            // Run-length encoded: a map is mostly flat colour, and without
            // this a single frame is tens of kilobytes of identical bytes.
            let mut runs: Vec<(char, usize)> = Vec::new();
            for x in 0..width {
                let mut bits = 0u8;
                for (bit, y) in (top..top + rows).enumerate() {
                    if pixels[y * width + x] == *colour {
                        bits |= 1 << bit;
                    }
                }
                let ch = (b'?' + bits) as char;
                match runs.last_mut() {
                    Some((previous, len)) if *previous == ch => *len += 1,
                    _ => runs.push((ch, 1)),
                }
            }
            // An empty run at the *end* draws nothing, so saying it costs
            // bytes for no picture. Empty runs anywhere else have to be
            // written: `?` is how the cursor crosses a gap, and dropping
            // one slides everything after it a column to the left.
            if runs.last().is_some_and(|(ch, _)| *ch == '?') {
                runs.pop();
            }
            for (ch, len) in runs {
                push_run(&mut out, ch, len);
            }
        }
        out.push('-');
    }

    out.push_str("\x1b\\");
    out
}

/// `!n` repeats the next character `n` times. Only worth it past three,
/// since `!4` plus the character is already four bytes.
fn push_run(out: &mut String, ch: char, len: usize) {
    match len > 3 {
        true => out.push_str(&format!("!{len}{ch}")),
        false => (0..len).for_each(|_| out.push(ch)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads our own output back to pixels, so the tests check what a
    /// terminal would actually draw rather than the bytes we happened to
    /// write. Only the subset `encode` emits.
    fn decode(sixel: &str, width: usize, height: usize) -> Vec<Option<u8>> {
        let mut pixels = vec![None; width * height];
        let body = sixel
            .strip_prefix("\x1bP0;2;0q")
            .expect("opens with a DCS")
            .strip_suffix("\x1b\\")
            .expect("closes with ST");
        let body = &body[body.find('#').expect("a palette")..];

        let (mut x, mut band, mut colour) = (0usize, 0usize, 0u8);
        let mut chars = body.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '#' => {
                    let mut n = String::new();
                    while chars.peek().is_some_and(char::is_ascii_digit) {
                        n.push(chars.next().unwrap());
                    }
                    // A palette definition (`#n;2;r;g;b`) rather than a
                    // selection: skip its parameters.
                    if chars.peek() == Some(&';') {
                        for _ in 0..4 {
                            chars.next();
                            while chars.peek().is_some_and(char::is_ascii_digit) {
                                chars.next();
                            }
                        }
                    } else {
                        colour = n.parse().unwrap();
                        x = 0;
                    }
                }
                '$' => x = 0,
                '-' => {
                    band += 1;
                    x = 0;
                }
                '!' => {
                    let mut n = String::new();
                    while chars.peek().is_some_and(char::is_ascii_digit) {
                        n.push(chars.next().unwrap());
                    }
                    let count: usize = n.parse().unwrap();
                    let ch = chars.next().unwrap();
                    for _ in 0..count {
                        paint(&mut pixels, width, height, x, band, colour, ch);
                        x += 1;
                    }
                }
                '?'..='~' => {
                    paint(&mut pixels, width, height, x, band, colour, ch);
                    x += 1;
                }
                _ => {}
            }
        }
        pixels
    }

    fn paint(
        pixels: &mut [Option<u8>],
        width: usize,
        height: usize,
        x: usize,
        band: usize,
        colour: u8,
        ch: char,
    ) {
        let bits = ch as u8 - b'?';
        for bit in 0..6 {
            if bits & (1 << bit) == 0 {
                continue;
            }
            let y = band * 6 + bit;
            if x < width && y < height {
                pixels[y * width + x] = Some(colour);
            }
        }
    }

    const PALETTE: [(u8, u8, u8); 3] = [(255, 0, 0), (0, 255, 0), (0, 0, 255)];

    #[test]
    fn a_bitmap_survives_the_round_trip() {
        // Deliberately taller than one six-pixel band and not a multiple of
        // it, since the last band is the one an off-by-one lands in.
        let (w, h) = (5, 8);
        let pixels: Vec<u8> = (0..w * h).map(|i| (i % 3) as u8).collect();

        let decoded = decode(&encode(w, h, &PALETTE, &pixels), w, h);

        let expected: Vec<Option<u8>> = pixels.iter().map(|i| Some(*i)).collect();
        assert_eq!(decoded, expected);
    }

    /// An index the palette does not have gets no explicit paint command in
    /// the raster — `P2=2` is what then tells the terminal to fill those
    /// positions with the background rather than leave them alone.
    #[test]
    fn an_index_outside_the_palette_is_never_explicitly_painted() {
        let pixels = vec![9u8; 4 * 6];

        let decoded = decode(&encode(4, 6, &PALETTE, &pixels), 4, 6);

        assert!(decoded.iter().all(Option::is_none));
    }

    /// The bug this pins: `P2=1` ("transparent") tells the terminal to
    /// leave whatever was already on screen alone at every pixel this
    /// image does not paint. Since the map recentres on the player every
    /// move, almost every frame leaves *some* pixel unpainted that a
    /// previous frame did paint — and with `P2=1` those old pixels never
    /// go away, so rooms the player walked away from kept accumulating on
    /// screen for the rest of the session. `P2=2` fills them with the
    /// background instead, so every frame is a genuine overwrite.
    #[test]
    fn unpainted_pixels_are_told_to_erase_to_background_not_stay_transparent() {
        let encoded = encode(1, 1, &PALETTE, &[0]);
        assert!(
            encoded.starts_with("\x1bP0;2;0q"),
            "P2 must not be 1 (transparent): {encoded:?}"
        );
    }

    /// A map is mostly flat colour, and without run-length encoding one
    /// frame is tens of kilobytes of identical bytes.
    #[test]
    fn flat_colour_is_run_length_encoded() {
        let flat = encode(400, 6, &PALETTE, &vec![0u8; 400 * 6]);

        assert!(flat.contains("!400"), "a 400-wide run should be one token");
        assert!(flat.len() < 200, "but it was {} bytes", flat.len());
    }

    /// The colour components are percentages, so full brightness has to
    /// reach 100 rather than the 99 truncation would give.
    #[test]
    fn full_brightness_is_a_hundred_percent() {
        let encoded = encode(1, 1, &[(255, 255, 255)], &[0]);

        assert!(encoded.contains("#0;2;100;100;100"), "{encoded}");
    }

    #[test]
    fn the_size_is_declared_before_the_pixels() {
        let encoded = encode(7, 13, &PALETTE, &[0u8; 7 * 13]);

        assert!(encoded.contains("\"1;1;7;13"), "{encoded}");
    }
}
