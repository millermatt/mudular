//! Sans-IO protocol codecs: pure state machines with no sockets and no async.
//!
//! The session task feeds them bytes and writes their replies to the
//! transport; everything here is unit-testable with byte fixtures
//! (docs/ARCHITECTURE.md §6).

pub mod charset;
pub mod gmcp;
pub mod mccp;
pub mod msdp;
pub mod telnet;

/// A flattened out-of-band payload, in the one shape both GMCP and MSDP
/// normalize into so the engine sees a single server-data namespace
/// (docs/ARCHITECTURE.md §6.3).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Flattened {
    /// Dotted-path `(key, value)` pairs for the server-data store.
    pub pairs: Vec<(String, String)>,
    /// `(path, length)` for every array flattened, including empty ones.
    ///
    /// Both protocols flatten arrays to *positional* keys
    /// (`Char.Affects.0`), so a store that only merges `pairs` keeps
    /// `Char.Affects.1` forever once the array shrinks — a dropped buff
    /// that never expires. These let the store drop the indices the new
    /// payload no longer has.
    pub arrays: Vec<(String, usize)>,
}
