//! MCCP2/MCCP3 (zlib stream compression), Telnet options 86/87.
//!
//! This stage sits in front of the Telnet machine: after the server's MCCP2
//! subnegotiation, all subsequent inbound bytes are one zlib stream —
//! including bytes that arrived in the same read as the subnegotiation
//! (docs/ARCHITECTURE.md §6.4). The mid-buffer switchover is the caller's
//! half of the handshake: the Telnet machine stops consuming at the
//! subnegotiation's `IAC SE` and hands the compressed tail back here.
//!
//! MCCP3 (client→server compression) is not implemented; outbound volume
//! does not justify it.

use flate2::{Decompress, FlushDecompress, Status};

/// Scratch buffer per inflate call. Inbound MUD reads are a few KiB, so
/// this usually empties the input in one pass.
const CHUNK: usize = 16 * 1024;

/// Ceiling on what a single inbound read may inflate to.
///
/// Inflate is an amplifier, and server data is untrusted
/// (docs/ARCHITECTURE.md §13): deflate reaches roughly 1032:1, so an
/// unbounded decoder turns one 4 KiB read into gigabytes of `Vec` growth.
/// Exploiting it needs no MUD account — only a hostile address in a
/// profile. Real MUD reads inflate to a few KiB, so this sits orders of
/// magnitude above legitimate traffic and orders of magnitude below the
/// point where a bomb costs the player anything.
const MAX_INFLATED_PER_READ: usize = 4 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum MccpError {
    #[error("compressed stream is corrupt: {0}")]
    Corrupt(#[from] flate2::DecompressError),
    #[error("compressed stream expanded past {MAX_INFLATED_PER_READ} bytes in one read")]
    Bomb,
}

#[derive(Debug, Default)]
pub struct MccpDecoder {
    /// `None` until MCCP2 starts, and again after the server ends the
    /// stream — in both states bytes pass through untouched.
    stream: Option<Decompress>,
}

impl MccpDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// For the §11 status-bar MCCP badge, which lands with the status bar
    /// rather than with the protocol.
    #[allow(dead_code)]
    pub fn is_active(&self) -> bool {
        self.stream.is_some()
    }

    /// Called when the Telnet machine sees the MCCP2 start subnegotiation.
    pub fn activate(&mut self) {
        self.stream = Some(Decompress::new(true));
    }

    /// Inflate `input` into `output`, or copy it when compression is off.
    pub fn feed(&mut self, input: &[u8], output: &mut Vec<u8>) -> Result<(), MccpError> {
        let Some(stream) = self.stream.as_mut() else {
            output.extend_from_slice(input);
            return Ok(());
        };

        let mut buf = vec![0u8; CHUNK];
        let mut cursor = 0;
        let mut produced = 0usize;
        loop {
            let (before_in, before_out) = (stream.total_in(), stream.total_out());
            let status = stream.decompress(&input[cursor..], &mut buf, FlushDecompress::None)?;
            let read = (stream.total_in() - before_in) as usize;
            let written = (stream.total_out() - before_out) as usize;
            cursor += read;
            produced += written;
            if produced > MAX_INFLATED_PER_READ {
                // Refuse before buffering it: the point is to not grow
                // `output` on a hostile server's say-so.
                return Err(MccpError::Bomb);
            }
            output.extend_from_slice(&buf[..written]);

            if status == Status::StreamEnd {
                // The server ended compression cleanly; the rest of this
                // read is plain again, and so is everything after it.
                self.stream = None;
                output.extend_from_slice(&input[cursor..]);
                return Ok(());
            }
            if read == 0 && written == 0 {
                // The block needs more input than this read carried.
                break;
            }
            if cursor == input.len() && written < buf.len() {
                // Input drained and the last pass did not fill the scratch
                // buffer, so nothing is still sitting inside zlib.
                break;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compress, Compression, FlushCompress};

    /// zlib-compress `plain` as one flushed block, the way a MUD does.
    fn deflate(plain: &[u8]) -> Vec<u8> {
        compress_with(plain, FlushCompress::Sync)
    }

    fn deflate_finished(plain: &[u8]) -> Vec<u8> {
        compress_with(plain, FlushCompress::Finish)
    }

    fn compress_with(plain: &[u8], flush: FlushCompress) -> Vec<u8> {
        let mut c = Compress::new(Compression::default(), true);
        let mut out = vec![0u8; plain.len() + 128];
        c.compress(plain, &mut out, flush).unwrap();
        out.truncate(c.total_out() as usize);
        out
    }

    fn feed(decoder: &mut MccpDecoder, input: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        decoder.feed(input, &mut out).unwrap();
        out
    }

    #[test]
    fn passes_bytes_through_until_activated() {
        let mut d = MccpDecoder::new();
        assert!(!d.is_active());
        assert_eq!(feed(&mut d, b"hello"), b"hello");
    }

    #[test]
    fn inflates_after_activation() {
        let mut d = MccpDecoder::new();
        d.activate();
        assert!(d.is_active());
        assert_eq!(
            feed(&mut d, &deflate(b"You are here.\r\n")),
            b"You are here.\r\n"
        );
    }

    /// The zlib stream does not respect read boundaries: a block can be
    /// split anywhere, including inside the two-byte header.
    #[test]
    fn reassembles_a_block_split_across_reads() {
        let plain = b"a long enough line to survive a split\r\n";
        let compressed = deflate(plain);
        for split in 1..compressed.len() {
            let mut d = MccpDecoder::new();
            d.activate();
            let mut out = feed(&mut d, &compressed[..split]);
            out.extend(feed(&mut d, &compressed[split..]));
            assert_eq!(out, plain, "split at {split}");
        }
    }

    #[test]
    fn several_flushed_blocks_form_one_stream() {
        let mut c = Compress::new(Compression::default(), true);
        let mut compressed = Vec::new();
        for line in [&b"one\r\n"[..], b"two\r\n", b"three\r\n"] {
            let mut buf = vec![0u8; 128];
            let before = c.total_out() as usize;
            c.compress(line, &mut buf, FlushCompress::Sync).unwrap();
            let written = c.total_out() as usize - before;
            compressed.extend_from_slice(&buf[..written]);
        }

        let mut d = MccpDecoder::new();
        d.activate();
        assert_eq!(feed(&mut d, &compressed), b"one\r\ntwo\r\nthree\r\n");
    }

    /// Z_STREAM_END returns the pipeline to passthrough mid-buffer (§6.4).
    #[test]
    fn stream_end_returns_to_passthrough_within_the_same_read() {
        let mut input = deflate_finished(b"compressed\r\n");
        input.extend_from_slice(b"plain again\r\n");

        let mut d = MccpDecoder::new();
        d.activate();
        assert_eq!(feed(&mut d, &input), b"compressed\r\nplain again\r\n");
        assert!(!d.is_active());
        assert_eq!(feed(&mut d, b"still plain"), b"still plain");
    }

    #[test]
    fn output_larger_than_the_scratch_buffer_survives() {
        let plain = "highly compressible\r\n".repeat(4000);
        let mut d = MccpDecoder::new();
        d.activate();
        assert_eq!(feed(&mut d, &deflate(plain.as_bytes())), plain.as_bytes());
    }

    /// A malformed server can send a second MCCP2 start subnegotiation
    /// without ending the first stream. That means a new zlib stream, so
    /// the previous decoder must be discarded rather than fed the new
    /// stream's header as if it were more deflate data.
    #[test]
    fn restarting_compression_discards_the_previous_stream() {
        let mut d = MccpDecoder::new();
        d.activate();
        assert_eq!(feed(&mut d, &deflate(b"first\r\n")), b"first\r\n");

        d.activate();
        assert_eq!(feed(&mut d, &deflate(b"second\r\n")), b"second\r\n");
    }

    /// Inflate is an amplifier and the server is untrusted (§13): a bomb
    /// is refused rather than buffered.
    #[test]
    fn a_decompression_bomb_is_refused_rather_than_buffered() {
        let payload = MAX_INFLATED_PER_READ + CHUNK;
        let bomb = deflate(&vec![0u8; payload]);
        assert!(
            payload / bomb.len() > 1000,
            "a bomb is cheap to send and expensive to hold: {} bytes in, \
             {payload} out",
            bomb.len()
        );

        let mut d = MccpDecoder::new();
        d.activate();
        let mut out = Vec::new();
        assert!(matches!(d.feed(&bomb, &mut out), Err(MccpError::Bomb)));
        assert!(
            out.len() <= MAX_INFLATED_PER_READ,
            "refused, but buffered {} bytes first",
            out.len()
        );
    }

    /// The cap sits far above real traffic: a large legitimate read still
    /// goes through untouched.
    #[test]
    fn output_just_under_the_cap_is_still_delivered() {
        let plain = vec![b'x'; MAX_INFLATED_PER_READ - 1];
        let mut d = MccpDecoder::new();
        d.activate();
        assert_eq!(feed(&mut d, &deflate(&plain)).len(), plain.len());
    }

    #[test]
    fn corrupt_stream_is_an_error_not_a_panic() {
        let mut d = MccpDecoder::new();
        d.activate();
        let mut out = Vec::new();
        assert!(
            d.feed(b"\x78\x9c not a deflate block at all", &mut out)
                .is_err()
        );
    }
}
