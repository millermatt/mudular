//! Test helpers shared by mudular's modules.
//!
//! A crate rather than a module, and a `dev-dependency` rather than a
//! dependency, because of a Rust rule that only bites across crate
//! boundaries: `#[cfg(test)]` items are compiled when testing *their own*
//! crate and are invisible to anything that depends on it. A helper shared
//! the way this one is — `net` wrote it, `config`, `ui` and `app` all use it
//! — therefore cannot live behind `#[cfg(test)]` once those modules become
//! separate crates (docs/ARCHITECTURE.md §4).
//!
//! Being a dev-dependency is what keeps it out of the shipped binary: it is
//! compiled for tests and benches and never linked into a release build, so
//! the single-static-binary promise of §15 is untouched.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Minimal scratch directory that cleans up after itself; the `tempfile`
/// crate would be a dependency for one test helper.
pub struct TempDir(PathBuf);

impl TempDir {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("mudular-test-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}
