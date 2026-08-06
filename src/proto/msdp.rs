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
}

/// Parses a complete MSDP subnegotiation payload into its top-level
/// `VAR`/`VAL` pairs.
pub fn parse(data: &[u8]) -> Result<Vec<(String, MsdpValue)>, MsdpError> {
    let mut cursor = Cursor { data, pos: 0 };
    let pairs = parse_pairs(&mut cursor)?;
    if cursor.pos != cursor.data.len() {
        return Err(MsdpError::TrailingBytes);
    }
    Ok(pairs)
}

/// Flattens a parsed value into dotted-path `(key, value)` pairs, mirroring
/// [`crate::proto::gmcp::flatten`] so both protocols land in one shape for
/// the server-data store.
pub fn flatten(name: &str, value: &MsdpValue, out: &mut Vec<(String, String)>) {
    match value {
        MsdpValue::String(s) => out.push((name.to_string(), s.clone())),
        MsdpValue::Array(items) => {
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

fn parse_pairs(cursor: &mut Cursor) -> Result<Vec<(String, MsdpValue)>, MsdpError> {
    let mut pairs = Vec::new();
    while cursor.peek() == Some(VAR) {
        cursor.next();
        let name = cursor.take_until_control();
        if cursor.next() != Some(VAL) {
            return Err(MsdpError::ExpectedVal);
        }
        let value = parse_value(cursor)?;
        pairs.push((name, value));
    }
    Ok(pairs)
}

fn parse_value(cursor: &mut Cursor) -> Result<MsdpValue, MsdpError> {
    match cursor.peek() {
        Some(ARRAY_OPEN) => {
            cursor.next();
            let mut items = Vec::new();
            while cursor.peek() == Some(VAL) {
                cursor.next();
                items.push(parse_value(cursor)?);
            }
            if cursor.next() != Some(ARRAY_CLOSE) {
                return Err(MsdpError::Unterminated);
            }
            Ok(MsdpValue::Array(items))
        }
        Some(TABLE_OPEN) => {
            cursor.next();
            let pairs = parse_pairs(cursor)?;
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
    fn flattens_a_table_into_dotted_keys() {
        let value = MsdpValue::Table(vec![
            ("hp".to_string(), MsdpValue::String("100".to_string())),
            (
                "exits".to_string(),
                MsdpValue::Array(vec![MsdpValue::String("north".to_string())]),
            ),
        ]);
        let mut out = Vec::new();
        flatten("Char.Vitals", &value, &mut out);
        assert_eq!(
            out,
            vec![
                ("Char.Vitals.hp".to_string(), "100".to_string()),
                ("Char.Vitals.exits.0".to_string(), "north".to_string()),
            ]
        );
    }
}
