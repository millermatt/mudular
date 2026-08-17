//! Transport layer: plain TCP or TLS over the same interface.
//!
//! TLS uses `tokio-rustls` with webpki roots by default. MUDs frequently
//! run self-signed certificates, so `pinned` records the server's SHA-256
//! on first connect (TOFU) and `insecure` skips verification entirely —
//! allowed, but surfaced loudly in the UI (docs/ARCHITECTURE.md §5, §13).

pub mod pins;

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::crypto::{
    CryptoProvider, verify_tls12_signature, verify_tls13_signature,
};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};

use pins::PinStore;

/// How much we trust the server's certificate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VerifyMode {
    /// Full webpki validation against the system/webpki roots.
    #[default]
    Full,
    /// Trust on first use: pin the certificate's SHA-256 and require it to
    /// stay the same afterwards.
    Pinned,
    /// No verification at all. Open to interception.
    Insecure,
}

impl std::fmt::Display for VerifyMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            VerifyMode::Full => "full",
            VerifyMode::Pinned => "pinned",
            VerifyMode::Insecure => "insecure",
        };
        f.write_str(name)
    }
}

impl std::str::FromStr for VerifyMode {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "full" => Ok(VerifyMode::Full),
            "pinned" => Ok(VerifyMode::Pinned),
            "insecure" => Ok(VerifyMode::Insecure),
            other => Err(format!(
                "unknown TLS verify mode `{other}` (expected full, pinned, or insecure)"
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TlsConfig {
    pub verify: VerifyMode,
    /// Where `pinned` keeps its fingerprints.
    pub pin_store: PinStore,
}

/// What the transport ended up trusting, for the status bar and for
/// warnings the player needs to see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Security {
    /// Short status-bar label.
    pub label: String,
    /// Something the player must notice, rendered in the pane.
    pub warning: Option<String>,
}

impl Security {
    fn plain() -> Self {
        Self {
            label: "plain".to_string(),
            warning: None,
        }
    }
}

#[derive(Debug)]
pub struct Connection {
    pub transport: Transport,
    pub security: Security,
}

#[derive(Debug)]
pub enum Transport {
    Tcp(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl AsyncRead for Transport {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Transport::Tcp(s) => Pin::new(s).poll_read(cx, buf),
            Transport::Tls(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Transport {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Transport::Tcp(s) => Pin::new(s).poll_write(cx, buf),
            Transport::Tls(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Transport::Tcp(s) => Pin::new(s).poll_flush(cx),
            Transport::Tls(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Transport::Tcp(s) => Pin::new(s).poll_shutdown(cx),
            Transport::Tls(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

pub async fn connect(host: &str, port: u16, tls: Option<&TlsConfig>) -> Result<Connection> {
    let tcp = TcpStream::connect((host, port))
        .await
        .with_context(|| format!("connecting to {host}:{port}"))?;
    tcp.set_nodelay(true)?;

    let Some(tls) = tls else {
        return Ok(Connection {
            transport: Transport::Tcp(tcp),
            security: Security::plain(),
        });
    };

    let provider = Arc::new(tokio_rustls::rustls::crypto::aws_lc_rs::default_provider());
    let name = ServerName::try_from(host.to_owned())
        .with_context(|| format!("invalid TLS server name: {host}"))?;

    // `pinned` needs the fingerprint the handshake observed, so the
    // verifier records it and we persist it only once the handshake has
    // actually succeeded.
    let observed: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let known = match tls.verify {
        VerifyMode::Pinned => tls.pin_store.get(&pins::key(host, port))?,
        _ => None,
    };

    let config = match tls.verify {
        VerifyMode::Full => {
            let mut roots = RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()?
                .with_root_certificates(roots)
                .with_no_client_auth()
        }
        VerifyMode::Pinned => {
            let verifier = TofuVerifier {
                expected: known.clone(),
                observed: Arc::clone(&observed),
                provider: Arc::clone(&provider),
            };
            ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()?
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(verifier))
                .with_no_client_auth()
        }
        VerifyMode::Insecure => {
            let verifier = NoVerification {
                provider: Arc::clone(&provider),
            };
            ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()?
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(verifier))
                .with_no_client_auth()
        }
    };

    let stream = TlsConnector::from(Arc::new(config))
        .connect(name, tcp)
        .await
        .with_context(|| format!("TLS handshake with {host}:{port}"))?;

    let security = match tls.verify {
        VerifyMode::Full => Security {
            label: "TLS".to_string(),
            warning: None,
        },
        VerifyMode::Insecure => Security {
            label: "TLS insecure".to_string(),
            warning: Some(
                "certificate NOT verified (--tls-verify insecure): this connection can be intercepted"
                    .to_string(),
            ),
        },
        VerifyMode::Pinned => {
            let fingerprint = observed
                .lock()
                .expect("verifier mutex")
                .clone()
                .ok_or_else(|| anyhow!("TLS handshake succeeded without inspecting a certificate"))?;
            match known {
                Some(_) => Security {
                    label: "TLS pinned".to_string(),
                    warning: None,
                },
                None => {
                    tls.pin_store.insert(&pins::key(host, port), &fingerprint)?;
                    Security {
                        label: "TLS pinned".to_string(),
                        warning: Some(format!(
                            "pinned new certificate for {host}:{port} (SHA-256 {fingerprint}); \
                             a future change will be refused"
                        )),
                    }
                }
            }
        }
    };

    Ok(Connection {
        transport: Transport::Tls(Box::new(stream)),
        security,
    })
}

fn fingerprint(cert: &CertificateDer<'_>) -> String {
    let digest = Sha256::digest(cert.as_ref());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Trust on first use: accept any certificate the first time, then require
/// that exact certificate from then on.
#[derive(Debug)]
struct TofuVerifier {
    expected: Option<String>,
    observed: Arc<Mutex<Option<String>>>,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for TofuVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, tokio_rustls::rustls::Error> {
        let seen = fingerprint(end_entity);
        if let Some(expected) = &self.expected
            && &seen != expected
        {
            return Err(tokio_rustls::rustls::Error::General(format!(
                "pinned certificate mismatch: expected SHA-256 {expected}, server offered {seen}"
            )));
        }
        *self.observed.lock().expect("verifier mutex") = Some(seen);
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Accepts any certificate. Only reachable through an explicit
/// `insecure` choice, which the UI is required to surface (§13).
#[derive(Debug)]
struct NoVerification {
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for NoVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, tokio_rustls::rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;
    use tokio_rustls::rustls::ServerConfig;
    use tokio_rustls::rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};

    use super::*;
    use ::test_support::TempDir;

    /// A self-signed TLS server, like the ones MUDs commonly run. Returns
    /// its port and the SHA-256 of the certificate it presents.
    async fn self_signed_server() -> (u16, String) {
        let issued = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert = issued.cert.der().clone();
        let expected = fingerprint(&cert);
        let key =
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(issued.signing_key.serialize_der()));

        let provider = Arc::new(tokio_rustls::rustls::crypto::aws_lc_rs::default_provider());
        let config = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert], key)
            .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let acceptor = TlsAcceptor::from(Arc::new(config));

        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    if let Ok(mut stream) = acceptor.accept(sock).await {
                        let _ = stream.write_all(b"Welcome to SecureMUD\r\n").await;
                        let mut buf = [0u8; 64];
                        let _ = stream.read(&mut buf).await;
                    }
                });
            }
        });

        (port, expected)
    }

    fn tls(verify: VerifyMode, dir: &TempDir) -> TlsConfig {
        TlsConfig {
            verify,
            pin_store: PinStore::new(dir.path().join("known_certs")),
        }
    }

    /// M2's acceptance case: a self-signed MUD is reachable via pinning,
    /// and the fingerprint is recorded on that first connect.
    #[tokio::test]
    async fn pinned_trusts_a_self_signed_server_on_first_use() {
        let (port, expected) = self_signed_server().await;
        let dir = TempDir::new();
        let tls = tls(VerifyMode::Pinned, &dir);

        let mut connection = connect("localhost", port, Some(&tls)).await.unwrap();
        assert_eq!(connection.security.label, "TLS pinned");
        assert!(
            connection
                .security
                .warning
                .as_deref()
                .unwrap()
                .contains(&expected),
            "first contact must surface the fingerprint it pinned"
        );

        let mut buf = [0u8; 32];
        let n = connection.transport.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"Welcome to SecureMUD\r\n");

        assert_eq!(
            tls.pin_store.get(&pins::key("localhost", port)).unwrap(),
            Some(expected)
        );
    }

    #[tokio::test]
    async fn pinned_reconnects_quietly_to_the_same_certificate() {
        let (port, _) = self_signed_server().await;
        let dir = TempDir::new();
        let tls = tls(VerifyMode::Pinned, &dir);

        connect("localhost", port, Some(&tls)).await.unwrap();
        let again = connect("localhost", port, Some(&tls)).await.unwrap();

        assert_eq!(again.security.label, "TLS pinned");
        assert_eq!(
            again.security.warning, None,
            "an already-known certificate is not worth warning about"
        );
    }

    /// The reason pinning is worth having: a different certificate on a
    /// pinned host is refused rather than silently accepted.
    #[tokio::test]
    async fn pinned_refuses_a_changed_certificate() {
        let (first_port, _) = self_signed_server().await;
        let dir = TempDir::new();
        let tls = tls(VerifyMode::Pinned, &dir);
        connect("localhost", first_port, Some(&tls)).await.unwrap();

        // A second server, same host, different key: the MITM shape.
        let (second_port, _) = self_signed_server().await;
        let pinned = tls
            .pin_store
            .get(&pins::key("localhost", first_port))
            .unwrap();
        tls.pin_store
            .insert(&pins::key("localhost", second_port), &pinned.unwrap())
            .unwrap();

        let err = connect("localhost", second_port, Some(&tls))
            .await
            .expect_err("a changed certificate must not connect");
        let message = format!("{err:#}");
        assert!(
            message.contains("pinned certificate mismatch"),
            "unexpected error: {message}"
        );
    }

    #[tokio::test]
    async fn full_verification_rejects_a_self_signed_server() {
        let (port, _) = self_signed_server().await;
        let dir = TempDir::new();
        let tls = tls(VerifyMode::Full, &dir);

        let err = connect("localhost", port, Some(&tls))
            .await
            .expect_err("self-signed must fail full verification");
        assert!(format!("{err:#}").contains("TLS handshake"));
    }

    #[tokio::test]
    async fn insecure_connects_but_says_so_loudly() {
        let (port, _) = self_signed_server().await;
        let dir = TempDir::new();
        let tls = tls(VerifyMode::Insecure, &dir);

        let connection = connect("localhost", port, Some(&tls)).await.unwrap();
        assert_eq!(connection.security.label, "TLS insecure");
        assert!(
            connection
                .security
                .warning
                .as_deref()
                .unwrap()
                .contains("NOT verified"),
            "§13 requires insecure mode to be surfaced"
        );
        // Nothing is pinned by a mode that verifies nothing.
        assert_eq!(
            tls.pin_store.get(&pins::key("localhost", port)).unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn plain_tcp_reports_no_transport_security() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = listener.accept().await;
        });

        let connection = connect("127.0.0.1", port, None).await.unwrap();
        assert_eq!(connection.security.label, "plain");
        assert_eq!(connection.security.warning, None);
    }

    #[test]
    fn parses_verify_modes_and_rejects_typos() {
        assert_eq!("full".parse::<VerifyMode>().unwrap(), VerifyMode::Full);
        assert_eq!("pinned".parse::<VerifyMode>().unwrap(), VerifyMode::Pinned);
        assert_eq!(
            "insecure".parse::<VerifyMode>().unwrap(),
            VerifyMode::Insecure
        );
        assert!("Full".parse::<VerifyMode>().is_err());
        assert!("none".parse::<VerifyMode>().is_err());
    }
}
