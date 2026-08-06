//! Transport layer: plain TCP or TLS over the same interface.
//!
//! Certificate pinning (TOFU) and the `insecure` verify mode arrive in M2;
//! this always does full webpki verification (docs/ARCHITECTURE.md §5).

use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};

pub enum Transport {
    Tcp(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

pub async fn connect(host: &str, port: u16, tls: bool) -> Result<Transport> {
    let tcp = TcpStream::connect((host, port))
        .await
        .with_context(|| format!("connecting to {host}:{port}"))?;
    tcp.set_nodelay(true)?;

    if !tls {
        return Ok(Transport::Tcp(tcp));
    }

    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(config));
    let name = ServerName::try_from(host.to_owned())
        .with_context(|| format!("invalid TLS server name: {host}"))?;
    let stream = connector
        .connect(name, tcp)
        .await
        .with_context(|| format!("TLS handshake with {host}:{port}"))?;
    Ok(Transport::Tls(Box::new(stream)))
}
