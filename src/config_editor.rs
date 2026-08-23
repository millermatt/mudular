//! The in-client profile editor's model (docs/ARCHITECTURE.md §10.2): CRUD
//! over one profile's connection settings, variables, aliases, triggers,
//! and timers, reached with `F5` or `/config` while a profile session is
//! focused.
//!
//! A pure state machine, and deliberately below `ui`: it names no widget
//! and draws nothing, so the model can be read and driven by a front end
//! that is not the full-screen TUI (§4.1). `ui::config_editor` is the half
//! that puts it on a terminal.
//!
//! No IO happens here either. A save is handed back to `app.rs` as
//! [`EditorAction::Save`], which owns the filesystem.

use std::collections::HashSet;
use std::path::PathBuf;

use crossterm::event::{Event, KeyCode, KeyModifiers};
use ratatui::style::Color;
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;

use crate::config::{self, Channel, Profile};
use crate::engine::{Alias, ScriptAction, Timer, Trigger};

/// The six things a profile can hold that this editor knows how to CRUD.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Section {
    Connection,
    Variables,
    Aliases,
    Triggers,
    Timers,
    Modules,
}

pub(crate) const SECTIONS: [Section; 6] = [
    Section::Connection,
    Section::Variables,
    Section::Aliases,
    Section::Triggers,
    Section::Timers,
    Section::Modules,
];

impl Section {
    pub(crate) fn title(self) -> &'static str {
        match self {
            Section::Connection => "Connection",
            Section::Variables => "Variables",
            Section::Aliases => "Aliases",
            Section::Triggers => "Triggers",
            Section::Timers => "Timers",
            Section::Modules => "Modules",
        }
    }

    pub(crate) fn index(self) -> usize {
        SECTIONS
            .iter()
            .position(|s| *s == self)
            .expect("in SECTIONS")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuleKind {
    Alias,
    Trigger,
    Timer,
}

impl RuleKind {
    pub(crate) fn fields(self) -> &'static [&'static str] {
        match self {
            RuleKind::Alias => &["id", "pattern", "when", "send", "enabled"],
            RuleKind::Trigger => &[
                "id", "pattern", "when", "send", "gag", "route", "bell", "corpse", "enabled",
            ],
            RuleKind::Timer => &["id", "every", "after", "send", "enabled"],
        }
    }

    pub(crate) fn noun(self) -> &'static str {
        match self {
            RuleKind::Alias => "alias",
            RuleKind::Trigger => "trigger",
            RuleKind::Timer => "timer",
        }
    }
}

/// One connection-section field. `Connection` isn't a list of objects like
/// the rule sections — it's a fixed set of scalar fields on the profile
/// itself, so it gets its own small enum rather than a `RuleKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnField {
    Host,
    Port,
    TlsEnabled,
    TlsVerify,
    Charset,
    Color,
    LoginName,
    LoginNamePrompt,
    LoginPasswordPrompt,
    Log,
}

pub(crate) const CONN_FIELDS: [ConnField; 10] = [
    ConnField::Host,
    ConnField::Port,
    ConnField::TlsEnabled,
    ConnField::TlsVerify,
    ConnField::Charset,
    ConnField::Color,
    ConnField::LoginName,
    ConnField::LoginNamePrompt,
    ConnField::LoginPasswordPrompt,
    ConnField::Log,
];

impl ConnField {
    pub(crate) fn label(self) -> &'static str {
        match self {
            ConnField::Host => "host",
            ConnField::Port => "port",
            ConnField::TlsEnabled => "tls.enabled",
            ConnField::TlsVerify => "tls.verify",
            ConnField::Charset => "charset",
            ConnField::Color => "color",
            ConnField::LoginName => "login.name",
            ConnField::LoginNamePrompt => "login.name_prompt",
            ConnField::LoginPasswordPrompt => "login.password_prompt",
            ConnField::Log => "log",
        }
    }

    /// `Space`/`Enter` in Browse toggle or cycle these in place, rather than
    /// opening a text field — there is nothing to type.
    pub(crate) fn is_toggle(self) -> bool {
        matches!(
            self,
            ConnField::TlsEnabled | ConnField::TlsVerify | ConnField::Log
        )
    }
}

/// What a commit out of [`Mode::Input`] writes back to.
#[derive(Debug, Clone)]
pub(crate) enum InputTarget {
    Connection(ConnField),
    RuleField {
        kind: RuleKind,
        index: usize,
        field: usize,
    },
    /// Adding a variable is two steps: the key, then the value.
    NewVariableKey,
    VariableValue {
        key: String,
    },
    ModuleEntry {
        index: Option<usize>,
    },
}

#[derive(Debug, Clone)]
pub(crate) enum PendingAction {
    DeleteRule { kind: RuleKind, index: usize },
    DeleteVariable { key: String },
    DeleteModule { index: usize },
    DiscardAndClose,
    OverwriteConflict,
    RecreateVanished,
}

