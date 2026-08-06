//! Telnet (RFC 854) byte-stream state machine.
//!
//! Handles IAC escaping, WILL/WONT/DO/DONT with RFC 1143 Q-method option
//! state (so a confused or hostile peer cannot drive a negotiation loop),
//! SB…SE subnegotiation framing (with embedded IAC IAC unescaping), and
//! GA/EOR prompt boundaries.
//!
//! Sans-IO (docs/ARCHITECTURE.md §6.1): replies are queued internally and
//! drained by the caller with [`TelnetMachine::take_output`]; the machine
//! never touches a socket.

use bytes::{Bytes, BytesMut};

pub mod option {
    pub const ECHO: u8 = 1;
    pub const SGA: u8 = 3;
    pub const TTYPE: u8 = 24;
    pub const EOR: u8 = 25;
    pub const NAWS: u8 = 31;
    pub const CHARSET: u8 = 42;
    /// Negotiated in M6.
    #[allow(dead_code)]
    pub const MSDP: u8 = 69;
    pub const MCCP2: u8 = 86;
    /// Deliberately never offered: outbound volume does not justify it.
    #[allow(dead_code)]
    pub const MCCP3: u8 = 87;
    /// Negotiated in M6.
    #[allow(dead_code)]
    pub const GMCP: u8 = 201;
}

const IAC: u8 = 255;
const DONT: u8 = 254;
const DO: u8 = 253;
const WONT: u8 = 252;
const WILL: u8 = 251;
const SB: u8 = 250;
const GA: u8 = 249;
const SE: u8 = 240;
const EOR_CMD: u8 = 239;

/// TTYPE subnegotiation commands (RFC 1091).
const TTYPE_IS: u8 = 0;
const TTYPE_SEND: u8 = 1;

/// What we report through the TTYPE/MTTS cycle (docs/ARCHITECTURE.md §6.2).
const CLIENT_NAME: &str = "mudular";
const TERMINAL_TYPE: &str = "xterm-256color";
/// MTTS bitvector: ANSI(1) | UTF-8(4) | 256 COLORS(8) | TRUECOLOR(256).
const MTTS_BITVECTOR: u32 = 1 | 4 | 8 | 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Will,
    Wont,
    Do,
    Dont,
}

impl Verb {
    fn to_byte(self) -> u8 {
        match self {
            Verb::Will => WILL,
            Verb::Wont => WONT,
            Verb::Do => DO,
            Verb::Dont => DONT,
        }
    }
}

/// Which end of the connection an option applies to: `Local` is what we do
/// (WILL/WONT), `Remote` is what the server does (DO/DONT).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Local,
    Remote,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TelnetEvent {
    /// Application bytes with all Telnet framing removed.
    Data(Bytes),
    /// GA or EOR: everything since the last newline is a prompt.
    PromptBoundary,
    /// An option finished negotiating into the enabled state.
    OptionEnabled { option: u8, side: Side },
    /// An option finished negotiating into the disabled state.
    OptionDisabled { option: u8, side: Side },
    /// A complete IAC SB <option> … IAC SE payload, IAC-unescaped.
    /// TTYPE and CHARSET are answered internally and never surface here.
    Subnegotiation { option: u8, data: Bytes },
    /// The server's MCCP2 subnegotiation: every inbound byte after it is
    /// zlib-compressed (§6.4). The machine stops consuming at this point;
    /// the rest of the read is held for [`TelnetMachine::take_deferred`].
    CompressionStart,
    /// The server's CHARSET REQUEST offered UTF-8 (or didn't); either way
    /// we already answered ACCEPTED/REJECTED. Decoding falls back to the
    /// profile-configured charset regardless (docs/ARCHITECTURE.md §9.2) —
    /// this is informational, for the status bar.
    CharsetResult { accepted: bool },
    /// Any other single-byte IAC command (NOP, AYT, …).
    Command(u8),
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum State {
    #[default]
    Data,
    Iac,
    Verb(Verb),
    SubOption,
    Sub,
    SubIac,
}

/// RFC 1143 per-option negotiation state ("Q method"). The `*Opposite`
/// variants are the queued-opposite-request states; keeping them explicit
/// is what stops WILL/DO ping-pong loops.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Q {
    #[default]
    No,
    Yes,
    WantNo,
    WantNoOpposite,
    WantYes,
    /// Only reachable by withdrawing an offer we have already sent, which
    /// needs a "request local disable" entry point this client has no use
    /// for yet. The arms that read it are kept so the table stays a
    /// complete transcription of RFC 1143.
    #[allow(dead_code)]
    WantYesOpposite,
}

