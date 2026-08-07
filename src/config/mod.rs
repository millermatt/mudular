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
use ratatui::style::Color;
use serde::{Deserialize, Deserializer};

use crate::engine::{Alias, RuleModule, Timer, Trigger};
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
    /// Part of the on-disk schema: `deny_unknown_fields` means dropping it
    /// would reject configs that set it. Read by the user, not the code.
    #[allow(dead_code)]
    pub name: String,
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub tls: TlsSettings,
    #[serde(default = "default_charset")]
    pub charset: String,
    /// Tints this character's pane border and tab entry, so panes are
    /// told apart at a glance rather than by reading their titles
    /// (docs/ARCHITECTURE.md §11). A colour name (`cyan`, `light blue`),
    /// `#rrggbb`, or a 0-255 terminal palette index.
    #[serde(default, deserialize_with = "parse_color")]
    pub color: Option<Color>,
    /// Answers the server's opening name/password prompts (§10). There is
    /// deliberately no `password:` field: `deny_unknown_fields` turns an
    /// attempt to put one here into a load error naming it, which is a
    /// better answer than quietly accepting a secret into a plaintext file.
    #[serde(default)]
    pub login: Option<Login>,
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
    #[serde(default)]
    pub timers: Vec<Timer>,
    /// Overrides the install-wide `cross_session` block for this character
    /// (docs/ARCHITECTURE.md §7.5). Receiver-side by design: only the
    /// profile whose aliases would run can opt into running them.
    #[serde(default)]
    pub cross_session: Option<CrossSessionOverride>,
}

/// Auto-login settings. The password lives in the OS keyring, not here
/// (docs/ARCHITECTURE.md §13); store it with `mudular --set-password`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Login {
    /// The character name to send at the name prompt.
    pub name: String,
    /// Overrides for MUDs whose prompts the defaults don't recognise.
    #[serde(default)]
    pub name_prompt: Option<String>,
    #[serde(default)]
    pub password_prompt: Option<String>,
}

/// The keyring service every profile's password is filed under. The account
/// is the profile name, so two characters on one MUD keep separate secrets.
const KEYRING_SERVICE: &str = "mudular";

/// Reads a profile's stored password. A missing entry is `Ok(None)` — an
/// unconfigured keyring is a normal state, not a failure to connect.
pub fn stored_password(profile: &str) -> Result<Option<String>> {
    match keyring::Entry::new(KEYRING_SERVICE, profile)?.get_password() {
        Ok(password) => Ok(Some(password)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(err) => {
            Err(anyhow::Error::new(err)
                .context(format!("reading the keyring password for {profile}")))
        }
    }
}

/// Stores (or replaces) a profile's password in the OS keyring.
pub fn store_password(profile: &str, password: &str) -> Result<()> {
    keyring::Entry::new(KEYRING_SERVICE, profile)?
        .set_password(password)
        .with_context(|| format!("storing the keyring password for {profile}"))
}

/// How a session treats commands other sessions inject into it (§7.5).
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CrossSession {
    /// Run injected commands through this session's aliases.
    #[serde(default)]
    pub expand_aliases: bool,
    /// How many times an injected command may bounce onward.
    #[serde(default = "default_max_hops")]
    pub max_hops: u8,
}

impl Default for CrossSession {
    fn default() -> Self {
        Self {
            expand_aliases: false,
            max_hops: default_max_hops(),
        }
    }
}

fn default_max_hops() -> u8 {
    1
}

/// The same block with every field optional, for the per-profile override.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CrossSessionOverride {
    #[serde(default)]
    pub expand_aliases: Option<bool>,
    #[serde(default)]
    pub max_hops: Option<u8>,
}

impl CrossSession {
    /// Applies a profile's overrides over the install-wide defaults.
    pub fn with_override(self, over: Option<CrossSessionOverride>) -> Self {
        let Some(over) = over else { return self };
        Self {
            expand_aliases: over.expand_aliases.unwrap_or(self.expand_aliases),
            max_hops: over.max_hops.unwrap_or(self.max_hops),
        }
    }
}

/// A named pane that collects matching lines out of the main scrollback
/// (docs/ARCHITECTURE.md §11.1).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Channel {
    pub name: String,
    /// Sugar: each pattern compiles to an ordinary route trigger, so
    /// classification gets the engine's full regex machinery.
    #[serde(default, rename = "match")]
    pub matches: Vec<String>,
    /// `false` (the default) moves matching lines out of the main
    /// scrollback; `true` mirrors them to both.
    #[serde(default)]
    pub keep_in_main: bool,
    #[serde(default)]
    pub timestamps: bool,
    /// Pins the channel to one session instead of aggregating across all.
    #[serde(default)]
    pub session: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub verify: VerifyMode,
}

