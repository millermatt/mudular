//! YAML configuration and character profiles.
//!
//! Layout under the config dir (docs/ARCHITECTURE.md §10):
//! `mudular.yaml` (app settings), `global.yaml` (global rules),
//! `modules/*.yaml` (shared rule modules), `profiles/*.yaml` (characters).
//! The scope merge (global → modules → profile) lands in M4; schemas use
//! `deny_unknown_fields` so typos fail loudly with file context.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::style::Color;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use crate::engine::script::ScriptSource;
use crate::engine::{Alias, Engine, RuleModule, Timer, Trigger};
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

/// Where session transcripts are written when a profile sets `log: true`
/// (§8, §12) — a subdirectory of the config dir, so `--config-dir` moves
/// logs along with everything else.
pub fn log_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("logs")
}

/// Where a profile's room graph is saved (§16) — a subdirectory of the
/// config dir, the same layout `log_dir` uses.
pub fn map_path(dir: &Path, profile: &str) -> PathBuf {
    dir.join("maps").join(format!("{profile}.json"))
}

/// Loads a profile's saved map, or an empty one if there is nothing to
/// load. Everything short of a valid file reads as "start empty" rather
/// than an error: a map is exploration a player can always redo, so a
/// missing, unreadable, or corrupt file must never be the reason a session
/// can't be played (docs/ARCHITECTURE.md §16).
pub fn load_map(dir: &Path, profile: &str) -> crate::map::Map {
    let path = map_path(dir, profile);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return crate::map::Map::default();
        }
        Err(err) => {
            tracing::warn!("could not read map {}: {err}", path.display());
            return crate::map::Map::default();
        }
    };
    match serde_json::from_str(&text) {
        Ok(map) => map,
        Err(err) => {
            tracing::warn!(
                "could not parse map {}: {err}; starting with an empty map",
                path.display()
            );
            crate::map::Map::default()
        }
    }
}

/// Saves a profile's map, merged with whatever is already on disk rather
/// than overwritten — two sessions open on the same profile explore
/// independently, and a plain overwrite would let whichever saves last
/// silently discard the other's half of the map. JSON, not YAML like the
/// rest of this directory: unlike everything else in the config dir, this
/// file is only ever machine-written and machine-read, never hand-edited.
pub fn save_map(dir: &Path, profile: &str, map: &crate::map::Map) -> Result<()> {
    let path = map_path(dir, profile);
    // Deliberately *not* `load_map`: that one answers "what can this
    // session start with", where anything unreadable is fairly an empty
    // map, because a corrupt file must never stop someone playing. This
    // answers a different question — "what am I about to merge into and
    // overwrite" — and there the same leniency is data loss. A transient
    // read error or one bad parse would turn "I could not read your map"
    // into "your map is now the few rooms this session has seen", and
    // saving every 30s makes that window continuous rather than rare.
    // Deliberately *not* `load_map`: that one answers "what can this
    // session start with", where anything unreadable is fairly an empty
    // map, because a corrupt file must never stop someone playing. This
    // answers a different question — "what am I about to merge into and
    // overwrite" — and there the same leniency is data loss. A transient
    // read error or one bad parse would turn "I could not read your map"
    // into "your map is now the few rooms this session has seen", and
    // saving every 30s makes that window continuous rather than rare.
    let mut on_disk = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("parsing the map already at {}", path.display()))?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => crate::map::Map::default(),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("reading the map already at {}", path.display()));
        }
    };
    on_disk.merge(map.clone());
    let json = serde_json::to_string_pretty(&on_disk).context("serializing the map")?;
    atomic_write(&path, json.as_bytes()).context("writing the map")?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
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
    #[serde(
        default,
        deserialize_with = "parse_color",
        serialize_with = "write_color",
        skip_serializing_if = "Option::is_none"
    )]
    pub color: Option<Color>,
    /// Answers the server's opening name/password prompts (§10). There is
    /// deliberately no `password:` field: `deny_unknown_fields` turns an
    /// attempt to put one here into a load error naming it, which is a
    /// better answer than quietly accepting a secret into a plaintext file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login: Option<Login>,
    /// Shared rule modules, applied in order (scope layer 2).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modules: Vec<String>,
    /// Profile-local overrides (scope layer 3).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub variables: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<Alias>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub triggers: Vec<Trigger>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub timers: Vec<Timer>,
    /// Profile-local scripts, loaded from beside the profile file (§7.4).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scripts: Vec<String>,
    /// Overrides the install-wide `cross_session` block for this character
    /// (docs/ARCHITECTURE.md §7.5). Receiver-side by design: only the
    /// profile whose aliases would run can opt into running them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cross_session: Option<CrossSessionOverride>,
    /// Appends this character's scrollback to `<config dir>/logs/<name>.log`
    /// (§8, §12). Off by default: a transcript is something a player opts
    /// into per character, not a standing side effect of connecting.
    #[serde(default, skip_serializing_if = "is_false")]
    pub log: bool,
}

fn is_false(b: &bool) -> bool {
    !b
}

/// Auto-login settings. The password lives in the OS keyring, not here
/// (docs/ARCHITECTURE.md §13); store it with `mudular --set-password`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Login {
    /// The character name to send at the name prompt.
    pub name: String,
    /// Overrides for MUDs whose prompts the defaults don't recognise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
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

/// Deletes a profile's stored password. `Ok(false)` when there was none:
/// the end state the caller asked for already holds.
pub fn forget_password(profile: &str) -> Result<bool> {
    match keyring::Entry::new(KEYRING_SERVICE, profile)?.delete_credential() {
        Ok(()) => Ok(true),
        Err(keyring::Error::NoEntry) => Ok(false),
        Err(err) => {
            Err(anyhow::Error::new(err)
                .context(format!("deleting the keyring password for {profile}")))
        }
    }
}

/// Profiles whose player turned down the offer to save a typed password,
/// one name per line. Only refusals need recording: a "yes" is remembered
/// by the keyring entry it creates.
const DECLINED_FILE: &str = "keyring_declined";

/// Whether this profile's player already said no to saving a password. An
/// unreadable file reads as "not asked" — the cost is one more question,
/// and refusing to ask because a file is missing is the worse failure.
pub fn password_save_declined(dir: &Path, profile: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(dir.join(DECLINED_FILE)) else {
        return false;
    };
    text.lines().any(|line| line.trim() == profile)
}

/// Records a refusal, so the offer is made once per profile and not once
/// per login.
pub fn decline_password_save(dir: &Path, profile: &str) -> Result<()> {
    if password_save_declined(dir, profile) {
        return Ok(());
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(DECLINED_FILE);
    let mut text = std::fs::read_to_string(&path).unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(profile);
    text.push('\n');
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CrossSessionOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expand_aliases: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
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

/// The inverse of `parse_color`: writes back through `Color`'s `Display`
/// impl (`Cyan`, `#ff0000`, `12`), which `Color::from_str` accepts, so a
/// saved profile loads back to the same colour. Not derived through
/// ratatui's own `serde` feature — pulling in a whole extra feature for one
/// field's write side isn't worth it next to twenty lines here.
fn write_color<S>(color: &Option<Color>, serializer: S) -> std::result::Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match color {
        Some(color) => serializer.serialize_str(&color.to_string()),
        None => serializer.serialize_none(),
    }
}

fn default_charset() -> String {
    "utf-8".to_string()
}

pub fn load_profile(path: &Path) -> Result<Profile> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading profile {}", path.display()))?;
    serde_yaml::from_str(&text).with_context(|| format!("parsing profile {}", path.display()))
}

