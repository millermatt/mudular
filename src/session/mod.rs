//! Per-character session pipeline.
//!
//! One tokio task per session owns its transport, decompressor, Telnet
//! machine, charset decoder, automation engine, and scrollback — nothing is
//! shared between sessions (docs/ARCHITECTURE.md §3). M0 wires a passthrough
//! pipeline (TCP → Telnet IAC stripping → UTF-8 decode → raw ANSI text);
//! MCCP, charset fallback, and the line/prompt assembler are later
//! milestones — for now, complete lines become `Line` events and any
//! trailing partial line (e.g. an unterminated login prompt) becomes a
//! `Prompt` event so it's still visible.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::net;
use crate::proto::telnet::{TelnetEvent, TelnetMachine};

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

/// Connect and spawn the session task, returning its event stream and
/// command sink. The task runs until the connection ends or `Disconnect`
/// is received.
pub fn spawn(
    host: String,
    port: u16,
    tls: bool,
) -> (mpsc::Receiver<SessionEvent>, mpsc::Sender<SessionCommand>) {
    let (event_tx, event_rx) = mpsc::channel(256);
    let (cmd_tx, cmd_rx) = mpsc::channel(256);
    tokio::spawn(run(host, port, tls, event_tx, cmd_rx));
    (event_rx, cmd_tx)
}

async fn run(
    host: String,
    port: u16,
    tls: bool,
    events: mpsc::Sender<SessionEvent>,
    mut commands: mpsc::Receiver<SessionCommand>,
) {
    let transport = match net::connect(&host, port, tls).await {
        Ok(t) => t,
        Err(err) => {
            let _ = events.send(SessionEvent::Ended(format!("{err:#}"))).await;
            return;
        }
    };

    let (mut reader, mut writer) = tokio::io::split(transport);
    let mut telnet = TelnetMachine::new();
    let mut decoder = Utf8Decoder::default();
    let mut line_buf = String::new();
    let mut sock_buf = [0u8; 4096];

    let reason = loop {
        tokio::select! {
            result = reader.read(&mut sock_buf) => {
                match result {
                    Ok(0) => break "connection closed".to_string(),
                    Ok(n) => {
                        for event in telnet.feed(&sock_buf[..n]) {
                            if let TelnetEvent::Data(bytes) = event {
                                line_buf.push_str(&strip_unsafe_controls(&decoder.decode(&bytes)));
                            }
                        }
                        while let Some(idx) = line_buf.find('\n') {
                            let line: String =
                                line_buf.drain(..=idx).collect::<String>();
                            let line = line.trim_end_matches(['\r', '\n']);
                            if events.send(SessionEvent::Line(line.to_string())).await.is_err() {
                                return;
                            }
                        }
                        if !line_buf.is_empty()
                            && events
                                .send(SessionEvent::Prompt(line_buf.clone()))
                                .await
                                .is_err()
                        {
                            return;
                        }
                    }
                    Err(err) => break format!("connection error: {err}"),
                }
            }
            cmd = commands.recv() => {
                match cmd {
                    Some(SessionCommand::SendLine(line)) => {
                        if writer.write_all(line.as_bytes()).await.is_err()
                            || writer.write_all(b"\r\n").await.is_err()
                        {
                            break "write failed".to_string();
                        }
                    }
                    Some(SessionCommand::Resize { .. }) => {
                        // NAWS lands in M1; ignored for now.
                    }
                    Some(SessionCommand::Disconnect) | None => {
                        break "disconnected".to_string();
                    }
                }
            }
        }
    };

    let _ = events.send(SessionEvent::Ended(reason)).await;
}

/// Drops C0 control bytes other than `\n` (line boundary) and ESC (needed
/// for the ANSI/SGR sequences `ansi-to-tui` renders). Server data is
/// untrusted (docs/ARCHITECTURE.md §13): a bare, unescaped control byte
/// like `\r` reaching the terminal executes as a real cursor command
/// instead of being treated as plain text — some CircleMUD/DikuMUD-lineage
/// servers do send a bare mid-line `\r` this way.
fn strip_unsafe_controls(text: &str) -> String {
    text.chars()
        .filter(|&c| c == '\n' || c == '\x1b' || !c.is_control())
        .collect()
}

/// Incremental UTF-8 decoder: replaces invalid bytes with U+FFFD but never
/// splits a multi-byte sequence across reads (docs/ARCHITECTURE.md §9.1).
#[derive(Default)]
struct Utf8Decoder {
    pending: Vec<u8>,
}

