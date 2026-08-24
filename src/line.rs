//! The line-oriented front end: output as a stream of lines appended to an
//! ordinary terminal, which keeps its own scrollback (docs/LINE_MODE.md §6).
//!
//! It shares the model, the pipeline, the rules and the sessions with `ui`
//! and shares none of the drawing. The whole of its contract with the
//! terminal is here: **append only, and never revisit a line already
//! written**. The input line is the one exception, and it is redrawn with
//! `\r` and `ESC[K` alone — an absolute cursor move is what makes a screen
//! reader re-read text the player has already heard (§6.2), and
//! `line_mode_appends_and_never_repaints` asserts none is ever sent.
//!
//! Nothing here draws a pane, so nothing here needs to know a pane's size.
//! What it needs to know instead is what it has *not printed yet*, which is
//! what `SessionView::pushed` counts.

use std::collections::HashMap;
use std::io::{self, Write};

use crate::scrollback::Origin;
use crate::state::{AppState, SessionId};

/// Back to column one, then erase to the end of the line. The two
/// sequences a line front end is allowed to move the cursor with.
const ERASE_LINE: &str = "\r\x1b[K";

/// What the player is typing, marked off from what the MUD said.
const PROMPT: &str = "> ";

/// The terminal, and how much of the model has reached it.
pub(crate) struct Screen {
    out: io::Stdout,
    /// `pushed` as of the last print, per session and per channel pane.
    /// Every session's count advances on every refresh even though only
    /// the bound one prints, so switching character starts at that
    /// character's next line rather than replaying what it missed (§6.5).
    sessions: HashMap<SessionId, u64>,
    channels: Vec<u64>,
    /// The prompt as last printed. A prompt that changed while output was
    /// arriving is printed at the next quiet moment instead, which is what
    /// keeps a prompt-per-line MUD out of the speech queue (§6.2).
    prompt: String,
    /// Print the player's own commands back into the stream. Off by
    /// default: they stay in scrollback for review without being spoken on
    /// arrival, which is the option Blightmud and Mudlet both landed on
    /// (§6.4).
    echo_sent: bool,
    /// Print the MUD's prompt at all.
    prompts: bool,
}

impl Screen {
    pub(crate) fn new(echo_sent: bool, prompts: bool) -> Self {
        Self {
            out: io::stdout(),
            sessions: HashMap::new(),
            channels: Vec::new(),
            prompt: String::new(),
            echo_sent,
            prompts,
        }
    }

    /// Prints everything the model has gained since the last call, then
    /// redraws the input line under it.
    pub(crate) fn refresh(&mut self, state: &AppState) -> io::Result<()> {
        let out = self.compose(state);
        self.out.write_all(out.as_bytes())?;
        self.out.flush()
    }

    /// What the next write to the terminal is, and the whole of what this
    /// front end decides. Separate from writing it so the decisions —
    /// which lines are new, whether an echo is printed, whether a prompt
    /// has earned a line, whether the input is masked — are testable
    /// without a terminal to read back.
    fn compose(&mut self, state: &AppState) -> String {
        let mut out = String::from(ERASE_LINE);
        let mut printed = false;
        printed |= self.take_session_lines(state, &mut out);
        printed |= self.take_routed_lines(state, &mut out);
        if !printed {
            self.take_prompt(state, &mut out);
        }
        let back = self.compose_input(state, &mut out);
        // Relative, not absolute: `CUB` walks back over what was just
        // written on this line, where `CUP` would name a row on a screen
        // this front end does not own.
        if back > 0 {
            out.push_str(&format!("\x1b[{back}D"));
        }
        out
    }

    /// Says something in the client's own voice, outside the model — used
    /// on the way out, when there is no session left to say it into.
    pub(crate) fn say(&mut self, text: &str) -> io::Result<()> {
        self.out
            .write_all(format!("{ERASE_LINE}{text}\r\n").as_bytes())?;
        self.out.flush()
    }