/// App-wide settings (`mudular.yaml`): keybinds, history, and scrollback
/// size. Absent entirely is fine — a fresh install has sensible defaults
/// and no config dir yet (§15).
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
    /// Lines kept per pane's scrollback buffer (docs/ARCHITECTURE.md §8).
    #[serde(default = "default_scrollback_size")]
    pub scrollback_size: usize,
    /// Starting width of the docked channel column (docs/ARCHITECTURE.md
    /// §11.4). The keybinds move it from here; nothing writes it back.
    #[serde(default = "default_channel_width")]
    pub channel_width: u16,
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
            scrollback_size: default_scrollback_size(),
            channel_width: default_channel_width(),
        }
    }
}

fn default_history_size() -> usize {
    500
}

fn default_scrollback_size() -> usize {
    10_000
}

fn default_channel_width() -> u16 {
    crate::ui::CHANNEL_WIDTH
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Keybinds {
    #[serde(default = "default_quit")]
    pub quit: KeyBinding,
    /// Toggles the raw server-data inspector view — GMCP and/or MSDP,
    /// whichever the server actually sent (docs/ARCHITECTURE.md §6.3, §14
    /// M6).
    #[serde(default = "default_server_data_inspector")]
    pub server_data_inspector: KeyBinding,
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
    /// Widens the channel column (§11.4).
    #[serde(default = "default_channel_wider")]
    pub channel_wider: KeyBinding,
    /// Narrows the channel column (§11.4).
    #[serde(default = "default_channel_narrower")]
    pub channel_narrower: KeyBinding,
    /// Opens the help overlay listing every binding (§11.2).
    #[serde(default = "default_help")]
    pub help: KeyBinding,
    /// Opens the in-client profile editor (§10.2).
    #[serde(default = "default_config_editor")]
    pub config_editor: KeyBinding,
    /// Enters the scrollback line-cursor, to turn a line into a new
    /// trigger's starting pattern (§10.2, §11.5).
    #[serde(default = "default_line_picker")]
    pub line_picker: KeyBinding,
    /// Recompiles rules and scripts from disk — the same thing typing
    /// `/reload` does. Every other frequent action has a default binding;
    /// this one didn't (UX_REVIEW.md F).
    #[serde(default = "default_reload")]
    pub reload: KeyBinding,
    /// Shows or hides the docked map column — the same thing typing `/map`
    /// does (§16).
    #[serde(default = "default_toggle_map")]
    pub toggle_map: KeyBinding,
    /// Widens the map column (§11.4, §16).
    #[serde(default = "default_map_wider")]
    pub map_wider: KeyBinding,
    /// Narrows the map column (§11.4, §16).
    #[serde(default = "default_map_narrower")]
    pub map_narrower: KeyBinding,
}

impl Default for Keybinds {
    fn default() -> Self {
        Self {
            quit: default_quit(),
            server_data_inspector: default_server_data_inspector(),
            focus_next: default_focus_next(),
            cycle_layout: default_cycle_layout(),
            toggle_channels: default_toggle_channels(),
            channel_wider: default_channel_wider(),
            channel_narrower: default_channel_narrower(),
            help: default_help(),
            config_editor: default_config_editor(),
            line_picker: default_line_picker(),
            reload: default_reload(),
            toggle_map: default_toggle_map(),
            map_wider: default_map_wider(),
            map_narrower: default_map_narrower(),
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

/// `Alt+-`/`Alt+=` — the channel column is the rightmost pane, so widening
/// it moves its left edge left and narrowing moves it right; the keys sit
/// left-to-right on the keyboard the same way, `-` widening and `=`
/// narrowing. Alt keeps them clear of the F-key toggles and of Ctrl+arrow,
/// which the binding parser does not name. Not `Alt+[`/`Alt+]`: those
/// bytes (`ESC` `[`) are the ANSI CSI prefix, the same ones many terminals
/// send for arrow keys and friends, so terminals resolve the ambiguity
/// inconsistently — some report it as Alt+`[`, others hand back a bare
/// `[` once the escape times out with nothing CSI-shaped following it.
fn default_channel_wider() -> KeyBinding {
    "alt+-".parse().expect("built-in default keybinding")
}

fn default_channel_narrower() -> KeyBinding {
    "alt+=".parse().expect("built-in default keybinding")
}

fn default_help() -> KeyBinding {
    // The one key a user tries unprompted, and clear of the F2-F4 toggles.
    "f1".parse().expect("built-in default keybinding")
}

fn default_quit() -> KeyBinding {
    "ctrl+c".parse().expect("built-in default keybinding")
}

fn default_server_data_inspector() -> KeyBinding {
    "f2".parse().expect("built-in default keybinding")
}

fn default_config_editor() -> KeyBinding {
    // Next free key in the F1 help / F2 GMCP / F3 layout / F4 channels row.
    "f5".parse().expect("built-in default keybinding")
}

fn default_line_picker() -> KeyBinding {
    // Alt keeps it clear of ordinary typing — a bare `v` would swallow the
    // letter itself out of every command line.
    "alt+v".parse().expect("built-in default keybinding")
}

fn default_reload() -> KeyBinding {
    // Next free key in the F1 help / F2 GMCP / F3 layout / F4 channels /
    // F5 config editor row.
    "f6".parse().expect("built-in default keybinding")
}

fn default_toggle_map() -> KeyBinding {
    // Next free after F6 reload, continuing the same row.
    "f7".parse().expect("built-in default keybinding")
}

// A second adjacent pair, beside the comms column's `alt+-` / `alt+=`:
// two columns need two pairs, and reusing one pair for both would depend
// on a focus the map column does not have. Same orientation as that pair,
// which is what the docked side makes natural — the left key widens,
// because widening a right-docked column moves its edge left.
//
// Not the brackets, whatever their mnemonic appeal: `alt+[` puts `ESC [`
// on the wire, which *is* the CSI introducer, so a terminal cannot tell it
// from the start of an escape sequence and the binding never arrives.
fn default_map_wider() -> KeyBinding {
    "alt+,".parse().expect("built-in default keybinding")
}

fn default_map_narrower() -> KeyBinding {
    "alt+.".parse().expect("built-in default keybinding")
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

/// Whether any profile is already configured — the trigger for the
/// first-run wizard (docs/ARCHITECTURE.md §15): a missing `profiles/` dir
/// reads the same as an empty one, since neither has anything to connect
/// with.
pub fn has_profiles(dir: &Path) -> bool {
    std::fs::read_dir(dir.join("profiles"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .any(|entry| entry.path().extension().is_some_and(|ext| ext == "yaml"))
}

/// What the in-TUI new-profile wizard collects (§15) — just enough to
/// connect; everything else a profile can do (`login:`, `modules:`,
/// `color:`, …) is left to hand-editing the file afterward.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewProfile {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub tls: bool,
}

/// Writes a wizard-built profile to disk, creating `profiles/` if this is
/// the first one. Builds a real `Profile` with everything past
/// name/host/port/TLS left at its schema default, and serializes that
/// (rather than a hand-formatted string) so a host or name with
/// YAML-special characters comes out correctly quoted. Goes through
/// `atomic_write` like every other profile write, though there is nothing
/// to conflict with yet — this is always a brand-new file.
pub fn save_new_profile(dir: &Path, profile: &NewProfile) -> Result<()> {
    let full = Profile {
        name: profile.name.clone(),
        host: profile.host.clone(),
        port: profile.port,
        tls: TlsSettings {
            enabled: profile.tls,
            verify: VerifyMode::default(),
        },
        charset: default_charset(),
        color: None,
        login: None,
        modules: Vec::new(),
        variables: BTreeMap::new(),
        aliases: Vec::new(),
        triggers: Vec::new(),
        timers: Vec::new(),
        scripts: Vec::new(),
        cross_session: None,
        log: false,
    };
    let path = profile_path(dir, &profile.name);
    let yaml = serde_yaml::to_string(&full).context("serializing the new profile")?;
    atomic_write(&path, yaml.as_bytes())?;
    Ok(())
}

/// A loaded profile plus the digest of the exact bytes it came from, so a
/// later `save_profile` can tell whether the file changed on disk in the
/// meantime (docs/ARCHITECTURE.md §10.2) — an in-client editor is open for
/// as long as a player is looking at it, which is long enough for a
/// hand-edit or another mudular process to land underneath it.
pub struct ProfileFile {
    pub path: PathBuf,
    pub profile: Profile,
    digest: [u8; 32],
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).into()
}

/// Reads and parses a profile, keeping the digest `save_profile` needs.
/// `load_profile` (the connect-path loader) stays separate: it doesn't need
/// the bookkeeping, and reusing it here would mean reading the file twice.
pub fn load_profile_file(path: &Path) -> Result<ProfileFile> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading profile {}", path.display()))?;
    let profile: Profile = serde_yaml::from_str(&text)
        .with_context(|| format!("parsing profile {}", path.display()))?;
    Ok(ProfileFile {
        path: path.to_path_buf(),
        profile,
        digest: sha256(text.as_bytes()),
    })
}

#[derive(Debug, Error)]
pub enum SaveError {
    #[error("{} changed on disk since it was opened here", .path.display())]
    Conflict { path: PathBuf },
    #[error("{} no longer exists", .path.display())]
    Vanished { path: PathBuf },
    #[error("serializing the profile: {0}")]
    Serialize(#[source] serde_yaml::Error),
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
}

fn io_error(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> SaveError {
    let context = context.into();
    move |source| SaveError::Io { context, source }
}

/// Whether a save must first check the file hasn't moved since it was
/// loaded (`Guarded`, the normal path) or has already been confirmed by the
/// player and should proceed regardless (`Overwrite`, after a `Conflict`/
/// `Vanished` prompt).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveMode {
    Guarded,
    Overwrite,
}

#[derive(Debug)]
pub struct Saved {
    /// Where the pre-save version of the file was copied, if there was one
    /// to copy (a brand-new profile has nothing to back up).
    pub backup: Option<PathBuf>,
}

/// The newest backups kept per profile before older ones are pruned —
/// enough to recover from "I broke it earlier this session" without
/// backups accumulating forever.
const PROFILE_BACKUPS_KEPT: usize = 20;

/// Saves an edited profile back to `file.path`, safely (docs/ARCHITECTURE.md
/// §10.2): the previous version is backed up before anything is
/// overwritten, the write itself is atomic (temp file + rename, so a crash
/// mid-write never leaves a truncated file), and — under `SaveMode::Guarded`
/// — a file that changed since it was loaded is reported rather than
/// clobbered. `file` is updated in place on success so a second save in the
/// same editor session compares against what was *just* written, not the
/// original load.
pub fn save_profile(
    file: &mut ProfileFile,
    profile: &Profile,
    mode: SaveMode,
) -> std::result::Result<Saved, SaveError> {
    let on_disk = std::fs::read(&file.path);
    let previous_bytes = match (&on_disk, mode) {
        (Ok(bytes), SaveMode::Guarded) => {
            if sha256(bytes) != file.digest {
                return Err(SaveError::Conflict {
                    path: file.path.clone(),
                });
            }
            Some(bytes.clone())
        }
        (Ok(bytes), SaveMode::Overwrite) => Some(bytes.clone()),
        (Err(err), SaveMode::Guarded) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(SaveError::Vanished {
                path: file.path.clone(),
            });
        }
        (Err(_), SaveMode::Overwrite) => None,
        (Err(err), SaveMode::Guarded) => {
            return Err(io_error(format!("reading {}", file.path.display()))(
                std::io::Error::new(err.kind(), err.to_string()),
            ));
        }
    };

    let yaml = serde_yaml::to_string(profile).map_err(SaveError::Serialize)?;

    // Back up before touching the target, so the window in which neither
    // the backup nor the new file exists is zero.
    let backup = previous_bytes
        .map(|bytes| write_backup(&file.path, &bytes))
        .transpose()?;

    atomic_write(&file.path, yaml.as_bytes())?;

    if let Err(err) = prune_backups(&file.path) {
        tracing::warn!(
            "could not prune old backups for {}: {err:#}",
            file.path.display()
        );
    }

    file.digest = sha256(yaml.as_bytes());
    file.profile = profile.clone();

    Ok(Saved { backup })
}

/// Writes `bytes` to `path` via a same-directory temp file, `fsync`, then
/// `rename` — the rename is what makes the write atomic (a reader never
/// observes a partial file), and same-directory is what makes the rename
/// itself atomic (rename is only guaranteed atomic within one filesystem).
/// The parent directory is `fsync`'d too on Unix, since that is the step
/// that makes the rename durable across a crash, not just the bytes.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> std::result::Result<(), SaveError> {
    use std::io::Write;

    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).map_err(io_error(format!("creating {}", dir.display())))?;

    let tmp_name = format!(
        ".{}.tmp{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("profile"),
        std::process::id()
    );
    let tmp = dir.join(tmp_name);

    let write_result = (|| -> std::io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        // A profile can carry account names and other details the player
        // treats as private, even with no `password:` field — owner-only
        // rather than the process umask. `rename` below preserves this.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut f = options.open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()
    })();
    if let Err(source) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(SaveError::Io {
            context: format!("writing {}", tmp.display()),
            source,
        });
    }

    if let Err(source) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(SaveError::Io {
            context: format!("renaming {} to {}", tmp.display(), path.display()),
            source,
        });
    }

    #[cfg(unix)]
    if let Ok(dir_file) = std::fs::File::open(dir) {
        let _ = dir_file.sync_all();
    }

    Ok(())
}

