//! Draws the profile editor (docs/ARCHITECTURE.md §10.2). The editor's
//! model — every mode, cursor and draft edit — is `crate::config_editor`;
//! this half only ever reads it and puts it on the terminal.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::config_editor::{
    CONN_FIELDS, ConfigEditorState, Mode, NoticeLevel, RuleKind, SECTIONS, Section,
    advanced_summary_alias, advanced_summary_timer, advanced_summary_trigger, has_advanced_alias,
    has_advanced_timer, has_advanced_trigger, input_label, rule_summary, tri_state_text,
};

impl ConfigEditorState {
    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        frame.render_widget(Clear, area);

        let dirty_mark = if self.dirty() { " ● unsaved" } else { "" };
        let title = format!(
            " Config — {} ({}){dirty_mark} ",
            self.name,
            self.file.path.display()
        );

        let mut lines: Vec<Line> = Vec::new();
        lines.push(tabs_line(self.section));
        lines.push(Line::raw(""));

        // The input line's row, so the cursor can be placed on it exactly —
        // content length varies (section, notice, mode), so this can't be a
        // fixed offset from either edge of `area` (which is the whole
        // frame, not sized to the content).
        let mut input_row: Option<u16> = None;

        match &self.mode {
            Mode::Browse | Mode::Form { .. } => {
                self.push_section_lines(&mut lines);
            }
            Mode::Input { input, target } => {
                self.push_section_lines(&mut lines);
                lines.push(Line::raw(""));
                input_row = Some(lines.len() as u16);
                lines.push(Line::from(vec![
                    Span::styled(input_label(target), Style::default().bold()),
                    Span::raw(input.value()),
                ]));
            }
            Mode::Confirm { prompt, .. } => {
                lines.push(Line::from(Span::styled(
                    prompt.clone(),
                    Style::default().fg(Color::Yellow),
                )));
            }
        }

