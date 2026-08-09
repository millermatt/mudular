//! What a pane keeps, rather than what it draws (docs/ARCHITECTURE.md §8).
//!
//! A pane used to hold `VecDeque<String>`: by the time a line was stored,
//! its arrival time and its provenance had been flattened into the text —
//! a channel timestamp prepended, a client warning marked only by the `**`
//! someone remembered to type. Anything wanting to tell the client's own
//! voice from the server's, or to know when a line arrived, had to parse
//! its way back out of the rendered string.
//!
//! So the buffer stores what it knows and the renderer composes: `at` is
//! the timestamp a channel pane formats, `origin` is why a line is there.

use chrono::{DateTime, Local};

/// One retained line. `text` is the line as it should read — server ANSI
/// with `highlight:` splices already applied (§7.7) — and never carries
/// anything a pane's own layout can add back at render time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedLine {
    pub text: String,
    /// When it arrived, in local time (§11.1's channel timestamps).
    pub at: DateTime<Local>,
    pub origin: Origin,
    /// The plain-text projection, kept only when `text` actually has
    /// escapes in it — otherwise `text` already *is* the projection and a
    /// second copy would be pure waste. Read it through [`Self::plain`].
    ///
    /// Computed once, on the way in. Scrollback search (§11.5) matches
    /// against this: deriving it per keystroke would mean re-stripping the
    /// whole buffer on every character typed, which is the cost §2 chose
    /// this stack to avoid (docs/ARCH_REVIEW.md).
    plain: Option<String>,
}

/// Who put a line in the buffer. Server text and the client's own notices
/// are the same bytes in the same deque otherwise, which is what makes a
/// missed warning unrecoverable once it scrolls (docs/UX_REVIEW.md D).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// The MUD sent it.
    Server,
    /// The client generated it: warnings, confirmations, the help overlay.
    Client,
    /// The player's own automation said it — a trigger's `echo:`, a
    /// `mud.echo()` from a script, a timer, `on_connect` (§7.1, §7.4).
    /// Distinct from `Client` because the two answer different questions:
    /// "what did the client warn me about" (docs/UX_REVIEW.md D) must not
    /// be drowned in every line the player's own rules printed. Which rule
    /// fired is the next thing this should carry, and needs the engine to
    /// report it (docs/ARCH_REVIEW.md).
    Rule,
    /// A command of ours, echoed locally as it was sent.
    Echo,
    /// Text that reached this pane from another session — a cross-session
    /// `echo_to` (§7.5), or a channel route naming the session it arrived
    /// in (§11.1).
    Session(String),
}

impl RetainedLine {
    pub fn server(text: impl Into<String>) -> Self {
        Self::new(text, Origin::Server)
    }

    pub fn client(text: impl Into<String>) -> Self {
        Self::new(text, Origin::Client)
    }

    pub fn echo(text: impl Into<String>) -> Self {
        Self::new(text, Origin::Echo)
    }

    pub fn with_origin(text: impl Into<String>, origin: Origin) -> Self {
        Self::new(text, origin)
    }

    pub fn from_session(name: impl Into<String>, text: impl Into<String>) -> Self {
        Self::new(text, Origin::Session(name.into()))
    }

    fn new(text: impl Into<String>, origin: Origin) -> Self {
        let text = text.into();
        // `strip_ansi` walks the line either way; keeping the result only
        // when it differs is what makes the cache free for the uncoloured
        // majority of a buffer.
        let stripped = strip_ansi(&text);
        let plain = (stripped != text).then_some(stripped);
        Self {
            text,
            at: Local::now(),
            origin,
            plain,
        }
    }

    /// What the player reads, with escapes taken out — the same projection
    /// triggers match against (§7.1), so a pattern built from a picked line
    /// means the same thing as one that matched it.
    pub fn plain(&self) -> &str {
        self.plain.as_deref().unwrap_or(&self.text)
    }
}

/// The plain-text projection of a line (docs/ARCHITECTURE.md §8): the text
/// with ANSI escape sequences removed, which is what triggers match against
/// (§7.1) so a pattern never has to account for colour codes.
pub fn strip_ansi(text: &str) -> String {
    strip_ansi_with_map(text).0
}

