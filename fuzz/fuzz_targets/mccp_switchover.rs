//! The MCCP2 mid-buffer switchover: the inflate/parse loop, not either half.
//!
//! This is the target the other two cannot stand in for. `mccp` and `telnet`
//! are each covered by their own fixtures, but the hazard in §6.4 is the seam
//! between them — turning compression on ends the Telnet parse mid-buffer,
//! and the rest of that same read comes back round as zlib. The loop below is
//! the one in `session::run` (§6.5), reproduced because that module is async
//! and socket-bound and cannot be driven from a fuzz target.
//!
//! The input is one flat read rather than a sequence of them, unlike the other
//! two targets. That is what makes the seed corpus work: a seed — or a raw
//! `--record` capture (§14 M1) — is the byte stream verbatim, where an
//! `Arbitrary`-decoded shape would eat its trailing bytes as length hints and
//! truncate exactly the deflate stream the seed exists to deliver. Splitting
//! a stream across reads is `telnet_fsm`'s property; the switchover this
//! target covers happens strictly within one read.
//!
//! Two properties are asserted, both of which are hangs or crashes in the
//! real client if they fail:
//!
//! 1. The loop terminates. `pending` is refilled from `take_deferred()` on
//!    every `CompressionStart`, so a stream that can produce a switchover
//!    without consuming input spins forever — a wedged session, not a panic,
//!    which is exactly the failure a timeout-only check reports badly.
//! 2. Inflation stays under the §13 cap. The bomb guard is what stands
//!    between a 4 KiB read and gigabytes of `Vec` growth.

#![no_main]

use libfuzzer_sys::fuzz_target;
use mudular::proto::mccp::MccpDecoder;
use mudular::proto::telnet::{TelnetEvent, TelnetMachine};

/// The cap `mccp` enforces per read (§13). Asserted rather than imported:
/// the constant is private, and a silent raise should trip this.
const MAX_INFLATED_PER_READ: usize = 4 * 1024 * 1024;

fuzz_target!(|read: &[u8]| {
    let mut mccp = MccpDecoder::new();
    let mut telnet = TelnetMachine::new();
    let mut plain: Vec<u8> = Vec::new();
    let mut pending: Vec<u8> = read.to_vec();

    // Not a bound derived from the input: deferred bytes are a suffix of the
    // *inflated* buffer, so a switchover can legitimately hand back more than
    // the raw read carried, and any tighter budget would fire on honest
    // streams. A real read switches over once. A thousand times is not a slow
    // stream, it is a stream that is not making progress.
    let mut budget = 1000usize;

    while !pending.is_empty() {
        budget = budget
            .checked_sub(1)
            .expect("switchover loop did not consume its input: the session would wedge here");

        plain.clear();
        if mccp.feed(&pending, &mut plain).is_err() {
            // A corrupt stream or a bomb: the session drops the connection.
            // Nothing further to drive.
            break;
        }
        assert!(
            plain.len() <= MAX_INFLATED_PER_READ,
            "inflated {} bytes past the {MAX_INFLATED_PER_READ}-byte cap",
            plain.len()
        );
        pending.clear();

        for event in telnet.feed(&plain) {
            if let TelnetEvent::CompressionStart = event {
                mccp.activate();
                pending = telnet.take_deferred().to_vec();
            }
        }

        let _ = telnet.take_output();
    }
});
