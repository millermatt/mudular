//! JavaScript binding of the `mud.*` API (§7.4), on QuickJS via `rquickjs`.
//!
//! This engine exists to keep the abstraction honest: everything the rule
//! engine asks of a host is asked here in a language with different types,
//! different error semantics, and a different idea of what a "hook" is. Both
//! hosts pass the same conformance suite, so a rule dispatching to a script
//! never has to know which one it got.
//!
//! Sandboxing works as it does for Lua, with one wrinkle: QuickJS itself
//! has no filesystem or process bindings — only what a host adds — so the
//! chosen intrinsics are the whole surface a script sees, plus the `mud`
//! object. The exception is `Eval`, which has to be present for the host to
//! compile a script file at all; the globals it brings (`eval` and the
//! `Function` constructor, both of which compile code from a string) are
//! dropped once the bootstrap has run.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use rquickjs::context::intrinsic;
use rquickjs::function::Func;
use rquickjs::{Array, CatchResultExt, Context, Ctx, Function, Object, Runtime};

use super::{Captures, Hook, ScriptCtx, ScriptError, ScriptHost, ScriptSource, TIME_BUDGET};

/// The dispatcher the bootstrap installs. It closes over the hook lists, so
/// they are unreachable from any script — the JavaScript equivalent of
/// Lua's registry — and the property itself is non-writable, so a script
/// cannot replace the dispatcher either.
const DISPATCH: &str = "__mudular_dispatch";

/// Installed once, over the `mud` object the Rust bindings built. Keeping
/// registration in JavaScript is what keeps the hook lists private: a
/// closure is the only container QuickJS has that a script cannot reach.
const BOOTSTRAP: &str = r#"
(function () {
  const peerVars = mud.__peer_vars;
  const peerData = mud.__peer_data;
  const sendTo = mud.__send_to;
  delete mud.__peer_vars;
  delete mud.__peer_data;
  delete mud.__send_to;

  // One handle onto one peer (§7.5), built from the snapshot as it stands
  // now, so a script reading `.data` twice in one hook sees one consistent
  // picture rather than two moments of the peer's life.
  mud.session = function (name) {
    const vars = peerVars(name);
    if (vars === null || vars === undefined) { return null; }
    return {
      name: name,
      vars: vars,
      data: peerData(name),
      send: function (command) { sendTo(name, command); },
    };
  };

  const hooks = { connect: [], disconnect: [], line: [], prompt: [], gmcp: [] };
  const register = function (name) {
    return function (fn) { hooks[name].push(fn); };
  };

  // `mud.on_peer(session, event, fn)` — one peer's server data (§7.5).
  // Filtered per subscription rather than broadcast, so `Char.Affects`
  // catches `Char.Affects.blessed` without naming it.
  const peerHooks = [];
  mud.on_peer = function (session, event, fn) {
    peerHooks.push({ session: session, event: event, fn: fn });
  };
  mud.on_connect = register("connect");
  mud.on_disconnect = register("disconnect");
  mud.on_line = register("line");
  mud.on_prompt = register("prompt");
  mud.on_gmcp = register("gmcp");
  Object.defineProperty(globalThis, "__mudular_dispatch", {
    value: function (name, args) {
      if (name === "peer") {
        const session = args[0], key = args[1], value = args[2];
        for (let i = 0; i < peerHooks.length; i++) {
          const hook = peerHooks[i];
          if (hook.session === session && key.indexOf(hook.event) === 0) {
            hook.fn(key, value);
          }
        }
        return;
      }
      const list = hooks[name];
      for (let i = 0; i < list.length; i++) { list[i].apply(null, args); }
    },
  });
})();
// Loading a script needs the eval machinery; a script itself does not, and
// keeping it would leave a way to compile code at runtime that Lua's
// sandbox does not have.
globalThis.eval = undefined;
globalThis.Function = undefined;
"#;

pub struct JsHost {
    /// Kept alive for the context, and owner of the interrupt handler that
    /// enforces the time budget.
    runtime: Runtime,
    context: Context,
    /// Swapped with the caller's [`ScriptCtx`] for the length of one call,
    /// so the `mud.*` closures — which outlive any single call — always see
    /// the current session state without copying it.
    shared: Arc<Mutex<ScriptCtx>>,
    budget: Arc<Budget>,
}

