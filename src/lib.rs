//! The sans-IO half of Mudular, exposed so out-of-process harnesses can
//! drive it.
//!
//! Mudular is a binary (`main.rs`); this library exists because the fuzz
//! targets in `fuzz/` are a separate crate and cannot reach into a binary's
//! private modules. Only `proto` is published, because only `proto` parses
//! attacker-controlled bytes (docs/ARCHITECTURE.md §12) — and because it is
//! the one module that already depends on nothing above it (§4), so lifting
//! it out costs no restructuring.
//!
//! This is not a stability promise. The crate ships as an application; the
//! library surface is a testing seam, and it moves with the binary.

pub mod proto;
