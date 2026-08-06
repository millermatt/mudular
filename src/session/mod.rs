//! Per-character session pipeline.
//!
//! One tokio task per session owns its transport, decompressor, Telnet
//! machine, charset decoder, automation engine, and scrollback — nothing is
//! shared between sessions (docs/ARCHITECTURE.md §3). The task assembly
//! itself lands in M0; these are the channel message types the UI and
//! session tasks exchange.

pub type SessionId = usize;

/// Session → UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// A completed output line (ANSI styling preserved).
    Line(String),
    /// The current prompt, delimited by EOR/GA or heuristics.
    Prompt(String),
    /// Server asked us to mask/unmask local input (Telnet ECHO).
    EchoMask(bool),
    /// The session terminated; the pane stays up showing the reason.
    Ended(String),
}

/// UI → session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionCommand {
    /// Send one line to the server (already alias-expanded).
    SendLine(String),
    /// Pane was resized; renegotiate NAWS.
    Resize {
        cols: u16,
        rows: u16,
    },
    Disconnect,
}