    /// New lines from the bound session. Every session's watermark moves,
    /// because an unbound character's output is not this front end's to
    /// print (§6.5) — but neither is it a backlog owed to the player the
    /// moment they switch.
    fn take_session_lines(&mut self, state: &AppState, out: &mut String) -> bool {
        let mut printed = false;
        for session in &state.sessions {
            let seen = self.sessions.entry(session.id).or_insert(0);
            let fresh = fresh_count(session.view.pushed, *seen, session.view.scrollback.len());
            *seen = session.view.pushed;
            if session.id != state.input_session {
                continue;
            }
            let start = session.view.scrollback.len() - fresh;
            for line in session.view.scrollback.iter().skip(start) {
                // The command the player just sent. Suppressed from the
                // live stream by default, kept in scrollback either way.
                if !self.echo_sent && line.origin == Origin::Echo {
                    continue;
                }
                append(out, &line.text);
                printed = true;
            }
        }
        printed
    }

    /// Channel-routed lines, printed inline and tagged.
    ///
    /// Routing *moves* a line out of the session's scrollback and into a
    /// channel pane (§11.1), so without this every tell and channel
    /// message would silently vanish in a front end with no panes — output
    /// disappearing rather than a surface refused, which is the most
    /// user-visible hole this design has (§6.2). It is the honest interim,
    /// not the answer: keyed recall is (#149).
    fn take_routed_lines(&mut self, state: &AppState, out: &mut String) -> bool {
        self.channels.resize(state.channels.len(), 0);
        let mut printed = false;
        for (index, pane) in state.channels.iter().enumerate() {
            let seen = &mut self.channels[index];
            let fresh = fresh_count(pane.pushed, *seen, pane.lines.len());
            *seen = pane.pushed;
            let start = pane.lines.len() - fresh;
            for line in pane.lines.iter().skip(start) {
                // Composed from the line's own `origin`, the way the pane
                // composes it — and plural on purpose, because one
                // broadcast heard by three characters is one line naming
                // all three (§6.2).
                let heard_by = match &line.origin {
                    Origin::Session(names) if !names.is_empty() => format!(" {}", names.join(", ")),
                    _ => String::new(),
                };
                append(
                    out,
                    &format!("[{}{}] {}", pane.config.name, heard_by, line.text),
                );
                printed = true;
            }
        }
        printed
    }

    /// The MUD's prompt, when it changed and nothing else has been printed
    /// since. Printing every update as a line would flood the speech queue
    /// §2 names as the dominant failure; never printing it withholds what
    /// the player reads most (§6.2).
    fn take_prompt(&mut self, state: &AppState, out: &mut String) {
        if !self.prompts {
            return;
        }
        let Some(prompt) = state.bound().map(|session| session.view.prompt.as_str()) else {
            return;
        };
        if prompt.is_empty() || prompt == self.prompt {
            return;
        }
        self.prompt = prompt.to_string();
        append(out, prompt);
    }

    /// Writes the input line and answers how far back the cursor belongs.
    ///
    /// Keystroke echo is not optional here — raw mode means the terminal
    /// shows nothing of what is typed — but it must stop at a masked
    /// prompt, which is the one place in this front end where getting it
    /// wrong prints a password to the terminal (§6.6).
    fn compose_input(&self, state: &AppState, out: &mut String) -> usize {
        let Some(session) = state.bound() else {
            out.push_str(PROMPT);
            out.push_str(state.shell_input.value());
            return 0;
        };
        if session.view.masked {
            out.push_str(PROMPT);
            return 0;
        }
        // The ghost is part of the line: what is shown is what Enter sends,
        // which is the whole bargain of an inline completion (§11.3).
        let shown = session.completed_input();
        out.push_str(PROMPT);
        out.push_str(&shown);
        shown.chars().count() - session.view.input.visual_cursor()
    }
}

/// How many of a buffer's lines are new. Capped at what the buffer still
/// holds: a burst longer than the whole scrollback has dropped its oldest
/// lines before anything could print them, and the alternative to printing
/// what is left is printing the newest lines twice.
fn fresh_count(pushed: u64, seen: u64, held: usize) -> usize {
    usize::try_from(pushed.saturating_sub(seen))
        .unwrap_or(usize::MAX)
        .min(held)
}

