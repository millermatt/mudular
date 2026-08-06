//! Line assembly: decoded text + prompt boundaries → session events.
//!
//! Sans-IO and unit-testable (docs/ARCHITECTURE.md §8). Text arrives in
//! arbitrary network-sized chunks, so a line may be split across reads and
//! several lines may share one read.
//!
//! Distinguishing "prompt" from "incomplete line" is the reason GA/EOR
//! exists: both look like trailing text with no newline. When the server
//! sends a boundary we know the text is a prompt, consume it, and keep
//! showing it while later output scrolls beneath. Without a boundary we
//! fall back to showing trailing text as a provisional prompt — which is
//! why a server that never sends GA/EOR can still run a partial line into
//! the following output.

use super::SessionEvent;

#[derive(Debug, Default)]
pub struct LineAssembler {
    /// Text since the last newline or prompt boundary.
    pending: String,
    /// The last GA/EOR-delimited prompt, kept pinned while output scrolls.
    sticky: String,
}

impl LineAssembler {
    /// Feed decoded text, emitting a `Line` per completed line and a
    /// `Prompt` reflecting what should currently sit above the input.
    pub fn feed(&mut self, text: &str) -> Vec<SessionEvent> {
        self.pending.push_str(text);

        let mut events = Vec::new();
        while let Some(idx) = self.pending.find('\n') {
            let line: String = self.pending.drain(..=idx).collect();
            events.push(SessionEvent::Line(
                line.trim_end_matches(['\r', '\n']).to_string(),
            ));
        }
        events.push(SessionEvent::Prompt(self.current_prompt()));
        events
    }

    /// GA/EOR received: the pending text is a confirmed prompt. Consuming
    /// it here is what stops the next burst of output from being appended
    /// onto the prompt line.
    pub fn prompt_boundary(&mut self) -> SessionEvent {
        self.sticky = std::mem::take(&mut self.pending);
        SessionEvent::Prompt(self.sticky.clone())
    }

    /// An unterminated tail supersedes the last confirmed prompt; once it
    /// completes into a line, the confirmed prompt is shown again.
    fn current_prompt(&self) -> String {
        if self.pending.is_empty() {
            self.sticky.clone()
        } else {
            self.pending.clone()
        }
    }
}

/// The plain-text projection of a line (docs/ARCHITECTURE.md §8): the text
/// with ANSI escape sequences removed, which is what triggers match against
/// (§7.1) so a pattern never has to account for colour codes.
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();

    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            // CSI: parameters and intermediates, then one final byte.
            Some('[') => {
                for ch in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&ch) {
                        break;
                    }
                }
            }
            // OSC: runs to BEL or the ST two-byte terminator.
            Some(']') => {
                while let Some(ch) = chars.next() {
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

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(events: &[SessionEvent]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|ev| match ev {
                SessionEvent::Line(line) => Some(line.as_str()),
                _ => None,
            })
            .collect()
    }

    fn prompt(events: &[SessionEvent]) -> &str {
        events
            .iter()
            .rev()
            .find_map(|ev| match ev {
                SessionEvent::Prompt(text) => Some(text.as_str()),
                _ => None,
            })
            .expect("feed always reports the current prompt")
    }

    #[test]
    fn splits_lines_and_strips_crlf() {
        let mut a = LineAssembler::default();
        let events = a.feed("one\r\ntwo\r\n");
        assert_eq!(lines(&events), vec!["one", "two"]);
        assert_eq!(prompt(&events), "");
    }

    #[test]
    fn reassembles_a_line_split_across_reads() {
        let mut a = LineAssembler::default();
        let first = a.feed("half a ");
        assert!(lines(&first).is_empty());
        // Provisional: we cannot yet tell a partial line from a prompt.
        assert_eq!(prompt(&first), "half a ");

        let second = a.feed("line\r\n");
        assert_eq!(lines(&second), vec!["half a line"]);
        assert_eq!(prompt(&second), "");
    }

    #[test]
    fn keeps_a_confirmed_prompt_pinned_while_output_scrolls() {
        let mut a = LineAssembler::default();
        a.feed("HP:100 MP:50> ");
        let ev = a.prompt_boundary();
        assert_eq!(ev, SessionEvent::Prompt("HP:100 MP:50> ".to_string()));

        // Output after the boundary is its own line, not glued onto the
        // prompt, and the prompt stays put above it.
        let events = a.feed("You hit the rat.\r\n");
        assert_eq!(lines(&events), vec!["You hit the rat."]);
        assert_eq!(prompt(&events), "HP:100 MP:50> ");
    }

    #[test]
    fn a_new_prompt_replaces_the_previous_one() {
        let mut a = LineAssembler::default();
        a.feed("HP:100> ");
        a.prompt_boundary();
        a.feed("You are hit.\r\nHP:90> ");
        let ev = a.prompt_boundary();
        assert_eq!(ev, SessionEvent::Prompt("HP:90> ".to_string()));
    }

    #[test]
    fn boundary_with_nothing_pending_clears_the_prompt() {
        let mut a = LineAssembler::default();
        a.feed("HP:100> ");
        a.prompt_boundary();
        // A boundary straight after a complete line means no prompt text.
        a.feed("You died.\r\n");
        assert_eq!(a.prompt_boundary(), SessionEvent::Prompt(String::new()));
    }

    #[test]
    fn handles_bare_newlines_without_carriage_returns() {
        let mut a = LineAssembler::default();
        let events = a.feed("one\ntwo\n");
        assert_eq!(lines(&events), vec!["one", "two"]);
    }

    #[test]
    fn preserves_ansi_sequences_in_line_text() {
        let mut a = LineAssembler::default();
        let events = a.feed("\x1b[1;33mgold\x1b[0m\r\n");
        assert_eq!(lines(&events), vec!["\x1b[1;33mgold\x1b[0m"]);
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
