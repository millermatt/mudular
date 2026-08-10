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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

use regex::{Captures, Regex};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::watch;

mod condition;
pub mod script;

use condition::Condition;
use script::{Hook, ScriptCtx, ScriptHost, ScriptOutcome, ScriptSource};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleModule {
    /// Label used in error messages; loaders set it from the file name.
    #[serde(default)]
    pub name: String,
    /// Part of the on-disk schema: `deny_unknown_fields` means dropping it
    /// would reject modules that set it. Read by the user, not the code.
    #[serde(default)]
    #[allow(dead_code)]
    pub description: Option<String>,
    #[serde(default)]
    pub variables: BTreeMap<String, String>,
    #[serde(default)]
    pub aliases: Vec<Alias>,
    #[serde(default)]
    pub triggers: Vec<Trigger>,
    #[serde(default)]
    pub timers: Vec<Timer>,
    /// Script files this module brings, by name relative to the module
    /// (§7.4). The extension picks the engine.
    #[serde(default)]
    pub scripts: Vec<String>,
    /// The declared scripts, read off disk by the loader — `engine` is
    /// sans-IO (§4), so it compiles source text it is handed rather than
    /// opening files itself.
    #[serde(skip)]
    pub script_sources: Vec<ScriptSource>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Alias {
    /// Stable identity for shadowing across scope layers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    /// Guard: the rule fires only if the pattern matches *and* this
    /// evaluates true (§7.6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send: Option<Vec<String>>,
    /// Commands for *other* sessions, keyed by session name (§7.5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send_to: Option<BTreeMap<String, Vec<String>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set: Option<BTreeMap<String, String>>,
    /// Call a function a script defined (§7.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<ScriptAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

/// A rule's `script:` action: `{file: combat.lua, fn: on_death}`. The file
/// picks the engine and must be one the module declared; the function is a
/// global that file's language defined.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptAction {
    pub file: String,
    #[serde(rename = "fn")]
    pub function: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Trigger {
    /// Stable identity for shadowing across scope layers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    /// Guard: the rule fires only if the pattern matches *and* this
    /// evaluates true (§7.6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send: Option<Vec<String>>,
    /// Commands for *other* sessions, keyed by session name (§7.5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send_to: Option<BTreeMap<String, Vec<String>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set: Option<BTreeMap<String, String>>,
    /// Call a function a script defined (§7.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<ScriptAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gag: Option<bool>,
    /// Send the matched line to this channel pane instead of leaving it to
    /// the main scrollback alone (§11.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    /// Recolour the matched text, or the whole line (§7.7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub highlight: Option<HighlightSpec>,
    /// Rings the terminal bell / desktop notification when this fires in a
    /// session that isn't focused (§14 M9). Independent of `gag:` — a line
    /// worth hiding can still be worth an alert.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bell: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

/// A trigger's `highlight:` action (§7.7). Colours stay as the player
/// wrote them until `Engine::compile` turns the block into an SGR string:
/// deserializing them here would report a bad name without being able to
/// say which rule it came from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HighlightSpec {
    /// A colour name, `#rrggbb`, or a 0-255 palette index — the same
    /// vocabulary as a profile's `color:` (§11).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fg: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bg: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub bold: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub italic: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub underline: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub reverse: bool,
    /// Restyle the entire line rather than only the matched text.
    #[serde(default, skip_serializing_if = "is_false")]
    pub whole_line: bool,
}

fn is_false(b: &bool) -> bool {
    !b
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Timer {
    /// Stable identity for shadowing across scope layers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Repeat on this interval, e.g. `60s`, `5m`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every: Option<String>,
    /// Fire once after this delay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
    #[error("invalid `when:` condition `{condition}` in `{module}`: {reason}")]
    BadCondition {
        module: String,
        condition: String,
        reason: String,
    },
    #[error("invalid `highlight:` on rule `{rule}` in `{module}`: {reason}")]
    BadHighlight {
        module: String,
        rule: String,
        reason: String,
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
    #[error("`{module}` declares `{script}`, but no script engine handles that extension")]
    UnknownScriptLanguage { module: String, script: String },
    #[error("`{module}` declares `{script}`, but this build has no {language} engine")]
    ScriptEngineMissing {
        module: String,
        script: String,
        language: &'static str,
    },
    #[error("script `{script}` in `{module}`: {reason}")]
    BadScript {
        module: String,
        script: String,
        reason: String,
    },
    #[error("a rule calls `{script}`, which no module or profile declares in `scripts:`")]
    UndeclaredScript { script: String },
    #[error("a rule calls `{function}`, which `{script}` does not define")]
    UnknownScriptFunction { script: String, function: String },
}

/// Commands a rule asks another session to run (docs/ARCHITECTURE.md §7.5).
/// `target` is a session name, or `*` for every other session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrossSend {
    pub target: String,
    pub lines: Vec<String>,
}

/// What a matched line asks the session to do.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LineOutcome {
    /// Keep this line out of the scrollback entirely.
    pub gag: bool,
    /// Commands to send to the server, in trigger order.
    pub sends: Vec<String>,
    /// Commands for other sessions, for the hub to route (§7.5).
    pub send_to: Vec<CrossSend>,
    /// Channel pane this line also belongs in (§11.1).
    pub route: Option<String>,
    /// Text a script asked to show in this session only (§7.4).
    pub echoes: Vec<String>,
    /// Text a script asked to show in *another* session's pane, by the
    /// name that addresses it (§7.5). Display-only.
    pub echo_to: Vec<(String, String)>,
    /// Replacement text for the line, from a script's `mud.substitute`.
    pub substitute: Option<String>,
    /// Ranges to restyle, as byte offsets into the *stripped* line the
    /// engine matched, each with the SGR parameters to wrap them in
    /// (§7.7). The engine never sees the original ANSI, so it returns
    /// ranges rather than styled text and the session splices them.
    pub highlights: Vec<(std::ops::Range<usize>, String)>,
    /// A trigger asked for a bell / desktop notification on this line
    /// (§14 M9). The session doesn't know its own focus state, so it only
    /// reports the request; the hub decides whether to ring it.
    pub bell: bool,
    /// Whether any trigger's pattern *and* guard held on this line — true
    /// even if its only action was silent (a bare `set:`), false if no
    /// rule matched at all. Used for the one-time "a trigger just fired"
    /// hint (UX_REVIEW.md G); not a count, since a running commentary is
    /// exactly what that hint is deliberately not.
    pub fired: bool,
}

/// What a typed input line expands to.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct InputOutcome {
    /// Commands to send to the server, in input order.
    pub sends: Vec<String>,
    /// Commands for other sessions, for the hub to route (§7.5).
    pub send_to: Vec<CrossSend>,
    /// Text a script action asked to show in this session only (§7.4).
    pub echoes: Vec<String>,
    /// Text for another session's pane (§7.5).
    pub echo_to: Vec<(String, String)>,
    /// Speedwalk paths this input expanded, original text paired with the
    /// steps it became — including one expanded from an alias's `send:`
    /// output, not just what was typed directly. The engine reports every
    /// expansion every call; it's the session that decides whether this is
    /// the first one worth telling the player about (UX_REVIEW.md G).
    pub speedwalks: Vec<(String, Vec<String>)>,
}

/// What one session publishes about itself for the others to read (§7.5):
/// its variables and its server-data map, as of the last change. Reads are
/// local and lock-free — a snapshot is a value, not a query.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PeerSnapshot {
    pub vars: HashMap<String, String>,
    pub data: HashMap<String, String>,
}

/// Every other session's snapshot, by the name that addresses it (§7.5).
/// A `watch::Receiver` borrow is a lock-free read of a value already in
/// memory, so consulting one costs a rule no more than reading its own
/// variables — and `engine` stays sans-IO (§4): nothing here awaits.
pub type Peers = HashMap<String, watch::Receiver<PeerSnapshot>>;

/// Everything a `${...}` term can resolve against: this line's captures,
/// this session's own state, and its peers' snapshots. Bundled because
/// every resolver needs all of it and the set grew past what is worth
/// threading positionally.
pub(crate) struct Scope<'a> {
    pub caps: Option<&'a Captures<'a>>,
    pub vars: &'a HashMap<String, String>,
    pub server_data: &'a HashMap<String, String>,
    pub peers: &'a Peers,
}

#[derive(Debug)]
struct CompiledRule {
    regex: Regex,
    /// Compiled `when:` guard, if the rule has one (§7.6).
    when: Option<Condition>,
    sends: Vec<String>,
    send_to: Vec<(String, Vec<String>)>,
    set: Vec<(String, String)>,
    /// A `script:` action, resolved at compile time to the host that will
    /// run it, so firing costs no lookup.
    script: Option<CompiledScriptCall>,
    gag: bool,
    route: Option<String>,
    highlight: Option<CompiledHighlight>,
    bell: bool,
}

/// A `highlight:` block reduced to what the hot path needs: the SGR
/// parameters to emit and how far they reach.
#[derive(Debug)]
struct CompiledHighlight {
    sgr: String,
    whole_line: bool,
}

#[derive(Debug)]
struct CompiledScriptCall {
    host: usize,
    function: String,
}

/// A script host and the language it speaks, so a rule's `script:` action
/// can be routed to the engine its file belongs to.
#[derive(Debug)]
struct HostEntry {
    language: &'static str,
    host: Box<dyn ScriptHost>,
}

impl CompiledRule {
    /// Whether this rule's `when:` guard permits it to fire. A rule
    /// without one always fires on a match.
    fn guard_holds(&self, scope: &Scope) -> bool {
        self.when.as_ref().is_none_or(|when| when.eval(scope))
    }
}

/// One armed `mud.timer` callback: which host issued it, that host's id
/// for the callback, and when it comes due.
#[derive(Debug)]
struct ScriptTimer {
    host: usize,
    id: u64,
    at: Instant,
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
    /// GMCP/MSDP values, keyed by dotted path (`Char.Vitals.hp`). Kept apart
    /// from `variables` so live server data can't collide with a rule
    /// author's own names; `${...}` templates fall back to it after
    /// captures and variables (docs/ARCHITECTURE.md §6.3/§14 M6).
    server_data: HashMap<String, String>,
    /// Keys currently sourced from GMCP. MSDP updates to one of these keys
    /// are dropped, since §6.3 says GMCP wins when a server offers both.
    gmcp_keys: HashSet<String>,
    /// One host per script language this session's modules actually use
    /// (§7.4). Empty for a rules-only session, which then never enters a
    /// VM at all.
    hosts: Vec<HostEntry>,
    /// The other sessions' published state, for `${@name.key}` (§7.5).
    peers: Peers,
    /// The last snapshot each peer was seen at, so a change can be
    /// reported as the keys that moved rather than as "something did".
    peers_seen: HashMap<String, PeerSnapshot>,
    /// Timers scripts armed with `mud.timer`, alongside the YAML ones
    /// (§7.4). The engine holds the deadlines and the session does the
    /// sleeping, exactly as for `timers:`.
    script_timers: Vec<ScriptTimer>,
    /// Whether this session's own state has moved since it last published.
    /// Snapshots are values, so publishing copies the stores; doing it only
    /// on change keeps a quiet session free.
    changed: bool,
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

        // Scripts load before the rules compile: a rule's `script:` action
        // is checked against the functions its file actually defined, so a
        // typo fails here rather than never firing.
        let (hosts, declared) = compile_scripts(layers)?;

