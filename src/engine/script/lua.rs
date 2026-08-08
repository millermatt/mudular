//! Lua binding of the `mud.*` API (§7.4), on vendored Lua 5.4 via `mlua`.
//!
//! The VM is sandboxed at construction rather than patched afterwards: the
//! filesystem, OS, and package libraries are never loaded, so there is no
//! `os.execute` for a shared community module to find. Everything a script
//! is allowed to do arrives through the one `mud` table.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use mlua::{Function, HookTriggers, Lua, LuaOptions, StdLib, Table, VmState};

use super::{Hook, ScriptCtx, ScriptError, ScriptHost, ScriptSource, TIME_BUDGET};

/// Registry key for the table of registered hooks, keyed by hook name. It
/// lives in the registry rather than in `mud` so a script cannot rewrite
/// another script's hooks by assigning over the table.
const HOOKS_KEY: &str = "mudular.hooks";

/// How often the VM stops to check the time budget. Small enough that a
/// tight loop is caught in well under a millisecond of overshoot, large
/// enough that the check is noise against real script work.
const CHECK_EVERY: u32 = 10_000;

#[derive(Debug)]
pub struct LuaHost {
    lua: Lua,
    /// Swapped with the caller's [`ScriptCtx`] for the length of one call,
    /// so the `mud.*` closures — which outlive any single call — always see
    /// the current session state without copying it.
    shared: Arc<Mutex<ScriptCtx>>,
    budget: Arc<Budget>,
}

/// The deadline the VM hook enforces. `aborted` distinguishes "we stopped
/// this script" from a script's own error, which otherwise look alike by
/// the time the error surfaces.
#[derive(Debug)]
struct Budget {
    deadline: Mutex<Option<Instant>>,
    aborted: AtomicBool,
}

impl LuaHost {
    pub fn new() -> Result<Self, ScriptError> {
        // No IO, OS, PACKAGE, or DEBUG: untrusted by default (§7.4).
        let lua = Lua::new_with(
            StdLib::COROUTINE | StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::UTF8,
            LuaOptions::default(),
        )
        .map_err(|err| load_error("<init>", err))?;

        let shared = Arc::new(Mutex::new(ScriptCtx::default()));
        let budget = Arc::new(Budget {
            deadline: Mutex::new(None),
            aborted: AtomicBool::new(false),
        });

        let host = LuaHost {
            lua,
            shared: Arc::clone(&shared),
            budget: Arc::clone(&budget),
        };
        host.seal_globals()?;
        host.install_api()?;
        host.install_budget()?;
        Ok(host)
    }

    /// Removes the base-library escapes the sandbox does not cover: loading
    /// more code at runtime, and `print`, which would write straight through
    /// the TUI's own screen.
    fn seal_globals(&self) -> Result<(), ScriptError> {
        let globals = self.lua.globals();
        for name in [
            "load",
            "loadstring",
            "loadfile",
            "dofile",
            "require",
            "print",
            "collectgarbage",
        ] {
            globals
                .set(name, mlua::Value::Nil)
                .map_err(|err| load_error("<init>", err))?;
        }
        Ok(())
    }

    fn install_api(&self) -> Result<(), ScriptError> {
        self.build_api().map_err(|err| load_error("<init>", err))
    }