/// `<config dir>/backups/profiles/<name>/` — a top-level `backups/` dir
/// rather than nesting inside `profiles/` (§10 documents `profiles/*.yaml`
/// as one file per character; a subdirectory living inside that namespace
/// is a foreign object anything that globs it would have to learn to
/// skip). Returns `None` for a path that doesn't look like
/// `<dir>/profiles/<name>.yaml` — nothing calls this any other way, but a
/// `None` is a safer failure than guessing a location.
fn backup_dir_for(profile_path: &Path) -> Option<PathBuf> {
    let profiles_dir = profile_path.parent()?;
    let config_dir = profiles_dir.parent()?;
    let name = profile_path.file_stem()?.to_str()?;
    Some(config_dir.join("backups").join("profiles").join(name))
}

fn write_backup(profile_path: &Path, bytes: &[u8]) -> std::result::Result<PathBuf, SaveError> {
    let dir = backup_dir_for(profile_path)
        .unwrap_or_else(|| profile_path.with_extension("backups").join("profiles"));
    std::fs::create_dir_all(&dir).map_err(io_error(format!("creating {}", dir.display())))?;

    let stamp = utc_stamp(std::time::SystemTime::now());
    let mut path = dir.join(format!("{stamp}.yaml"));
    let mut suffix = 2;
    while path.exists() {
        path = dir.join(format!("{stamp}-{suffix}.yaml"));
        suffix += 1;
    }

    std::fs::write(&path, bytes).map_err(io_error(format!("writing {}", path.display())))?;
    Ok(path)
}

