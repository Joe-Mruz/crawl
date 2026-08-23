//! Smoke test for HTTPS support: binds the real router behind
//! `axum_server::bind_rustls` with a self-signed cert (generated via the
//! `openssl` CLI - no test-only crypto deps needed beyond a TLS client),
//! then fetches `/status/version/` over TLS with certificate verification
//! disabled (self-signed, so there's nothing else to trust).

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use webtiles_rs::config::ServerConfig;
use webtiles_rs::state::AppState;
use webtiles_rs::userdb::UserDb;

/// Generate a self-signed cert/key pair via the `openssl` CLI into `dir`,
/// returning (cert_path, key_path).
fn generate_self_signed_cert(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");
    let status = std::process::Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-keyout",
            key.to_str().unwrap(),
            "-out",
            cert.to_str().unwrap(),
            "-days",
            "1",
            "-subj",
            "/CN=localhost",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("failed to run openssl");
    assert!(status.success(), "openssl failed to generate a test certificate");
    (cert, key)
}

/// Accepts any server certificate - fine for a test against a self-signed
/// cert we just generated ourselves, never for production use.
#[derive(Debug)]
struct AcceptAnyCert;

impl ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::aws_lc_rs::default_provider().signature_verification_algorithms.supported_schemes()
    }
}

#[tokio::test]
async fn serves_status_version_over_https() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = generate_self_signed_cert(dir.path());

    let users = UserDb::open(dir.path().join("passwd.db3"), dir.path().join("settings.db3")).unwrap();
    let config = ServerConfig::default();
    let state = AppState::new(config, users);
    let router = webtiles_rs::http::build_router(state);

    let tls_config = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert, &key).await.unwrap();
    let handle = axum_server::Handle::<std::net::SocketAddr>::new();
    let bind_handle = handle.clone();
    tokio::spawn(async move {
        axum_server::bind_rustls("127.0.0.1:0".parse().unwrap(), tls_config)
            .handle(bind_handle)
            .serve(router.into_make_service())
            .await
            .unwrap();
    });
    let addr = handle.listening().await.expect("server should start listening");

    let client_config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCert))
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));

    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let server_name = ServerName::try_from("localhost").unwrap();
    let mut tls = connector.connect(server_name, tcp).await.unwrap();

    tls.write_all(b"GET /status/version/ HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut buf = Vec::new();
    tls.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf);

    assert!(text.starts_with("HTTP/1.1 200"), "expected 200 OK over HTTPS, got: {text}");
    assert!(text.contains("\"rust\""), "expected the rust status field in the body: {text}");
}
