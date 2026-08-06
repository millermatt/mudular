//! Legacy charset decoding for MUDs that predate UTF-8.
//!
//! CHARSET negotiation (Telnet option 42) requests UTF-8; on rejection or
//! absence, decoding falls back to the profile's configured charset
//! (docs/ARCHITECTURE.md §9.2). UTF-8 needs the incremental byte-boundary
//! handling in `session`; the legacy charsets here are single-byte, so
//! every byte maps to exactly one Unicode scalar value with no state
//! carried across reads.
//!
//! CP437 has no crate: `encoding_rs` is web-focused and omits it (§2.1),
//! so its upper 128 code points are a small built-in table here.

use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Charset {
    /// Handled separately by the incremental UTF-8 decoder in `session`.
    Utf8,
    /// ISO 8859-1: byte value equals Unicode scalar value.
    Latin1,
    /// IBM PC code page 437: the original MS-DOS-era MUD encoding.
    Cp437,
}

impl FromStr for Charset {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "utf-8" | "utf8" => Ok(Charset::Utf8),
            "latin1" | "latin-1" | "iso-8859-1" | "iso8859-1" => Ok(Charset::Latin1),
            "cp437" | "ibm437" | "cp-437" => Ok(Charset::Cp437),
            other => Err(format!(
                "unknown charset `{other}` (expected utf-8, latin1, or cp437)"
            )),
        }
    }
}

impl Charset {
    /// Decode one byte. Not meaningful for `Utf8` — callers special-case it
    /// and use the incremental UTF-8 decoder instead.
    pub fn decode_byte(self, byte: u8) -> char {
        match self {
            Charset::Utf8 => byte as char, // unreachable in practice; see above
            Charset::Latin1 => byte as char,
            Charset::Cp437 => decode_cp437(byte),
        }
    }
}

fn decode_cp437(byte: u8) -> char {
    if byte < 0x80 {
        byte as char
    } else {
        CP437_HIGH[(byte - 0x80) as usize]
    }
}

/// Code points for bytes 0x80..=0xFF, in order (Code page 437).
const CP437_HIGH: [char; 128] = [
    'Ç', 'ü', 'é', 'â', 'ä', 'à', 'å', 'ç', 'ê', 'ë', 'è', 'ï', 'î', 'ì', 'Ä', 'Å', 'É', 'æ', 'Æ',
    'ô', 'ö', 'ò', 'û', 'ù', 'ÿ', 'Ö', 'Ü', '¢', '£', '¥', '₧', 'ƒ', 'á', 'í', 'ó', 'ú', 'ñ', 'Ñ',
    'ª', 'º', '¿', '⌐', '¬', '½', '¼', '¡', '«', '»', '░', '▒', '▓', '│', '┤', '╡', '╢', '╖', '╕',
    '╣', '║', '╗', '╝', '╜', '╛', '┐', '└', '┴', '┬', '├', '─', '┼', '╞', '╟', '╚', '╔', '╩', '╦',
    '╠', '═', '╬', '╧', '╨', '╤', '╥', '╙', '╘', '╒', '╓', '╫', '╪', '┘', '┌', '█', '▄', '▌', '▐',
    '▀', 'α', 'ß', 'Γ', 'π', 'Σ', 'σ', 'µ', 'τ', 'Φ', 'Θ', 'Ω', 'δ', '∞', 'φ', 'ε', '∩', '≡', '±',
    '≥', '≤', '⌠', '⌡', '÷', '≈', '°', '∙', '·', '√', 'ⁿ', '²', '■', '\u{00A0}',
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_charset_names_case_insensitively() {
        assert_eq!("UTF-8".parse::<Charset>().unwrap(), Charset::Utf8);
        assert_eq!("Latin1".parse::<Charset>().unwrap(), Charset::Latin1);
        assert_eq!("CP437".parse::<Charset>().unwrap(), Charset::Cp437);
        assert!("shift-jis".parse::<Charset>().is_err());
    }

    #[test]
    fn latin1_maps_byte_value_to_the_same_scalar_value() {
        assert_eq!(Charset::Latin1.decode_byte(0x41), 'A');
        assert_eq!(Charset::Latin1.decode_byte(0xE9), 'é');
        assert_eq!(Charset::Latin1.decode_byte(0xFF), 'ÿ');
    }

    #[test]
    fn cp437_ascii_range_is_unchanged() {
        for byte in 0x20u8..0x7F {
            assert_eq!(Charset::Cp437.decode_byte(byte), byte as char);
        }
    }

    /// The actual "done when" case: box-drawing wall art from a legacy
    /// CP437 MUD map, byte-fixture style (docs/ARCHITECTURE.md §14 M3).
    #[test]
    fn cp437_decodes_box_drawing_wall_art() {
        let bytes = [
            0xC9u8, 0xCD, 0xCD, 0xBB, 0x0A, 0xBA, 0x20, 0xBA, 0x0A, 0xC8, 0xCD, 0xCD, 0xBC,
        ];
        let text: String = bytes
            .iter()
            .map(|&b| Charset::Cp437.decode_byte(b))
            .collect();
        assert_eq!(text, "╔══╗\n║ ║\n╚══╝");
    }

    #[test]
    fn cp437_decodes_accented_letters_and_block_shading() {
        assert_eq!(Charset::Cp437.decode_byte(0x80), 'Ç');
        assert_eq!(Charset::Cp437.decode_byte(0xB0), '░');
        assert_eq!(Charset::Cp437.decode_byte(0xDB), '█');
        assert_eq!(Charset::Cp437.decode_byte(0xFF), '\u{00A0}');
    }
}
