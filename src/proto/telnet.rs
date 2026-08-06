//! Telnet (RFC 854) byte-stream state machine.
//!
//! Handles IAC escaping, WILL/WONT/DO/DONT, SB…SE subnegotiation framing
//! (with embedded IAC IAC unescaping), and GA/EOR prompt boundaries.
//! Per-option negotiation state (RFC 1143 Q-method) lands in M1.

use bytes::{Bytes, BytesMut};

pub mod option {
    pub const ECHO: u8 = 1;
    pub const SGA: u8 = 3;
    pub const TTYPE: u8 = 24;
    pub const EOR: u8 = 25;
    pub const NAWS: u8 = 31;
    pub const CHARSET: u8 = 42;
    pub const MSDP: u8 = 69;
    pub const MCCP2: u8 = 86;
    pub const MCCP3: u8 = 87;
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TelnetEvent {
    /// Application bytes with all Telnet framing removed.
    Data(Bytes),
    /// GA or EOR: everything since the last newline is a prompt.
    PromptBoundary,
    /// A negotiation request/acknowledgement from the server.
    Negotiation { verb: Verb, option: u8 },
    /// A complete IAC SB <option> … IAC SE payload, IAC-unescaped.
    Subnegotiation { option: u8, data: Bytes },
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

#[derive(Debug, Default)]
pub struct TelnetMachine {
    state: State,
    data: BytesMut,
    sub_option: u8,
    sub_data: BytesMut,
}

impl TelnetMachine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Consume raw inbound bytes, producing events in stream order.
    /// Incomplete sequences are held across calls.
    pub fn feed(&mut self, input: &[u8]) -> Vec<TelnetEvent> {
        let mut events = Vec::new();
        for &byte in input {
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
                    events.push(TelnetEvent::Negotiation { verb, option: byte });
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
                        events.push(TelnetEvent::Subnegotiation {
                            option: self.sub_option,
                            data: self.sub_data.split().freeze(),
                        });
                        self.state = State::Data;
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
    fn parses_negotiation_and_prompt_boundary() {
        let mut m = TelnetMachine::new();
        let events = m.feed(&[IAC, WILL, option::GMCP, b'>', b' ', IAC, GA]);
        assert_eq!(
            events,
            vec![
                TelnetEvent::Negotiation {
                    verb: Verb::Will,
                    option: option::GMCP
                },
                TelnetEvent::Data(Bytes::from_static(b"> ")),
                TelnetEvent::PromptBoundary,
            ]
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
            vec![TelnetEvent::Negotiation {
                verb: Verb::Will,
                option: option::EOR,
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