/// QuickJS gives no way to attach state to the interrupt callback, so the
/// deadline lives here. `aborted` separates "we stopped this script" from a
/// script's own exception.
#[derive(Debug)]
struct Budget {
    deadline: Mutex<Option<Instant>>,
    aborted: AtomicBool,
}

impl std::fmt::Debug for JsHost {
    /// The VM has no useful debug representation, and `Engine` derives
    /// `Debug`; naming the engine is all a reader wants here.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("JsHost")
    }
}

impl JsHost {
    pub fn new() -> Result<Self, ScriptError> {
        let runtime = Runtime::new().map_err(|err| load_error("<init>", &err.to_string()))?;
        // No `Promise`: hooks are synchronous (§7.4), so a resolved promise
        // would have nowhere to run.
        let context = Context::builder()
            .with::<(
                intrinsic::Eval,
                intrinsic::Date,
                intrinsic::RegExp,
                intrinsic::RegExpCompiler,
                intrinsic::Json,
                intrinsic::MapSet,
                intrinsic::TypedArrays,
            )>()
            .build(&runtime)
            .map_err(|err| load_error("<init>", &err.to_string()))?;

        let host = JsHost {
            runtime,
            context,
            shared: Arc::new(Mutex::new(ScriptCtx::default())),
            budget: Arc::new(Budget {
                deadline: Mutex::new(None),
                aborted: AtomicBool::new(false),
            }),
        };
        host.install_api()?;
        host.install_budget();
        Ok(host)
    }

    fn install_api(&self) -> Result<(), ScriptError> {
        self.context.with(|ctx| {
            self.build_api(&ctx)
                .catch(&ctx)
                .map_err(|err| load_error("<init>", &err.to_string()))
        })
    }

    fn build_api(&self, ctx: &Ctx<'_>) -> rquickjs::Result<()> {
        let mud = Object::new(ctx.clone())?;

        let shared = Arc::clone(&self.shared);
        mud.set(
            "send",
            Func::from(move |command: String| lock(&shared).out.sends.push(command)),
        )?;

        let shared = Arc::clone(&self.shared);
        mud.set(
            "echo",
            Func::from(move |text: String| lock(&shared).out.echoes.push(text)),
        )?;

        let shared = Arc::clone(&self.shared);
        mud.set("gag", Func::from(move || lock(&shared).out.gag = true))?;

        let shared = Arc::clone(&self.shared);
        mud.set(
            "substitute",
            Func::from(move |text: String| lock(&shared).out.substitute = Some(text)),
        )?;

        let shared = Arc::clone(&self.shared);
        mud.set(
            "get",
            Func::from(move |name: String| lock(&shared).vars.get(&name).cloned()),
        )?;

        let shared = Arc::clone(&self.shared);
        mud.set(
            "set",
            Func::from(move |name: String, value: String| {
                lock(&shared).vars.insert(name, value);
            }),
        )?;

        let shared = Arc::clone(&self.shared);
        mud.set(
            "data",
            Func::from(move |key: String| lock(&shared).server_data.get(&key).cloned()),
        )?;

        // The peer bridge (§7.5). These return plain maps rather than
        // built objects: a Rust closure cannot name the VM's lifetime, so
        // the handle itself is assembled in the bootstrap, which can.
        let shared = Arc::clone(&self.shared);
        mud.set(
            "__peer_vars",
            Func::from(move |name: String| lock(&shared).peer(&name).map(|peer| peer.vars)),
        )?;

        let shared = Arc::clone(&self.shared);
        mud.set(
            "__peer_data",
            Func::from(move |name: String| lock(&shared).peer(&name).map(|peer| peer.data)),
        )?;

        let shared = Arc::clone(&self.shared);
        mud.set(
            "__send_to",
            Func::from(move |target: String, command: String| {
                lock(&shared).out.send_to.push((target, vec![command]));
            }),
        )?;

        ctx.globals().set("mud", mud)?;
        ctx.eval::<(), _>(BOOTSTRAP.as_bytes())
    }

