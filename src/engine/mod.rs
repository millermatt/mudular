//! Automation engine: triggers, aliases, and variables from YAML modules.
//!
//! Rules compile to a per-session `Engine` instance; the hierarchical scope
//! merge (global → shared modules → profile overrides) lands in M4
//! (docs/ARCHITECTURE.md §7). All matching uses the `regex` crate:
//! Unicode-aware and linear-time, so hostile server output cannot stall the
//! client. Send templates use `${1}` / `${name}` capture substitution.

use std::collections::HashMap;

use regex::Regex;
use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleModule {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub variables: HashMap<String, String>,
    #[serde(default)]
    pub aliases: Vec<Alias>,
    #[serde(default)]
    pub triggers: Vec<Trigger>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Alias {
    /// Stable identity for shadowing across scope layers.
    #[serde(default)]
    pub id: Option<String>,
    pub pattern: String,
    #[serde(default)]
    pub send: Vec<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Trigger {
    /// Stable identity for shadowing across scope layers.
    #[serde(default)]
    pub id: Option<String>,
    pub pattern: String,
    #[serde(default)]
    pub send: Vec<String>,
    #[serde(default)]
    pub gag: bool,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("invalid pattern `{pattern}` in module `{module}`: {source}")]
    BadPattern {
        module: String,
        pattern: String,
        source: regex::Error,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Send(String),
    Gag,
}

#[derive(Debug)]
struct CompiledRule {
    regex: Regex,
    send: Vec<String>,
    gag: bool,
}

#[derive(Debug)]
pub struct Engine {
    aliases: Vec<CompiledRule>,
    triggers: Vec<CompiledRule>,
    variables: HashMap<String, String>,
}

impl Engine {
    /// Compile a flat, already-merged list of modules (scope merge: M4).
    pub fn compile(modules: &[RuleModule]) -> Result<Self, EngineError> {
        let mut engine = Engine {
            aliases: Vec::new(),
            triggers: Vec::new(),
            variables: HashMap::new(),
        };
        for module in modules {
            engine.variables.extend(module.variables.clone());
            for alias in module.aliases.iter().filter(|a| a.enabled) {
                engine.aliases.push(CompiledRule {
                    regex: compile_pattern(&module.name, &alias.pattern)?,
                    send: alias.send.clone(),
                    gag: false,
                });
            }
            for trigger in module.triggers.iter().filter(|t| t.enabled) {
                engine.triggers.push(CompiledRule {
                    regex: compile_pattern(&module.name, &trigger.pattern)?,
                    send: trigger.send.clone(),
                    gag: trigger.gag,
                });
            }
        }
        Ok(engine)
    }

    /// Run triggers against a completed inbound line (ANSI already stripped).
    pub fn process_line(&self, line: &str) -> Vec<Action> {
        let mut actions = Vec::new();
        for rule in &self.triggers {
            if let Some(caps) = rule.regex.captures(line) {
                for template in &rule.send {
                    let mut expanded = String::new();
                    caps.expand(template, &mut expanded);
                    actions.push(Action::Send(expanded));
                }
                if rule.gag {
                    actions.push(Action::Gag);
                }
            }
        }
        actions
    }

    /// Expand typed input through aliases; `None` means send it verbatim.
    pub fn expand_alias(&self, input: &str) -> Option<Vec<String>> {
        for rule in &self.aliases {
            if let Some(caps) = rule.regex.captures(input) {
                return Some(
                    rule.send
                        .iter()
                        .map(|template| {
                            let mut expanded = String::new();
                            caps.expand(template, &mut expanded);
                            expanded
                        })
                        .collect(),
                );
            }
        }
        None
    }
}

fn compile_pattern(module: &str, pattern: &str) -> Result<Regex, EngineError> {
    Regex::new(pattern).map_err(|source| EngineError::BadPattern {
        module: module.to_string(),
        pattern: pattern.to_string(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn module(yaml: &str) -> RuleModule {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn trigger_matches_with_named_capture_substitution() {
        let m = module(
            r#"
            name: test
            triggers:
              - pattern: '^(?P<who>\p{L}+) has arrived\.$'
                send: ["look ${who}"]
            "#,
        );
        let engine = Engine::compile(&[m]).unwrap();
        let actions = engine.process_line("Ærlend has arrived.");
        assert_eq!(actions, vec![Action::Send("look Ærlend".to_string())]);
        assert!(engine.process_line("nothing here").is_empty());
    }

    #[test]
    fn alias_expands_or_passes_through() {
        let m = module(
            r#"
            name: test
            aliases:
              - pattern: '^gh (.+)$'
                send: ["get ${1}", "wear ${1}"]
            "#,
        );
        let engine = Engine::compile(&[m]).unwrap();
        assert_eq!(
            engine.expand_alias("gh 帽子"),
            Some(vec!["get 帽子".to_string(), "wear 帽子".to_string()])
        );
        assert_eq!(engine.expand_alias("look"), None);
    }

    #[test]
    fn disabled_rules_are_skipped() {
        let m = module(
            r#"
            name: test
            triggers:
              - pattern: 'x'
                send: ["y"]
                enabled: false
            "#,
        );
        let engine = Engine::compile(&[m]).unwrap();
        assert!(engine.process_line("x").is_empty());
    }

    #[test]
    fn bad_pattern_reports_module_context() {
        let m = module(
            r#"
            name: broken
            triggers:
              - pattern: '('
            "#,
        );
        let err = Engine::compile(&[m]).unwrap_err();
        assert!(err.to_string().contains("broken"));
    }
}
