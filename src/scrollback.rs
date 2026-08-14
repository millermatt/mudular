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
//!
//! It is also where text is made safe to show. Ratatui writes a cell's
//! symbol to the terminal unfiltered, so any control byte that reaches a
//! retained line reaches the terminal as a command — which §13 forbids for
//! data the client did not author.

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

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
/// Serialized as part of a persisted channel pane (§11.1), so renaming a
/// variant renames it on disk — `config::load_comms` is lenient about a
/// file it cannot parse, which turns such a rename into silently dropped
/// history rather than an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// The sessions a line involves — a cross-session `echo_to` (§7.5),
    /// which is always the one it came from, or a channel route naming the
    /// characters it arrived in (§11.1).
    ///
    /// Several, because one broadcast heard by three characters is one
    /// message: the channel pane collapses the copies into a single entry
    /// and lists everyone who heard it, rather than repeating the sentence
    /// once per character.
    Session(Vec<String>),
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
        Self::new(text, Origin::Session(vec![name.into()]))
    }

    /// Rebuilds a line read back off disk (§11.1's persisted comms), with
    /// the time it arrived rather than the time it was read — a restored
    /// pane whose every line was stamped "now" would say a week-old tell
    /// had just come in.
    ///
    /// `plain` is recomputed rather than stored: it is derived from `text`,
    /// and a file able to disagree with its own text would be a second
    /// source of truth for what a line says.
    pub fn restored(text: impl Into<String>, at: DateTime<Local>, origin: Origin) -> Self {
        Self {
            at,
            ..Self::new(text, origin)
        }
    }

    fn new(text: impl Into<String>, origin: Origin) -> Self {
        // Every line, whoever wrote it. `session` strips server text on the
        // way in (its triggers match the result), but a client notice, a
        // script's `mud.echo`, and a peer's relayed line never passed
        // through that — and a community module's script is untrusted
        // (docs/ACTORS.md §2).
        let text = strip_unsafe_controls(&text.into());
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

/// Drops the control bytes a terminal would obey, keeping the two a pane
/// needs: `\n`, which separates rendered rows, and `ESC`, which carries the
/// SGR colour that is the whole point of a MUD pane — `ansi-to-tui` consumes
/// escape sequences at render time, so an `ESC` never reaches a cell.
///
/// A tab becomes a space rather than disappearing: dropping it welds the
/// words either side together, which is how a Lua traceback's `in\tfunction`
/// reached the screen as `infunction` (docs/UX_REVIEW.md 4).
pub fn strip_unsafe_controls(text: &str) -> String {
    text.chars()
        .filter_map(|c| match c {
            '\t' => Some(' '),
            '\n' | '\x1b' => Some(c),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect()
}

/// The same bytes, shown rather than removed — for the raw GMCP inspector
/// (§14 M6), whose whole purpose is to reveal what the server actually sent.
/// Stripping there would hide the one thing a player opened it to see, so a
/// control byte renders as its own escape (`\x1b`) and executes nothing.
pub fn escape_controls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            c if c.is_control() => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out
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
    /// The invariant is between the line and *its own* text: `plain()` is
    /// the projection of what was stored, after sanitising, not of the
    /// argument that went in. (They differ where sanitising removes a
    /// `BEL` that was terminating an OSC — the sequence then runs to the
    /// end of the line. A server sending OSC is trying to drive the
    /// terminal, so losing the rest of that line is the right way to lose.)
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
            let line = RetainedLine::server(text);
            assert_eq!(line.plain(), strip_ansi(&line.text), "{text:?}");
        }
    }

    /// §13 again, on the other side of the funnel. `strip_unsafe_controls`
    /// only ever ran on decoded *server* text, so a line the client or a
    /// script wrote reached the screen with whatever control bytes it
    /// carried — and a community module's script (docs/ACTORS.md §2) is
    /// untrusted by the same reasoning the §7.4 sandbox exists for.
    /// A retained line is safe by construction instead.
    #[test]
    fn a_retained_line_never_carries_a_raw_control_byte() {
        let line = RetainedLine::client("a\rb\x07c");
        assert!(
            !line
                .text
                .chars()
                .any(|c| c.is_control() && c != '\x1b' && c != '\n'),
            "{:?}",
            line.text
        );
    }

    /// A tab is dropped rather than kept, which welds words together: a Lua
    /// traceback's `in\tfunction` became `infunction` (docs/UX_REVIEW.md 4).
    /// A space keeps them apart without letting the tab move the cursor.
    #[test]
    fn a_tab_becomes_a_space_rather_than_vanishing() {
        assert_eq!(
            RetainedLine::client("in\tfunction 'error'").text,
            "in function 'error'"
        );
    }

    /// SGR has to survive: colour is the one escape a pane is *for*.
    #[test]
    fn colour_escapes_survive_sanitising() {
        let line = RetainedLine::server("\x1b[1;33mgold\x1b[0m");
        assert_eq!(line.text, "\x1b[1;33mgold\x1b[0m");
        assert_eq!(line.plain(), "gold");
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
