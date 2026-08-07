mod app;
mod config;
mod engine;
mod net;
mod proto;
mod session;
mod ui;

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

    /// Store this profile's auto-login password in the OS keyring, then
    /// exit. Prompts for the password; it is never echoed or logged.
    #[arg(long, value_name = "PROFILE")]
    set_password: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    if let Some(path) = &cli.log {
        let file = std::fs::File::create(path)?;
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

    let dir = config::config_dir(cli.config_dir.clone())?;
    let app_config = config::load_app_config(&dir)?;

    let channels = app_config.channels.clone();
    let mut targets = Vec::new();
    let mut names: Vec<String> = Vec::new();

    for name in &cli.profiles {
        let path = config::profile_path(&dir, name);
        let profile = config::load_profile(&path)?;
        let tls = profile.tls.enabled.then(|| net::TlsConfig {
            verify: profile.tls.verify,
            pin_store: net::pins::PinStore::new(dir.join("known_certs")),
        });
        let charset: Charset = profile
            .charset
            .parse()
            .map_err(|err: String| anyhow::anyhow!(err))
            .with_context(|| format!("charset in {}", path.display()))?;
        let layers = config::load_rules(&dir, Some(name), &channels)?;
        targets.push(app::ConnectTarget {
            name: session_name(name, &mut names),
            host: profile.host,
            port: profile.port,
            tls,
            record: cli.record.clone(),
            charset,
            rules: app::Rules {
                engine: engine::Engine::compile(&layers)?,
                config_dir: dir.clone(),
                profile: Some(name.clone()),
            },
            cross: app_config
                .cross_session
                .with_override(profile.cross_session),
            color: profile.color,
            login: autologin(profile.login.as_ref(), name)?,
        });
    }

    if let Some(host) = &cli.host {
        let tls = cli.tls.then(|| net::TlsConfig {
            verify: cli.tls_verify,
            pin_store: net::pins::PinStore::new(dir.join("known_certs")),
        });
        // No profile, so only the global layer applies.
        let layers = config::load_rules(&dir, None, &channels)?;
        targets.push(app::ConnectTarget {
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
        });
    }

    app::run(
        targets,
        app_config.keybinds,
        channels,
        app_config.history_size,
    )
    .await
}

/// Builds the auto-login machine for a profile, reading its password from
/// the OS keyring (docs/ARCHITECTURE.md §10). A profile with no `login:`
/// block gets `None` and the ordinary hand-typed login.
fn autologin(
    login: Option<&config::Login>,
    profile: &str,
) -> Result<Option<session::login::Autologin>> {
    let Some(login) = login else {
        return Ok(None);
    };
    // Read once, at startup: the keyring may prompt, and doing that from
    // inside the session task would block the pipeline mid-connection.
    let password = config::stored_password(profile)?;
    session::login::Autologin::new(
        login.name.clone(),
        password,
        login.name_prompt.as_deref(),
        login.password_prompt.as_deref(),
    )
    .map(Some)
    .with_context(|| format!("auto-login for {profile}"))
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
