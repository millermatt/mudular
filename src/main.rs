mod app;
mod complete;
mod config;
mod engine;
mod map;
mod net;
mod proto;
mod scrollback;
mod session;
mod ui;
mod vitals;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use proto::charset::Charset;

/// Mudular — a modern, keyboard-centric terminal MUD client.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Profile names to connect with. Naming several opens one session per
    /// character, side by side (docs/ARCHITECTURE.md §14 M7).
    profiles: Vec<String>,

    /// Connect directly to this host (bypasses profiles).
    #[arg(long)]
    host: Option<String>,

    /// Port for --host.
    #[arg(long, default_value_t = 23)]
    port: u16,

    /// Use TLS (STelnet) for --host.
    #[arg(long)]
    tls: bool,

    /// How to verify the server certificate: full, pinned, or insecure.
    #[arg(long, default_value = "full")]
    tls_verify: net::VerifyMode,

    /// Charset for --host, if CHARSET negotiation is absent or rejected:
    /// utf-8, latin1, or cp437.
    #[arg(long, default_value = "utf-8")]
    charset: Charset,

    /// Configuration directory (default: the platform config directory).
    #[arg(long)]
    config_dir: Option<PathBuf>,

    /// Write diagnostic logs to this file (filtered via RUST_LOG).
    #[arg(long)]
    log: Option<PathBuf>,

    /// Record raw inbound bytes to this file for replay as a test fixture.
    #[arg(long)]
    record: Option<PathBuf>,

    /// Write a snapshot of the map pane to this directory every time it
    /// changes — a room listing and an ASCII picture of the whole scene,
    /// not just what fit on screen, for diagnosing map bugs after the run.
    #[arg(long)]
    map_debug: Option<PathBuf>,

    /// Store this profile's auto-login password in the OS keyring, then
    /// exit. Prompts for the password; it is never echoed or logged.
    #[arg(long, value_name = "PROFILE")]
    set_password: Option<String>,

    /// Delete this profile's stored auto-login password from the OS
    /// keyring, then exit.
    #[arg(long, value_name = "PROFILE")]
    forget_password: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut cli = Cli::parse();

    if let Some(path) = &cli.log {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        // Diagnostic output can end up carrying scrollback fragments —
        // owner-only rather than the process umask.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path)?;
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .with_writer(std::sync::Mutex::new(file))
            .with_ansi(false)
            .init();
    }

    if let Some(profile) = &cli.set_password {
        return set_password(profile);
    }

    if let Some(profile) = &cli.forget_password {
        return forget_password(profile);
    }

    let dir = config::config_dir(cli.config_dir.clone())?;

    // Shown on the new session's first screen once it connects
    // (UX_REVIEW.md C) — a newcomer who just answered the wizard has no
    // other way to know F1 exists, and it's the single biggest "what do I
    // do now" gap for a first-time player.
    let mut first_run_hint = false;

    // Leave a commented `mudular.yaml` behind on a fresh install, so the
    // settings that are not per-character have somewhere findable to be. It
    // is inert — every line is a comment — so this changes nothing about how
    // this launch behaves; the point is that the file is *there* to be found
    // next to `profiles/`, which is where somebody looking for it looks.
    //
    // Gated on the same "no profiles yet" signal §15 uses for the wizard, and
    // placed before it so it lands even if the form is backed out of. That
    // also means deleting the file later does not resurrect it: an
    // established config is not a first run.
    //
    // Non-fatal. A config dir we cannot write to is worth knowing about, but
    // not worth refusing to start a client over — `--host` needs no config at
    // all.
    if !config::has_profiles(&dir)
        && let Err(err) = config::write_starter_app_config(&dir)
    {
        tracing::warn!("could not write a starter mudular.yaml: {err:#}");
    }

    // First run (docs/ARCHITECTURE.md §15): nothing named on the command
    // line and no profile saved yet, so there is nothing this launch could
    // possibly connect to without either a form or a text editor.
    if cli.profiles.is_empty() && cli.host.is_none() && !config::has_profiles(&dir) {
        // Esc backs out of the form without saving or connecting
        // (docs/USAGE.md) — not out of the whole client. `cli.profiles`
        // stays empty on `None`, so this falls through to the same
        // "no target" shell §15 shows once a profile already exists and
        // `mudular` is run bare, rather than exiting the process with
        // nothing said.
        if let Some(new_profile) = app::run_new_profile_wizard(&dir).await? {
            config::save_new_profile(&dir, &new_profile)?;
            cli.profiles = vec![new_profile.name];
            first_run_hint = true;
        }
    }

    let app_config = config::load_app_config(&dir)?;

    let channels = app_config.channels.clone();
    let mut targets = Vec::new();
    let mut names: Vec<String> = Vec::new();

    for name in &cli.profiles {
        targets.push(app::build_profile_target(
            &dir,
            name,
            &mut names,
            &channels,
            app_config.cross_session,
            cli.record.clone(),
        )?);
    }

    if let Some(host) = &cli.host {
        let tls = cli.tls.then(|| net::TlsConfig {
            verify: cli.tls_verify,
            pin_store: net::pins::PinStore::new(dir.join("known_certs")),
        });
        // No profile, so only the global layer applies.
        let layers = config::load_rules(&dir, None, &channels)?;
        targets.push(app::ConnectTarget {
            // An ad-hoc `--host` session has no profile to name a world,
            // so its host and port are the whole answer.
            world: None,
            name: session_name(host, &mut names),
            host: host.clone(),
            port: cli.port,
            tls,
            record: cli.record.clone(),
            charset: cli.charset,
            rules: app::Rules {
                engine: engine::Engine::compile(&layers)?,
                config_dir: dir.clone(),
                profile: None,
            },
            cross: app_config.cross_session,
            // A --host connection has no profile to carry a colour, and
            // nothing to key a stored password on.
            color: None,
            login: None,
            offer_password_save: false,
            log_path: None,
        });
    }

    app::run(
        dir,
        targets,
        app_config.keybinds,
        channels,
        app_config.history_size,
        app_config.scrollback_size,
        app_config.channel_width,
        app_config.map_width,
        app_config.map_graphics,
        app_config.autocomplete,
        app_config.cross_session,
        first_run_hint,
        cli.map_debug,
    )
    .await
}

