//! MSDP (MUD Server Data Protocol), Telnet option 69.
//!
//! Type definitions and wire constants; the codec lands in M6
//! (docs/ARCHITECTURE.md §6.3). MSDP values are normalized into the same
//! server-data namespace as GMCP.

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
