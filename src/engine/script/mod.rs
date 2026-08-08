//! Script hosting: one API surface, any number of languages (§7.4).
//!
//! YAML rules cover the common cases; scripts cover everything else. The
//! rule engine never names a language — it holds a [`ScriptHost`], and each
//! language binds the same `mud.*` API to its own VM. Adding an engine means
//! adding an implementation of this trait, not touching anything here.
//!
//! Scripts are sans-IO like the rest of `engine`: a hook cannot reach a
//! socket or the screen, it records what it wants done in [`ScriptOutcome`]
//! and the session performs it. That is what keeps a script from bypassing
//! the pipeline the rules run through, and what makes hooks testable
//! without a connection.

// A build with no engine compiled in still parses `scripts:` and still
// carries this API, so that declaring a script fails with "this build has
// no Lua engine" instead of an unknown-field error. That leaves the types
// here unused in that configuration, which is the point of them.
#![cfg_attr(not(feature = "lua"), allow(dead_code))]

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use thiserror::Error;

#[cfg(feature = "lua")]
pub mod lua;

/// How long one hook invocation may run before it is aborted. Hooks run
/// synchronously on the owning session's task (§7.4), so the budget is the
/// worst case that session's pipeline can stall — generous for real work,
/// short enough that `while true do end` is a logged error rather than a
/// hung character.
pub const TIME_BUDGET: Duration = Duration::from_millis(100);

/// One script file, named for error messages.
#[derive(Debug, Clone)]
pub struct ScriptSource {
    pub name: String,
    pub code: String,
}

/// An event a script can hook. The payload travels with the hook so a host
/// has everything it needs to make the call in one place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hook {
    Connect,
    Disconnect,
    /// A completed inbound line, ANSI already stripped (§7.1).
    Line(String),
    Prompt(String),
    /// One GMCP message: package name and its JSON payload, unparsed —
    /// every scripting language has a better JSON decoder than we would
    /// pick for it.
    Gmcp {
        package: String,
        json: String,
    },
    /// A function a YAML rule named in its `script:` action, called with
    /// the text that matched and the rule's captures.
    Function {
        name: String,
        line: String,
        captures: Captures,
    },
}

/// A rule's regex captures, in a shape every language can bind naturally:
/// `numbered[0]` is group 1, and named groups keep their names. Groups that
/// did not participate in the match are absent rather than empty.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Captures {
    pub numbered: Vec<Option<String>>,
    pub named: BTreeMap<String, String>,
}

impl Hook {
    /// The `mud.on_*` name scripts register against.
    fn name(&self) -> &'static str {
        match self {
            Hook::Connect => "connect",
            Hook::Disconnect => "disconnect",
            Hook::Line(_) => "line",
            Hook::Prompt(_) => "prompt",
            Hook::Gmcp { .. } => "gmcp",
            Hook::Function { .. } => "script action",
        }
    }
}

/// What a hook asked the session to do.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ScriptOutcome {
    /// Commands for the server, in the order the script sent them.
    pub sends: Vec<String>,
    /// Text for this session's scrollback only — never the server.
    pub echoes: Vec<String>,
    /// Keep the line that triggered this hook out of the scrollback.
    pub gag: bool,
    /// Replace the triggering line's text. Last writer wins: a later hook
    /// substitutes over an earlier one rather than both appearing.
    pub substitute: Option<String>,
}

/// The state a hook reads and writes, lent to the host for one call.
///
/// The stores are owned rather than borrowed because a host hands them to a
/// VM whose closures outlive any single call; [`ScriptHost::call`] moves
/// them in and back out, so lending them costs no clone.
#[derive(Debug, Default)]
pub struct ScriptCtx {
    /// Rule variables. Scripts may write these; the engine keeps the result.
    pub vars: HashMap<String, String>,
    /// GMCP/MSDP values by dotted path. Read-only to scripts: the server
    /// owns them, and a script that wants its own name should use a
    /// variable.
    pub server_data: HashMap<String, String>,
    pub out: ScriptOutcome,
}

#[derive(Debug, Error)]
pub enum ScriptError {
    #[error("script `{script}` failed to load: {reason}")]
    Load { script: String, reason: String },
    #[error("`{hook}` hook failed: {reason}")]
    Runtime { hook: String, reason: String },
    #[error("`{hook}` hook ran longer than {}ms and was aborted", TIME_BUDGET.as_millis())]
    Timeout { hook: String },
}

/// A language binding. One instance per session — script state is per
/// character, exactly like the rule engine's variables.
pub trait ScriptHost: std::fmt::Debug + Send {
    /// Compile and run one script file, registering whatever hooks it
    /// declares. Errors carry the script name: a broken community module
    /// must say which file it was.
    fn load(&mut self, source: &ScriptSource) -> Result<(), ScriptError>;

    /// Run every hook registered for `hook`, in registration order.
    /// A host that has none returns `Ok` without entering its VM.
    fn call(&mut self, hook: &Hook, ctx: &mut ScriptCtx) -> Result<(), ScriptError>;

    /// Whether a loaded script defined this function. Checked when the
    /// rules compile, so a typo in a rule's `fn:` fails at load like a bad
    /// pattern does — not silently never firing.
    fn has_function(&self, name: &str) -> bool;
}
