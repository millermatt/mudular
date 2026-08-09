//! GMCP (Generic MUD Communication Protocol), Telnet option 201.
//!
//! Subnegotiation payload is `Package.SubPackage [<json>]`. The JSON payload
//! is kept as text by [`parse`]; [`flatten`] parses it lazily into the
//! dotted-path pairs the server-data store uses (docs/ARCHITECTURE.md §6.3).

use serde_json::json;
use thiserror::Error;

use super::Flattened;

/// What the client identifies itself as in `Core.Hello`.
const CLIENT_NAME: &str = "Mudular";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GmcpMessage {
    pub package: String,
    pub payload: Option<String>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum GmcpError {
    #[error("GMCP payload is not valid UTF-8")]
    InvalidUtf8,
    #[error("GMCP message is empty")]
    Empty,
}

pub fn parse(data: &[u8]) -> Result<GmcpMessage, GmcpError> {
    let text = std::str::from_utf8(data).map_err(|_| GmcpError::InvalidUtf8)?;
    let text = text.trim();
    if text.is_empty() {
        return Err(GmcpError::Empty);
    }
    match text.split_once(' ') {
        Some((package, payload)) => Ok(GmcpMessage {
            package: package.to_string(),
            payload: Some(payload.trim().to_string()),
        }),
        None => Ok(GmcpMessage {
            package: text.to_string(),
            payload: None,
        }),
    }
}

/// Encode a message back to wire form: `Package.SubPackage [<json>]`.
pub fn encode(message: &GmcpMessage) -> Vec<u8> {
    match &message.payload {
        Some(payload) => format!("{} {payload}", message.package).into_bytes(),
        None => message.package.clone().into_bytes(),
    }
}

/// `Core.Hello`, sent once the server enables GMCP (§6.3).
pub fn hello_message() -> GmcpMessage {
    GmcpMessage {
        package: "Core.Hello".to_string(),
        payload: Some(
            json!({"client": CLIENT_NAME, "version": env!("CARGO_PKG_VERSION")}).to_string(),
        ),
    }
}

/// `Core.Supports.Set`, advertising the packages we want pushed — vitals and
/// room data, the two the milestone's "done when" criterion calls for.
pub fn supports_message() -> GmcpMessage {
    GmcpMessage {
        package: "Core.Supports.Set".to_string(),
        payload: Some(json!(["Char 1", "Room 1"]).to_string()),
    }
}

/// Flattens a message's JSON payload into dotted-path `(key, value)` pairs
/// for the server-data store, e.g. `Char.Vitals {"hp":100}` becomes
/// `("Char.Vitals.hp", "100")`. A non-object/array payload (or one that
/// fails to parse as JSON) is stored verbatim under the bare package name.
pub fn flatten(message: &GmcpMessage) -> Flattened {
    let mut out = Flattened::default();
    if let Some(text) = &message.payload {
        match serde_json::from_str::<serde_json::Value>(text) {
            Ok(value) => flatten_json(&message.package, &value, &mut out),
            Err(_) => out.pairs.push((message.package.clone(), text.clone())),
        }
    }
    out
}

fn flatten_json(prefix: &str, value: &serde_json::Value, out: &mut Flattened) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, val) in map {
                flatten_json(&format!("{prefix}.{key}"), val, out);
            }
        }
        serde_json::Value::Array(items) => {
            // Recorded before the recursion, and for an empty array too:
            // `[]` emits no pairs at all, which is exactly the case where
            // a stale `….0` would otherwise survive unnoticed.
            out.arrays.push((prefix.to_string(), items.len()));
            for (index, val) in items.iter().enumerate() {
                flatten_json(&format!("{prefix}.{index}"), val, out);
            }
        }
        serde_json::Value::String(s) => out.pairs.push((prefix.to_string(), s.clone())),
        serde_json::Value::Null => {}
        other => out.pairs.push((prefix.to_string(), other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_package_with_json_payload() {
        let msg = parse(b"Char.Vitals {\"hp\": 100}").unwrap();
        assert_eq!(msg.package, "Char.Vitals");
        assert_eq!(msg.payload.as_deref(), Some("{\"hp\": 100}"));
    }

    #[test]
    fn parses_bare_package() {
        let msg = parse(b"Core.Ping").unwrap();
        assert_eq!(msg.package, "Core.Ping");
        assert_eq!(msg.payload, None);
    }

    #[test]
    fn rejects_empty_message() {
        assert_eq!(parse(b"  "), Err(GmcpError::Empty));
    }

    #[test]
    fn flattens_nested_object_payload_into_dotted_keys() {
        let msg = parse(br#"Char.Vitals {"hp": 100, "stats": {"str": 18}}"#).unwrap();
        let mut flat = flatten(&msg);
        flat.pairs.sort();
        assert_eq!(
            flat.pairs,
            vec![
                ("Char.Vitals.hp".to_string(), "100".to_string()),
                ("Char.Vitals.stats.str".to_string(), "18".to_string()),
            ]
        );
        assert!(flat.arrays.is_empty(), "no arrays in this payload");
    }

    #[test]
    fn flattens_array_payload_by_index() {
        let msg = parse(br#"Room.Exits ["north", "south"]"#).unwrap();
        let flat = flatten(&msg);
        assert_eq!(
            flat.pairs,
            vec![
                ("Room.Exits.0".to_string(), "north".to_string()),
                ("Room.Exits.1".to_string(), "south".to_string()),
            ]
        );
        assert_eq!(flat.arrays, vec![("Room.Exits".to_string(), 2)]);
    }

    /// An empty array emits no pairs at all, so its length is the only
    /// signal that everything under it is now gone.
    #[test]
    fn an_emptied_array_still_reports_its_length() {
        let msg = parse(br#"Char.Affects []"#).unwrap();
        let flat = flatten(&msg);
        assert!(flat.pairs.is_empty());
        assert_eq!(flat.arrays, vec![("Char.Affects".to_string(), 0)]);
    }

    #[test]
    fn a_non_json_payload_is_stored_verbatim_under_the_package_name() {
        let msg = parse(b"Char.Name Kestrel").unwrap();
        assert_eq!(
            flatten(&msg).pairs,
            vec![("Char.Name".to_string(), "Kestrel".to_string())]
        );
    }

    #[test]
    fn a_bare_package_with_no_payload_flattens_to_nothing() {
        let msg = parse(b"Core.Ping").unwrap();
        let flat = flatten(&msg);
        assert!(flat.pairs.is_empty() && flat.arrays.is_empty());
    }

    #[test]
    fn encode_round_trips_through_parse() {
        let msg = hello_message();
        let encoded = encode(&msg);
        let parsed = parse(&encoded).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn hello_advertises_the_client_name() {
        let msg = hello_message();
        assert_eq!(msg.package, "Core.Hello");
        assert!(msg.payload.unwrap().contains("Mudular"));
    }
}
