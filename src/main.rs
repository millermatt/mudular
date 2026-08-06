#![allow(dead_code)]

mod app;
mod config;
mod engine;
mod net;
mod proto;
mod session;
mod ui;

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

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

    let tls = match cli.tls {
        true => Some(net::TlsConfig {
            verify: cli.tls_verify,
            pin_store: net::pins::PinStore::new(config_dir(cli.config_dir)?.join("known_certs")),
        }),
        false => None,
    };

    let target = match (&cli.profile, &cli.host) {
        (Some(profile), _) => {
            anyhow::bail!("profile `{profile}` not wired yet — M3; use --host instead")
        }
        (None, Some(host)) => Some(app::ConnectTarget {
            host: host.clone(),
            port: cli.port,
            tls,
            record: cli.record.clone(),
        }),
        (None, None) => None,
    };

    app::run(target).await
}

/// Where the pin store (and, from M3, the YAML config) lives.
fn config_dir(override_dir: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(dir) = override_dir {
        return Ok(dir);
    }
    let dirs = directories::ProjectDirs::from("", "", "mudular")
        .ok_or_else(|| anyhow::anyhow!("cannot determine a config directory; use --config-dir"))?;
    Ok(dirs.config_dir().to_path_buf())
}
