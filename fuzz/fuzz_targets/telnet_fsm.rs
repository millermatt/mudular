//! The Telnet IAC state machine against arbitrary server bytes.
//!
//! Server bytes are attacker-controlled (docs/ARCHITECTURE.md §12/§13), and
//! the machine holds state across reads: a partial IAC sequence, a
//! half-collected subnegotiation, the RFC 1143 option table. The hand-written
//! fixtures in `proto::telnet` cover sequences someone thought to write down;
//! this covers the ones nobody did.
//!
//! The input is a *sequence* of reads rather than one buffer, because the
//! interesting bugs live at read boundaries — a sequence split across two
//! `feed` calls has to behave exactly as it does in one.

#![no_main]

use libfuzzer_sys::fuzz_target;
use mudular::proto::telnet::{TelnetEvent, TelnetMachine};

fuzz_target!(|reads: Vec<Vec<u8>>| {
    let mut machine = TelnetMachine::new();

    for read in &reads {
        for event in machine.feed(read) {
            // Force the payloads to be looked at, so a bad length or offset
            // is a fault here rather than something the optimiser elides.
            match event {
                TelnetEvent::Data(bytes) | TelnetEvent::Subnegotiation { data: bytes, .. } => {
                    std::hint::black_box(bytes.len());
                }
                _ => {}
            }
        }

        // The session drains both every read; a machine that only stays
        // consistent when nobody collects its output is not fuzzed properly.
        let out = machine.take_output();
        let deferred = machine.take_deferred();

        // Deferred bytes are the compressed tail of *this* read, so they can
        // never exceed it. If this ever trips, the switchover handed the
        // caller bytes from somewhere it should not have reached.
        assert!(
            deferred.len() <= read.len(),
            "deferred {} bytes from a {}-byte read",
            deferred.len(),
            read.len()
        );

        std::hint::black_box(out.len());
    }
});