    fn build_api(&self) -> mlua::Result<()> {
        let lua = &self.lua;
        let mud = lua.create_table()?;

        let hooks = lua.create_table()?;
        for name in ["connect", "disconnect", "line", "prompt", "gmcp", "peer"] {
            hooks.set(name, lua.create_table()?)?;
        }
        lua.set_named_registry_value(HOOKS_KEY, hooks)?;

        let ctx = Arc::clone(&self.shared);
        mud.set(
            "send",
            lua.create_function(move |_, command: String| {
                lock(&ctx).out.sends.push(command);
                Ok(())
            })?,
        )?;

        let ctx = Arc::clone(&self.shared);
        mud.set(
            "echo",
            lua.create_function(move |_, text: String| {
                lock(&ctx).out.echoes.push(text);
                Ok(())
            })?,
        )?;

        let ctx = Arc::clone(&self.shared);
        mud.set(
            "gag",
            lua.create_function(move |_, ()| {
                lock(&ctx).out.gag = true;
                Ok(())
            })?,
        )?;

        let ctx = Arc::clone(&self.shared);
        mud.set(
            "substitute",
            lua.create_function(move |_, text: String| {
                lock(&ctx).out.substitute = Some(text);
                Ok(())
            })?,
        )?;

        let ctx = Arc::clone(&self.shared);
        mud.set(
            "get",
            lua.create_function(move |_, name: String| Ok(lock(&ctx).vars.get(&name).cloned()))?,
        )?;

        let ctx = Arc::clone(&self.shared);
        mud.set(
            "set",
            lua.create_function(move |_, (name, value): (String, String)| {
                lock(&ctx).vars.insert(name, value);
                Ok(())
            })?,
        )?;

        let ctx = Arc::clone(&self.shared);
        mud.set(
            "data",
            lua.create_function(move |_, key: String| {
                Ok(lock(&ctx).server_data.get(&key).cloned())
            })?,
        )?;

        // `mud.session("cleric")` — a handle onto one peer (§7.5). It is
        // built from the snapshot as it stands now, so a script reading
        // `.data` twice in one hook sees one consistent picture rather than
        // two moments of the peer's life.
        let ctx = Arc::clone(&self.shared);
        mud.set(
            "session",
            lua.create_function(move |lua, name: String| {
                let Some(snapshot) = lock(&ctx).peer(&name) else {
                    return Ok(mlua::Value::Nil);
                };
                let handle = lua.create_table()?;
                handle.set("name", name.clone())?;
                handle.set("vars", lua.create_table_from(snapshot.vars)?)?;
                handle.set("data", lua.create_table_from(snapshot.data)?)?;

                let sends = Arc::clone(&ctx);
                let target = name.clone();
                handle.set(
                    "send",
                    lua.create_function(move |_, (_, command): (mlua::Table, String)| {
                        lock(&sends)
                            .out
                            .send_to
                            .push((target.clone(), vec![command]));
                        Ok(())
                    })?,
                )?;
                Ok(mlua::Value::Table(handle))
            })?,
        )?;

        // `mud.on_peer(session, event, fn)` — subscribe to one peer's
        // server data (§7.5). Registrations live in the same registry as
        // the event hooks, as a list of {session, event, fn} triples.
        mud.set(
            "on_peer",
            lua.create_function(
                |lua, (session, event, callback): (String, String, Function)| {
                    let entry = lua.create_table()?;
                    entry.set("session", session)?;
                    entry.set("event", event)?;
                    entry.set("fn", callback)?;
                    let hooks: Table = lua.named_registry_value(HOOKS_KEY)?;
                    hooks.get::<Table>("peer")?.push(entry)
                },
            )?,
        )?;

        for (api, hook) in [
            ("on_connect", "connect"),
            ("on_disconnect", "disconnect"),
            ("on_line", "line"),
            ("on_prompt", "prompt"),
            ("on_gmcp", "gmcp"),
        ] {
            mud.set(
                api,
                lua.create_function(move |lua, callback: Function| {
                    let hooks: Table = lua.named_registry_value(HOOKS_KEY)?;
                    let list: Table = hooks.get(hook)?;
                    list.push(callback)
                })?,
            )?;
        }

        lua.globals().set("mud", mud)
    }

    /// Arms the instruction-count hook that enforces [`TIME_BUDGET`]. It is
    /// installed once and stays armed; between calls the deadline is `None`
    /// and the check is a load and a branch.
    fn install_budget(&self) -> Result<(), ScriptError> {
        let budget = Arc::clone(&self.budget);
        self.lua
            .set_hook(
                HookTriggers::new().every_nth_instruction(CHECK_EVERY),
                move |_, _| {
                    let expired = lock(&budget.deadline).is_some_and(|at| Instant::now() > at);
                    if expired {
                        budget.aborted.store(true, Ordering::Relaxed);
                        return Err(mlua::Error::RuntimeError("time budget exhausted".into()));
                    }
                    Ok(VmState::Continue)
                },
            )
            .map_err(|err| load_error("<init>", err))
    }

    /// Runs `body` under the time budget, translating an abort into
    /// [`ScriptError::Timeout`] so a runaway script is not reported as a
    /// script bug.
    fn with_budget<T>(
        &self,
        what: &str,
        body: impl FnOnce() -> mlua::Result<T>,
    ) -> Result<T, ScriptError> {
        self.budget.aborted.store(false, Ordering::Relaxed);
        *lock(&self.budget.deadline) = Some(Instant::now() + TIME_BUDGET);
        let result = body();
        *lock(&self.budget.deadline) = None;

        result.map_err(|err| {
            if self.budget.aborted.load(Ordering::Relaxed) {
                ScriptError::Timeout {
                    hook: what.to_string(),
                }
            } else {
                ScriptError::Runtime {
                    hook: what.to_string(),
                    reason: err.to_string(),
                }
            }
        })
    }
}

