//! Automation engine: triggers, aliases, variables, and timers.
//!
//! Rules arrive as ordered scope layers (global → shared modules → profile
//! overrides, docs/ARCHITECTURE.md §7.3), are merged, then compiled into a
//! per-session [`Engine`]. All matching uses the `regex` crate:
//! Unicode-aware and linear-time, so hostile server output cannot stall the
//! client.
//!
//! Every rule field is optional so a later layer can *patch* an earlier
//! one — that is what makes `enabled: false` disable an inherited rule
//! "without redefining it". A patch keeps the shadowed rule's position, so
//! overriding a rule never silently reorders when it fires.
//!
//! Sans-IO: no files, no sockets, no async. Timers report when they are
//! next due; the session task owns the actual sleeping.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use regex::{Captures, Regex};
use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleModule {
    /// Label used in error messages; loaders set it from the file name.
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub variables: HashMap<String, String>,
    #[serde(default)]
    pub aliases: Vec<Alias>,
    #[serde(default)]
    pub triggers: Vec<Trigger>,
    #[serde(default)]
    pub timers: Vec<Timer>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Alias {
    /// Stable identity for shadowing across scope layers.
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub pattern: Option<String>,
    #[serde(default)]
    pub send: Option<Vec<String>>,
    #[serde(default)]
    pub set: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Trigger {
    /// Stable identity for shadowing across scope layers.
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub pattern: Option<String>,
    #[serde(default)]
    pub send: Option<Vec<String>>,
    #[serde(default)]
    pub set: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub gag: Option<bool>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Timer {
    /// Stable identity for shadowing across scope layers.
    #[serde(default)]
    pub id: Option<String>,
    /// Repeat on this interval, e.g. `60s`, `5m`.
    #[serde(default)]
    pub every: Option<String>,
    /// Fire once after this delay.
    #[serde(default)]
    pub after: Option<String>,
    #[serde(default)]
    pub send: Option<Vec<String>>,
    #[serde(default)]
    pub set: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("invalid pattern `{pattern}` in `{module}`: {source}")]
    BadPattern {
        module: String,
        pattern: String,
        source: regex::Error,
    },
    #[error("rule in `{module}` needs an `id` or a `pattern`")]
    RuleWithoutIdentity { module: String },
    #[error("`{module}` refers to unknown rule id `{id}`; define it before overriding it")]
    UnknownRule { module: String, id: String },
    #[error("timer `{id}` in `{module}` needs `every` or `after`")]
    TimerWithoutSchedule { module: String, id: String },
    #[error("invalid duration `{value}` in `{module}`: {reason}")]
    BadDuration {
        module: String,
        value: String,
        reason: String,
    },
}

/// What a matched line asks the session to do.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LineOutcome {
    /// Keep this line out of the scrollback entirely.
    pub gag: bool,
    /// Commands to send to the server, in trigger order.
    pub sends: Vec<String>,
}

#[derive(Debug)]
struct CompiledRule {
    regex: Regex,
    sends: Vec<String>,
    set: Vec<(String, String)>,
    gag: bool,
}

#[derive(Debug)]
struct CompiledTimer {
    interval: Duration,
    repeat: bool,
    sends: Vec<String>,
    set: Vec<(String, String)>,
    next: Option<Instant>,
}

#[derive(Debug, Default)]
pub struct Engine {
    aliases: Vec<CompiledRule>,
    triggers: Vec<CompiledRule>,
    timers: Vec<CompiledTimer>,
    variables: HashMap<String, String>,
}

impl Engine {
    /// Merge ordered scope layers and compile the result. Earlier layers
    /// are lower precedence (§7.3).
    pub fn compile(layers: &[RuleModule]) -> Result<Self, EngineError> {
        let mut variables = HashMap::new();
        let mut aliases: Vec<Alias> = Vec::new();
        let mut triggers: Vec<Trigger> = Vec::new();
        let mut timers: Vec<Timer> = Vec::new();

        for layer in layers {
            variables.extend(layer.variables.clone());
            merge_layer(&mut aliases, &layer.aliases, &layer.name)?;
            merge_layer(&mut triggers, &layer.triggers, &layer.name)?;
            merge_layer(&mut timers, &layer.timers, &layer.name)?;
        }

        Ok(Engine {
            aliases: compile_rules(&aliases)?,
            triggers: compile_rules(&triggers)?,
            timers: compile_timers(&timers)?,
            variables,
        })
    }

    /// Run triggers against a completed inbound line, ANSI already
    /// stripped (§7.1). Takes `&mut self` because a trigger may `set:` a
    /// variable that later rules and sends can read.
    pub fn process_line(&mut self, line: &str) -> LineOutcome {
        let mut outcome = LineOutcome::default();
        let mut updates: Vec<(String, String)> = Vec::new();

        for rule in &self.triggers {
            let Some(caps) = rule.regex.captures(line) else {
                continue;
            };
            for template in &rule.sends {
                outcome
                    .sends
                    .push(expand(template, Some(&caps), &self.variables));
            }
            for (name, template) in &rule.set {
                updates.push((name.clone(), expand(template, Some(&caps), &self.variables)));
            }
            outcome.gag |= rule.gag;
        }

        self.variables.extend(updates);
        outcome
    }

    /// Turn one typed input line into the commands to send: split on `;`,
    /// then expand each part through aliases. Alias output is never
    /// re-expanded, so aliases cannot recurse into each other.
    pub fn expand_input(&mut self, input: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut updates: Vec<(String, String)> = Vec::new();

        for part in input.split(';') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            match self
                .aliases
                .iter()
                .find_map(|rule| rule.regex.captures(part).map(|caps| (rule, caps)))
            {
                Some((rule, caps)) => {
                    for template in &rule.sends {
                        out.push(expand(template, Some(&caps), &self.variables));
                    }
                    for (name, template) in &rule.set {
                        updates
                            .push((name.clone(), expand(template, Some(&caps), &self.variables)));
                    }
                }
                None => out.push(part.to_string()),
            }
        }

        self.variables.extend(updates);
        out
    }

    /// Arm every timer relative to `now`. The session calls this once the
    /// connection is up.
    pub fn start_timers(&mut self, now: Instant) {
        for timer in &mut self.timers {
            timer.next = Some(now + timer.interval);
        }
    }

    /// When the earliest armed timer is due, if any.
    pub fn next_timer_deadline(&self) -> Option<Instant> {
        self.timers.iter().filter_map(|timer| timer.next).min()
    }

    /// Fire every timer due at `now`, returning the commands to send.
    /// Repeating timers re-arm from `now` rather than from their previous
    /// deadline, so a stalled session cannot build up a burst of catch-up
    /// firings.
    pub fn fire_due_timers(&mut self, now: Instant) -> Vec<String> {
        let mut sends = Vec::new();
        let mut updates: Vec<(String, String)> = Vec::new();

        for timer in &mut self.timers {
            if timer.next.is_none_or(|next| next > now) {
                continue;
            }
            for template in &timer.sends {
                sends.push(expand(template, None, &self.variables));
            }
            for (name, template) in &timer.set {
                updates.push((name.clone(), expand(template, None, &self.variables)));
            }
            timer.next = timer.repeat.then(|| now + timer.interval);
        }

        self.variables.extend(updates);
        sends
    }

    #[cfg(test)]
    fn variable(&self, name: &str) -> Option<&str> {
        self.variables.get(name).map(String::as_str)
    }
}

