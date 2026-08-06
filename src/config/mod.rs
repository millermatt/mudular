//! YAML configuration and character profiles.
//!
//! Layout under the config dir (docs/ARCHITECTURE.md §10):
//! `mudular.yaml` (app settings), `global.yaml` (global rules),
//! `modules/*.yaml` (shared rule modules), `profiles/*.yaml` (characters).
//! The scope merge (global → modules → profile) lands in M4; schemas use
//! `deny_unknown_fields` so typos fail loudly with file context.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyModifiers};
use serde::{Deserialize, Deserializer};

use crate::engine::{Alias, RuleModule, Trigger};
use crate::net::VerifyMode;

/// Where the config dir lives: `--config-dir`, or the platform default.
pub fn config_dir(override_dir: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(dir) = override_dir {
        return Ok(dir);
    }
    let dirs = directories::ProjectDirs::from("", "", "mudular")
        .ok_or_else(|| anyhow::anyhow!("cannot determine a config directory; use --config-dir"))?;
    Ok(dirs.config_dir().to_path_buf())
}

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

/// App-wide settings (`mudular.yaml`): keybinds today; theme and
/// scrollback size are later milestones. Absent entirely is fine — a
/// fresh install has sensible defaults and no config dir yet (§15).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    #[serde(default)]
    pub keybinds: Keybinds,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Keybinds {
    #[serde(default = "default_quit")]
    pub quit: KeyBinding,
}

impl Default for Keybinds {
    fn default() -> Self {
        Self {
            quit: default_quit(),
        }
    }
}

fn default_quit() -> KeyBinding {
    "ctrl+c".parse().expect("built-in default keybinding")
}

/// A single key combination, parsed from strings like `ctrl+c` or `f1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyBinding {
    code: KeyCode,
    modifiers: KeyModifiers,
}

impl KeyBinding {
    pub fn matches(&self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        self.code == code && self.modifiers == modifiers
    }
}

impl std::fmt::Display for KeyBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.modifiers.contains(KeyModifiers::CONTROL) {
            write!(f, "Ctrl+")?;
        }
        if self.modifiers.contains(KeyModifiers::ALT) {
            write!(f, "Alt+")?;
        }
        if self.modifiers.contains(KeyModifiers::SHIFT) {
            write!(f, "Shift+")?;
        }
        match self.code {
            KeyCode::Char(c) => write!(f, "{}", c.to_ascii_uppercase()),
            other => write!(f, "{other:?}"),
        }
    }
}

impl std::str::FromStr for KeyBinding {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let mut modifiers = KeyModifiers::NONE;
        let parts: Vec<&str> = s.split('+').map(str::trim).collect();
        let (key, mods) = parts
            .split_last()
            .ok_or_else(|| format!("empty keybinding `{s}`"))?;

        for part in mods {
            match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => modifiers |= KeyModifiers::CONTROL,
                "alt" => modifiers |= KeyModifiers::ALT,
                "shift" => modifiers |= KeyModifiers::SHIFT,
                other => return Err(format!("unknown modifier `{other}` in keybinding `{s}`")),
            }
        }

        let code = match key.to_ascii_lowercase().as_str() {
            "esc" | "escape" => KeyCode::Esc,
            "enter" | "return" => KeyCode::Enter,
            "tab" => KeyCode::Tab,
            "backspace" => KeyCode::Backspace,
            other if other.chars().count() == 1 => {
                KeyCode::Char(other.chars().next().expect("checked non-empty"))
            }
            other => return Err(format!("unknown key `{other}` in keybinding `{s}`")),
        };

        Ok(KeyBinding { code, modifiers })
    }
}

impl<'de> Deserialize<'de> for KeyBinding {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Loads `mudular.yaml` from the config dir, or the defaults if it (or the
/// dir) doesn't exist yet — the common case before a first-run wizard
/// exists (§15).
pub fn load_app_config(dir: &Path) -> Result<AppConfig> {
    let path = dir.join("mudular.yaml");
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            serde_yaml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(AppConfig::default()),
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

/// The path a named profile lives at under the config dir.
pub fn profile_path(dir: &Path, name: &str) -> PathBuf {
    dir.join("profiles").join(format!("{name}.yaml"))
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

    #[test]
    fn parses_a_profile_with_a_legacy_charset() {
        let profile: Profile =
            serde_yaml::from_str("name: retro\nhost: bbs.example.org\nport: 23\ncharset: cp437\n")
                .unwrap();
        assert_eq!(profile.charset, "cp437");
    }

    #[test]
    fn parses_modifier_combinations() {
        let binding: KeyBinding = "ctrl+c".parse().unwrap();
        assert!(binding.matches(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!binding.matches(KeyCode::Char('c'), KeyModifiers::NONE));
        assert!(!binding.matches(KeyCode::Char('d'), KeyModifiers::CONTROL));

        let plain: KeyBinding = "esc".parse().unwrap();
        assert!(plain.matches(KeyCode::Esc, KeyModifiers::NONE));

        let multi: KeyBinding = "ctrl+shift+q".parse().unwrap();
        assert!(multi.matches(
            KeyCode::Char('q'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        ));
    }

    #[test]
    fn rejects_unknown_modifiers_and_keys() {
        assert!("hyper+c".parse::<KeyBinding>().is_err());
        assert!("ctrl+".parse::<KeyBinding>().is_err());
        assert!("".parse::<KeyBinding>().is_err());
    }

    #[test]
    fn displays_a_human_readable_form() {
        let binding: KeyBinding = "ctrl+c".parse().unwrap();
        assert_eq!(binding.to_string(), "Ctrl+C");
    }

    #[test]
    fn keybinding_round_trips_through_yaml() {
        #[derive(Deserialize)]
        struct Wrapper {
            key: KeyBinding,
        }
        let wrapper: Wrapper = serde_yaml::from_str("key: ctrl+q").unwrap();
        assert!(
            wrapper
                .key
                .matches(KeyCode::Char('q'), KeyModifiers::CONTROL)
        );
    }

    #[test]
    fn app_config_defaults_to_ctrl_c_quit_when_file_is_absent() {
        let dir = std::env::temp_dir().join(format!("mudular-cfg-{}", std::process::id()));
        let config = load_app_config(&dir).unwrap();
        assert!(
            config
                .keybinds
                .quit
                .matches(KeyCode::Char('c'), KeyModifiers::CONTROL)
        );
    }

    #[test]
    fn app_config_loads_a_remapped_quit_key() {
        let dir = std::env::temp_dir().join(format!("mudular-cfg2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("mudular.yaml"), "keybinds:\n  quit: ctrl+q\n").unwrap();

        let config = load_app_config(&dir).unwrap();
        assert!(
            config
                .keybinds
                .quit
                .matches(KeyCode::Char('q'), KeyModifiers::CONTROL)
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn profile_path_lives_under_profiles_subdir() {
        let path = profile_path(Path::new("/cfg"), "kestrel");
        assert_eq!(path, Path::new("/cfg/profiles/kestrel.yaml"));
    }
}