pub(crate) enum Mode {
    Browse,
    Form {
        kind: RuleKind,
        index: usize,
        field: usize,
    },
    Input {
        input: Input,
        target: InputTarget,
    },
    Confirm {
        prompt: String,
        action: PendingAction,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoticeLevel {
    Info,
    Warn,
    Error,
}

pub(crate) struct Notice {
    pub(crate) level: NoticeLevel,
    pub(crate) text: String,
}

/// What a keypress asked `app.rs` to do, once the editor itself is done
/// with it. The editor always consumes the key while it is open — even
/// `Consumed` means "don't let this reach the session's input line".
pub enum EditorAction {
    Consumed,
    Close,
    Save { force: bool },
}

pub struct ConfigEditorState {
    pub(crate) file: config::ProfileFile,
    pub(crate) draft: Profile,
    dir: PathBuf,
    pub(crate) name: String,
    pub(crate) section: Section,
    pub(crate) cursor: [usize; 6],
    pub(crate) mode: Mode,
    pub(crate) notice: Option<Notice>,
    channel_names: Vec<String>,
    pub(crate) known_modules: HashSet<String>,
}

impl ConfigEditorState {
    pub fn open(
        file: config::ProfileFile,
        dir: PathBuf,
        name: String,
        channels: &[Channel],
        known_modules: HashSet<String>,
        had_comments: bool,
    ) -> Self {
        let draft = file.profile.clone();
        Self {
            file,
            draft,
            dir,
            name,
            section: Section::Connection,
            cursor: [0; 6],
            mode: Mode::Browse,
            notice: had_comments.then(|| Notice {
                level: NoticeLevel::Warn,
                text: "this file has comments — saving rewrites it and drops them \
                       (a backup is kept)"
                    .to_string(),
            }),
            channel_names: channels.iter().map(|c| c.name.clone()).collect(),
            known_modules,
        }
    }

    /// Moves the cursor to the named connection setting, so the palette
    /// can land on it rather than at the top of the section (#43).
    ///
    /// Returns whether the name is one of them: `app.rs` checks before
    /// opening, so a typo is refused rather than opening somewhere the
    /// player did not ask for.
    pub fn focus_setting(&mut self, name: &str) -> bool {
        let Some(index) = CONN_FIELDS.iter().position(|field| field.label() == name) else {
            return false;
        };
        self.section = Section::Connection;
        self.cursor[Section::Connection.index()] = index;
        true
    }

    /// Which connection setting the cursor is on, for the tests that pin
    /// where opening lands.
    #[cfg(test)]
    pub fn selected_setting(&self) -> Option<&'static str> {
        (self.section == Section::Connection)
            .then(|| CONN_FIELDS[self.cursor[Section::Connection.index()]].label())
    }

    /// Opens straight into a brand-new trigger with `pattern` prefilled —
    /// the scrollback line-cursor's entry point (§11.5).
    pub fn open_with_new_trigger(
        file: config::ProfileFile,
        dir: PathBuf,
        name: String,
        channels: &[Channel],
        known_modules: HashSet<String>,
        had_comments: bool,
        pattern: String,
    ) -> Self {
        let mut state = Self::open(file, dir, name, channels, known_modules, had_comments);
        state.draft.triggers.push(Trigger {
            pattern: Some(pattern),
            ..Default::default()
        });
        state.section = Section::Triggers;
        let index = state.draft.triggers.len() - 1;
        state.cursor[Section::Triggers.index()] = index;
        state.mode = Mode::Form {
            kind: RuleKind::Trigger,
            index,
            field: 1, // land on `send`, since `pattern` is already filled
        };
        state
    }

    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn draft(&self) -> &Profile {
        &self.draft
    }

    pub fn file(&mut self) -> &mut config::ProfileFile {
        &mut self.file
    }

    pub fn original_profile(&self) -> &Profile {
        &self.file.profile
    }

    pub fn dirty(&self) -> bool {
        self.draft != self.file.profile
    }

    pub fn set_notice_error(&mut self, text: String) {
        self.notice = Some(Notice {
            level: NoticeLevel::Error,
            text,
        });
    }

    pub fn set_notice_info(&mut self, text: String) {
        self.notice = Some(Notice {
            level: NoticeLevel::Info,
            text,
        });
    }

    /// After a successful save, the on-disk snapshot the profile is now
    /// compared against moves forward, so a second edit-then-save in the
    /// same session doesn't read as "already saved".
    pub fn note_saved(&mut self) {
        // `file.profile` was already updated by `config::save_profile`.
    }

    /// Enters the conflict/vanished confirmation, called by `app.rs` when a
    /// save comes back with one of those errors.
    pub fn prompt_conflict(&mut self, at: &str) {
        self.mode = Mode::Confirm {
            prompt: format!(
                "{} changed on disk since you opened it (at {at}). Overwrite it? \
                 Your version will be written; the current file is backed up first. (y/N)",
                self.file.path.display()
            ),
            action: PendingAction::OverwriteConflict,
        };
    }

    pub fn prompt_vanished(&mut self) {
        self.mode = Mode::Confirm {
            prompt: format!(
                "{} no longer exists. Recreate it with your changes? (y/N)",
                self.file.path.display()
            ),
            action: PendingAction::RecreateVanished,
        };
    }

    pub fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> EditorAction {
        if modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('s' | 'S')) {
            return EditorAction::Save { force: false };
        }

        match &mut self.mode {
            Mode::Browse => self.handle_browse(code, modifiers),
            Mode::Form { .. } => self.handle_form(code),
            Mode::Input { .. } => self.handle_input(code, modifiers),
            Mode::Confirm { .. } => self.handle_confirm(code),
        }
    }

    fn list_len(&self, section: Section) -> usize {
        match section {
            Section::Connection => CONN_FIELDS.len(),
            Section::Variables => self.draft.variables.len(),
            Section::Aliases => self.draft.aliases.len(),
            Section::Triggers => self.draft.triggers.len(),
            Section::Timers => self.draft.timers.len(),
            Section::Modules => self.draft.modules.len(),
        }
    }

    fn move_cursor(&mut self, delta: i64) {
        let len = self.list_len(self.section);
        if len == 0 {
            return;
        }
        let cur = &mut self.cursor[self.section.index()];
        let next = (*cur as i64 + delta).clamp(0, len as i64 - 1);
        *cur = next as usize;
    }