/// Merges one layer over the accumulator. A rule shadows an existing one
/// when their `id`s match, or — when neither carries an `id` — when their
/// patterns match exactly (§7.3). The shadowed rule keeps its position.
fn merge_layer<T: Layered>(
    acc: &mut Vec<T>,
    incoming: &[T],
    module: &str,
) -> Result<(), EngineError> {
    for rule in incoming {
        if !rule.has_identity() {
            return Err(rule.identity_error(module));
        }

        match acc.iter().position(|existing| same_rule(existing, rule)) {
            Some(pos) => {
                let mut patched = rule.clone();
                patched.fill_from(&acc[pos]);
                acc[pos] = patched;
            }
            // Nothing to inherit from, and not enough to stand alone: this
            // is an override of a rule that was never defined.
            None if !rule.is_definition() => {
                return Err(EngineError::UnknownRule {
                    module: module.to_string(),
                    id: rule.id().unwrap_or_default().to_string(),
                });
            }
            None => acc.push(rule.clone()),
        }
    }
    Ok(())
}

/// An explicit `id` is authoritative: two rules that both name an id are
/// the same rule only if the ids match, even if their patterns coincide.
fn same_rule<T: Layered>(existing: &T, incoming: &T) -> bool {
    match (existing.id(), incoming.id()) {
        (Some(a), Some(b)) => a == b,
        _ => match (existing.pattern(), incoming.pattern()) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        },
    }
}