        Ok(Engine {
            aliases: compile_rules(&aliases, &hosts, &declared)?,
            triggers: compile_rules(&triggers, &hosts, &declared)?,
            timers: compile_timers(&timers)?,
            variables,
            server_data: HashMap::new(),
            gmcp_keys: HashSet::new(),
            hosts,
            peers: Peers::new(),
            peers_seen: HashMap::new(),
            script_timers: Vec::new(),
            changed: false,
        })
    }

    /// Hands this session the receivers for every other session's snapshot
    /// (§7.5). Called once, by the hub that knows what else is open at
    /// spawn time.
    pub fn set_peers(&mut self, peers: Peers) {
        self.peers = peers;
    }

    /// Everything the server has told us about itself, GMCP and MSDP merged
    /// into one namespace (§6.3). Read by the session to pick the room out
    /// of it (§16), which is why this is the merged store and not either
    /// protocol's own message.
    pub fn server_data(&self) -> &HashMap<String, String> {
        &self.server_data
    }

    /// Adds one more peer without disturbing the rest — a session added to
    /// a running instance after this one, whose existence this one has no
    /// other way to learn (§7.5, `/connect`). Overwrites a same-named
    /// entry, which only happens if a name was already reused; the newer
    /// receiver is the live one.
    pub fn add_peer(&mut self, name: String, rx: watch::Receiver<PeerSnapshot>) {
        self.peers.insert(name, rx);
    }

    /// This session's state to publish, or `None` if nothing has changed
    /// since the last call — peers hold the previous value either way, so
    /// republishing an identical snapshot would only wake them.
    pub fn take_snapshot(&mut self) -> Option<PeerSnapshot> {
        self.changed.then(|| {
            self.changed = false;
            PeerSnapshot {
                vars: self.variables.clone(),
                data: self.server_data.clone(),
            }
        })
    }

    /// Runs one hook across every host, in the order their languages first
    /// appeared. Scripts read and write the same variable store the rules
    /// use, so a `set:` and a `mud.set` are the same variable.
    ///
    /// A host that fails says so in the session's own scrollback: a script
    /// that stopped working is exactly as invisible as a trigger that
    /// stopped firing, and neither should be.
    fn run_hook(&mut self, hook: &Hook) -> ScriptOutcome {
        self.run_hook_on(hook, None)
    }

    /// As [`Self::run_hook`], but `only` narrows it to a single host — what
    /// a rule's `script:` action needs, since it names one function in one
    /// language.
    fn run_hook_on(&mut self, hook: &Hook, only: Option<usize>) -> ScriptOutcome {
        if self.hosts.is_empty() {
            return ScriptOutcome::default();
        }

        let mut ctx = ScriptCtx {
            vars: std::mem::take(&mut self.variables),
            server_data: std::mem::take(&mut self.server_data),
            peers: std::mem::take(&mut self.peers),
            out: ScriptOutcome::default(),
        };
        let mut armed: Vec<ScriptTimer> = Vec::new();
        let now = Instant::now();
        for (index, entry) in self.hosts.iter_mut().enumerate() {
            if only.is_some_and(|wanted| wanted != index) {
                continue;
            }
            if let Err(err) = entry.host.call(hook, &mut ctx) {
                ctx.out.echoes.push(format!("** {err}"));
            }
            // Drained per host: an id means nothing outside the host that
            // issued it.
            armed.extend(ctx.out.timers.drain(..).map(|(id, after)| ScriptTimer {
                host: index,
                id,
                at: now + after,
            }));
        }
        self.script_timers.extend(armed);
        self.variables = ctx.vars;
        self.server_data = ctx.server_data;
        self.peers = ctx.peers;
        // A hook may have called `mud.set`; asking which is not worth the
        // bookkeeping when the answer only costs one snapshot.
        self.changed = true;
        ctx.out
    }

    /// Reports one peer's changed server-data keys to the scripts watching
    /// it (§7.5). The session calls this when that peer's snapshot channel
    /// wakes it; keys go in sorted order, so two runs of the same change
    /// dispatch the same way.
    ///
    /// This is the "observe" half of cross-session automation: the reaction
    /// lives with the character that acts, and runs in its own session,
    /// where the commands it sends belong.
    pub fn poll_peer(&mut self, name: &str) -> ScriptOutcome {
        let Some(current) = self.peers.get(name).map(|rx| rx.borrow().clone()) else {
            return ScriptOutcome::default();
        };
        let previous = self.peers_seen.insert(name.to_string(), current.clone());

        let mut out = ScriptOutcome::default();
        if self.hosts.is_empty() {
            return out;
        }
        let previous = previous.unwrap_or_default();
        let mut changed: Vec<(String, String)> = current
            .data
            .iter()
            .filter(|(key, value)| previous.data.get(*key) != Some(*value))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        // A key that *vanished* is news too — §7.5's headline example is a
        // buff dropping, and watching only the keys still present would
        // never report it. Empty is the value an absent key already
        // resolves to everywhere else (`${@peer.gone}`), so a hook needs no
        // new vocabulary to test for it.
        changed.extend(
            previous
                .data
                .keys()
                .filter(|key| !current.data.contains_key(*key))
                .map(|key| (key.clone(), String::new())),
        );
        changed.sort();

        for (key, value) in changed {
            merge(
                &mut out,
                self.run_hook(&Hook::Peer {
                    session: name.to_string(),
                    key,
                    value,
                }),
            );
        }
        out
    }

    /// Session lifecycle hooks (§7.4). `on_connect` runs once the
    /// connection is up, alongside timer arming, so a script can log in or
    /// prime its state exactly when a timer would first fire.
    pub fn on_connect(&mut self) -> ScriptOutcome {
        self.run_hook(&Hook::Connect)
    }

    pub fn on_disconnect(&mut self) -> ScriptOutcome {
        self.run_hook(&Hook::Disconnect)
    }

    /// Runs prompt hooks. Prompts do not go through the trigger table —
    /// they are not scrollback lines — so this is a script's only view of
    /// them.
    pub fn process_prompt(&mut self, text: &str) -> ScriptOutcome {
        self.run_hook(&Hook::Prompt(text.to_string()))
    }

    /// Runs GMCP hooks with the message as it arrived. The flattened values
    /// are already in the server-data store by then, so a script can either
    /// decode the JSON itself or read `mud.data`.
    pub fn process_gmcp(&mut self, package: &str, json: &str) -> ScriptOutcome {
        self.run_hook(&Hook::Gmcp {
            package: package.to_string(),
            json: json.to_string(),
        })
    }

    /// Run triggers against a completed inbound line, ANSI already
    /// stripped (§7.1). Takes `&mut self` because a trigger may `set:` a
    /// variable that later rules and sends can read.
    pub fn process_line(&mut self, line: &str) -> LineOutcome {
        let mut outcome = LineOutcome::default();
        let mut updates: Vec<(String, String)> = Vec::new();
        let mut calls: Vec<(usize, String, script::Captures)> = Vec::new();

        for rule in &self.triggers {
            let Some(caps) = rule.regex.captures(line) else {
                continue;
            };
            let scope = Scope {
                caps: Some(&caps),
                vars: &self.variables,
                server_data: &self.server_data,
                peers: &self.peers,
            };
            // A guarded-out rule does nothing at all — not even gag or
            // route the line (§7.6: the rule fires only if both hold).
            if !rule.guard_holds(&scope) {
                continue;
            }
            outcome.fired = true;
            for template in &rule.sends {
                outcome.sends.push(expand(template, &scope));
            }
            for (target, templates) in &rule.send_to {
                outcome.send_to.push(CrossSend {
                    target: target.clone(),
                    lines: templates
                        .iter()
                        .map(|template| expand(template, &scope))
                        .collect(),
                });
            }
            for (name, template) in &rule.set {
                updates.push((name.clone(), expand(template, &scope)));
            }
            if let Some(call) = &rule.script {
                calls.push((
                    call.host,
                    call.function.clone(),
                    captured(&rule.regex, &caps),
                ));
            }
            outcome.gag |= rule.gag;
            // First route wins: a line belongs in one channel, and rules
            // fire in scope order, so the most specific layer decides.
            if outcome.route.is_none() {
                outcome.route.clone_from(&rule.route);
            }
            if let Some(highlight) = &rule.highlight {
                let span = match highlight.whole_line {
                    true => 0..line.len(),
                    // Group 0 is the whole match, which always
                    // participates; capture groups are deliberately not
                    // offered as a target (§7.7).
                    false => {
                        let whole = caps.get(0).expect("group 0 always participates");
                        whole.start()..whole.end()
                    }
                };
                // Overlaps: first match wins, as with `route:`. Nested SGR
                // has no defined "restore to the middle state", so a span
                // colliding with one already taken is dropped whole.
                let clash = outcome
                    .highlights
                    .iter()
                    .any(|(taken, _)| span.start < taken.end && taken.start < span.end);
                if !clash {
                    outcome.highlights.push((span, highlight.sgr.clone()));
                }
            }
            outcome.bell |= rule.bell;
        }

        self.changed |= !updates.is_empty();
        self.variables.extend(updates);

        // Scripts run after the trigger table, so a hook sees the variables
        // this line's rules just set and can override their verdict on the
        // line itself. Rules' own `script:` actions go first, in rule
        // order, then the `on_line` hooks.
        let mut scripted = ScriptOutcome::default();
        for (host, function, captures) in calls {
            merge(
                &mut scripted,
                self.run_hook_on(
                    &Hook::Function {
                        name: function,
                        line: line.to_string(),
                        captures,
                    },
                    Some(host),
                ),
            );
        }
        merge(&mut scripted, self.run_hook(&Hook::Line(line.to_string())));
        outcome.sends.extend(scripted.sends);
        outcome.send_to.extend(cross_sends(scripted.send_to));
        outcome.echo_to = scripted.echo_to;
        outcome.echoes = scripted.echoes;
        outcome.gag |= scripted.gag;
        outcome.substitute = scripted.substitute;
        outcome
    }

    /// Records one GMCP value under its dotted path, making it available to
    /// `${...}` templates in aliases, triggers, and timers
    /// (docs/ARCHITECTURE.md §14 M6 — "triggers can react to server data").
    /// GMCP always wins over MSDP for the same key (§6.3).
    pub fn update_server_data_from_gmcp(&mut self, key: &str, value: String) {
        self.gmcp_keys.insert(key.to_string());
        self.changed |= self.server_data.insert(key.to_string(), value.clone()) != Some(value);
    }

    /// Drops the positional keys a GMCP array left behind when it shrank.
    ///
    /// Arrays flatten to `Char.Affects.0`, `Char.Affects.1`, … so merging
    /// each new payload key-by-key keeps the tail of a longer previous
    /// array forever: `["bless","haste"]` followed by `["haste"]` leaves
    /// `Char.Affects.1` reading `haste` for the rest of the session — a
    /// phantom buff that never expires, which is precisely §7.5's own
    /// "rebuff the tank when his blessing drops" example failing.
    ///
    /// Deliberately narrower than replacing the package's whole subtree:
    /// servers that send partial object updates (`Char.Vitals {"hp":90}`
    /// after a fuller one) would lose the keys they omitted, so only
    /// indices at or past the new length are removed, along with anything
    /// nested under them.
    pub fn prune_gmcp_array(&mut self, path: &str, len: usize) {
        for key in self.stale_array_keys(path, len) {
            self.server_data.remove(&key);
            self.gmcp_keys.remove(&key);
            self.changed = true;
        }
    }

    /// As [`Self::prune_gmcp_array`], but for MSDP: MSDP arrays are
    /// positional too and go stale identically. Keys GMCP owns are left
    /// alone — a server offering both has GMCP preferred, so MSDP must not
    /// expire a value it was never allowed to set (§6.3).
    pub fn prune_msdp_array(&mut self, path: &str, len: usize) {
        for key in self.stale_array_keys(path, len) {
            if self.gmcp_keys.contains(&key) {
                continue;
            }
            self.server_data.remove(&key);
            self.changed = true;
        }
    }

    /// The keys under `path` whose array index is at or past `len` — what a
    /// shrunken array left behind, including anything nested under those
    /// elements.
    fn stale_array_keys(&self, path: &str, len: usize) -> Vec<String> {
        let prefix = format!("{path}.");
        self.server_data
            .keys()
            .filter(|key| {
                let Some(rest) = key.strip_prefix(&prefix) else {
                    return false;
                };
                // The component right after the array path is the index;
                // anything deeper (`…​.2.name`) belongs to that element and
                // goes with it.
                let index = rest.split('.').next().unwrap_or_default();
                index.parse::<usize>().is_ok_and(|i| i >= len)
            })
            .cloned()
            .collect()
    }

    /// As [`Self::update_server_data_from_gmcp`], but for MSDP: a no-op if
    /// GMCP already owns this key, since a server offering both protocols
    /// has GMCP preferred (§6.3).
    pub fn update_server_data_from_msdp(&mut self, key: &str, value: String) {
        if self.gmcp_keys.contains(key) {
            return;
        }
        self.changed |= self.server_data.insert(key.to_string(), value.clone()) != Some(value);
    }

    /// Turn one typed input line into the commands to send: split on `;`,
    /// then expand each part through aliases. Alias output is never
    /// re-expanded, so aliases cannot recurse into each other.
    pub fn expand_input(&mut self, input: &str) -> InputOutcome {
        let mut out = InputOutcome::default();
        let mut updates: Vec<(String, String)> = Vec::new();
        let mut calls: Vec<(usize, String, String, script::Captures)> = Vec::new();

        for part in input.split(';') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            // A guarded-out alias is not a match, so a later alias — or
            // the literal input — still gets its turn (§7.6).
            match self.aliases.iter().find_map(|rule| {
                let caps = rule.regex.captures(part)?;
                let scope = Scope {
                    caps: Some(&caps),
                    vars: &self.variables,
                    server_data: &self.server_data,
                    peers: &self.peers,
                };
                rule.guard_holds(&scope).then_some((rule, caps))
            }) {
                Some((rule, caps)) => {
                    let scope = Scope {
                        caps: Some(&caps),
                        vars: &self.variables,
                        server_data: &self.server_data,
                        peers: &self.peers,
                    };
                    for template in &rule.sends {
                        push_send(&mut out, expand(template, &scope));
                    }
                    for (target, templates) in &rule.send_to {
                        out.send_to.push(CrossSend {
                            target: target.clone(),
                            lines: templates
                                .iter()
                                .map(|template| expand(template, &scope))
                                .collect(),
                        });
                    }
                    for (name, template) in &rule.set {
                        updates.push((name.clone(), expand(template, &scope)));
                    }
                    if let Some(call) = &rule.script {
                        calls.push((
                            call.host,
                            call.function.clone(),
                            part.to_string(),
                            captured(&rule.regex, &caps),
                        ));
                    }
                }
                None => push_send(&mut out, part.to_string()),
            }
        }

        self.changed |= !updates.is_empty();
        self.variables.extend(updates);

        // As on an inbound line: the alias's own commands are collected
        // first, then its script action runs over the variables they set.
        for (host, function, part, captures) in calls {
            let scripted = self.run_hook_on(
                &Hook::Function {
                    name: function,
                    line: part,
                    captures,
                },
                Some(host),
            );
            out.sends.extend(scripted.sends);
            out.echoes.extend(scripted.echoes);
            out.send_to.extend(cross_sends(scripted.send_to));
            out.echo_to.extend(scripted.echo_to);
        }
        out
    }

    /// Arm every timer relative to `now`. The session calls this once the
    /// connection is up.
    pub fn start_timers(&mut self, now: Instant) {
        for timer in &mut self.timers {
            timer.next = Some(now + timer.interval);
        }
    }

    /// When the earliest armed timer is due, if any — a script's and a
    /// rule's are the same kind of thing to the session that sleeps on it.
    pub fn next_timer_deadline(&self) -> Option<Instant> {
        self.timers
            .iter()
            .filter_map(|timer| timer.next)
            .chain(self.script_timers.iter().map(|timer| timer.at))
            .min()
    }

    /// Fire every `mud.timer` callback due at `now`, earliest first. They
    /// are one-shot: a script that wants a heartbeat re-arms from inside
    /// its own callback, which is also what keeps a slow callback from
    /// stacking up behind itself.
    pub fn fire_due_script_timers(&mut self, now: Instant) -> ScriptOutcome {
        let mut due: Vec<(Instant, usize, u64)> = self
            .script_timers
            .iter()
            .filter(|timer| timer.at <= now)
            .map(|timer| (timer.at, timer.host, timer.id))
            .collect();
        due.sort();
        self.script_timers.retain(|timer| timer.at > now);

        let mut out = ScriptOutcome::default();
        for (_, host, id) in due {
            merge(&mut out, self.run_hook_on(&Hook::Timer { id }, Some(host)));
        }
        out
    }

    /// Fire every timer due at `now`, returning the commands to send.
    /// Repeating timers re-arm from `now` rather than from their previous
    /// deadline, so a stalled session cannot build up a burst of catch-up
    /// firings.
    pub fn fire_due_timers(&mut self, now: Instant) -> Vec<String> {
        let mut sends = Vec::new();
        let mut updates: Vec<(String, String)> = Vec::new();

        // Expanding reads the stores this session owns; re-arming writes
        // the timers. Two passes, so neither borrow has to outlive the
        // other.
        let mut fired = Vec::new();
        {
            let scope = Scope {
                caps: None,
                vars: &self.variables,
                server_data: &self.server_data,
                peers: &self.peers,
            };
            for (index, timer) in self.timers.iter().enumerate() {
                if timer.next.is_none_or(|next| next > now) {
                    continue;
                }
                for template in &timer.sends {
                    sends.push(expand(template, &scope));
                }
                for (name, template) in &timer.set {
                    updates.push((name.clone(), expand(template, &scope)));
                }
                fired.push(index);
            }
        }
        for index in fired {
            let timer = &mut self.timers[index];
            timer.next = timer.repeat.then(|| now + timer.interval);
        }

        self.changed |= !updates.is_empty();
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
        self.when = self.when.take().or_else(|| base.when.clone());
        self.send = self.send.take().or_else(|| base.send.clone());
        self.send_to = self.send_to.take().or_else(|| base.send_to.clone());
        self.set = self.set.take().or_else(|| base.set.clone());
        self.script = self.script.take().or_else(|| base.script.clone());
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
        self.when = self.when.take().or_else(|| base.when.clone());
        self.send = self.send.take().or_else(|| base.send.clone());
        self.send_to = self.send_to.take().or_else(|| base.send_to.clone());
        self.set = self.set.take().or_else(|| base.set.clone());
        self.script = self.script.take().or_else(|| base.script.clone());
        self.gag = self.gag.or(base.gag);
        self.route = self.route.take().or_else(|| base.route.clone());
        self.highlight = self.highlight.take().or_else(|| base.highlight.clone());
        self.bell = self.bell.or(base.bell);
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
    fn when_str(&self) -> Option<&str>;
    fn enabled(&self) -> bool;
    fn sends(&self) -> Vec<String>;
    fn send_to(&self) -> Vec<(String, Vec<String>)>;
    fn sets(&self) -> Vec<(String, String)>;
    fn script(&self) -> Option<ScriptAction>;
    fn gag(&self) -> bool;
    fn route(&self) -> Option<String>;
    fn highlight(&self) -> Option<HighlightSpec>;
    fn bell(&self) -> bool;
}