    fn handle_browse(&mut self, code: KeyCode, modifiers: KeyModifiers) -> EditorAction {
        match code {
            KeyCode::Esc => {
                if self.dirty() {
                    self.mode = Mode::Confirm {
                        prompt: format!(
                            "Discard unsaved changes to {}? (y/N, s = save then close)",
                            self.name
                        ),
                        action: PendingAction::DiscardAndClose,
                    };
                    EditorAction::Consumed
                } else {
                    EditorAction::Close
                }
            }
            KeyCode::Tab | KeyCode::Right => {
                self.section = SECTIONS[(self.section.index() + 1) % SECTIONS.len()];
                EditorAction::Consumed
            }
            KeyCode::BackTab | KeyCode::Left => {
                self.section =
                    SECTIONS[(self.section.index() + SECTIONS.len() - 1) % SECTIONS.len()];
                EditorAction::Consumed
            }
            KeyCode::Char(c @ '1'..='6') => {
                self.section = SECTIONS[c.to_digit(10).unwrap() as usize - 1];
                EditorAction::Consumed
            }
            KeyCode::Up => {
                self.move_cursor(-1);
                EditorAction::Consumed
            }
            KeyCode::Down => {
                self.move_cursor(1);
                EditorAction::Consumed
            }
            KeyCode::PageUp => {
                self.move_cursor(-5);
                EditorAction::Consumed
            }
            KeyCode::PageDown => {
                self.move_cursor(5);
                EditorAction::Consumed
            }
            KeyCode::Char('a') => self.begin_add(),
            KeyCode::Char('d') => self.begin_delete(),
            KeyCode::Enter | KeyCode::Char('e') => self.begin_edit(),
            KeyCode::Char(' ') => self.toggle_selected(modifiers),
            _ => EditorAction::Consumed,
        }
    }

    fn begin_add(&mut self) -> EditorAction {
        let cursor_index = self.cursor[self.section.index()];
        match self.section {
            Section::Aliases => {
                self.draft.aliases.push(Alias::default());
                let index = self.draft.aliases.len() - 1;
                self.cursor[Section::Aliases.index()] = index;
                self.mode = Mode::Form {
                    kind: RuleKind::Alias,
                    index,
                    field: 0,
                };
            }
            Section::Triggers => {
                self.draft.triggers.push(Trigger::default());
                let index = self.draft.triggers.len() - 1;
                self.cursor[Section::Triggers.index()] = index;
                self.mode = Mode::Form {
                    kind: RuleKind::Trigger,
                    index,
                    field: 0,
                };
            }
            Section::Timers => {
                self.draft.timers.push(Timer::default());
                let index = self.draft.timers.len() - 1;
                self.cursor[Section::Timers.index()] = index;
                self.mode = Mode::Form {
                    kind: RuleKind::Timer,
                    index,
                    field: 0,
                };
            }
            Section::Variables => {
                self.mode = Mode::Input {
                    input: Input::default(),
                    target: InputTarget::NewVariableKey,
                };
            }
            Section::Modules => {
                self.mode = Mode::Input {
                    input: Input::default(),
                    target: InputTarget::ModuleEntry { index: None },
                };
            }
            Section::Connection => {
                let _ = cursor_index;
            }
        }
        EditorAction::Consumed
    }

    fn begin_delete(&mut self) -> EditorAction {
        let index = self.cursor[self.section.index()];
        match self.section {
            Section::Aliases if index < self.draft.aliases.len() => {
                let rule = &self.draft.aliases[index];
                self.mode = Mode::Confirm {
                    prompt: delete_prompt(
                        "alias",
                        rule.id.as_deref(),
                        rule.pattern.as_deref().or(rule.regex.as_deref()),
                        has_advanced_alias(rule),
                    ),
                    action: PendingAction::DeleteRule {
                        kind: RuleKind::Alias,
                        index,
                    },
                };
            }
            Section::Triggers if index < self.draft.triggers.len() => {
                let rule = &self.draft.triggers[index];
                self.mode = Mode::Confirm {
                    prompt: delete_prompt(
                        "trigger",
                        rule.id.as_deref(),
                        rule.pattern.as_deref().or(rule.regex.as_deref()),
                        has_advanced_trigger(rule),
                    ),
                    action: PendingAction::DeleteRule {
                        kind: RuleKind::Trigger,
                        index,
                    },
                };
            }
            Section::Timers if index < self.draft.timers.len() => {
                let rule = &self.draft.timers[index];
                self.mode = Mode::Confirm {
                    prompt: delete_prompt(
                        "timer",
                        rule.id.as_deref(),
                        rule.every.as_deref().or(rule.after.as_deref()),
                        has_advanced_timer(rule),
                    ),
                    action: PendingAction::DeleteRule {
                        kind: RuleKind::Timer,
                        index,
                    },
                };
            }
            Section::Variables => {
                if let Some((key, _)) = self.draft.variables.iter().nth(index) {
                    let key = key.clone();
                    self.mode = Mode::Confirm {
                        prompt: format!("Delete variable `{key}`? (y/N)"),
                        action: PendingAction::DeleteVariable { key },
                    };
                }
            }
            Section::Modules if index < self.draft.modules.len() => {
                let name = &self.draft.modules[index];
                self.mode = Mode::Confirm {
                    prompt: format!("Remove module `{name}` from this profile? (y/N)"),
                    action: PendingAction::DeleteModule { index },
                };
            }
            _ => {}
        }
        EditorAction::Consumed
    }

    fn begin_edit(&mut self) -> EditorAction {
        let index = self.cursor[self.section.index()];
        match self.section {
            Section::Aliases if index < self.draft.aliases.len() => {
                self.mode = Mode::Form {
                    kind: RuleKind::Alias,
                    index,
                    field: 0,
                };
            }
            Section::Triggers if index < self.draft.triggers.len() => {
                self.mode = Mode::Form {
                    kind: RuleKind::Trigger,
                    index,
                    field: 0,
                };
            }
            Section::Timers if index < self.draft.timers.len() => {
                self.mode = Mode::Form {
                    kind: RuleKind::Timer,
                    index,
                    field: 0,
                };
            }
            Section::Variables => {
                if let Some((key, value)) = self.draft.variables.iter().nth(index) {
                    self.mode = Mode::Input {
                        input: Input::new(value.clone()),
                        target: InputTarget::VariableValue { key: key.clone() },
                    };
                }
            }
            Section::Modules if index < self.draft.modules.len() => {
                self.mode = Mode::Input {
                    input: Input::new(self.draft.modules[index].clone()),
                    target: InputTarget::ModuleEntry { index: Some(index) },
                };
            }
            Section::Connection => {
                let field = CONN_FIELDS[index];
                if field.is_toggle() {
                    self.cycle_connection_toggle(field);
                } else {
                    self.mode = Mode::Input {
                        input: Input::new(self.connection_field_text(field)),
                        target: InputTarget::Connection(field),
                    };
                }
            }
            _ => {}
        }
        EditorAction::Consumed
    }