        if let Some(notice) = &self.notice {
            lines.push(Line::raw(""));
            // The word, not just the hue (#120): green, amber and red were
            // the only thing separating a failed save from a successful one,
            // so without colour they read identically. Info stays unmarked —
            // it is the "nothing is wrong" case, and labelling that is what
            // teaches the eye to stop reading the label, the same reason the
            // status bar never says "0 errors".
            let (style, prefix) = match notice.level {
                NoticeLevel::Info => (Style::default().fg(Color::Green), ""),
                NoticeLevel::Warn => (Style::default().fg(Color::Yellow), "Warning — "),
                NoticeLevel::Error => (Style::default().fg(Color::Red), "Error — "),
            };
            lines.push(Line::from(Span::styled(
                format!("{prefix}{}", notice.text),
                style,
            )));
        }

        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            self.footer_text(),
            Style::default().add_modifier(Modifier::DIM),
        )));

        let block = Block::bordered().title(title.bold());
        frame.render_widget(Paragraph::new(Text::from(lines)).block(block), area);

        if let (Mode::Input { input, target }, Some(input_row)) = (&self.mode, input_row) {
            let label_len = input_label(target).chars().count();
            let cursor_x = area.x + 1 + label_len as u16 + input.visual_cursor() as u16;
            let cursor_y = area.y + 1 + input_row;
            frame.set_cursor_position((
                cursor_x.min(area.x + area.width.saturating_sub(1)),
                cursor_y.min(area.y + area.height.saturating_sub(1)),
            ));
        }
    }

    fn footer_text(&self) -> String {
        match &self.mode {
            Mode::Browse => "BROWSE  Tab/1-6 section  ↑↓ select  a add  e edit  d delete  \
                 Space enable/disable  Ctrl+S save  Esc close  * = advanced fields kept as-is"
                .to_string(),
            Mode::Form { kind, .. } => format!(
                "EDIT {}  ↑↓ field  Enter edit/toggle  Ctrl+S save  Esc back",
                kind.noun()
            ),
            Mode::Input { .. } => "EDIT  Enter commit  Esc cancel".to_string(),
            Mode::Confirm { .. } => "CONFIRM  y/Enter yes  n/Esc no".to_string(),
        }
    }

    fn push_section_lines(&self, lines: &mut Vec<Line>) {
        match self.section {
            Section::Connection => self.push_connection_lines(lines),
            Section::Variables => self.push_variable_lines(lines),
            Section::Aliases => self.push_rule_list_lines(lines, RuleKind::Alias),
            Section::Triggers => self.push_rule_list_lines(lines, RuleKind::Trigger),
            Section::Timers => self.push_rule_list_lines(lines, RuleKind::Timer),
            Section::Modules => self.push_module_lines(lines),
        }
        if let Mode::Form { kind, index, field } = self.mode {
            lines.push(Line::raw(""));
            self.push_form_lines(lines, kind, index, field);
        }
    }

    fn push_connection_lines(&self, lines: &mut Vec<Line>) {
        let selected = self.cursor[Section::Connection.index()];
        for (i, field) in CONN_FIELDS.iter().enumerate() {
            let marker = if i == selected { "> " } else { "  " };
            let value = self.connection_field_text(*field);
            lines.push(Line::raw(format!("{marker}{:<20}{value}", field.label())));
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "name is read-only here — renaming means moving the file \
             (touches the keyring account and log file too)",
            Style::default().add_modifier(Modifier::DIM),
        ));
        lines.push(Line::styled(
            "password lives in the OS keyring — mudular --set-password <profile>",
            Style::default().add_modifier(Modifier::DIM),
        ));
    }

    fn push_variable_lines(&self, lines: &mut Vec<Line>) {
        let selected = self.cursor[Section::Variables.index()];
        if self.draft.variables.is_empty() {
            lines.push(Line::styled(
                "(no variables — press `a` to add one)",
                Style::default().add_modifier(Modifier::DIM),
            ));
            return;
        }
        for (i, (key, value)) in self.draft.variables.iter().enumerate() {
            let marker = if i == selected { "> " } else { "  " };
            lines.push(Line::raw(format!("{marker}{key} = {value}")));
        }
    }

    fn push_module_lines(&self, lines: &mut Vec<Line>) {
        let selected = self.cursor[Section::Modules.index()];
        if self.draft.modules.is_empty() {
            lines.push(Line::styled(
                "(no modules — press `a` to add one)",
                Style::default().add_modifier(Modifier::DIM),
            ));
            return;
        }
        for (i, name) in self.draft.modules.iter().enumerate() {
            let marker = if i == selected { "> " } else { "  " };
            let missing = if self.known_modules.contains(name) {
                ""
            } else {
                " (not found)"
            };
            lines.push(Line::raw(format!("{marker}{name}{missing}")));
        }
    }

    fn push_rule_list_lines(&self, lines: &mut Vec<Line>, kind: RuleKind) {
        let section = match kind {
            RuleKind::Alias => Section::Aliases,
            RuleKind::Trigger => Section::Triggers,
            RuleKind::Timer => Section::Timers,
        };
        let selected = self.cursor[section.index()];
        let rows: Vec<(String, bool)> = match kind {
            RuleKind::Alias => self
                .draft
                .aliases
                .iter()
                .map(|r| {
                    (
                        rule_summary(r.id.as_deref(), r.pattern.as_deref().or(r.regex.as_deref())),
                        has_advanced_alias(r),
                    )
                })
                .collect(),
            RuleKind::Trigger => self
                .draft
                .triggers
                .iter()
                .map(|r| {
                    (
                        rule_summary(r.id.as_deref(), r.pattern.as_deref().or(r.regex.as_deref())),
                        has_advanced_trigger(r),
                    )
                })
                .collect(),
            RuleKind::Timer => self
                .draft
                .timers
                .iter()
                .map(|r| {
                    (
                        rule_summary(r.id.as_deref(), r.every.as_deref().or(r.after.as_deref())),
                        has_advanced_timer(r),
                    )
                })
                .collect(),
        };
        if rows.is_empty() {
            lines.push(Line::styled(
                format!("(no {}s — press `a` to add one)", kind.noun()),
                Style::default().add_modifier(Modifier::DIM),
            ));
            return;
        }
        for (i, (summary, advanced)) in rows.iter().enumerate() {
            let marker = if i == selected { "> " } else { "  " };
            let star = if *advanced { "* " } else { "  " };
            lines.push(Line::raw(format!("{marker}{star}{summary}")));
        }
    }

    fn push_form_lines(&self, lines: &mut Vec<Line>, kind: RuleKind, index: usize, field: usize) {
        for (i, name) in kind.fields().iter().enumerate() {
            let marker = if i == field { "> " } else { "  " };
            let display = if self.is_toggle_field(kind, i) {
                tri_state_text(self.rule_toggle_value(kind, index, i))
            } else {
                self.rule_field_text(kind, index, i)
            };
            lines.push(Line::raw(format!("{marker}{name:<10}{display}")));
        }
        let advanced = match kind {
            RuleKind::Alias => self.draft.aliases.get(index).map(advanced_summary_alias),
            RuleKind::Trigger => self.draft.triggers.get(index).map(advanced_summary_trigger),
            RuleKind::Timer => self.draft.timers.get(index).map(advanced_summary_timer),
        }
        .unwrap_or_default();
        if !advanced.is_empty() {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "Advanced — kept as written, edit the file to change:",
                Style::default().add_modifier(Modifier::DIM),
            ));
            for row in advanced {
                lines.push(Line::styled(
                    format!("  {row}"),
                    Style::default().add_modifier(Modifier::DIM),
                ));
            }
        }
    }
}