/// Fails at load with the offending string, like every other schema typo —
/// a silently ignored colour is a setting the user thinks they applied.
fn parse_color<'de, D>(deserializer: D) -> std::result::Result<Option<Color>, D::Error>
where
    D: Deserializer<'de>,
{
    use std::str::FromStr;

    let Some(name) = Option::<String>::deserialize(deserializer)? else {
        return Ok(None);
    };
    Color::from_str(&name).map(Some).map_err(|_| {
        serde::de::Error::custom(format!(
            "unknown color {name:?}: use a name (cyan, light blue), #rrggbb, or 0-255"
        ))
    })
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
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    #[serde(default)]
    pub keybinds: Keybinds,
    /// Channel panes shared by every session (docs/ARCHITECTURE.md §11.1).
    #[serde(default)]
    pub channels: Vec<Channel>,
    /// Install-wide default for injected commands (§7.5).
    #[serde(default)]
    pub cross_session: CrossSession,
    /// Commands each session remembers for `Up`/`Down` recall
    /// (docs/ARCHITECTURE.md §11.3).
    #[serde(default = "default_history_size")]
    pub history_size: usize,
}

/// Hand-written rather than derived: a derived `Default` would give a
/// config-less install a zero-length history, so the one case with no file
/// to fix it would be the one case with the feature switched off.
impl Default for AppConfig {
    fn default() -> Self {
        Self {
            keybinds: Keybinds::default(),
            channels: Vec::new(),
            cross_session: CrossSession::default(),
            history_size: default_history_size(),
        }
    }
}