    fn toggle_selected(&mut self, _modifiers: KeyModifiers) -> EditorAction {
        let index = self.cursor[self.section.index()];
        match self.section {
            Section::Aliases if index < self.draft.aliases.len() => {
                cycle_tri_state(&mut self.draft.aliases[index].enabled);
            }
            Section::Triggers if index < self.draft.triggers.len() => {
                cycle_tri_state(&mut self.draft.triggers[index].enabled);
            }
            Section::Timers if index < self.draft.timers.len() => {
                cycle_tri_state(&mut self.draft.timers[index].enabled);
            }
            Section::Connection => {
                let field = CONN_FIELDS[index];
                if field.is_toggle() {
                    self.cycle_connection_toggle(field);
                }
            }
            _ => {}
        }
        EditorAction::Consumed
    }

    fn cycle_connection_toggle(&mut self, field: ConnField) {
        match field {
            ConnField::TlsEnabled => self.draft.tls.enabled = !self.draft.tls.enabled,
            ConnField::TlsVerify => {
                use crate::config::VerifyMode;
                self.draft.tls.verify = match self.draft.tls.verify {
                    VerifyMode::Full => VerifyMode::Pinned,
                    VerifyMode::Pinned => VerifyMode::Insecure,
                    VerifyMode::Insecure => VerifyMode::Full,
                };
                if self.draft.tls.verify == crate::config::VerifyMode::Insecure {
                    self.set_notice_error(
                        "tls.verify: insecure — the connection will not be validated \
                         at all (docs/ARCHITECTURE.md §13)"
                            .to_string(),
                    );
                }
            }
            ConnField::Log => self.draft.log = !self.draft.log,
            _ => {}
        }
    }

    pub(crate) fn connection_field_text(&self, field: ConnField) -> String {
        match field {
            ConnField::Host => self.draft.host.clone(),
            ConnField::Port => self.draft.port.to_string(),
            ConnField::TlsEnabled => self.draft.tls.enabled.to_string(),
            ConnField::TlsVerify => format!("{:?}", self.draft.tls.verify),
            ConnField::Charset => self.draft.charset.clone(),
            ConnField::Color => self.draft.color.map(|c| c.to_string()).unwrap_or_default(),
            ConnField::LoginName => self
                .draft
                .login
                .as_ref()
                .map(|l| l.name.clone())
                .unwrap_or_default(),
            ConnField::LoginNamePrompt => self
                .draft
                .login
                .as_ref()
                .and_then(|l| l.name_prompt.clone())
                .unwrap_or_default(),
            ConnField::LoginPasswordPrompt => self
                .draft
                .login
                .as_ref()
                .and_then(|l| l.password_prompt.clone())
                .unwrap_or_default(),
            ConnField::Log => self.draft.log.to_string(),
        }
    }

    fn handle_form(&mut self, code: KeyCode) -> EditorAction {
        let Mode::Form { kind, index, field } = self.mode else {
            unreachable!()
        };
        let field_count = kind.fields().len();
        match code {
            KeyCode::Esc => {
                self.cancel_add_if_blank(kind, index);
                self.mode = Mode::Browse;
                EditorAction::Consumed
            }
            KeyCode::Up => {
                if let Mode::Form { field, .. } = &mut self.mode {
                    *field = field.checked_sub(1).unwrap_or(0);
                }
                EditorAction::Consumed
            }
            KeyCode::Down => {
                if let Mode::Form { field, .. } = &mut self.mode {
                    *field = (*field + 1).min(field_count - 1);
                }
                EditorAction::Consumed
            }
            KeyCode::Char(' ') if self.is_toggle_field(kind, field) => {
                self.cycle_rule_toggle(kind, index, field);
                EditorAction::Consumed
            }
            KeyCode::Enter if self.is_toggle_field(kind, field) => {
                self.cycle_rule_toggle(kind, index, field);
                EditorAction::Consumed
            }
            KeyCode::Enter => {
                let current = self.rule_field_text(kind, index, field);
                self.mode = Mode::Input {
                    input: Input::new(current),
                    target: InputTarget::RuleField { kind, index, field },
                };
                EditorAction::Consumed
            }
            _ => EditorAction::Consumed,
        }
    }

    /// A rule added via `a` and abandoned with `Esc` before anything was
    /// typed shouldn't leave a blank stub behind.
    fn cancel_add_if_blank(&mut self, kind: RuleKind, index: usize) {
        let blank = match kind {
            RuleKind::Alias => self
                .draft
                .aliases
                .get(index)
                .is_some_and(|a| a.id.is_none() && a.pattern.is_none() && a.send.is_none()),
            RuleKind::Trigger => self
                .draft
                .triggers
                .get(index)
                .is_some_and(|t| t.id.is_none() && t.pattern.is_none() && t.send.is_none()),
            RuleKind::Timer => self
                .draft
                .timers
                .get(index)
                .is_some_and(|t| t.id.is_none() && t.every.is_none() && t.after.is_none()),
        };
        if !blank {
            return;
        }
        match kind {
            RuleKind::Alias => {
                self.draft.aliases.remove(index);
            }
            RuleKind::Trigger => {
                self.draft.triggers.remove(index);
            }
            RuleKind::Timer => {
                self.draft.timers.remove(index);
            }
        }
    }

    pub(crate) fn is_toggle_field(&self, kind: RuleKind, field: usize) -> bool {
        matches!(kind.fields()[field], "enabled" | "gag" | "bell" | "corpse")
    }