impl Utf8Decoder {
    fn decode(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        match std::str::from_utf8(&self.pending) {
            Ok(text) => {
                let text = text.to_string();
                self.pending.clear();
                text
            }
            Err(err) => {
                let valid_up_to = err.valid_up_to();
                let text = String::from_utf8_lossy(&self.pending[..valid_up_to]).into_owned();
                match err.error_len() {
                    // Invalid (not just incomplete) sequence: drop it and
                    // mark with the replacement character.
                    Some(bad_len) => {
                        let rest = self.pending.split_off(valid_up_to + bad_len);
                        self.pending = rest;
                        format!("{text}\u{FFFD}")
                    }
                    // Incomplete trailing sequence: hold it for next read.
                    None => {
                        self.pending.drain(..valid_up_to);
                        text
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::time::timeout;

    use super::*;

    /// Some servers (CircleMUD/DikuMUD-lineage codebases, observed live
    /// against hercmud.net) send a bare `\r` mid-line with no following
    /// `\n` — a raw carriage-return control byte, not part of any ANSI/SGR
    /// escape sequence. If it reaches the terminal unfiltered, it executes
    /// as a real carriage return and yanks the cursor back to column 1,
    /// overwriting the pane's left border — exactly the "terminal escape
    /// injection" docs/ARCHITECTURE.md §13 calls out ("server data is
    /// untrusted"). Verified by replaying a captured raw session through a
    /// real terminal emulator (pyte): the corruption is in the bytes we
    /// write, not specific to any one terminal's rendering.
    #[tokio::test]
    async fn strips_bare_carriage_return_from_mid_line_server_output() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            sock.write_all(b"Alignment:  \r[    0]  Can't decide to be good or evil.\r\n")
                .await
                .unwrap();
        });

        let (mut events, _commands) = spawn("127.0.0.1".to_string(), port, false);

        let line = next_event(&mut events).await;
        assert_eq!(
            line,
            SessionEvent::Line("Alignment:  [    0]  Can't decide to be good or evil.".into()),
            "a bare \\r must not reach the rendered line"
        );
    }

    /// Reads a fake MUD server's minimal login/echo/quit script against a
    /// real session task over a real loopback socket — the in-process fake
    /// server integration test called for in docs/ARCHITECTURE.md §12.
    #[tokio::test]
    async fn walks_a_login_echo_quit_session() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            sock.write_all(b"\x1b[1;33mWelcome to FakeMUD\x1b[0m\r\n\r\nLogin: ")
                .await
                .unwrap();

            let mut buf = vec![0u8; 1024];
            let n = sock.read(&mut buf).await.unwrap();
            let name = String::from_utf8_lossy(&buf[..n]);
            let name = name.trim();
            sock.write_all(format!("Hello, {name}!\r\n> ").as_bytes())
                .await
                .unwrap();

            let n = sock.read(&mut buf).await.unwrap();
            let cmd = String::from_utf8_lossy(&buf[..n]);
            assert_eq!(cmd.trim(), "quit");
            sock.write_all(b"Bye!\r\n").await.unwrap();
        });

        let (mut events, commands) = spawn("127.0.0.1".to_string(), port, false);

        let banner = next_event(&mut events).await;
        assert_eq!(
            banner,
            SessionEvent::Line("\x1b[1;33mWelcome to FakeMUD\x1b[0m".into())
        );
        let blank = next_event(&mut events).await;
        assert_eq!(blank, SessionEvent::Line(String::new()));
        let prompt = next_event(&mut events).await;
        assert_eq!(prompt, SessionEvent::Prompt("Login: ".into()));

        commands
            .send(SessionCommand::SendLine("Kestrel".into()))
            .await
            .unwrap();

        // Without GA/EOR prompt-boundary detection (M1), a prompt with no
        // trailing `\n` legitimately merges with the server's next output —
        // this is the documented M0 limitation (docs/ARCHITECTURE.md §6.2, §8).
        let greeting = next_event(&mut events).await;
        assert_eq!(
            greeting,
            SessionEvent::Line("Login: Hello, Kestrel!".into())
        );
        let prompt = next_event(&mut events).await;
        assert_eq!(prompt, SessionEvent::Prompt("> ".into()));

        commands
            .send(SessionCommand::SendLine("quit".into()))
            .await
            .unwrap();

        let bye = next_event(&mut events).await;
        assert_eq!(bye, SessionEvent::Line("> Bye!".into()));
        let ended = next_event(&mut events).await;
        assert!(matches!(ended, SessionEvent::Ended(_)));
    }

    async fn next_event(events: &mut mpsc::Receiver<SessionEvent>) -> SessionEvent {
        timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("timed out waiting for session event")
            .expect("session event stream ended early")
    }
}