impl CompilableRule for Alias {
    fn id_label(&self) -> String {
        self.id.clone().unwrap_or_default()
    }
    fn pattern_str(&self) -> Option<&str> {
        self.pattern.as_deref()
    }
    fn when_str(&self) -> Option<&str> {
        self.when.as_deref()
    }
    fn enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }
    fn sends(&self) -> Vec<String> {
        self.send.clone().unwrap_or_default()
    }
    fn send_to(&self) -> Vec<(String, Vec<String>)> {
        self.send_to
            .clone()
            .unwrap_or_default()
            .into_iter()
            .collect()
    }
    fn sets(&self) -> Vec<(String, String)> {
        self.set.clone().unwrap_or_default().into_iter().collect()
    }
    fn script(&self) -> Option<ScriptAction> {
        self.script.clone()
    }
    fn gag(&self) -> bool {
        false
    }
    fn route(&self) -> Option<String> {
        None
    }
    /// Aliases never render server output, so there is nothing to restyle.
    fn highlight(&self) -> Option<HighlightSpec> {
        None
    }
    /// Aliases fire on outbound input, not inbound lines, so there is
    /// nothing here for the player to be alerted to.
    fn bell(&self) -> bool {
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
    fn when_str(&self) -> Option<&str> {
        self.when.as_deref()
    }
    fn enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }
    fn sends(&self) -> Vec<String> {
        self.send.clone().unwrap_or_default()
    }
    fn send_to(&self) -> Vec<(String, Vec<String>)> {
        self.send_to
            .clone()
            .unwrap_or_default()
            .into_iter()
            .collect()
    }
    fn sets(&self) -> Vec<(String, String)> {
        self.set.clone().unwrap_or_default().into_iter().collect()
    }
    fn script(&self) -> Option<ScriptAction> {
        self.script.clone()
    }
    fn gag(&self) -> bool {
        self.gag.unwrap_or(false)
    }
    fn route(&self) -> Option<String> {
        self.route.clone()
    }
    fn highlight(&self) -> Option<HighlightSpec> {
        self.highlight.clone()
    }
    fn bell(&self) -> bool {
        self.bell.unwrap_or(false)
    }
}

fn compile_rules<T: CompilableRule>(
    rules: &[T],
    hosts: &[HostEntry],
    declared: &HashSet<String>,
) -> Result<Vec<CompiledRule>, EngineError> {
    let mut out = Vec::new();
    for rule in rules.iter().filter(|rule| rule.enabled()) {
        let pattern = rule.pattern_str().ok_or_else(|| EngineError::UnknownRule {
            module: "merged rules".to_string(),
            id: rule.id_label(),
        })?;
        let when = rule
            .when_str()
            .map(|src| {
                Condition::parse(src).map_err(|reason| EngineError::BadCondition {
                    module: "merged rules".to_string(),
                    condition: src.to_string(),
                    reason,
                })
            })
            .transpose()?;
        out.push(CompiledRule {
            regex: Regex::new(pattern).map_err(|source| EngineError::BadPattern {
                module: "merged rules".to_string(),
                pattern: pattern.to_string(),
                source,
            })?,
            when,
            sends: rule.sends(),
            send_to: rule.send_to(),
            set: rule.sets(),
            script: rule
                .script()
                .map(|action| compile_script_call(&action, hosts, declared))
                .transpose()?,
            gag: rule.gag(),
            route: rule.route(),
            highlight: rule
                .highlight()
                .map(|spec| {
                    compile_highlight(&spec).map_err(|reason| EngineError::BadHighlight {
                        module: "merged rules".to_string(),
                        rule: match rule.id_label().is_empty() {
                            true => pattern.to_string(),
                            false => rule.id_label(),
                        },
                        reason,
                    })
                })
                .transpose()?,
            bell: rule.bell(),
        });
    }
    Ok(out)
}

/// Turns a `highlight:` block into SGR parameters once, at load, so the
/// per-line cost is a string splice rather than a colour lookup (§7.7).
fn compile_highlight(spec: &HighlightSpec) -> Result<CompiledHighlight, String> {
    let mut params: Vec<String> = Vec::new();
    for (set, code) in [
        (spec.bold, "1"),
        (spec.italic, "3"),
        (spec.underline, "4"),
        (spec.reverse, "7"),
    ] {
        if set {
            params.push(code.to_string());
        }
    }
    if let Some(fg) = &spec.fg {
        params.push(sgr_color(fg, false)?);
    }
    if let Some(bg) = &spec.bg {
        params.push(sgr_color(bg, true)?);
    }
    // A block that styles nothing is a typo, not a rule the player meant
    // to have no effect.
    if params.is_empty() {
        return Err("sets no colour or attribute".to_string());
    }
    Ok(CompiledHighlight {
        sgr: params.join(";"),
        whole_line: spec.whole_line,
    })
}