    fn cycle_rule_toggle(&mut self, kind: RuleKind, index: usize, field: usize) {
        let name = kind.fields()[field];
        match (kind, name) {
            (RuleKind::Alias, "enabled") => {
                if let Some(rule) = self.draft.aliases.get_mut(index) {
                    cycle_tri_state(&mut rule.enabled);
                }
            }
            (RuleKind::Trigger, "enabled") => {
                if let Some(rule) = self.draft.triggers.get_mut(index) {
                    cycle_tri_state(&mut rule.enabled);
                }
            }
            (RuleKind::Trigger, "gag") => {
                if let Some(rule) = self.draft.triggers.get_mut(index) {
                    cycle_tri_state(&mut rule.gag);
                }
            }
            (RuleKind::Trigger, "bell") => {
                if let Some(rule) = self.draft.triggers.get_mut(index) {
                    cycle_tri_state(&mut rule.bell);
                }
            }
            (RuleKind::Trigger, "corpse") => {
                if let Some(rule) = self.draft.triggers.get_mut(index) {
                    cycle_tri_state(&mut rule.corpse);
                }
            }
            (RuleKind::Timer, "enabled") => {
                if let Some(rule) = self.draft.timers.get_mut(index) {
                    cycle_tri_state(&mut rule.enabled);
                }
            }
            _ => {}
        }
    }

    /// Toggle fields are read here rather than through `rule_field_text`,
    /// which speaks in strings and so can't tell `no` from "not set".
    pub(crate) fn rule_toggle_value(
        &self,
        kind: RuleKind,
        index: usize,
        field: usize,
    ) -> Option<bool> {
        let name = kind.fields()[field];
        match (kind, name) {
            (RuleKind::Alias, "enabled") => self.draft.aliases.get(index)?.enabled,
            (RuleKind::Trigger, "enabled") => self.draft.triggers.get(index)?.enabled,
            (RuleKind::Trigger, "gag") => self.draft.triggers.get(index)?.gag,
            (RuleKind::Trigger, "bell") => self.draft.triggers.get(index)?.bell,
            (RuleKind::Trigger, "corpse") => self.draft.triggers.get(index)?.corpse,
            (RuleKind::Timer, "enabled") => self.draft.timers.get(index)?.enabled,
            _ => None,
        }
    }

    pub(crate) fn rule_field_text(&self, kind: RuleKind, index: usize, field: usize) -> String {
        let name = kind.fields()[field];
        match kind {
            RuleKind::Alias => {
                let Some(rule) = self.draft.aliases.get(index) else {
                    return String::new();
                };
                match name {
                    "id" => rule.id.clone().unwrap_or_default(),
                    "pattern" => rule.pattern.clone().unwrap_or_default(),
                    "when" => rule.when.clone().unwrap_or_default(),
                    "send" => join_send(&rule.send),
                    _ => String::new(),
                }
            }
            RuleKind::Trigger => {
                let Some(rule) = self.draft.triggers.get(index) else {
                    return String::new();
                };
                match name {
                    "id" => rule.id.clone().unwrap_or_default(),
                    "pattern" => rule.pattern.clone().unwrap_or_default(),
                    "when" => rule.when.clone().unwrap_or_default(),
                    "send" => join_send(&rule.send),
                    "route" => rule.route.clone().unwrap_or_default(),
                    _ => String::new(),
                }
            }
            RuleKind::Timer => {
                let Some(rule) = self.draft.timers.get(index) else {
                    return String::new();
                };
                match name {
                    "id" => rule.id.clone().unwrap_or_default(),
                    "every" => rule.every.clone().unwrap_or_default(),
                    "after" => rule.after.clone().unwrap_or_default(),
                    "send" => join_send(&rule.send),
                    _ => String::new(),
                }
            }
        }
    }

    fn handle_input(&mut self, code: KeyCode, modifiers: KeyModifiers) -> EditorAction {
        match code {
            KeyCode::Esc => {
                self.mode = Mode::Browse;
                self.restore_form_mode_after_input();
                EditorAction::Consumed
            }
            KeyCode::Enter => {
                self.commit_input();
                EditorAction::Consumed
            }
            _ => {
                if let Mode::Input { input, .. } = &mut self.mode {
                    input.handle_event(&Event::Key(crossterm::event::KeyEvent::new(
                        code, modifiers,
                    )));
                }
                EditorAction::Consumed
            }
        }
    }

    /// Cancelling out of editing a rule *field* returns to that rule's Form,
    /// not all the way to Browse — only cancelling the Form itself does that.
    fn restore_form_mode_after_input(&mut self) {
        // Nothing to do: `commit_input`/Esc already leave `mode` how the
        // caller wants except for the rule-field case, handled here.
    }

    fn commit_input(&mut self) {
        let Mode::Input { input, target } = std::mem::replace(&mut self.mode, Mode::Browse) else {
            return;
        };
        let value = input.value().trim().to_string();
        match target {
            InputTarget::Connection(field) => {
                if let Err(err) = self.commit_connection_field(field, &value) {
                    self.set_notice_error(err);
                }
                self.mode = Mode::Browse;
            }
            InputTarget::RuleField { kind, index, field } => {
                if let Err(err) = self.commit_rule_field(kind, index, field, &value) {
                    self.set_notice_error(err);
                }
                self.mode = Mode::Form { kind, index, field };
            }
            InputTarget::NewVariableKey => {
                if value.is_empty() {
                    self.set_notice_error("a variable name can't be empty".to_string());
                    self.mode = Mode::Browse;
                } else {
                    self.mode = Mode::Input {
                        input: Input::default(),
                        target: InputTarget::VariableValue { key: value },
                    };
                }
            }
            InputTarget::VariableValue { key } => {
                self.draft.variables.insert(key, value);
                self.mode = Mode::Browse;
            }
            InputTarget::ModuleEntry { index } => {
                match index {
                    Some(i) if i < self.draft.modules.len() => self.draft.modules[i] = value,
                    _ => self.draft.modules.push(value),
                }
                self.mode = Mode::Browse;
            }
        }
    }

