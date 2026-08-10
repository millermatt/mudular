//! MSDP (MUD Server Data Protocol), Telnet option 69.
//!
//! Subnegotiation payload is a sequence of `VAR <name> VAL <value>` pairs,
//! where a value can itself be a nested `TABLE_OPEN…TABLE_CLOSE` (more
//! VAR/VAL pairs) or `ARRAY_OPEN…ARRAY_CLOSE` (a list of values). Names and
//! scalar values are raw bytes terminated by the next control byte — MSDP
//! has no escaping. Where a server offers both GMCP and MSDP, GMCP is
//! preferred; MSDP values are normalized into the same server-data
//! namespace as GMCP (docs/ARCHITECTURE.md §6.3).

use thiserror::Error;

use super::Flattened;

pub const VAR: u8 = 1;
pub const VAL: u8 = 2;
pub const TABLE_OPEN: u8 = 3;
pub const TABLE_CLOSE: u8 = 4;
pub const ARRAY_OPEN: u8 = 5;
pub const ARRAY_CLOSE: u8 = 6;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MsdpValue {
    String(String),
    Array(Vec<MsdpValue>),
    Table(Vec<(String, MsdpValue)>),
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum MsdpError {
    #[error("MSDP VAR name not followed by VAL")]
    ExpectedVal,
    #[error("MSDP array or table not terminated")]
    Unterminated,
    #[error("MSDP message has trailing bytes after its VAR/VAL pairs")]
    TrailingBytes,
    #[error("MSDP array/table nesting exceeds the maximum depth")]
    TooDeep,
}

/// Recursion cap for nested `ARRAY_OPEN`/`TABLE_OPEN` values. Nesting costs
/// as little as two bytes per level on the wire, so an unbounded depth lets
/// a small malicious payload overflow the stack; no legitimate MUD payload
/// needs anywhere near this deep.
const MAX_DEPTH: usize = 32;

/// Parses a complete MSDP subnegotiation payload into its top-level
/// `VAR`/`VAL` pairs.
/// Encodes a `VAR name VAL value` pair, the whole of what a client needs to
/// send: MSDP's client half is `REPORT`, `UNREPORT`, `LIST` and `SEND`,
/// each of which is one variable naming one value.
pub fn encode_pair(name: &str, value: &str) -> Vec<u8> {
    let mut out = vec![VAR];
    out.extend_from_slice(name.as_bytes());
    out.push(VAL);
    out.extend_from_slice(value.as_bytes());
    out
}

/// What the client asks the server to keep it posted about once MSDP is
/// live (docs/ARCHITECTURE.md §6.3).
///
/// **A server sends nothing until asked.** MSDP is subscription-based —
/// the widely used implementation transmits only variables the client has
/// `REPORT`ed — so negotiating the option and then staying quiet leaves a
/// perfectly working server silent, which reads exactly like a server that
/// does not speak MSDP at all.
///
/// Hardcoded, and for the same reason as GMCP's `Core.Supports.Set`
/// (`gmcp::supports_message`): making either declarable is one change, not
/// two, and neither is useful to declare until profiles can say so.
pub fn report_requests() -> Vec<Vec<u8>> {
    ["ROOM"]
        .into_iter()
        .map(|variable| encode_pair("REPORT", variable))
        .collect()
}

pub fn parse(data: &[u8]) -> Result<Vec<(String, MsdpValue)>, MsdpError> {
    let mut cursor = Cursor { data, pos: 0 };
    let pairs = parse_pairs(&mut cursor, 0)?;
    if cursor.pos != cursor.data.len() {
        return Err(MsdpError::TrailingBytes);
    }
    Ok(pairs)
}

/// Flattens a parsed value into dotted-path `(key, value)` pairs, mirroring
/// [`crate::proto::gmcp::flatten`] so both protocols land in one shape for
/// the server-data store — [`Flattened`] included, since MSDP arrays are
/// positional for exactly the same reason GMCP's are and go stale the same
/// way when one shrinks.
pub fn flatten(name: &str, value: &MsdpValue, out: &mut Flattened) {
    match value {
        MsdpValue::String(s) => out.pairs.push((name.to_string(), s.clone())),
        MsdpValue::Array(items) => {
            // Before the recursion, and for an empty array too: `[]` emits
            // no pairs at all, which is the case where a stale `….0` would
            // otherwise survive unnoticed.
            out.arrays.push((name.to_string(), items.len()));
            for (index, item) in items.iter().enumerate() {
                flatten(&format!("{name}.{index}"), item, out);
            }
        }
        MsdpValue::Table(pairs) => {
            for (key, val) in pairs {
                flatten(&format!("{name}.{key}"), val, out);
            }
        }
    }
}

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn peek(&self) -> Option<u8> {
        self.data.get(self.pos).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let byte = self.peek();
        if byte.is_some() {
            self.pos += 1;
        }
        byte
    }

    /// Raw bytes up to (not including) the next control byte or end of
    /// input — a name or a scalar value, per MSDP's unescaped wire format.
    fn take_until_control(&mut self) -> String {
        let start = self.pos;
        while let Some(byte) = self.peek() {
            if is_control(byte) {
                break;
            }
            self.pos += 1;
        }
        String::from_utf8_lossy(&self.data[start..self.pos]).into_owned()
    }
}