/// Stores a profile's password in the OS keyring. Reads it with the
/// terminal's echo off, so it is not left on screen or in shell history —
/// the reason this is a prompt rather than a `--password` argument.
fn set_password(profile: &str) -> Result<()> {
    use std::io::Write as _;

    print!("Password for {profile}: ");
    std::io::stdout().flush()?;

    crossterm::terminal::enable_raw_mode().context("reading the password without echo")?;
    let entered = read_hidden_line();
    crossterm::terminal::disable_raw_mode()?;
    println!();

    let password = entered?;
    if password.is_empty() {
        anyhow::bail!("no password entered; nothing stored");
    }
    config::store_password(profile, &password)?;
    println!("Stored in the keyring for profile {profile}.");
    Ok(())
}

/// Deletes a profile's stored password. Having none is reported, not an
/// error: the caller wanted the keyring empty for this profile, and it is.
fn forget_password(profile: &str) -> Result<()> {
    if config::forget_password(profile)? {
        println!("Removed from the keyring for profile {profile}.");
    } else {
        println!("No keyring password stored for profile {profile}.");
    }
    Ok(())
}

/// Reads a line of typed characters in raw mode without echoing them.
fn read_hidden_line() -> Result<String> {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, read};

    let mut password = String::new();
    loop {
        let Event::Key(key) = read()? else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Enter => return Ok(password),
            KeyCode::Backspace => {
                password.pop();
            }
            // Ctrl+C has to work here too, and must not store a partial
            // password on the way out.
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                anyhow::bail!("cancelled");
            }
            KeyCode::Char(c) => password.push(c),
            _ => {}
        }
    }
}

/// Sessions are addressed by profile name; a second session on the same
/// profile gets a numeric suffix (`cleric-2`, docs/ARCHITECTURE.md §7.5).
fn session_name(base: &str, taken: &mut Vec<String>) -> String {
    let mut name = base.to_string();
    let mut suffix = 1;
    while taken.contains(&name) {
        suffix += 1;
        name = format!("{base}-{suffix}");
    }
    taken.push(name.clone());
    name
}