/// Deletes all but the newest [`PROFILE_BACKUPS_KEPT`] backups. Best-effort
/// by design: a prune failure is logged and never fails a save that has
/// already succeeded (`save_profile` above).
fn prune_backups(profile_path: &Path) -> Result<()> {
    let Some(dir) = backup_dir_for(profile_path) else {
        return Ok(());
    };
    let mut backups: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "yaml"))
            .collect(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("reading {}", dir.display())),
    };
    // Fixed-width UTC stamps sort lexicographically in chronological order.
    backups.sort();
    if backups.len() > PROFILE_BACKUPS_KEPT {
        for old in &backups[..backups.len() - PROFILE_BACKUPS_KEPT] {
            std::fs::remove_file(old).with_context(|| format!("removing {}", old.display()))?;
        }
    }
    Ok(())
}

/// A dependency-free `YYYYMMDDTHHMMSSZ` UTC timestamp. Fixed-width so
/// lexicographic order equals chronological order, which backup pruning
/// and "which is newest" both depend on — worth the ~20 lines below over
/// pulling in a date/time crate for one function.
fn utc_stamp(time: std::time::SystemTime) -> String {
    let secs = time
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = (secs / 86_400) as i64;
    let time_of_day = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = time_of_day / 3600;
    let minute = (time_of_day % 3600) / 60;
    let second = time_of_day % 60;
    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z")
}

/// Howard Hinnant's `civil_from_days`: days since the Unix epoch to a
/// proleptic-Gregorian (year, month, day), valid for any date this client
/// will ever save a backup on.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if month <= 2 { y + 1 } else { y };
    (year, month, day)
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
    module.script_sources = load_scripts(path, &module.scripts)?;
    Ok(module)
}

/// Reads the scripts a module declares. They live beside the file that
/// declares them (§7.4), so a shared module is one directory to copy —
/// and a bare name can never reach outside it.
fn load_scripts(module_path: &Path, names: &[String]) -> Result<Vec<ScriptSource>> {
    let dir = module_path.parent().unwrap_or(Path::new("."));
    names
        .iter()
        .map(|name| {
            if Path::new(name).components().count() != 1 {
                bail!(
                    "script `{name}` in {}: expected a file name beside the module, not a path",
                    module_path.display()
                );
            }
            let path = dir.join(name);
            Ok(ScriptSource {
                name: name.clone(),
                code: std::fs::read_to_string(&path)
                    .with_context(|| format!("reading script {}", path.display()))?,
            })
        })
        .collect()
}

/// Resolves a profile's `modules:` entry to the file it names under
/// `dir/modules`, refusing anything that isn't a bare file-stem (`..`, an
/// absolute path, or a path separator) — the same bare-name rule
/// `load_scripts` applies to scripts, so a hostile profile can't reach a
/// module path outside the profile directory.
fn module_path(dir: &Path, module: &str, profile_name: &str) -> Result<PathBuf> {
    if Path::new(module).components().count() != 1 {
        bail!(
            "module `{module}` listed in profile `{profile_name}`: expected a file name, not a path"
        );
    }
    Ok(dir.join("modules").join(format!("{module}.yaml")))
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
            let path = module_path(dir, module, name)?;
            layers.push(
                load_module(&path)
                    .with_context(|| format!("module `{module}` listed in profile `{name}`"))?,
            );
        }

        layers.push(profile_layer(name, &path, &profile)?);
    }

    for layer in &mut layers {
        apply_channel_defaults(layer, channels)?;
    }
    Ok(layers)
}

/// Turns a profile's own inline rules into the scope layer §7.3 calls
/// "profile overrides" — shared by the connect-path loader above and
/// `validate_profile_rules`, which runs the same construction over an
/// in-editor draft that may not (yet) be the file on disk.
fn profile_layer(name: &str, path: &Path, profile: &Profile) -> Result<RuleModule> {
    Ok(RuleModule {
        // Plain text, not pre-wrapped in backticks: every `engine::mod.rs`
        // error template that reports `{module}` wraps it in backticks
        // itself, the same as a loaded module's plain filename — a label
        // that quotes itself here doubles up into `` `profile `tank`` ``
        // (docs/UX_REVIEW.md, Adversarial findings, Low #5).
        name: format!("profile {name}"),
        description: None,
        variables: profile.variables.clone(),
        aliases: profile.aliases.clone(),
        triggers: profile.triggers.clone(),
        timers: profile.timers.clone(),
        script_sources: load_scripts(path, &profile.scripts)?,
        scripts: profile.scripts.clone(),
    })
}

/// Checks that a profile's aliases/triggers/timers would compile — the
/// same construction `load_rules` uses (global layer, then the profile's
/// declared modules, then its own rules), run against `draft` rather than
/// whatever is on disk. Used by the in-client profile editor (§10.2) before
/// every save, so "valid" can never drift from what the session itself
/// would load: a second, editor-only validator would be wrong in exactly
/// the cases that matter.
pub fn validate_profile_rules(
    dir: &Path,
    name: &str,
    draft: &Profile,
    channels: &[Channel],
) -> Result<()> {
    let mut layers = vec![channel_module(channels, Some(name))];

    let global = dir.join("global.yaml");
    if global.exists() {
        layers.push(load_module(&global)?);
    }

    for module in &draft.modules {
        let path = module_path(dir, module, name)?;
        layers.push(
            load_module(&path)
                .with_context(|| format!("module `{module}` listed in profile `{name}`"))?,
        );
    }

    check_trigger_hygiene(&draft.triggers)?;

    layers.push(profile_layer(name, &profile_path(dir, name), draft)?);

    for layer in &mut layers {
        apply_channel_defaults(layer, channels)?;
    }
    Engine::compile(&layers)?;
    Ok(())
}

