//! Per-character session pipeline.
//!
//! One tokio task per session owns its transport, decompressor, Telnet
//! machine, charset decoder, automation engine, and scrollback — nothing is
//! shared between sessions (docs/ARCHITECTURE.md §3). The inbound pipeline
//! is §6.5: TCP → MCCP inflate → Telnet FSM (with RFC 1143 negotiation) →
//! charset decode → line assembler → trigger engine → UI events.

mod line;
pub mod login;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, watch};

use crate::engine::{Engine, PeerSnapshot, Peers};
use crate::net::{self, TlsConfig};
use crate::proto::Flattened;
use crate::proto::charset::Charset;
use crate::proto::gmcp;
use crate::proto::mccp::MccpDecoder;
use crate::proto::msdp;
use crate::proto::telnet::{Side, TelnetEvent, TelnetMachine, encode_subnegotiation, option};
use crate::scrollback::{Origin, strip_ansi, strip_unsafe_controls};
use line::{LineAssembler, apply_highlights};
use login::{Autologin, LoginAction};

/// Session → UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// A completed output line (ANSI styling preserved), and who produced
    /// it. Not every `Line` is the server talking: a trigger's `echo:`, an
    /// auto-login notice and another session's injected command all reach a
    /// pane the same way, and the pane cannot tell them apart afterwards
    /// unless the event says so (docs/ARCHITECTURE.md §8).
    Line { text: String, origin: Origin },
    /// A line a rule routed to a channel pane (docs/ARCHITECTURE.md §11.1).
    /// Emitted alongside `Line` when the channel keeps lines in main.
    Route { channel: String, text: String },
    /// Commands this session's rules want another session to run. The hub
    /// resolves `target` (a session name, or `*` for all others) and
    /// delivers them as [`SessionCommand::Inject`] (§7.5).
    SendTo {
        target: String,
        lines: Vec<String>,
        /// Which hop this delivery would be; 1 for a locally-originated
        /// action, one more for each further bounce.
        hops: u8,
    },
    /// Text a script asked another session's pane to show (§7.5). The hub
    /// resolves `target` as for [`SessionEvent::SendTo`] and writes it
    /// straight to that pane: nothing is sent to any server.
    EchoTo { target: String, text: String },
    /// The text that should sit above the input line. Empty means none.
    Prompt(String),
    /// Server asked us to mask/unmask local input (Telnet ECHO).
    EchoMask(bool),
    /// A trigger's `bell:` fired on this line (§14 M9). The hub rings the
    /// terminal bell / desktop notification only if the pane isn't
    /// focused — the session has no notion of focus to decide that itself.
    Bell,
    /// A trigger's `corpse:` fired on this line: the character has just
    /// died (§16). Carries no room, because the session is not what tracks
    /// which one the character is in — the hub is, and it takes its answer
    /// from the room it holds *now*, before the [`SessionEvent::Room`] for
    /// wherever death sends them arrives behind this in stream order.
    Corpse,
    /// A trigger's `mark:` fired: label the room the character is in (§16).
    /// Carries no room for the same reason [`SessionEvent::Corpse`] does —
    /// the hub is what knows which one that is.
    Mark(String),
    /// What the transport is trusting, once connected.
    Security(net::Security),
    /// A raw GMCP message, for the inspector view (§14 M6, §6.3).
    Gmcp {
        package: String,
        payload: Option<String>,
    },
    /// The MSDP twin of [`SessionEvent::Gmcp`], for the same inspector view
    /// (§6.3). MSDP's wire form is `VAR`/`VAL` control-byte framing, not
    /// text, so there is no literal payload worth showing raw — the
    /// flattened key/value pairs are shown instead, which are both
    /// readable and exactly what `update_server_data_from_msdp` stored, so
    /// the inspector shows what a trigger could actually read.
    Msdp { pairs: Vec<(String, String)> },
    /// The character is somewhere new (§16). Raised for whichever protocol
    /// supplied it — the room is read back out of the merged server-data
    /// store, not out of a GMCP message, so an MSDP-only MUD maps just as
    /// well as a GMCP one (§6.3, and §7.5's reasoning about naming a key
    /// and not a protocol).
    ///
    /// `arrived_via` is the movement that got us here, when the client can
    /// be *sure* which one it was — see [`note_outbound`]. `None` means the
    /// room is known but the edge into it is not, which is the honest
    /// answer: a wrong edge is a lie the map keeps telling, where a missing
    /// one is only a gap.
    Room {
        info: Box<crate::map::RoomInfo>,
        arrived_via: Option<String>,
    },
    /// The connection dropped and the session is waiting to try again
    /// (docs/ARCHITECTURE.md §5). `attempt` counts from 1 and resets once a
    /// connection comes back up.
    Reconnecting {
        attempt: u32,
        delay: Duration,
        reason: String,
    },
    /// How long the earliest outstanding round trip took: the gap between a
    /// write reaching the socket and the next data coming back (§11). It is
    /// a heuristic, not a protocol probe, so it includes however long the
    /// server took to think — which is the wait a player actually feels.
    Latency(Duration),
    /// The session terminated; the pane stays up showing the reason.
    Ended(String),
}

impl SessionEvent {
    /// The MUD said it.
    pub fn server(text: impl Into<String>) -> Self {
        Self::line(text, Origin::Server)
    }

    /// The player's rules or scripts said it (§7.1, §7.4).
    pub fn rule(text: impl Into<String>) -> Self {
        Self::line(text, Origin::Rule)
    }

    /// The client itself said it — auto-login progress, and the like.
    pub fn client(text: impl Into<String>) -> Self {
        Self::line(text, Origin::Client)
    }

    /// Another character's session put it here (§7.5).
    pub fn from_session(name: impl Into<String>, text: impl Into<String>) -> Self {
        Self::line(text, Origin::Session(name.into()))
    }

    fn line(text: impl Into<String>, origin: Origin) -> Self {
        Self::Line {
            text: text.into(),
            origin,
        }
    }
}

/// UI → session.
#[derive(Debug)]
pub enum SessionCommand {
    /// One line as typed. The session splits it on `;` and expands
    /// aliases: the engine owns the variable store that both aliases and
    /// triggers read, so it lives in the session task where that state
    /// needs no locking (docs/ARCHITECTURE.md §3).
    SendLine(String),
    /// Commands another session's rules asked this one to run (§7.5).
    /// Whether they run through this session's aliases is this session's
    /// own `cross_session.expand_aliases` setting — a sender can never
    /// force it.
    Inject {
        from: String,
        lines: Vec<String>,
        hops: u8,
    },
    /// Replace the rule set without reconnecting (`/reload`).
    SetRules(Box<Engine>),
    /// A session added to a running instance after this one, so this
    /// session's peer mesh — built once at spawn (§7.5) — has no other way
    /// to learn about it (`/connect`, docs/ARCH_REVIEW.md "One-way doors"
    /// #2). Handled identically whether it arrives connected or mid-
    /// reconnect-backoff: both keep a `watching` list, and a peer added
    /// during a backoff must not be lost by the time the connection comes
    /// back.
    AddPeer {
        name: String,
        rx: watch::Receiver<PeerSnapshot>,
    },
    /// Pane was resized; renegotiate NAWS.
    Resize { cols: u16, rows: u16 },
    /// End the session for good, cancelling any pending reconnect. No UI
    /// affordance sends it yet — per-pane connect/disconnect control is not
    /// part of any milestone so far — but it is what tells the retry loop
    /// the player is done, as against a connection that merely dropped.
    #[allow(dead_code)]
    Disconnect,
}

/// Connect and spawn the session task, returning its event stream and
/// command sink. The task runs until the connection ends or `Disconnect`
/// is received.
#[allow(clippy::too_many_arguments)]
/// A session's half of the peer-snapshot mesh (§7.5): the channel it
/// publishes its own state on, and a receiver for every other session's.
/// Empty by default — a lone character has no peers to watch.
#[derive(Default)]
pub struct PeerLinks {
    pub publish: Option<watch::Sender<PeerSnapshot>>,
    pub others: Peers,
}

#[allow(clippy::too_many_arguments)]
pub fn spawn(
    host: String,
    port: u16,
    tls: Option<TlsConfig>,
    record: Option<PathBuf>,
    charset: Charset,
    engine: Engine,
    expand_injected: bool,
    login: Option<Autologin>,
    peers: PeerLinks,
) -> (mpsc::Receiver<SessionEvent>, mpsc::Sender<SessionCommand>) {
    let (event_tx, event_rx) = mpsc::channel(256);
    let (cmd_tx, cmd_rx) = mpsc::channel(256);
    tokio::spawn(run(
        host,
        port,
        tls,
        record,
        charset,
        engine,
        expand_injected,
        login,
        peers,
        event_tx,
        cmd_rx,
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
    expand_injected: bool,
    mut login: Option<Autologin>,
    peers: PeerLinks,
    events: mpsc::Sender<SessionEvent>,
    mut commands: mpsc::Receiver<SessionCommand>,
) {
    // The engine reads peers on demand; the loop watches the same channels
    // so a peer's change can wake this session's `on_peer` scripts (§7.5).
    let mut watching: Vec<(String, watch::Receiver<PeerSnapshot>)> = peers
        .others
        .iter()
        .map(|(name, rx)| (name.clone(), rx.clone()))
        .collect();
    engine.set_peers(peers.others);
    let publish_to = peers.publish;

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

    // Pane size outlives any one connection: the UI only reports a size
    // that changed (§6.2), so a reconnect has to renegotiate NAWS with the
    // size it was last told rather than with none.
    let mut window: Option<(u16, u16)> = None;
    // Consecutive failures since the last connection came up.
    let mut attempt: u32 = 0;
    let mut established = false;
    let mut hints = FirstUseHints::default();

    let reason = loop {
        let lost = match net::connect(&host, port, tls.as_ref()).await {
            Ok(connection) => {
                established = true;
                attempt = 0;
                match run_connection(
                    connection,
                    charset,
                    &mut engine,
                    expand_injected,
                    &mut login,
                    &mut watching,
                    &publish_to,
                    &mut recorder,
                    &mut window,
                    &mut hints,
                    &events,
                    &mut commands,
                )
                .await
                {
                    Outcome::Gone => return,
                    Outcome::Ended(reason) => break reason,
                    Outcome::Lost(reason) => reason,
                }
            }
            // Nothing to re-establish yet: an address that never answered
            // is a mistake to report, not a server to wait for.
            Err(err) if !established => break format!("{err:#}"),
            Err(err) => format!("{err:#}"),
        };

        attempt += 1;
        let delay = backoff_delay(attempt);
        let notice = SessionEvent::Reconnecting {
            attempt,
            delay,
            reason: lost,
        };
        if events.send(notice).await.is_err() {
            return;
        }
        if !wait_to_retry(
            delay,
            &mut engine,
            &mut window,
            &mut watching,
            &mut commands,
        )
        .await
        {
            break "disconnected".to_string();
        }
    };

    let _ = events.send(SessionEvent::Ended(reason)).await;
}

/// Why one connection ended, and what the session should do next.
enum Outcome {
    /// The transport dropped under us: worth another attempt.
    Lost(String),
    /// The player ended it (or the UI hung up its command sink): no retry.
    Ended(String),
    /// The UI is no longer listening, so there is nothing left to serve.
    Gone,
}

/// First backoff step, and the ceiling the doubling stops at. A minute is
/// long enough that a MUD rebooting for half an hour costs a handful of
/// attempts, short enough that a player watching the pane sees it come back
/// (docs/ARCHITECTURE.md §5).
const RECONNECT_BASE: Duration = Duration::from_secs(1);
const RECONNECT_CAP: Duration = Duration::from_secs(60);

/// 1s, 2s, 4s … capped. `attempt` counts from 1.
fn backoff_delay(attempt: u32) -> Duration {
    let doublings = attempt.saturating_sub(1).min(16);
    RECONNECT_BASE
        .saturating_mul(1u32 << doublings)
        .min(RECONNECT_CAP)
}

/// Sleeps out the backoff. A retry that could not be called off would
/// outlive the pane, so the wait still serves commands — the two that mean
/// anything without a socket, plus the `Disconnect` (or hung-up sink) that
/// cancels it. Returns whether to go on and reconnect.
async fn wait_to_retry(
    delay: Duration,
    engine: &mut Engine,
    window: &mut Option<(u16, u16)>,
    watching: &mut Vec<(String, watch::Receiver<PeerSnapshot>)>,
    commands: &mut mpsc::Receiver<SessionCommand>,
) -> bool {
    let deadline = tokio::time::Instant::now() + delay;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return true,
            cmd = commands.recv() => match cmd {
                Some(SessionCommand::Disconnect) | None => return false,
                // `/reload` still lands: the reconnect runs the new rules'
                // `on_connect` and starts their timers, as a reload against
                // a live socket does (§7.4).
                Some(SessionCommand::SetRules(rules)) => *engine = *rules,
                Some(SessionCommand::Resize { cols, rows }) => *window = Some((cols, rows)),
                // A peer added while this session is mid-backoff must not
                // be lost by the time the connection comes back (§7.5).
                Some(SessionCommand::AddPeer { name, rx }) => {
                    engine.add_peer(name.clone(), rx.clone());
                    watching.push((name, rx));
                }
                // Nothing to send anything down; the status line already
                // says why.
                Some(_) => {}
            },
        }
    }
}

/// One-time UX hints, shown at most once per *play session* rather than per
/// connection or per `Engine` (UX_REVIEW.md G): a reconnect or `/reload`
/// replaces the transport or the rule set, but the player hasn't started a
/// new session, so this lives in `run`'s scope and outlives both.
#[derive(Default)]
struct FirstUseHints {
    speedwalk_shown: bool,
    trigger_shown: bool,
}

