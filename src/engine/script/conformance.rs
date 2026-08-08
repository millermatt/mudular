//! One behavioural suite, run against every language binding.
//!
//! §7.4 promises "one `mud.*` API surface, identical across languages". That
//! is a claim about behaviour, not about which functions exist, so it is
//! tested by running the same scenarios against each host and asserting the
//! same outcomes — the ported scripts differ only in syntax.
//!
//! A host that passes this suite is a host the rule engine can dispatch to
//! without knowing which language it speaks.

use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

use super::{
    Captures, Hook, PeerSnapshot, Peers, ScriptCtx, ScriptError, ScriptHost, ScriptSource,
    TIME_BUDGET,
};

/// The suite's scripts, ported to one language. Each field is the whole
/// script for one scenario; the assertions live in [`run`].
pub struct Ported {
    /// Registers a line hook that sends `eat bread` and gags on a line
    /// starting `You are hungry`, and does nothing otherwise.
    pub line_hook: &'static str,
    /// Registers a line hook counting lines into the `seen` variable.
    pub counter: &'static str,
    /// Registers a prompt hook sending `quaff heal` when the server data
    /// key `Char.Vitals.hp` is below 40.
    pub reads_server_data: &'static str,
    /// Registers a GMCP hook echoing `<package> <json>`.
    pub gmcp: &'static str,
    /// Registers two line hooks, sending `first` then `second`.
    pub two_hooks: &'static str,
    /// Registers a connect hook sending `look` and a disconnect hook
    /// echoing `bye`.
    pub lifecycle: &'static str,
    /// Registers a line hook substituting `polished` for the line `raw`.
    pub substitute: &'static str,
    /// Defines a callable `on_death(line, caps)` that echoes the line and
    /// sends `say <caps.killer> got <caps[2]>`.
    pub rule_action: &'static str,
    /// Registers a line hook that sends `sent before the error`, then
    /// raises an error.
    pub failing: &'static str,
    /// Registers a line hook that never returns.
    pub runaway: &'static str,
    /// Registers a line hook that reads the peer `tank`'s `hp` variable
    /// and `Char.Vitals.hp` data, echoes them as `<hp>/<data>`, sends the
    /// tank `stand`, and echoes `no tank` when there is no such peer.
    pub peers: &'static str,
    /// Subscribes to peer `tank`'s `Char.Affects`, echoing `<key>=<value>`
    /// for each update, and to peer `healer`'s, echoing `healer <key>`.
    pub peer_hook: &'static str,
    /// Not a valid program in this language.
    pub broken: &'static str,
}

