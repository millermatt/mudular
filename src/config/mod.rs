//! YAML configuration and character profiles.
//!
//! Layout under the config dir (docs/ARCHITECTURE.md §10):
//! `mudular.yaml` (app settings), `global.yaml` (global rules),
//! `modules/*.yaml` (shared rule modules), `profiles/*.yaml` (characters).
//! Directory discovery and the scope merge land in M3/M4; schemas use
//! `deny_unknown_fields` so typos fail loudly with file context.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::engine::{Alias, RuleModule, Trigger};
use crate::net::VerifyMode;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub name: String,
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub tls: TlsSettings,
    #[serde(default = "default_charset")]
    pub charset: String,
    /// Shared rule modules, applied in order (scope layer 2).
    #[serde(default)]
    pub modules: Vec<String>,
    /// Profile-local overrides (scope layer 3).
    #[serde(default)]
    pub variables: HashMap<String, String>,
    #[serde(default)]
    pub aliases: Vec<Alias>,
    #[serde(default)]
    pub triggers: Vec<Trigger>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub verify: VerifyMode,
}

fn default_charset() -> String {
    "utf-8".to_string()
}

pub fn load_profile(path: &Path) -> Result<Profile> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading profile {}", path.display()))?;
    serde_yaml::from_str(&text).with_context(|| format!("parsing profile {}", path.display()))
}

pub fn load_module(path: &Path) -> Result<RuleModule> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading module {}", path.display()))?;
    serde_yaml::from_str(&text).with_context(|| format!("parsing module {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_profile() {
        let profile: Profile = serde_yaml::from_str(
            r#"
            name: kestrel
            host: mud.example.org
            port: 4443
            tls:
              enabled: true
              verify: pinned
            charset: utf-8
            modules: [uw-common, uw-combat]
            variables:
              target: rat
            "#,
        )
        .unwrap();
        assert_eq!(profile.name, "kestrel");
        assert!(profile.tls.enabled);
        assert_eq!(profile.tls.verify, VerifyMode::Pinned);
        assert_eq!(profile.modules, vec!["uw-common", "uw-combat"]);
    }

    #[test]
    fn defaults_apply_for_minimal_profile() {
        let profile: Profile =
            serde_yaml::from_str("name: min\nhost: mud.example.org\nport: 23\n").unwrap();
        assert!(!profile.tls.enabled);
        assert_eq!(profile.tls.verify, VerifyMode::Full);
        assert_eq!(profile.charset, "utf-8");
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let result: std::result::Result<Profile, _> =
            serde_yaml::from_str("name: x\nhost: h\nport: 23\ntypo_field: true\n");
        assert!(result.is_err());
    }
}