/// A rule that participates in scope layering. `fill_from` inherits every
/// field this rule left unset from the rule it shadows.
///
/// Aliases and triggers are identified by `id` or `pattern`; timers have no
/// pattern, so only an `id` can shadow one. `is_definition` says whether a
/// rule carries enough to stand on its own — if not, it must be patching
/// something an earlier layer defined.
trait Layered: Clone {
    fn id(&self) -> Option<&str>;
    fn pattern(&self) -> Option<&str>;
    fn has_identity(&self) -> bool;
    fn is_definition(&self) -> bool;
    fn identity_error(&self, module: &str) -> EngineError;
    fn fill_from(&mut self, base: &Self);
}

impl Layered for Alias {
    fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }
    fn pattern(&self) -> Option<&str> {
        self.pattern.as_deref()
    }
    fn has_identity(&self) -> bool {
        self.id.is_some() || self.pattern.is_some()
    }
    fn is_definition(&self) -> bool {
        self.pattern.is_some()
    }
    fn identity_error(&self, module: &str) -> EngineError {
        EngineError::RuleWithoutIdentity {
            module: module.to_string(),
        }
    }
    fn fill_from(&mut self, base: &Self) {
        self.id = self.id.take().or_else(|| base.id.clone());
        self.pattern = self.pattern.take().or_else(|| base.pattern.clone());
        self.send = self.send.take().or_else(|| base.send.clone());
        self.set = self.set.take().or_else(|| base.set.clone());
        self.enabled = self.enabled.or(base.enabled);
    }
}

impl Layered for Trigger {
    fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }
    fn pattern(&self) -> Option<&str> {
        self.pattern.as_deref()
    }
    fn has_identity(&self) -> bool {
        self.id.is_some() || self.pattern.is_some()
    }
    fn is_definition(&self) -> bool {
        self.pattern.is_some()
    }
    fn identity_error(&self, module: &str) -> EngineError {
        EngineError::RuleWithoutIdentity {
            module: module.to_string(),
        }
    }
    fn fill_from(&mut self, base: &Self) {
        self.id = self.id.take().or_else(|| base.id.clone());
        self.pattern = self.pattern.take().or_else(|| base.pattern.clone());
        self.send = self.send.take().or_else(|| base.send.clone());
        self.set = self.set.take().or_else(|| base.set.clone());
        self.gag = self.gag.or(base.gag);
        self.enabled = self.enabled.or(base.enabled);
    }
}

impl Layered for Timer {
    fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }
    /// Timers have no pattern, so only an explicit `id` can shadow one;
    /// an id-less timer is always a new one.
    fn pattern(&self) -> Option<&str> {
        None
    }
    fn has_identity(&self) -> bool {
        self.id.is_some() || self.every.is_some() || self.after.is_some()
    }
    fn is_definition(&self) -> bool {
        self.every.is_some() || self.after.is_some()
    }
    fn identity_error(&self, module: &str) -> EngineError {
        EngineError::TimerWithoutSchedule {
            module: module.to_string(),
            id: self.id.clone().unwrap_or_default(),
        }
    }
    fn fill_from(&mut self, base: &Self) {
        self.id = self.id.take().or_else(|| base.id.clone());
        self.every = self.every.take().or_else(|| base.every.clone());
        self.after = self.after.take().or_else(|| base.after.clone());
        self.send = self.send.take().or_else(|| base.send.clone());
        self.set = self.set.take().or_else(|| base.set.clone());
        self.enabled = self.enabled.or(base.enabled);
    }
}

trait CompilableRule {
    fn id_label(&self) -> String;
    fn pattern_str(&self) -> Option<&str>;
    fn enabled(&self) -> bool;
    fn sends(&self) -> Vec<String>;
    fn sets(&self) -> Vec<(String, String)>;
    fn gag(&self) -> bool;
}

impl CompilableRule for Alias {
    fn id_label(&self) -> String {
        self.id.clone().unwrap_or_default()
    }
    fn pattern_str(&self) -> Option<&str> {
        self.pattern.as_deref()
    }
    fn enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }
    fn sends(&self) -> Vec<String> {
        self.send.clone().unwrap_or_default()
    }
    fn sets(&self) -> Vec<(String, String)> {
        self.set.clone().unwrap_or_default().into_iter().collect()
    }
    fn gag(&self) -> bool {
        false
    }
}