/// Runs one connection to completion: everything from the Telnet handshake
/// to the disconnect hook. All the protocol state is created here, so each
/// reconnect starts from a clean machine (§6.5); the engine is not, so
/// aliases, variables and the rule set survive a reconnect the way they
/// survive a `/reload`.
#[allow(clippy::too_many_arguments)]
async fn run_connection(
    connection: net::Connection,
    charset: Charset,
    engine: &mut Engine,
    expand_injected: bool,
    login: &mut Option<Autologin>,
    watching: &mut Vec<(String, watch::Receiver<PeerSnapshot>)>,
    publish_to: &Option<watch::Sender<PeerSnapshot>>,
    recorder: &mut Option<Recorder>,
    window: &mut Option<(u16, u16)>,
    hints: &mut FirstUseHints,
    events: &mpsc::Sender<SessionEvent>,
    commands: &mut mpsc::Receiver<SessionCommand>,
) -> Outcome {
    let net::Connection {
        transport,
        security,
    } = connection;
    if events.send(SessionEvent::Security(security)).await.is_err() {
        return Outcome::Gone;
    }

    let (mut reader, mut writer) = tokio::io::split(transport);
    let mut telnet = TelnetMachine::new();
    let mut mccp = MccpDecoder::new();
    let mut decoder = TextDecoder::new(charset);
    let mut assembler = LineAssembler::default();
    let mut sock_buf = [0u8; 4096];
    // When the round trip currently being timed started, if any (§11).
    let mut sent_at: Option<Instant> = None;
    // Room tracking (§16), per connection rather than per session: a
    // reconnect can land the character somewhere else entirely, and
    // carrying the old room across would invent an edge between wherever
    // they were and wherever they came back.
    let mut outstanding_moves: Vec<(String, Instant)> = Vec::new();
    let mut last_room: Option<crate::map::RoomId> = None;
    // Inflate output, reused across reads.
    let mut plain: Vec<u8> = Vec::new();

    // Offer NAWS up front; the size follows once the server agrees (§6.2).
    telnet.request_local_enable(option::NAWS);
    if let Some((cols, rows)) = *window {
        telnet.set_window_size(cols, rows);
    }
    if flush_telnet(&mut telnet, &mut writer, &mut sent_at)
        .await
        .is_err()
    {
        return Outcome::Lost("write failed".to_string());
    }
    engine.start_timers(Instant::now());
    // Scripts get the same starting gun as the timers (§7.4).
    let connected = engine.on_connect();
    if send_lines(
        &mut writer,
        &connected.sends,
        &mut sent_at,
        &mut outstanding_moves,
    )
    .await
    .is_err()
    {
        return Outcome::Lost("write failed".to_string());
    }
    for text in connected.echoes {
        if events.send(SessionEvent::rule(text)).await.is_err() {
            return Outcome::Gone;
        }
    }

    let outcome = loop {
        // `select!` needs a future even when nothing is scheduled; park
        // far out rather than busy-waiting when there are no timers.
        let timer_deadline = engine
            .next_timer_deadline()
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(3600));

        tokio::select! {
            result = reader.read(&mut sock_buf) => {
                match result {
                    Ok(0) => break Outcome::Lost("connection closed".to_string()),
                    Ok(n) => {
                        // Anything at all coming back closes the round trip:
                        // the wait being measured is the player's, and it is
                        // over as soon as the server says something.
                        if let Some(started) = sent_at.take()
                            && events.send(SessionEvent::Latency(started.elapsed())).await.is_err()
                        {
                            return Outcome::Gone;
                        }
                        let raw = &sock_buf[..n];
                        if let Some(recorder) = recorder.as_mut() {
                            recorder.record(raw);
                        }

                        let mut outbound: Vec<String> = Vec::new();
                        // Why auto-login didn't finish, shown in the pane.
                        let mut login_notices: Vec<String> = Vec::new();
                        // Cross-session actions the hub has to route (§7.5).
                        let mut cross_out: Vec<crate::engine::CrossSend> = Vec::new();
                        // Negotiation replies triggered by inbound events
                        // (Core.Hello/Supports) rather than typed/trigger
                        // commands, so they bypass the CRLF line framing.
                        let mut raw_out: Vec<Bytes> = Vec::new();
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
                                // Set by whichever protocol wrote to the
                                // server-data store, so the room is only
                                // re-read when something could have changed
                                // it — not on every line of output.
                                let mut store_touched = false;
                                let mut emitted = match event {
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
                                    // The server just agreed to send GMCP;
                                    // announce ourselves (§6.3).
                                    TelnetEvent::OptionEnabled { option: option::GMCP, side: Side::Remote } => {
                                        raw_out.push(encode_subnegotiation(
                                            option::GMCP,
                                            &gmcp::encode(&gmcp::hello_message()),
                                        ));
                                        raw_out.push(encode_subnegotiation(
                                            option::GMCP,
                                            &gmcp::encode(&gmcp::supports_message(
                                                engine.gmcp_packages(),
                                            )),
                                        ));
                                        Vec::new()
                                    }
                                    TelnetEvent::Subnegotiation { option: option::GMCP, data } => {
                                        match gmcp::parse(&data) {
                                            Ok(message) => {
                                                let flat = gmcp::flatten(&message);
                                                forget_exits_on_room_change(engine, &flat.pairs);
                                                for (key, value) in flat.pairs {
                                                    engine.update_server_data_from_gmcp(&key, value);
                                                }
                                                store_touched = true;
                                                // After the inserts: a shorter
                                                // array's leftover indices are
                                                // only stale once the new ones
                                                // are in (§6.3).
                                                for (path, len) in flat.arrays {
                                                    engine.prune_gmcp_array(&path, len);
                                                }
                                                vec![SessionEvent::Gmcp {
                                                    package: message.package,
                                                    payload: message.payload,
                                                }]
                                            }
                                            Err(_) => Vec::new(),
                                        }
                                    }
                                    TelnetEvent::OptionEnabled { option: option::MSDP, side: Side::Remote } => {
                                        // Ask, or be told nothing at all
                                        // (§6.3) — see `report_requests`.
                                        for request in msdp::report_requests() {
                                            raw_out.push(encode_subnegotiation(
                                                option::MSDP,
                                                &request,
                                            ));
                                        }
                                        Vec::new()
                                    }
                                    TelnetEvent::Subnegotiation { option: option::MSDP, data } => {
                                        if let Ok(pairs) = msdp::parse(&data) {
                                            let mut flat = Flattened::default();
                                            for (name, value) in &pairs {
                                                msdp::flatten(name, value, &mut flat);
                                            }
                                            // Cloned for the inspector event below:
                                            // the loop right after this one consumes
                                            // `flat.pairs` feeding the engine.
                                            let inspector_pairs = flat.pairs.clone();
                                            forget_exits_on_room_change(engine, &flat.pairs);
                                            for (key, value) in flat.pairs {
                                                engine.update_server_data_from_msdp(&key, value);
                                            }
                                            store_touched = true;
                                            // As for GMCP: only stale once
                                            // the new indices are in (§6.3).
                                            for (path, len) in flat.arrays {
                                                engine.prune_msdp_array(&path, len);
                                            }
                                            vec![SessionEvent::Msdp { pairs: inspector_pairs }]
                                        } else {
                                            Vec::new()
                                        }
                                    }
                                    // Other options are handled inside the
                                    // Telnet machine.
                                    _ => Vec::new(),
                                };

                                // Whichever protocol just spoke, the room is
                                // read back out of the merged store rather
                                // than out of that protocol's own message
                                // (§6.3, §16) — which is what lets an
                                // MSDP-only MUD map at all, since MSDP
                                // raises no event of its own.
                                if store_touched
                                    && let Some(info) =
                                        crate::map::RoomInfo::from_server_data(engine.server_data())
                                    && let Some(room) = room_event(
                                        info,
                                        &mut last_room,
                                        &mut outstanding_moves,
                                        Instant::now(),
                                    )
                                {
                                    emitted.push(room);
                                }

                                for event in emitted {
                                    // Auto-login sees the exchange before the
                                    // rules do, and is disarmed for good the
                                    // moment the player types (§10).
                                    if let Some(machine) = login.as_mut() {
                                        let action = match &event {
                                            SessionEvent::Line { text, .. } => {
                                                machine.on_line(&strip_ansi(text))
                                            }
                                            SessionEvent::Prompt(text) => {
                                                machine.on_line(&strip_ansi(text))
                                            }
                                            SessionEvent::EchoMask(masked) => {
                                                machine.on_echo_mask(*masked)
                                            }
                                            _ => None,
                                        };
                                        match action {
                                            Some(LoginAction::Send(line)) => outbound.push(line),
                                            Some(LoginAction::Notice(text)) => {
                                                login_notices.push(text)
                                            }
                                            None => {}
                                        }
                                    }
                                    // Triggers run between the line assembler
                                    // and the UI (§6.5), so a gagged line never
                                    // reaches the scrollback at all.
                                    let mut emit: Vec<SessionEvent> = Vec::new();
                                    match event {
                                        SessionEvent::Line { text, origin } => {
                                            let outcome = engine.process_line(&strip_ansi(&text));
                                            outbound.extend(outcome.sends);
                                            cross_out.extend(outcome.send_to);
                                            // A substitution replaces the
                                            // line everywhere it would have
                                            // appeared — including the
                                            // channel copy — so one line is
                                            // never two different texts.
                                            // Its styling goes with it: the
                                            // script chose the replacement.
                                            // Highlight ranges are offsets
                                            // into the line the engine
                                            // matched, which the
                                            // replacement is not, so a
                                            // substitution drops them
                                            // rather than restyling text
                                            // they never described.
                                            let text = match outcome.substitute {
                                                Some(replacement) => replacement,
                                                None => {
                                                    apply_highlights(&text, &outcome.highlights)
                                                }
                                            };
                                            if let Some(channel) = outcome.route {
                                                emit.push(SessionEvent::Route {
                                                    channel,
                                                    text: text.clone(),
                                                });
                                            }
                                            if !outcome.gag {
                                                emit.push(SessionEvent::line(text, origin));
                                            }
                                            // Independent of gag: a line
                                            // worth hiding can still be
                                            // worth an alert.
                                            if outcome.bell {
                                                emit.push(SessionEvent::Bell);
                                            }
                                            // Ordered ahead of whatever
                                            // room death drops the
                                            // character into: the corpse
                                            // is where they were standing
                                            // when this line arrived, and
                                            // the hub reads that off the
                                            // room it still holds (§16).
                                            if outcome.corpse {
                                                emit.push(SessionEvent::Corpse);
                                            }
                                            if let Some(mark) = outcome.mark {
                                                emit.push(SessionEvent::Mark(mark));
                                            }
                                            // The one-time nudge that
                                            // automation is live (UX_REVIEW.md
                                            // G) — independent of gag too, so
                                            // even a silently-hidden trigger
                                            // still counts as the moment it
                                            // proved itself.
                                            if !hints.trigger_shown && outcome.fired {
                                                hints.trigger_shown = true;
                                                emit.push(SessionEvent::client(
                                                    "a trigger just fired — automation is live for this character"
                                                        .to_string(),
                                                ));
                                            }
                                            // Echoes follow the line that
                                            // provoked them.
                                            emit.extend(
                                                outcome.echoes.into_iter().map(SessionEvent::rule),
                                            );
                                            emit.extend(cross_echoes(outcome.echo_to));
                                        }
                                        SessionEvent::Prompt(text) => {
                                            let outcome = engine.process_prompt(&strip_ansi(&text));
                                            outbound.extend(outcome.sends);
                                            // A gagged prompt is an empty
                                            // one: the input line still has
                                            // to know there is nothing above
                                            // it now.
                                            let text = match outcome.gag {
                                                true => String::new(),
                                                false => outcome.substitute.unwrap_or(text),
                                            };
                                            emit.push(SessionEvent::Prompt(text));
                                            emit.extend(
                                                outcome.echoes.into_iter().map(SessionEvent::rule),
                                            );
                                            emit.extend(cross_echoes(outcome.echo_to));
                                        }
                                        SessionEvent::Gmcp { package, payload } => {
                                            let outcome = engine.process_gmcp(
                                                &package,
                                                payload.as_deref().unwrap_or(""),
                                            );
                                            outbound.extend(outcome.sends);
                                            emit.push(SessionEvent::Gmcp { package, payload });
                                            emit.extend(
                                                outcome.echoes.into_iter().map(SessionEvent::rule),
                                            );
                                            emit.extend(cross_echoes(outcome.echo_to));
                                        }
                                        other => emit.push(other),
                                    }
                                    for event in emit {
                                        if events.send(event).await.is_err() {
                                            return Outcome::Gone;
                                        }
                                    }
                                }
                            }
                        }
                        if let Some(reason) = break_reason {
                            break Outcome::Lost(reason);
                        }
                        publish(engine, publish_to);

                        for notice in login_notices.drain(..) {
                            if events.send(SessionEvent::client(notice)).await.is_err() {
                                return Outcome::Gone;
                            }
                        }
                        // Trigger output is sent verbatim: it is never fed
                        // back through aliases, so rules cannot recurse.
                        if send_lines(&mut writer, &outbound, &mut sent_at, &mut outstanding_moves).await.is_err() {
                            break Outcome::Lost("write failed".to_string());
                        }
                        if emit_cross_sends(events, cross_out, FIRST_HOP).await.is_err() {
                            return Outcome::Gone;
                        }
                        // Negotiation replies first, then anything they
                        // provoked: `raw_out` holds subnegotiations sent
                        // *because* an option came up, and a server that
                        // has not yet seen our DO is entitled to discard a
                        // subnegotiation for an option it does not consider
                        // enabled — which is exactly how MSDP `REPORT ROOM`
                        // went missing against hercmud (§6.3).
                        if flush_telnet(&mut telnet, &mut writer, &mut sent_at).await.is_err() {
                            break Outcome::Lost("write failed".to_string());
                        }
                        for bytes in &raw_out {
                            if writer.write_all(bytes).await.is_err() {
                                break_reason = Some("write failed".to_string());
                                break;
                            }
                            start_round_trip(&mut sent_at);
                        }
                        if let Some(reason) = break_reason {
                            break Outcome::Lost(reason);
                        }
                    }
                    Err(err) => break Outcome::Lost(format!("connection error: {err}")),
                }
            }
            Some(peer) = next_peer_change(watching) => {
                let outcome = engine.poll_peer(&peer);
                if send_lines(&mut writer, &outcome.sends, &mut sent_at, &mut outstanding_moves).await.is_err() {
                    break Outcome::Lost("write failed".to_string());
                }
                for text in outcome.echoes {
                    if events.send(SessionEvent::rule(text)).await.is_err() {
                        return Outcome::Gone;
                    }
                }
                let cross = outcome
                    .send_to
                    .into_iter()
                    .map(|(target, lines)| crate::engine::CrossSend { target, lines })
                    .collect();
                if emit_cross_sends(events, cross, FIRST_HOP).await.is_err() {
                    return Outcome::Gone;
                }
                for event in cross_echoes(outcome.echo_to) {
                    if events.send(event).await.is_err() {
                        return Outcome::Gone;
                    }
                }
            }
            _ = tokio::time::sleep_until(timer_deadline.into()) => {
                let now = Instant::now();
                let due = engine.fire_due_timers(now);
                // Script timers are armed against the same clock and woken
                // by the same sleep (§7.4).
                let scripted = engine.fire_due_script_timers(now);
                publish(engine, publish_to);
                if send_lines(&mut writer, &due, &mut sent_at, &mut outstanding_moves).await.is_err() {
                    break Outcome::Lost("write failed".to_string());
                }
                if send_lines(&mut writer, &scripted.sends, &mut sent_at, &mut outstanding_moves).await.is_err() {
                    break Outcome::Lost("write failed".to_string());
                }
                for text in scripted.echoes {
                    if events.send(SessionEvent::rule(text)).await.is_err() {
                        return Outcome::Gone;
                    }
                }
                for event in cross_echoes(scripted.echo_to) {
                    if events.send(event).await.is_err() {
                        return Outcome::Gone;
                    }
                }
                let cross = scripted
                    .send_to
                    .into_iter()
                    .map(|(target, lines)| crate::engine::CrossSend { target, lines })
                    .collect();
                if emit_cross_sends(events, cross, FIRST_HOP).await.is_err() {
                    return Outcome::Gone;
                }
            }
            cmd = commands.recv() => {
                // The player is driving now: auto-login must never fire
                // again on this connection (§10).
                if matches!(cmd, Some(SessionCommand::SendLine(_)))
                    && let Some(machine) = login.as_mut() {
                        machine.disarm();
                    }
                match cmd {
                    // A bare Enter carries meaning of its own, so it goes
                    // out as a plain CRLF instead of through the alias
                    // splitter — which drops empty parts, so that `a;;b`
                    // sends two commands rather than three.
                    Some(SessionCommand::SendLine(line)) if line.is_empty() => {
                        if send_lines(&mut writer, &[String::new()], &mut sent_at, &mut outstanding_moves)
                            .await
                            .is_err()
                        {
                            break Outcome::Lost("write failed".to_string());
                        }
                    }
                    Some(SessionCommand::SendLine(line)) => {
                        let outcome = engine.expand_input(&line);
                        publish(engine, publish_to);
                        if send_lines(&mut writer, &outcome.sends, &mut sent_at, &mut outstanding_moves).await.is_err() {
                            break Outcome::Lost("write failed".to_string());
                        }
                        // The one-time nudge that speedwalking actually
                        // did something (UX_REVIEW.md G) — only what the
                        // player typed, not an injected command, so the
                        // wording can say "speedwalk" without hedging
                        // about who typed it.
                        if !hints.speedwalk_shown
                            && let Some((path, steps)) = outcome.speedwalks.first()
                        {
                            hints.speedwalk_shown = true;
                            let text = format!("speedwalk: {path} → {}", steps.join(", "));
                            if events.send(SessionEvent::client(text)).await.is_err() {
                                return Outcome::Gone;
                            }
                        }
                        for text in outcome.echoes {
                            if events.send(SessionEvent::rule(text)).await.is_err() {
                                return Outcome::Gone;
                            }
                        }
                        for event in cross_echoes(outcome.echo_to) {
                            if events.send(event).await.is_err() {
                                return Outcome::Gone;
                            }
                        }
                        if emit_cross_sends(events, outcome.send_to, FIRST_HOP).await.is_err() {
                            return Outcome::Gone;
                        }
                    }
                    Some(SessionCommand::Inject { from, lines, hops }) => {
                        // Echo locally first: nothing another session does to
                        // this one may happen invisibly (§7.5).
                        for line in &lines {
                            let echo = SessionEvent::from_session(&from, format!("[from {from}] {line}"));
                            if events.send(echo).await.is_err() {
                                return Outcome::Gone;
                            }
                        }
                        let (sends, cross) = if expand_injected {
                            let mut sends = Vec::new();
                            let mut cross = Vec::new();
                            for line in &lines {
                                let outcome = engine.expand_input(line);
                                sends.extend(outcome.sends);
                                cross.extend(outcome.send_to);
                                for text in outcome.echoes {
                                    if events.send(SessionEvent::rule(text)).await.is_err() {
                                        return Outcome::Gone;
                                    }
                                }
                            }
                            (sends, cross)
                        } else {
                            (lines, Vec::new())
                        };
                        publish(engine, publish_to);
                        if send_lines(&mut writer, &sends, &mut sent_at, &mut outstanding_moves).await.is_err() {
                            break Outcome::Lost("write failed".to_string());
                        }
                        // Anything this injection set off is one hop further
                        // from where the chain started, so it runs out.
                        if emit_cross_sends(events, cross, hops.saturating_add(1)).await.is_err() {
                            return Outcome::Gone;
                        }
                    }
                    Some(SessionCommand::SetRules(rules)) => {
                        *engine = *rules;
                        engine.start_timers(Instant::now());
                        // Freshly loaded scripts have not seen this
                        // connection come up, so `/reload` is their
                        // `on_connect` — as it is the timers' start.
                        let reloaded = engine.on_connect();
                        if send_lines(&mut writer, &reloaded.sends, &mut sent_at, &mut outstanding_moves).await.is_err() {
                            break Outcome::Lost("write failed".to_string());
                        }
                        for text in reloaded.echoes {
                            if events.send(SessionEvent::rule(text)).await.is_err() {
                                return Outcome::Gone;
                            }
                        }
                    }
                    Some(SessionCommand::Resize { cols, rows }) => {
                        *window = Some((cols, rows));
                        telnet.set_window_size(cols, rows);
                        if flush_telnet(&mut telnet, &mut writer, &mut sent_at).await.is_err() {
                            break Outcome::Lost("write failed".to_string());
                        }
                    }
                    Some(SessionCommand::AddPeer { name, rx }) => {
                        engine.add_peer(name.clone(), rx.clone());
                        watching.push((name, rx));
                    }
                    Some(SessionCommand::Disconnect) | None => {
                        break Outcome::Ended("disconnected".to_string());
                    }
                }
            }
        }
    };

    // The disconnect hook runs on the way out. Its commands are attempted
    // rather than dropped — a session ending on `/disconnect` still has a
    // socket, and a farewell that only sometimes arrives beats one that
    // never does — but a write failing now says nothing worth printing over
    // the reason we are already leaving.
    let closing = engine.on_disconnect();
    let _ = send_lines(
        &mut writer,
        &closing.sends,
        &mut sent_at,
        &mut outstanding_moves,
    )
    .await;
    for text in closing.echoes {
        if events.send(SessionEvent::rule(text)).await.is_err() {
            return Outcome::Gone;
        }
    }
    outcome
}

