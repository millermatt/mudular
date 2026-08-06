//! Per-character session pipeline.
//!
//! One tokio task per session owns its transport, decompressor, Telnet
//! machine, charset decoder, automation engine, and scrollback — nothing is
//! shared between sessions (docs/ARCHITECTURE.md §3). The inbound pipeline
//! is §6.5: TCP → MCCP inflate → Telnet FSM (with RFC 1143 negotiation) →
//! charset decode → line assembler → trigger engine → UI events.

mod line;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::engine::Engine;
use crate::net::{self, TlsConfig};
use crate::proto::charset::Charset;
use crate::proto::mccp::MccpDecoder;
use crate::proto::telnet::{Side, TelnetEvent, TelnetMachine, option};
use line::{LineAssembler, strip_ansi};

pub type SessionId = usize;

/// Session → UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// A completed output line (ANSI styling preserved).
    Line(String),
    /// The text that should sit above the input line. Empty means none.
    Prompt(String),
    /// Server asked us to mask/unmask local input (Telnet ECHO).
    EchoMask(bool),
    /// What the transport is trusting, once connected.
    Security(net::Security),
    /// The session terminated; the pane stays up showing the reason.
    Ended(String),
}

/// UI → session.
#[derive(Debug)]
pub enum SessionCommand {
    /// One line as typed. The session splits it on `;` and expands
    /// aliases: the engine owns the variable store that both aliases and
    /// triggers read, so it lives in the session task where that state
    /// needs no locking (docs/ARCHITECTURE.md §3).
    SendLine(String),
    /// Replace the rule set without reconnecting (`/reload`).
    SetRules(Box<Engine>),
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
    tls: Option<TlsConfig>,
    record: Option<PathBuf>,
    charset: Charset,
    engine: Engine,
) -> (mpsc::Receiver<SessionEvent>, mpsc::Sender<SessionCommand>) {
    let (event_tx, event_rx) = mpsc::channel(256);
    let (cmd_tx, cmd_rx) = mpsc::channel(256);
    tokio::spawn(run(
        host, port, tls, record, charset, engine, event_tx, cmd_rx,
    ));
    (event_rx, cmd_tx)
}