#[derive(Debug, Default, Clone, Copy)]
struct OptionState {
    /// Whether the option is enabled on our side (WILL/WONT).
    us: Q,
    /// Whether the option is enabled on the server's side (DO/DONT).
    him: Q,
}

/// Options we let the server enable on its side (server WILL → we DO).
fn accept_remote(option: u8) -> bool {
    matches!(
        option,
        option::ECHO | option::SGA | option::EOR | option::CHARSET | option::MCCP2
    )
}

/// Options we are willing to enable on our side (server DO → we WILL).
fn accept_local(option: u8) -> bool {
    matches!(
        option,
        option::SGA | option::TTYPE | option::NAWS | option::CHARSET
    )
}

#[derive(Debug)]
pub struct TelnetMachine {
    state: State,
    data: BytesMut,
    sub_option: u8,
    sub_data: BytesMut,
    options: [OptionState; 256],
    out: BytesMut,
    /// Bytes of the current read that follow an MCCP2 subnegotiation and
    /// are therefore compressed rather than Telnet-framed.
    deferred: BytesMut,
    /// Position in the TTYPE/MTTS reply cycle.
    ttype_index: usize,
    window: Option<(u16, u16)>,
}

impl Default for TelnetMachine {
    fn default() -> Self {
        Self {
            state: State::default(),
            data: BytesMut::new(),
            sub_option: 0,
            sub_data: BytesMut::new(),
            options: [OptionState::default(); 256],
            out: BytesMut::new(),
            deferred: BytesMut::new(),
            ttype_index: 0,
            window: None,
        }
    }
}