/// Two accident classes `Engine::compile` above doesn't catch, because both
/// are legitimate *across* scope layers (a module and a profile shadowing
/// the same pattern, say) and wrong only within one profile's own list
/// (docs/UX_REVIEW.md, Adversarial findings, Medium #3) — so this checks
/// `draft.triggers` alone, not the merged layers `Engine::compile` sees.
///
/// - **Two triggers sharing the exact same pattern.** Both fire on every
///   matching line — triggers aren't first-match-wins the way aliases are
///   (§7.1) — so this is redundant matching at best and, in the case that
///   prompted this check, one of the two silently doing nothing at worst.
/// - **A trigger with no action at all.** Matches, and does nothing —
///   easy to create by accident (an empty `send:` left behind while
///   editing) and confusing to debug later, since nothing in the editor
///   flags it as inert.
fn check_trigger_hygiene(triggers: &[Trigger]) -> Result<()> {
    let mut seen_patterns = std::collections::HashSet::new();
    for trigger in triggers {
        if let Some(pattern) = &trigger.pattern
            && !seen_patterns.insert(pattern.as_str())
        {
            bail!("two triggers share the pattern `{pattern}`");
        }
        let no_action = trigger.send.is_none()
            && trigger.send_to.is_none()
            && trigger.set.is_none()
            && trigger.script.is_none()
            && trigger.gag != Some(true)
            && trigger.route.is_none()
            && trigger.highlight.is_none()
            && trigger.bell != Some(true);
        if no_action {
            let label = trigger
                .id
                .as_deref()
                .or(trigger.pattern.as_deref())
                .unwrap_or("(unnamed)");
            bail!(
                "trigger `{label}` matches but has no action — nothing to \
                 send, gag, route, or set"
            );
        }
    }
    Ok(())
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
                .server_data_inspector
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

    /// Mirrors `history_size_defaults_with_and_without_a_config_file`: the
    /// no-file path must not ship scrollback switched off either.
    #[test]
    fn scrollback_size_defaults_with_and_without_a_config_file() {
        let dir = std::env::temp_dir().join(format!("mudular-cfg4-{}", std::process::id()));
        assert_eq!(load_app_config(&dir).unwrap().scrollback_size, 10_000);

        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("mudular.yaml"), "keybinds:\n  quit: ctrl+q\n").unwrap();
        assert_eq!(load_app_config(&dir).unwrap().scrollback_size, 10_000);

        std::fs::write(dir.join("mudular.yaml"), "scrollback_size: 20\n").unwrap();
        assert_eq!(load_app_config(&dir).unwrap().scrollback_size, 20);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Mirrors the two above for the channel column's starting width: a
    /// config-less install gets the built-in 28, not a zero-width column
    /// (docs/ARCHITECTURE.md §11.4).
    #[test]
    fn channel_width_defaults_with_and_without_a_config_file() {
        let dir = std::env::temp_dir().join(format!("mudular-cfg5-{}", std::process::id()));
        assert_eq!(load_app_config(&dir).unwrap().channel_width, 28);

        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("mudular.yaml"), "keybinds:\n  quit: ctrl+q\n").unwrap();
        assert_eq!(load_app_config(&dir).unwrap().channel_width, 28);

        std::fs::write(dir.join("mudular.yaml"), "channel_width: 40\n").unwrap();
        assert_eq!(load_app_config(&dir).unwrap().channel_width, 40);

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

    // ---- map persistence (§16) ----

    #[test]
    fn map_path_lives_under_maps_subdir() {
        let path = map_path(Path::new("/cfg"), "kestrel");
        assert_eq!(path, Path::new("/cfg/maps/kestrel.json"));
    }

    #[test]
    fn a_missing_map_file_loads_as_empty() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        assert_eq!(load_map(dir.path(), "kestrel"), crate::map::Map::default());
    }

    /// Garbage on disk must lose exploration, not the session — the same
    /// reasoning `load_app_config`'s defaults-on-absence gets, but for a
    /// file that can also be *present and broken*, not just missing.
    #[test]
    fn a_corrupt_map_file_loads_as_empty_rather_than_panicking() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let path = map_path(dir.path(), "kestrel");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ not json at all").unwrap();

        assert_eq!(load_map(dir.path(), "kestrel"), crate::map::Map::default());
    }

    fn sample_room(id: i64, name: &str) -> crate::map::RoomInfo {
        crate::map::RoomInfo {
            id: crate::map::RoomId(id),
            name: Some(name.to_string()),
            area: Some("Test".to_string()),
            exits: BTreeMap::new(),
        }
    }

    #[test]
    fn a_saved_map_round_trips_through_load_map() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let mut map = crate::map::Map::default();
        map.observe(&sample_room(1, "Temple Square"));
        map.connect(crate::map::RoomId(1), "n", crate::map::RoomId(2));

        save_map(dir.path(), "kestrel", &map).unwrap();
        let loaded = load_map(dir.path(), "kestrel");

        assert_eq!(loaded, map);
    }

    /// Two sessions on the same profile each learn something the other
    /// doesn't; saving one must not blank out what the other already
    /// wrote to disk.
    /// An adversarial review flagged this as data loss: `save_map` used to
    /// merge into `load_map`, which answers "start empty" for anything it
    /// cannot read. One unreadable file, and the next save — every 30s now
    /// — replaced a whole explored world with the handful of rooms this
    /// session happened to see.
    #[test]
    fn save_map_refuses_to_overwrite_a_map_it_could_not_read() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let path = dir.path().join("maps").join("kestrel.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{ this is not json").unwrap();

        let mut session = crate::map::Map::default();
        session.observe(&sample_room(1, "Temple Square"));
        let err = save_map(dir.path(), "kestrel", &session).unwrap_err();

        assert!(
            format!("{err:#}").contains("parsing the map already at"),
            "the failure should name what it refused to do: {err:#}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{ this is not json",
            "and the file the player still has must be left alone"
        );
    }

    /// The other half — a missing file is not a failure, it is a first run.
    #[test]
    fn save_map_still_writes_when_there_is_nothing_on_disk_yet() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let mut session = crate::map::Map::default();
        session.observe(&sample_room(1, "Temple Square"));

        save_map(dir.path(), "kestrel", &session).unwrap();

        assert!(
            load_map(dir.path(), "kestrel")
                .rooms
                .contains_key(&crate::map::RoomId(1))
        );
    }

    #[test]
    fn save_map_merges_with_what_is_already_on_disk() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();

        let mut first = crate::map::Map::default();
        first.observe(&sample_room(1, "Temple Square"));
        save_map(dir.path(), "kestrel", &first).unwrap();

        let mut second = crate::map::Map::default();
        second.observe(&sample_room(2, "The Docks"));
        save_map(dir.path(), "kestrel", &second).unwrap();

        let loaded = load_map(dir.path(), "kestrel");
        assert_eq!(
            loaded
                .rooms
                .get(&crate::map::RoomId(1))
                .map(|r| r.name.as_deref()),
            Some(Some("Temple Square")),
            "the first session's room must survive the second session's save"
        );
        assert_eq!(
            loaded
                .rooms
                .get(&crate::map::RoomId(2))
                .map(|r| r.name.as_deref()),
            Some(Some("The Docks"))
        );
    }

    // ---- first-run wizard (§15) ----

    #[test]
    fn has_profiles_is_false_until_the_profiles_dir_has_one() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let dir = dir.path();
        assert!(!has_profiles(dir), "neither the dir nor a profile exists");

        std::fs::create_dir_all(dir.join("profiles")).unwrap();
        assert!(!has_profiles(dir), "an empty profiles dir is still none");

        std::fs::write(dir.join("profiles/kestrel.yaml"), "name: kestrel").unwrap();
        assert!(has_profiles(dir));
    }

    /// What the wizard writes must be exactly what `load_profile` reads —
    /// otherwise the form's whole point (no hand-editing YAML) is undone
    /// the moment a saved profile fails to load back.
    #[test]
    fn a_wizard_profile_round_trips_through_load_profile() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let dir = dir.path();
        let new_profile = NewProfile {
            name: "kestrel".to_string(),
            host: "underworld.example.org".to_string(),
            port: 4443,
            tls: true,
        };

        save_new_profile(dir, &new_profile).unwrap();
        let loaded = load_profile(&profile_path(dir, "kestrel")).unwrap();

        assert_eq!(loaded.host, "underworld.example.org");
        assert_eq!(loaded.port, 4443);
        assert!(loaded.tls.enabled);
    }

    /// A host with YAML-special characters must still come out as the
    /// literal string typed, not be reinterpreted as YAML syntax — the
    /// reason this is serialized rather than hand-formatted.
    #[test]
    fn a_host_with_yaml_special_characters_survives_the_round_trip() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let dir = dir.path();
        let new_profile = NewProfile {
            name: "weird".to_string(),
            host: "mud.example.org: # not a comment".to_string(),
            port: 23,
            tls: false,
        };

        save_new_profile(dir, &new_profile).unwrap();
        let loaded = load_profile(&profile_path(dir, "weird")).unwrap();

        assert_eq!(loaded.host, "mud.example.org: # not a comment");
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

    /// Scripts come off disk here, not in `engine`, and from beside the
    /// module that declared them (§7.4).
    #[test]
    fn a_module_loads_the_scripts_it_declares_from_beside_itself() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let dir = dir.path();
        std::fs::create_dir_all(dir.join("modules")).unwrap();
        std::fs::write(
            dir.join("modules/combat.yaml"),
            "name: combat\nscripts: [combat.lua]\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("modules/combat.lua"),
            "mud.on_line(function() end)",
        )
        .unwrap();

        let module = load_module(&dir.join("modules/combat.yaml")).expect("module loads");
        assert_eq!(module.script_sources.len(), 1);
        assert_eq!(module.script_sources[0].name, "combat.lua");
        assert!(module.script_sources[0].code.contains("mud.on_line"));
    }

    /// A shared community module is one directory to copy, so a script name
    /// is a name — not a way to reach the rest of the filesystem.
    #[test]
    fn a_script_name_that_is_a_path_is_refused() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let dir = dir.path();
        std::fs::create_dir_all(dir.join("modules")).unwrap();
        std::fs::write(
            dir.join("modules/combat.yaml"),
            "name: combat\nscripts: ['../../etc/passwd.lua']\n",
        )
        .unwrap();

        let err = load_module(&dir.join("modules/combat.yaml")).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("passwd.lua"), "{message}");
    }

    /// A missing script must name itself: a module that half-loaded is
    /// worse than one that refused to.
    #[test]
    fn a_missing_script_names_the_file() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let dir = dir.path();
        std::fs::create_dir_all(dir.join("modules")).unwrap();
        std::fs::write(
            dir.join("modules/combat.yaml"),
            "name: combat\nscripts: [combat.lua]\n",
        )
        .unwrap();

        let err = load_module(&dir.join("modules/combat.yaml")).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("combat.lua"), "{message}");
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

    /// A shared/imported profile is one file, and a module name in it is a
    /// name — not a way to make `load_rules` read an arbitrary file on the
    /// victim's disk and fold its contents into the rule engine.
    #[test]
    fn a_module_name_that_is_a_path_is_refused() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let dir = dir.path();
        std::fs::create_dir_all(dir.join("profiles")).unwrap();
        // The escape route: `modules/` must actually exist for `..` to
        // climb out of it, so this fixture creates it — a bare-name-only
        // subdir like the shipped examples use — with a real file one
        // level up for a would-be traversal to land on.
        std::fs::create_dir_all(dir.join("modules")).unwrap();
        std::fs::write(dir.join("secret.yaml"), "name: leaked\n").unwrap();
        std::fs::write(
            dir.join("profiles/tank.yaml"),
            "name: tank\nhost: h\nport: 1\nmodules: ['../secret']\n",
        )
        .unwrap();

        let err = load_rules(dir, Some("tank"), &[]).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("../secret"), "{message}");
        assert!(message.contains("tank"), "{message}");
    }

    /// `validate_profile_rules` (the in-editor validator, §10.2) must refuse
    /// the same traversal `load_rules` does — it runs the identical
    /// construction against a draft that isn't on disk yet.
    #[test]
    fn validate_profile_rules_also_refuses_a_module_path() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let dir = dir.path();
        std::fs::create_dir_all(dir.join("profiles")).unwrap();
        std::fs::create_dir_all(dir.join("modules")).unwrap();
        std::fs::write(dir.join("secret.yaml"), "name: leaked\n").unwrap();

        let mut draft = minimal_profile("tank", "h");
        draft.modules = vec!["../secret".to_string()];

        let err = validate_profile_rules(dir, "tank", &draft, &[]).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("../secret"), "{message}");
    }

    // ---- trigger hygiene (§10.2, UX_REVIEW.md finding 3) ----

    fn trigger(pattern: &str) -> Trigger {
        Trigger {
            pattern: Some(pattern.to_string()),
            ..Trigger::default()
        }
    }

    #[test]
    fn check_trigger_hygiene_allows_distinct_patterns_with_actions() {
        let triggers = vec![
            Trigger {
                send: Some(vec!["cast heal".to_string()]),
                ..trigger("hp low")
            },
            Trigger {
                gag: Some(true),
                ..trigger("spam")
            },
        ];
        assert!(check_trigger_hygiene(&triggers).is_ok());
    }

    #[test]
    fn check_trigger_hygiene_rejects_a_duplicate_pattern() {
        let triggers = vec![
            Trigger {
                send: Some(vec!["look".to_string()]),
                ..trigger("rat")
            },
            Trigger {
                gag: Some(true),
                ..trigger("rat")
            },
        ];
        let err = check_trigger_hygiene(&triggers).unwrap_err();
        assert!(format!("{err:#}").contains("rat"), "{err:#}");
    }

    #[test]
    fn check_trigger_hygiene_rejects_a_trigger_with_no_action() {
        let triggers = vec![trigger("rat")];
        let err = check_trigger_hygiene(&triggers).unwrap_err();
        assert!(format!("{err:#}").contains("rat"), "{err:#}");
    }

    /// `gag:` alone is a real, common action — hiding a line is not
    /// "nothing," even though it never touches `send:`.
    #[test]
    fn check_trigger_hygiene_allows_a_gag_only_trigger() {
        let triggers = vec![Trigger {
            gag: Some(true),
            ..trigger("spam")
        }];
        assert!(check_trigger_hygiene(&triggers).is_ok());
    }

    /// An id-only trigger (matched by `id`, not `pattern` — used for
    /// shadowing) has no pattern to collide on; two of them must not be
    /// mistaken for duplicates of each other.
    #[test]
    fn check_trigger_hygiene_ignores_id_only_triggers_when_checking_duplicates() {
        let triggers = vec![
            Trigger {
                id: Some("a".to_string()),
                gag: Some(true),
                ..Trigger::default()
            },
            Trigger {
                id: Some("b".to_string()),
                gag: Some(true),
                ..Trigger::default()
            },
        ];
        assert!(check_trigger_hygiene(&triggers).is_ok());
    }

    /// Wired into the save-time validator a profile session actually uses,
    /// not just the standalone function.
    #[test]
    fn validate_profile_rules_refuses_a_no_op_trigger() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let dir = dir.path();
        std::fs::create_dir_all(dir.join("profiles")).unwrap();

        let mut draft = minimal_profile("tank", "h");
        draft.triggers = vec![trigger("rat")];

        let err = validate_profile_rules(dir, "tank", &draft, &[]).unwrap_err();
        assert!(format!("{err:#}").contains("no action"), "{err:#}");
    }

    /// A profile-level rule missing both `id` and `pattern` used to read
    /// as `` rule in `profile `tank`` needs an `id` or a `pattern` `` —
    /// the profile layer's own label pre-wrapped itself in backticks, and
    /// the error template wrapped it again (docs/UX_REVIEW.md, Adversarial
    /// findings, Low #5).
    #[test]
    fn validate_profile_rules_does_not_double_the_backticks_around_a_profile_name() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let dir = dir.path();
        std::fs::create_dir_all(dir.join("profiles")).unwrap();

        let mut draft = minimal_profile("tank", "h");
        // An action but no id/pattern: passes trigger-hygiene, so
        // `Engine::compile` is what rejects it, for lacking identity.
        draft.triggers = vec![Trigger {
            send: Some(vec!["look".to_string()]),
            ..Trigger::default()
        }];

        let err = validate_profile_rules(dir, "tank", &draft, &[]).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("in `profile tank`"),
            "expected a single-quoted label: {message}"
        );
        assert!(
            !message.contains("`profile `tank`"),
            "backticks must not double up: {message}"
        );
    }

    /// The shipped examples are documentation: if they stop loading, the
    /// docs are wrong. Loading them here keeps that from going unnoticed.
    #[cfg(feature = "lua")]
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
        let death = engine.process_line("The kobold is DEAD!");
        assert!(death.sends.is_empty(), "profile disables autoloot");
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

        // The example module's script loaded too, and reads the variables
        // its YAML set.
        #[cfg(feature = "lua")]
        {
            // ...and the still-enabled rule beside autoloot called into it.
            assert_eq!(death.echoes, vec!["** The kobold down (1 this session)"]);
            assert_eq!(engine.on_connect().sends, vec!["say well met"]);
            assert_eq!(
                engine.process_line("You quaff a blue potion.").echoes,
                vec!["** 1 potions this session"]
            );
        }
    }

    /// The examples use a Lua script, so a build without that engine
    /// cannot run them — and the one thing it owes the player is to say so
    /// by name rather than fail obscurely.
    #[cfg(not(feature = "lua"))]
    #[test]
    fn shipped_example_config_says_which_engine_this_build_lacks() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/config");
        let app = load_app_config(&dir).expect("example mudular.yaml loads");

        let layers = load_rules(&dir, Some("kestrel"), &app.channels).expect("example rules load");
        let err = crate::engine::Engine::compile(&layers).unwrap_err();
        assert!(
            matches!(err, crate::engine::EngineError::ScriptEngineMissing { .. }),
            "{err}"
        );
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

    /// A refusal is per profile, and recording one twice must not turn the
    /// file into a growing list of the same name.
    #[test]
    fn a_refusal_is_recorded_per_profile_and_only_once() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let dir = dir.path();

        assert!(!password_save_declined(dir, "kestrel"));
        decline_password_save(dir, "kestrel").unwrap();
        decline_password_save(dir, "kestrel").unwrap();
        decline_password_save(dir, "tank").unwrap();

        assert!(password_save_declined(dir, "kestrel"));
        assert!(password_save_declined(dir, "tank"));
        assert!(!password_save_declined(dir, "cleric"));
        assert_eq!(
            std::fs::read_to_string(dir.join(DECLINED_FILE)).unwrap(),
            "kestrel\ntank\n"
        );
    }

    // ---- in-client profile editor safe save (§10.2) ----

    fn minimal_profile(name: &str, host: &str) -> Profile {
        Profile {
            name: name.to_string(),
            host: host.to_string(),
            port: 4000,
            tls: TlsSettings::default(),
            charset: default_charset(),
            color: None,
            login: None,
            modules: Vec::new(),
            variables: BTreeMap::new(),
            aliases: Vec::new(),
            triggers: Vec::new(),
            timers: Vec::new(),
            scripts: Vec::new(),
            cross_session: None,
            log: false,
        }
    }

    #[test]
    fn utc_stamp_matches_known_instants() {
        assert_eq!(utc_stamp(std::time::UNIX_EPOCH), "19700101T000000Z");
        // A leap day.
        assert_eq!(
            utc_stamp(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_709_164_800)),
            "20240229T000000Z"
        );
        // A year boundary, well into the hour/minute/second fields too.
        assert_eq!(
            utc_stamp(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_735_689_599)),
            "20241231T235959Z"
        );
    }

    #[test]
    fn atomic_write_leaves_no_partial_file() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let path = dir.path().join("profiles").join("kestrel.yaml");
        atomic_write(&path, b"host: h\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "host: h\n");
        let leftover = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().contains(".tmp"));
        assert!(!leftover, "no temp file should survive a successful write");
    }

    /// A profile can carry an account name and other details the player
    /// treats as private — owner-only on disk, not left at the process
    /// umask.
    #[cfg(unix)]
    #[test]
    fn atomic_write_creates_the_file_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let path = dir.path().join("profiles").join("kestrel.yaml");
        atomic_write(&path, b"host: h\n").unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
    }

    #[test]
    fn atomic_write_cleans_up_its_temp_file_on_a_write_failure() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        // A directory where the target file should be turns "create the temp
        // file" into an error, exercising the cleanup path.
        let profiles_dir = dir.path().join("profiles");
        std::fs::create_dir_all(&profiles_dir).unwrap();
        let bad_target = profiles_dir.join("kestrel.yaml");
        std::fs::create_dir_all(&bad_target).unwrap();

        let err = atomic_write(&bad_target, b"host: h\n").unwrap_err();
        assert!(matches!(err, SaveError::Io { .. }));
        let leftover = std::fs::read_dir(&profiles_dir)
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().contains(".tmp"));
        assert!(
            !leftover,
            "a failed write must not leave its temp file behind"
        );
    }

    #[test]
    fn save_creates_a_timestamped_backup_of_the_previous_version() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let path = dir.path().join("profiles").join("kestrel.yaml");
        let first = minimal_profile("kestrel", "first.example.org");
        atomic_write(&path, serde_yaml::to_string(&first).unwrap().as_bytes()).unwrap();

        let mut file = load_profile_file(&path).unwrap();
        assert!(
            std::fs::read_dir(dir.path().join("backups")).is_err(),
            "nothing to back up yet"
        );

        let second = minimal_profile("kestrel", "second.example.org");
        let saved = save_profile(&mut file, &second, SaveMode::Guarded).unwrap();
        let backup = saved.backup.expect("a previous version existed");
        assert_eq!(
            std::fs::read_to_string(&backup).unwrap(),
            serde_yaml::to_string(&first).unwrap()
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            serde_yaml::to_string(&second).unwrap()
        );
    }

    #[test]
    fn prune_keeps_only_the_newest_backups() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let path = dir.path().join("profiles").join("kestrel.yaml");
        let backup_dir = backup_dir_for(&path).unwrap();
        std::fs::create_dir_all(&backup_dir).unwrap();
        for day in 1..=25u32 {
            std::fs::write(
                backup_dir.join(format!("202401{day:02}T000000Z.yaml")),
                b"x",
            )
            .unwrap();
        }

        prune_backups(&path).unwrap();

        let mut remaining: Vec<String> = std::fs::read_dir(&backup_dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        remaining.sort();
        assert_eq!(remaining.len(), PROFILE_BACKUPS_KEPT);
        assert_eq!(
            remaining[0], "20240106T000000Z.yaml",
            "the oldest 5 were pruned"
        );
        assert_eq!(remaining[remaining.len() - 1], "20240125T000000Z.yaml");
    }

    #[test]
    fn conflict_is_detected_when_the_file_changed_on_disk() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let path = dir.path().join("profiles").join("kestrel.yaml");
        let original = minimal_profile("kestrel", "original.example.org");
        atomic_write(&path, serde_yaml::to_string(&original).unwrap().as_bytes()).unwrap();
        let mut file = load_profile_file(&path).unwrap();

        // A digest catches this regardless of whether the byte count
        // happens to match, unlike a size-only or mtime-only check.
        let interloper = minimal_profile("kestrel", "sneaky.example.org");
        std::fs::write(&path, serde_yaml::to_string(&interloper).unwrap()).unwrap();

        let edit = minimal_profile("kestrel", "edited.example.org");
        let err = save_profile(&mut file, &edit, SaveMode::Guarded).unwrap_err();
        assert!(matches!(err, SaveError::Conflict { .. }));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            serde_yaml::to_string(&interloper).unwrap(),
            "a rejected save must not touch the file"
        );
    }

    #[test]
    fn overwrite_mode_ignores_the_conflict_but_still_backs_up_the_interloper() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let path = dir.path().join("profiles").join("kestrel.yaml");
        let original = minimal_profile("kestrel", "original.example.org");
        atomic_write(&path, serde_yaml::to_string(&original).unwrap().as_bytes()).unwrap();
        let mut file = load_profile_file(&path).unwrap();

        let interloper = minimal_profile("kestrel", "sneaky.example.org");
        std::fs::write(&path, serde_yaml::to_string(&interloper).unwrap()).unwrap();

        let edit = minimal_profile("kestrel", "edited.example.org");
        let saved = save_profile(&mut file, &edit, SaveMode::Overwrite).unwrap();
        let backup = saved
            .backup
            .expect("the interloper's version gets backed up");
        assert_eq!(
            std::fs::read_to_string(&backup).unwrap(),
            serde_yaml::to_string(&interloper).unwrap()
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            serde_yaml::to_string(&edit).unwrap()
        );
    }

    #[test]
    fn two_saves_in_one_editor_session_do_not_self_conflict() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let path = dir.path().join("profiles").join("kestrel.yaml");
        let original = minimal_profile("kestrel", "one.example.org");
        atomic_write(&path, serde_yaml::to_string(&original).unwrap().as_bytes()).unwrap();
        let mut file = load_profile_file(&path).unwrap();

        let edit1 = minimal_profile("kestrel", "two.example.org");
        save_profile(&mut file, &edit1, SaveMode::Guarded).unwrap();
        let edit2 = minimal_profile("kestrel", "three.example.org");
        save_profile(&mut file, &edit2, SaveMode::Guarded).unwrap();
    }

    #[test]
    fn missing_file_reports_vanished() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let path = dir.path().join("profiles").join("kestrel.yaml");
        let original = minimal_profile("kestrel", "one.example.org");
        atomic_write(&path, serde_yaml::to_string(&original).unwrap().as_bytes()).unwrap();
        let mut file = load_profile_file(&path).unwrap();
        std::fs::remove_file(&path).unwrap();

        let edit = minimal_profile("kestrel", "two.example.org");
        let err = save_profile(&mut file, &edit, SaveMode::Guarded).unwrap_err();
        assert!(matches!(err, SaveError::Vanished { .. }));
    }

    #[test]
    fn unchanged_profile_saves_are_byte_identical() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let path = dir.path().join("profiles").join("kestrel.yaml");
        let mut profile = minimal_profile("kestrel", "h.example.org");
        profile
            .variables
            .insert("target".to_string(), "rat".to_string());
        profile
            .variables
            .insert("mode".to_string(), "aggro".to_string());
        atomic_write(&path, serde_yaml::to_string(&profile).unwrap().as_bytes()).unwrap();

        let mut file = load_profile_file(&path).unwrap();
        save_profile(&mut file, &profile, SaveMode::Guarded).unwrap();
        let first_save = std::fs::read_to_string(&path).unwrap();
        save_profile(&mut file, &profile, SaveMode::Guarded).unwrap();
        let second_save = std::fs::read_to_string(&path).unwrap();
        assert_eq!(first_save, second_save);
    }

    #[test]
    fn round_trips_a_profile_with_every_field_type() {
        let dir = crate::net::pins::tests::tempdir::TempDir::new();
        let path = dir.path().join("profiles").join("kestrel.yaml");
        let mut profile = minimal_profile("kestrel", "underworld.example.org");
        profile.tls = TlsSettings {
            enabled: true,
            verify: crate::net::VerifyMode::Pinned,
        };
        profile.color = Some(ratatui::style::Color::Cyan);
        profile.login = Some(Login {
            name: "Kestrel".to_string(),
            name_prompt: Some(r"Name:\s*$".to_string()),
            password_prompt: None,
        });
        profile.modules = vec!["uw-common".to_string()];
        profile
            .variables
            .insert("target".to_string(), "rat".to_string());
        profile.cross_session = Some(CrossSessionOverride {
            expand_aliases: Some(true),
            max_hops: None,
        });
        profile.scripts = vec!["combat.lua".to_string()];
        profile.log = true;

        let mut alias = Alias {
            id: Some("quicklook".to_string()),
            pattern: Some("^ll$".to_string()),
            send: Some(vec!["look".to_string()]),
            ..Default::default()
        };
        let mut send_to = BTreeMap::new();
        send_to.insert("cleric".to_string(), vec!["heal".to_string()]);
        alias.send_to = Some(send_to);
        let mut set = BTreeMap::new();
        set.insert("last".to_string(), "look".to_string());
        alias.set = Some(set);
        alias.script = Some(crate::engine::ScriptAction {
            file: "combat.lua".to_string(),
            function: "on_look".to_string(),
        });
        alias.when = Some("${target} != \"\"".to_string());
        profile.aliases = vec![alias];

        let trigger = Trigger {
            id: Some("greet".to_string()),
            pattern: Some(r"^(?P<who>\w+) has arrived\.$".to_string()),
            send: Some(vec!["say welcome ${who}".to_string()]),
            gag: Some(false),
            route: Some("chat".to_string()),
            bell: Some(true),
            highlight: Some(crate::engine::HighlightSpec {
                fg: Some("red".to_string()),
                bold: true,
                whole_line: true,
                ..Default::default()
            }),
            enabled: None,
            ..Default::default()
        };
        profile.triggers = vec![trigger];

        let timer = Timer {
            id: Some("tick".to_string()),
            every: Some("60s".to_string()),
            set: Some(BTreeMap::from([("ticks".to_string(), "0".to_string())])),
            ..Default::default()
        };
        profile.timers = vec![timer];

        atomic_write(&path, serde_yaml::to_string(&profile).unwrap().as_bytes()).unwrap();
        let reloaded = load_profile(&path).unwrap();
        assert_eq!(reloaded, profile);

        // The editor's own output must re-parse through the real,
        // `deny_unknown_fields` loader without complaint.
        let mut file = load_profile_file(&path).unwrap();
        save_profile(&mut file, &profile, SaveMode::Guarded).unwrap();
        let reloaded_again = load_profile(&path).unwrap();
        assert_eq!(reloaded_again, profile);
    }

    #[test]
    fn unset_options_are_omitted_not_nulled() {
        let profile = minimal_profile("kestrel", "h.example.org");
        let yaml = serde_yaml::to_string(&profile).unwrap();
        assert!(!yaml.contains("color"));
        assert!(!yaml.contains("login"));
        assert!(!yaml.contains("cross_session"));

        let trigger = Trigger {
            id: Some("x".to_string()),
            pattern: Some("x".to_string()),
            enabled: Some(false),
            ..Default::default()
        };
        let yaml = serde_yaml::to_string(&trigger).unwrap();
        assert!(yaml.contains("enabled: false"), "{yaml}");
        assert!(!yaml.contains("gag"));
        assert!(!yaml.contains("when"));
    }
}