impl ScriptHost for LuaHost {
    fn load(&mut self, source: &ScriptSource) -> Result<(), ScriptError> {
        // Top-level code runs now — including whatever a hostile module put
        // outside a function — so loading is budgeted like a hook.
        self.with_budget(&source.name, || {
            self.lua.load(&source.code).set_name(&source.name).exec()
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
            return self.call_function(name, line, captures, ctx);
        }

        if let Hook::Peer {
            session,
            key,
            value,
        } = hook
        {
            return self.call_peer(session, key, value, ctx);
        }

        let name = hook.name();
        let callbacks = self.callbacks(name)?;
        if callbacks.is_empty() {
            return Ok(());
        }

        self.swap_ctx(ctx);
        let result = self.with_budget(name, || {
            for callback in &callbacks {
                match hook {
                    Hook::Connect | Hook::Disconnect => callback.call::<()>(())?,
                    Hook::Line(text) | Hook::Prompt(text) => callback.call::<()>(text.clone())?,
                    Hook::Gmcp { package, json } => {
                        callback.call::<()>((package.clone(), json.clone()))?
                    }
                    // Both handled above: a rule's `script:` action names
                    // its function, and a peer update is filtered per
                    // subscription rather than broadcast.
                    Hook::Function { .. } | Hook::Peer { .. } => unreachable!(),
                }
            }
            Ok(())
        });
        // Swapped back even on failure: a hook that errored halfway may
        // already have sent commands, and dropping them would be a worse
        // surprise than running them.
        self.swap_ctx(ctx);
        result
    }

    fn has_function(&self, name: &str) -> bool {
        self.lua.globals().get::<Function>(name).is_ok()
    }
}

impl LuaHost {
    /// Calls a global the script defined, as `fn(line, captures)`. Numbered
    /// groups land in the array part so `caps[1]` is group 1, named ones
    /// under their own names — the shape a Lua author would have written by
    /// hand.
    fn call_function(
        &mut self,
        name: &str,
        line: &str,
        captures: &super::Captures,
        ctx: &mut ScriptCtx,
    ) -> Result<(), ScriptError> {
        self.swap_ctx(ctx);
        let result = self.with_budget(name, || {
            let table = self.lua.create_table()?;
            for (index, value) in captures.numbered.iter().enumerate() {
                if let Some(value) = value {
                    table.set(index + 1, value.as_str())?;
                }
            }
            for (key, value) in &captures.named {
                table.set(key.as_str(), value.as_str())?;
            }
            self.lua
                .globals()
                .get::<Function>(name)?
                .call::<()>((line, table))
        });
        self.swap_ctx(ctx);
        result
    }

    /// Calls the `mud.on_peer` subscriptions that match: same session, and
    /// an event that is a prefix of the changed key — so `Char.Affects`
    /// catches `Char.Affects.blessed` without naming it.
    fn call_peer(
        &mut self,
        session: &str,
        key: &str,
        value: &str,
        ctx: &mut ScriptCtx,
    ) -> Result<(), ScriptError> {
        let entries: Vec<Table> = self
            .lua
            .named_registry_value::<Table>(HOOKS_KEY)
            .and_then(|hooks| hooks.get::<Table>("peer"))
            .and_then(|list| list.sequence_values::<Table>().collect())
            .map_err(|err| ScriptError::Runtime {
                hook: "peer".to_string(),
                reason: err.to_string(),
            })?;
        if entries.is_empty() {
            return Ok(());
        }

        self.swap_ctx(ctx);
        let result = self.with_budget("peer", || {
            for entry in &entries {
                if entry.get::<String>("session")? != session {
                    continue;
                }
                if !key.starts_with(&entry.get::<String>("event")?) {
                    continue;
                }
                entry.get::<Function>("fn")?.call::<()>((key, value))?;
            }
            Ok(())
        });
        self.swap_ctx(ctx);
        result
    }

    fn callbacks(&self, hook: &str) -> Result<Vec<Function>, ScriptError> {
        self.lua
            .named_registry_value::<Table>(HOOKS_KEY)
            .and_then(|hooks| hooks.get::<Table>(hook))
            .and_then(|list| list.sequence_values::<Function>().collect())
            .map_err(|err| ScriptError::Runtime {
                hook: hook.to_string(),
                reason: err.to_string(),
            })
    }

