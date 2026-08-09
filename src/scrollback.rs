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

    pub fn from_session(name: impl Into<String>, text: impl Into<String>) -> Self {
        Self::new(text, Origin::Session(name.into()))
    }

    fn new(text: impl Into<String>, origin: Origin) -> Self {
        Self {
            text: text.into(),
            at: Local::now(),
            origin,
        }
    }
}