    fn commit_connection_field(&mut self, field: ConnField, value: &str) -> Result<(), String> {
        match field {
            ConnField::Host => {
                if value.is_empty() {
                    return Err("a host is required".to_string());
                }
                self.draft.host = value.to_string();
            }
            ConnField::Port => {
                self.draft.port = value
                    .parse()
                    .map_err(|_| "port must be a number from 1-65535".to_string())?;
            }
            ConnField::Charset => {
                self.draft.charset = if value.is_empty() {
                    "utf-8".to_string()
                } else {
                    value.to_string()
                };
            }
            ConnField::Color => {
                use std::str::FromStr;
                self.draft.color = if value.is_empty() {
                    None
                } else {
                    Some(Color::from_str(value).map_err(|_| {
                        format!("unknown color {value:?}: use a name, #rrggbb, or 0-255")
                    })?)
                };
            }
            ConnField::LoginName => {
                if value.is_empty() {
                    if let Some(login) = &mut self.draft.login
                        && login.name_prompt.is_none()
                        && login.password_prompt.is_none()
                    {
                        self.draft.login = None;
                    } else if let Some(login) = &mut self.draft.login {
                        login.name.clear();
                    }
                } else {
                    match &mut self.draft.login {
                        Some(login) => login.name = value.to_string(),
                        None => {
                            self.draft.login = Some(config::Login {
                                name: value.to_string(),
                                name_prompt: None,
                                password_prompt: None,
                            })
                        }
                    }
                }
            }
            ConnField::LoginNamePrompt | ConnField::LoginPasswordPrompt => {
                let login = self.draft.login.get_or_insert(config::Login {
                    name: String::new(),
                    name_prompt: None,
                    password_prompt: None,
                });
                let target = if field == ConnField::LoginNamePrompt {
                    &mut login.name_prompt
                } else {
                    &mut login.password_prompt
                };
                *target = (!value.is_empty()).then(|| value.to_string());
            }
            ConnField::TlsEnabled | ConnField::TlsVerify | ConnField::Log => {
                unreachable!("toggle fields never open an Input")
            }
        }
        Ok(())
    }

