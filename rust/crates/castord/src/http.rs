//! The HTTP client for the control plane's APIs (Authentik, the LLM gateway).

use std::time::Duration;

/// No redirects (a bearer token must not follow one elsewhere), a timeout,
/// and optionally one more CA (CASTORD_EXTRA_CA_CERTS: the dev CA, as
/// NODE_EXTRA_CA_CERTS is for the TypeScript worker).
pub fn client(extra_ca_pem: Option<&[u8]>) -> Result<reqwest::Client, String> {
    let mut b = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none());
    if let Some(pem) = extra_ca_pem {
        for cert in reqwest::Certificate::from_pem_bundle(pem).map_err(|e| format!("extra CA: {e}"))? {
            b = b.add_root_certificate(cert);
        }
    }
    b.build().map_err(|e| e.to_string())
}