fn is_control(byte: u8) -> bool {
    matches!(
        byte,
        VAR | VAL | TABLE_OPEN | TABLE_CLOSE | ARRAY_OPEN | ARRAY_CLOSE
    )
}

fn parse_pairs(cursor: &mut Cursor, depth: usize) -> Result<Vec<(String, MsdpValue)>, MsdpError> {
    let mut pairs = Vec::new();
    while cursor.peek() == Some(VAR) {
        cursor.next();
        let name = cursor.take_until_control();
        if cursor.next() != Some(VAL) {
            return Err(MsdpError::ExpectedVal);
        }
        let value = parse_value(cursor, depth)?;
        pairs.push((name, value));
    }
    Ok(pairs)
}

fn parse_value(cursor: &mut Cursor, depth: usize) -> Result<MsdpValue, MsdpError> {
    match cursor.peek() {
        Some(ARRAY_OPEN | TABLE_OPEN) if depth >= MAX_DEPTH => Err(MsdpError::TooDeep),
        Some(ARRAY_OPEN) => {
            cursor.next();
            let mut items = Vec::new();
            while cursor.peek() == Some(VAL) {
                cursor.next();
                items.push(parse_value(cursor, depth + 1)?);
            }
            if cursor.next() != Some(ARRAY_CLOSE) {
                return Err(MsdpError::Unterminated);
            }
            Ok(MsdpValue::Array(items))
        }
        Some(TABLE_OPEN) => {
            cursor.next();
            let pairs = parse_pairs(cursor, depth + 1)?;
            if cursor.next() != Some(TABLE_CLOSE) {
                return Err(MsdpError::Unterminated);
            }
            Ok(MsdpValue::Table(pairs))
        }
        _ => Ok(MsdpValue::String(cursor.take_until_control())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(bytes: &[u8]) -> Vec<u8> {
        bytes.to_vec()
    }

    #[test]
    fn parses_a_single_scalar_pair() {
        let mut bytes = msg(&[VAR]);
        bytes.extend_from_slice(b"HEALTH");
        bytes.push(VAL);
        bytes.extend_from_slice(b"100");

        assert_eq!(
            parse(&bytes).unwrap(),
            vec![("HEALTH".to_string(), MsdpValue::String("100".to_string()))]
        );
    }

    #[test]
    fn parses_multiple_pairs_in_one_message() {
        let mut bytes = msg(&[VAR]);
        bytes.extend_from_slice(b"HEALTH");
        bytes.push(VAL);
        bytes.extend_from_slice(b"100");
        bytes.push(VAR);
        bytes.extend_from_slice(b"HEALTH_MAX");
        bytes.push(VAL);
        bytes.extend_from_slice(b"100");

        assert_eq!(
            parse(&bytes).unwrap(),
            vec![
                ("HEALTH".to_string(), MsdpValue::String("100".to_string())),
                (
                    "HEALTH_MAX".to_string(),
                    MsdpValue::String("100".to_string())
                ),
            ]
        );
    }

    #[test]
    fn parses_an_array_value() {
        let mut bytes = msg(&[VAR]);
        bytes.extend_from_slice(b"ROOM_EXITS");
        bytes.push(VAL);
        bytes.push(ARRAY_OPEN);
        bytes.push(VAL);
        bytes.extend_from_slice(b"north");
        bytes.push(VAL);
        bytes.extend_from_slice(b"south");
        bytes.push(ARRAY_CLOSE);

        assert_eq!(
            parse(&bytes).unwrap(),
            vec![(
                "ROOM_EXITS".to_string(),
                MsdpValue::Array(vec![
                    MsdpValue::String("north".to_string()),
                    MsdpValue::String("south".to_string()),
                ])
            )]
        );
    }

    #[test]
    fn parses_a_nested_table_value() {
        let mut bytes = msg(&[VAR]);
        bytes.extend_from_slice(b"ROOM");
        bytes.push(VAL);
        bytes.push(TABLE_OPEN);
        bytes.push(VAR);
        bytes.extend_from_slice(b"NAME");
        bytes.push(VAL);
        bytes.extend_from_slice(b"The Bazaar");
        bytes.push(TABLE_CLOSE);

        assert_eq!(
            parse(&bytes).unwrap(),
            vec![(
                "ROOM".to_string(),
                MsdpValue::Table(vec![(
                    "NAME".to_string(),
                    MsdpValue::String("The Bazaar".to_string())
                )])
            )]
        );
    }

    #[test]
    fn an_empty_array_parses_to_no_items() {
        let mut bytes = msg(&[VAR]);
        bytes.extend_from_slice(b"LIST");
        bytes.push(VAL);
        bytes.push(ARRAY_OPEN);
        bytes.push(ARRAY_CLOSE);

        assert_eq!(
            parse(&bytes).unwrap(),
            vec![("LIST".to_string(), MsdpValue::Array(vec![]))]
        );
    }

    #[test]
    fn an_unterminated_array_is_an_error() {
        let mut bytes = msg(&[VAR]);
        bytes.extend_from_slice(b"LIST");
        bytes.push(VAL);
        bytes.push(ARRAY_OPEN);
        bytes.push(VAL);
        bytes.extend_from_slice(b"x");

        assert_eq!(parse(&bytes), Err(MsdpError::Unterminated));
    }

    #[test]
    fn a_var_without_val_is_an_error() {
        let mut bytes = msg(&[VAR]);
        bytes.extend_from_slice(b"HEALTH");

        assert_eq!(parse(&bytes), Err(MsdpError::ExpectedVal));
    }

    #[test]
    fn trailing_garbage_after_the_pairs_is_an_error() {
        let mut bytes = msg(&[VAR]);
        bytes.extend_from_slice(b"HEALTH");
        bytes.push(VAL);
        bytes.extend_from_slice(b"100");
        bytes.push(TABLE_CLOSE); // stray, never opened

        assert_eq!(parse(&bytes), Err(MsdpError::TrailingBytes));
    }

    #[test]
    fn nesting_past_the_depth_cap_is_an_error_not_a_stack_overflow() {
        let mut bytes = msg(&[VAR]);
        bytes.extend_from_slice(b"X");
        bytes.push(VAL);
        for _ in 0..(MAX_DEPTH + 1) {
            bytes.push(ARRAY_OPEN);
            bytes.push(VAL);
        }

        assert_eq!(parse(&bytes), Err(MsdpError::TooDeep));
    }

    #[test]
    fn nesting_up_to_the_depth_cap_still_parses() {
        let mut bytes = msg(&[VAR]);
        bytes.extend_from_slice(b"X");
        bytes.push(VAL);
        for _ in 0..MAX_DEPTH {
            bytes.push(ARRAY_OPEN);
            bytes.push(VAL);
        }
        bytes.extend_from_slice(b"leaf");
        bytes.extend(std::iter::repeat_n(ARRAY_CLOSE, MAX_DEPTH));

        assert!(parse(&bytes).is_ok());
    }

    #[test]
    fn flattens_a_table_into_dotted_keys() {
        let value = MsdpValue::Table(vec![
            ("hp".to_string(), MsdpValue::String("100".to_string())),
            (
                "exits".to_string(),
                MsdpValue::Array(vec![MsdpValue::String("north".to_string())]),
            ),
        ]);
        let mut out = Flattened::default();
        flatten("Char.Vitals", &value, &mut out);
        assert_eq!(
            out.pairs,
            vec![
                ("Char.Vitals.hp".to_string(), "100".to_string()),
                ("Char.Vitals.exits.0".to_string(), "north".to_string()),
            ]
        );
        assert_eq!(out.arrays, vec![("Char.Vitals.exits".to_string(), 1)]);
    }

    /// An emptied MSDP array reports its length and nothing else — the
    /// only signal that the indices it used to have are gone.
    #[test]
    fn an_emptied_msdp_array_still_reports_its_length() {
        let mut out = Flattened::default();
        flatten("Char.Affects", &MsdpValue::Array(vec![]), &mut out);
        assert!(out.pairs.is_empty());
        assert_eq!(out.arrays, vec![("Char.Affects".to_string(), 0)]);
    }
}