    fn commit_rule_field(
        &mut self,
        kind: RuleKind,
        index: usize,
        field: usize,
        value: &str,
    ) -> Result<(), String> {
        let name = kind.fields()[field];
        if name == "pattern" && !value.is_empty() {
            crate::engine::check_pattern(value, kind == RuleKind::Alias)
                .map_err(|reason| format!("invalid pattern: {reason}"))?;
        }
        if name == "route" && !value.is_empty() && !self.channel_names.iter().any(|c| c == value) {
            return Err(format!(
                "unknown channel `{value}` — one of: {}",
                self.channel_names.join(", ")
            ));
        }
        let opt = (!value.is_empty()).then(|| value.to_string());
        match kind {
            RuleKind::Alias => {
                let Some(rule) = self.draft.aliases.get_mut(index) else {
                    return Ok(());
                };
                match name {
                    "id" => rule.id = opt,
                    "pattern" => rule.pattern = opt,
                    "when" => rule.when = opt,
                    "send" => rule.send = split_send(value),
                    _ => {}
                }
            }
            RuleKind::Trigger => {
                let Some(rule) = self.draft.triggers.get_mut(index) else {
                    return Ok(());
                };
                match name {
                    "id" => rule.id = opt,
                    "pattern" => rule.pattern = opt,
                    "when" => rule.when = opt,
                    "send" => rule.send = split_send(value),
                    "route" => rule.route = opt,
                    _ => {}
                }
            }
            RuleKind::Timer => {
                let Some(rule) = self.draft.timers.get_mut(index) else {
                    return Ok(());
                };
                match name {
                    "id" => rule.id = opt,
                    "every" => rule.every = opt,
                    "after" => rule.after = opt,
                    "send" => rule.send = split_send(value),
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn handle_confirm(&mut self, code: KeyCode) -> EditorAction {
        let Mode::Confirm { action, .. } = std::mem::replace(&mut self.mode, Mode::Browse) else {
            return EditorAction::Consumed;
        };
        match code {
            KeyCode::Char('y' | 'Y') | KeyCode::Enter => self.apply_confirmed(action),
            KeyCode::Char('s' | 'S') if matches!(action, PendingAction::DiscardAndClose) => {
                EditorAction::Save { force: false }
            }
            _ => {
                self.mode = Mode::Browse;
                EditorAction::Consumed
            }
        }
    }

    fn apply_confirmed(&mut self, action: PendingAction) -> EditorAction {
        match action {
            PendingAction::DeleteRule { kind, index } => {
                match kind {
                    RuleKind::Alias if index < self.draft.aliases.len() => {
                        self.draft.aliases.remove(index);
                    }
                    RuleKind::Trigger if index < self.draft.triggers.len() => {
                        self.draft.triggers.remove(index);
                    }
                    RuleKind::Timer if index < self.draft.timers.len() => {
                        self.draft.timers.remove(index);
                    }
                    _ => {}
                }
                self.mode = Mode::Browse;
                EditorAction::Consumed
            }
            PendingAction::DeleteVariable { key } => {
                self.draft.variables.remove(&key);
                self.mode = Mode::Browse;
                EditorAction::Consumed
            }
            PendingAction::DeleteModule { index } => {
                if index < self.draft.modules.len() {
                    self.draft.modules.remove(index);
                }
                self.mode = Mode::Browse;
                EditorAction::Consumed
            }
            PendingAction::DiscardAndClose => EditorAction::Close,
            PendingAction::OverwriteConflict | PendingAction::RecreateVanished => {
                EditorAction::Save { force: true }
            }
        }
    }
}

pub(crate) fn tri_state_text(value: Option<bool>) -> String {
    match value {
        None => "— (inherit from lower layers)".to_string(),
        Some(true) => "yes".to_string(),
        Some(false) => "no".to_string(),
    }
}

fn cycle_tri_state(field: &mut Option<bool>) {
    *field = match *field {
        None => Some(true),
        Some(true) => Some(false),
        Some(false) => None,
    };
}

fn join_send(send: &Option<Vec<String>>) -> String {
    send.as_ref()
        .map(|lines| lines.join(" ;; "))
        .unwrap_or_default()
}

fn split_send(value: &str) -> Option<Vec<String>> {
    if value.is_empty() {
        return None;
    }
    let lines: Vec<String> = value
        .split(";;")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    (!lines.is_empty()).then_some(lines)
}

pub(crate) fn rule_summary(id: Option<&str>, key: Option<&str>) -> String {
    match (id, key) {
        (Some(id), Some(key)) => format!("{id}  {key}"),
        (Some(id), None) => id.to_string(),
        (None, Some(key)) => key.to_string(),
        (None, None) => "(unnamed)".to_string(),
    }
}

fn delete_prompt(noun: &str, id: Option<&str>, key: Option<&str>, advanced: bool) -> String {
    let name = rule_summary(id, key);
    let warning = if advanced {
        " It has advanced fields (script:/send_to:/highlight:) that will be lost."
    } else {
        ""
    };
    format!("Delete {noun} \"{name}\"?{warning} (y/N)")
}

pub(crate) fn has_advanced_alias(rule: &Alias) -> bool {
    rule.send_to.is_some() || rule.set.is_some() || rule.script.is_some() || rule.regex.is_some()
}

pub(crate) fn has_advanced_trigger(rule: &Trigger) -> bool {
    rule.send_to.is_some()
        || rule.set.is_some()
        || rule.script.is_some()
        || rule.highlight.is_some()
        // The editor's `pattern` field is plain (§7.1). A rule that matches
        // by `regex:` instead is edited in the file, and marking it says so
        // rather than showing it as a rule with no pattern at all.
        || rule.regex.is_some()
}

pub(crate) fn has_advanced_timer(rule: &Timer) -> bool {
    rule.set.is_some()
}

fn describe_script(script: &ScriptAction) -> String {
    format!("script      {} :: {}", script.file, script.function)
}

pub(crate) fn advanced_summary_alias(rule: &Alias) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(send_to) = &rule.send_to {
        out.push(format!("send_to     {} session(s)", send_to.len()));
    }
    if let Some(set) = &rule.set {
        out.push(format!("set         {} variable(s)", set.len()));
    }
    if let Some(script) = &rule.script {
        out.push(describe_script(script));
    }
    out
}

pub(crate) fn advanced_summary_trigger(rule: &Trigger) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(send_to) = &rule.send_to {
        out.push(format!("send_to     {} session(s)", send_to.len()));
    }
    if let Some(set) = &rule.set {
        out.push(format!("set         {} variable(s)", set.len()));
    }
    if let Some(script) = &rule.script {
        out.push(describe_script(script));
    }
    if let Some(highlight) = &rule.highlight {
        out.push(format!(
            "highlight   fg {}, {}",
            highlight.fg.as_deref().unwrap_or("(none)"),
            if highlight.whole_line {
                "whole line"
            } else {
                "matched text"
            }
        ));
    }
    out
}

pub(crate) fn advanced_summary_timer(rule: &Timer) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(set) = &rule.set {
        out.push(format!("set         {} variable(s)", set.len()));
    }
    out
}

pub(crate) fn input_label(target: &InputTarget) -> String {
    match target {
        InputTarget::Connection(field) => format!("{}: ", field.label()),
        InputTarget::RuleField { kind, field, .. } => format!("{}: ", kind.fields()[*field]),
        InputTarget::NewVariableKey => "new variable name: ".to_string(),
        InputTarget::VariableValue { key } => format!("{key} = "),
        InputTarget::ModuleEntry { .. } => "module name: ".to_string(),
    }
}

/// The builders the editor's own tests use, and the drawing half's test
/// alongside them: one editor over one throwaway profile on disk.
#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::HashSet;

    use ::test_support::TempDir;

    use super::ConfigEditorState;
    use crate::config::{self, Profile};

    pub(crate) fn minimal(name: &str) -> Profile {
        Profile {
            world: None,
            name: name.to_string(),
            host: "mud.example.org".to_string(),
            port: 4000,
            tls: crate::config::TlsSettings::default(),
            charset: "utf-8".to_string(),
            color: None,
            login: None,
            modules: Vec::new(),
            variables: Default::default(),
            aliases: Vec::new(),
            triggers: Vec::new(),
            timers: Vec::new(),
            scripts: Vec::new(),
            cross_session: None,
            log: false,
        }
    }

    pub(crate) fn open_state(profile: &Profile) -> (TempDir, ConfigEditorState) {
        let dir = TempDir::new();
        let path = dir
            .path()
            .join("profiles")
            .join(format!("{}.yaml", profile.name));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_yaml::to_string(profile).unwrap()).unwrap();
        let file = config::load_profile_file(&path).unwrap();
        let state = ConfigEditorState::open(
            file,
            dir.path().to_path_buf(),
            profile.name.clone(),
            &[],
            HashSet::new(),
            false,
        );
        (dir, state)
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{minimal, open_state};
    use super::*;

    /// The palette offers these settings by name (#43), and its list lives
    /// in `config` rather than beside `ConnField` — the palette is built in
    /// `state`, which must not depend on `ui` (#6). Two lists of the same
    /// fields, so this is what keeps them from drifting apart: a field
    /// added here and not there is a setting the palette cannot find, and
    /// one named there and not here is a setting `/config` would refuse.
    #[test]
    fn every_connection_field_is_offered_in_the_palette() {
        let offered: Vec<&str> = crate::config::CONNECTION_SETTINGS
            .iter()
            .map(|(name, _)| *name)
            .collect();
        let edited: Vec<&str> = CONN_FIELDS.iter().map(|field| field.label()).collect();

        assert_eq!(
            offered, edited,
            "the palette's settings and the editor's fields are the same \
             fields, in the same order"
        );
    }

