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

/// Advertised no matter what the config declares: vitals and room data,
/// the two the M6 "done when" criterion calls for.
const DEFAULT_PACKAGES: [&str; 2] = ["Char 1", "Room 1"];

/// `Core.Supports.Set`, advertising the packages we want pushed (§6.3).
///
/// `declared` is what the config layers asked for on top of the defaults
/// (§7.3) — added rather than substituted, so a shared module that names
/// only `Group 1` cannot silently unsubscribe the vitals every other rule
/// reads. A declaration for a package already in the list replaces that
/// entry instead: which of two advertised versions of one package a server
/// honours is the server's choice, so naming both is a coin flip.
pub fn supports_message(declared: &[String]) -> GmcpMessage {
    let mut packages: Vec<String> = DEFAULT_PACKAGES.iter().map(|p| p.to_string()).collect();
    for entry in declared {
        let entry = entry.trim();
        // A blank list entry would go out as `""`, which names no package
        // any server can act on.
        if entry.is_empty() {
            continue;
        }
        match packages
            .iter_mut()
            .find(|listed| same_package(listed, entry))
        {
            Some(listed) => *listed = entry.to_string(),
            None => packages.push(entry.to_string()),
        }
    }
    GmcpMessage {
        package: "Core.Supports.Set".to_string(),
        payload: Some(json!(packages).to_string()),
    }
}

/// Whether two entries name the same package: the version follows a space,
/// and GMCP package names are not case-sensitive.
fn same_package(a: &str, b: &str) -> bool {
    let name = |entry: &str| entry.split(' ').next().unwrap_or("").to_ascii_lowercase();
    name(a) == name(b)
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
    fn supports_advertises_the_defaults_when_nothing_is_declared() {
        let msg = supports_message(&[]);
        assert_eq!(msg.package, "Core.Supports.Set");
        assert_eq!(
            encode(&msg),
            br#"Core.Supports.Set ["Char 1","Room 1"]"#.to_vec()
        );
    }

    /// A server that gates its pushes on `Core.Supports.Set` sends nothing
    /// for a package it was never named, so a declared one has to reach the
    /// wire — and the defaults have to survive alongside it.
    #[test]
    fn declared_packages_are_advertised_after_the_defaults() {
        let declared = ["Group 1".to_string(), "Comm.Channel 1".to_string()];
        assert_eq!(
            encode(&supports_message(&declared)),
            br#"Core.Supports.Set ["Char 1","Room 1","Group 1","Comm.Channel 1"]"#.to_vec()
        );
    }

    /// Which of two versions of one package a server honours is its own
    /// business, so naming both is a coin flip: a declaration for a package
    /// already listed replaces that entry instead of joining it.
    #[test]
    fn a_declared_version_replaces_an_earlier_entry_for_the_same_package() {
        let declared = [
            "char 2".to_string(),
            "Group 1".to_string(),
            "Group 2".into(),
        ];
        assert_eq!(
            encode(&supports_message(&declared)),
            br#"Core.Supports.Set ["char 2","Room 1","Group 2"]"#.to_vec()
        );
    }

    /// A stray empty list entry would otherwise be advertised as `""`, which
    /// no server can make sense of.
    #[test]
    fn a_blank_declaration_is_dropped_rather_than_advertised() {
        assert_eq!(
            encode(&supports_message(&["  ".to_string()])),
            br#"Core.Supports.Set ["Char 1","Room 1"]"#.to_vec()
        );
    }

    #[test]
    fn hello_advertises_the_client_name() {
        let msg = hello_message();
        assert_eq!(msg.package, "Core.Hello");
        assert!(msg.payload.unwrap().contains("Mudular"));
    }
}