/// How long a step may stay unresolved and still be believed to be what
/// moved the character (§16). A step either resolves in about a round trip
/// or did not happen: past this, something else is the likelier cause of a
/// room change — a summon, a wimpy auto-flee, a portal — and crediting the
/// step would write an edge the world does not have. Generous next to a
/// real round trip, so ordinary lag never costs a legitimate edge.
const MOVEMENT_SETTLE: Duration = Duration::from_secs(5);

/// Records what an outbound command means for room tracking (§16).
///
/// Drops the store's exit list when an arriving message names a different
/// room than the one already there (§6.3, §16).
///
/// Has to run *before* the message is merged in: once the new room's keys
/// are written, the previous room's exits are indistinguishable from this
/// room's own. A message that names no room at all changes nothing, so a
/// server sending `Room.Info` in pieces still accumulates as it did.
fn forget_exits_on_room_change(engine: &mut Engine, pairs: &[(String, String)]) {
    // A message carrying exits carries *all* of that room's exits — servers
    // send the list whole, never a key at a time — so it replaces whatever
    // is there rather than merging into it. This is what catches a server
    // that sends the exits and the room number in separate messages, which
    // the flat `ROOM_EXITS.*` / `ROOM_VNUM` form invites: keying only on the
    // number meant the exits arriving first were merged onto the room the
    // character had just left.
    if pairs.iter().any(|(key, _)| crate::map::is_exit_key(key)) {
        engine.forget_room_exits();
        return;
    }
    // And a message that names a different room replaces them even when it
    // brings no exits of its own, so arriving somewhere with no exits at all
    // does not inherit the last room's.
    let Some((arriving_key, arriving)) = crate::map::vnum_in_pairs(pairs) else {
        return;
    };
    // Whichever protocol just named the room is the one describing it now.
    // Any other spelling of the room number in the store is an older
    // report, and leaving it there let a single stale GMCP `Room.Info.num`
    // outrank every MSDP move for the rest of the session.
    engine.forget_stale_room_numbers(arriving_key);
    if crate::map::vnum_in_store(engine.server_data()) != Some(arriving) {
        engine.forget_room_exits();
    }
}

/// Movement accumulates; anything else clears it. The clearing is the
/// interesting half: a command that is *not* a step is the likeliest
/// explanation for a room change right after it — a recall, a portal, a
/// trigger's teleport — so crediting that change to a movement sent earlier
/// would invent an edge the world does not have.
fn note_outbound(outstanding: &mut Vec<(String, Instant)>, line: &str, now: Instant) {
    match crate::map::canonical_direction(line) {
        Some(direction) => outstanding.push((direction.to_string(), now)),
        None => outstanding.clear(),
    }
}

/// Turns a room sighting into the event the hub should see, or `None` when
/// the character has not actually gone anywhere.
///
/// The edge is claimed only when exactly one movement is outstanding. A
/// speedwalk puts every step on the wire before the first room update comes
/// back, and pairing those in arrival order assumes each one succeeded —
/// a single wall part-way through would misrecord every edge after it.
/// Learning nothing from a speedwalk beats learning it wrong, because a
/// wrong edge is a lie the map keeps telling every time it paths.
fn room_event(
    info: crate::map::RoomInfo,
    last_room: &mut Option<crate::map::RoomId>,
    outstanding: &mut Vec<(String, Instant)>,
    now: Instant,
) -> Option<SessionEvent> {
    if *last_room == Some(info.id) {
        // Still here. Whatever is outstanding has not resolved yet — the
        // step may simply have been refused — so it keeps waiting rather
        // than being spent on a room we never left.
        return None;
    }
    // Unambiguous *and* recent. One outstanding step says the client knows
    // which one it was; the settle window says it is still plausibly the
    // reason the room changed, rather than a step that was refused minutes
    // ago and has been waiting while something else did the moving.
    let arrived_via = match outstanding.as_slice() {
        [(direction, sent)] if now.duration_since(*sent) < MOVEMENT_SETTLE => {
            Some(direction.clone())
        }
        _ => None,
    };
    outstanding.clear();
    *last_room = Some(info.id);
    Some(SessionEvent::Room {
        info: Box::new(info),
        arrived_via,
    })
}

/// A script's cross-session echoes, as events for the hub to place. They
/// are events rather than commands because nothing runs at the far end:
/// the hub owns the panes, so it can write the text itself (§7.5).
fn cross_echoes(echoes: Vec<(String, String)>) -> Vec<SessionEvent> {
    echoes
        .into_iter()
        .map(|(target, text)| SessionEvent::EchoTo { target, text })
        .collect()
}

/// Waits for any peer to publish something new, and says which. Parks
/// forever for a lone character, so `select!` has a branch that simply
/// never fires rather than a special case (§7.5).
async fn next_peer_change(
    watching: &mut [(String, watch::Receiver<PeerSnapshot>)],
) -> Option<String> {
    if watching.is_empty() {
        std::future::pending::<()>().await;
    }
    let changes = watching
        .iter_mut()
        .map(|(name, rx)| Box::pin(async move { rx.changed().await.ok().map(|()| name.clone()) }));
    futures::future::select_all(changes).await.0
}

/// Publishes this session's state for its peers to read, if it has moved
/// since the last time (§7.5). A snapshot is a value, so peers read the
/// last one published and never this session's live stores — which is what
/// keeps buffer/state isolation (§3) intact while still letting one
/// character's rules consult another's vitals.
fn publish(engine: &mut Engine, publish_to: &Option<watch::Sender<PeerSnapshot>>) {
    if let Some(sender) = publish_to
        && let Some(snapshot) = engine.take_snapshot()
    {
        let _ = sender.send(snapshot);
    }
}

/// A cross-session action a session raised itself is the first hop; the
/// counter only grows when one injection sets off another (§7.5).
const FIRST_HOP: u8 = 1;

/// Hand the hub the cross-session actions a rule produced, tagged with the
/// hop they would be. The hub owns addressing and the hop limit, since only
/// it knows what other sessions exist.
async fn emit_cross_sends(
    events: &mpsc::Sender<SessionEvent>,
    sends: Vec<crate::engine::CrossSend>,
    hops: u8,
) -> Result<(), mpsc::error::SendError<SessionEvent>> {
    for send in sends {
        events
            .send(SessionEvent::SendTo {
                target: send.target,
                lines: send.lines,
                hops,
            })
            .await?;
    }
    Ok(())
}

