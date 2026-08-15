//! Telling the player a newer Mudular exists (docs/ARCHITECTURE.md §15).
//!
//! Deliberately split in two. **Checking** happens here, over one HTTPS
//! request to GitHub's public releases API. **Applying** does not: that is
//! `mudular-update`, the updater `dist` ships alongside the binary, which
//! already knows how this copy was installed because the installer left a
//! receipt beside it. Reimplementing that here would mean guessing at an
//! install layout we did not perform.
//!
//! Nothing in here is allowed to be load-bearing. No network, a rate limit, a
//! GitHub outage, a reply we cannot parse — every one of them means "no news",
//! never a failure to start or a stalled pane. The player asked to play a MUD.

use std::process::Command;

/// Where releases are published. A const rather than config: this is the
/// project's own release channel, not a preference, and a client that could be
/// pointed at an arbitrary update server is a different and much worse
/// security proposition.
const RELEASES_API: &str = "https://api.github.com/repos/millermatt/mudular/releases/latest";

/// The updater `dist` installs next to us. Looked up on `PATH`, where the
/// installer put both.
const UPDATER: &str = "mudular-update";

/// How long the whole check may take before we give up on it. Short: this is
/// a courtesy, and a player who launched to go and play should never wait on
/// it. It runs off the UI thread regardless, so this only bounds the task.
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// A release newer than what is running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Available {
    pub version: String,
}

/// A semantic version, reduced to what comparing two releases needs.
///
/// Not the `semver` crate: the only versions compared here are this crate's
/// own and a tag it published, so the parsing surface is a handful of digits
/// and the dependency would outweigh it. Anything unparseable — a tag with a
/// suffix, a hand-made tag, a future scheme — sorts as "not newer", so a
/// release we do not understand is never announced as an upgrade.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Version(u64, u64, u64);

fn parse_version(text: &str) -> Option<Version> {
    // Tags are `v0.4.0`; `CARGO_PKG_VERSION` is `0.4.0`.
    let text = text.trim().trim_start_matches('v');
    // A prerelease or build suffix means we cannot order it confidently
    // against a plain release, so decline rather than guess.
    if text.contains('-') || text.contains('+') {
        return None;
    }
    let mut parts = text.split('.');
    let mut next = || parts.next()?.parse::<u64>().ok();
    let version = Version(next()?, next()?, next()?);
    // Trailing junk (`0.4.0.1`) is not a version we know how to compare.
    parts.next().is_none().then_some(version)
}

/// Whether `latest` is newer than `current`, by version rather than by string.
/// Both must parse, or the answer is no.
fn is_newer(current: &str, latest: &str) -> bool {
    match (parse_version(current), parse_version(latest)) {
        (Some(current), Some(latest)) => latest > current,
        _ => false,
    }
}

/// Asks GitHub for the newest release, returning it only if it is newer than
/// `current`. `None` for every other outcome, including every failure.
pub fn check(current: &str) -> Option<Available> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .build()
        .new_agent();
    let body = agent
        .get(RELEASES_API)
        .header("Accept", "application/vnd.github+json")
        // GitHub rejects requests without one, and naming ourselves means a
        // rate-limited or misbehaving version is identifiable from their end.
        .header("User-Agent", concat!("mudular/", env!("CARGO_PKG_VERSION")))
        .call()
        .ok()?
        .body_mut()
        .read_to_string()
        .ok()?;
    let release: serde_json::Value = serde_json::from_str(&body).ok()?;
    let tag = release.get("tag_name")?.as_str()?;
    is_newer(current, tag).then(|| Available {
        version: tag.trim_start_matches('v').to_string(),
    })
}

/// Runs the updater and returns what to show the player, whether it worked or
/// not. Its own output is the report: it says which version it installed, or
/// that there was nothing to do, better than a paraphrase would.
pub fn apply() -> Vec<String> {
    let output = match Command::new(UPDATER).output() {
        Ok(output) => output,
        // Overwhelmingly this means the updater is not installed, because this
        // copy came from the `.msi` or `cargo install` rather than one of the
        // shell installers — the case where self-updating was never going to
        // work. Say what to do instead of what failed.
        Err(_) => {
            return vec![
                format!("could not run `{UPDATER}` — this copy was probably not installed by the"),
                "shell installer. Download the latest release from".to_string(),
                "https://github.com/millermatt/mudular/releases/latest".to_string(),
            ];
        }
    };
    // The updater reports progress on stderr, so both streams are the answer.
    let mut lines: Vec<String> = String::from_utf8_lossy(&output.stderr)
        .lines()
        .chain(String::from_utf8_lossy(&output.stdout).lines())
        .filter(|line| !line.trim().is_empty())
        .map(str::to_string)
        .collect();
    if output.status.success() {
        lines.push("restart Mudular to run the new version".to_string());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_later_release_is_newer() {
        assert!(is_newer("0.4.0", "v0.4.1"));
        assert!(is_newer("0.4.0", "v0.5.0"));
        assert!(is_newer("0.4.0", "v1.0.0"));
        assert!(
            is_newer("0.9.0", "v0.10.0"),
            "compared as numbers, not text"
        );
    }

    #[test]
    fn the_same_or_older_release_is_not_newer() {
        assert!(!is_newer("0.4.0", "v0.4.0"));
        assert!(!is_newer("0.4.1", "v0.4.0"));
        assert!(!is_newer("1.0.0", "v0.9.9"));
    }

    /// A version neither side understands must never be announced as an
    /// upgrade — offering to "update" someone onto something we cannot order
    /// is worse than staying quiet.
    #[test]
    fn an_unparseable_version_is_never_newer() {
        for tag in [
            "v0.5.0-rc.1", // prerelease: not orderable against a release
            "v0.5",        // too few parts
            "v0.5.0.1",    // too many
            "nightly",
            "",
            "v0.5.0+build7",
        ] {
            assert!(!is_newer("0.4.0", tag), "{tag} must not read as newer");
        }
        assert!(
            !is_newer("not-a-version", "v9.9.9"),
            "an unparseable *current* version must not trigger an update either"
        );
    }

    /// The `v` on tags is optional as far as this is concerned, so a release
    /// process that drops it does not silently stop announcing updates.
    #[test]
    fn the_v_prefix_is_optional() {
        assert!(is_newer("0.4.0", "0.5.0"));
        assert_eq!(parse_version("v1.2.3"), parse_version("1.2.3"));
    }
}
