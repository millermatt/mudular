#!/usr/bin/env python3
"""Generate the seed corpus for the mccp_switchover fuzz target.

Random bytes are never a valid deflate stream, so an unseeded fuzzer spends
its whole budget confirming that `mccp` rejects garbage and never reaches the
inflate path the target exists to exercise. These seeds put it there: each is
a raw inbound byte stream in the shape a real MCCP2 server sends, which the
fuzzer then mutates.

Raw inbound captures from `--record` (docs/ARCHITECTURE.md §14 M1) can be
dropped into the same directory and are usually better than anything
synthetic here.

Usage:  python3 fuzz/seeds/make_seeds.py
"""

import pathlib
import zlib

IAC, SB, SE, WILL, GA = 255, 250, 240, 251, 249
MCCP2, GMCP = 86, 201

OUT = pathlib.Path(__file__).parent / "mccp_switchover"

# The handshake every seed opens with: the server offers MCCP2, we accept,
# and its empty subnegotiation is the last uncompressed byte on the wire.
START = bytes([IAC, WILL, MCCP2]) + bytes([IAC, SB, MCCP2, IAC, SE])


def deflate(payload: bytes) -> bytes:
    return zlib.compress(payload)


def seed(name: str, data: bytes) -> None:
    (OUT / name).write_bytes(data)


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)

    # 1. The plain case: text arrives compressed, all in the same read as the
    #    subnegotiation that turned compression on. This is the mid-buffer
    #    switchover of §6.4 in its simplest form.
    seed("01-basic-switchover", START + deflate(b"You are standing in an open field.\r\n"))

    # 2. Telnet framing *inside* the compressed stream: the parser has to keep
    #    working on inflated bytes, not just on the raw read.
    inner = bytes([IAC, GA]) + b"HP:100 MP:50> " + bytes([IAC, SB, GMCP]) + b'Char.Vitals {"hp":100}' + bytes([IAC, SE])
    seed("02-telnet-inside-compression", START + deflate(inner))

    # 3. An escaped IAC inside compression: 255 doubled, which must decode to
    #    one literal 0xFF of application data rather than opening a command.
    seed("03-escaped-iac-inside", START + deflate(b"cost: 100" + bytes([IAC, IAC]) + b"gold\r\n"))

    # 4. Uncompressed text ahead of the switchover, so the read is genuinely
    #    split: plain bytes, then the subnegotiation, then zlib.
    seed("04-plain-tail-then-switchover", b"Welcome!\r\n" + START + deflate(b"Compressed now.\r\n"))

    # 5. A stream that ends cleanly mid-read, after which the tail is plain
    #    again — the other half of the switchover, and the path that resets
    #    `stream` back to None.
    compressor = zlib.compressobj()
    ended = compressor.compress(b"compressed part\r\n") + compressor.flush(zlib.Z_FINISH)
    seed("05-stream-end-then-plain", START + ended + b"plain again\r\n")

    # 6. A truncated deflate stream: the decoder must hold, not fault, when a
    #    block needs more input than the read carried.
    seed("06-truncated-stream", START + deflate(b"x" * 4096)[:20])

    # 7. A modest zip bomb: highly compressible input, well under the §13 cap,
    #    so the fuzzer has a starting point for growing one past it.
    seed("07-compressible", START + deflate(b"\x00" * 512_000))

    print(f"wrote {len(list(OUT.iterdir()))} seeds to {OUT}")


if __name__ == "__main__":
    main()
