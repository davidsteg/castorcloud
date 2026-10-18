//! The link between castord and a node role (`castor node`, docs/gatekeeper.md).
//!
//! castord holds no gatekeeper token. It asks the node role for one tool of
//! the castor catalog (crate::gatekeeper) at a time; the node role checks the
//! call against the same catalog, then forwards it to gatekeeper with the
//! token only it holds. Nothing else crosses: no generic proxy, no shell.
//!
//! TLS 1.3 with mutual authentication, each side pinned to the other's exact
//! certificate (self-signed, no CA). One request per connection, as a
//! length-prefixed JSON frame:
//!   request  {"tool": "<id>", "args": {...}, "secret": "<value>"?}
//!   answer   {"ok": "<stdout>"} or {"err": "<message>"}
//! The secret (a dataset passphrase, a file's content) is the value of the
//! tool's one secret parameter; neither side logs it.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio_rustls::rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use tokio_rustls::rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use tokio_rustls::rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use tokio_rustls::rustls::{
    self, ClientConfig, DigitallySignedStruct, DistinguishedName, ServerConfig, SignatureScheme,
};
use tokio_rustls::{TlsAcceptor, TlsConnector};
use zeroize::Zeroizing;

use crate::exec::ToolCall;
use crate::gatekeeper::Tool;

/// A file's content is the largest request (templates, configs).
const MAX_REQUEST: usize = 4 * 1024 * 1024;
/// gatekeeper's own answer limit, plus room for the JSON around it.
const MAX_ANSWER: usize = 9 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Longer than gatekeeper's longest tool (15 minutes).
const CALL_TIMEOUT: Duration = Duration::from_secs(17 * 60);
/// A caller must have sent its whole request by then.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Calls the node role runs at once.
const CONCURRENT: usize = 16;