fn tabs_line(current: Section) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, section) in SECTIONS.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" │ "));
        }
        let title = section.title();
        if *section == current {
            spans.push(Span::styled(
                format!("[{title}]"),
                Style::default().bold().fg(Color::Cyan),
            ));
        } else {
            spans.push(Span::raw(title));
        }
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_editor::{
        Notice,
        test_support::{minimal, open_state},
    };
    use crate::engine::Trigger;

    /// #120: the three notice levels were green, amber and red and nothing
    /// else, so without colour a failure to save and a confirmation that it
    /// saved read identically. The level is the one signal here that colour
    /// carried alone.
    ///
    /// Info stays unmarked on purpose: it is the "nothing is wrong" case,
    /// and the status bar already declines to say "0 errors" for the same
    /// reason — labelling the absence of a problem teaches the eye to skip
    /// the label.
    #[test]
    fn a_notice_says_which_kind_it_is_without_relying_on_colour() {
        let render = |state: &ConfigEditorState| {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
            terminal
                .draw(|frame| state.draw(frame, frame.area()))
                .unwrap();
            let buffer = terminal.backend().buffer().clone();
            let area = buffer.area;
            (0..area.height)
                .map(|y| {
                    (0..area.width)
                        .map(|x| buffer.cell((x, y)).unwrap().symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        let (_dir, mut state) = open_state(&minimal("kestrel"));

        state.set_notice_error("could not save".to_string());
        let screen = render(&state);
        assert!(screen.contains("Error"), "an error should say so: {screen}");
        assert!(screen.contains("could not save"), "{screen}");

        state.notice = Some(Notice {
            level: NoticeLevel::Warn,
            text: "this file has comments".to_string(),
        });
        let screen = render(&state);
        assert!(
            screen.contains("Warning"),
            "a warning should say so: {screen}"
        );

        state.set_notice_info("saved".to_string());
        let screen = render(&state);
        assert!(screen.contains("saved"), "{screen}");
        assert!(
            !screen.contains("Error") && !screen.contains("Warning"),
            "an info notice should not wear another level's word: {screen}"
        );
    }

    #[test]
    fn a_trigger_form_shows_each_toggle_state_it_is_actually_in() {
        let (_dir, state) = {
            let mut profile = minimal("kestrel");
            profile.triggers.push(Trigger {
                gag: Some(true),
                bell: Some(false),
                corpse: None,
                enabled: Some(true),
                ..Default::default()
            });
            open_state(&profile)
        };
        let mut lines = Vec::new();
        state.push_form_lines(&mut lines, RuleKind::Trigger, 0, 0);
        let text: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        let field = |name: &str| {
            text.iter()
                .find(|l| l[2..].starts_with(name))
                .unwrap_or_else(|| panic!("no `{name}` row in {text:?}"))
                .clone()
        };
        assert!(field("gag").ends_with("yes"), "{:?}", field("gag"));
        assert!(field("bell").ends_with("no"), "{:?}", field("bell"));
        assert!(field("corpse").contains("inherit"), "{:?}", field("corpse"));
        assert!(field("enabled").ends_with("yes"), "{:?}", field("enabled"));
    }
}