    #[test]
    fn adding_an_alias_marks_the_editor_dirty_and_opens_its_form() {
        let (_dir, mut state) = open_state(&minimal("kestrel"));
        assert!(!state.dirty());
        state.section = Section::Aliases;
        state.handle_key(KeyCode::Char('a'), KeyModifiers::NONE);
        assert!(state.dirty());
        assert_eq!(state.draft.aliases.len(), 1);
        assert!(matches!(
            state.mode,
            Mode::Form {
                kind: RuleKind::Alias,
                index: 0,
                ..
            }
        ));
    }

    #[test]
    fn cancelling_a_blank_new_alias_removes_it_and_clears_dirty() {
        let (_dir, mut state) = open_state(&minimal("kestrel"));
        state.section = Section::Aliases;
        state.handle_key(KeyCode::Char('a'), KeyModifiers::NONE);
        state.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(state.draft.aliases.is_empty());
        assert!(!state.dirty());
        assert!(matches!(state.mode, Mode::Browse));
    }

    #[test]
    fn editing_a_field_commits_it_into_the_draft() {
        let (_dir, mut state) = open_state(&minimal("kestrel"));
        state.section = Section::Aliases;
        state.draft.aliases.push(Alias::default());
        state.mode = Mode::Form {
            kind: RuleKind::Alias,
            index: 0,
            field: 1,
        }; // pattern
        state.handle_key(KeyCode::Enter, KeyModifiers::NONE); // -> Input
        assert!(matches!(state.mode, Mode::Input { .. }));
        for c in "ll".chars() {
            state.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        state.handle_key(KeyCode::Enter, KeyModifiers::NONE); // commit
        assert_eq!(state.draft.aliases[0].pattern.as_deref(), Some("ll"));
        assert!(matches!(state.mode, Mode::Form { .. }));
    }

    #[test]
    fn an_invalid_pattern_is_rejected_and_the_field_keeps_its_old_value() {
        let (_dir, mut state) = open_state(&minimal("kestrel"));
        state.draft.aliases.push(Alias {
            pattern: Some("look".to_string()),
            ..Default::default()
        });
        state.mode = Mode::Form {
            kind: RuleKind::Alias,
            index: 0,
            field: 1,
        };
        state.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        for c in "{unclosed".chars() {
            state.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        state.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(state.draft.aliases[0].pattern.as_deref(), Some("look"));
        assert_eq!(
            state.notice.as_ref().map(|n| n.level),
            Some(NoticeLevel::Error)
        );
    }
    #[test]
    fn deleting_a_rule_requires_confirmation() {
        let (_dir, mut state) = open_state(&minimal("kestrel"));
        state.section = Section::Triggers;
        state.draft.triggers.push(Trigger {
            regex: Some("hi".to_string()),
            ..Default::default()
        });
        state.handle_key(KeyCode::Char('d'), KeyModifiers::NONE);
        assert!(matches!(state.mode, Mode::Confirm { .. }));
        assert_eq!(state.draft.triggers.len(), 1, "not deleted until confirmed");
        state.handle_key(KeyCode::Char('y'), KeyModifiers::NONE);
        assert!(state.draft.triggers.is_empty());
    }

    #[test]
    fn tri_state_enabled_cycles_inherit_yes_no_inherit() {
        let mut field = None;
        cycle_tri_state(&mut field);
        assert_eq!(field, Some(true));
        cycle_tri_state(&mut field);
        assert_eq!(field, Some(false));
        cycle_tri_state(&mut field);
        assert_eq!(field, None);
    }

    #[test]
    fn a_rule_with_advanced_fields_is_flagged_and_named_on_delete() {
        let (_dir, mut state) = open_state(&minimal("kestrel"));
        state.section = Section::Aliases;
        let mut set = std::collections::BTreeMap::new();
        set.insert("k".to_string(), "v".to_string());
        state.draft.aliases.push(Alias {
            regex: Some("hi".to_string()),
            set: Some(set),
            ..Default::default()
        });
        assert!(has_advanced_alias(&state.draft.aliases[0]));
        state.handle_key(KeyCode::Char('d'), KeyModifiers::NONE);
        let Mode::Confirm { prompt, .. } = &state.mode else {
            panic!("expected a confirmation prompt");
        };
        assert!(prompt.contains("advanced fields"), "{prompt}");
    }

    #[test]
    fn escape_with_unsaved_changes_asks_before_closing() {
        let (_dir, mut state) = open_state(&minimal("kestrel"));
        state.draft.host = "changed.example.org".to_string();
        assert!(matches!(
            state.handle_key(KeyCode::Esc, KeyModifiers::NONE),
            EditorAction::Consumed
        ));
        assert!(matches!(state.mode, Mode::Confirm { .. }));
    }

    #[test]
    fn escape_with_no_changes_closes_immediately() {
        let (_dir, mut state) = open_state(&minimal("kestrel"));
        assert!(matches!(
            state.handle_key(KeyCode::Esc, KeyModifiers::NONE),
            EditorAction::Close
        ));
    }

    #[test]
    fn ctrl_s_asks_app_rs_to_save() {
        let (_dir, mut state) = open_state(&minimal("kestrel"));
        assert!(matches!(
            state.handle_key(KeyCode::Char('s'), KeyModifiers::CONTROL),
            EditorAction::Save { force: false }
        ));
    }

    #[test]
    fn adding_a_variable_is_a_two_step_key_then_value() {
        let (_dir, mut state) = open_state(&minimal("kestrel"));
        state.section = Section::Variables;
        state.handle_key(KeyCode::Char('a'), KeyModifiers::NONE);
        for c in "target".chars() {
            state.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        state.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        for c in "rat".chars() {
            state.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        state.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            state.draft.variables.get("target").map(String::as_str),
            Some("rat")
        );
    }
}