/// `host:port` of a node role: a Docker network name, a DNS name or an IPv4
/// address. No IPv6 literals (the port split would be ambiguous).
pub fn address_ok(addr: &str) -> bool {
    let Some((host, port)) = addr.rsplit_once(':') else {
        return false;
    };
    (1..=253).contains(&host.len())
        && host
            .split('.')
            .all(|l| !l.is_empty() && !l.starts_with('-') && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
        && port.parse::<u16>().is_ok_and(|p| p > 0)
}

/// A certificate chain and its key, from PEM files.
pub struct Identity {
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
}

impl Identity {
    pub fn from_pem(cert_pem: &[u8], key_pem: &[u8]) -> Result<Self, String> {
        let chain = pem_chain(cert_pem)?;
        let key = PrivateKeyDer::from_pem_slice(key_pem).map_err(|e| format!("link key: {e}"))?;
        Ok(Self { chain, key })
    }

    pub fn from_files(cert_file: &str, key_file: &str) -> Result<Self, String> {
        let cert = std::fs::read(cert_file).map_err(|e| format!("{cert_file}: {e}"))?;
        let key = Zeroizing::new(std::fs::read(key_file).map_err(|e| format!("{key_file}: {e}"))?);
        Self::from_pem(&cert, &key)
    }
}

fn pem_chain(pem: &[u8]) -> Result<Vec<CertificateDer<'static>>, String> {
    let chain = CertificateDer::pem_slice_iter(pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("link certificates: {e}"))?;
    if chain.is_empty() {
        return Err("link certificates: none found".into());
    }
    Ok(chain)
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Accepts exactly these certificates, and checks the handshake was signed
/// with the key of the one presented.
#[derive(Debug)]
struct Pinned {
    chain: Vec<CertificateDer<'static>>,
    provider: Arc<CryptoProvider>,
}

impl Pinned {
    fn new(pem: &[u8]) -> Result<Self, String> {
        Ok(Self {
            chain: pem_chain(pem)?,
            provider: provider(),
        })
    }

    fn check(&self, end_entity: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        if self.chain.iter().any(|c| c.as_ref() == end_entity.as_ref()) {
            Ok(())
        } else {
            Err(rustls::Error::General("certificate is not pinned".into()))
        }
    }

    fn tls12(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn tls13(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.check(end_entity)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.tls12(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.tls13(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes()
    }
}

impl ClientCertVerifier for Pinned {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.check(end_entity)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.tls12(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.tls13(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes()
    }
}

/// castord's side.
pub struct Client {
    tls: TlsConnector,
}

impl Client {
    /// `peers_pem`: the certificates of the node roles castord talks to.
    pub fn new(identity: Identity, peers_pem: &[u8]) -> Result<Self, String> {
        let config = ClientConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|e| e.to_string())?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(Pinned::new(peers_pem)?))
            .with_client_auth_cert(identity.chain, identity.key)
            .map_err(|e| format!("link identity: {e}"))?;
        Ok(Self {
            tls: TlsConnector::from(Arc::new(config)),
        })
    }

    /// LINK_CERT_FILE, LINK_KEY_FILE and LINK_PEERS_FILE; None without them.
    pub fn from_env() -> Result<Option<Self>, String> {
        let var = crate::config::var;
        let (cert, key, peers) = (var("LINK_CERT_FILE"), var("LINK_KEY_FILE"), var("LINK_PEERS_FILE"));
        let (Some(cert), Some(key), Some(peers)) = (cert.clone(), key.clone(), peers.clone()) else {
            if cert.or(key).or(peers).is_some() {
                return Err("LINK_CERT_FILE, LINK_KEY_FILE and LINK_PEERS_FILE go together".into());
            }
            return Ok(None);
        };
        let peers_pem = std::fs::read(&peers).map_err(|e| format!("{peers}: {e}"))?;
        Self::new(Identity::from_files(&cert, &key)?, &peers_pem).map(Some)
    }

    /// Runs one tool on the node role at `addr`; its stdout, or the error.
    pub async fn call(&self, addr: &str, call: ToolCall) -> Result<String, String> {
        let name = call.name.clone();
        tokio::time::timeout(CALL_TIMEOUT, self.exchange(addr, &call))
            .await
            .map_err(|_| format!("{name}: node {addr} did not answer"))?
            .map_err(|e| format!("{name}: {e}"))
    }

    async fn exchange(&self, addr: &str, call: &ToolCall) -> Result<String, String> {
        if !address_ok(addr) {
            return Err(format!("invalid node address {addr}"));
        }
        let host = addr.rsplit_once(':').map_or(addr, |(h, _)| h);
        let name = ServerName::try_from(host.to_owned()).map_err(|e| format!("{addr}: {e}"))?;
        let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
            .await
            .map_err(|_| format!("node {addr}: connect timed out"))?
            .map_err(|e| format!("node {addr}: {e}"))?;
        let mut tls = self
            .tls
            .connect(name, tcp)
            .await
            .map_err(|e| format!("node {addr}: TLS: {e}"))?;
        write_frame(&mut tls, &encode(call)?).await?;
        tls.flush().await.map_err(|e| e.to_string())?;
        let answer = read_frame(&mut tls, MAX_ANSWER).await?;
        decode_answer(&answer)
    }
}

#[derive(Serialize)]
struct Request<'a> {
    tool: &'a str,
    args: &'a Map<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    secret: Option<&'a str>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Incoming {
    tool: String,
    args: Map<String, Value>,
    secret: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
enum Answer {
    Ok(String),
    Err(String),
}

/// The request, in a buffer that is wiped afterwards.
fn encode(call: &ToolCall) -> Result<Zeroizing<Vec<u8>>, String> {
    let secret = call.secret.as_ref().map(|(_, s)| s.as_str());
    let args_len = Value::Object(call.args.clone()).to_string().len();
    let mut buf = Zeroizing::new(Vec::with_capacity(
        call.name.len() + args_len + secret.map_or(0, |s| s.len() * 6) + 64,
    ));
    serde_json::to_writer(
        &mut *buf,
        &Request {
            tool: &call.name,
            args: &call.args,
            secret,
        },
    )
    .map_err(|e| e.to_string())?;
    Ok(buf)
}

fn decode_answer(bytes: &[u8]) -> Result<String, String> {
    match serde_json::from_slice(bytes).map_err(|e| format!("node answer: {e}"))? {
        Answer::Ok(out) => Ok(out),
        Answer::Err(e) => Err(e),
    }
}

/// A request from castord as a call of one catalog tool, checked the way
/// gatekeeper will check it: every parameter given, nothing more, each
/// value passing its check, the secret only where the tool has one.
pub fn checked_call(tools: &[Tool], bytes: &[u8]) -> Result<ToolCall, String> {
    let Incoming { tool, args, secret } = serde_json::from_slice(bytes).map_err(|_| "malformed request".to_owned())?;
    let secret = secret.map(Zeroizing::new);
    let t = tools
        .iter()
        .find(|t| t.id == tool)
        .ok_or_else(|| format!("no tool {tool}"))?;
    let refuse = |what: &str| format!("{tool}: {what}");
    let mut secret_param = None;
    for p in &t.params {
        if p.secret {
            secret_param = Some(p);
            continue;
        }
        match args.get(p.name).and_then(Value::as_str) {
            Some(v) if p.check.ok(v) => {}
            Some(_) => return Err(refuse(&format!("{} not allowed", p.name))),
            None => return Err(refuse(&format!("{} missing", p.name))),
        }
    }
    if args.keys().any(|k| !t.params.iter().any(|p| !p.secret && p.name == k)) {
        return Err(refuse("unknown parameter"));
    }
    let secret = match (secret_param, secret) {
        (Some(p), Some(s)) if p.check.ok(&s) => Some((p.name, s)),
        (None, None) => None,
        _ => return Err(refuse("secret does not fit the tool")),
    };
    Ok(ToolCall {
        name: t.id.to_owned(),
        args,
        secret,
    })
}

async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, body: &[u8]) -> Result<(), String> {
    let len = u32::try_from(body.len()).map_err(|_| "frame too large")?;
    w.write_all(&len.to_be_bytes()).await.map_err(|e| e.to_string())?;
    w.write_all(body).await.map_err(|e| e.to_string())
}

/// One frame, in a buffer that is wiped afterwards.
async fn read_frame<R: AsyncRead + Unpin>(r: &mut R, max: usize) -> Result<Zeroizing<Vec<u8>>, String> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await.map_err(|e| format!("link: {e}"))?;
    let len = u32::from_be_bytes(len) as usize;
    if len > max {
        return Err("link: frame too large".into());
    }
    let mut buf = Zeroizing::new(vec![0u8; len]);
    r.read_exact(&mut buf).await.map_err(|e| format!("link: {e}"))?;
    Ok(buf)
}

/// Where the node role sends a checked call: gatekeeper.
#[async_trait::async_trait]
pub trait Upstream: Send + Sync {
    async fn call(&self, call: ToolCall) -> Result<String, String>;
}

#[async_trait::async_trait]
impl Upstream for crate::gatekeeper::client::Gatekeeper {
    async fn call(&self, call: ToolCall) -> Result<String, String> {
        crate::gatekeeper::client::Gatekeeper::call(self, call).await
    }
}

/// The node role's side: TLS 1.3, castord's certificate only.
pub fn acceptor(identity: Identity, peers_pem: &[u8]) -> Result<TlsAcceptor, String> {
    let config = ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| e.to_string())?
        .with_client_cert_verifier(Arc::new(Pinned::new(peers_pem)?))
        .with_single_cert(identity.chain, identity.key)
        .map_err(|e| format!("link identity: {e}"))?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Serves castord's calls until the listener fails.
pub async fn serve(
    listener: TcpListener,
    tls: TlsAcceptor,
    tools: Arc<Vec<Tool>>,
    upstream: Arc<dyn Upstream>,
) -> Result<(), String> {
    let slots = Arc::new(Semaphore::new(CONCURRENT));
    loop {
        let (tcp, peer) = listener.accept().await.map_err(|e| format!("accept: {e}"))?;
        let Ok(slot) = slots.clone().try_acquire_owned() else {
            eprintln!("castor-node: {peer}: busy, refused");
            continue;
        };
        let (tls, tools, upstream) = (tls.clone(), tools.clone(), upstream.clone());
        tokio::spawn(async move {
            if let Err(e) = handle(tcp, tls, &tools, upstream.as_ref()).await {
                eprintln!("castor-node: {peer}: {e}");
            }
            drop(slot);
        });
    }
}

async fn handle(tcp: TcpStream, tls: TlsAcceptor, tools: &[Tool], upstream: &dyn Upstream) -> Result<(), String> {
    let (mut stream, request) = tokio::time::timeout(REQUEST_TIMEOUT, async {
        let mut stream = tls.accept(tcp).await.map_err(|e| format!("TLS: {e}"))?;
        let request = read_frame(&mut stream, MAX_REQUEST).await?;
        Ok::<_, String>((stream, request))
    })
    .await
    .map_err(|_| "request timed out".to_owned())??;
    let answer = match checked_call(tools, &request) {
        Err(e) => {
            eprintln!("castor-node: refused: {e}");
            Answer::Err(e)
        }
        Ok(call) => {
            let name = call.name.clone();
            match upstream.call(call).await {
                Ok(out) => {
                    eprintln!("castor-node: {name}: ok");
                    Answer::Ok(out)
                }
                Err(e) => {
                    eprintln!("castor-node: {name}: failed");
                    Answer::Err(e)
                }
            }
        }
    };
    drop(request);
    let body = serde_json::to_vec(&answer).map_err(|e| e.to_string())?;
    write_frame(&mut stream, &body).await?;
    stream.shutdown().await.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn addresses() {
        assert!(address_ok("castor-node:7443"));
        assert!(address_ok("10.0.0.7:7443"));
        assert!(address_ok("nas.lan:1"));
        assert!(!address_ok("castor-node"));
        assert!(!address_ok("castor-node:0"));
        assert!(!address_ok(":7443"));
        assert!(!address_ok("-x:7443"));
        assert!(!address_ok("a b:7443"));
        assert!(!address_ok("user@host:7443"));
    }

    #[test]
    fn requests_are_checked_against_the_catalog() {
        let tools = crate::gatekeeper::catalog("tank");
        let ok = |v: Value| checked_call(&tools, v.to_string().as_bytes());
        let call = ok(serde_json::json!({"tool": "docker.tenant_up", "args": {"slug": "acme"}})).unwrap();
        assert_eq!(call.label(), r#"docker.tenant_up {"slug":"acme"}"#);
        // Unknown tools, missing, extra or bad parameters.
        assert!(ok(serde_json::json!({"tool": "docker.run", "args": {}})).is_err());
        assert!(ok(serde_json::json!({"tool": "docker.tenant_up", "args": {}})).is_err());
        assert!(ok(serde_json::json!({"tool": "docker.tenant_up", "args": {"slug": "acme", "x": "1"}})).is_err());
        assert!(ok(serde_json::json!({"tool": "docker.tenant_up", "args": {"slug": "../etc"}})).is_err());
        assert!(ok(serde_json::json!({"tool": "docker.tenant_up", "args": {"slug": 1}})).is_err());
        assert!(ok(serde_json::json!({"tool": "docker.tenant_up", "args": {"slug": "acme"}, "x": 1})).is_err());
        // The secret only where the tool has one.
        assert!(ok(serde_json::json!({"tool": "docker.tenant_up", "args": {"slug": "acme"}, "secret": "s"})).is_err());
        let path = "/mnt/tank/castor/node/tenants/acme/tenant.env";
        let write =
            ok(serde_json::json!({"tool": "file.tenant_write", "args": {"path": path}, "secret": "A=1\n"})).unwrap();
        assert_eq!(
            write.secret.as_ref().map(|(n, s)| (*n, s.as_str())),
            Some(("content", "A=1\n"))
        );
        assert!(ok(serde_json::json!({"tool": "file.tenant_write", "args": {"path": path}})).is_err());
        assert!(
            ok(serde_json::json!({"tool": "file.tenant_write", "args": {"path": "/etc/passwd"}, "secret": "x"}))
                .is_err()
        );
        // The secret can't come in as an ordinary argument either.
        assert!(
            ok(serde_json::json!({"tool": "file.tenant_write", "args": {"path": path, "content": "x"}, "secret": "x"}))
                .is_err()
        );
    }

    struct Echo;

    #[async_trait::async_trait]
    impl Upstream for Echo {
        async fn call(&self, call: ToolCall) -> Result<String, String> {
            match &call.secret {
                Some((_, s)) => Ok(format!("{} {}", call.name, s.len())),
                None => Ok(call.label()),
            }
        }
    }

    fn identity(name: &str) -> (Identity, String) {
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).unwrap();
        let cert = rcgen::CertificateParams::new(vec![name.to_owned()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        let pem = cert.pem();
        (
            Identity::from_pem(pem.as_bytes(), key.serialize_pem().as_bytes()).unwrap(),
            pem,
        )
    }

    #[tokio::test]
    async fn calls_cross_with_pinned_certificates_only() {
        let (node_id, node_pem) = identity("castor-node");
        let (castord_id, castord_pem) = identity("castord");
        let (stranger_id, _) = identity("castord");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
        let tls = acceptor(node_id, castord_pem.as_bytes()).unwrap();
        tokio::spawn(serve(
            listener,
            tls,
            Arc::new(crate::gatekeeper::catalog("tank")),
            Arc::new(Echo),
        ));

        let client = Client::new(castord_id, node_pem.as_bytes()).unwrap();
        let mut args = Map::new();
        args.insert("slug".into(), "acme".into());
        let up = ToolCall {
            name: "docker.tenant_up".into(),
            args: args.clone(),
            secret: None,
        };
        assert_eq!(
            client.call(&addr, up).await.unwrap(),
            r#"docker.tenant_up {"slug":"acme"}"#
        );
        let mut file = Map::new();
        file.insert("path".into(), "/mnt/tank/castor/node/tenants/acme/tenant.env".into());
        let write = ToolCall {
            name: "file.tenant_write".into(),
            args: file,
            secret: Some(("content", Zeroizing::new("A=1\n".into()))),
        };
        assert_eq!(client.call(&addr, write).await.unwrap(), "file.tenant_write 4");
        // The node role refuses what isn't in the catalog.
        let bad = ToolCall {
            name: "docker.run".into(),
            args,
            secret: None,
        };
        assert_eq!(
            client.call(&addr, bad).await.unwrap_err(),
            "docker.run: no tool docker.run"
        );

        // A client whose certificate isn't pinned gets nothing.
        let stranger = Client::new(stranger_id, node_pem.as_bytes()).unwrap();
        let none = ToolCall {
            name: "docker.version".into(),
            args: Map::new(),
            secret: None,
        };
        assert!(stranger.call(&addr, none).await.is_err());
        // Nor does castord talk to a node role whose certificate isn't pinned.
        let (_, other_pem) = identity("castor-node");
        let (castord_id, _) = identity("castord");
        let wrong = Client::new(castord_id, other_pem.as_bytes()).unwrap();
        let none = ToolCall {
            name: "docker.version".into(),
            args: Map::new(),
            secret: None,
        };
        assert!(wrong.call(&addr, none).await.unwrap_err().contains("TLS"));
    }
}