impl TelnetMachine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Consume raw inbound bytes, producing events in stream order.
    /// Incomplete sequences are held across calls. Any negotiation replies
    /// are queued for [`Self::take_output`].
    pub fn feed(&mut self, input: &[u8]) -> Vec<TelnetEvent> {
        let mut events = Vec::new();
        for (index, &byte) in input.iter().enumerate() {
            match self.state {
                State::Data => {
                    if byte == IAC {
                        self.flush_data(&mut events);
                        self.state = State::Iac;
                    } else {
                        self.data.extend_from_slice(&[byte]);
                    }
                }
                State::Iac => match byte {
                    IAC => {
                        self.data.extend_from_slice(&[IAC]);
                        self.state = State::Data;
                    }
                    WILL => self.state = State::Verb(Verb::Will),
                    WONT => self.state = State::Verb(Verb::Wont),
                    DO => self.state = State::Verb(Verb::Do),
                    DONT => self.state = State::Verb(Verb::Dont),
                    SB => self.state = State::SubOption,
                    GA | EOR_CMD => {
                        events.push(TelnetEvent::PromptBoundary);
                        self.state = State::Data;
                    }
                    other => {
                        events.push(TelnetEvent::Command(other));
                        self.state = State::Data;
                    }
                },
                State::Verb(verb) => {
                    match verb {
                        Verb::Will => self.recv_will(byte, &mut events),
                        Verb::Wont => self.recv_wont(byte, &mut events),
                        Verb::Do => self.recv_do(byte, &mut events),
                        Verb::Dont => self.recv_dont(byte, &mut events),
                    }
                    self.state = State::Data;
                }
                State::SubOption => {
                    self.sub_option = byte;
                    self.sub_data.clear();
                    self.state = State::Sub;
                }
                State::Sub => {
                    if byte == IAC {
                        self.state = State::SubIac;
                    } else {
                        self.sub_data.extend_from_slice(&[byte]);
                    }
                }
                State::SubIac => match byte {
                    IAC => {
                        self.sub_data.extend_from_slice(&[IAC]);
                        self.state = State::Sub;
                    }
                    SE => {
                        let option = self.sub_option;
                        let data = self.sub_data.split().freeze();
                        self.state = State::Data;
                        if option == option::TTYPE {
                            self.answer_ttype(&data);
                        } else if option == option::CHARSET {
                            self.answer_charset(&data, &mut events);
                        } else if option == option::MCCP2 {
                            // Everything past this byte is zlib, so it is
                            // not ours to parse: hand it back (§6.4).
                            events.push(TelnetEvent::CompressionStart);
                            self.deferred.extend_from_slice(&input[index + 1..]);
                            return events;
                        } else {
                            events.push(TelnetEvent::Subnegotiation { option, data });
                        }
                    }
                    // Malformed: keep both bytes as subnegotiation payload
                    // rather than corrupting the outer stream.
                    other => {
                        self.sub_data.extend_from_slice(&[IAC, other]);
                        self.state = State::Sub;
                    }
                },
            }
        }
        self.flush_data(&mut events);
        events
    }

    /// Drain queued outbound bytes (negotiation replies, subnegotiations).
    pub fn take_output(&mut self) -> Bytes {
        self.out.split().freeze()
    }

    /// Drain the still-compressed tail of the read that turned compression
    /// on. The caller pushes it back through the inflate stage and feeds
    /// the result here again (§6.4).
    pub fn take_deferred(&mut self) -> Bytes {
        self.deferred.split().freeze()
    }

    /// Offer an option on our side (RFC 1143 "ask to enable local").
    pub fn request_local_enable(&mut self, option: u8) {
        match self.options[option as usize].us {
            Q::No => {
                self.options[option as usize].us = Q::WantYes;
                self.send(Verb::Will, option);
            }
            Q::WantNo => self.options[option as usize].us = Q::WantNoOpposite,
            Q::WantYesOpposite => self.options[option as usize].us = Q::WantYes,
            // Already enabled, or already asking: nothing to send.
            Q::Yes | Q::WantNoOpposite | Q::WantYes => {}
        }
    }

    /// Record the current window size, sending NAWS if it is negotiated.
    /// This is the pane size, not the whole terminal (§6.2).
    pub fn set_window_size(&mut self, cols: u16, rows: u16) {
        if self.window == Some((cols, rows)) {
            return;
        }
        self.window = Some((cols, rows));
        self.send_naws();
    }

    fn recv_will(&mut self, option: u8, events: &mut Vec<TelnetEvent>) {
        match self.options[option as usize].him {
            Q::No => {
                if accept_remote(option) {
                    self.options[option as usize].him = Q::Yes;
                    self.send(Verb::Do, option);
                    events.push(TelnetEvent::OptionEnabled {
                        option,
                        side: Side::Remote,
                    });
                } else {
                    self.send(Verb::Dont, option);
                }
            }
            Q::Yes => {}
            // Error per RFC 1143: DONT answered by WILL.
            Q::WantNo => self.options[option as usize].him = Q::No,
            Q::WantNoOpposite | Q::WantYes => {
                self.options[option as usize].him = Q::Yes;
                events.push(TelnetEvent::OptionEnabled {
                    option,
                    side: Side::Remote,
                });
            }
            Q::WantYesOpposite => {
                self.options[option as usize].him = Q::WantNo;
                self.send(Verb::Dont, option);
            }
        }
    }

    fn recv_wont(&mut self, option: u8, events: &mut Vec<TelnetEvent>) {
        match self.options[option as usize].him {
            Q::No => {}
            Q::Yes => {
                self.options[option as usize].him = Q::No;
                self.send(Verb::Dont, option);
                events.push(TelnetEvent::OptionDisabled {
                    option,
                    side: Side::Remote,
                });
            }
            Q::WantNo | Q::WantYes | Q::WantYesOpposite => {
                self.options[option as usize].him = Q::No;
            }
            Q::WantNoOpposite => {
                self.options[option as usize].him = Q::WantYes;
                self.send(Verb::Do, option);
            }
        }
    }

    fn recv_do(&mut self, option: u8, events: &mut Vec<TelnetEvent>) {
        match self.options[option as usize].us {
            Q::No => {
                if accept_local(option) {
                    self.options[option as usize].us = Q::Yes;
                    self.send(Verb::Will, option);
                    self.on_local_enabled(option, events);
                } else {
                    self.send(Verb::Wont, option);
                }
            }
            Q::Yes => {}
            // Error per RFC 1143: WONT answered by DO.
            Q::WantNo => self.options[option as usize].us = Q::No,
            Q::WantNoOpposite | Q::WantYes => {
                self.options[option as usize].us = Q::Yes;
                self.on_local_enabled(option, events);
            }
            Q::WantYesOpposite => {
                self.options[option as usize].us = Q::WantNo;
                self.send(Verb::Wont, option);
            }
        }
    }

    fn recv_dont(&mut self, option: u8, events: &mut Vec<TelnetEvent>) {
        match self.options[option as usize].us {
            Q::No => {}
            Q::Yes => {
                self.options[option as usize].us = Q::No;
                self.send(Verb::Wont, option);
                events.push(TelnetEvent::OptionDisabled {
                    option,
                    side: Side::Local,
                });
            }
            Q::WantNo | Q::WantYes | Q::WantYesOpposite => {
                self.options[option as usize].us = Q::No;
            }
            Q::WantNoOpposite => {
                self.options[option as usize].us = Q::WantYes;
                self.send(Verb::Will, option);
            }
        }
    }

    fn on_local_enabled(&mut self, option: u8, events: &mut Vec<TelnetEvent>) {
        events.push(TelnetEvent::OptionEnabled {
            option,
            side: Side::Local,
        });
        if option == option::NAWS {
            self.send_naws();
        }
    }

    /// Reply to `IAC SB TTYPE SEND IAC SE` with the next entry in the MTTS
    /// cycle; repeating the last entry is how a client signals the end.
    fn answer_ttype(&mut self, data: &[u8]) {
        if data.first() != Some(&TTYPE_SEND) {
            return;
        }
        let value = match self.ttype_index {
            0 => CLIENT_NAME.to_string(),
            1 => TERMINAL_TYPE.to_string(),
            _ => format!("MTTS {MTTS_BITVECTOR}"),
        };
        self.ttype_index = (self.ttype_index + 1).min(2);

        let mut payload = Vec::with_capacity(value.len() + 1);
        payload.push(TTYPE_IS);
        payload.extend_from_slice(value.as_bytes());
        let bytes = encode_subnegotiation(option::TTYPE, &payload);
        self.out.extend_from_slice(&bytes);
    }

    /// Reply to `IAC SB CHARSET REQUEST <sep><charset>...IAC SE` (RFC 2066):
    /// ACCEPTED "UTF-8" if the server offered it, REJECTED otherwise. We
    /// always decode with the profile-configured charset regardless of the
    /// outcome (§9.2); this only tells the server what we picked.
    fn answer_charset(&mut self, data: &[u8], events: &mut Vec<TelnetEvent>) {
        const REQUEST: u8 = 1;
        const ACCEPTED: u8 = 2;
        const REJECTED: u8 = 3;

        let Some((&REQUEST, rest)) = data.split_first() else {
            // ACCEPTED/REJECTED/TTABLE-* replies to a REQUEST we never
            // sent, or a malformed message: nothing to answer.
            return;
        };
        let Some((&sep, list)) = rest.split_first() else {
            return;
        };
        let offered = list
            .split(|&b| b == sep)
            .any(|name| name.eq_ignore_ascii_case(b"UTF-8"));

        let reply = if offered {
            let mut payload = vec![ACCEPTED];
            payload.extend_from_slice(b"UTF-8");
            payload
        } else {
            vec![REJECTED]
        };
        let bytes = encode_subnegotiation(option::CHARSET, &reply);
        self.out.extend_from_slice(&bytes);
        events.push(TelnetEvent::CharsetResult { accepted: offered });
    }

    fn send_naws(&mut self) {
        let Some((cols, rows)) = self.window else {
            return;
        };
        if self.options[option::NAWS as usize].us != Q::Yes {
            return;
        }
        let payload = [
            (cols >> 8) as u8,
            (cols & 0xff) as u8,
            (rows >> 8) as u8,
            (rows & 0xff) as u8,
        ];
        let bytes = encode_subnegotiation(option::NAWS, &payload);
        self.out.extend_from_slice(&bytes);
    }

    fn send(&mut self, verb: Verb, option: u8) {
        self.out
            .extend_from_slice(&encode_negotiation(verb, option));
    }

    fn flush_data(&mut self, events: &mut Vec<TelnetEvent>) {
        if !self.data.is_empty() {
            events.push(TelnetEvent::Data(self.data.split().freeze()));
        }
    }
}