fn default_history_size() -> usize {
    500
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Keybinds {
    #[serde(default = "default_quit")]
    pub quit: KeyBinding,
    /// Toggles the raw GMCP inspector view (docs/ARCHITECTURE.md §14 M6).
    #[serde(default = "default_gmcp_inspector")]
    pub gmcp_inspector: KeyBinding,
    /// Cycles focus to the next pane. Terminals vary in whether they
    /// deliver Ctrl+Tab at all — remap to `alt+tab` if yours does not
    /// (docs/ARCHITECTURE.md §11).
    #[serde(default = "default_focus_next")]
    pub focus_next: KeyBinding,
    /// Switches between the tabbed and split layouts.
    #[serde(default = "default_cycle_layout")]
    pub cycle_layout: KeyBinding,
    /// Shows or hides the channel panes (§11.1).
    #[serde(default = "default_toggle_channels")]
    pub toggle_channels: KeyBinding,
    /// Opens the help overlay listing every binding (§11.2).
    #[serde(default = "default_help")]
    pub help: KeyBinding,
}

impl Default for Keybinds {
    fn default() -> Self {
        Self {
            quit: default_quit(),
            gmcp_inspector: default_gmcp_inspector(),
            focus_next: default_focus_next(),
            cycle_layout: default_cycle_layout(),
            toggle_channels: default_toggle_channels(),
            help: default_help(),
        }
    }
}

fn default_focus_next() -> KeyBinding {
    "ctrl+tab".parse().expect("built-in default keybinding")
}

fn default_cycle_layout() -> KeyBinding {
    "f3".parse().expect("built-in default keybinding")
}

fn default_toggle_channels() -> KeyBinding {
    "f4".parse().expect("built-in default keybinding")
}

fn default_help() -> KeyBinding {
    // The one key a user tries unprompted, and clear of the F2-F4 toggles.
    "f1".parse().expect("built-in default keybinding")
}

fn default_quit() -> KeyBinding {
    "ctrl+c".parse().expect("built-in default keybinding")
}

fn default_gmcp_inspector() -> KeyBinding {
    "f2".parse().expect("built-in default keybinding")
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
            KeyCode::F(n) => write!(f, "F{n}"),
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

        let lower = key.to_ascii_lowercase();
        let code = match lower.as_str() {
            "esc" | "escape" => KeyCode::Esc,
            "enter" | "return" => KeyCode::Enter,
            "tab" => KeyCode::Tab,
            "backspace" => KeyCode::Backspace,
            other
                if other.len() > 1
                    && other.starts_with('f')
                    && other[1..].chars().all(|c| c.is_ascii_digit()) =>
            {
                let n: u8 = other[1..]
                    .parse()
                    .map_err(|_| format!("unknown key `{other}` in keybinding `{s}`"))?;
                KeyCode::F(n)
            }
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
    let mut module: RuleModule = serde_yaml::from_str(&text)
        .with_context(|| format!("parsing module {}", path.display()))?;
    // Error messages should name the file the rule actually came from.
    if module.name.is_empty() {
        module.name = path.display().to_string();
    }
    Ok(module)
}

/// The ordered scope layers for a session, lowest precedence first
/// (docs/ARCHITECTURE.md §7.3): global defaults, then the shared modules
/// the profile lists in order, then the profile's own inline rules.
/// `channels` are the install-wide channel panes: their `match:` patterns
/// become the lowest-precedence layer (so a profile can disable one by id),
/// and every `route:` a rule names must resolve to one of them.
pub fn load_rules(
    dir: &Path,
    profile: Option<&str>,
    channels: &[Channel],
) -> Result<Vec<RuleModule>> {
    let mut layers = vec![channel_module(channels, profile)];

    let global = dir.join("global.yaml");
    if global.exists() {
        layers.push(load_module(&global)?);
    }

    if let Some(name) = profile {
        let path = profile_path(dir, name);
        let profile = load_profile(&path)?;

        for module in &profile.modules {
            let module_path = dir.join("modules").join(format!("{module}.yaml"));
            layers.push(
                load_module(&module_path)
                    .with_context(|| format!("module `{module}` listed in profile `{name}`"))?,
            );
        }

        layers.push(RuleModule {
            name: format!("profile `{name}`"),
            description: None,
            variables: profile.variables,
            aliases: profile.aliases,
            triggers: profile.triggers,
            timers: profile.timers,
        });
    }

    for layer in &mut layers {
        apply_channel_defaults(layer, channels)?;
    }
    Ok(layers)
}

/// Compiles the channels' `match:` sugar into ordinary route triggers
/// (docs/ARCHITECTURE.md §11.1). A channel pinned to another session
/// contributes nothing here, so its lines are never classified — and so
/// never gagged — in sessions it does not belong to.
fn channel_module(channels: &[Channel], profile: Option<&str>) -> RuleModule {
    let mut triggers = Vec::new();
    for channel in channels {
        if let Some(pinned) = &channel.session
            && Some(pinned.as_str()) != profile
        {
            continue;
        }
        for (index, pattern) in channel.matches.iter().enumerate() {
            triggers.push(Trigger {
                id: Some(format!("channel:{}:{index}", channel.name)),
                pattern: Some(pattern.clone()),
                route: Some(channel.name.clone()),
                ..Trigger::default()
            });
        }
    }
    RuleModule {
        name: "channels".to_string(),
        triggers,
        ..RuleModule::default()
    }
}

/// Fills in what a routed trigger inherits from its channel: move-vs-copy
/// is a property of the channel, so `keep_in_main: false` becomes the
/// trigger's `gag` unless the rule sets one itself.
fn apply_channel_defaults(layer: &mut RuleModule, channels: &[Channel]) -> Result<()> {
    for trigger in &mut layer.triggers {
        let Some(name) = trigger.route.clone() else {
            continue;
        };
        let channel = channels
            .iter()
            .find(|channel| channel.name == name)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "rule in `{}` routes to unknown channel `{name}`; declare it under `channels:` in mudular.yaml",
                    layer.name
                )
            })?;
        trigger.gag.get_or_insert(!channel.keep_in_main);
    }
    Ok(())
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
    fn parses_function_keys() {
        let binding: KeyBinding = "f2".parse().unwrap();
        assert!(binding.matches(KeyCode::F(2), KeyModifiers::NONE));

        let binding: KeyBinding = "F12".parse().unwrap();
        assert!(binding.matches(KeyCode::F(12), KeyModifiers::NONE));
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
        assert!(
            config
                .keybinds
                .gmcp_inspector
                .matches(KeyCode::F(2), KeyModifiers::NONE)
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

    /// The no-file path is the one that cannot be corrected by editing a
    /// file, so it must not be the one that ships history switched off
    /// (docs/ARCHITECTURE.md §11.3).
    #[test]
    fn history_size_defaults_with_and_without_a_config_file() {
        let dir = std::env::temp_dir().join(format!("mudular-cfg3-{}", std::process::id()));
        assert_eq!(load_app_config(&dir).unwrap().history_size, 500);

        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("mudular.yaml"), "keybinds:\n  quit: ctrl+q\n").unwrap();
        assert_eq!(load_app_config(&dir).unwrap().history_size, 500);

        std::fs::write(dir.join("mudular.yaml"), "history_size: 20\n").unwrap();
        assert_eq!(load_app_config(&dir).unwrap().history_size, 20);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A colour the terminal can't name is a typo, and a typo the loader
    /// swallowed is a setting the user believes they applied (§10).
    #[test]
    fn a_profile_color_parses_every_accepted_form_and_rejects_the_rest() {
        let base = "name: t\nhost: h\nport: 1\n";
        for (yaml, expected) in [
            ("color: cyan\n", Color::Cyan),
            ("color: light blue\n", Color::LightBlue),
            ("color: '#ff8800'\n", Color::Rgb(255, 136, 0)),
            ("color: '12'\n", Color::Indexed(12)),
        ] {
            let profile: Profile = serde_yaml::from_str(&format!("{base}{yaml}")).unwrap();
            assert_eq!(profile.color, Some(expected), "{yaml}");
        }

        let profile: Profile = serde_yaml::from_str(base).unwrap();
        assert_eq!(profile.color, None, "no color: is not an error");

        let err = serde_yaml::from_str::<Profile>(&format!("{base}color: puce\n"))
            .expect_err("an unknown color is rejected");
        assert!(err.to_string().contains("puce"), "{err}");
    }

    /// §13's rule made mechanical: there is no field to put a password in,
    /// so trying names the mistake at load instead of silently keeping a
    /// secret in a plaintext file.
    #[test]
    fn a_login_block_takes_a_name_but_refuses_a_password() {
        let base = "name: t\nhost: h\nport: 1\n";

        let profile: Profile =
            serde_yaml::from_str(&format!("{base}login:\n  name: Kestrel\n")).unwrap();
        let login = profile.login.expect("the block parsed");
        assert_eq!(login.name, "Kestrel");
        assert_eq!(login.name_prompt, None, "prompts are optional overrides");

        let err = serde_yaml::from_str::<Profile>(&format!(
            "{base}login:\n  name: Kestrel\n  password: hunter2\n"
        ))
        .expect_err("a password in YAML is rejected");
        assert!(err.to_string().contains("password"), "{err}");
    }

    #[test]
    fn profile_path_lives_under_profiles_subdir() {
        let path = profile_path(Path::new("/cfg"), "kestrel");
        assert_eq!(path, Path::new("/cfg/profiles/kestrel.yaml"));
    }

    /// M4's acceptance criterion (docs/ARCHITECTURE.md §14): one shared
    /// module, two profiles, each behaving per the scope rules. The engine
    /// tests cover merging in memory; this covers the part only real files
    /// exercise — global.yaml, `modules:` resolution, and the profile's
    /// own layer actually reaching the merge in the right order.
    #[test]
    fn two_profiles_share_a_module_and_layer_it_differently() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let dir = dir.path();
        std::fs::create_dir_all(dir.join("profiles")).unwrap();
        std::fs::create_dir_all(dir.join("modules")).unwrap();

        std::fs::write(
            dir.join("global.yaml"),
            r#"
name: global
variables:
  target: rat
aliases:
  - id: quicklook
    pattern: '^ll$'
    send: ["look"]
triggers:
  - id: greet
    pattern: '^(?P<who>\w+) has arrived\.$'
    send: ["say welcome ${who}"]
"#,
        )
        .unwrap();

        std::fs::write(
            dir.join("modules/combat.yaml"),
            r#"
name: combat
variables:
  target: kobold
aliases:
  - id: kill
    pattern: '^k$'
    send: ["kill ${target}"]
triggers:
  - id: autoloot
    pattern: 'is DEAD'
    send: ["get all corpse"]
"#,
        )
        .unwrap();

        // Takes the shared module as-is.
        std::fs::write(
            dir.join("profiles/tank.yaml"),
            "name: tank\nhost: h\nport: 1\nmodules: [combat]\n",
        )
        .unwrap();

        // Same module, but overrides the variable, disables one of its
        // rules, and patches a *global* rule without restating its pattern.
        std::fs::write(
            dir.join("profiles/cleric.yaml"),
            r#"
name: cleric
host: h
port: 1
modules: [combat]
variables:
  target: dragon
triggers:
  - id: autoloot
    enabled: false
  - id: greet
    send: ["say blessings ${who}"]
"#,
        )
        .unwrap();

        let engine = |profile: &str| {
            let layers = load_rules(dir, Some(profile), &[]).expect("rules load");
            crate::engine::Engine::compile(&layers).expect("rules compile")
        };

        let mut tank = engine("tank");
        let mut cleric = engine("cleric");

        // Module variable beats global; profile beats module.
        assert_eq!(tank.expand_input("k").sends, vec!["kill kobold"]);
        assert_eq!(cleric.expand_input("k").sends, vec!["kill dragon"]);

        // The profile disables an inherited module rule by id alone.
        assert_eq!(
            tank.process_line("The kobold is DEAD!").sends,
            vec!["get all corpse"]
        );
        assert!(cleric.process_line("The kobold is DEAD!").sends.is_empty());

        // ...and patches a global rule's command, inheriting its pattern.
        assert_eq!(
            tank.process_line("Bob has arrived.").sends,
            vec!["say welcome Bob"]
        );
        assert_eq!(
            cleric.process_line("Bob has arrived.").sends,
            vec!["say blessings Bob"]
        );

        // An untouched global rule survives in both.
        assert_eq!(tank.expand_input("ll").sends, vec!["look"]);
        assert_eq!(cleric.expand_input("ll").sends, vec!["look"]);
    }

    /// A profile naming a module that isn't there must say which module and
    /// which profile, not just "file not found".
    #[test]
    fn a_missing_module_names_the_profile_that_wanted_it() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let dir = dir.path();
        std::fs::create_dir_all(dir.join("profiles")).unwrap();
        std::fs::write(
            dir.join("profiles/tank.yaml"),
            "name: tank\nhost: h\nport: 1\nmodules: [nope]\n",
        )
        .unwrap();

        let err = load_rules(dir, Some("tank"), &[]).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("nope"), "{message}");
        assert!(message.contains("tank"), "{message}");
    }

    /// The shipped examples are documentation: if they stop loading, the
    /// docs are wrong. Loading them here keeps that from going unnoticed.
    #[test]
    fn shipped_example_config_loads_and_compiles() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/config");

        let app = load_app_config(&dir).expect("example mudular.yaml loads");
        assert!(
            app.keybinds
                .quit
                .matches(KeyCode::Char('q'), KeyModifiers::CONTROL)
        );

        let layers = load_rules(&dir, Some("kestrel"), &app.channels).expect("example rules load");
        let engine = crate::engine::Engine::compile(&layers).expect("example rules compile");

        // The profile disables the module's autoloot and overrides its
        // greeting — the layering the comments in those files describe.
        let mut engine = engine;
        assert!(
            engine.process_line("The kobold is DEAD!").sends.is_empty(),
            "profile disables autoloot"
        );
        assert_eq!(
            engine.process_line("Ærlend has arrived.").sends,
            vec!["say well met, Ærlend"]
        );
        // ...and overrides the module's `target` variable.
        assert_eq!(engine.expand_input("k").sends, vec!["kill dragon"]);

        // The example channel classifies tells into the comms pane.
        let outcome = engine.process_line("Bob tells you hello");
        assert_eq!(outcome.route.as_deref(), Some("comms"));

        // ...and the example's cross-session rule addresses the cleric.
        let outcome = engine.process_line("You are badly wounded and bleeding");
        assert_eq!(outcome.send_to.len(), 1, "{outcome:?}");
        assert_eq!(outcome.send_to[0].target, "cleric");
    }

    // ---- channel panes (docs/ARCHITECTURE.md §11.1) ----

    fn channel(name: &str, patterns: &[&str], keep_in_main: bool) -> Channel {
        Channel {
            name: name.to_string(),
            matches: patterns.iter().map(|s| s.to_string()).collect(),
            keep_in_main,
            timestamps: false,
            session: None,
        }
    }

    /// `match:` is sugar for route triggers, so classification gets the
    /// engine's full regex machinery — and `keep_in_main: false` (the
    /// default) becomes the gag that moves the line out of main.
    #[test]
    fn a_channel_match_compiles_to_a_gagging_route_trigger() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let channels = [channel("comms", &[r"^\w+ tells you"], false)];

        let layers = load_rules(dir.path(), None, &channels).unwrap();
        let mut engine = crate::engine::Engine::compile(&layers).unwrap();

        let outcome = engine.process_line("Bob tells you hi");
        assert_eq!(outcome.route.as_deref(), Some("comms"));
        assert!(outcome.gag, "the default is to move, not copy");
    }

    #[test]
    fn keep_in_main_copies_the_line_instead_of_moving_it() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let channels = [channel("comms", &[r"^\w+ tells you"], true)];

        let layers = load_rules(dir.path(), None, &channels).unwrap();
        let mut engine = crate::engine::Engine::compile(&layers).unwrap();

        let outcome = engine.process_line("Bob tells you hi");
        assert_eq!(outcome.route.as_deref(), Some("comms"));
        assert!(!outcome.gag);
    }

    /// A trigger that routes explicitly inherits the channel's move-vs-copy
    /// setting: that is a property of the channel, not of each rule.
    #[test]
    fn an_explicit_route_inherits_the_channels_move_setting() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        std::fs::write(
            dir.path().join("global.yaml"),
            "name: global\ntriggers:\n  - pattern: 'the guild announces'\n    route: comms\n",
        )
        .unwrap();

        let channels = [channel("comms", &[], false)];
        let layers = load_rules(dir.path(), None, &channels).unwrap();
        let mut engine = crate::engine::Engine::compile(&layers).unwrap();

        let outcome = engine.process_line("the guild announces a raid");
        assert_eq!(outcome.route.as_deref(), Some("comms"));
        assert!(outcome.gag, "keep_in_main: false applies to any route");
    }

    /// A channel pinned to one character must not classify — or gag —
    /// anything in the other sessions.
    #[test]
    fn a_pinned_channel_only_compiles_into_its_own_session() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        std::fs::create_dir_all(dir.path().join("profiles")).unwrap();
        for name in ["tank", "cleric"] {
            std::fs::write(
                dir.path().join(format!("profiles/{name}.yaml")),
                format!("name: {name}\nhost: h\nport: 1\n"),
            )
            .unwrap();
        }

        let mut channels = [channel("tankchat", &["^chat:"], false)];
        channels[0].session = Some("tank".to_string());

        let engine = |profile: &str| {
            let layers = load_rules(dir.path(), Some(profile), &channels).unwrap();
            crate::engine::Engine::compile(&layers).unwrap()
        };

        assert_eq!(
            engine("tank").process_line("chat: hello").route.as_deref(),
            Some("tankchat")
        );
        let outcome = engine("cleric").process_line("chat: hello");
        assert_eq!(outcome.route, None);
        assert!(!outcome.gag, "a pinned channel must not gag other sessions");
    }

    /// A typo in `route:` would otherwise send lines to a pane that is never
    /// drawn — they must fail loudly, like any other config typo.
    #[test]
    fn a_route_to_an_undeclared_channel_is_rejected() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        std::fs::write(
            dir.path().join("global.yaml"),
            "name: global\ntriggers:\n  - pattern: 'x'\n    route: coms\n",
        )
        .unwrap();

        let err = load_rules(dir.path(), None, &[channel("comms", &[], false)]).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("coms"), "{message}");
        assert!(message.contains("unknown channel"), "{message}");
    }

    #[test]
    fn parses_a_channel_declaration() {
        let config: AppConfig = serde_yaml::from_str(
            r#"
            channels:
              - name: comms
                match: ['^\[gossip\]']
                keep_in_main: true
                timestamps: true
                session: tank
            "#,
        )
        .unwrap();
        let channel = &config.channels[0];
        assert_eq!(channel.name, "comms");
        assert_eq!(channel.matches, vec![r"^\[gossip\]"]);
        assert!(channel.keep_in_main);
        assert!(channel.timestamps);
        assert_eq!(channel.session.as_deref(), Some("tank"));
    }

    // ---- cross-session settings (docs/ARCHITECTURE.md §7.5) ----

    #[test]
    fn cross_session_defaults_to_verbatim_injection_and_one_hop() {
        let config = AppConfig::default();
        assert!(!config.cross_session.expand_aliases);
        assert_eq!(config.cross_session.max_hops, 1);
    }

    /// The profile is the receiver, so its block wins — field by field, so
    /// setting one does not silently reset the other.
    #[test]
    fn a_profile_overrides_only_the_cross_session_fields_it_names() {
        let install = CrossSession {
            expand_aliases: false,
            max_hops: 3,
        };
        let profile: Profile = serde_yaml::from_str(
            "name: cleric\nhost: h\nport: 1\ncross_session:\n  expand_aliases: true\n",
        )
        .unwrap();

        let merged = install.with_override(profile.cross_session);
        assert!(merged.expand_aliases);
        assert_eq!(merged.max_hops, 3, "unnamed fields keep the install value");
    }
}
