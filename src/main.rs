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
    /// Profile name to connect with.
    profile: Option<String>,

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

    let dir = config::config_dir(cli.config_dir.clone())?;
    let app_config = config::load_app_config(&dir)?;

    let target = match (&cli.profile, &cli.host) {
        (Some(name), _) => {
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
            let layers = config::load_rules(&dir, Some(name))?;
            Some(app::ConnectTarget {
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
            })
        }
        (None, Some(host)) => {
            let tls = cli.tls.then(|| net::TlsConfig {
                verify: cli.tls_verify,
                pin_store: net::pins::PinStore::new(dir.join("known_certs")),
            });
            // No profile, so only the global layer applies.
            let layers = config::load_rules(&dir, None)?;
            Some(app::ConnectTarget {
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
            })
        }
        (None, None) => None,
    };

    app::run(target, app_config.keybinds).await
}
