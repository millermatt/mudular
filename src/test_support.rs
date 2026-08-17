//! Helpers shared by tests across modules.
//!
//! It lives at the crate root rather than inside whichever module happened to
//! need it first. `TempDir` was written for `net::pins` and then reached for
//! from `config`, `ui` and `app` — 100-odd call sites naming a path through
//! another module's *private test module*, which reads as though pin storage
//! were involved in parsing a profile.
//!
//! It is also the one thing here that a workspace split cannot carry across
//! as it stands (docs/ARCHITECTURE.md §4): `#[cfg(test)]` items are not
//! compiled for dependent crates, so a helper shared this way stops
//! resolving the moment these modules become separate crates. Gathering it
//! here does not fix that on its own — it makes the fix one file's problem
//! rather than a hundred call sites' problem.

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
