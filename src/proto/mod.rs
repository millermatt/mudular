//! Sans-IO protocol codecs: pure state machines with no sockets and no async.
//!
//! The session task feeds them bytes and writes their replies to the
//! transport; everything here is unit-testable with byte fixtures
//! (docs/ARCHITECTURE.md §6).

pub mod gmcp;
pub mod mccp;
pub mod msdp;
pub mod telnet;