    /// Arms the interrupt handler enforcing [`TIME_BUDGET`]. QuickJS calls
    /// it periodically while executing; between calls the deadline is
    /// `None` and it answers "keep going" immediately.
    fn install_budget(&self) {
        let budget = Arc::clone(&self.budget);
        self.runtime.set_interrupt_handler(Some(Box::new(move || {
            let expired = lock(&budget.deadline).is_some_and(|at| Instant::now() > at);
            if expired {
                budget.aborted.store(true, Ordering::Relaxed);
            }
            expired
        })));
    }

    /// Runs `body` under the time budget, translating an interrupt into
    /// [`ScriptError::Timeout`] rather than reporting it as a script bug.
    fn with_budget<T>(
        &self,
        what: &str,
        body: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, ScriptError> {
        self.budget.aborted.store(false, Ordering::Relaxed);
        *lock(&self.budget.deadline) = Some(Instant::now() + TIME_BUDGET);
        let result = body();
        *lock(&self.budget.deadline) = None;

        result.map_err(|reason| {
            if self.budget.aborted.load(Ordering::Relaxed) {
                ScriptError::Timeout {
                    hook: what.to_string(),
                }
            } else {
                ScriptError::Runtime {
                    hook: what.to_string(),
                    reason,
                }
            }
        })
    }

    fn swap_ctx(&self, ctx: &mut ScriptCtx) {
        std::mem::swap(&mut *lock(&self.shared), ctx);
    }

    /// Lends the session state to the VM, runs `body` under the budget, and
    /// takes the state back — even on failure, since a hook that errored
    /// halfway may already have sent commands, and dropping them would be a
    /// worse surprise than running them.
    fn run(
        &self,
        what: &str,
        ctx: &mut ScriptCtx,
        body: impl FnOnce(&Ctx<'_>) -> rquickjs::Result<()>,
    ) -> Result<(), ScriptError> {
        self.swap_ctx(ctx);
        let result = self.with_budget(what, || {
            self.context
                .with(|js| body(&js).catch(&js).map_err(|err| err.to_string()))
        });
        self.swap_ctx(ctx);
        result
    }
}

impl ScriptHost for JsHost {
    fn load(&mut self, source: &ScriptSource) -> Result<(), ScriptError> {
        // Top-level code runs now, so loading is budgeted like a hook.
        self.with_budget(&source.name, || {
            self.context.with(|ctx| {
                ctx.eval::<(), _>(source.code.as_bytes())
                    .catch(&ctx)
                    .map_err(|err| err.to_string())
            })
        })
        .map_err(|err| match err {
            ScriptError::Runtime { reason, .. } => ScriptError::Load {
                script: source.name.clone(),
                reason,
            },
            other => other,
        })
    }

    fn call(&mut self, hook: &Hook, ctx: &mut ScriptCtx) -> Result<(), ScriptError> {
        if let Hook::Function {
            name,
            line,
            captures,
        } = hook
        {
            return self.run(name, ctx, |js| call_named(js, name, line, captures));
        }

        let name = hook.name();
        self.run(name, ctx, |js| call_hooks(js, hook))
    }

    fn has_function(&self, name: &str) -> bool {
        self.context
            .with(|ctx| ctx.globals().get::<_, Function>(name).is_ok())
    }
}

/// Hands the hook's arguments to the private dispatcher, which calls every
/// registered callback in registration order. The arguments go as an array
/// and are `apply`d, so a hook that takes none is called with none.
fn call_hooks(ctx: &Ctx<'_>, hook: &Hook) -> rquickjs::Result<()> {
    let args = Array::new(ctx.clone())?;
    match hook {
        Hook::Connect | Hook::Disconnect => {}
        Hook::Line(text) | Hook::Prompt(text) => args.set(0, text.as_str())?,
        Hook::Gmcp { package, json } => {
            args.set(0, package.as_str())?;
            args.set(1, json.as_str())?;
        }
        Hook::Peer {
            session,
            key,
            value,
        } => {
            args.set(0, session.as_str())?;
            args.set(1, key.as_str())?;
            args.set(2, value.as_str())?;
        }
        // Handled by the caller: a rule's `script:` action names its
        // function rather than going through the dispatcher.
        Hook::Function { .. } => unreachable!(),
    }
    ctx.globals()
        .get::<_, Function>(DISPATCH)?
        .call::<_, ()>((hook.name(), args))
}

/// Calls a global the script defined, as `fn(line, captures)`. Numbered
/// groups are array indices and named ones are properties of the same
/// object, so `caps[2]` and `caps.killer` both read naturally — the shape a
/// JavaScript author would have written by hand.
fn call_named(ctx: &Ctx<'_>, name: &str, line: &str, captures: &Captures) -> rquickjs::Result<()> {
    let table = Object::new(ctx.clone())?;
    for (index, value) in captures.numbered.iter().enumerate() {
        if let Some(value) = value {
            table.set(index as u32 + 1, value.as_str())?;
        }
    }
    for (key, value) in &captures.named {
        table.set(key.as_str(), value.as_str())?;
    }
    ctx.globals()
        .get::<_, Function>(name)?
        .call::<_, ()>((line, table))
}

/// A poisoned lock here means a `mud.*` closure panicked mid-update. The
/// state it guards is a plain set of strings with no invariant to violate,
/// so recovering beats taking the session down with it.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn load_error(script: &str, reason: &str) -> ScriptError {
    ScriptError::Load {
        script: script.to_string(),
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::script::conformance;

    /// The JavaScript port of the cross-language suite (§7.4) — the same
    /// scenarios, the same assertions, a different language.
    #[test]
    fn passes_the_hook_api_conformance_suite() {
        conformance::run(
            || Box::new(JsHost::new().expect("VM starts")),
            &conformance::Ported {
                line_hook: r#"
                    mud.on_line(function (line) {
                      if (line.startsWith("You are hungry")) {
                        mud.send("eat bread");
                        mud.gag();
                      }
                    });
                "#,
                counter: r#"
                    mud.on_line(function () {
                      mud.set("seen", String(Number(mud.get("seen") || "0") + 1));
                    });
                "#,
                reads_server_data: r#"
                    mud.on_prompt(function () {
                      if (Number(mud.data("Char.Vitals.hp")) < 40) {
                        mud.send("quaff heal");
                      }
                    });
                "#,
                gmcp: r#"
                    mud.on_gmcp(function (pkg, json) { mud.echo(pkg + " " + json); });
                "#,
                two_hooks: r#"
                    mud.on_line(function () { mud.send("first"); });
                    mud.on_line(function () { mud.send("second"); });
                "#,
                lifecycle: r#"
                    mud.on_connect(function () { mud.send("look"); });
                    mud.on_disconnect(function () { mud.echo("bye"); });
                "#,
                substitute: r#"
                    mud.on_line(function (line) {
                      if (line === "raw") { mud.substitute("polished"); }
                    });
                "#,
                rule_action: r#"
                    function on_death(line, caps) {
                      mud.echo(line);
                      mud.send("say " + caps.killer + " got " + caps[2]);
                    }
                "#,
                failing: r#"
                    mud.on_line(function () {
                      mud.send("sent before the error");
                      throw new Error("boom");
                    });
                "#,
                peers: r#"
                    mud.on_line(function () {
                      const tank = mud.session("tank");
                      if (tank === null) {
                        mud.echo("no tank");
                      } else {
                        mud.echo(tank.vars.hp + "/" + tank.data["Char.Vitals.hp"]);
                        tank.send("stand");
                      }
                    });
                "#,
                runaway: "mud.on_line(function () { for (;;) {} });",
                peer_hook: r#"
                    mud.on_peer("tank", "Char.Affects", function (key, value) {
                      mud.echo(key + "=" + value);
                    });
                    mud.on_peer("healer", "Char.Affects", function (key) {
                      mud.echo("healer " + key);
                    });
                "#,
                broken: "function (",
            },
        );
    }

    /// JavaScript-specific: no `eval`, and none of the host bindings a
    /// browser or Node would have added.
    #[test]
    fn the_sandbox_has_no_filesystem_or_process_access() {
        let host = JsHost::new().unwrap();
        for expression in [
            "typeof eval",
            "typeof require",
            "typeof process",
            "typeof fetch",
            "typeof globalThis.std",
            "typeof globalThis.os",
        ] {
            let kind: String = host
                .context
                .with(|ctx| ctx.eval(expression.as_bytes()))
                .expect("expression evaluates");
            assert_eq!(kind, "undefined", "`{expression}` is reachable");
        }
    }
}
