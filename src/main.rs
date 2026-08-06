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

    /// Write diagnostic logs to this file (filtered via RUST_LOG).
    #[arg(long)]
    log: Option<PathBuf>,
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

    let target = match (&cli.profile, &cli.host) {
        (Some(profile), _) => {
            anyhow::bail!("profile `{profile}` not wired yet — M3; use --host instead")
        }
        (None, Some(host)) => Some(app::ConnectTarget {
            host: host.clone(),
            port: cli.port,
            tls: cli.tls,
        }),
        (None, None) => None,
    };

    app::run(target).await
}
