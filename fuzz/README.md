# Fuzz targets

Coverage-guided fuzzing for the code that parses attacker-controlled bytes.
Everything a MUD sends is untrusted input (docs/ARCHITECTURE.md §12, §13), and
`proto` is where it first gets interpreted.

This is a separate crate with its own `[workspace]`, so `cargo build`, `cargo
clippy` and CI at the repo root never try to build libfuzzer. It needs nightly.

```sh
cargo install cargo-fuzz
cargo +nightly fuzz build          # all three
cargo +nightly fuzz run telnet_fsm -- -max_total_time=300
```

A crash is written to `fuzz/artifacts/<target>/` and replayed with:

```sh
cargo +nightly fuzz run <target> fuzz/artifacts/<target>/<file>
```

Anything found here should become a byte-fixture unit test next to the code it
broke, in the style of `src/proto/telnet.rs` — the fuzzer finds it once, the
fixture stops it coming back.

## The targets, and how much each is worth

They are not equally load-bearing, and it is better to say so than to let a
directory of three imply three equal defences.

**`mccp_switchover`** — the one that justifies the directory. `mccp` and
`telnet` each have their own fixtures; what neither covers is the seam between
them, where turning compression on ends the Telnet parse mid-buffer and the
rest of that same read comes back round as zlib (§6.4). It asserts the loop
terminates — a stream that switches over without consuming input wedges the
session rather than panicking, which a timeout-only check reports badly — and
that inflation stays under the §13 cap.

**`telnet_fsm`** — arbitrary bytes across a *sequence* of reads, because the
machine holds a partial IAC sequence, a half-collected subnegotiation, and the
RFC 1143 option table across `feed` calls. The fixtures cover the sequences
someone thought to write down; this covers the ones nobody did.

**`charset_decode`** — the weakest of the three, deliberately kept anyway.
`Charset::decode_byte` is a total function of one `u8` with no state, so its
entire input domain is 256 values, and `every_byte_decodes_in_every_charset`
in `src/proto/charset.rs` walks all of them — a proof where a fuzzer is only a
search. What is left here is the property a per-byte test cannot see: that
decoding is byte-independent, so splitting a read anywhere gives the same
string. Note that the charset code which *would* most repay fuzzing is the
incremental UTF-8 decoder, and it lives in `session`, out of reach of this
crate.

## Seeds

`seeds/mccp_switchover/` holds committed starting inputs, because random bytes
are never a valid deflate stream: unseeded, the fuzzer spends its whole budget
confirming that `mccp` rejects garbage and never reaches the inflate path at
all. Seeding raises initial coverage from a standing start to ~560 edges.

```sh
cargo +nightly fuzz run mccp_switchover \
  fuzz/corpus/mccp_switchover fuzz/seeds/mccp_switchover
```

Regenerate them with `python3 fuzz/seeds/make_seeds.py`. Better than anything
synthetic in there: a real capture from `--record` (§14 M1), which is raw
inbound bytes and can be dropped into the corpus directly. That is why this
target takes one flat `&[u8]` rather than the `Vec<Vec<u8>>` the other two
take — an `Arbitrary`-decoded shape eats trailing bytes as length hints and
would truncate exactly the deflate stream a seed exists to deliver.