#[allow(clippy::too_many_arguments)]
async fn run(
    host: String,
    port: u16,
    tls: Option<TlsConfig>,
    record: Option<PathBuf>,
    charset: Charset,
    mut engine: Engine,
    events: mpsc::Sender<SessionEvent>,
    mut commands: mpsc::Receiver<SessionCommand>,
) {
    let connection = match net::connect(&host, port, tls.as_ref()).await {
        Ok(connection) => connection,
        Err(err) => {
            let _ = events.send(SessionEvent::Ended(format!("{err:#}"))).await;
            return;
        }
    };
    let net::Connection {
        transport,
        security,
    } = connection;
    if events.send(SessionEvent::Security(security)).await.is_err() {
        return;
    }

    let mut recorder = match record {
        Some(path) => match Recorder::create(&path, &host, port) {
            Ok(recorder) => Some(recorder),
            Err(err) => {
                let _ = events
                    .send(SessionEvent::Ended(format!(
                        "cannot record to {}: {err}",
                        path.display()
                    )))
                    .await;
                return;
            }
        },
        None => None,
    };

    let (mut reader, mut writer) = tokio::io::split(transport);
    let mut telnet = TelnetMachine::new();
    let mut mccp = MccpDecoder::new();
    let mut decoder = TextDecoder::new(charset);
    let mut assembler = LineAssembler::default();
    let mut sock_buf = [0u8; 4096];
    // Inflate output, reused across reads.
    let mut plain: Vec<u8> = Vec::new();

    // Offer NAWS up front; the size follows once the server agrees (§6.2).
    telnet.request_local_enable(option::NAWS);
    if flush_telnet(&mut telnet, &mut writer).await.is_err() {
        let _ = events
            .send(SessionEvent::Ended("write failed".to_string()))
            .await;
        return;
    }
    engine.start_timers(Instant::now());

    let reason = loop {
        // `select!` needs a future even when nothing is scheduled; park
        // far out rather than busy-waiting when there are no timers.
        let timer_deadline = engine
            .next_timer_deadline()
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(3600));

        tokio::select! {
            result = reader.read(&mut sock_buf) => {
                match result {
                    Ok(0) => break "connection closed".to_string(),
                    Ok(n) => {
                        let raw = &sock_buf[..n];
                        if let Some(recorder) = recorder.as_mut() {
                            recorder.record(raw);
                        }

                        let mut outbound: Vec<String> = Vec::new();
                        let mut break_reason: Option<String> = None;
                        // Inflate, then parse. Turning compression on ends
                        // the parse mid-buffer: the rest of the read is
                        // zlib, so it goes back through `mccp` (§6.4/§6.5).
                        let mut pending = Bytes::copy_from_slice(raw);
                        while !pending.is_empty() {
                            plain.clear();
                            if let Err(err) = mccp.feed(&pending, &mut plain) {
                                break_reason = Some(err.to_string());
                                break;
                            }
                            pending = Bytes::new();

                            for event in telnet.feed(&plain) {
                                let emitted = match event {
                                    TelnetEvent::Data(bytes) => {
                                        let text = strip_unsafe_controls(&decoder.decode(&bytes));
                                        assembler.feed(&text)
                                    }
                                    TelnetEvent::PromptBoundary => {
                                        vec![assembler.prompt_boundary()]
                                    }
                                    TelnetEvent::OptionEnabled { option: option::ECHO, side: Side::Remote } => {
                                        vec![SessionEvent::EchoMask(true)]
                                    }
                                    TelnetEvent::OptionDisabled { option: option::ECHO, side: Side::Remote } => {
                                        vec![SessionEvent::EchoMask(false)]
                                    }
                                    TelnetEvent::CompressionStart => {
                                        mccp.activate();
                                        pending = telnet.take_deferred();
                                        Vec::new()
                                    }
                                    // Other options are handled inside the
                                    // Telnet machine; subnegotiations reach
                                    // the engine in M6.
                                    _ => Vec::new(),
                                };

                                for event in emitted {
                                    // Triggers run between the line assembler
                                    // and the UI (§6.5), so a gagged line never
                                    // reaches the scrollback at all.
                                    let event = match event {
                                        SessionEvent::Line(text) => {
                                            let outcome = engine.process_line(&strip_ansi(&text));
                                            outbound.extend(outcome.sends);
                                            if outcome.gag {
                                                continue;
                                            }
                                            SessionEvent::Line(text)
                                        }
                                        other => other,
                                    };
                                    if events.send(event).await.is_err() {
                                        return;
                                    }
                                }
                            }
                        }
                        if let Some(reason) = break_reason {
                            break reason;
                        }

                        // Trigger output is sent verbatim: it is never fed
                        // back through aliases, so rules cannot recurse.
                        if send_lines(&mut writer, &outbound).await.is_err() {
                            break "write failed".to_string();
                        }
                        if flush_telnet(&mut telnet, &mut writer).await.is_err() {
                            break "write failed".to_string();
                        }
                    }
                    Err(err) => break format!("connection error: {err}"),
                }
            }
            _ = tokio::time::sleep_until(timer_deadline.into()) => {
                let due = engine.fire_due_timers(Instant::now());
                if send_lines(&mut writer, &due).await.is_err() {
                    break "write failed".to_string();
                }
            }
            cmd = commands.recv() => {
                match cmd {
                    Some(SessionCommand::SendLine(line)) => {
                        let expanded = engine.expand_input(&line);
                        if send_lines(&mut writer, &expanded).await.is_err() {
                            break "write failed".to_string();
                        }
                    }
                    Some(SessionCommand::SetRules(rules)) => {
                        engine = *rules;
                        engine.start_timers(Instant::now());
                    }
                    Some(SessionCommand::Resize { cols, rows }) => {
                        telnet.set_window_size(cols, rows);
                        if flush_telnet(&mut telnet, &mut writer).await.is_err() {
                            break "write failed".to_string();
                        }
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

/// Write each command as its own CRLF-terminated line.
async fn send_lines<W>(writer: &mut W, lines: &[String]) -> std::io::Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    for line in lines {
        writer.write_all(line.as_bytes()).await?;
        writer.write_all(b"\r\n").await?;
    }
    Ok(())
}

/// Write any negotiation replies the Telnet machine has queued.
async fn flush_telnet<W>(telnet: &mut TelnetMachine, writer: &mut W) -> std::io::Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    let out = telnet.take_output();
    if out.is_empty() {
        return Ok(());
    }
    writer.write_all(&out).await
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

/// Raw inbound capture (`--record`). One line per socket read:
/// `<elapsed_ms> <hex>`, so any real-MUD quirk replays as a byte fixture
/// (docs/ARCHITECTURE.md §12).
struct Recorder {
    writer: std::io::BufWriter<std::fs::File>,
    start: Instant,
}

impl Recorder {
    fn create(path: &Path, host: &str, port: u16) -> std::io::Result<Self> {
        let file = std::fs::File::create(path)?;
        let mut writer = std::io::BufWriter::new(file);
        writeln!(writer, "# mudular raw capture of {host}:{port}")?;
        Ok(Self {
            writer,
            start: Instant::now(),
        })
    }

    fn record(&mut self, bytes: &[u8]) {
        let millis = self.start.elapsed().as_millis();
        let mut line = String::with_capacity(bytes.len() * 2 + 8);
        for byte in bytes {
            use std::fmt::Write as _;
            let _ = write!(line, "{byte:02x}");
        }
        // A capture that cannot be written is a debugging aid failing, not
        // a reason to drop the player's session.
        let _ = writeln!(self.writer, "{millis} {line}");
        let _ = self.writer.flush();
    }
}

/// Decodes inbound bytes per the profile's configured charset (§9.2). UTF-8
/// needs incremental byte-boundary state; the legacy charsets are
/// single-byte, so every byte maps to exactly one `char` independently.
enum TextDecoder {
    Utf8(Utf8Decoder),
    SingleByte(Charset),
}

impl TextDecoder {
    fn new(charset: Charset) -> Self {
        match charset {
            Charset::Utf8 => TextDecoder::Utf8(Utf8Decoder::default()),
            other => TextDecoder::SingleByte(other),
        }
    }

    fn decode(&mut self, bytes: &[u8]) -> String {
        match self {
            TextDecoder::Utf8(decoder) => decoder.decode(bytes),
            TextDecoder::SingleByte(charset) => {
                bytes.iter().map(|&b| charset.decode_byte(b)).collect()
            }
        }
    }
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

    const IAC: u8 = 255;
    const DO: u8 = 253;
    const WILL: u8 = 251;
    const WONT: u8 = 252;
    const SB: u8 = 250;
    const SE: u8 = 240;
    const GA: u8 = 249;

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
        let (mut events, _commands) = serve(|mut sock| async move {
            sock.write_all(b"Alignment:  \r[    0]  Can't decide to be good or evil.\r\n")
                .await
                .unwrap();
        });

        assert_eq!(
            next_line(&mut events).await,
            "Alignment:  [    0]  Can't decide to be good or evil.",
            "a bare \\r must not reach the rendered line"
        );
    }

    /// The M0 limitation this milestone removes: a prompt delimited by GA
    /// is consumed at the boundary, so the next burst of output starts a
    /// fresh line instead of being glued onto the prompt.
    #[tokio::test]
    async fn ga_delimited_prompt_does_not_swallow_the_next_line() {
        let (mut events, _commands) = serve(|mut sock| async move {
            sock.write_all(b"Login: ").await.unwrap();
            sock.write_all(&[IAC, GA]).await.unwrap();
            sock.write_all(b"Hello, Kestrel!\r\n").await.unwrap();
        });

        assert_eq!(
            next_prompt(&mut events).await,
            "Login: ",
            "GA confirms the pending text is a prompt"
        );
        assert_eq!(
            next_line(&mut events).await,
            "Hello, Kestrel!",
            "output after the boundary is its own line"
        );
    }

    #[tokio::test]
    async fn masks_input_while_the_server_echoes() {
        let (mut events, commands) = serve(|mut sock| async move {
            sock.write_all(b"Password: ").await.unwrap();
            sock.write_all(&[IAC, WILL, 1]).await.unwrap();
            // Unmask only once the player has actually submitted a line.
            assert_eq!(read_command(&mut sock).await, "hunter2");
            sock.write_all(&[IAC, WONT, 1]).await.unwrap();
            sock.write_all(b"\r\nWelcome.\r\n").await.unwrap();
        });

        assert!(next_mask(&mut events).await, "server WILL ECHO masks");

        commands
            .send(SessionCommand::SendLine("hunter2".into()))
            .await
            .unwrap();

        assert!(
            !next_mask(&mut events).await,
            "server WONT ECHO unmasks once the password is submitted"
        );
    }

    #[tokio::test]
    async fn offers_naws_and_reports_the_pane_size() {
        let (tx, mut rx) = mpsc::channel(8);
        let (_events, commands) = serve(move |mut sock| async move {
            let mut buf = vec![0u8; 64];
            // The client offers NAWS unprompted on connect.
            let n = sock.read(&mut buf).await.unwrap();
            tx.send(buf[..n].to_vec()).await.unwrap();

            sock.write_all(&[IAC, DO, 31]).await.unwrap();
            let n = sock.read(&mut buf).await.unwrap();
            tx.send(buf[..n].to_vec()).await.unwrap();
        });

        assert_eq!(rx.recv().await.unwrap(), vec![IAC, WILL, 31]);

        commands
            .send(SessionCommand::Resize {
                cols: 100,
                rows: 40,
            })
            .await
            .unwrap();

        let naws = timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for NAWS")
            .unwrap();
        assert_eq!(naws, vec![IAC, SB, 31, 0, 100, 0, 40, IAC, SE]);
    }

    #[tokio::test]
    async fn records_raw_inbound_bytes_for_replay() {
        let dir = std::env::temp_dir().join(format!("mudular-record-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("capture.txt");

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            sock.write_all(b"hi\r\n").await.unwrap();
        });

        let (mut events, _commands) = spawn(
            "127.0.0.1".to_string(),
            port,
            None,
            Some(path.clone()),
            Charset::Utf8,
            Engine::default(),
        );
        assert_eq!(next_line(&mut events).await, "hi");

        let captured = std::fs::read_to_string(&path).unwrap();
        let data_line = captured
            .lines()
            .find(|line| !line.starts_with('#'))
            .expect("a capture line");
        let (_millis, hex) = data_line.split_once(' ').expect("<ms> <hex>");
        assert_eq!(hex, "68690d0a", "raw bytes, before \\r is stripped");

        std::fs::remove_dir_all(&dir).ok();
    }

    fn rules(yaml: &str) -> Engine {
        let module = serde_yaml::from_str(yaml).expect("valid test YAML");
        Engine::compile(&[module]).expect("compiles")
    }

    /// A trigger fires against real server output and its command reaches
    /// the server — the whole point of the automation engine.
    #[tokio::test]
    async fn a_trigger_answers_the_server_automatically() {
        let (tx, mut sent) = mpsc::channel(8);
        let (_events, _commands) = serve_with_rules(
            move |mut sock| async move {
                sock.write_all(b"The kobold is DEAD!\r\n").await.unwrap();
                tx.send(read_command(&mut sock).await).await.unwrap();
            },
            rules(
                r#"
                name: test
                triggers:
                  - pattern: 'is DEAD!'
                    send: ["get all corpse"]
                "#,
            ),
        );

        assert_eq!(
            timeout(Duration::from_secs(2), sent.recv())
                .await
                .expect("timed out")
                .unwrap(),
            "get all corpse"
        );
    }

    /// Triggers match the ANSI-stripped projection (§7.1), so a pattern
    /// does not have to know the server coloured the line.
    #[tokio::test]
    async fn a_trigger_matches_through_ansi_colour() {
        let (tx, mut sent) = mpsc::channel(8);
        let (_events, _commands) = serve_with_rules(
            move |mut sock| async move {
                sock.write_all(b"\x1b[31mThe \x1b[1mkobold\x1b[0m is DEAD!\r\n")
                    .await
                    .unwrap();
                tx.send(read_command(&mut sock).await).await.unwrap();
            },
            rules(
                r#"
                name: test
                triggers:
                  - pattern: '^The kobold is DEAD!$'
                    send: ["cheer"]
                "#,
            ),
        );

        assert_eq!(
            timeout(Duration::from_secs(2), sent.recv())
                .await
                .expect("timed out")
                .unwrap(),
            "cheer"
        );
    }

    /// A gagged line is dropped before the UI sees it, so it never reaches
    /// the scrollback at all (§6.5).
    #[tokio::test]
    async fn a_gagged_line_never_reaches_the_ui() {
        let (mut events, _commands) = serve_with_rules(
            |mut sock| async move {
                sock.write_all(b"channel spam here\r\nsomething real\r\n")
                    .await
                    .unwrap();
            },
            rules(
                r#"
                name: test
                triggers:
                  - pattern: 'spam'
                    gag: true
                "#,
            ),
        );

        assert_eq!(
            next_line(&mut events).await,
            "something real",
            "the gagged line must not be delivered"
        );
    }

    /// Typed input is expanded by the session, not the UI: one line can
    /// become several commands.
    #[tokio::test]
    async fn typed_input_is_alias_expanded_and_split_on_semicolons() {
        let (tx, mut sent) = mpsc::channel(8);
        let (_events, commands) = serve_with_rules(
            move |mut sock| async move {
                let mut reader = CommandReader::default();
                for _ in 0..3 {
                    tx.send(reader.next(&mut sock).await).await.unwrap();
                }
            },
            rules(
                r#"
                name: test
                variables:
                  target: rat
                aliases:
                  - pattern: '^k$'
                    send: ["kill ${target}"]
                "#,
            ),
        );

        commands
            .send(SessionCommand::SendLine("north; k".into()))
            .await
            .unwrap();

        assert_eq!(next_sent(&mut sent).await, "north");
        assert_eq!(next_sent(&mut sent).await, "kill rat");

        // A trigger's `set:` updates the variable the alias reads.
        commands
            .send(SessionCommand::SendLine("k".into()))
            .await
            .unwrap();
        assert_eq!(next_sent(&mut sent).await, "kill rat");
    }

    /// `/reload` swaps the rule set on a live session (§7.3).
    #[tokio::test]
    async fn set_rules_replaces_the_rule_set_without_reconnecting() {
        let (tx, mut sent) = mpsc::channel(8);
        let (_events, commands) = serve_with_rules(
            move |mut sock| async move {
                let mut reader = CommandReader::default();
                for _ in 0..2 {
                    tx.send(reader.next(&mut sock).await).await.unwrap();
                }
            },
            rules(
                r#"
                name: before
                aliases:
                  - pattern: '^x$'
                    send: ["old"]
                "#,
            ),
        );

        commands
            .send(SessionCommand::SendLine("x".into()))
            .await
            .unwrap();
        assert_eq!(next_sent(&mut sent).await, "old");

        commands
            .send(SessionCommand::SetRules(Box::new(rules(
                r#"
                name: after
                aliases:
                  - pattern: '^x$'
                    send: ["new"]
                "#,
            ))))
            .await
            .unwrap();

        commands
            .send(SessionCommand::SendLine("x".into()))
            .await
            .unwrap();
        assert_eq!(next_sent(&mut sent).await, "new");
    }

    #[tokio::test]
    async fn timers_fire_on_their_own_without_server_traffic() {
        let (tx, mut sent) = mpsc::channel(8);
        let (_events, _commands) = serve_with_rules(
            move |mut sock| async move {
                tx.send(read_command(&mut sock).await).await.unwrap();
            },
            rules(
                r#"
                name: test
                timers:
                  - every: 50ms
                    send: ["save"]
                "#,
            ),
        );

        assert_eq!(next_sent(&mut sent).await, "save");
    }

    async fn next_sent(rx: &mut mpsc::Receiver<String>) -> String {
        timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for a sent command")
            .expect("sender dropped")
    }

    /// Reads a fake MUD server's minimal login/echo/quit script against a
    /// real session task over a real loopback socket — the in-process fake
    /// server integration test called for in docs/ARCHITECTURE.md §12.
    #[tokio::test]
    async fn walks_a_login_echo_quit_session() {
        let (mut events, commands) = serve(|mut sock| async move {
            sock.write_all(b"\x1b[1;33mWelcome to FakeMUD\x1b[0m\r\n\r\nLogin: ")
                .await
                .unwrap();

            let name = read_command(&mut sock).await;
            sock.write_all(format!("Hello, {name}!\r\n> ").as_bytes())
                .await
                .unwrap();

            assert_eq!(read_command(&mut sock).await, "quit");
            sock.write_all(b"Bye!\r\n").await.unwrap();
        });

        assert_eq!(
            next_line(&mut events).await,
            "\x1b[1;33mWelcome to FakeMUD\x1b[0m"
        );
        assert_eq!(next_line(&mut events).await, "");
        assert_eq!(next_prompt(&mut events).await, "Login: ");

        commands
            .send(SessionCommand::SendLine("Kestrel".into()))
            .await
            .unwrap();

        // This server sends no GA/EOR, so the prompt is only provisional
        // and still runs into the following output (§6.2) — the reason
        // boundary-aware servers get the better behaviour above.
        assert_eq!(next_line(&mut events).await, "Login: Hello, Kestrel!");
        assert_eq!(next_prompt(&mut events).await, "> ");

        commands
            .send(SessionCommand::SendLine("quit".into()))
            .await
            .unwrap();

        assert_eq!(next_line(&mut events).await, "> Bye!");
        assert!(matches!(
            next_matching(&mut events, |ev| matches!(ev, SessionEvent::Ended(_))).await,
            SessionEvent::Ended(_)
        ));
    }

    /// M5's acceptance criterion (§14): the same scripted session played
    /// twice — once in clear Telnet, once MCCP2-compressed with the
    /// switchover happening mid-read — must reach the UI identically.
    #[tokio::test]
    async fn a_compressed_session_is_identical_to_the_uncompressed_one() {
        // Telnet framing inside the compressed stream — an option
        // negotiation and a GA prompt boundary — proves the stages stay
        // ordered: compression wraps Telnet, not the reverse (§6.5).
        let body: Vec<u8> = [
            b"\x1b[1;33mThe Grand Bazaar\x1b[0m\r\nA merchant waves.\r\n".as_slice(),
            &[IAC, WILL, option::ECHO],
            b"Password: ",
            &[IAC, GA],
        ]
        .concat();

        let plain = drain(serve({
            let body = body.clone();
            move |mut sock| async move {
                sock.write_all(b"Welcome\r\n").await.unwrap();
                sock.write_all(&body).await.unwrap();
                idle(&mut sock).await;
            }
        }))
        .await;

        let compressed = drain(serve({
            let body = body.clone();
            move |mut sock| async move {
                sock.write_all(b"Welcome\r\n").await.unwrap();
                sock.write_all(&[IAC, WILL, option::MCCP2]).await.unwrap();
                await_agreement(&mut sock, option::MCCP2).await;

                // The subnegotiation and the first compressed bytes share
                // one write: this is the mid-buffer switchover (§6.4).
                let deflated = deflate(&body);
                let (head, tail) = deflated.split_at(deflated.len() / 2);
                let mut first = vec![IAC, SB, option::MCCP2, IAC, SE];
                first.extend_from_slice(head);
                sock.write_all(&first).await.unwrap();
                sock.write_all(tail).await.unwrap();
                idle(&mut sock).await;
            }
        }))
        .await;

        let plain = transcript(&plain);
        assert_eq!(
            transcript(&compressed),
            plain,
            "compressed session diverged from the fixture"
        );
        assert_eq!(
            plain,
            vec![
                SessionEvent::Line("Welcome".into()),
                SessionEvent::Line("\x1b[1;33mThe Grand Bazaar\x1b[0m".into()),
                SessionEvent::Line("A merchant waves.".into()),
                SessionEvent::EchoMask(true),
                SessionEvent::Prompt("Password: ".into()),
            ],
            "the fixture itself must carry lines, a negotiation and a prompt"
        );
    }

    /// Starts MCCP2 the way a real server does — offer, wait for our `DO`,
    /// then the subnegotiation with `payload` sharing the same write.
    async fn start_compression(sock: &mut tokio::net::TcpStream, payload: &[u8]) {
        sock.write_all(&[IAC, WILL, option::MCCP2]).await.unwrap();
        await_agreement(sock, option::MCCP2).await;
        let mut first = vec![IAC, SB, option::MCCP2, IAC, SE];
        first.extend_from_slice(payload);
        sock.write_all(&first).await.unwrap();
    }

    /// Writes `bytes` far enough after the previous write that the client
    /// sees them as their own socket read. Without the pause TCP is free to
    /// coalesce the two, and the read boundary under test never happens.
    async fn separate_read(sock: &mut tokio::net::TcpStream, bytes: &[u8]) {
        tokio::time::sleep(Duration::from_millis(50)).await;
        sock.write_all(bytes).await.unwrap();
    }

    /// `Z_STREAM_END` mid-read: the server ended compression cleanly and
    /// the rest of that same read is plain Telnet again (§6.4).
    #[tokio::test]
    async fn a_finished_stream_returns_the_session_to_plain_text() {
        let (mut events, _commands) = serve(|mut sock| async move {
            let mut payload = deflate_finished(b"compressed line\r\n");
            payload.extend_from_slice(b"plain in the same read\r\n");
            start_compression(&mut sock, &payload).await;
            // A later read must still be plain: the decoder is gone, not
            // merely bypassed for the remainder of the ending read.
            separate_read(&mut sock, b"plain in a later read\r\n").await;
            idle(&mut sock).await;
        });

        assert_eq!(next_line(&mut events).await, "compressed line");
        assert_eq!(
            next_line(&mut events).await,
            "plain in the same read",
            "bytes past Z_STREAM_END are plain Telnet, not zlib"
        );
        assert_eq!(next_line(&mut events).await, "plain in a later read");
    }

    /// A server may end one zlib stream and start another. The second
    /// subnegotiation arrives *inside* the plain tail of the first, so the
    /// session has to re-enter the inflate stage with a fresh decoder.
    #[tokio::test]
    async fn a_second_compressed_stream_starts_after_the_first_one_ends() {
        let (mut events, _commands) = serve(|mut sock| async move {
            start_compression(&mut sock, &deflate_finished(b"first stream\r\n")).await;

            let mut restart = b"between streams\r\n".to_vec();
            restart.extend_from_slice(&[IAC, SB, option::MCCP2, IAC, SE]);
            restart.extend_from_slice(&deflate(b"second stream\r\n"));
            separate_read(&mut sock, &restart).await;
            idle(&mut sock).await;
        });

        assert_eq!(next_line(&mut events).await, "first stream");
        assert_eq!(next_line(&mut events).await, "between streams");
        assert_eq!(
            next_line(&mut events).await,
            "second stream",
            "the restarted stream must decode against a fresh decoder"
        );
    }

    /// A corrupt stream cannot be recovered from — the session ends with
    /// the inflate error as its reason rather than emitting garbage.
    #[tokio::test]
    async fn a_corrupt_compressed_stream_ends_the_session() {
        let (mut events, _commands) = serve(|mut sock| async move {
            start_compression(&mut sock, b"\x78\x9c not a deflate block at all").await;
            idle(&mut sock).await;
        });

        let ended = next_matching(&mut events, |ev| matches!(ev, SessionEvent::Ended(_))).await;
        let SessionEvent::Ended(reason) = ended else {
            unreachable!()
        };
        assert!(
            reason.contains("compressed stream is corrupt"),
            "unexpected end reason: {reason}"
        );
    }

    /// zlib state has to survive socket read boundaries: half a block
    /// decodes to nothing on its own and must be held, not dropped.
    #[tokio::test]
    async fn a_compressed_block_split_across_two_reads_is_reassembled() {
        let (mut events, _commands) = serve(|mut sock| async move {
            let deflated = deflate(b"a line long enough to be split in half\r\n");
            let (head, tail) = deflated.split_at(deflated.len() / 2);
            start_compression(&mut sock, head).await;
            separate_read(&mut sock, tail).await;
            idle(&mut sock).await;
        });

        assert_eq!(
            next_line(&mut events).await,
            "a line long enough to be split in half"
        );
    }

    /// What the pane ends up showing: completed lines, echo state, and the
    /// pinned prompt. Provisional prompt updates are dropped — how many of
    /// them appear depends on where TCP splits the reads, which is an
    /// arrival artifact rather than a difference in the decoded stream.
    fn transcript(events: &[SessionEvent]) -> Vec<SessionEvent> {
        let mut out: Vec<SessionEvent> = events
            .iter()
            .filter(|ev| matches!(ev, SessionEvent::Line(_) | SessionEvent::EchoMask(_)))
            .cloned()
            .collect();
        if let Some(prompt) = events
            .iter()
            .rev()
            .find(|ev| matches!(ev, SessionEvent::Prompt(text) if !text.is_empty()))
        {
            out.push(prompt.clone());
        }
        out
    }

    /// Reads until the client agrees to `option`, so the server only
    /// starts compressing once the handshake is complete.
    async fn await_agreement(sock: &mut tokio::net::TcpStream, option: u8) {
        let mut seen = Vec::new();
        let mut buf = vec![0u8; 64];
        loop {
            let n = sock.read(&mut buf).await.unwrap();
            assert!(n > 0, "client closed before agreeing to option {option}");
            seen.extend_from_slice(&buf[..n]);
            if seen.windows(3).any(|w| w == [IAC, DO, option]) {
                return;
            }
        }
    }

    /// Holds the connection open and drains the client's replies: dropping
    /// a socket with unread data pending sends an RST, which discards
    /// output the client has not read yet and makes the fixture flaky.
    async fn idle(sock: &mut tokio::net::TcpStream) {
        let mut sink = vec![0u8; 256];
        while sock.read(&mut sink).await.unwrap_or(0) > 0 {}
    }

    /// zlib-compress as one flushed block, the way an MCCP2 server does.
    fn deflate(plain: &[u8]) -> Vec<u8> {
        compress_with(plain, flate2::FlushCompress::Sync)
    }

    /// As `deflate`, but ending the zlib stream (`Z_STREAM_END`) — how a
    /// server turns compression back off.
    fn deflate_finished(plain: &[u8]) -> Vec<u8> {
        compress_with(plain, flate2::FlushCompress::Finish)
    }

    fn compress_with(plain: &[u8], flush: flate2::FlushCompress) -> Vec<u8> {
        let mut c = flate2::Compress::new(flate2::Compression::default(), true);
        let mut out = vec![0u8; plain.len() + 128];
        c.compress(plain, &mut out, flush).unwrap();
        out.truncate(c.total_out() as usize);
        out
    }

    /// Collects a session's events until the server goes quiet. The two
    /// scripts stay connected afterwards, so `Ended` is not the terminator
    /// here — and its reason would differ between them anyway.
    async fn drain(
        channels: (mpsc::Receiver<SessionEvent>, mpsc::Sender<SessionCommand>),
    ) -> Vec<SessionEvent> {
        let (mut events, _commands) = channels;
        let mut seen = Vec::new();
        while let Ok(Some(event)) = timeout(Duration::from_millis(250), events.recv()).await {
            seen.push(event);
        }
        seen
    }

    /// Strips Telnet framing from client→server bytes, the way any real
    /// server does before treating input as a typed command. Without this
    /// a fake server mistakes our NAWS offer for the player's first line.
    fn strip_telnet(bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut iter = bytes.iter().copied().peekable();
        while let Some(byte) = iter.next() {
            if byte != IAC {
                out.push(byte);
                continue;
            }
            match iter.next() {
                Some(IAC) => out.push(IAC),
                Some(SB) => {
                    // Skip through the terminating IAC SE.
                    while let Some(b) = iter.next() {
                        if b == IAC && iter.peek() == Some(&SE) {
                            iter.next();
                            break;
                        }
                    }
                }
                // WILL/WONT/DO/DONT carry one option byte.
                Some(WILL | WONT | DO | 254) => {
                    iter.next();
                }
                _ => {}
            }
        }
        out
    }

    /// Reads until the client sends a complete line of actual input.
    ///
    /// Several commands can share one TCP segment (an alias expands to a
    /// list, `;` splits a typed line), so leftover bytes are kept for the
    /// next call rather than discarded.
    #[derive(Default)]
    struct CommandReader {
        pending: Vec<u8>,
    }

    impl CommandReader {
        async fn next(&mut self, sock: &mut tokio::net::TcpStream) -> String {
            let mut buf = vec![0u8; 1024];
            loop {
                if let Some(idx) = self.pending.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = self.pending.drain(..=idx).collect();
                    return String::from_utf8_lossy(&line).trim().to_string();
                }
                let n = sock.read(&mut buf).await.unwrap();
                if n == 0 {
                    return String::new();
                }
                self.pending.extend_from_slice(&strip_telnet(&buf[..n]));
            }
        }
    }

    /// One-shot read of a single command, for scripts that only need one.
    async fn read_command(sock: &mut tokio::net::TcpStream) -> String {
        CommandReader::default().next(sock).await
    }

    /// Runs `script` against one loopback connection and returns the
    /// session's channels.
    fn serve<F, Fut>(script: F) -> (mpsc::Receiver<SessionEvent>, mpsc::Sender<SessionCommand>)
    where
        F: FnOnce(tokio::net::TcpStream) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        serve_with_rules(script, Engine::default())
    }

    /// As `serve`, but with a compiled rule set driving the session.
    fn serve_with_rules<F, Fut>(
        script: F,
        engine: Engine,
    ) -> (mpsc::Receiver<SessionEvent>, mpsc::Sender<SessionCommand>)
    where
        F: FnOnce(tokio::net::TcpStream) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            let listener = TcpListener::from_std(listener).unwrap();
            let (sock, _) = listener.accept().await.unwrap();
            script(sock).await;
        });

        spawn(
            "127.0.0.1".to_string(),
            port,
            None,
            None,
            Charset::Utf8,
            engine,
        )
    }

    async fn next_matching(
        events: &mut mpsc::Receiver<SessionEvent>,
        want: impl Fn(&SessionEvent) -> bool,
    ) -> SessionEvent {
        loop {
            let event = timeout(Duration::from_secs(2), events.recv())
                .await
                .expect("timed out waiting for session event")
                .expect("session event stream ended early");
            if want(&event) {
                return event;
            }
        }
    }

    async fn next_line(events: &mut mpsc::Receiver<SessionEvent>) -> String {
        match next_matching(events, |ev| matches!(ev, SessionEvent::Line(_))).await {
            SessionEvent::Line(line) => line,
            _ => unreachable!(),
        }
    }

    /// Skips the empty prompts that follow completed lines.
    async fn next_prompt(events: &mut mpsc::Receiver<SessionEvent>) -> String {
        match next_matching(
            events,
            |ev| matches!(ev, SessionEvent::Prompt(text) if !text.is_empty()),
        )
        .await
        {
            SessionEvent::Prompt(text) => text,
            _ => unreachable!(),
        }
    }

    async fn next_mask(events: &mut mpsc::Receiver<SessionEvent>) -> bool {
        match next_matching(events, |ev| matches!(ev, SessionEvent::EchoMask(_))).await {
            SessionEvent::EchoMask(masked) => masked,
            _ => unreachable!(),
        }
    }
}