/// Encode `IAC <verb> <option>` for an outbound negotiation reply.
pub fn encode_negotiation(verb: Verb, option: u8) -> [u8; 3] {
    [IAC, verb.to_byte(), option]
}

/// Encode `IAC SB <option> <payload> IAC SE`, doubling any IAC in payload.
pub fn encode_subnegotiation(option: u8, payload: &[u8]) -> Bytes {
    let mut out = BytesMut::with_capacity(payload.len() + 5);
    out.extend_from_slice(&[IAC, SB, option]);
    for &byte in payload {
        if byte == IAC {
            out.extend_from_slice(&[IAC, IAC]);
        } else {
            out.extend_from_slice(&[byte]);
        }
    }
    out.extend_from_slice(&[IAC, SE]);
    out.freeze()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_plain_data_through() {
        let mut m = TelnetMachine::new();
        let events = m.feed(b"hello world");
        assert_eq!(
            events,
            vec![TelnetEvent::Data(Bytes::from_static(b"hello world"))]
        );
    }

    #[test]
    fn unescapes_doubled_iac() {
        let mut m = TelnetMachine::new();
        let events = m.feed(&[b'a', IAC, IAC, b'b']);
        assert_eq!(
            events,
            vec![
                TelnetEvent::Data(Bytes::from_static(b"a")),
                TelnetEvent::Data(Bytes::from_static(&[IAC, b'b'])),
            ]
        );
    }

    #[test]
    fn accepts_offered_option_and_reports_prompt_boundary() {
        let mut m = TelnetMachine::new();
        let events = m.feed(&[IAC, WILL, option::EOR, b'>', b' ', IAC, GA]);
        assert_eq!(
            events,
            vec![
                TelnetEvent::OptionEnabled {
                    option: option::EOR,
                    side: Side::Remote,
                },
                TelnetEvent::Data(Bytes::from_static(b"> ")),
                TelnetEvent::PromptBoundary,
            ]
        );
        assert_eq!(&m.take_output()[..], &[IAC, DO, option::EOR]);
    }

    #[test]
    fn refuses_options_we_do_not_implement_yet() {
        let mut m = TelnetMachine::new();
        // GMCP lands in M6; until then we must decline rather than leave
        // the server waiting.
        let events = m.feed(&[IAC, WILL, option::GMCP]);
        assert!(events.is_empty());
        assert_eq!(&m.take_output()[..], &[IAC, DONT, option::GMCP]);
    }

    #[test]
    fn accepts_mccp2_when_the_server_offers_it() {
        let mut m = TelnetMachine::new();
        assert_eq!(
            m.feed(&[IAC, WILL, option::MCCP2]),
            vec![TelnetEvent::OptionEnabled {
                option: option::MCCP2,
                side: Side::Remote,
            }]
        );
        assert_eq!(&m.take_output()[..], &[IAC, DO, option::MCCP2]);
    }

    /// The switchover the whole stage exists for: bytes after the MCCP2
    /// `IAC SE` are zlib, not Telnet, even inside the same read (§6.4).
    #[test]
    fn hands_back_the_compressed_tail_of_the_starting_read() {
        let mut m = TelnetMachine::new();
        let mut input = vec![b'h', b'i', IAC, SB, option::MCCP2, IAC, SE];
        input.extend_from_slice(&[0x78, 0x9c, IAC, IAC, b'\r']);

        assert_eq!(
            m.feed(&input),
            vec![
                TelnetEvent::Data(Bytes::from_static(b"hi")),
                TelnetEvent::CompressionStart,
            ],
            "no bytes past the SE may be parsed as Telnet"
        );
        assert_eq!(&m.take_deferred()[..], &[0x78, 0x9c, IAC, IAC, b'\r']);
        assert!(m.take_deferred().is_empty(), "drained once");
    }

    /// The subnegotiation itself can be split by a read boundary — here
    /// between its `IAC` and `SE` — so the compressed tail is whatever
    /// follows the `SE` in the *second* read (§6.4).
    #[test]
    fn hands_back_the_tail_when_the_subnegotiation_is_split_across_reads() {
        let mut m = TelnetMachine::new();
        assert!(m.feed(&[IAC, SB, option::MCCP2, IAC]).is_empty());
        assert!(m.take_deferred().is_empty(), "nothing to defer yet");

        assert_eq!(
            m.feed(&[SE, 0x78, 0x9c, 0x01]),
            vec![TelnetEvent::CompressionStart]
        );
        assert_eq!(&m.take_deferred()[..], &[0x78, 0x9c, 0x01]);
    }

    #[test]
    fn compression_start_with_nothing_after_it_defers_nothing() {
        let mut m = TelnetMachine::new();
        assert_eq!(
            m.feed(&[IAC, SB, option::MCCP2, IAC, SE]),
            vec![TelnetEvent::CompressionStart]
        );
        assert!(m.take_deferred().is_empty());
    }

    /// The whole point of RFC 1143: a peer repeating an offer we already
    /// accepted must not make us re-answer forever.
    #[test]
    fn repeated_offers_do_not_loop() {
        let mut m = TelnetMachine::new();
        m.feed(&[IAC, WILL, option::ECHO]);
        assert_eq!(&m.take_output()[..], &[IAC, DO, option::ECHO]);

        let events = m.feed(&[IAC, WILL, option::ECHO]);
        assert!(events.is_empty(), "no state change, so no event");
        assert!(m.take_output().is_empty(), "must not answer twice");
    }

    #[test]
    fn tracks_echo_enable_and_disable_for_password_masking() {
        let mut m = TelnetMachine::new();
        assert_eq!(
            m.feed(&[IAC, WILL, option::ECHO]),
            vec![TelnetEvent::OptionEnabled {
                option: option::ECHO,
                side: Side::Remote,
            }]
        );
        assert_eq!(
            m.feed(&[IAC, WONT, option::ECHO]),
            vec![TelnetEvent::OptionDisabled {
                option: option::ECHO,
                side: Side::Remote,
            }]
        );
    }

    #[test]
    fn offers_naws_and_sends_size_once_agreed() {
        let mut m = TelnetMachine::new();
        m.set_window_size(100, 40);
        m.request_local_enable(option::NAWS);
        assert_eq!(&m.take_output()[..], &[IAC, WILL, option::NAWS]);

        let events = m.feed(&[IAC, DO, option::NAWS]);
        assert_eq!(
            events,
            vec![TelnetEvent::OptionEnabled {
                option: option::NAWS,
                side: Side::Local,
            }]
        );
        // WILL is not repeated (we already offered); the size follows.
        assert_eq!(
            &m.take_output()[..],
            &[IAC, SB, option::NAWS, 0, 100, 0, 40, IAC, SE]
        );
    }

    #[test]
    fn resize_after_negotiation_resends_naws() {
        let mut m = TelnetMachine::new();
        m.set_window_size(100, 40);
        m.request_local_enable(option::NAWS);
        m.feed(&[IAC, DO, option::NAWS]);
        m.take_output();

        m.set_window_size(80, 24);
        assert_eq!(
            &m.take_output()[..],
            &[IAC, SB, option::NAWS, 0, 80, 0, 24, IAC, SE]
        );

        // An identical resize is not worth a round trip.
        m.set_window_size(80, 24);
        assert!(m.take_output().is_empty());
    }

    #[test]
    fn naws_escapes_iac_in_dimensions() {
        let mut m = TelnetMachine::new();
        m.request_local_enable(option::NAWS);
        m.feed(&[IAC, DO, option::NAWS]);
        m.take_output();

        // 255 columns must not be mistaken for an IAC byte.
        m.set_window_size(255, 24);
        assert_eq!(
            &m.take_output()[..],
            &[IAC, SB, option::NAWS, 0, IAC, IAC, 0, 24, IAC, SE]
        );
    }

    #[test]
    fn answers_ttype_cycle_then_repeats_mtts() {
        let mut m = TelnetMachine::new();
        m.feed(&[IAC, DO, option::TTYPE]);
        assert_eq!(&m.take_output()[..], &[IAC, WILL, option::TTYPE]);

        let send = [IAC, SB, option::TTYPE, TTYPE_SEND, IAC, SE];
        let reply = |m: &mut TelnetMachine| {
            let events = m.feed(&send);
            assert!(events.is_empty(), "TTYPE is answered internally");
            let out = m.take_output();
            String::from_utf8_lossy(&out[4..out.len() - 2]).into_owned()
        };

        assert_eq!(reply(&mut m), "mudular");
        assert_eq!(reply(&mut m), "xterm-256color");
        assert_eq!(reply(&mut m), "MTTS 269");
        assert_eq!(reply(&mut m), "MTTS 269", "repeat signals end of cycle");
    }

    #[test]
    fn accepts_charset_when_the_server_offers_utf8() {
        let mut m = TelnetMachine::new();
        m.feed(&[IAC, DO, option::CHARSET]);
        assert_eq!(&m.take_output()[..], &[IAC, WILL, option::CHARSET]);

        let mut request = vec![IAC, SB, option::CHARSET, 1, b';'];
        request.extend_from_slice(b"US-ASCII;UTF-8;LATIN1");
        request.extend_from_slice(&[IAC, SE]);

        let events = m.feed(&request);
        assert_eq!(events, vec![TelnetEvent::CharsetResult { accepted: true }]);

        let mut expected = vec![IAC, SB, option::CHARSET, 2];
        expected.extend_from_slice(b"UTF-8");
        expected.extend_from_slice(&[IAC, SE]);
        assert_eq!(&m.take_output()[..], &expected[..]);
    }

    #[test]
    fn rejects_charset_when_utf8_is_not_offered() {
        let mut m = TelnetMachine::new();
        let mut request = vec![IAC, SB, option::CHARSET, 1, b';'];
        request.extend_from_slice(b"US-ASCII;LATIN1");
        request.extend_from_slice(&[IAC, SE]);

        let events = m.feed(&request);
        assert_eq!(events, vec![TelnetEvent::CharsetResult { accepted: false }]);
        assert_eq!(
            &m.take_output()[..],
            &[IAC, SB, option::CHARSET, 3, IAC, SE]
        );
    }

    #[test]
    fn parses_subnegotiation_with_escaped_iac() {
        let mut m = TelnetMachine::new();
        let events = m.feed(&[IAC, SB, option::GMCP, b'x', IAC, IAC, b'y', IAC, SE]);
        assert_eq!(
            events,
            vec![TelnetEvent::Subnegotiation {
                option: option::GMCP,
                data: Bytes::from_static(&[b'x', IAC, b'y']),
            }]
        );
    }

    #[test]
    fn holds_incomplete_sequences_across_feeds() {
        let mut m = TelnetMachine::new();
        assert_eq!(
            m.feed(&[b'a', IAC]),
            vec![TelnetEvent::Data(Bytes::from_static(b"a"))]
        );
        assert_eq!(m.feed(&[WILL]), vec![]);
        assert_eq!(
            m.feed(&[option::EOR]),
            vec![TelnetEvent::OptionEnabled {
                option: option::EOR,
                side: Side::Remote,
            }]
        );
    }

    #[test]
    fn round_trips_subnegotiation_encoding() {
        let payload = [b'x', IAC, b'y'];
        let encoded = encode_subnegotiation(option::MSDP, &payload);
        let mut m = TelnetMachine::new();
        let events = m.feed(&encoded);
        assert_eq!(
            events,
            vec![TelnetEvent::Subnegotiation {
                option: option::MSDP,
                data: Bytes::copy_from_slice(&payload),
            }]
        );
    }
}