/// Write each command as its own CRLF-terminated line.
async fn send_lines<W>(
    writer: &mut W,
    lines: &[String],
    sent_at: &mut Option<Instant>,
    outstanding: &mut Vec<(String, Instant)>,
) -> std::io::Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    for line in lines {
        writer.write_all(line.as_bytes()).await?;
        writer.write_all(b"\r\n").await?;
        start_round_trip(sent_at);
        // Every outbound command passes here, whoever wrote it — typed,
        // alias-expanded, a speedwalk step, a trigger's `send:` — so this
        // is the one place room tracking can see movement at all (§16).
        note_outbound(outstanding, line, Instant::now());
    }
    Ok(())
}

/// Write any negotiation replies the Telnet machine has queued.
async fn flush_telnet<W>(
    telnet: &mut TelnetMachine,
    writer: &mut W,
    sent_at: &mut Option<Instant>,
) -> std::io::Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    let out = telnet.take_output();
    if out.is_empty() {
        return Ok(());
    }
    writer.write_all(&out).await?;
    start_round_trip(sent_at);
    Ok(())
}

/// Starts timing a round trip, unless one is already being timed. Keeping
/// the earliest outstanding send means a burst of commands is measured from
/// the first of them to the first reply, rather than each write resetting
/// the clock and reporting only the tail of the wait.
fn start_round_trip(sent_at: &mut Option<Instant>) {
    sent_at.get_or_insert_with(Instant::now);
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
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        // Raw capture is inbound server bytes verbatim — owner-only, same
        // as the transcript log it sits alongside.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path)?;
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

    /// Raw capture (`--record`) is inbound server bytes verbatim, which can
    /// include a login exchange — owner-only on disk, not left at the
    /// process umask.
    #[cfg(unix)]
    #[test]
    fn recorder_create_creates_the_file_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!(
            "mudular-test-record-{}-{:?}.log",
            std::process::id(),
            std::thread::current().id()
        ));
        let recorder = Recorder::create(&path, "example.org", 4000).unwrap();
        drop(recorder);

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
    }

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

    /// §11: the pane reports the round trip a player waits through, which
    /// is the gap between what they sent and the server answering.
    #[tokio::test]
    async fn measures_the_round_trip_of_a_typed_line() {
        let (mut events, commands) = serve(|mut sock| async move {
            // The greeting closes the round trip the connect-time NAWS
            // offer started, so the next one is the typed line's alone.
            sock.write_all(b"Welcome.\r\n").await.unwrap();
            // Must be the typed line, not merely the next bytes to arrive:
            // a raw read is satisfied by the client's connect-time Telnet
            // offer, which starts this delay before the command was ever
            // sent and measures a round trip shorter than the wait.
            assert_eq!(read_command(&mut sock).await, "north");
            tokio::time::sleep(Duration::from_millis(20)).await;
            sock.write_all(b"You go north.\r\n").await.unwrap();
            std::future::pending::<()>().await;
        });

        next_latency(&mut events).await;
        // Deliberate: it makes a regression to a raw server-side read fail
        // every time rather than once in a hundred runs.
        tokio::time::sleep(Duration::from_millis(10)).await;
        commands
            .send(SessionCommand::SendLine("north".into()))
            .await
            .unwrap();

        let rtt = next_latency(&mut events).await;
        assert!(
            rtt >= Duration::from_millis(20),
            "round trip shorter than the server's own delay: {rtt:?}"
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
            false,
            None,
            PeerLinks::default(),
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

    // ---- exits across a move (§6.3, §16) ----

    fn pairs(entries: &[(&str, &str)]) -> Vec<(String, String)> {
        entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn engine_in_room(vnum: &str, exits: &[(&str, &str)]) -> Engine {
        let mut engine = rules("name: t\n");
        engine.update_server_data_from_gmcp("Room.Info.num", vnum.to_string());
        for (direction, dest) in exits {
            engine.update_server_data_from_gmcp(
                &format!("Room.Info.exits.{direction}"),
                dest.to_string(),
            );
        }
        engine
    }

    #[test]
    fn arriving_in_a_new_room_drops_the_last_rooms_exits() {
        let mut engine = engine_in_room("40601", &[("n", "40600"), ("e", "40602")]);

        forget_exits_on_room_change(&mut engine, &pairs(&[("Room.Info.num", "40602")]));

        assert!(
            !engine.server_data().keys().any(|k| k.contains("exits")),
            "the crossroads' exits must not survive into the shop: {:?}",
            engine.server_data()
        );
    }

    /// The other half: a server updating the room it already described must
    /// not have its exits thrown away, or a partial `Room.Info` would empty
    /// the map every time it arrived (§6.3).
    #[test]
    fn an_update_about_the_same_room_keeps_its_exits() {
        let mut engine = engine_in_room("40601", &[("n", "40600"), ("e", "40602")]);

        forget_exits_on_room_change(
            &mut engine,
            &pairs(&[("Room.Info.num", "40601"), ("Room.Info.name", "Renamed")]),
        );

        assert_eq!(
            engine
                .server_data()
                .get("Room.Info.exits.n")
                .map(String::as_str),
            Some("40600"),
        );
    }

    /// An adversarial review found the previous fix only closed the
    /// ordering where the room number arrives with its exits. A server that
    /// sends the exits first — natural in MSDP's flat `ROOM_EXITS.*` /
    /// `ROOM_VNUM` form, which `EXIT_PREFIXES` supports — still had them
    /// merged onto the room the character had just left, and that blend was
    /// then written to disk.
    #[test]
    fn exits_arriving_before_the_room_number_do_not_join_the_last_room() {
        let mut engine = engine_in_room("40601", &[("n", "40600")]);

        // The next room's exits, with its number still to come.
        forget_exits_on_room_change(&mut engine, &pairs(&[("Room.Info.exits.s", "40700")]));
        engine.update_server_data_from_gmcp("Room.Info.exits.s", "40700".to_string());

        let room = crate::map::RoomInfo::from_server_data(engine.server_data()).expect("a room");
        assert_eq!(
            room.exits.get("n"),
            None,
            "the room left behind must not keep lending its exits: {:?}",
            room.exits
        );
    }

    /// An adversarial review found a whole class of server on which the map
    /// simply froze: one that names the room over GMCP once, then reports
    /// every move over MSDP. `from_server_data` prefers GMCP's spelling
    /// unconditionally, so the stale value won forever and the character
    /// never appeared to move.
    #[test]
    fn a_stale_gmcp_room_number_does_not_outrank_the_move_just_reported() {
        let mut engine = rules("name: t\n");
        engine.update_server_data_from_gmcp("Room.Info.num", "40600".to_string());

        // From here on the server reports movement over MSDP only.
        let arriving = pairs(&[("ROOM.VNUM", "40601")]);
        forget_exits_on_room_change(&mut engine, &arriving);
        for (key, value) in arriving {
            engine.update_server_data_from_msdp(&key, value);
        }

        let room = crate::map::RoomInfo::from_server_data(engine.server_data()).expect("a room");
        assert_eq!(
            room.id,
            crate::map::RoomId(40601),
            "the map must follow the move it was actually told about"
        );
    }

    /// A message that names no room says nothing about which room we are
    /// in, so it decides nothing about the exits either.
    #[test]
    fn a_message_with_no_room_number_changes_nothing() {
        let mut engine = engine_in_room("40601", &[("n", "40600")]);

        forget_exits_on_room_change(&mut engine, &pairs(&[("Char.Vitals.hp", "90")]));

        assert_eq!(
            engine
                .server_data()
                .get("Room.Info.exits.n")
                .map(String::as_str),
            Some("40600"),
        );
    }

    /// A session publishes what its rules learn, so its peers can read it
    /// without touching its buffers or its engine (§7.5).
    #[tokio::test]
    async fn a_session_publishes_its_state_for_peers() {
        let (publish, snapshots) = watch::channel(PeerSnapshot::default());
        let (_events, _commands) = serve_with_login(
            move |mut sock| async move {
                sock.write_all(b"You are now fighting kobold\r\n")
                    .await
                    .unwrap();
                tokio::time::sleep(Duration::from_secs(5)).await;
            },
            rules(
                r#"
                name: test
                triggers:
                  - pattern: '^You are now fighting (?P<foe>\w+)'
                    set: {target: "${foe}"}
                "#,
            ),
            false,
            None,
            PeerLinks {
                publish: Some(publish),
                others: crate::engine::Peers::new(),
            },
        );

        let mut snapshots = snapshots;
        timeout(Duration::from_secs(2), snapshots.changed())
            .await
            .expect("timed out waiting for a snapshot")
            .expect("the session is still publishing");
        assert_eq!(
            snapshots.borrow().vars.get("target").map(String::as_str),
            Some("kobold")
        );
    }

    /// `SessionCommand::AddPeer` (`/connect`, §7.5) lets an already-running
    /// session learn about a character added after it — the gap
    /// `ARCH_REVIEW.md` "Features that would break the architecture"
    /// found: the peer mesh was built once before the event loop and never
    /// revisited.
    #[tokio::test]
    async fn add_peer_command_makes_a_new_peer_readable() {
        let (tx, mut sent) = mpsc::channel(8);
        let (_events, commands) = serve_with_rules(
            move |mut sock| async move {
                let mut reader = CommandReader::default();
                tx.send(reader.next(&mut sock).await).await.unwrap();
            },
            rules(
                r#"
                name: test
                aliases:
                  - pattern: '^check$'
                    send: ["say ${@cleric.hp}"]
                "#,
            ),
        );

        let (publish, rx) = watch::channel(PeerSnapshot {
            vars: [("hp".to_string(), "80".to_string())].into(),
            data: Default::default(),
        });
        Box::leak(Box::new(publish));

        commands
            .send(SessionCommand::AddPeer {
                name: "cleric".to_string(),
                rx,
            })
            .await
            .unwrap();
        commands
            .send(SessionCommand::SendLine("check".to_string()))
            .await
            .unwrap();

        assert_eq!(next_sent(&mut sent).await, "say 80");
    }

    /// The same, but the peer arrives while this session is between
    /// connections — `wait_to_retry` has to keep the same `watching` list
    /// `run_connection` does, or a peer added during a drop is lost by the
    /// time the reconnect completes.
    #[tokio::test]
    async fn add_peer_survives_a_reconnect_wait() {
        let (tx, mut sent) = mpsc::channel(8);
        let (_events, commands) = revolving_door_with_rules(
            move |mut sock| {
                let tx = tx.clone();
                tokio::spawn(async move {
                    let _ = tx.send(read_command(&mut sock).await).await;
                });
            },
            rules(
                r#"
                name: test
                aliases:
                  - pattern: '^check$'
                    send: ["say ${@cleric.hp}"]
                "#,
            ),
        );

        // The first connection drops immediately (`revolving_door` never
        // keeps a socket open), landing the session in its 1s backoff —
        // this is meant to arrive during that wait, not before it starts.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let (publish, rx) = watch::channel(PeerSnapshot {
            vars: [("hp".to_string(), "80".to_string())].into(),
            data: Default::default(),
        });
        Box::leak(Box::new(publish));
        commands
            .send(SessionCommand::AddPeer {
                name: "cleric".to_string(),
                rx,
            })
            .await
            .unwrap();

        // `SendLine` while disconnected is a no-op (`wait_to_retry` only
        // serves `SetRules`/`AddPeer`/`Resize`/`Disconnect`), so wait past
        // the 1s backoff for the reconnect before asking anything of it.
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        commands
            .send(SessionCommand::SendLine("check".to_string()))
            .await
            .unwrap();

        assert_eq!(
            timeout(Duration::from_secs(3), sent.recv())
                .await
                .expect("timed out waiting for the reconnected session")
                .unwrap(),
            "say 80"
        );
    }

    #[cfg(feature = "lua")]
    fn scripted_rules(lua: &str) -> Engine {
        let mut module: crate::engine::RuleModule =
            serde_yaml::from_str("name: test").expect("valid test YAML");
        module
            .script_sources
            .push(crate::engine::script::ScriptSource {
                name: "test.lua".to_string(),
                code: lua.to_string(),
            });
        Engine::compile(&[module]).expect("compiles")
    }

    /// The script counterpart of `a_trigger_answers_the_server_automatically`:
    /// a hook's command reaches the socket, and its echo reaches the pane
    /// without ever reaching the server.
    #[cfg(feature = "lua")]
    #[tokio::test]
    async fn a_script_hook_answers_the_server_and_echoes_locally() {
        let (tx, mut sent) = mpsc::channel(8);
        let (mut events, _commands) = serve_with_rules(
            move |mut sock| async move {
                sock.write_all(b"The kobold is DEAD!\r\n").await.unwrap();
                tx.send(read_command(&mut sock).await).await.unwrap();
            },
            scripted_rules(
                r#"
                mud.on_line(function(line)
                  if line:match("is DEAD!") then
                    mud.send("get all corpse")
                    mud.echo("** looted")
                  end
                end)
                "#,
            ),
        );

        // The server's line and the script's echo reach the pane the same
        // way; only `origin` tells them apart afterwards (§8).
        assert_eq!(
            next_line_with_origin(&mut events).await,
            ("The kobold is DEAD!".to_string(), Origin::Server)
        );
        assert_eq!(
            next_line_with_origin(&mut events).await,
            ("** looted".to_string(), Origin::Rule)
        );
        assert_eq!(
            timeout(Duration::from_secs(2), sent.recv())
                .await
                .expect("timed out")
                .unwrap(),
            "get all corpse"
        );
    }

    /// `mud.substitute` rewrites what the pane shows without the server
    /// ever knowing, and `mud.gag` keeps a line off it entirely.
    #[cfg(feature = "lua")]
    #[tokio::test]
    async fn a_script_can_rewrite_and_hide_lines_in_the_pane() {
        let (mut events, _commands) = serve_with_rules(
            move |mut sock| async move {
                sock.write_all(b"a boring line\r\nsomething shouty\r\n")
                    .await
                    .unwrap();
                // Hold the connection open so the pane is not cut short.
                tokio::time::sleep(Duration::from_secs(5)).await;
            },
            scripted_rules(
                r#"
                mud.on_line(function(line)
                  if line == "a boring line" then mud.gag() end
                  if line == "something shouty" then mud.substitute("something calm") end
                end)
                "#,
            ),
        );

        assert_eq!(next_line(&mut events).await, "something calm");
    }

    /// The connect hook runs when the timers are armed, so a script can
    /// take its first action without waiting for the server to say
    /// something.
    #[cfg(feature = "lua")]
    #[tokio::test]
    async fn a_connect_hook_sends_before_any_server_output() {
        let (tx, mut sent) = mpsc::channel(8);
        let (_events, _commands) = serve_with_rules(
            move |mut sock| async move {
                tx.send(read_command(&mut sock).await).await.unwrap();
            },
            scripted_rules(r#"mud.on_connect(function() mud.send("look") end)"#),
        );

        assert_eq!(
            timeout(Duration::from_secs(2), sent.recv())
                .await
                .expect("timed out")
                .unwrap(),
            "look"
        );
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

        // The gagged line still fired a trigger, so the one-time hint
        // (UX_REVIEW.md G) still shows — gag hides the *line*, not the
        // fact that automation ran.
        let (hint, origin) = next_line_with_origin(&mut events).await;
        assert_eq!(origin, Origin::Client);
        assert!(hint.contains("trigger"));

        assert_eq!(
            next_line(&mut events).await,
            "something real",
            "the gagged line must not be delivered"
        );
    }

    /// The hint fires once, not on every trigger — "a running commentary is
    /// exactly what this is deliberately not" (UX_REVIEW.md G).
    #[tokio::test]
    async fn a_trigger_firing_shows_the_hint_only_once() {
        let (mut events, _commands) = serve_with_rules(
            |mut sock| async move {
                sock.write_all(b"spam one\r\nspam two\r\nreal line\r\n")
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

        let (hint, origin) = next_line_with_origin(&mut events).await;
        assert_eq!(origin, Origin::Client);
        assert!(hint.contains("trigger"));

        // Second gagged fire: straight to the next real line, no second hint.
        assert_eq!(next_line(&mut events).await, "real line");
    }

    /// A `bell:` trigger emits its own event, separate from the line it
    /// fired on — the hub decides whether it actually rings (§14 M9).
    #[tokio::test]
    async fn a_bell_trigger_emits_a_bell_event() {
        let (mut events, _commands) = serve_with_rules(
            |mut sock| async move {
                sock.write_all(b"You have been slain by a rat.\r\n")
                    .await
                    .unwrap();
            },
            rules(
                r#"
                name: test
                triggers:
                  - pattern: 'slain'
                    bell: true
                "#,
            ),
        );

        assert_eq!(
            next_line(&mut events).await,
            "You have been slain by a rat."
        );
        next_matching(&mut events, |ev| matches!(ev, SessionEvent::Bell)).await;
    }

    /// A `corpse:` trigger emits its own event too (§16). The session says
    /// only that it happened; the hub is what turns that into a room.
    #[tokio::test]
    async fn a_corpse_trigger_emits_a_corpse_event() {
        let (mut events, _commands) = serve_with_rules(
            |mut sock| async move {
                sock.write_all(b"You have been KILLED!!\r\n").await.unwrap();
            },
            rules(
                r#"
                name: test
                triggers:
                  - pattern: 'You have been KILLED'
                    corpse: true
                "#,
            ),
        );

        assert_eq!(next_line(&mut events).await, "You have been KILLED!!");
        next_matching(&mut events, |ev| matches!(ev, SessionEvent::Corpse)).await;
    }

    /// The first speedwalk expansion this session shows a one-time hint
    /// explaining what it did (UX_REVIEW.md G); a second one stays quiet.
    #[tokio::test]
    async fn a_speedwalk_expansion_shows_a_one_time_hint() {
        let (tx, mut sent) = mpsc::channel(8);
        let (mut events, commands) = serve_with_rules(
            move |mut sock| async move {
                let mut reader = CommandReader::default();
                for _ in 0..7 {
                    tx.send(reader.next(&mut sock).await).await.unwrap();
                }
                // Stay connected: closing here would read as a dropped
                // connection and add a spurious Reconnecting event to what
                // `drain` collects below.
                std::future::pending::<()>().await;
            },
            rules("name: test"),
        );

        commands
            .send(SessionCommand::SendLine(".3n2e".into()))
            .await
            .unwrap();
        for expected in ["n", "n", "n", "e", "e"] {
            assert_eq!(next_sent(&mut sent).await, expected);
        }

        let (hint, origin) = next_line_with_origin(&mut events).await;
        assert_eq!(origin, Origin::Client);
        assert_eq!(hint, "speedwalk: .3n2e → n, n, n, e, e");

        // A second speedwalk still sends fine, but shows no second hint.
        commands
            .send(SessionCommand::SendLine(".2n".into()))
            .await
            .unwrap();
        assert_eq!(next_sent(&mut sent).await, "n");
        assert_eq!(next_sent(&mut sent).await, "n");
        let seen = drain((events, commands)).await;
        assert!(seen.is_empty(), "no second hint expected, got {seen:?}");
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

    /// The M6 acceptance criterion (§14): GMCP negotiation announces the
    /// client, a vitals message reaches the UI as a raw event, and its
    /// value is live in the engine's server-data store for a trigger's
    /// `${...}` template to read straight away.
    #[tokio::test]
    async fn gmcp_negotiates_surfaces_vitals_and_feeds_the_engine() {
        let (tx, mut sent) = mpsc::channel(8);
        let (mut events, commands) = serve_with_rules(
            move |mut sock| async move {
                sock.write_all(&[IAC, WILL, option::GMCP]).await.unwrap();

                let mut subnegs = SubnegReader::default();
                let hello = subnegs.next(&mut sock, option::GMCP).await;
                assert!(
                    String::from_utf8_lossy(&hello).starts_with("Core.Hello"),
                    "client must announce itself first: {hello:?}"
                );
                let supports = subnegs.next(&mut sock, option::GMCP).await;
                assert!(
                    String::from_utf8_lossy(&supports).starts_with("Core.Supports.Set"),
                    "client must advertise supported packages: {supports:?}"
                );

                let vitals = encode_subnegotiation(option::GMCP, br#"Char.Vitals {"hp":87}"#);
                sock.write_all(&vitals).await.unwrap();

                tx.send(read_command(&mut sock).await).await.unwrap();
            },
            rules(
                r#"
                name: test
                aliases:
                  - pattern: '^hp$'
                    send: ["tell hp ${Char.Vitals.hp}"]
                "#,
            ),
        );

        assert_eq!(
            next_matching(&mut events, |ev| matches!(ev, SessionEvent::Gmcp { .. })).await,
            SessionEvent::Gmcp {
                package: "Char.Vitals".to_string(),
                payload: Some(r#"{"hp":87}"#.to_string()),
            }
        );

        commands
            .send(SessionCommand::SendLine("hp".into()))
            .await
            .unwrap();
        assert_eq!(next_sent(&mut sent).await, "tell hp 87");
    }

    /// A declared package is only worth anything if it reaches the wire:
    /// a server that gates its pushes on `Core.Supports.Set` stays silent
    /// about `Group` forever if the advertisement never names it.
    #[tokio::test]
    async fn a_declared_gmcp_package_reaches_the_advertisement() {
        let (tx, mut sent) = mpsc::channel(8);
        let (_events, _commands) = serve_with_rules(
            move |mut sock| async move {
                sock.write_all(&[IAC, WILL, option::GMCP]).await.unwrap();

                let mut subnegs = SubnegReader::default();
                subnegs.next(&mut sock, option::GMCP).await; // Core.Hello
                let supports = subnegs.next(&mut sock, option::GMCP).await;
                tx.send(String::from_utf8_lossy(&supports).into_owned())
                    .await
                    .unwrap();
            },
            rules(
                r#"
                name: test
                gmcp_packages: ['Group 1']
                "#,
            ),
        );

        let supports = sent.recv().await.expect("advertisement");
        assert_eq!(
            supports,
            r#"Core.Supports.Set ["Char 1","Room 1","Group 1"]"#
        );
    }

    /// GMCP arrays flatten to positional keys, so merging each payload
    /// key-by-key would keep the tail of a longer previous array forever:
    /// `["bless","haste"]` then `["haste"]` left `Char.Affects.1` reading
    /// `haste` for the rest of the session — a buff that never expires,
    /// which is §7.5's "rebuff when a blessing drops" example failing
    /// silently. End-to-end rather than against the engine alone, because
    /// the merge and the prune are composed in this loop (§6.3).
    #[tokio::test]
    async fn a_shrinking_gmcp_array_does_not_leave_a_phantom_entry() {
        let (tx, mut sent) = mpsc::channel(8);
        let (mut events, commands) = serve_with_rules(
            move |mut sock| async move {
                sock.write_all(&[IAC, WILL, option::GMCP]).await.unwrap();

                let mut subnegs = SubnegReader::default();
                subnegs.next(&mut sock, option::GMCP).await; // Core.Hello
                subnegs.next(&mut sock, option::GMCP).await; // Core.Supports.Set

                for payload in [
                    br#"Char.Affects ["bless","haste"]"#.as_slice(),
                    br#"Char.Affects ["haste"]"#.as_slice(),
                ] {
                    sock.write_all(&encode_subnegotiation(option::GMCP, payload))
                        .await
                        .unwrap();
                }

                tx.send(read_command(&mut sock).await).await.unwrap();
            },
            rules(
                r#"
                name: test
                aliases:
                  - pattern: '^aff$'
                    send: ["tell 0=${Char.Affects.0} 1=${Char.Affects.1}"]
                "#,
            ),
        );

        // Wait for the *shrunk* payload specifically, so the store is
        // known to have seen both messages before the alias reads it.
        next_matching(&mut events, |ev| {
            matches!(ev, SessionEvent::Gmcp { package, payload }
                if package == "Char.Affects"
                    && payload.as_deref().is_some_and(|p| !p.contains("bless")))
        })
        .await;

        commands
            .send(SessionCommand::SendLine("aff".into()))
            .await
            .unwrap();
        // Index 0 slid down to `haste`; index 1 is gone, and an unknown
        // name is left as written rather than silently blank (§7.1).
        assert_eq!(
            next_sent(&mut sent).await,
            "tell 0=haste 1=${Char.Affects.1}"
        );
    }

    /// MSDP has no negotiation handshake of its own (unlike GMCP's
    /// Core.Hello/Supports) — a server can push data as soon as the option
    /// is enabled. This exercises that path end to end: negotiation, an
    /// MSDP VAR/VAL pair surfacing as a raw inspector event with the
    /// flattened pairs (§6.3, §14 M6), and an alias's `${...}` template
    /// reading the same value straight from the server-data store.
    #[tokio::test]
    async fn msdp_negotiates_surfaces_an_inspector_event_and_feeds_the_engine() {
        let (tx, mut sent) = mpsc::channel(8);
        let (mut events, commands) = serve_with_rules(
            move |mut sock| async move {
                sock.write_all(&[IAC, WILL, option::MSDP]).await.unwrap();
                let tail = await_agreement(&mut sock, option::MSDP).await;

                let mut payload = vec![msdp::VAR];
                payload.extend_from_slice(b"ROOM_NAME");
                payload.push(msdp::VAL);
                payload.extend_from_slice(b"The Bazaar");
                let msg = encode_subnegotiation(option::MSDP, &payload);
                sock.write_all(&msg).await.unwrap();

                let mut reader = CommandReader { raw: tail };
                tx.send(reader.next(&mut sock).await).await.unwrap();
            },
            rules(
                r#"
                name: test
                aliases:
                  - pattern: '^look$'
                    send: ["you see ${ROOM_NAME}"]
                "#,
            ),
        );

        assert_eq!(
            next_matching(&mut events, |ev| matches!(ev, SessionEvent::Msdp { .. })).await,
            SessionEvent::Msdp {
                pairs: vec![("ROOM_NAME".to_string(), "The Bazaar".to_string())],
            }
        );

        commands
            .send(SessionCommand::SendLine("look".into()))
            .await
            .unwrap();
        assert_eq!(next_sent(&mut sent).await, "you see The Bazaar");
    }

    /// MSDP is a subscription: the widely deployed implementation sends a
    /// variable only once the client has `REPORT`ed it, so negotiating the
    /// option and then staying quiet leaves a perfectly working server
    /// silent — which reads exactly like a server that does not speak MSDP,
    /// and leaves the auto-mapper inert on an MSDP-only MUD (observed live
    /// against hercmud, where `ROOM` never arrived).
    ///
    /// Asserted on the raw stream rather than through `SubnegReader`
    /// because the *order* is half the requirement: the subscription is
    /// provoked by the option coming up, and a server that has not yet
    /// seen our `DO` may discard a subnegotiation for an option it does
    /// not consider enabled. Sending it first is the same silence with the
    /// bytes present on the wire (§6.3).
    #[tokio::test]
    async fn msdp_negotiation_subscribes_to_room_reports_after_agreeing() {
        let (tx, mut stream) = mpsc::channel(8);
        let (_events, _commands) = serve(move |mut sock| async move {
            sock.write_all(&[IAC, WILL, option::MSDP]).await.unwrap();

            let mut seen = Vec::new();
            let mut buf = vec![0u8; 256];
            loop {
                let n = sock.read(&mut buf).await.unwrap();
                assert!(n > 0, "client closed before subscribing");
                seen.extend_from_slice(&buf[..n]);
                if seen.windows(2).any(|w| w == [IAC, SE]) {
                    break;
                }
            }
            tx.send(seen).await.unwrap();
            idle(&mut sock).await;
        });

        let seen = timeout(Duration::from_secs(2), stream.recv())
            .await
            .expect("timed out waiting for the MSDP subscription")
            .expect("sender dropped");

        let mut subscribe = vec![IAC, SB, option::MSDP, msdp::VAR];
        subscribe.extend_from_slice(b"REPORT");
        subscribe.push(msdp::VAL);
        subscribe.extend_from_slice(b"ROOM");
        subscribe.extend_from_slice(&[IAC, SE]);

        let agreed = seen
            .windows(3)
            .position(|w| w == [IAC, DO, option::MSDP])
            .expect("client must agree to MSDP");
        let subscribed = seen
            .windows(subscribe.len())
            .position(|w| w == subscribe)
            .expect("client must subscribe to ROOM");
        assert!(agreed < subscribed, "DO must precede the REPORT: {seen:?}");
    }

    /// The MSDP twin of `a_shrinking_gmcp_array_does_not_leave_a_phantom_entry`:
    /// MSDP arrays are positional for the same reason and went stale the
    /// same way, feeding the same server-data namespace (§6.3).
    #[tokio::test]
    async fn a_shrinking_msdp_array_does_not_leave_a_phantom_entry() {
        let (tx, mut sent) = mpsc::channel(8);
        let (mut events, commands) = serve_with_rules(
            move |mut sock| async move {
                sock.write_all(&[IAC, WILL, option::MSDP]).await.unwrap();
                let tail = await_agreement(&mut sock, option::MSDP).await;

                // `ROOM_EXITS` as a two-element array, then a one-element
                // one — the exit that closed behind you.
                let exits = |values: &[&str]| {
                    let mut payload = vec![msdp::VAR];
                    payload.extend_from_slice(b"ROOM_EXITS");
                    payload.push(msdp::VAL);
                    payload.push(msdp::ARRAY_OPEN);
                    for value in values {
                        payload.push(msdp::VAL);
                        payload.extend_from_slice(value.as_bytes());
                    }
                    payload.push(msdp::ARRAY_CLOSE);
                    encode_subnegotiation(option::MSDP, &payload)
                };

                let mut combined = exits(&["north", "south"]).to_vec();
                combined.extend_from_slice(&exits(&["north"]));
                // Same synchronisation as the test above: MSDP raises no
                // event of its own, and the session processes the stream in
                // order, so a plain line after both updates means both have
                // landed by the time the test sees it.
                combined.extend_from_slice(b"msdp applied\r\n");
                sock.write_all(&combined).await.unwrap();

                let mut reader = CommandReader { raw: tail };
                tx.send(reader.next(&mut sock).await).await.unwrap();
            },
            rules(
                r#"
                name: test
                aliases:
                  - pattern: '^exits$'
                    send: ["tell 0=${ROOM_EXITS.0} 1=${ROOM_EXITS.1}"]
                "#,
            ),
        );

        assert_eq!(next_line(&mut events).await, "msdp applied");

        commands
            .send(SessionCommand::SendLine("exits".into()))
            .await
            .unwrap();
        assert_eq!(next_sent(&mut sent).await, "tell 0=north 1=${ROOM_EXITS.1}");
    }

    /// Reads complete `IAC SB <option> … IAC SE` subnegotiations,
    /// IAC-unescaped, ignoring any other Telnet framing (like our own NAWS
    /// offer) mixed in with them. Keeps leftover bytes across calls, since
    /// two subnegotiations written back-to-back can arrive in one read.
    #[derive(Default)]
    struct SubnegReader {
        pending: Vec<u8>,
    }

    impl SubnegReader {
        async fn next(&mut self, sock: &mut tokio::net::TcpStream, option: u8) -> Vec<u8> {
            let mut buf = vec![0u8; 256];
            loop {
                if let Some(pos) = self.pending.windows(3).position(|w| w == [IAC, SB, option]) {
                    let mut data = Vec::new();
                    let mut i = pos + 3;
                    while i + 1 < self.pending.len() {
                        if self.pending[i] == IAC && self.pending[i + 1] == SE {
                            self.pending.drain(..i + 2);
                            return data;
                        }
                        if self.pending[i] == IAC && self.pending[i + 1] == IAC {
                            data.push(IAC);
                            i += 2;
                            continue;
                        }
                        data.push(self.pending[i]);
                        i += 1;
                    }
                }
                let n = sock.read(&mut buf).await.unwrap();
                assert!(n > 0, "client closed before sending the subnegotiation");
                self.pending.extend_from_slice(&buf[..n]);
            }
        }
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
        // The server hanging up is not the player leaving, so the session
        // lines up a reconnect rather than ending (§5).
        let (attempt, _delay, _reason) = next_reconnect(&mut events).await;
        assert_eq!(attempt, 1);
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
                SessionEvent::server("Welcome"),
                SessionEvent::server("\x1b[1;33mThe Grand Bazaar\x1b[0m"),
                SessionEvent::server("A merchant waves."),
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

    /// A corrupt stream cannot be recovered from — the connection drops
    /// with the inflate error as its reason rather than emitting garbage.
    #[tokio::test]
    async fn a_corrupt_compressed_stream_drops_the_connection() {
        let (mut events, _commands) = serve(|mut sock| async move {
            start_compression(&mut sock, b"\x78\x9c not a deflate block at all").await;
            idle(&mut sock).await;
        });

        let (_attempt, _delay, reason) = next_reconnect(&mut events).await;
        assert!(
            reason.contains("compressed stream is corrupt"),
            "unexpected end reason: {reason}"
        );
    }

    /// A decompression bomb needs no MUD account to fire — just a hostile
    /// address in a profile (§13). The connection drops instead of growing
    /// its buffer until the process dies.
    #[tokio::test]
    async fn a_decompression_bomb_drops_the_connection() {
        let (mut events, _commands) = serve(|mut sock| async move {
            start_compression(&mut sock, &deflate(&vec![0u8; 8 * 1024 * 1024])).await;
            idle(&mut sock).await;
        });

        let (_attempt, _delay, reason) = next_reconnect(&mut events).await;
        assert!(
            reason.contains("expanded past"),
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
            .filter(|ev| matches!(ev, SessionEvent::Line { .. } | SessionEvent::EchoMask(_)))
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
    /// Waits for the client to agree to an option, and hands back whatever
    /// it read past the agreement.
    ///
    /// Returning the tail rather than dropping it matters once the client
    /// says more than one thing after agreeing: the `REPORT` subscriptions
    /// follow the `DO` immediately, and a read that swallowed half of one
    /// used to leave the next reader starting mid-subnegotiation. It looked
    /// like `REPORT\x02MANA` welded onto the front of a command.
    async fn await_agreement(sock: &mut tokio::net::TcpStream, option: u8) -> Vec<u8> {
        let mut seen = Vec::new();
        let mut buf = vec![0u8; 64];
        loop {
            let n = sock.read(&mut buf).await.unwrap();
            assert!(n > 0, "client closed before agreeing to option {option}");
            seen.extend_from_slice(&buf[..n]);
            if let Some(at) = seen.windows(3).position(|w| w == [IAC, DO, option]) {
                return seen.split_off(at + 3);
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
    /// Reads until the client sends a complete line of actual input.
    ///
    /// Several commands can share one TCP segment (an alias expands to a
    /// list, `;` splits a typed line), so leftover bytes are kept for the
    /// next call rather than discarded.
    #[derive(Default)]
    struct CommandReader {
        /// Bytes as they arrived, Telnet framing and all. Stripping is done
        /// over the whole buffer rather than per read: a subnegotiation can
        /// straddle two reads, and stripping each read on its own leaves
        /// half of one in the output. Found when the client began sending
        /// seven `REPORT`s instead of one and they stopped fitting in a
        /// single read — `REPORT\x02MANA` turned up welded to the front of
        /// the next command.
        raw: Vec<u8>,
    }

    impl CommandReader {
        async fn next(&mut self, sock: &mut tokio::net::TcpStream) -> String {
            let mut buf = vec![0u8; 1024];
            loop {
                let (text, consumed) = strip_complete_telnet(&self.raw);
                if let Some(idx) = text.iter().position(|&b| b == b'\n') {
                    // Only the raw bytes behind this line are spent; the
                    // rest stays for the next call.
                    let spent = raw_len_for(&self.raw, idx + 1, consumed);
                    self.raw.drain(..spent);
                    return String::from_utf8_lossy(&text[..=idx]).trim().to_string();
                }
                let n = sock.read(&mut buf).await.unwrap();
                if n == 0 {
                    return String::new();
                }
                self.raw.extend_from_slice(&buf[..n]);
            }
        }
    }

    /// Strips Telnet framing, stopping at any sequence that is not yet
    /// complete. Returns the readable bytes and how much of `raw` they came
    /// from, so an unfinished subnegotiation waits for the rest rather than
    /// leaking its bytes into the output.
    fn strip_complete_telnet(raw: &[u8]) -> (Vec<u8>, usize) {
        let mut out = Vec::new();
        let mut i = 0;
        while i < raw.len() {
            if raw[i] != IAC {
                out.push(raw[i]);
                i += 1;
                continue;
            }
            match raw.get(i + 1) {
                None => break,
                Some(&IAC) => {
                    out.push(IAC);
                    i += 2;
                }
                Some(&SB) => {
                    let Some(end) = (i + 2..raw.len().saturating_sub(1))
                        .find(|&j| raw[j] == IAC && raw[j + 1] == SE)
                    else {
                        break;
                    };
                    i = end + 2;
                }
                Some(&(WILL | WONT | DO | 254)) => {
                    if raw.len() < i + 3 {
                        break;
                    }
                    i += 3;
                }
                Some(_) => i += 2,
            }
        }
        (out, i)
    }

    /// How many raw bytes produced the first `wanted` readable ones.
    fn raw_len_for(raw: &[u8], wanted: usize, consumed: usize) -> usize {
        for end in 0..=consumed {
            if strip_complete_telnet(&raw[..end]).0.len() >= wanted {
                return end;
            }
        }
        consumed
    }

    /// One-shot read of a single command, for scripts that only need one.
    async fn read_command(sock: &mut tokio::net::TcpStream) -> String {
        CommandReader::default().next(sock).await
    }

    /// "Press return to continue" is a real step in plenty of MUD login
    /// flows, so a bare Enter has to reach the server as an empty line
    /// rather than being treated as nothing typed.
    #[tokio::test]
    async fn a_bare_enter_reaches_the_server_as_an_empty_line() {
        let (tx, mut sent) = mpsc::channel(8);
        let (_events, commands) = serve(move |mut sock| async move {
            let mut reader = CommandReader::default();
            tx.send(reader.next(&mut sock).await).await.unwrap();
            tx.send(reader.next(&mut sock).await).await.unwrap();
        });

        commands
            .send(SessionCommand::SendLine(String::new()))
            .await
            .unwrap();
        commands
            .send(SessionCommand::SendLine("look".to_string()))
            .await
            .unwrap();

        let mut received = Vec::new();
        for _ in 0..2 {
            received.push(
                timeout(Duration::from_secs(2), sent.recv())
                    .await
                    .expect("timed out")
                    .unwrap(),
            );
        }
        assert_eq!(received, vec!["", "look"]);
    }

    /// The whole point, through a real socket: the server asks, the client
    /// answers, and the player never types either (docs/ARCHITECTURE.md §10).
    #[tokio::test]
    async fn auto_login_answers_the_opening_prompts_over_the_wire() {
        let (tx, mut sent) = mpsc::channel(8);
        let login = Autologin::new("Kestrel".into(), Some("hunter2".into()), None, None).unwrap();

        let (_events, _commands) = serve_with_login(
            move |mut sock| async move {
                let mut reader = CommandReader::default();
                sock.write_all(b"By what name are you known?\r\n")
                    .await
                    .unwrap();
                tx.send(reader.next(&mut sock).await).await.unwrap();
                sock.write_all(b"Password:\r\n").await.unwrap();
                tx.send(reader.next(&mut sock).await).await.unwrap();
            },
            Engine::default(),
            false,
            Some(login),
            PeerLinks::default(),
        );

        let mut received = Vec::new();
        for _ in 0..2 {
            received.push(
                timeout(Duration::from_secs(2), sent.recv())
                    .await
                    .expect("timed out")
                    .unwrap(),
            );
        }
        assert_eq!(received, vec!["Kestrel", "hunter2"]);
    }

    /// Auto-login's own progress notices are the client talking, not the
    /// MUD. They reach the pane as `Line` events like everything else, so
    /// only `origin` keeps them distinguishable once they have scrolled
    /// (§8, docs/UX_REVIEW.md D).
    #[tokio::test]
    async fn an_auto_login_notice_is_marked_as_the_clients_own() {
        let login = Autologin::new("Kestrel".into(), None, None, None).unwrap();

        let (mut events, _commands) = serve_with_login(
            move |mut sock| async move {
                sock.write_all(b"By what name are you known?\r\n")
                    .await
                    .unwrap();
                sock.write_all(b"Password:\r\n").await.unwrap();
                // Hold the connection open so the pane is not cut short.
                tokio::time::sleep(Duration::from_secs(5)).await;
            },
            Engine::default(),
            false,
            Some(login),
            PeerLinks::default(),
        );

        let (text, origin) = loop {
            let (text, origin) = next_line_with_origin(&mut events).await;
            if text.starts_with("auto-login:") {
                break (text, origin);
            }
        };
        assert!(text.contains("no password in the keyring"), "{text}");
        assert_eq!(origin, Origin::Client);
    }

    /// The empty line bypasses the alias splitter, which drops empty parts
    /// so that `a;;b` sends two commands — that must not swallow the bare
    /// Enter as well.
    #[tokio::test]
    async fn a_bare_enter_is_sent_even_with_aliases_loaded() {
        let (tx, mut sent) = mpsc::channel(8);
        let (_events, commands) = serve_with_rules(
            move |mut sock| async move {
                tx.send(read_command(&mut sock).await).await.unwrap();
            },
            rules(
                r#"
                name: test
                aliases:
                  - pattern: '^k$'
                    send: ["kill rat"]
                "#,
            ),
        );

        commands
            .send(SessionCommand::SendLine(String::new()))
            .await
            .unwrap();

        assert_eq!(
            timeout(Duration::from_secs(2), sent.recv())
                .await
                .expect("timed out")
                .unwrap(),
            ""
        );
    }

    // ---- cross-session actions (docs/ARCHITECTURE.md §7.5) ----

    /// A trigger's `send_to` leaves the session as an event for the hub —
    /// one session never touches another's transport directly.
    #[tokio::test]
    async fn a_trigger_send_to_is_raised_for_the_hub_to_route() {
        let (mut events, _commands) = serve_with_rules(
            |mut sock| async move {
                sock.write_all(b"HP: 30%\r\n").await.unwrap();
                std::future::pending::<()>().await;
            },
            rules(
                r#"
                name: test
                triggers:
                  - pattern: '^HP: (?P<hp>\d+)%'
                    send_to:
                      cleric: ["cast 'major heal' Grunk", "say healing ${hp}"]
                "#,
            ),
        );

        match next_matching(&mut events, |ev| matches!(ev, SessionEvent::SendTo { .. })).await {
            SessionEvent::SendTo {
                target,
                lines,
                hops,
            } => {
                assert_eq!(target, "cleric");
                assert_eq!(lines, vec!["cast 'major heal' Grunk", "say healing 30"]);
                assert_eq!(hops, 1, "a locally-raised action is the first hop");
            }
            other => panic!("expected SendTo, got {other:?}"),
        }
    }

    /// An alias can address another session too, and its captures expand
    /// the same way a `send` would.
    #[tokio::test]
    async fn an_alias_send_to_expands_captures() {
        let (mut events, commands) = serve_with_rules(
            |_sock| async { std::future::pending::<()>().await },
            rules(
                r#"
                name: test
                aliases:
                  - pattern: '^heal (?P<who>\w+)$'
                    send_to:
                      cleric: ["cast heal ${who}"]
                "#,
            ),
        );

        commands
            .send(SessionCommand::SendLine("heal Grunk".to_string()))
            .await
            .unwrap();

        match next_matching(&mut events, |ev| matches!(ev, SessionEvent::SendTo { .. })).await {
            SessionEvent::SendTo { target, lines, .. } => {
                assert_eq!(target, "cleric");
                assert_eq!(lines, vec!["cast heal Grunk"]);
            }
            other => panic!("expected SendTo, got {other:?}"),
        }
    }

    /// The receiver decides: with expansion off, an injected command goes to
    /// the server exactly as the sender wrote it — a sender can never make
    /// this session's aliases run.
    #[tokio::test]
    async fn an_injected_command_is_sent_verbatim_by_default() {
        let (tx, mut sent) = mpsc::channel(8);
        let (mut events, commands) = serve_with(
            move |mut sock| async move {
                tx.send(read_command(&mut sock).await).await.unwrap();
            },
            rules(
                r#"
                name: test
                aliases:
                  - pattern: '^hh$'
                    send: ["cast 'major heal' me"]
                "#,
            ),
            false,
        );

        commands
            .send(SessionCommand::Inject {
                from: "tank".to_string(),
                lines: vec!["hh".to_string()],
                hops: 1,
            })
            .await
            .unwrap();

        assert_eq!(
            timeout(Duration::from_secs(2), sent.recv())
                .await
                .expect("timed out")
                .unwrap(),
            "hh",
            "aliases must not run unless this session opted in"
        );
        // ...and it is echoed locally, so nothing happens invisibly — and
        // the pane records *which* session, not just the `[from tank]` text
        // that says so (§8).
        assert_eq!(
            next_line_with_origin(&mut events).await,
            (
                "[from tank] hh".to_string(),
                Origin::Session("tank".to_string())
            )
        );
    }

    /// With `expand_aliases: true` the receiver lets the sender use its own
    /// shorthands.
    #[tokio::test]
    async fn an_injected_command_runs_through_aliases_when_the_receiver_opts_in() {
        let (tx, mut sent) = mpsc::channel(8);
        let (_events, commands) = serve_with(
            move |mut sock| async move {
                tx.send(read_command(&mut sock).await).await.unwrap();
            },
            rules(
                r#"
                name: test
                aliases:
                  - pattern: '^hh$'
                    send: ["cast 'major heal' me"]
                "#,
            ),
            true,
        );

        commands
            .send(SessionCommand::Inject {
                from: "tank".to_string(),
                lines: vec!["hh".to_string()],
                hops: 1,
            })
            .await
            .unwrap();

        assert_eq!(
            timeout(Duration::from_secs(2), sent.recv())
                .await
                .expect("timed out")
                .unwrap(),
            "cast 'major heal' me"
        );
    }

    /// Loop safety: an injected command whose expansion addresses a third
    /// session carries a higher hop count, so a chain runs out (§7.5).
    #[tokio::test]
    async fn an_injection_that_bounces_onward_counts_a_hop() {
        let (mut events, commands) = serve_with(
            |_sock| async { std::future::pending::<()>().await },
            rules(
                r#"
                name: test
                aliases:
                  - pattern: '^relay$'
                    send_to:
                      mage: ["shield tank"]
                "#,
            ),
            true,
        );

        commands
            .send(SessionCommand::Inject {
                from: "tank".to_string(),
                lines: vec!["relay".to_string()],
                hops: 1,
            })
            .await
            .unwrap();

        match next_matching(&mut events, |ev| matches!(ev, SessionEvent::SendTo { .. })).await {
            SessionEvent::SendTo { hops, target, .. } => {
                assert_eq!(target, "mage");
                assert_eq!(hops, 2, "one hop further than the injection that caused it");
            }
            other => panic!("expected SendTo, got {other:?}"),
        }
    }

    // ---- channel routing (docs/ARCHITECTURE.md §11.1) ----

    /// A routed line reaches the channel pane, and the gag that
    /// `keep_in_main: false` compiles to keeps it out of the main scrollback.
    #[tokio::test]
    async fn a_routed_line_goes_to_its_channel_and_not_to_main() {
        let (mut events, _commands) = serve_with_rules(
            |mut sock| async move {
                sock.write_all(b"Bob tells you hi\r\nYou see a rat.\r\n")
                    .await
                    .unwrap();
                std::future::pending::<()>().await;
            },
            rules(
                r#"
                name: test
                triggers:
                  - pattern: 'tells you'
                    route: comms
                    gag: true
                "#,
            ),
        );

        match next_matching(&mut events, |ev| matches!(ev, SessionEvent::Route { .. })).await {
            SessionEvent::Route { channel, text } => {
                assert_eq!(channel, "comms");
                assert_eq!(text, "Bob tells you hi");
            }
            other => panic!("expected Route, got {other:?}"),
        }
        // The routed trigger was also this session's first fire, so the
        // one-time hint (UX_REVIEW.md G) lands next.
        let (hint, origin) = next_line_with_origin(&mut events).await;
        assert_eq!(origin, Origin::Client);
        assert!(hint.contains("trigger"));
        // The next real line the pane sees is the un-routed one: the tell
        // was moved, not copied.
        assert_eq!(next_line(&mut events).await, "You see a rat.");
    }

    /// `keep_in_main: true` compiles to a routed trigger with no gag, so the
    /// line is mirrored to both panes.
    #[tokio::test]
    async fn a_copied_channel_line_also_stays_in_main() {
        let (mut events, _commands) = serve_with_rules(
            |mut sock| async move {
                sock.write_all(b"Bob tells you hi\r\n").await.unwrap();
                std::future::pending::<()>().await;
            },
            rules(
                r#"
                name: test
                triggers:
                  - pattern: 'tells you'
                    route: comms
                    gag: false
                "#,
            ),
        );

        assert!(matches!(
            next_matching(&mut events, |ev| matches!(ev, SessionEvent::Route { .. })).await,
            SessionEvent::Route { .. }
        ));
        assert_eq!(next_line(&mut events).await, "Bob tells you hi");
    }

    // ---- highlights (docs/ARCHITECTURE.md §7.7) ----

    /// The engine matched the stripped line, but what reaches the pane is
    /// the raw line with the span spliced into it — and the channel copy
    /// is the same text, since a routed line is not a second rendering.
    #[tokio::test]
    async fn a_highlight_is_spliced_into_the_line_and_its_channel_copy() {
        let (mut events, _commands) = serve_with_rules(
            |mut sock| async move {
                sock.write_all(b"\x1b[32mBob tells you hi\x1b[0m\r\n")
                    .await
                    .unwrap();
                std::future::pending::<()>().await;
            },
            rules(
                r#"
                name: test
                triggers:
                  - pattern: '\bBob\b'
                    highlight: {fg: bright_yellow, bold: true}
                    route: comms
                "#,
            ),
        );

        let expected = "\x1b[32m\x1b[1;93mBob\x1b[0m\x1b[32m tells you hi\x1b[0m";
        match next_matching(&mut events, |ev| matches!(ev, SessionEvent::Route { .. })).await {
            SessionEvent::Route { channel, text } => {
                assert_eq!(channel, "comms");
                assert_eq!(text, expected);
            }
            other => panic!("expected Route, got {other:?}"),
        }
        assert_eq!(next_line(&mut events).await, expected);
    }

    /// A connection the player did not end is worth another attempt, and
    /// the wait grows between attempts (docs/ARCHITECTURE.md §5).
    #[tokio::test]
    async fn a_dropped_connection_is_retried_with_a_growing_backoff() {
        // The listener goes away with the script, so every retry after
        // this first connection is refused and the backoff keeps stepping.
        let (mut events, _commands) = serve(|_sock| async move {});

        let (attempt, delay, _reason) = next_reconnect(&mut events).await;
        assert_eq!((attempt, delay), (1, Duration::from_secs(1)));
        let (attempt, delay, reason) = next_reconnect(&mut events).await;
        assert_eq!(
            (attempt, delay),
            (2, Duration::from_secs(2)),
            "a second failure waits twice as long: {reason}"
        );
    }

    /// Backoff describes one outage, not the session's whole life: a
    /// connection that comes back resets the wait for the next drop.
    #[tokio::test]
    async fn a_successful_reconnect_resets_the_backoff() {
        let (mut events, _commands) = revolving_door(|_sock| {});

        let (attempt, delay, _reason) = next_reconnect(&mut events).await;
        assert_eq!((attempt, delay), (1, Duration::from_secs(1)));
        let (attempt, delay, _reason) = next_reconnect(&mut events).await;
        assert_eq!(
            (attempt, delay),
            (1, Duration::from_secs(1)),
            "the reconnect succeeded, so the next drop starts over"
        );
    }

    /// The one ending that is not a dropped connection: an explicit
    /// disconnect stops the session instead of arming a retry.
    #[tokio::test]
    async fn an_explicit_disconnect_is_not_retried() {
        let (mut events, commands) = serve(|mut sock| async move { idle(&mut sock).await });
        commands.send(SessionCommand::Disconnect).await.unwrap();

        let event = next_matching(&mut events, |ev| {
            matches!(
                ev,
                SessionEvent::Ended(_) | SessionEvent::Reconnecting { .. }
            )
        })
        .await;
        assert!(
            matches!(event, SessionEvent::Ended(_)),
            "the player left; nothing should be retried: {event:?}"
        );
    }

    /// Reconnecting is a fresh connection to the same character: scripts
    /// get their `on_connect` again, as they do on `/reload` (§7.4).
    #[cfg(feature = "lua")]
    #[tokio::test]
    async fn on_connect_runs_again_after_a_reconnect() {
        let (tx, mut sent) = mpsc::channel(8);
        let (_events, _commands) = revolving_door_with_rules(
            move |mut sock| {
                let tx = tx.clone();
                tokio::spawn(async move {
                    let _ = tx.send(read_command(&mut sock).await).await;
                });
            },
            scripted_rules(r#"mud.on_connect(function() mud.send("look") end)"#),
        );

        for _ in 0..2 {
            assert_eq!(
                timeout(Duration::from_secs(3), sent.recv())
                    .await
                    .expect("timed out waiting for on_connect")
                    .unwrap(),
                "look"
            );
        }
    }

    #[test]
    fn backoff_doubles_up_to_the_cap() {
        assert_eq!(backoff_delay(1), RECONNECT_BASE);
        assert_eq!(backoff_delay(2), Duration::from_secs(2));
        assert_eq!(backoff_delay(3), Duration::from_secs(4));
        assert_eq!(backoff_delay(7), RECONNECT_CAP, "64s is past the ceiling");
        assert_eq!(backoff_delay(u32::MAX), RECONNECT_CAP, "and it stays there");
    }

    /// A server that keeps answering but never keeps a connection: each
    /// accept is handed to `greet` and then dropped, so the session drops
    /// and reconnects for as long as the test watches it.
    fn revolving_door<F>(greet: F) -> (mpsc::Receiver<SessionEvent>, mpsc::Sender<SessionCommand>)
    where
        F: FnMut(tokio::net::TcpStream) + Send + 'static,
    {
        revolving_door_with_rules(greet, Engine::default())
    }

    fn revolving_door_with_rules<F>(
        mut greet: F,
        engine: Engine,
    ) -> (mpsc::Receiver<SessionEvent>, mpsc::Sender<SessionCommand>)
    where
        F: FnMut(tokio::net::TcpStream) + Send + 'static,
    {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            let listener = TcpListener::from_std(listener).unwrap();
            while let Ok((sock, _)) = listener.accept().await {
                greet(sock);
            }
        });

        spawn(
            "127.0.0.1".to_string(),
            port,
            None,
            None,
            Charset::Utf8,
            engine,
            false,
            None,
            PeerLinks::default(),
        )
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
        serve_with(script, engine, false)
    }

    /// As `serve_with_rules`, but choosing whether injected commands run
    /// through this session's aliases (§7.5).
    fn serve_with<F, Fut>(
        script: F,
        engine: Engine,
        expand_injected: bool,
    ) -> (mpsc::Receiver<SessionEvent>, mpsc::Sender<SessionCommand>)
    where
        F: FnOnce(tokio::net::TcpStream) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        serve_with_login(script, engine, expand_injected, None, PeerLinks::default())
    }

    /// As `serve_with`, with an auto-login machine driving the opening
    /// exchange (docs/ARCHITECTURE.md §10), and this session's half of the
    /// peer mesh (§7.5).
    fn serve_with_login<F, Fut>(
        script: F,
        engine: Engine,
        expand_injected: bool,
        login: Option<Autologin>,
        peers: PeerLinks,
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
            expand_injected,
            login,
            peers,
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

    async fn next_latency(events: &mut mpsc::Receiver<SessionEvent>) -> Duration {
        match next_matching(events, |ev| matches!(ev, SessionEvent::Latency(_))).await {
            SessionEvent::Latency(rtt) => rtt,
            _ => unreachable!(),
        }
    }

    async fn next_line_with_origin(events: &mut mpsc::Receiver<SessionEvent>) -> (String, Origin) {
        match next_matching(events, |ev| matches!(ev, SessionEvent::Line { .. })).await {
            SessionEvent::Line { text, origin } => (text, origin),
            _ => unreachable!(),
        }
    }

    async fn next_line(events: &mut mpsc::Receiver<SessionEvent>) -> String {
        match next_matching(events, |ev| matches!(ev, SessionEvent::Line { .. })).await {
            SessionEvent::Line { text, .. } => text,
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

    async fn next_reconnect(events: &mut mpsc::Receiver<SessionEvent>) -> (u32, Duration, String) {
        match next_matching(events, |ev| matches!(ev, SessionEvent::Reconnecting { .. })).await {
            SessionEvent::Reconnecting {
                attempt,
                delay,
                reason,
            } => (attempt, delay, reason),
            _ => unreachable!(),
        }
    }

    async fn next_mask(events: &mut mpsc::Receiver<SessionEvent>) -> bool {
        match next_matching(events, |ev| matches!(ev, SessionEvent::EchoMask(_))).await {
            SessionEvent::EchoMask(masked) => masked,
            _ => unreachable!(),
        }
    }

    /// Waits for the next room-change event and unpacks it, for tests that
    /// only care about the id and how the client says it got there (§16).
    async fn next_room(
        events: &mut mpsc::Receiver<SessionEvent>,
    ) -> (crate::map::RoomId, Option<String>) {
        match next_matching(events, |ev| matches!(ev, SessionEvent::Room { .. })).await {
            SessionEvent::Room { info, arrived_via } => (info.id, arrived_via),
            _ => unreachable!(),
        }
    }

    /// A movement, in whatever spelling the player used, accumulates on
    /// `outstanding` in its canonical short form — `room_event` only ever
    /// deals in that form, so a server-spelled `north` has to normalise
    /// here to line up with it later.
    #[test]
    fn note_outbound_pushes_canonicalised_movements_and_accumulates_them() {
        let now = Instant::now();
        let mut outstanding = Vec::new();
        note_outbound(&mut outstanding, "north", now);
        note_outbound(&mut outstanding, "e", now);
        let directions: Vec<&str> = outstanding.iter().map(|(d, _)| d.as_str()).collect();
        assert_eq!(directions, vec!["n", "e"]);
    }

    /// A command that is *not* a step is the likeliest explanation for a
    /// room change right after it — a recall, a portal, a trigger's
    /// teleport — so crediting that change to a movement sent earlier
    /// would invent an edge the world does not have; the outstanding list
    /// is cleared instead.
    #[test]
    fn note_outbound_clears_on_a_non_movement() {
        let now = Instant::now();
        let mut outstanding = vec![("n".to_string(), now)];
        note_outbound(&mut outstanding, "look", now);
        assert!(outstanding.is_empty());

        let mut outstanding = vec![("n".to_string(), now)];
        note_outbound(&mut outstanding, "", now);
        assert!(outstanding.is_empty());
    }

    /// Still being in the same room is not an arrival, even when a step
    /// was in flight: a refused step (a wall) leaves it still waiting
    /// rather than spent on a room the character never left.
    #[test]
    fn room_event_is_none_when_the_room_has_not_changed_and_leaves_outstanding_alone() {
        let info = crate::map::RoomInfo {
            id: crate::map::RoomId(1),
            name: None,
            area: None,
            exits: Default::default(),
        };
        let mut last_room = Some(crate::map::RoomId(1));
        let now = Instant::now();
        let mut outstanding = vec![("n".to_string(), now)];
        assert_eq!(
            room_event(info, &mut last_room, &mut outstanding, now),
            None
        );
        assert_eq!(outstanding, vec![("n".to_string(), now)]);
    }

    /// Exactly one outstanding movement is the only case honest enough to
    /// name: the client can actually be sure which step led here.
    #[test]
    fn room_event_credits_a_single_outstanding_movement() {
        let info = crate::map::RoomInfo {
            id: crate::map::RoomId(2),
            name: None,
            area: None,
            exits: Default::default(),
        };
        let mut last_room = Some(crate::map::RoomId(1));
        let now = Instant::now();
        let mut outstanding = vec![("n".to_string(), now)];
        let event = room_event(info.clone(), &mut last_room, &mut outstanding, now);
        assert_eq!(
            event,
            Some(SessionEvent::Room {
                info: Box::new(info),
                arrived_via: Some("n".to_string()),
            })
        );
        assert!(outstanding.is_empty());
        assert_eq!(last_room, Some(crate::map::RoomId(2)));
    }

    /// Something else moved the character while a step was outstanding —
    /// a summon, a wimpy auto-flee, a portal. The step never resolved, so
    /// crediting it with the room that turned up writes an edge the world
    /// does not have. Time is the only signal available without the server
    /// saying so: a step resolves in about a round trip, or it did not
    /// happen (§16).
    #[test]
    fn a_step_that_never_resolved_is_not_credited_to_a_later_arrival() {
        let sent = Instant::now();
        let mut outstanding = vec![("n".to_string(), sent)];
        let mut last_room = Some(crate::map::RoomId(1));
        let info = crate::map::RoomInfo {
            id: crate::map::RoomId(500),
            name: None,
            area: None,
            exits: Default::default(),
        };

        let event = room_event(
            info,
            &mut last_room,
            &mut outstanding,
            sent + MOVEMENT_SETTLE + Duration::from_secs(1),
        )
        .expect("the room still changed, so it is still reported");

        match event {
            SessionEvent::Room { arrived_via, .. } => assert_eq!(
                arrived_via, None,
                "a step this stale cannot be what moved the character"
            ),
            other => panic!("expected Room, got {other:?}"),
        }
        assert_eq!(
            last_room,
            Some(crate::map::RoomId(500)),
            "but where they now are is still tracked"
        );
    }

    /// Zero or several outstanding movements are both too ambiguous to
    /// name: a speedwalk puts every step on the wire before the first room
    /// update comes back, and pairing those in arrival order assumes each
    /// one succeeded — a wrong edge is a lie the map keeps telling, so
    /// nothing is claimed either way. Both still clear `outstanding`: it
    /// was spent on this arrival either way, credited or not.
    #[test]
    fn room_event_does_not_credit_zero_or_multiple_outstanding_movements() {
        let info = crate::map::RoomInfo {
            id: crate::map::RoomId(2),
            name: None,
            area: None,
            exits: Default::default(),
        };

        let now = Instant::now();
        let mut last_room = Some(crate::map::RoomId(1));
        let mut outstanding: Vec<(String, Instant)> = Vec::new();
        let event = room_event(info.clone(), &mut last_room, &mut outstanding, now);
        assert!(matches!(
            event,
            Some(SessionEvent::Room {
                arrived_via: None,
                ..
            })
        ));
        assert!(outstanding.is_empty());

        let mut last_room = Some(crate::map::RoomId(1));
        let mut outstanding = vec![("n".to_string(), now), ("n".to_string(), now)];
        let event = room_event(info, &mut last_room, &mut outstanding, now);
        assert!(matches!(
            event,
            Some(SessionEvent::Room {
                arrived_via: None,
                ..
            })
        ));
        assert!(outstanding.is_empty());
    }

    /// The room event is not read out of the GMCP message itself — it's
    /// read back out of the merged server-data store the message just fed
    /// (§6.3, §16). Exercises the whole path: negotiation, a `Room.Info`
    /// payload, and the event the hub actually sees.
    #[tokio::test]
    async fn gmcp_room_reaches_the_hub_as_a_room_event() {
        let (mut events, _commands) = serve_with_rules(
            move |mut sock| async move {
                sock.write_all(&[IAC, WILL, option::GMCP]).await.unwrap();

                let mut subnegs = SubnegReader::default();
                subnegs.next(&mut sock, option::GMCP).await; // Core.Hello
                subnegs.next(&mut sock, option::GMCP).await; // Core.Supports.Set

                let room = encode_subnegotiation(
                    option::GMCP,
                    br#"Room.Info {"num":12345,"name":"Temple Square","area":"Midgaard"}"#,
                );
                sock.write_all(&room).await.unwrap();
            },
            rules("name: test"),
        );

        let (id, arrived_via) = next_room(&mut events).await;
        assert_eq!(id, crate::map::RoomId(12345));
        assert_eq!(arrived_via, None);
    }

    /// The regression guard for a design bug where only GMCP could ever
    /// produce a map: the room event has to come from the merged
    /// server-data store rather than a GMCP-specific message, or an
    /// MSDP-only MUD would raise no `Room` event at all and never map
    /// (§6.3, §16). MSDP has no negotiation handshake of its own, so a
    /// server can push room data as soon as the option is enabled.
    #[tokio::test]
    async fn msdp_room_reaches_the_hub_as_a_room_event() {
        let (mut events, _commands) = serve_with_rules(
            move |mut sock| async move {
                sock.write_all(&[IAC, WILL, option::MSDP]).await.unwrap();
                await_agreement(&mut sock, option::MSDP).await;

                let mut payload = vec![msdp::VAR];
                payload.extend_from_slice(b"ROOM_VNUM");
                payload.push(msdp::VAL);
                payload.extend_from_slice(b"777");
                payload.push(msdp::VAR);
                payload.extend_from_slice(b"ROOM_NAME");
                payload.push(msdp::VAL);
                payload.extend_from_slice(b"The Vault");
                let msg = encode_subnegotiation(option::MSDP, &payload);
                sock.write_all(&msg).await.unwrap();
            },
            rules("name: test"),
        );

        let (id, arrived_via) = next_room(&mut events).await;
        assert_eq!(id, crate::map::RoomId(777));
        assert_eq!(arrived_via, None);
    }

    /// Exactly one command sent between two room sightings is unambiguous:
    /// the client can be sure which step led here, so the edge is worth
    /// recording rather than left a gap (§16).
    #[tokio::test]
    async fn a_single_step_is_credited_to_the_room_it_leads_to() {
        let (mut events, commands) = serve_with_rules(
            move |mut sock| async move {
                sock.write_all(&[IAC, WILL, option::GMCP]).await.unwrap();

                let mut subnegs = SubnegReader::default();
                subnegs.next(&mut sock, option::GMCP).await; // Core.Hello
                subnegs.next(&mut sock, option::GMCP).await; // Core.Supports.Set

                let room_a =
                    encode_subnegotiation(option::GMCP, br#"Room.Info {"num":1,"name":"A"}"#);
                sock.write_all(&room_a).await.unwrap();

                read_command(&mut sock).await; // "n"

                let room_b =
                    encode_subnegotiation(option::GMCP, br#"Room.Info {"num":2,"name":"B"}"#);
                sock.write_all(&room_b).await.unwrap();
            },
            rules("name: test"),
        );

        let (first, _) = next_room(&mut events).await;
        assert_eq!(first, crate::map::RoomId(1));

        commands
            .send(SessionCommand::SendLine("n".into()))
            .await
            .unwrap();

        let (second, arrived_via) = next_room(&mut events).await;
        assert_eq!(second, crate::map::RoomId(2));
        assert_eq!(arrived_via, Some("n".to_string()));
    }

    /// A speedwalk puts every step on the wire before the first room
    /// update comes back, so pairing them in arrival order would assume
    /// each one succeeded — a single wall part-way through would misrecord
    /// every edge after it. Learning nothing from a speedwalk beats
    /// learning it wrong, so `arrived_via` stays unset rather than
    /// guessing (§16).
    #[tokio::test]
    async fn an_ambiguous_multi_step_walk_is_not_credited() {
        let (mut events, commands) = serve_with_rules(
            move |mut sock| async move {
                sock.write_all(&[IAC, WILL, option::GMCP]).await.unwrap();

                let mut subnegs = SubnegReader::default();
                subnegs.next(&mut sock, option::GMCP).await; // Core.Hello
                subnegs.next(&mut sock, option::GMCP).await; // Core.Supports.Set

                let room_a =
                    encode_subnegotiation(option::GMCP, br#"Room.Info {"num":1,"name":"A"}"#);
                sock.write_all(&room_a).await.unwrap();

                let mut reader = CommandReader::default();
                reader.next(&mut sock).await; // "n"
                reader.next(&mut sock).await; // "n"

                let room_b =
                    encode_subnegotiation(option::GMCP, br#"Room.Info {"num":2,"name":"B"}"#);
                sock.write_all(&room_b).await.unwrap();
            },
            rules("name: test"),
        );

        let (first, _) = next_room(&mut events).await;
        assert_eq!(first, crate::map::RoomId(1));

        commands
            .send(SessionCommand::SendLine(".2n".into()))
            .await
            .unwrap();

        let (second, arrived_via) = next_room(&mut events).await;
        assert_eq!(second, crate::map::RoomId(2));
        assert_eq!(arrived_via, None);
    }
}
