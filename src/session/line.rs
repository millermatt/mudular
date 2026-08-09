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

use std::ops::Range;

use super::SessionEvent;
use crate::scrollback::strip_ansi_with_map;

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
            events.push(SessionEvent::server(line.trim_end_matches(['\r', '\n'])));
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

/// Splices highlight ranges (§7.7) into the raw line the server sent.
/// Ranges are byte offsets over the stripped projection, so they are
/// mapped back through the same walk `strip_ansi` performs.
///
/// Closing a highlight with a bare `ESC[0m` would destroy whatever colour
/// the server had running for the rest of the line. Instead the close
/// replays, verbatim, every SGR sequence that appeared earlier in the raw
/// line: re-emitting the same sequences in the same order leaves the
/// terminal in the state it was in, without this module having to know
/// what any of them mean.
pub fn apply_highlights(raw: &str, spans: &[(Range<usize>, String)]) -> String {
    if spans.is_empty() {
        return raw.to_string();
    }
    let (_, map) = strip_ansi_with_map(raw);

    let mut mapped: Vec<(usize, usize, &str)> = spans
        .iter()
        .filter_map(|(span, sgr)| Some((*map.get(span.start)?, *map.get(span.end)?, sgr.as_str())))
        .collect();
    // Highest offset first: each insertion then leaves the raw offsets of
    // the spans still to come untouched.
    mapped.sort_by(|a, b| b.0.cmp(&a.0));

    let mut out = raw.to_string();
    for (start, end, sgr) in mapped {
        out.insert_str(end, &format!("\x1b[0m{}", sgr_prefix(raw, end)));
        out.insert_str(start, &format!("\x1b[{sgr}m"));
    }
    out
}

/// Every `ESC[...m` sequence in `raw` before `upto`, concatenated in order.
fn sgr_prefix(raw: &str, upto: usize) -> String {
    let mut out = String::new();
    let head = &raw[..upto];
    let mut chars = head.char_indices();

    while let Some((at, ch)) = chars.next() {
        if ch != '\x1b' || !matches!(chars.next(), Some((_, '['))) {
            continue;
        }
        for (end, ch) in chars.by_ref() {
            if ('\x40'..='\x7e').contains(&ch) {
                if ch == 'm' {
                    out.push_str(&head[at..end + 1]);
                }
                break;
            }
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
                SessionEvent::Line { text, .. } => Some(text.as_str()),
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
mod highlight_tests {
    use super::apply_highlights;
    use crate::scrollback::strip_ansi;

    fn span(range: std::ops::Range<usize>, sgr: &str) -> (std::ops::Range<usize>, String) {
        (range, sgr.to_string())
    }

    #[test]
    fn splices_a_span_into_a_plain_line() {
        assert_eq!(
            apply_highlights("You see Kestrel here.", &[span(8..15, "1;93")]),
            "You see \x1b[1;93mKestrel\x1b[0m here."
        );
    }

    #[test]
    fn a_line_with_no_spans_is_returned_untouched() {
        assert_eq!(
            apply_highlights("\x1b[32mplain\x1b[0m", &[]),
            "\x1b[32mplain\x1b[0m"
        );
    }

    /// §7.7's acceptance test: a highlight inside a coloured region leaves
    /// the region looking untouched on both sides. Closing with a bare
    /// `ESC[0m` would leave the rest of the line uncoloured; replaying the
    /// server's own sequences puts its green back.
    #[test]
    fn a_highlight_restores_the_servers_colour_after_itself() {
        let raw = "\x1b[32mYou see Kestrel here\x1b[0m";
        let spliced = apply_highlights(raw, &[span(8..15, "1;93")]);

        assert_eq!(
            spliced,
            "\x1b[32mYou see \x1b[1;93mKestrel\x1b[0m\x1b[32m here\x1b[0m"
        );
        // The text either side is still whatever the server sent, and the
        // line reads the same with the colour taken back off.
        assert_eq!(strip_ansi(&spliced), strip_ansi(raw));
    }

    /// Offsets are over the stripped line, so escapes *before* the span
    /// have to shift it — and all of them are replayed at the close, not
    /// just the last.
    #[test]
    fn offsets_are_mapped_through_the_escapes_that_precede_them() {
        let raw = "\x1b[32mThe \x1b[1mkobold\x1b[22m is here";
        let spliced = apply_highlights(raw, &[span(14..18, "31")]);

        assert_eq!(
            spliced,
            "\x1b[32mThe \x1b[1mkobold\x1b[22m is \x1b[31mhere\x1b[0m\x1b[32m\x1b[1m\x1b[22m"
        );
        assert_eq!(strip_ansi(&spliced), "The kobold is here");
    }

    /// Two spans on one line: splicing the later one first keeps the raw
    /// offsets of the earlier one valid.
    #[test]
    fn several_spans_do_not_disturb_each_others_offsets() {
        assert_eq!(
            apply_highlights(
                "Ærlend meets Kestrel",
                &[span(0..7, "1"), span(14..21, "31")]
            ),
            "\x1b[1mÆrlend\x1b[0m meets \x1b[31mKestrel\x1b[0m"
        );
    }

    /// A whole-line span over a coloured line stops at the last character,
    /// leaving the server's trailing reset where it was.
    #[test]
    fn a_whole_line_span_wraps_the_text_not_the_trailing_reset() {
        let raw = "\x1b[32mYou are bleeding\x1b[0m";
        assert_eq!(
            apply_highlights(raw, &[span(0..16, "97;41")]),
            "\x1b[32m\x1b[97;41mYou are bleeding\x1b[0m\x1b[32m\x1b[0m"
        );
    }
}