impl CompilableRule for Trigger {
    fn id_label(&self) -> String {
        self.id.clone().unwrap_or_default()
    }
    fn pattern_str(&self) -> Option<&str> {
        self.pattern.as_deref()
    }
    fn enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }
    fn sends(&self) -> Vec<String> {
        self.send.clone().unwrap_or_default()
    }
    fn sets(&self) -> Vec<(String, String)> {
        self.set.clone().unwrap_or_default().into_iter().collect()
    }
    fn gag(&self) -> bool {
        self.gag.unwrap_or(false)
    }
}

fn compile_rules<T: CompilableRule>(rules: &[T]) -> Result<Vec<CompiledRule>, EngineError> {
    let mut out = Vec::new();
    for rule in rules.iter().filter(|rule| rule.enabled()) {
        let pattern = rule.pattern_str().ok_or_else(|| EngineError::UnknownRule {
            module: "merged rules".to_string(),
            id: rule.id_label(),
        })?;
        out.push(CompiledRule {
            regex: Regex::new(pattern).map_err(|source| EngineError::BadPattern {
                module: "merged rules".to_string(),
                pattern: pattern.to_string(),
                source,
            })?,
            sends: rule.sends(),
            set: rule.sets(),
            gag: rule.gag(),
        });
    }
    Ok(out)
}

fn compile_timers(timers: &[Timer]) -> Result<Vec<CompiledTimer>, EngineError> {
    let mut out = Vec::new();
    for timer in timers.iter().filter(|t| t.enabled.unwrap_or(true)) {
        let label = timer.id.clone().unwrap_or_default();
        let (value, repeat) = match (&timer.every, &timer.after) {
            (Some(every), _) => (every, true),
            (None, Some(after)) => (after, false),
            (None, None) => {
                return Err(EngineError::TimerWithoutSchedule {
                    module: "merged rules".to_string(),
                    id: label,
                });
            }
        };
        let interval = parse_duration(value).map_err(|reason| EngineError::BadDuration {
            module: "merged rules".to_string(),
            value: value.clone(),
            reason,
        })?;
        out.push(CompiledTimer {
            interval,
            repeat,
            sends: timer.send.clone().unwrap_or_default(),
            set: timer.set.clone().unwrap_or_default().into_iter().collect(),
            next: None,
        });
    }
    Ok(out)
}

/// `500ms`, `30s`, `5m`, `2h`. A unit is required: a bare number is too
/// easy to read as the wrong scale.
fn parse_duration(value: &str) -> Result<Duration, String> {
    let value = value.trim();
    let (digits, unit) = value.split_at(
        value
            .find(|c: char| !c.is_ascii_digit())
            .ok_or_else(|| "missing a unit (ms, s, m, h)".to_string())?,
    );
    if digits.is_empty() {
        return Err("missing a number".to_string());
    }
    let amount: u64 = digits
        .parse()
        .map_err(|_| "number is too large".to_string())?;
    let millis = match unit {
        "ms" => amount,
        "s" => amount * 1_000,
        "m" => amount * 60_000,
        "h" => amount * 3_600_000,
        other => return Err(format!("unknown unit `{other}` (expected ms, s, m, h)")),
    };
    Ok(Duration::from_millis(millis))
}

/// Substitutes `${...}` in a send/set template: numbered and named regex
/// captures first, then the variable store (§7.1). An unresolved name is
/// left verbatim so a typo is visible rather than silently blank.
fn expand(template: &str, caps: Option<&Captures>, vars: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;

    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            // Unterminated: nothing to substitute, keep it as typed.
            out.push_str(&rest[start..]);
            return out;
        };
        let name = &after[..end];
        match lookup(name, caps, vars) {
            Some(value) => out.push_str(&value),
            None => {
                out.push_str("${");
                out.push_str(name);
                out.push('}');
            }
        }
        rest = &after[end + 1..];
    }

    out.push_str(rest);
    out
}