/// Appends one line. `\r\n` because raw mode leaves the carriage return to
/// the writer.
fn append(out: &mut String, text: &str) {
    out.push_str(text);
    out.push_str("\r\n");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scrollback::RetainedLine;
    use crate::state::test_support::app_with_receivers;

    /// §6.4 — sent-command echo, both branches. Off by default: the
    /// command is kept in scrollback for review without being spoken back
    /// on arrival, which is what Blightmud ships and what Mudlet's
    /// accessibility manual tells its users to configure.
    #[test]
    fn a_sent_command_is_kept_but_not_printed_by_default() {
        let (mut state, _rx) = app_with_receivers(&["tank"]);
        state.sessions[0].push_line(RetainedLine::echo("> kill dragon"));
        state.sessions[0].push_line(RetainedLine::server("The dragon dies."));

        let quiet = Screen::new(false, true).compose(&state);
        assert!(!quiet.contains("kill dragon"), "{quiet:?}");
        assert!(quiet.contains("The dragon dies."), "{quiet:?}");

        let (mut state, _rx) = app_with_receivers(&["tank"]);
        state.sessions[0].push_line(RetainedLine::echo("> kill dragon"));
        let echoing = Screen::new(true, true).compose(&state);
        assert!(echoing.contains("kill dragon"), "{echoing:?}");
    }

    /// §6.2 — a prompt that changed is worth a line only when nothing else
    /// has been printed since. A prompt printed on every update is the
    /// speech backlog §2 names as the dominant failure.
    #[test]
    fn the_prompt_waits_for_a_quiet_moment() {
        let (mut state, _rx) = app_with_receivers(&["tank"]);
        state.sessions[0].view.prompt = "100hp 50m>".to_string();
        state.sessions[0].push_line(RetainedLine::server("A rat scurries by."));

        let busy = {
            let mut screen = Screen::new(false, true);
            let busy = screen.compose(&state);
            assert!(!busy.contains("100hp"), "{busy:?}");
            // Nothing new arrived this time, so the prompt gets its line.
            screen.compose(&state)
        };
        assert!(busy.contains("100hp 50m>"), "{busy:?}");
    }

    /// The same prompt, suppressed outright — the setting §6.4 asks for.
    #[test]
    fn the_prompt_can_be_turned_off() {
        let (mut state, _rx) = app_with_receivers(&["tank"]);
        state.sessions[0].view.prompt = "100hp 50m>".to_string();
        let mut screen = Screen::new(false, false);
        screen.compose(&state);
        let out = screen.compose(&state);
        assert!(!out.contains("100hp"), "{out:?}");
    }

    /// §6.6 — keystroke echo is the client's job here, and it stops at a
    /// masked prompt. Nothing inherits this: `push_line` does not keep a
    /// password out of scrollback either.
    #[test]
    fn a_masked_prompt_is_not_echoed() {
        let (mut state, _rx) = app_with_receivers(&["tank"]);
        state.sessions[0].view.input = state.sessions[0]
            .view
            .input
            .clone()
            .with_value("hunter2".to_string());

        let visible = Screen::new(false, true).compose(&state);
        assert!(visible.contains("hunter2"), "{visible:?}");

        state.sessions[0].view.masked = true;
        let masked = Screen::new(false, true).compose(&state);
        assert!(!masked.contains("hunter2"), "{masked:?}");
    }

    /// Only the bound character prints, and switching does not replay what
    /// the other one said while it was unbound (§6.5).
    #[test]
    fn an_unbound_character_neither_prints_nor_accumulates() {
        let (mut state, _rx) = app_with_receivers(&["tank", "cleric"]);
        let mut screen = Screen::new(false, true);
        screen.compose(&state);

        state.sessions[1].push_line(RetainedLine::server("cleric sees a rat"));
        let out = screen.compose(&state);
        assert!(!out.contains("cleric sees a rat"), "{out:?}");

        let cleric = state.sessions[1].id;
        state.input_session = cleric;
        let after = screen.compose(&state);
        assert!(
            !after.contains("cleric sees a rat"),
            "switching should start at the next line, not replay a backlog: {after:?}"
        );
    }

    #[test]
    fn a_burst_longer_than_the_buffer_prints_what_survived() {
        // 500 lines pushed, none seen, but the buffer only kept 100.
        assert_eq!(fresh_count(500, 0, 100), 100);
    }

    #[test]
    fn nothing_new_prints_nothing() {
        assert_eq!(fresh_count(12, 12, 100), 0);
    }
}