    fn swap_ctx(&self, ctx: &mut ScriptCtx) {
        std::mem::swap(&mut *lock(&self.shared), ctx);
    }
}

/// A poisoned lock here means a `mud.*` closure panicked mid-update. The
/// state it guards is a plain set of strings with no invariant to violate,
/// so recovering beats taking the session down with it.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn load_error(script: &str, err: mlua::Error) -> ScriptError {
    ScriptError::Load {
        script: script.to_string(),
        reason: err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::script::conformance;

    fn host_with(code: &str) -> LuaHost {
        let mut host = LuaHost::new().expect("VM starts");
        host.load(&ScriptSource {
            name: "test.lua".to_string(),
            code: code.to_string(),
        })
        .expect("script loads");
        host
    }

    /// The Lua port of the cross-language suite (§7.4). Everything the API
    /// promises is asserted there, once, for every engine.
    #[test]
    fn passes_the_hook_api_conformance_suite() {
        conformance::run(
            || Box::new(LuaHost::new().expect("VM starts")),
            &conformance::Ported {
                line_hook: r#"
                    mud.on_line(function(line)
                      if line:match("^You are hungry") then
                        mud.send("eat bread")
                        mud.gag()
                      end
                    end)
                "#,
                counter: r#"
                    mud.on_line(function()
                      mud.set("seen", tostring(tonumber(mud.get("seen") or "0") + 1))
                    end)
                "#,
                reads_server_data: r#"
                    mud.on_prompt(function()
                      if tonumber(mud.data("Char.Vitals.hp")) < 40 then
                        mud.send("quaff heal")
                      end
                    end)
                "#,
                gmcp: r#"
                    mud.on_gmcp(function(package, json)
                      mud.echo(package .. " " .. json)
                    end)
                "#,
                two_hooks: r#"
                    mud.on_line(function() mud.send("first") end)
                    mud.on_line(function() mud.send("second") end)
                "#,
                lifecycle: r#"
                    mud.on_connect(function() mud.send("look") end)
                    mud.on_disconnect(function() mud.echo("bye") end)
                "#,
                substitute: r#"
                    mud.on_line(function(line)
                      if line == "raw" then mud.substitute("polished") end
                    end)
                "#,
                rule_action: r#"
                    function on_death(line, caps)
                      mud.echo(line)
                      mud.send("say " .. caps.killer .. " got " .. caps[2])
                    end
                "#,
                failing: r#"
                    mud.on_line(function()
                      mud.send("sent before the error")
                      error("boom")
                    end)
                "#,
                peers: r#"
                    mud.on_line(function()
                      local tank = mud.session("tank")
                      if tank == nil then
                        mud.echo("no tank")
                      else
                        mud.echo(tank.vars.hp .. "/" .. tank.data["Char.Vitals.hp"])
                        tank:send("stand")
                      end
                    end)
                "#,
                runaway: "mud.on_line(function() while true do end end)",
                peer_hook: r#"
                    mud.on_peer("tank", "Char.Affects", function(key, value)
                      mud.echo(key .. "=" .. value)
                    end)
                    mud.on_peer("healer", "Char.Affects", function(key)
                      mud.echo("healer " .. key)
                    end)
                "#,
                broken: "this is not lua",
            },
        );
    }

    /// Lua-specific: the sandbox is what the VM was built without, not what
    /// a script politely avoids.
    #[test]
    fn the_sandbox_has_no_filesystem_or_process_access() {
        let host = host_with("");
        for expression in ["io", "os", "package", "require", "load", "dofile", "debug"] {
            let value: mlua::Value = host
                .lua
                .load(format!("return {expression}"))
                .eval()
                .expect("expression evaluates");
            assert!(value.is_nil(), "`{expression}` is reachable from a script");
        }
    }

    /// Registered hooks live in the registry, so a script cannot unregister
    /// another script's by assigning over the `mud` table.
    #[test]
    fn hooks_are_not_reachable_from_a_script() {
        let mut host = host_with(
            r#"
            mud.on_line(function() mud.send("registered") end)
            mud.on_line = nil
            "#,
        );

        let mut ctx = ScriptCtx::default();
        host.call(&Hook::Line("x".to_string()), &mut ctx)
            .expect("hook runs");
        assert_eq!(ctx.out.sends, vec!["registered"]);
    }
}