fn lookup(name: &str, caps: Option<&Captures>, vars: &HashMap<String, String>) -> Option<String> {
    if let Some(caps) = caps {
        if let Ok(index) = name.parse::<usize>() {
            return caps.get(index).map(|m| m.as_str().to_string());
        }
        if let Some(m) = caps.name(name) {
            return Some(m.as_str().to_string());
        }
    }
    vars.get(name).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn module(yaml: &str) -> RuleModule {
        serde_yaml::from_str(yaml).expect("valid test YAML")
    }

    fn engine(yaml: &str) -> Engine {
        Engine::compile(&[module(yaml)]).expect("compiles")
    }

    #[test]
    fn trigger_matches_with_named_capture_substitution() {
        let mut engine = engine(
            r#"
            name: test
            triggers:
              - pattern: '^(?P<who>\p{L}+) has arrived\.$'
                send: ["look ${who}"]
            "#,
        );
        assert_eq!(
            engine.process_line("Ærlend has arrived.").sends,
            vec!["look Ærlend"]
        );
        assert!(engine.process_line("nothing here").sends.is_empty());
    }

    #[test]
    fn alias_expands_or_passes_through() {
        let mut engine = engine(
            r#"
            name: test
            aliases:
              - pattern: '^gh (.+)$'
                send: ["get ${1}", "wear ${1}"]
            "#,
        );
        assert_eq!(
            engine.expand_input("gh 帽子"),
            vec!["get 帽子", "wear 帽子"]
        );
        assert_eq!(engine.expand_input("look"), vec!["look"]);
    }

    /// The `${target}` in §7.2's own example is a *variable*, not a capture
    /// group — templates have to consult the variable store too.
    #[test]
    fn templates_substitute_variables_as_well_as_captures() {
        let mut engine = engine(
            r#"
            name: test
            variables:
              target: rat
            aliases:
              - pattern: '^hh$'
                send: ["cast heal ${target}"]
            "#,
        );
        assert_eq!(engine.expand_input("hh"), vec!["cast heal rat"]);
    }

    #[test]
    fn unresolved_placeholders_stay_visible() {
        let mut engine = engine(
            r#"
            name: test
            aliases:
              - pattern: '^t$'
                send: ["kill ${nope}"]
            "#,
        );
        assert_eq!(engine.expand_input("t"), vec!["kill ${nope}"]);
    }

    #[test]
    fn triggers_can_set_variables_for_later_use() {
        let mut engine = engine(
            r#"
            name: test
            triggers:
              - pattern: '^You are fighting (?P<foe>\w+)'
                set:
                  target: "${foe}"
            aliases:
              - pattern: '^k$'
                send: ["kill ${target}"]
            "#,
        );
        engine.process_line("You are fighting kobold");
        assert_eq!(engine.variable("target"), Some("kobold"));
        assert_eq!(engine.expand_input("k"), vec!["kill kobold"]);
    }

    #[test]
    fn input_splits_on_semicolons_and_expands_each_part() {
        let mut engine = engine(
            r#"
            name: test
            aliases:
              - pattern: '^gh (.+)$'
                send: ["get ${1}", "wear ${1}"]
            "#,
        );
        assert_eq!(
            engine.expand_input("n; gh sword ;n"),
            vec!["n", "get sword", "wear sword", "n"]
        );
        assert_eq!(engine.expand_input(";;"), Vec::<String>::new());
    }

    /// Alias output is not re-expanded, so two aliases cannot bounce off
    /// each other forever.
    #[test]
    fn alias_output_is_not_re_expanded() {
        let mut engine = engine(
            r#"
            name: test
            aliases:
              - pattern: '^a$'
                send: ["b"]
              - pattern: '^b$'
                send: ["a"]
            "#,
        );
        assert_eq!(engine.expand_input("a"), vec!["b"]);
    }

    #[test]
    fn gagged_lines_are_reported_once() {
        let mut engine = engine(
            r#"
            name: test
            triggers:
              - pattern: 'spam'
                gag: true
            "#,
        );
        let outcome = engine.process_line("noisy spam line");
        assert!(outcome.gag);
        assert!(!engine.process_line("quiet line").gag);
    }

    #[test]
    fn disabled_rules_are_skipped() {
        let mut engine = engine(
            r#"
            name: test
            triggers:
              - pattern: 'x'
                send: ["y"]
                enabled: false
            "#,
        );
        assert!(engine.process_line("x").sends.is_empty());
    }

    #[test]
    fn bad_pattern_reports_context() {
        let err = Engine::compile(&[module(
            r#"
            name: broken
            triggers:
              - pattern: '('
            "#,
        )])
        .unwrap_err();
        assert!(err.to_string().contains('('), "{err}");
    }

    // ---- scope merge (§7.3) ----

    fn layers(yaml: &[&str]) -> Engine {
        let modules: Vec<RuleModule> = yaml.iter().map(|y| module(y)).collect();
        Engine::compile(&modules).expect("compiles")
    }

    #[test]
    fn later_layers_override_variables() {
        let mut engine = layers(&[
            "name: global\nvariables:\n  target: rat\n  heal_at: '40'\n",
            "name: profile\nvariables:\n  target: dragon\n",
        ]);
        engine.process_line("x");
        assert_eq!(engine.variable("target"), Some("dragon"));
        assert_eq!(engine.variable("heal_at"), Some("40"));
    }

    #[test]
    fn a_later_layer_shadows_by_id_keeping_position() {
        let mut engine = layers(&[
            r#"
            name: module
            triggers:
              - id: greet
                pattern: 'hello'
                send: ["wave"]
              - pattern: 'bye'
                send: ["farewell"]
            "#,
            r#"
            name: profile
            triggers:
              - id: greet
                pattern: 'hello'
                send: ["bow"]
            "#,
        ]);
        assert_eq!(engine.process_line("hello").sends, vec!["bow"]);
        assert_eq!(engine.process_line("bye").sends, vec!["farewell"]);

        // Overriding must not move the rule to the end: a line matching
        // both rules still fires them in the original order.
        let mut engine = layers(&[
            r#"
            name: module
            triggers:
              - id: first
                pattern: 'x'
                send: ["one"]
              - id: second
                pattern: 'x'
                send: ["two"]
            "#,
            r#"
            name: profile
            triggers:
              - id: first
                pattern: 'x'
                send: ["ONE"]
            "#,
        ]);
        assert_eq!(engine.process_line("x").sends, vec!["ONE", "two"]);
    }

    #[test]
    fn a_later_layer_shadows_by_exact_pattern_when_no_id() {
        let mut engine = layers(&[
            "name: module\ntriggers:\n  - pattern: 'hello'\n    send: [\"wave\"]\n",
            "name: profile\ntriggers:\n  - pattern: 'hello'\n    send: [\"bow\"]\n",
        ]);
        assert_eq!(engine.process_line("hello").sends, vec!["bow"]);
    }

    /// An explicit id is authoritative: same pattern but different ids are
    /// deliberately two different rules.
    #[test]
    fn different_ids_with_the_same_pattern_stay_separate() {
        let mut engine = layers(&[r#"
            name: module
            triggers:
              - id: a
                pattern: 'x'
                send: ["one"]
              - id: b
                pattern: 'x'
                send: ["two"]
            "#]);
        assert_eq!(engine.process_line("x").sends, vec!["one", "two"]);
    }

    /// The headline case from §7.3: turn off an inherited rule without
    /// repeating its pattern.
    #[test]
    fn a_later_layer_disables_an_inherited_rule_by_id() {
        let mut engine = layers(&[
            r#"
            name: module
            triggers:
              - id: autoloot
                pattern: 'is DEAD'
                send: ["get all corpse"]
            "#,
            r#"
            name: profile
            triggers:
              - id: autoloot
                enabled: false
            "#,
        ]);
        assert!(engine.process_line("the kobold is DEAD!").sends.is_empty());
    }

    #[test]
    fn a_later_layer_can_re_enable_a_disabled_rule() {
        let mut engine = layers(&[
            r#"
            name: global
            triggers:
              - id: autoloot
                pattern: 'is DEAD'
                send: ["get all corpse"]
                enabled: false
            "#,
            r#"
            name: profile
            triggers:
              - id: autoloot
                enabled: true
            "#,
        ]);
        assert_eq!(
            engine.process_line("the kobold is DEAD!").sends,
            vec!["get all corpse"]
        );
    }

    /// A patch inherits everything it does not restate — here the pattern.
    #[test]
    fn a_patch_can_change_only_the_commands() {
        let mut engine = layers(&[
            r#"
            name: module
            triggers:
              - id: greet
                pattern: '^(?P<who>\w+) waves'
                send: ["wave ${who}"]
            "#,
            r#"
            name: profile
            triggers:
              - id: greet
                send: ["bow ${who}"]
            "#,
        ]);
        assert_eq!(engine.process_line("Bob waves").sends, vec!["bow Bob"]);
    }

    #[test]
    fn overriding_an_unknown_id_is_an_error() {
        let err = Engine::compile(&[module(
            "name: profile\ntriggers:\n  - id: nope\n    enabled: false\n",
        )])
        .unwrap_err();
        assert!(err.to_string().contains("nope"), "{err}");
    }

    #[test]
    fn a_rule_with_neither_id_nor_pattern_is_an_error() {
        let err = Engine::compile(&[module("name: profile\ntriggers:\n  - send: [\"x\"]\n")])
            .unwrap_err();
        assert!(
            err.to_string().contains("needs an `id` or a `pattern`"),
            "{err}"
        );
    }

    #[test]
    fn aliases_and_triggers_have_separate_id_namespaces() {
        let mut engine = layers(&[r#"
            name: module
            aliases:
              - id: shared
                pattern: '^x$'
                send: ["alias fired"]
            triggers:
              - id: shared
                pattern: 'x'
                send: ["trigger fired"]
            "#]);
        assert_eq!(engine.expand_input("x"), vec!["alias fired"]);
        assert_eq!(engine.process_line("x").sends, vec!["trigger fired"]);
    }

    // ---- timers (§7.1) ----

    #[test]
    fn repeating_timers_fire_on_their_interval() {
        let mut engine = engine(
            r#"
            name: test
            timers:
              - every: 60s
                send: ["save"]
            "#,
        );
        let start = Instant::now();
        engine.start_timers(start);

        assert!(engine.fire_due_timers(start).is_empty(), "not due yet");
        assert_eq!(
            engine.fire_due_timers(start + Duration::from_secs(60)),
            vec!["save"]
        );
        // Re-armed, so it fires again an interval later.
        assert!(
            engine
                .fire_due_timers(start + Duration::from_secs(61))
                .is_empty()
        );
        assert_eq!(
            engine.fire_due_timers(start + Duration::from_secs(120)),
            vec!["save"]
        );
    }

    #[test]
    fn one_shot_timers_do_not_re_arm() {
        let mut engine = engine(
            r#"
            name: test
            timers:
              - after: 30s
                send: ["stand"]
            "#,
        );
        let start = Instant::now();
        engine.start_timers(start);

        assert_eq!(
            engine.fire_due_timers(start + Duration::from_secs(30)),
            vec!["stand"]
        );
        assert!(
            engine
                .fire_due_timers(start + Duration::from_secs(600))
                .is_empty()
        );
        assert_eq!(engine.next_timer_deadline(), None);
    }

    #[test]
    fn timer_templates_read_variables() {
        let mut engine = engine(
            r#"
            name: test
            variables:
              target: rat
            timers:
              - every: 10s
                send: ["kill ${target}"]
            "#,
        );
        let start = Instant::now();
        engine.start_timers(start);
        assert_eq!(
            engine.fire_due_timers(start + Duration::from_secs(10)),
            vec!["kill rat"]
        );
    }

    #[test]
    fn next_deadline_is_the_earliest_armed_timer() {
        let mut engine = engine(
            r#"
            name: test
            timers:
              - every: 5m
                send: ["a"]
              - every: 30s
                send: ["b"]
            "#,
        );
        let start = Instant::now();
        engine.start_timers(start);
        assert_eq!(
            engine.next_timer_deadline(),
            Some(start + Duration::from_secs(30))
        );
    }

    #[test]
    fn timers_shadow_by_id_across_layers() {
        let mut engine = layers(&[
            "name: module\ntimers:\n  - id: autosave\n    every: 60s\n    send: [\"save\"]\n",
            "name: profile\ntimers:\n  - id: autosave\n    enabled: false\n",
        ]);
        let start = Instant::now();
        engine.start_timers(start);
        assert_eq!(engine.next_timer_deadline(), None);
    }

    #[test]
    fn parses_duration_units() {
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
    }

    #[test]
    fn rejects_durations_without_a_unit() {
        assert!(parse_duration("60").is_err());
        assert!(parse_duration("s").is_err());
        assert!(parse_duration("10 fortnights").is_err());
    }

    #[test]
    fn a_timer_without_a_schedule_is_an_error() {
        let err = Engine::compile(&[module("name: t\ntimers:\n  - send: [\"x\"]\n")]).unwrap_err();
        assert!(err.to_string().contains("every"), "{err}");
    }
}