/// One colour as an SGR parameter. The vocabulary is a profile's `color:`
/// (§11), so the same parser backs both; backgrounds are the foreground
/// codes offset by ten.
fn sgr_color(name: &str, background: bool) -> Result<String, String> {
    use ratatui::style::Color;
    use std::str::FromStr;

    let color = Color::from_str(name).map_err(|_| {
        format!("unknown color {name:?}: use a name (cyan, light blue), #rrggbb, or 0-255")
    })?;
    let shift = u32::from(background) * 10;
    let basic = |code: u32| (code + shift).to_string();
    Ok(match color {
        Color::Reset => basic(39),
        Color::Black => basic(30),
        Color::Red => basic(31),
        Color::Green => basic(32),
        Color::Yellow => basic(33),
        Color::Blue => basic(34),
        Color::Magenta => basic(35),
        Color::Cyan => basic(36),
        Color::Gray => basic(37),
        Color::DarkGray => basic(90),
        Color::LightRed => basic(91),
        Color::LightGreen => basic(92),
        Color::LightYellow => basic(93),
        Color::LightBlue => basic(94),
        Color::LightMagenta => basic(95),
        Color::LightCyan => basic(96),
        Color::White => basic(97),
        Color::Indexed(index) => format!("{};5;{index}", 38 + shift),
        Color::Rgb(r, g, b) => format!("{};2;{r};{g};{b}", 38 + shift),
    })
}

/// Starts a host for each language the layers use and loads their scripts
/// in scope order, so a profile's script sees whatever a shared module's
/// script registered first (§7.3). Also returns every declared file name,
/// which is what a rule's `script:` action must name.
fn compile_scripts(
    layers: &[RuleModule],
) -> Result<(Vec<HostEntry>, HashSet<String>), EngineError> {
    let mut hosts: Vec<HostEntry> = Vec::new();
    let mut declared = HashSet::new();

    for layer in layers {
        for source in &layer.script_sources {
            let language =
                language_of(&source.name).ok_or_else(|| EngineError::UnknownScriptLanguage {
                    module: layer.name.clone(),
                    script: source.name.clone(),
                })?;
            let index = match hosts.iter().position(|entry| entry.language == language) {
                Some(index) => index,
                None => {
                    hosts.push(HostEntry {
                        language,
                        host: new_host(language, layer, &source.name)?,
                    });
                    hosts.len() - 1
                }
            };
            hosts[index]
                .host
                .load(source)
                .map_err(|err| EngineError::BadScript {
                    module: layer.name.clone(),
                    script: source.name.clone(),
                    reason: err.to_string(),
                })?;
            declared.insert(source.name.clone());
        }
    }

    Ok((hosts, declared))
}

/// Resolves a rule's `script:` action to the host that will run it. Both
/// halves are checked now: the file must be one the layers declared, and
/// the function must exist in it — a rule that would never fire is a load
/// error, exactly like an invalid pattern.
fn compile_script_call(
    action: &ScriptAction,
    hosts: &[HostEntry],
    declared: &HashSet<String>,
) -> Result<CompiledScriptCall, EngineError> {
    if !declared.contains(&action.file) {
        return Err(EngineError::UndeclaredScript {
            script: action.file.clone(),
        });
    }
    let language = language_of(&action.file).ok_or_else(|| EngineError::UnknownScriptLanguage {
        module: "merged rules".to_string(),
        script: action.file.clone(),
    })?;
    let host = hosts
        .iter()
        .position(|entry| entry.language == language)
        .ok_or_else(|| EngineError::ScriptEngineMissing {
            module: "merged rules".to_string(),
            script: action.file.clone(),
            language,
        })?;
    if !hosts[host].host.has_function(&action.function) {
        return Err(EngineError::UnknownScriptFunction {
            script: action.file.clone(),
            function: action.function.clone(),
        });
    }
    Ok(CompiledScriptCall {
        host,
        function: action.function.clone(),
    })
}

/// Collects a rule's captures in the shape scripts see them: numbered
/// groups in order, named groups by name, non-participating groups absent.
fn captured(regex: &Regex, caps: &Captures) -> script::Captures {
    script::Captures {
        numbered: caps
            .iter()
            .skip(1)
            .map(|group| group.map(|m| m.as_str().to_string()))
            .collect(),
        named: regex
            .capture_names()
            .flatten()
            .filter_map(|name| {
                caps.name(name)
                    .map(|m| (name.to_string(), m.as_str().to_string()))
            })
            .collect(),
    }
}

/// Folds one script call's results into the line's running total. `gag` is
/// sticky and a later `substitute` wins, matching how the rules combine.
/// A script's cross-session sends, in the shape the hub routes (§7.5).
fn cross_sends(sends: Vec<(String, Vec<String>)>) -> Vec<CrossSend> {
    sends
        .into_iter()
        .map(|(target, lines)| CrossSend { target, lines })
        .collect()
}

fn merge(into: &mut ScriptOutcome, from: ScriptOutcome) {
    into.sends.extend(from.sends);
    into.echoes.extend(from.echoes);
    into.gag |= from.gag;
    into.send_to.extend(from.send_to);
    into.echo_to.extend(from.echo_to);
    if from.substitute.is_some() {
        into.substitute = from.substitute;
    }
}

/// A script's extension picks its engine — the same file name a player
/// types into `scripts:` says which VM runs it, with nothing to configure.
fn language_of(script: &str) -> Option<&'static str> {
    match script.rsplit('.').next() {
        Some("lua") => Some("lua"),
        Some("js") => Some("JavaScript"),
        _ => None,
    }
}