/// As [`strip_ansi`], plus the offset walk highlight splicing needs
/// (§7.7): for each byte of the stripped text, the offset it came from in
/// `text`, and a final entry one past the last kept byte.
pub(crate) fn strip_ansi_with_map(text: &str) -> (String, Vec<usize>) {
    let mut out = String::with_capacity(text.len());
    let mut map: Vec<usize> = Vec::with_capacity(text.len() + 1);
    let mut end = 0;
    let mut chars = text.char_indices();

    while let Some((at, ch)) = chars.next() {
        if ch != '\x1b' {
            out.push(ch);
            map.extend(at..at + ch.len_utf8());
            end = at + ch.len_utf8();
            continue;
        }
        match chars.next() {
            // CSI: parameters and intermediates, then one final byte.
            Some((_, '[')) => {
                for (_, ch) in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&ch) {
                        break;
                    }
                }
            }
            // OSC: runs to BEL or the ST two-byte terminator.
            Some((_, ']')) => {
                while let Some((_, ch)) = chars.next() {
                    if ch == '\x07' {
                        break;
                    }
                    if ch == '\x1b' {
                        chars.next();
                        break;
                    }
                }
            }
            // Two-byte escapes (charset selection and friends): drop both.
            Some(_) => {}
            None => {}
        }
    }

    map.push(end);
    (out, map)
}

#[cfg(test)]
mod plain_tests {
    use super::*;

    /// The cache has to agree with the projection everything else uses. A
    /// stored copy that drifts from `strip_ansi` would make a trigger built
    /// from a picked line match something other than the line it was picked
    /// from (§7.1).
    #[test]
    fn the_cached_projection_matches_strip_ansi() {
        for text in [
            "\x1b[1;33mgold\x1b[0m",
            "plain text",
            "",
            "\x1b[32mYou see \x1b[1;93mKestrel\x1b[0m\x1b[32m here\x1b[0m",
            "a\x1b]0;title\x07b",
            "Ærlend has arrived.",
            "text\x1b[",
        ] {
            assert_eq!(
                RetainedLine::server(text).plain(),
                strip_ansi(text),
                "{text:?}"
            );
        }
    }

    /// A buffer is mostly uncoloured lines, and on those the projection *is*
    /// the text — storing it twice would double a 10,000-line pane's memory
    /// to cache something it already has.
    #[test]
    fn an_uncoloured_line_stores_no_second_copy() {
        assert!(RetainedLine::server("You see a rat.").plain.is_none());
        assert!(RetainedLine::server("\x1b[31mred\x1b[0m").plain.is_some());
    }

    /// §7.7 splices highlights into the raw line *before* it is retained, so
    /// the projection has to be of the spliced text — and still read as the
    /// line the server sent.
    #[test]
    fn a_highlighted_line_projects_to_what_the_server_sent() {
        let raw = "\x1b[32mYou see Kestrel here\x1b[0m";
        let spliced = "\x1b[32mYou see \x1b[1;93mKestrel\x1b[0m\x1b[32m here\x1b[0m";
        assert_eq!(
            RetainedLine::server(spliced).plain(),
            RetainedLine::server(raw).plain()
        );
        assert_eq!(
            RetainedLine::server(spliced).plain(),
            "You see Kestrel here"
        );
    }
}

#[cfg(test)]
mod strip_tests {
    use super::strip_ansi;

    #[test]
    fn removes_sgr_colour_codes() {
        assert_eq!(strip_ansi("\x1b[1;33mgold\x1b[0m"), "gold");
        assert_eq!(
            strip_ansi("\x1b[38;2;255;0;0mtruecolor\x1b[0m"),
            "truecolor"
        );
    }

    #[test]
    fn leaves_plain_text_untouched() {
        assert_eq!(strip_ansi("Ærlend has arrived."), "Ærlend has arrived.");
    }

    /// A trigger must be able to match a line the server coloured, without
    /// the pattern knowing anything about colour codes.
    #[test]
    fn a_coloured_line_matches_a_plain_pattern() {
        let plain = strip_ansi("\x1b[31mThe \x1b[1mkobold\x1b[0m is DEAD!\x1b[0m");
        assert_eq!(plain, "The kobold is DEAD!");
    }

    #[test]
    fn removes_cursor_movement_and_osc_sequences() {
        assert_eq!(strip_ansi("a\x1b[2Jb"), "ab");
        assert_eq!(strip_ansi("a\x1b]0;title\x07b"), "ab");
        assert_eq!(strip_ansi("a\x1b]0;title\x1b\\b"), "ab");
    }

    #[test]
    fn tolerates_a_truncated_escape_at_the_end() {
        assert_eq!(strip_ansi("text\x1b"), "text");
        assert_eq!(strip_ansi("text\x1b["), "text");
    }
}
