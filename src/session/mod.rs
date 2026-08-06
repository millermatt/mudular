//! Per-character session pipeline.
//!
//! One tokio task per session owns its transport, decompressor, Telnet
//! machine, charset decoder, automation engine, and scrollback — nothing is
//! shared between sessions (docs/ARCHITECTURE.md §3). M1 wires:
//! TCP → Telnet FSM (with RFC 1143 negotiation) → UTF-8 decode → line
//! assembler. MCCP inflation and charset fallback slot in ahead of the
//! decoder in M5/M3.

mod line;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::net::{self, TlsConfig};
use crate::proto::telnet::{Side, TelnetEvent, TelnetMachine, option};
use line::LineAssembler;

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
    tls: Option<TlsConfig>,
    record: Option<PathBuf>,
) -> (mpsc::Receiver<SessionEvent>, mpsc::Sender<SessionCommand>) {
    let (event_tx, event_rx) = mpsc::channel(256);
    let (cmd_tx, cmd_rx) = mpsc::channel(256);
    tokio::spawn(run(host, port, tls, record, event_tx, cmd_rx));
    (event_rx, cmd_tx)
}

async fn run(
    host: String,
    port: u16,
    tls: Option<TlsConfig>,
    record: Option<PathBuf>,
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
    let mut decoder = Utf8Decoder::default();
    let mut assembler = LineAssembler::default();
    let mut sock_buf = [0u8; 4096];

    // Offer NAWS up front; the size follows once the server agrees (§6.2).
    telnet.request_local_enable(option::NAWS);
    if flush_telnet(&mut telnet, &mut writer).await.is_err() {
        let _ = events
            .send(SessionEvent::Ended("write failed".to_string()))
            .await;
        return;
    }

    let reason = loop {
        tokio::select! {
            result = reader.read(&mut sock_buf) => {
                match result {
                    Ok(0) => break "connection closed".to_string(),
                    Ok(n) => {
                        let raw = &sock_buf[..n];
                        if let Some(recorder) = recorder.as_mut() {
                            recorder.record(raw);
                        }

                        for event in telnet.feed(raw) {
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
                                // Other options are handled inside the
                                // Telnet machine; subnegotiations reach the
                                // engine in M6.
                                _ => Vec::new(),
                            };
                            for event in emitted {
                                if events.send(event).await.is_err() {
                                    return;
                                }
                            }
                        }

                        if flush_telnet(&mut telnet, &mut writer).await.is_err() {
                            break "write failed".to_string();
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

        let (mut events, _commands) =
            spawn("127.0.0.1".to_string(), port, None, Some(path.clone()));
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
    async fn read_command(sock: &mut tokio::net::TcpStream) -> String {
        let mut buf = vec![0u8; 1024];
        let mut acc = Vec::new();
        loop {
            let n = sock.read(&mut buf).await.unwrap();
            if n == 0 {
                return String::new();
            }
            acc.extend_from_slice(&buf[..n]);
            let text = strip_telnet(&acc);
            if let Some(idx) = text.iter().position(|&b| b == b'\n') {
                return String::from_utf8_lossy(&text[..idx]).trim().to_string();
            }
        }
    }

    /// Runs `script` against one loopback connection and returns the
    /// session's channels.
    fn serve<F, Fut>(script: F) -> (mpsc::Receiver<SessionEvent>, mpsc::Sender<SessionCommand>)
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

        spawn("127.0.0.1".to_string(), port, None, None)
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
