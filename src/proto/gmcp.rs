//! GMCP (Generic MUD Communication Protocol), Telnet option 201.
//!
//! Subnegotiation payload is `Package.SubPackage [<json>]`. The JSON payload
//! is kept as text here; consumers parse it lazily (M6).

use thiserror::Error;

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
}
