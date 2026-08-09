//! Trust-on-first-use store for pinned certificate fingerprints.
//!
//! MUDs frequently run self-signed certificates, so `verify: pinned`
//! records the server's SHA-256 on first connect and refuses to connect
//! later if it changes (docs/ARCHITECTURE.md §5, §13). The format is one
//! `<host>:<port> <sha256-hex>` line per server, in the spirit of
//! `known_hosts` — hand-editable and diffable.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const HEADER: &str = "# mudular pinned certificate fingerprints (SHA-256)";

#[derive(Debug, Clone)]
pub struct PinStore {
    path: PathBuf,
}

impl PinStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    // Unlike the other allowances in this crate this one marks no seam:
    // nothing has ever called it. Left rather than removed as an unrelated
    // change — a removal candidate.
    #[allow(dead_code)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The fingerprint recorded for `key`, if this server is known.
    pub fn get(&self, key: &str) -> Result<Option<String>> {
        Ok(self.load()?.remove(key))
    }

    /// Record (or replace) the fingerprint for `key`.
    pub fn insert(&self, key: &str, fingerprint: &str) -> Result<()> {
        let mut pins = self.load()?;
        pins.insert(key.to_string(), fingerprint.to_string());

        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }

        let mut out = String::from(HEADER);
        out.push('\n');
        for (host, fingerprint) in &pins {
            out.push_str(host);
            out.push(' ');
            out.push_str(fingerprint);
            out.push('\n');
        }
        write_pins(&self.path, out.as_bytes())
            .with_context(|| format!("writing {}", self.path.display()))
    }

    fn load(&self) -> Result<BTreeMap<String, String>> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            // A missing store just means nothing has been pinned yet.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(err) => {
                return Err(err).with_context(|| format!("reading {}", self.path.display()));
            }
        };

        Ok(text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .filter_map(|line| line.split_once(' '))
            .map(|(host, fingerprint)| (host.to_string(), fingerprint.trim().to_string()))
            .collect())
    }
}

/// The key a server is stored under.
pub fn key(host: &str, port: u16) -> String {
    format!("{host}:{port}")
}

/// Like `std::fs::write`, but owner-only on Unix: a pin identifies a server
/// the player connects to, private in the same way the profile that names
/// it is.
fn write_pins(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    fn store() -> (PinStore, tempdir::TempDir) {
        let dir = tempdir::TempDir::new();
        let store = PinStore::new(dir.path().join("nested").join("known_certs"));
        (store, dir)
    }

    #[test]
    fn absent_store_has_no_pins() {
        let (store, _dir) = store();
        assert_eq!(store.get(&key("mud.example.org", 4443)).unwrap(), None);
    }

    #[test]
    fn round_trips_a_pin_creating_missing_directories() {
        let (store, _dir) = store();
        let key = key("mud.example.org", 4443);
        store.insert(&key, "aa:bb").unwrap();
        assert_eq!(store.get(&key).unwrap(), Some("aa:bb".to_string()));
    }

    #[test]
    fn keeps_entries_for_other_servers() {
        let (store, _dir) = store();
        store.insert(&key("one.example.org", 4443), "1111").unwrap();
        store.insert(&key("two.example.org", 4443), "2222").unwrap();

        assert_eq!(
            store.get(&key("one.example.org", 4443)).unwrap(),
            Some("1111".to_string())
        );
        assert_eq!(
            store.get(&key("two.example.org", 4443)).unwrap(),
            Some("2222".to_string())
        );
    }

    #[test]
    fn distinguishes_ports_on_the_same_host() {
        let (store, _dir) = store();
        store.insert(&key("mud.example.org", 4443), "4443").unwrap();
        assert_eq!(store.get(&key("mud.example.org", 23)).unwrap(), None);
    }

    /// The pin store names servers the player connects to — owner-only on
    /// disk, not left at the process umask.
    #[cfg(unix)]
    #[test]
    fn the_store_file_is_created_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let (store, _dir) = store();
        store
            .insert(&key("mud.example.org", 4443), "aa:bb")
            .unwrap();

        let mode = std::fs::metadata(store.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
    }

    #[test]
    fn replaces_a_fingerprint_in_place() {
        let (store, _dir) = store();
        let key = key("mud.example.org", 4443);
        store.insert(&key, "old").unwrap();
        store.insert(&key, "new").unwrap();
        assert_eq!(store.get(&key).unwrap(), Some("new".to_string()));

        let text = std::fs::read_to_string(store.path()).unwrap();
        assert_eq!(
            text.lines()
                .filter(|l| l.starts_with("mud.example.org:"))
                .count(),
            1,
            "the old fingerprint must be replaced, not appended: {text:?}"
        );
    }

    #[test]
    fn ignores_comments_and_blank_lines() {
        let (store, dir) = store();
        std::fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        std::fs::write(store.path(), "# a comment\n\nmud.example.org:4443 abcd\n").unwrap();
        assert_eq!(
            store.get(&key("mud.example.org", 4443)).unwrap(),
            Some("abcd".to_string())
        );
        drop(dir);
    }

    /// Minimal scratch directory that cleans up after itself; the `tempfile`
    /// crate would be a dependency for one test helper.
    pub mod tempdir {
        use std::path::{Path, PathBuf};
        use std::sync::atomic::{AtomicU32, Ordering};

        static COUNTER: AtomicU32 = AtomicU32::new(0);

        pub struct TempDir(PathBuf);

        impl TempDir {
            #[allow(clippy::new_without_default)]
            pub fn new() -> Self {
                let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir()
                    .join(format!("mudular-pins-{}-{unique}", std::process::id()));
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
    }
}
