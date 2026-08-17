//! Legacy charset decoding against arbitrary server bytes.
//!
//! Honest scope note, because this target is weaker than its two siblings and
//! it is better to say so here than to let the `fuzz/` listing imply three
//! equal defences:
//!
//! `Charset::decode_byte` is a total function of one `u8` with no state
//! carried across calls, so its whole input domain is 256 values — and
//! `every_byte_decodes_in_every_charset` in `proto::charset` already walks all
//! of them in every charset, which is a proof where this is a search. What is
//! left for a fuzzer is the part a per-byte test cannot see: that decoding a
//! whole read is byte-independent, i.e. that splitting a read anywhere gives
//! the same string as decoding it whole. That is the property `session` relies
//! on when it decodes socket reads one at a time without buffering.
//!
//! The stateful decoder — UTF-8, which *does* carry a partial sequence across
//! reads — lives in `session`, not here, and is not reachable from this crate.
//! It is the charset code that actually warrants fuzzing.

#![no_main]

use libfuzzer_sys::fuzz_target;
use mudular::proto::charset::Charset;

fuzz_target!(|reads: Vec<Vec<u8>>| {
    for charset in [Charset::Latin1, Charset::Cp437] {
        let whole: String = reads
            .iter()
            .flatten()
            .map(|&b| charset.decode_byte(b))
            .collect();

        // The same bytes, decoded a read at a time the way `session` does.
        let mut split = String::new();
        for read in &reads {
            for &b in read {
                split.push(charset.decode_byte(b));
            }
        }

        assert_eq!(
            whole, split,
            "{charset:?} decoded differently across read boundaries"
        );
    }
});