fn new_host(
    language: &'static str,
    layer: &RuleModule,
    script: &str,
) -> Result<Box<dyn ScriptHost>, EngineError> {
    let missing = || EngineError::ScriptEngineMissing {
        module: layer.name.clone(),
        script: script.to_string(),
        language,
    };
    // Unused in a build with no engine compiled in, where every arm below
    // is cfg'd away and only `missing()` is left.
    #[cfg_attr(not(any(feature = "lua", feature = "js")), allow(unused_variables))]
    let failed = |err: script::ScriptError| EngineError::BadScript {
        module: layer.name.clone(),
        script: script.to_string(),
        reason: err.to_string(),
    };
    match language {
        #[cfg(feature = "lua")]
        "lua" => Ok(Box::new(script::lua::LuaHost::new().map_err(failed)?)),
        #[cfg(feature = "js")]
        "JavaScript" => Ok(Box::new(script::js::JsHost::new().map_err(failed)?)),
        _ => Err(missing()),
    }
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

/// Speedwalk direction tokens, longest first so a two-letter diagonal is
/// preferred over the two one-letter moves it could also parse as — `ne`
/// is one move, not `n` then `e` (§7, §16).
const SPEEDWALK_DIRECTIONS: &[&str] = &["ne", "nw", "se", "sw", "n", "s", "e", "w", "u", "d"];

/// A single count run's upper bound, and the whole path's — a typo like
/// `.99999999n` should fall back to being sent as literal text, not queue
/// up a send storm (§13: bound anything that can multiply input).
const MAX_SPEEDWALK_COUNT: usize = 999;
const MAX_SPEEDWALK_STEPS: usize = 999;

/// Queues a send, first checking whether it's a `.3n2e`-style speedwalk
/// path (§7.8's sibling for movement rather than attention) — the classic
/// TinTin++/zMUD notation, with no room graph behind it (§16). A path that
/// doesn't parse as one is queued as typed, so `.` remains an ordinary
/// character everywhere else in a command.
fn push_send(out: &mut InputOutcome, text: String) {
    match expand_speedwalk(&text) {
        Some(steps) => {
            out.sends.extend(steps.clone());
            out.speedwalks.push((text, steps));
        }
        None => out.sends.push(text),
    }
}

/// Expands a leading-dot speedwalk path into its individual movement
/// commands, each sent as its own line (a MUD reads one direction per
/// command). `None` for anything that isn't one — no leading `.`, an empty
/// path, a token that isn't a digit run followed by a known direction, or
/// a count/total past the sanity bound — so the caller falls back to
/// sending the text unchanged.
fn expand_speedwalk(path: &str) -> Option<Vec<String>> {
    let mut rest = path.strip_prefix('.')?;
    if rest.is_empty() {
        return None;
    }
    let mut steps = Vec::new();
    while !rest.is_empty() {
        let digits_len = rest.bytes().take_while(u8::is_ascii_digit).count();
        let (digits, after_digits) = rest.split_at(digits_len);
        let count: usize = match digits {
            "" => 1,
            digits => digits
                .parse()
                .ok()
                .filter(|&n| n > 0 && n <= MAX_SPEEDWALK_COUNT)?,
        };
        let dir = SPEEDWALK_DIRECTIONS
            .iter()
            .find(|dir| after_digits.starts_with(*dir))?;
        if steps.len() + count > MAX_SPEEDWALK_STEPS {
            return None;
        }
        steps.extend(std::iter::repeat_n(dir.to_string(), count));
        rest = &after_digits[dir.len()..];
    }
    Some(steps)
}

/// Substitutes `${...}` in a send/set template: numbered and named regex
/// captures first, then the variable store (§7.1). An unresolved name is
/// left verbatim so a typo is visible rather than silently blank.
fn expand(template: &str, scope: &Scope) -> String {
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
        match lookup(name, scope) {
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

/// Resolution order: regex captures, then rule-defined variables, then live
/// server data — a variable a rule author names can always shadow a GMCP/
/// MSDP key of the same name.
pub(crate) fn lookup(name: &str, scope: &Scope) -> Option<String> {
    // `@session.key` reads another character's published state (§7.5), and
    // never this one's: the prefix is what tells them apart, so a peer name
    // can never shadow a local variable or a GMCP key.
    if let Some(peer_ref) = name.strip_prefix('@') {
        let (peer, key) = peer_ref.split_once('.')?;
        let snapshot = scope.peers.get(peer)?.borrow();
        return snapshot
            .vars
            .get(key)
            .or_else(|| snapshot.data.get(key))
            .cloned();
    }
    if let Some(caps) = scope.caps {
        if let Ok(index) = name.parse::<usize>() {
            return caps.get(index).map(|m| m.as_str().to_string());
        }
        if let Some(m) = caps.name(name) {
            return Some(m.as_str().to_string());
        }
    }
    scope
        .vars
        .get(name)
        .cloned()
        .or_else(|| scope.server_data.get(name).cloned())
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

    /// `fired` is true whenever a rule's pattern *and* guard both held —
    /// even with no visible action — and false otherwise. What the
    /// session's one-time "a trigger fired" hint (UX_REVIEW.md G) reads.
    #[test]
    fn process_line_reports_whether_a_trigger_fired() {
        let mut engine = engine(
            r#"
            name: test
            triggers:
              - pattern: 'spam'
                set: {seen: "yes"}
            "#,
        );
        assert!(engine.process_line("spam here").fired);
        assert!(!engine.process_line("nothing here").fired);
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
            engine.expand_input("gh 帽子").sends,
            vec!["get 帽子", "wear 帽子"]
        );
        assert_eq!(engine.expand_input("look").sends, vec!["look"]);
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
        assert_eq!(engine.expand_input("hh").sends, vec!["cast heal rat"]);
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
        assert_eq!(engine.expand_input("t").sends, vec!["kill ${nope}"]);
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
        assert_eq!(engine.expand_input("k").sends, vec!["kill kobold"]);
    }

    /// `set:` is applied once the whole line has been processed, so two
    /// triggers firing on one line never see each other's writes (§7.1).
    /// That is what keeps rule order within a line semantically inert: a
    /// layer that reorders rules cannot change what they read.
    #[test]
    fn a_variable_set_on_a_line_is_visible_from_the_next_line_on() {
        let mut engine = engine(
            r#"
            name: test
            triggers:
              - pattern: 'fighting (?P<foe>\w+)'
                set:
                  target: "${foe}"
              - pattern: 'fighting'
                send: ["kill ${target}"]
            "#,
        );

        // Same line: the second trigger reads the store as it was before
        // the line, where `target` is unset — so the name stays verbatim,
        // the same way any unresolved `${...}` does.
        let first = engine.process_line("You are fighting kobold");
        assert_eq!(first.sends, vec!["kill ${target}"]);
        assert_eq!(engine.variable("target"), Some("kobold"));

        // From the next line on, it reads back.
        let second = engine.process_line("You are fighting kobold");
        assert_eq!(second.sends, vec!["kill kobold"]);
    }

    /// The same batching on the input side: `;`-separated parts of one
    /// typed line don't see each other's `set:` either.
    #[test]
    fn a_variable_set_by_an_alias_is_visible_from_the_next_input_on() {
        let mut engine = engine(
            r#"
            name: test
            aliases:
              - pattern: '^mark (?P<who>\w+)$'
                set:
                  target: "${who}"
              - pattern: '^k$'
                send: ["kill ${target}"]
            "#,
        );

        assert_eq!(
            engine.expand_input("mark rat; k").sends,
            vec!["kill ${target}"]
        );
        assert_eq!(engine.expand_input("k").sends, vec!["kill rat"]);
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
            engine.expand_input("n; gh sword ;n").sends,
            vec!["n", "get sword", "wear sword", "n"]
        );
        assert_eq!(engine.expand_input(";;").sends, Vec::<String>::new());
    }

    // ---- speedwalk (§7.8, §16) ----

    #[test]
    fn a_speedwalk_path_expands_to_one_move_per_step() {
        let mut engine = engine("name: test");
        assert_eq!(
            engine.expand_input(".3n2e").sends,
            vec!["n", "n", "n", "e", "e"]
        );
    }

    /// The `speedwalks` field is what the session's one-time hint
    /// (UX_REVIEW.md G) is built from: the original path text paired with
    /// what it became. Empty when nothing in the input was a speedwalk.
    #[test]
    fn expand_input_reports_what_it_expanded_as_a_speedwalk() {
        let mut engine = engine("name: test");
        assert_eq!(
            engine.expand_input(".3n2e").speedwalks,
            vec![(
                ".3n2e".to_string(),
                vec![
                    "n".to_string(),
                    "n".to_string(),
                    "n".to_string(),
                    "e".to_string(),
                    "e".to_string()
                ]
            )]
        );
        assert_eq!(engine.expand_input("look").speedwalks, vec![]);
    }

    /// Two-letter diagonals are greedy: `ne` is one move, not `n` then `e`.
    #[test]
    fn a_speedwalk_diagonal_is_not_split_into_two_moves() {
        let mut engine = engine("name: test");
        assert_eq!(engine.expand_input(".2ne1d").sends, vec!["ne", "ne", "d"]);
    }

    /// Text that merely starts with `.` but isn't a valid path — no
    /// direction after a digit run, or no leading digits at all that
    /// resolve to a direction — is sent exactly as typed.
    #[test]
    fn text_that_only_looks_like_a_speedwalk_is_sent_unchanged() {
        let mut engine = engine("name: test");
        assert_eq!(engine.expand_input(".hello").sends, vec![".hello"]);
        assert_eq!(engine.expand_input(".").sends, vec!["."]);
        assert_eq!(engine.expand_input("3n2e").sends, vec!["3n2e"]);
    }

    /// A stored alias is a speedwalk macro for free: its `send:` output
    /// goes through the same expansion as anything typed directly.
    #[test]
    fn an_alias_can_store_a_speedwalk_macro() {
        let mut engine = engine(
            r#"
            name: test
            aliases:
              - pattern: '^home$'
                send: [".2s1w"]
            "#,
        );
        assert_eq!(engine.expand_input("home").sends, vec!["s", "s", "w"]);
    }

    /// A count or a total past the sanity bound falls back to the literal
    /// text rather than queuing a send storm (§13).
    #[test]
    fn an_oversized_speedwalk_count_is_not_expanded() {
        let mut engine = engine("name: test");
        assert_eq!(engine.expand_input(".99999999n").sends, vec![".99999999n"]);
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
        assert_eq!(engine.expand_input("a").sends, vec!["b"]);
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

    // ---- server data (§6.3/§14 M6) ----

    #[test]
    fn templates_read_live_server_data() {
        let mut engine = engine(
            r#"
            name: test
            aliases:
              - pattern: '^hp$'
                send: ["hp is ${Char.Vitals.hp}"]
            "#,
        );
        engine.update_server_data_from_gmcp("Char.Vitals.hp", "87".to_string());
        assert_eq!(engine.expand_input("hp").sends, vec!["hp is 87"]);
    }

    /// A shorter array's leftover indices go, along with anything nested
    /// under them — `Char.Group.1.name` belongs to the element that left.
    #[test]
    fn pruning_an_array_drops_stale_indices_and_their_nested_keys() {
        let mut engine = engine("name: test");
        for (key, value) in [
            ("Char.Group.0.name", "Grunk"),
            ("Char.Group.1.name", "Bob"),
            ("Char.Group.1.hp", "50"),
            ("Char.Group.2.name", "Ann"),
        ] {
            engine.update_server_data_from_gmcp(key, value.to_string());
        }

        engine.prune_gmcp_array("Char.Group", 1);

        assert_eq!(
            engine.server_data.get("Char.Group.0.name"),
            Some(&"Grunk".to_string())
        );
        for gone in ["Char.Group.1.name", "Char.Group.1.hp", "Char.Group.2.name"] {
            assert_eq!(engine.server_data.get(gone), None, "{gone} survived");
        }
    }

    /// The prune is deliberately narrower than replacing a package's whole
    /// subtree: a server sending a partial object update must not lose the
    /// keys it did not mention. Only indices are eligible.
    #[test]
    fn pruning_an_array_leaves_sibling_and_object_keys_alone() {
        let mut engine = engine("name: test");
        for (key, value) in [
            ("Char.Vitals.hp", "90"),
            ("Char.Vitals.maxhp", "100"),
            ("Char.Affects.0", "haste"),
            // A key whose path merely *starts* like the array's.
            ("Char.AffectsCount", "1"),
        ] {
            engine.update_server_data_from_gmcp(key, value.to_string());
        }

        engine.prune_gmcp_array("Char.Affects", 1);

        for kept in [
            "Char.Vitals.hp",
            "Char.Vitals.maxhp",
            "Char.Affects.0",
            "Char.AffectsCount",
        ] {
            assert!(engine.server_data.contains_key(kept), "{kept} was pruned");
        }
    }

    /// MSDP arrays are positional too, so they go stale exactly as GMCP's
    /// do — but MSDP must not expire a key GMCP owns, since a server
    /// offering both has GMCP preferred (§6.3) and MSDP was never allowed
    /// to write it in the first place.
    #[test]
    fn pruning_an_msdp_array_spares_the_keys_gmcp_owns() {
        let mut engine = engine("name: test");
        engine.update_server_data_from_msdp("Room.Exits.0", "north".to_string());
        engine.update_server_data_from_msdp("Room.Exits.1", "south".to_string());
        // GMCP claims this one; MSDP's own write is already a no-op.
        engine.update_server_data_from_gmcp("Room.Exits.2", "up".to_string());

        engine.prune_msdp_array("Room.Exits", 1);

        assert_eq!(
            engine.server_data.get("Room.Exits.0"),
            Some(&"north".to_string())
        );
        assert_eq!(engine.server_data.get("Room.Exits.1"), None, "stale MSDP");
        assert_eq!(
            engine.server_data.get("Room.Exits.2"),
            Some(&"up".to_string()),
            "GMCP owns this key; MSDP must not expire it"
        );
    }

    #[test]
    fn a_later_server_data_update_overwrites_the_earlier_value() {
        let mut engine = engine(
            r#"
            name: test
            aliases:
              - pattern: '^hp$'
                send: ["hp is ${Char.Vitals.hp}"]
            "#,
        );
        engine.update_server_data_from_gmcp("Char.Vitals.hp", "87".to_string());
        engine.update_server_data_from_gmcp("Char.Vitals.hp", "42".to_string());
        assert_eq!(engine.expand_input("hp").sends, vec!["hp is 42"]);
    }

    /// A rule author's own variable of the same name wins over live server
    /// data, so server data can never silently shadow a name a rule set.
    #[test]
    fn a_rule_defined_variable_shadows_server_data_of_the_same_name() {
        let mut engine = engine(
            r#"
            name: test
            variables:
              hp: unknown
            aliases:
              - pattern: '^hp$'
                send: ["hp is ${hp}"]
            "#,
        );
        engine.update_server_data_from_gmcp("hp", "87".to_string());
        assert_eq!(engine.expand_input("hp").sends, vec!["hp is unknown"]);
    }

    /// The milestone's actual acceptance wording (§14 M6): a *trigger* — an
    /// inbound-line match via `process_line`, not an alias via
    /// `expand_input` — reacts to live server data in its `send`.
    #[test]
    fn a_trigger_reacts_to_live_server_data() {
        let mut engine = engine(
            r#"
            name: test
            triggers:
              - pattern: '^low hp$'
                send: ["quaff heal, currently ${Char.Vitals.hp}"]
            "#,
        );
        engine.update_server_data_from_gmcp("Char.Vitals.hp", "12".to_string());
        assert_eq!(
            engine.process_line("low hp").sends,
            vec!["quaff heal, currently 12"]
        );
    }

    /// A trigger's `set:` can also capture server data into a plain
    /// variable for later rules to read.
    #[test]
    fn a_trigger_can_copy_server_data_into_a_variable() {
        let mut engine = engine(
            r#"
            name: test
            triggers:
              - pattern: '^snapshot$'
                set:
                  last_hp: "${Char.Vitals.hp}"
            aliases:
              - pattern: '^recall$'
                send: ["it was ${last_hp}"]
            "#,
        );
        engine.update_server_data_from_gmcp("Char.Vitals.hp", "55".to_string());
        engine.process_line("snapshot");
        assert_eq!(engine.expand_input("recall").sends, vec!["it was 55"]);
    }

    /// §6.3: "where a server offers both, GMCP is preferred" — an MSDP
    /// update must not clobber a key GMCP already set.
    #[test]
    fn gmcp_data_is_not_overwritten_by_a_later_msdp_update_of_the_same_key() {
        let mut engine = engine(
            r#"
            name: test
            aliases:
              - pattern: '^hp$'
                send: ["hp is ${hp}"]
            "#,
        );
        engine.update_server_data_from_gmcp("hp", "87".to_string());
        engine.update_server_data_from_msdp("hp", "0".to_string());
        assert_eq!(engine.expand_input("hp").sends, vec!["hp is 87"]);
    }

    /// The reverse is fine: GMCP data always overrides an earlier MSDP
    /// value for the same key.
    #[test]
    fn gmcp_data_overwrites_an_earlier_msdp_value_for_the_same_key() {
        let mut engine = engine(
            r#"
            name: test
            aliases:
              - pattern: '^hp$'
                send: ["hp is ${hp}"]
            "#,
        );
        engine.update_server_data_from_msdp("hp", "0".to_string());
        engine.update_server_data_from_gmcp("hp", "87".to_string());
        assert_eq!(engine.expand_input("hp").sends, vec!["hp is 87"]);
    }

    /// MSDP-only keys (no GMCP equivalent) still land normally.
    #[test]
    fn msdp_data_populates_keys_gmcp_never_touched() {
        let mut engine = engine(
            r#"
            name: test
            aliases:
              - pattern: '^room$'
                send: ["you are in ${room_name}"]
            "#,
        );
        engine.update_server_data_from_msdp("room_name", "The Bazaar".to_string());
        assert_eq!(
            engine.expand_input("room").sends,
            vec!["you are in The Bazaar"]
        );
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
        assert_eq!(engine.expand_input("x").sends, vec!["alias fired"]);
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

    // ---- cross-session actions (docs/ARCHITECTURE.md §7.5) ----

    /// `send_to` is a separate output from `send`: the engine never mixes a
    /// command meant for another character into this session's stream.
    #[test]
    fn send_to_is_kept_apart_from_this_sessions_sends() {
        let mut engine = Engine::compile(&[module(
            r#"
name: t
triggers:
  - pattern: '^HP: (?P<hp>\d+)%'
    send: ["quaff heal"]
    send_to:
      cleric: ["cast 'major heal' ${hp}"]
"#,
        )])
        .unwrap();

        let outcome = engine.process_line("HP: 30%");
        assert_eq!(outcome.sends, vec!["quaff heal"]);
        assert_eq!(
            outcome.send_to,
            vec![CrossSend {
                target: "cleric".to_string(),
                lines: vec!["cast 'major heal' 30".to_string()],
            }]
        );
    }

    /// A later scope layer can patch just the `send_to` of an inherited
    /// rule, the same as any other field (§7.3).
    #[test]
    fn a_profile_can_override_an_inherited_send_to() {
        let mut engine = Engine::compile(&[
            module(
                r#"
name: global
triggers:
  - id: heal-me
    pattern: 'low health'
    send_to:
      cleric: ["heal tank"]
"#,
            ),
            module(
                r#"
name: profile
triggers:
  - id: heal-me
    send_to:
      mage: ["shield tank"]
"#,
            ),
        ])
        .unwrap();

        let outcome = engine.process_line("you are at low health");
        assert_eq!(outcome.send_to.len(), 1);
        assert_eq!(outcome.send_to[0].target, "mage");
    }

    /// `${...}` in a cross-session command resolves against live server data
    /// too, so a GMCP vital can drive another character's action.
    #[test]
    fn send_to_expands_server_data() {
        let mut engine = Engine::compile(&[module(
            r#"
name: t
triggers:
  - pattern: 'wounded'
    send_to:
      cleric: ["heal ${Char.Base.name}"]
"#,
        )])
        .unwrap();
        engine.update_server_data_from_gmcp("Char.Base.name", "Grunk".to_string());

        let outcome = engine.process_line("you are wounded");
        assert_eq!(outcome.send_to[0].lines, vec!["heal Grunk"]);
    }

    // ---- channel routing (docs/ARCHITECTURE.md §11.1) ----

    #[test]
    fn a_routed_trigger_names_its_channel_and_can_gag_the_line() {
        let mut engine = Engine::compile(&[module(
            r#"
name: t
triggers:
  - pattern: 'tells you'
    route: comms
    gag: true
"#,
        )])
        .unwrap();

        let outcome = engine.process_line("Bob tells you hi");
        assert_eq!(outcome.route.as_deref(), Some("comms"));
        assert!(outcome.gag);

        let outcome = engine.process_line("You see a rat.");
        assert_eq!(outcome.route, None);
    }

    /// A line belongs in one channel. Rules fire in scope order, so the
    /// first match — the lowest layer that classified it — decides, and a
    /// second matching rule cannot silently move it.
    #[test]
    fn the_first_matching_route_wins() {
        let mut engine = Engine::compile(&[module(
            r#"
name: t
triggers:
  - pattern: 'tells you'
    route: comms
  - pattern: 'Bob'
    route: spam
"#,
        )])
        .unwrap();

        assert_eq!(
            engine.process_line("Bob tells you hi").route.as_deref(),
            Some("comms")
        );
    }

    // ---- highlights (docs/ARCHITECTURE.md §7.7) ----

    /// §7.7's own example: the matched text only, as a range over the
    /// stripped line plus the SGR the session will splice in.
    #[test]
    fn a_highlight_covers_the_matched_text_and_carries_its_sgr() {
        let mut engine = Engine::compile(&[module(
            r#"
name: t
triggers:
  - pattern: '\bKestrel\b'
    highlight: {fg: bright_yellow, bold: true}
"#,
        )])
        .unwrap();

        assert_eq!(
            engine.process_line("You see Kestrel here.").highlights,
            vec![(8..15, "1;93".to_string())]
        );
        assert!(engine.process_line("You see a rat.").highlights.is_empty());
    }

    #[test]
    fn whole_line_covers_the_line_however_little_the_pattern_matched() {
        let mut engine = Engine::compile(&[module(
            r#"
name: t
triggers:
  - pattern: '^You are bleeding'
    highlight: {fg: white, bg: red, whole_line: true}
"#,
        )])
        .unwrap();

        assert_eq!(
            engine.process_line("You are bleeding badly.").highlights,
            vec![(0..23, "97;41".to_string())]
        );
    }

    #[test]
    fn palette_indexes_and_hex_colours_compile_to_extended_sgr() {
        let mut engine = Engine::compile(&[module(
            r#"
name: t
triggers:
  - pattern: 'rare'
    highlight: {fg: 208, bg: '#102030', underline: true}
"#,
        )])
        .unwrap();

        assert_eq!(
            engine.process_line("a rare drop").highlights,
            vec![(2..6, "4;38;5;208;48;2;16;32;48".to_string())]
        );
    }

    /// Nested SGR has no defined "restore to the middle state", so an
    /// overlapping span is dropped whole rather than nested — the same
    /// first-match-wins rule `route:` follows.
    #[test]
    fn an_overlapping_highlight_is_dropped_rather_than_nested() {
        let mut engine = Engine::compile(&[module(
            r#"
name: t
triggers:
  - pattern: 'Kestrel the'
    highlight: {bold: true}
  - pattern: 'the Bold'
    highlight: {fg: red}
  - pattern: 'arrives'
    highlight: {fg: cyan}
"#,
        )])
        .unwrap();

        assert_eq!(
            engine.process_line("Kestrel the Bold arrives.").highlights,
            vec![(0..11, "1".to_string()), (17..24, "36".to_string())],
            "the overlapping span goes, the disjoint one stays"
        );
    }

    /// A gagged line never reaches the scrollback, so its highlights are
    /// moot — but computing them anyway keeps the two actions independent.
    #[test]
    fn a_gagged_line_still_reports_its_highlights() {
        let mut engine = Engine::compile(&[module(
            r#"
name: t
triggers:
  - pattern: 'spam'
    highlight: {bold: true}
    gag: true
"#,
        )])
        .unwrap();

        let outcome = engine.process_line("spam here");
        assert!(outcome.gag);
        assert_eq!(outcome.highlights, vec![(0..4, "1".to_string())]);
    }

    // ---- bell (§14 M9) ----

    #[test]
    fn a_bell_trigger_flags_the_outcome() {
        let mut engine = Engine::compile(&[module(
            r#"
name: t
triggers:
  - pattern: 'You have been slain'
    bell: true
"#,
        )])
        .unwrap();

        assert!(engine.process_line("You have been slain by a rat.").bell);
        assert!(!engine.process_line("You see a rat.").bell);
    }

    /// A gagged line can still be worth an alert — `bell:` and `gag:` are
    /// independent, unlike `highlight:` which only matters if the line is
    /// shown at all.
    #[test]
    fn a_bell_trigger_still_rings_when_the_line_is_gagged() {
        let mut engine = Engine::compile(&[module(
            r#"
name: t
triggers:
  - pattern: 'tick'
    gag: true
    bell: true
"#,
        )])
        .unwrap();

        let outcome = engine.process_line("tick");
        assert!(outcome.gag);
        assert!(outcome.bell);
    }

    #[test]
    fn a_later_layer_patches_a_highlight_by_id() {
        let mut engine = layers(&[
            r#"
            name: module
            triggers:
              - id: my-name
                pattern: 'Kestrel'
                highlight: {fg: bright_yellow}
            "#,
            r#"
            name: profile
            triggers:
              - id: my-name
                highlight: {fg: red, bold: true}
            "#,
        ]);
        assert_eq!(
            engine.process_line("Kestrel waves").highlights,
            vec![(0..7, "1;31".to_string())],
            "the profile's block replaces the module's whole"
        );

        // And a patch that restates neither keeps the inherited one.
        let mut engine = layers(&[
            r#"
            name: module
            triggers:
              - id: my-name
                pattern: 'Kestrel'
                highlight: {fg: bright_yellow}
            "#,
            "name: profile\ntriggers:\n  - id: my-name\n    send: [\"wave\"]\n",
        ]);
        let outcome = engine.process_line("Kestrel waves");
        assert_eq!(outcome.sends, vec!["wave"]);
        assert_eq!(outcome.highlights, vec![(0..7, "93".to_string())]);
    }

    #[test]
    fn a_highlight_that_styles_nothing_is_a_load_error_naming_the_rule() {
        let err = Engine::compile(&[module(
            r#"
name: t
triggers:
  - id: low-hp
    pattern: 'bleeding'
    highlight: {whole_line: true}
"#,
        )])
        .unwrap_err();

        let message = err.to_string();
        assert!(matches!(err, EngineError::BadHighlight { .. }), "{message}");
        assert!(message.contains("low-hp"), "{message}");
    }

    /// An id-less rule is named by the pattern the player wrote, which is
    /// the only identity it has.
    #[test]
    fn a_bad_highlight_colour_is_a_load_error_naming_the_rule() {
        let err = Engine::compile(&[module(
            r#"
name: t
triggers:
  - pattern: '\bKestrel\b'
    highlight: {fg: chartreuse}
"#,
        )])
        .unwrap_err();

        let message = err.to_string();
        assert!(matches!(err, EngineError::BadHighlight { .. }), "{message}");
        assert!(message.contains("chartreuse"), "{message}");
        assert!(message.contains("Kestrel"), "{message}");
    }

    /// §7.6's own example: a threshold the pattern cannot express, read
    /// from a capture and a variable.
    #[test]
    fn a_guard_gates_a_trigger_on_a_capture_and_a_variable() {
        let mut engine = engine(
            r#"
            name: test
            variables:
              heal_at: "40"
            triggers:
              - pattern: '^Your health: (?P<hp>\d+)%'
                when: '${hp} < ${heal_at}'
                send: ["quaff heal"]
            "#,
        );
        assert_eq!(
            engine.process_line("Your health: 30%").sends,
            vec!["quaff heal"]
        );
        assert!(engine.process_line("Your health: 90%").sends.is_empty());
    }

    #[test]
    fn a_guard_can_read_server_data() {
        let mut engine = engine(
            r#"
            name: test
            triggers:
              - pattern: 'The dragon roars'
                when: '${Char.Vitals.hp} < 50'
                send: ["flee"]
            "#,
        );
        engine.update_server_data_from_gmcp("Char.Vitals.hp", "80".to_string());
        assert!(engine.process_line("The dragon roars").sends.is_empty());
        engine.update_server_data_from_gmcp("Char.Vitals.hp", "20".to_string());
        assert_eq!(engine.process_line("The dragon roars").sends, vec!["flee"]);
    }

    /// A guarded-out trigger does nothing at all: the line reaches the
    /// scrollback it would otherwise have been gagged from or routed away.
    #[test]
    fn a_guarded_out_trigger_neither_gags_nor_routes() {
        let mut engine = engine(
            r#"
            name: test
            variables:
              quiet: "0"
            triggers:
              - pattern: 'tells you'
                when: '${quiet} == 1'
                gag: true
                route: comms
            "#,
        );
        let outcome = engine.process_line("Bob tells you hi");
        assert!(!outcome.gag);
        assert_eq!(outcome.route, None);
    }

    /// A guard that fails is not a match, so the next alias — or the
    /// literal input — still gets its turn.
    #[test]
    fn a_guarded_out_alias_falls_through_to_the_next_one() {
        let mut engine = engine(
            r#"
            name: test
            variables:
              mounted: "0"
            aliases:
              - id: ride
                pattern: '^go$'
                when: '${mounted} == 1'
                send: ["ride north"]
              - id: walk
                pattern: '^go$'
                send: ["walk north"]
            "#,
        );
        assert_eq!(engine.expand_input("go").sends, vec!["walk north"]);
    }

    #[test]
    fn an_undefined_name_in_a_guard_stops_the_rule_firing() {
        let mut engine = engine(
            r#"
            name: test
            triggers:
              - pattern: 'The dragon roars'
                when: '${Char.Vitals.hp} < 50'
                send: ["flee"]
            "#,
        );
        assert!(engine.process_line("The dragon roars").sends.is_empty());
    }

    /// Malformed guards fail at load with module context, like an invalid
    /// pattern, rather than silently never firing at runtime (§7.6).
    #[test]
    fn a_malformed_guard_is_a_compile_error() {
        let err = Engine::compile(&[module(
            r#"
            name: test
            triggers:
              - pattern: 'x'
                when: '${hp} =! 40'
                send: ["y"]
            "#,
        )])
        .expect_err("a malformed condition should not compile");
        assert!(matches!(err, EngineError::BadCondition { .. }));
    }

    #[test]
    fn a_later_layer_inherits_and_can_replace_a_guard() {
        let base = module(
            r#"
            name: base
            triggers:
              - id: heal
                pattern: '^Your health: (?P<hp>\d+)%'
                when: '${hp} < 40'
                send: ["quaff heal"]
            "#,
        );
        let patch = module(
            r#"
            name: profile
            triggers:
              - id: heal
                when: '${hp} < 80'
            "#,
        );

        let mut inherited = Engine::compile(std::slice::from_ref(&base)).expect("compiles");
        assert!(inherited.process_line("Your health: 60%").sends.is_empty());

        let mut patched = Engine::compile(&[base, patch]).expect("compiles");
        assert_eq!(
            patched.process_line("Your health: 60%").sends,
            vec!["quaff heal"]
        );
    }

    /// A module whose script name has no engine behind it must say so at
    /// load, not stay silent while its hooks never run.
    #[test]
    fn a_script_in_an_unknown_language_fails_to_compile() {
        let mut layer = module("name: test");
        layer.script_sources.push(ScriptSource {
            name: "combat.rb".to_string(),
            code: String::new(),
        });

        let err = Engine::compile(&[layer]).unwrap_err();
        assert!(
            matches!(&err, EngineError::UnknownScriptLanguage { script, .. } if script == "combat.rb"),
            "{err}"
        );
    }

    // ---- peer snapshots (docs/ARCHITECTURE.md §7.5) ----

    /// A session with one peer, whose published state the test controls.
    fn with_peer(yaml: &str, peer: &str, snapshot: PeerSnapshot) -> Engine {
        let (tx, rx) = watch::channel(snapshot);
        // The sender outlives the engine for the length of the test: a
        // closed channel would just freeze the last value, which is not
        // what these are checking.
        Box::leak(Box::new(tx));
        let mut engine = engine(yaml);
        engine.set_peers(Peers::from([(peer.to_string(), rx)]));
        engine
    }

    fn snapshot(vars: &[(&str, &str)], data: &[(&str, &str)]) -> PeerSnapshot {
        let owned = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect()
        };
        PeerSnapshot {
            vars: owned(vars),
            data: owned(data),
        }
    }

    /// The §7.5 case: the cleric's own rule watches the tank's vitals, so
    /// the response lives with the character that acts.
    #[test]
    fn a_guard_reads_a_peers_server_data() {
        let mut engine = with_peer(
            r#"
            name: test
            triggers:
              - pattern: '^You feel rested'
                when: '${@tank.Char.Vitals.hp} < 40'
                send: ["cast 'major heal' Grunk"]
            "#,
            "tank",
            snapshot(&[], &[("Char.Vitals.hp", "30")]),
        );

        assert_eq!(
            engine.process_line("You feel rested.").sends,
            vec!["cast 'major heal' Grunk"]
        );
    }

    #[test]
    fn a_template_expands_a_peers_variable() {
        let mut engine = with_peer(
            r#"
            name: test
            aliases:
              - pattern: '^help$'
                send: ["cast heal ${@tank.target}"]
            "#,
            "tank",
            snapshot(&[("target", "kobold")], &[]),
        );

        assert_eq!(engine.expand_input("help").sends, vec!["cast heal kobold"]);
    }

    /// An unknown peer, or an unknown key on a known one, resolves to
    /// nothing — which leaves a template visibly unexpanded and makes a
    /// guard false (§7.6), exactly as a local name would.
    #[test]
    fn an_unknown_peer_resolves_to_nothing() {
        let mut engine = with_peer(
            r#"
            name: test
            triggers:
              - pattern: '^guarded$'
                when: '${@ghost.hp} < 40'
                send: ["never"]
              - pattern: '^expanded$'
                send: ["say ${@ghost.hp}/${@tank.missing}"]
            "#,
            "tank",
            snapshot(&[("hp", "30")], &[]),
        );

        assert!(engine.process_line("guarded").sends.is_empty());
        assert_eq!(
            engine.process_line("expanded").sends,
            vec!["say ${@ghost.hp}/${@tank.missing}"]
        );
    }

    /// A peer name cannot shadow local state, and local state cannot answer
    /// for a peer: the `@` prefix is what separates the two namespaces.
    #[test]
    fn peer_state_and_local_state_are_separate_namespaces() {
        let mut engine = with_peer(
            r#"
            name: test
            variables:
              hp: "90"
            aliases:
              - pattern: '^both$'
                send: ["say ${hp} ${@tank.hp}"]
            "#,
            "tank",
            snapshot(&[("hp", "30")], &[]),
        );

        assert_eq!(engine.expand_input("both").sends, vec!["say 90 30"]);
    }

    /// `add_peer` inserts one more without disturbing the rest — the
    /// mechanism `/connect` uses to tell an already-running session about
    /// a character added after it (§7.5, `ARCH_REVIEW.md` "Features that
    /// would break the architecture").
    #[test]
    fn add_peer_inserts_without_disturbing_existing_peers() {
        let mut engine = with_peer(
            r#"
            name: test
            aliases:
              - pattern: '^both$'
                send: ["say ${@tank.hp} ${@cleric.hp}"]
            "#,
            "tank",
            snapshot(&[("hp", "30")], &[]),
        );

        let (tx, rx) = watch::channel(snapshot(&[("hp", "80")], &[]));
        Box::leak(Box::new(tx));
        engine.add_peer("cleric".to_string(), rx);

        assert_eq!(engine.expand_input("both").sends, vec!["say 30 80"]);
    }

    /// A snapshot is published only when this session's state has actually
    /// moved, so a quiet character costs its peers nothing.
    #[test]
    fn a_snapshot_is_published_only_after_a_change() {
        let mut engine = engine(
            r#"
            name: test
            triggers:
              - pattern: '^You are now fighting (?P<foe>\w+)'
                set: {target: "${foe}"}
            "#,
        );
        assert!(engine.take_snapshot().is_none(), "nothing has happened yet");

        engine.process_line("A leaf falls.");
        assert!(engine.take_snapshot().is_none(), "no rule fired");

        engine.process_line("You are now fighting kobold");
        let published = engine.take_snapshot().expect("the target changed");
        assert_eq!(
            published.vars.get("target").map(String::as_str),
            Some("kobold")
        );
        assert!(engine.take_snapshot().is_none(), "already published");

        engine.update_server_data_from_gmcp("Char.Vitals.hp", "30".to_string());
        let published = engine.take_snapshot().expect("server data changed");
        assert_eq!(
            published.data.get("Char.Vitals.hp").map(String::as_str),
            Some("30")
        );
        engine.update_server_data_from_gmcp("Char.Vitals.hp", "30".to_string());
        assert!(
            engine.take_snapshot().is_none(),
            "the same value is no news"
        );
    }

    /// A build without the engine a script needs must say which engine is
    /// missing — the script is fine, this binary is not the one to run it.
    #[cfg(not(feature = "lua"))]
    #[test]
    fn a_lua_script_without_a_lua_engine_says_which_engine_is_missing() {
        let mut layer = module("name: test");
        layer.script_sources.push(ScriptSource {
            name: "combat.lua".to_string(),
            code: String::new(),
        });

        let err = Engine::compile(&[layer]).unwrap_err();
        assert!(
            matches!(&err, EngineError::ScriptEngineMissing { language, .. } if *language == "lua"),
            "{err}"
        );
    }

    #[cfg(feature = "js")]
    #[test]
    fn a_javascript_module_is_hosted_like_a_lua_one() {
        let mut layer = module(
            r#"
            name: test
            triggers:
              - pattern: '^(\w+) is DEAD!$'
                script: {file: combat.js, fn: onDeath}
            "#,
        );
        layer.script_sources.push(ScriptSource {
            name: "combat.js".to_string(),
            code: r#"function onDeath(_, caps) { mud.send("loot " + caps[1]); }"#.to_string(),
        });

        let mut engine = Engine::compile(&[layer]).expect("compiles");
        assert_eq!(
            engine.process_line("kobold is DEAD!").sends,
            vec!["loot kobold"]
        );
    }

    /// Two languages in one session get one host each, and a line reaches
    /// both — the engine never asks which language it is dispatching to.
    #[cfg(all(feature = "lua", feature = "js"))]
    #[test]
    fn two_languages_share_one_session() {
        let mut layer = module("name: test");
        layer.script_sources.push(ScriptSource {
            name: "a.lua".to_string(),
            code: r#"mud.on_line(function() mud.send("from lua") end)"#.to_string(),
        });
        layer.script_sources.push(ScriptSource {
            name: "b.js".to_string(),
            code: r#"mud.on_line(function () { mud.send("from js"); });"#.to_string(),
        });

        let mut engine = Engine::compile(&[layer]).expect("compiles");
        assert_eq!(
            engine.process_line("anything").sends,
            vec!["from lua", "from js"]
        );
    }

    #[cfg(feature = "lua")]
    mod scripted {
        use super::*;

        fn scripted_engine(yaml: &str, lua: &str) -> Engine {
            let mut layer = module(yaml);
            layer.script_sources.push(ScriptSource {
                name: "test.lua".to_string(),
                code: lua.to_string(),
            });
            Engine::compile(&[layer]).expect("compiles")
        }

        /// The M8 acceptance case in the other direction from `when:`: what
        /// YAML cannot express, a script can, over the same state.
        #[test]
        fn a_script_hook_reads_the_rules_own_variables_and_server_data() {
            let mut engine = scripted_engine(
                r#"
                name: test
                variables:
                  heal_at: "40"
                triggers:
                  - pattern: '^Your health: (?P<hp>\d+)%'
                    set: {hp: "${hp}"}
                "#,
                r#"
                mud.on_line(function()
                  local hp = tonumber(mud.get("hp") or "100")
                  if hp < tonumber(mud.get("heal_at")) and mud.data("Char.Status.combat") == "1" then
                    mud.send("quaff heal")
                  end
                end)
                "#,
            );
            engine.update_server_data_from_gmcp("Char.Status.combat", "1".to_string());

            // The trigger's `set:` runs first, so the hook sees this line's
            // health rather than the previous line's.
            assert_eq!(
                engine.process_line("Your health: 30%").sends,
                vec!["quaff heal"]
            );
            assert!(engine.process_line("Your health: 90%").sends.is_empty());
        }

        #[test]
        fn a_script_variable_is_visible_to_the_rules() {
            let mut engine = scripted_engine(
                r#"
                name: test
                aliases:
                  - pattern: '^t$'
                    send: ["kill ${quarry}"]
                "#,
                r#"mud.on_line(function(line) mud.set("quarry", line) end)"#,
            );

            engine.process_line("kobold");
            assert_eq!(engine.expand_input("t").sends, vec!["kill kobold"]);
        }

        #[test]
        fn a_script_can_gag_and_substitute_a_line() {
            let mut engine = scripted_engine(
                "name: test",
                r#"
                mud.on_line(function(line)
                  if line == "spam" then mud.gag() end
                  if line == "raw" then mud.substitute("polished") end
                end)
                "#,
            );

            assert!(engine.process_line("spam").gag);
            assert_eq!(
                engine.process_line("raw").substitute.as_deref(),
                Some("polished")
            );
            assert!(engine.process_line("plain").substitute.is_none());
        }

        #[test]
        fn lifecycle_and_server_data_hooks_reach_the_same_script() {
            let mut engine = scripted_engine(
                "name: test",
                r#"
                mud.on_connect(function() mud.send("look") end)
                mud.on_prompt(function(text) mud.echo("prompt: " .. text) end)
                mud.on_gmcp(function(package, json) mud.echo(package .. "=" .. json) end)
                mud.on_disconnect(function() mud.echo("bye") end)
                "#,
            );

            assert_eq!(engine.on_connect().sends, vec!["look"]);
            assert_eq!(engine.process_prompt("HP:30").echoes, vec!["prompt: HP:30"]);
            assert_eq!(
                engine.process_gmcp("Char.Vitals", r#"{"hp":30}"#).echoes,
                vec![r#"Char.Vitals={"hp":30}"#]
            );
            assert_eq!(engine.on_disconnect().echoes, vec!["bye"]);
        }

        /// A script that stops working must be as visible as a trigger that
        /// stops firing — in the player's scrollback, with the line's own
        /// rules still honoured.
        #[test]
        fn a_failing_hook_reports_itself_without_stopping_the_line() {
            let mut engine = scripted_engine(
                r#"
                name: test
                triggers:
                  - pattern: '^ouch$'
                    send: ["quaff heal"]
                "#,
                r#"mud.on_line(function() error("boom") end)"#,
            );

            let outcome = engine.process_line("ouch");
            assert_eq!(outcome.sends, vec!["quaff heal"]);
            assert_eq!(outcome.echoes.len(), 1, "{:?}", outcome.echoes);
            assert!(outcome.echoes[0].contains("line"), "{:?}", outcome.echoes);
        }

        /// Scripts layer like rules do (§7.3), and later layers see what
        /// earlier ones left behind.
        #[test]
        fn scripts_load_in_scope_order_into_one_vm() {
            let mut shared = module("name: shared");
            shared.script_sources.push(ScriptSource {
                name: "shared.lua".to_string(),
                code: "greeting = 'hello'".to_string(),
            });
            let mut profile = module("name: profile");
            profile.script_sources.push(ScriptSource {
                name: "profile.lua".to_string(),
                code: r#"mud.on_line(function() mud.send(greeting) end)"#.to_string(),
            });

            let mut engine = Engine::compile(&[shared, profile]).expect("compiles");
            assert_eq!(engine.process_line("x").sends, vec!["hello"]);
        }

        /// The `script:` action from §7.4: a rule matches, and the function
        /// it names gets the line and its captures.
        #[test]
        fn a_rule_calls_a_script_function_with_its_captures() {
            let mut engine = scripted_engine(
                r#"
                name: test
                triggers:
                  - pattern: '^(?P<killer>\w+) killed (\w+)!$'
                    script: {file: test.lua, fn: on_death}
                "#,
                r#"
                function on_death(line, caps)
                  mud.echo(line)
                  mud.send("say " .. caps.killer .. " got " .. caps[2])
                end
                "#,
            );

            let outcome = engine.process_line("Grunk killed kobold!");
            assert_eq!(outcome.echoes, vec!["Grunk killed kobold!"]);
            assert_eq!(outcome.sends, vec!["say Grunk got kobold"]);
        }

        #[test]
        fn an_alias_can_call_a_script_function() {
            let mut engine = scripted_engine(
                r#"
                name: test
                aliases:
                  - pattern: '^hunt (\w+)$'
                    script: {file: test.lua, fn: hunt}
                "#,
                r#"function hunt(_, caps) mud.send("kill " .. caps[1]) end"#,
            );

            assert_eq!(engine.expand_input("hunt rat").sends, vec!["kill rat"]);
        }

        /// A rule's script action runs after the trigger table, so it reads
        /// what this line's rules just set — the same order the `on_line`
        /// hooks see.
        #[test]
        fn a_script_action_sees_the_variables_its_line_set() {
            let mut engine = scripted_engine(
                r#"
                name: test
                triggers:
                  - pattern: '^You are now fighting (?P<foe>\w+)'
                    set: {target: "${foe}"}
                  - pattern: '^You are now fighting'
                    script: {file: test.lua, fn: engaged}
                "#,
                r#"function engaged() mud.send("consider " .. mud.get("target")) end"#,
            );

            assert_eq!(
                engine.process_line("You are now fighting kobold").sends,
                vec!["consider kobold"]
            );
        }

        /// A script's timer is armed against the same clock the session
        /// already sleeps on, so `mud.timer` needs nothing of the session
        /// that `timers:` did not already need (§7.4).
        #[test]
        fn a_script_timer_shares_the_engines_clock() {
            let mut engine = scripted_engine(
                r#"
                name: test
                timers:
                  - id: autosave
                    every: 5m
                    send: ["save"]
                "#,
                r#"
                mud.on_connect(function()
                  mud.timer(30, function() mud.send("stand") end)
                end)
                "#,
            );

            let start = Instant::now();
            engine.start_timers(start);
            assert_eq!(
                engine.next_timer_deadline(),
                Some(start + Duration::from_secs(300)),
                "only the YAML timer is armed before the hook runs"
            );

            engine.on_connect();
            assert!(
                engine.next_timer_deadline().expect("a deadline")
                    <= start + Duration::from_secs(31),
                "the script's timer is the earlier one now"
            );

            // Nothing is due yet, and the YAML timer is unaffected either
            // way.
            assert!(
                engine
                    .fire_due_script_timers(start + Duration::from_secs(29))
                    .sends
                    .is_empty()
            );
            let outcome = engine.fire_due_script_timers(start + Duration::from_secs(31));
            assert_eq!(outcome.sends, vec!["stand"]);

            // One-shot: with the script timer spent, the YAML timer's
            // deadline is what is left.
            assert!(
                engine
                    .fire_due_script_timers(start + Duration::from_secs(60))
                    .sends
                    .is_empty()
            );
            assert_eq!(
                engine.next_timer_deadline(),
                Some(start + Duration::from_secs(300))
            );
        }

        /// A timer callback may arm the next one, which is how a script
        /// keeps a heartbeat without a repeating timer to leak.
        #[test]
        fn a_timer_callback_can_re_arm_itself() {
            let mut engine = scripted_engine(
                "name: test",
                r#"
                function beat()
                  mud.send("look")
                  mud.timer(10, beat)
                end
                mud.on_connect(function() mud.timer(10, beat) end)
                "#,
            );

            let start = Instant::now();
            engine.on_connect();
            for tick in 1..=3 {
                let at = start + Duration::from_secs(10 * tick + 1);
                assert_eq!(
                    engine.fire_due_script_timers(at).sends,
                    vec!["look"],
                    "beat {tick}"
                );
            }
        }

        /// The M8 acceptance case for §7.5: the cleric's own script watches
        /// the tank's GMCP affects and rebuffs when one drops — the
        /// reaction lives with the character that acts, in the session
        /// whose commands they are.
        #[test]
        fn a_cleric_script_rebuffs_off_the_tanks_affects() {
            let mut layer = module("name: cleric");
            layer.script_sources.push(ScriptSource {
                name: "cleric.lua".to_string(),
                code: r#"
                mud.on_peer("tank", "Char.Affects", function(key, value)
                  if value == "0" then
                    local spell = key:match("([^.]+)$")
                    mud.session("tank"):send("cast " .. spell .. " Grunk")
                  end
                end)
                "#
                .to_string(),
            });
            let mut engine = Engine::compile(&[layer]).expect("compiles");

            let (tank, rx) = watch::channel(snapshot(
                &[],
                &[("Char.Affects.bless", "1"), ("Char.Vitals.hp", "90")],
            ));
            engine.set_peers(Peers::from([("tank".to_string(), rx)]));

            // First poll: everything is new, but nothing has worn off.
            assert!(engine.poll_peer("tank").send_to.is_empty());

            // The blessing drops — and only that key is reported, so the
            // unchanged vitals do not wake the subscription.
            tank.send(snapshot(
                &[],
                &[("Char.Affects.bless", "0"), ("Char.Vitals.hp", "90")],
            ))
            .expect("the engine holds a receiver");
            let outcome = engine.poll_peer("tank");
            assert_eq!(
                outcome.send_to,
                vec![("tank".to_string(), vec!["cast bless Grunk".to_string()])]
            );

            // Nothing new: nothing fires.
            assert!(engine.poll_peer("tank").send_to.is_empty());
        }

        /// The other half of a buff dropping: when the affect is an array
        /// element rather than a `0`/`1` flag, the key does not change
        /// value — it *disappears*. Watching only the keys still present
        /// would report nothing at all, so a vanished key is reported with
        /// an empty value, matching what an absent `${@peer.key}` already
        /// resolves to.
        #[test]
        fn a_vanished_peer_key_is_reported_as_empty() {
            let mut layer = module("name: cleric");
            layer.script_sources.push(ScriptSource {
                name: "cleric.lua".to_string(),
                code: r#"
                mud.on_peer("tank", "Char.Affects", function(key, value)
                  if value == "" then
                    mud.session("tank"):send("cast bless Grunk")
                  end
                end)
                "#
                .to_string(),
            });
            let mut engine = Engine::compile(&[layer]).expect("compiles");

            let (tank, rx) = watch::channel(snapshot(
                &[],
                &[("Char.Affects.0", "bless"), ("Char.Affects.1", "haste")],
            ));
            engine.set_peers(Peers::from([("tank".to_string(), rx)]));
            assert!(engine.poll_peer("tank").send_to.is_empty());

            // The blessing wears off: the array shrinks, so `…​.1` is gone
            // rather than changed.
            tank.send(snapshot(&[], &[("Char.Affects.0", "haste")]))
                .expect("the engine holds a receiver");

            assert_eq!(
                engine.poll_peer("tank").send_to,
                vec![("tank".to_string(), vec!["cast bless Grunk".to_string()])]
            );
            assert!(engine.poll_peer("tank").send_to.is_empty(), "fires once");
        }

        /// A guard governs the script action like any other: no match, no
        /// call.
        #[test]
        fn a_guarded_out_rule_does_not_call_its_script() {
            let mut engine = scripted_engine(
                r#"
                name: test
                triggers:
                  - pattern: '^Your health: (?P<hp>\d+)%'
                    when: '${hp} < 40'
                    script: {file: test.lua, fn: hurt}
                "#,
                r#"function hurt() mud.send("quaff heal") end"#,
            );

            assert!(engine.process_line("Your health: 90%").sends.is_empty());
            assert_eq!(
                engine.process_line("Your health: 30%").sends,
                vec!["quaff heal"]
            );
        }

        /// A typo in `fn:` is a load error, not a rule that quietly never
        /// does anything — the same promise `when:` and `pattern:` make.
        #[test]
        fn a_rule_naming_a_function_that_does_not_exist_fails_at_load() {
            let mut layer = module(
                r#"
                name: test
                triggers:
                  - pattern: 'x'
                    script: {file: test.lua, fn: on_deth}
                "#,
            );
            layer.script_sources.push(ScriptSource {
                name: "test.lua".to_string(),
                code: "function on_death() end".to_string(),
            });

            let err = Engine::compile(&[layer]).unwrap_err();
            assert!(
                matches!(&err, EngineError::UnknownScriptFunction { function, .. } if function == "on_deth"),
                "{err}"
            );
        }

        #[test]
        fn a_rule_naming_an_undeclared_script_fails_at_load() {
            let layer = module(
                r#"
                name: test
                triggers:
                  - pattern: 'x'
                    script: {file: ghost.lua, fn: boo}
                "#,
            );

            let err = Engine::compile(&[layer]).unwrap_err();
            assert!(
                matches!(&err, EngineError::UndeclaredScript { script } if script == "ghost.lua"),
                "{err}"
            );
        }

        #[test]
        fn a_broken_script_names_its_file_and_module() {
            let mut layer = module("name: uw-combat");
            layer.script_sources.push(ScriptSource {
                name: "combat.lua".to_string(),
                code: "this is not lua".to_string(),
            });

            let err = Engine::compile(&[layer]).unwrap_err();
            let message = err.to_string();
            assert!(message.contains("combat.lua"), "{message}");
            assert!(message.contains("uw-combat"), "{message}");
        }
    }
}
