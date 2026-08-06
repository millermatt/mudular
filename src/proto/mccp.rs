//! MCCP2/MCCP3 (zlib stream compression), Telnet options 86/87.
//!
//! This stage sits in front of the Telnet machine: after the server's MCCP2
//! subnegotiation, all subsequent inbound bytes are one zlib stream — including
//! bytes that arrived in the same read as the subnegotiation
//! (docs/ARCHITECTURE.md §6.4). Inflation itself lands in M5; until then this
//! stage is a passthrough that tracks activation.

#[derive(Debug, Default)]
pub struct MccpDecoder {
    active: bool,
}

impl MccpDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Called when the Telnet machine sees the MCCP2 start subnegotiation.
    pub fn activate(&mut self) {
        self.active = true;
    }

    pub fn feed(&mut self, input: &[u8], output: &mut Vec<u8>) {
        output.extend_from_slice(input);
    }
}
