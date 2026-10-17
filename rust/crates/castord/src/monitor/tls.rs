//! The certificate a host serves, read the way a browser would connect:
//! verified against the public roots (and the dev CA), but read even when it
//! is bad, so an expired or mismatched certificate is reported as such.
//! Nothing is sent over the connection; it is closed after the handshake.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::client::WebPkiServerVerifier;
use tokio_rustls::rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{self, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};

use super::rules::CertResult;

/// Accepts any certificate, recording what the real verifier said of it.
/// Handshake signatures are still checked, so the certificate read is the
/// one the server holds the key of.
#[derive(Debug)]
struct Recording {
    inner: Arc<WebPkiServerVerifier>,
    verdict: Mutex<Option<Result<(), String>>>,
}

impl ServerCertVerifier for Recording {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let verdict = self
            .inner
            .verify_server_cert(end_entity, intermediates, server_name, ocsp, now)
            .map(|_| ())
            .map_err(|e| e.to_string());
        if let Ok(mut v) = self.verdict.lock() {
            *v = Some(verdict);
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// The public roots plus an extra PEM bundle (the dev CA).
pub fn roots(extra_pem: Option<&[u8]>) -> Result<Arc<RootCertStore>, String> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(pem) = extra_pem {
        for cert in rustls_pemfile_certs(pem)? {
            roots.add(cert).map_err(|e| format!("extra CA: {e}"))?;
        }
    }
    Ok(Arc::new(roots))
}

fn rustls_pemfile_certs(pem: &[u8]) -> Result<Vec<CertificateDer<'static>>, String> {
    use tokio_rustls::rustls::pki_types::pem::PemObject;
    CertificateDer::pem_slice_iter(pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("extra CA: {e}"))
}

/// Connects to `address` (default: `host`) on port 443 with `host` as the
/// server name.
pub async fn certificate_of(host: &str, address: Option<&str>, roots: Arc<RootCertStore>) -> CertResult {
    let failed = |error: String| CertResult {
        host: host.to_owned(),
        valid_to: None,
        trusted: false,
        error: Some(error),
    };
    let inner =
        match WebPkiServerVerifier::builder_with_provider(roots, Arc::new(rustls::crypto::ring::default_provider()))
            .build()
        {
            Ok(v) => v,
            Err(e) => return failed(e.to_string()),
        };
    let verifier = Arc::new(Recording {
        inner,
        verdict: Mutex::new(None),
    });
    let config = match ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
    {
        Ok(b) => b
            .dangerous()
            .with_custom_certificate_verifier(verifier.clone())
            .with_no_client_auth(),
        Err(e) => return failed(e.to_string()),
    };
    let Ok(name) = ServerName::try_from(host.to_owned()) else {
        return failed("invalid host name".into());
    };
    let handshake = async {
        let tcp = TcpStream::connect((address.unwrap_or(host), 443))
            .await
            .map_err(|e| e.to_string())?;
        let tls = TlsConnector::from(Arc::new(config))
            .connect(name, tcp)
            .await
            .map_err(|e| e.to_string())?;
        let der = tls
            .get_ref()
            .1
            .peer_certificates()
            .and_then(|c| c.first())
            .map(|c| c.to_vec())
            .ok_or("no certificate")?;
        Ok::<_, String>(der)
    };
    let der = match tokio::time::timeout(Duration::from_secs(10), handshake).await {
        Ok(Ok(der)) => der,
        Ok(Err(e)) => return failed(e),
        Err(_) => return failed("timeout".into()),
    };
    let valid_to = match x509_parser::parse_x509_certificate(&der) {
        Ok((_, cert)) => cert.validity().not_after.timestamp().saturating_mul(1000),
        Err(e) => return failed(format!("certificate: {e}")),
    };
    let verdict = verifier
        .verdict
        .lock()
        .ok()
        .and_then(|v| v.clone())
        .unwrap_or_else(|| Err("not verified".into()));
    CertResult {
        host: host.to_owned(),
        valid_to: Some(valid_to),
        trusted: verdict.is_ok(),
        error: verdict.err(),
    }
}