pub fn run(new_host: impl Fn() -> Box<dyn ScriptHost>, ported: &Ported) {
    let load = |code: &str| {
        let mut host = new_host();
        host.load(&ScriptSource {
            name: "suite.lua".to_string(),
            code: code.to_string(),
        })
        .expect("suite script loads");
        host
    };
    let call = |host: &mut Box<dyn ScriptHost>, hook: Hook, ctx: &mut ScriptCtx| {
        host.call(&hook, ctx).expect("hook runs");
    };
    let line = |text: &str| Hook::Line(text.to_string());

    // A line hook acts on the line it was given, and only on that line.
    {
        let mut host = load(ported.line_hook);
        let mut ctx = ScriptCtx::default();
        call(&mut host, line("You are hungry."), &mut ctx);
        assert_eq!(ctx.out.sends, vec!["eat bread"]);
        assert!(ctx.out.gag);

        let mut ctx = ScriptCtx::default();
        call(&mut host, line("You are full."), &mut ctx);
        assert!(ctx.out.sends.is_empty());
        assert!(!ctx.out.gag);
    }

    // Variables are the engine's, not the VM's: they survive between calls
    // and come back out for the rules to read.
    {
        let mut host = load(ported.counter);
        let mut ctx = ScriptCtx::default();
        call(&mut host, line("a"), &mut ctx);
        assert_eq!(ctx.vars.get("seen").map(String::as_str), Some("1"));
        call(&mut host, line("b"), &mut ctx);
        assert_eq!(ctx.vars.get("seen").map(String::as_str), Some("2"));
    }

    // Server data reads through, and numeric comparison means the same
    // thing in both languages for the values this store actually holds.
    {
        let mut host = load(ported.reads_server_data);
        let mut ctx = ScriptCtx::default();
        ctx.server_data
            .insert("Char.Vitals.hp".to_string(), "30".to_string());
        call(&mut host, Hook::Prompt("HP:30".to_string()), &mut ctx);
        assert_eq!(ctx.out.sends, vec!["quaff heal"]);

        let mut ctx = ScriptCtx::default();
        ctx.server_data
            .insert("Char.Vitals.hp".to_string(), "90".to_string());
        call(&mut host, Hook::Prompt("HP:90".to_string()), &mut ctx);
        assert!(ctx.out.sends.is_empty());
    }

    // GMCP arrives as package plus unparsed payload.
    {
        let mut host = load(ported.gmcp);
        let mut ctx = ScriptCtx::default();
        call(
            &mut host,
            Hook::Gmcp {
                package: "Char.Vitals".to_string(),
                json: r#"{"hp":30}"#.to_string(),
            },
            &mut ctx,
        );
        assert_eq!(ctx.out.echoes, vec![r#"Char.Vitals {"hp":30}"#]);
    }

    // Hooks run in registration order, so scope layering (§7.3) means the
    // same thing for scripts as for rules.
    {
        let mut host = load(ported.two_hooks);
        let mut ctx = ScriptCtx::default();
        call(&mut host, line("x"), &mut ctx);
        assert_eq!(ctx.out.sends, vec!["first", "second"]);
    }

    {
        let mut host = load(ported.lifecycle);
        let mut ctx = ScriptCtx::default();
        call(&mut host, Hook::Connect, &mut ctx);
        call(&mut host, Hook::Disconnect, &mut ctx);
        assert_eq!(ctx.out.sends, vec!["look"]);
        assert_eq!(ctx.out.echoes, vec!["bye"]);
    }

    {
        let mut host = load(ported.substitute);
        let mut ctx = ScriptCtx::default();
        call(&mut host, line("raw"), &mut ctx);
        assert_eq!(ctx.out.substitute.as_deref(), Some("polished"));

        let mut ctx = ScriptCtx::default();
        call(&mut host, line("plain"), &mut ctx);
        assert!(ctx.out.substitute.is_none());
    }

    // A rule's `script:` action: the function is found by name, and gets
    // the line with numbered and named captures.
    {
        let mut host = load(ported.rule_action);
        assert!(host.has_function("on_death"));
        assert!(!host.has_function("on_deth"));

        let mut ctx = ScriptCtx::default();
        call(
            &mut host,
            Hook::Function {
                name: "on_death".to_string(),
                line: "Grunk killed kobold!".to_string(),
                captures: Captures {
                    numbered: vec![Some("Grunk".to_string()), Some("kobold".to_string())],
                    named: BTreeMap::from([("killer".to_string(), "Grunk".to_string())]),
                },
            },
            &mut ctx,
        );
        assert_eq!(ctx.out.echoes, vec!["Grunk killed kobold!"]);
        assert_eq!(ctx.out.sends, vec!["say Grunk got kobold"]);
    }

    // A hook that fails is reported against the hook, and what it already
    // did still stands.
    {
        let mut host = load(ported.failing);
        let mut ctx = ScriptCtx::default();
        let err = host.call(&line("x"), &mut ctx).unwrap_err();
        assert!(
            matches!(&err, ScriptError::Runtime { hook, .. } if hook == "line"),
            "{err}"
        );
        assert_eq!(ctx.out.sends, vec!["sent before the error"]);
    }

    // A runaway hook is stopped, and stopping it does not break the host.
    {
        let mut host = load(ported.runaway);
        let mut ctx = ScriptCtx::default();
        let started = Instant::now();
        let err = host.call(&line("x"), &mut ctx).unwrap_err();
        assert!(matches!(err, ScriptError::Timeout { .. }), "{err}");
        assert!(
            started.elapsed() < TIME_BUDGET * 10,
            "{:?}",
            started.elapsed()
        );

        // The host survives its own abort: the next call still runs.
        let mut host = load(ported.line_hook);
        let mut ctx = ScriptCtx::default();
        call(&mut host, line("You are hungry."), &mut ctx);
        assert_eq!(ctx.out.sends, vec!["eat bread"]);
    }

    // Peer state (§7.5): readable by name, and addressable by name.
    {
        let mut host = load(ported.peers);
        let (tx, rx) = tokio::sync::watch::channel(PeerSnapshot {
            vars: HashMap::from([("hp".to_string(), "30".to_string())]),
            data: HashMap::from([("Char.Vitals.hp".to_string(), "31".to_string())]),
        });

        let mut ctx = ScriptCtx {
            peers: Peers::from([("tank".to_string(), rx)]),
            ..ScriptCtx::default()
        };
        call(&mut host, line("x"), &mut ctx);
        assert_eq!(ctx.out.echoes, vec!["30/31"]);
        assert_eq!(
            ctx.out.send_to,
            vec![("tank".to_string(), vec!["stand".to_string()])]
        );

        // A peer that is not there is absent, not an error: sessions come
        // and go, and a script must be able to ask.
        let mut ctx = ScriptCtx::default();
        call(&mut host, line("x"), &mut ctx);
        assert_eq!(ctx.out.echoes, vec!["no tank"]);
        assert!(ctx.out.send_to.is_empty());

        drop(tx);
    }

    // `mud.on_peer` filters by session and by key prefix, so a
    // subscription hears its own peer's affects and nothing else (§7.5).
    {
        let mut host = load(ported.peer_hook);
        let mut ctx = ScriptCtx::default();
        call(
            &mut host,
            Hook::Peer {
                session: "tank".to_string(),
                key: "Char.Affects.blessed".to_string(),
                value: "0".to_string(),
            },
            &mut ctx,
        );
        call(
            &mut host,
            Hook::Peer {
                session: "tank".to_string(),
                key: "Char.Vitals.hp".to_string(),
                value: "30".to_string(),
            },
            &mut ctx,
        );
        call(
            &mut host,
            Hook::Peer {
                session: "healer".to_string(),
                key: "Char.Affects.blessed".to_string(),
                value: "0".to_string(),
            },
            &mut ctx,
        );
        // The tank's affects reached its own subscription; its vitals
        // reached neither (wrong prefix); and the healer's affects reached
        // the healer's subscription alone (wrong session).
        assert_eq!(
            ctx.out.echoes,
            vec!["Char.Affects.blessed=0", "healer Char.Affects.blessed"]
        );
    }

    // A script that will not compile names the file it came from.
    {
        let mut host = new_host();
        let err = host
            .load(&ScriptSource {
                name: "combat".to_string(),
                code: ported.broken.to_string(),
            })
            .unwrap_err();
        assert!(
            matches!(&err, ScriptError::Load { script, .. } if script == "combat"),
            "{err}"
        );
    }
}
