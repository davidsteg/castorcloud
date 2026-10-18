//! Castor's own node images: cryptd (local LUKS helper), assistant-tools
//! (every tenant node) and egress (every stacks node).
//!
//! Production (CASTOR_IMAGE_TAG set): the images come from GHCR, published
//! and signed by the Images workflow (docs/ci.md). castord resolves each tag
//! to a digest and accepts it only when a cosign signature from
//! CASTOR_COSIGN_PUBKEY covers that repository and digest; the node then
//! pulls exactly that digest and tags it with the local name the stacks use.
//! The pull credential (GHCR_PULL_TOKEN) stays on the control plane: the
//! docker CLI hands it to the node's engine for that one pull.
//!
//! Development (CASTOR_IMAGE_TAG unset): the node builds them from this
//! checkout, as before.
//!
//! The signature check needs no cosign binary. Scope: key-based signatures in
//! the classic layout cosign v2 writes (`cosign sign --key`, stored at
//! `<repo>:sha256-<hex>.sig`).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::node::Node;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeImage {
    Cryptd,
    AssistantTools,
    Egress,
}

impl NodeImage {
    pub fn name(self) -> &'static str {
        match self {
            NodeImage::Cryptd => "cryptd",
            NodeImage::AssistantTools => "assistant-tools",
            NodeImage::Egress => "egress",
        }
    }
}

const DEFAULT_REPO: &str = "ghcr.io/davidsteg/castorcloud";
const MAX_BYTES: usize = 4 * 1024 * 1024;
const SIG_ANNOTATION: &str = "dev.cosignproject.cosign/signature";
const MANIFESTS: &str = "application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, \
     application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json";

/// Where the images come from (the control plane's .env).
pub enum Source {
    Build,
    Registry(Registry),
}

pub struct Registry {
    /// ghcr.io/davidsteg/castorcloud; each image is `<repo>-<name>`.
    pub repo: String,
    pub tag: String,
    /// The uncompressed P-256 point of the cosign public key.
    key: Vec<u8>,
    user: String,
    token: Option<Zeroizing<String>>,
}

impl Source {
    /// CASTOR_IMAGE_TAG, CASTOR_IMAGE_REPO, CASTOR_COSIGN_PUBKEY (a file),
    /// GHCR_PULL_USER, GHCR_PULL_TOKEN.
    pub fn from_env(var: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let Some(tag) = var("CASTOR_IMAGE_TAG")
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty())
        else {
            return Ok(Source::Build);
        };
        if !valid_tag(&tag) {
            return Err("CASTOR_IMAGE_TAG is not a valid image tag".into());
        }
        let repo = var("CASTOR_IMAGE_REPO").map_or_else(|| DEFAULT_REPO.to_owned(), |r| r.trim().to_owned());
        if !valid_repo(&repo) {
            return Err("CASTOR_IMAGE_REPO is not a valid repository".into());
        }
        // Unsigned images are never run: no key, no pull.
        let path = var("CASTOR_COSIGN_PUBKEY").ok_or("CASTOR_IMAGE_TAG is set but CASTOR_COSIGN_PUBKEY is not")?;
        let pem = std::fs::read(path.trim()).map_err(|e| format!("CASTOR_COSIGN_PUBKEY {}: {e}", path.trim()))?;
        let key = public_key(&pem).map_err(|e| format!("CASTOR_COSIGN_PUBKEY: {e}"))?;
        let token = var("GHCR_PULL_TOKEN")
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty());
        let user = var("GHCR_PULL_USER")
            .map(|u| u.trim().to_owned())
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| "castor".into());
        if user.contains(':') || user.chars().any(char::is_control) {
            return Err("GHCR_PULL_USER is not a valid user name".into());
        }
        Ok(Source::Registry(Registry {
            repo,
            tag,
            key,
            user,
            token: token.map(Zeroizing::new),
        }))
    }
}

/// A Docker tag: [A-Za-z0-9_][A-Za-z0-9_.-]{0,127}.
fn valid_tag(t: &str) -> bool {
    let mut chars = t.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        && t.len() <= 128
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// host[:port]/path with lower-case path components.
fn valid_repo(r: &str) -> bool {
    let Some((host, path)) = r.split_once('/') else {
        return false;
    };
    let (name, port) = host.split_once(':').unwrap_or((host, ""));
    let host_ok = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-'))
        && (port.is_empty() || (port.len() <= 5 && port.chars().all(|c| c.is_ascii_digit())));
    host_ok
        && r.len() <= 255
        && path.split('/').all(|p| {
            !p.is_empty()
                && p.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
        })
}

/// The P-256 point of a PEM "PUBLIC KEY" (cosign.pub).
fn public_key(pem: &[u8]) -> Result<Vec<u8>, String> {
    use x509_parser::oid_registry::{OID_EC_P256, OID_KEY_TYPE_EC_PUBLIC_KEY};
    use x509_parser::prelude::FromDer;
    let (_, pem) = x509_parser::pem::parse_x509_pem(pem).map_err(|_| "not a PEM file")?;
    if pem.label != "PUBLIC KEY" {
        return Err(format!("expected a PUBLIC KEY, not {}", pem.label));
    }
    let (rest, spki) =
        x509_parser::x509::SubjectPublicKeyInfo::from_der(&pem.contents).map_err(|_| "not a public key")?;
    let curve = spki.algorithm.parameters.as_ref().and_then(|p| p.as_oid().ok());
    if !rest.is_empty() || spki.algorithm.algorithm != OID_KEY_TYPE_EC_PUBLIC_KEY || curve != Some(OID_EC_P256) {
        return Err("only ECDSA P-256 keys (cosign generate-key-pair) are supported".into());
    }
    let point = spki.subject_public_key.data.to_vec();
    if point.len() != 65 || point[0] != 4 {
        return Err("not an uncompressed P-256 point".into());
    }
    Ok(point)
}

fn sha256(b: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(b)))
}

fn valid_digest(d: &str) -> bool {
    d.strip_prefix("sha256:")
        .is_some_and(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)))
}

/// Plain HTTP only for a registry on this machine (tests); everything else is HTTPS.
fn is_local(host: &str) -> bool {
    let name = host.rsplit_once(':').map_or(host, |(n, _)| n);
    name == "localhost" || name == "127.0.0.1"
}

fn base(host: &str) -> String {
    if is_local(host) {
        format!("http://{host}")
    } else {
        format!("https://{host}")
    }
}

/// Request headers, names in lower case.
pub type Headers = Vec<(&'static str, String)>;

pub struct Response {
    pub status: u16,
    /// Names in lower case.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

/// GET with headers; the body is cut off above MAX_BYTES.
#[async_trait::async_trait]
pub trait Fetch: Send + Sync {
    async fn get(&self, url: &str, headers: Headers) -> Result<Response, String>;
}

/// reqwest, following redirects (GHCR serves blobs from another host) only
/// to HTTPS; reqwest drops the Authorization header when one leaves the host.
pub struct Http(reqwest::Client);

impl Http {
    pub fn new() -> Result<Self, String> {
        let policy = reqwest::redirect::Policy::custom(|a| {
            let local = a.url().host_str().is_some_and(is_local);
            if a.url().scheme() != "https" && !local {
                a.error("redirect to plain HTTP")
            } else if a.previous().len() > 3 {
                a.stop()
            } else {
                a.follow()
            }
        });
        reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .redirect(policy)
            .build()
            .map(Http)
            .map_err(|e| e.to_string())
    }
}

#[async_trait::async_trait]
impl Fetch for Http {
    async fn get(&self, url: &str, headers: Headers) -> Result<Response, String> {
        let mut req = self.0.get(url);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        let mut res = req.send().await.map_err(|e| format!("registry: {e}"))?;
        let status = res.status().as_u16();
        let headers = res
            .headers()
            .iter()
            .filter_map(|(k, v)| Some((k.as_str().to_owned(), v.to_str().ok()?.to_owned())))
            .collect();
        let mut body = Vec::new();
        while let Some(chunk) = res.chunk().await.map_err(|e| format!("registry: {e}"))? {
            body.extend_from_slice(&chunk);
            if body.len() > MAX_BYTES {
                return Err("registry: response too large".into());
            }
        }
        Ok(Response { status, headers, body })
    }
}

/// One repository on a registry, answering one Bearer token challenge.
struct Client<'a> {
    fetch: &'a dyn Fetch,
    host: String,
    name: String,
    user: &'a str,
    token: Option<&'a str>,
    bearer: Option<Zeroizing<String>>,
}

impl Client<'_> {
    async fn get(&mut self, path: &str, accept: &str) -> Result<Response, String> {
        let url = format!("{}/v2/{}/{path}", base(&self.host), self.name);
        let mut res = self.fetch.get(&url, self.headers(accept)).await?;
        if res.status == 401 && self.bearer.is_none() {
            let challenge = res.header("www-authenticate").unwrap_or_default().to_owned();
            self.login(&challenge).await?;
            res = self.fetch.get(&url, self.headers(accept)).await?;
        }
        if res.status != 200 {
            return Err(format!("registry: {path} answered {}", res.status));
        }
        if res.body.len() > MAX_BYTES {
            return Err(format!("registry: {path} is too large"));
        }
        Ok(res)
    }

    fn headers(&self, accept: &str) -> Headers {
        let mut h = vec![("accept", accept.to_owned())];
        if let Some(b) = &self.bearer {
            h.push(("authorization", format!("Bearer {}", b.as_str())));
        }
        h
    }

    async fn login(&mut self, challenge: &str) -> Result<(), String> {
        let Some(params) = challenge
            .get(..7)
            .filter(|s| s.eq_ignore_ascii_case("bearer "))
            .map(|_| auth_params(&challenge[7..]))
        else {
            return Err("registry: unsupported auth challenge".into());
        };
        let realm = params.get("realm").ok_or("registry: unsupported auth challenge")?;
        let mut url = reqwest::Url::parse(realm).map_err(|_| "registry: invalid token realm")?;
        // The credential goes only to the registry's own host, over HTTPS
        // (plain HTTP only for a registry on this machine).
        let host = self.host.split(':').next().unwrap_or_default();
        if url.host_str() != Some(host) {
            return Err("registry: token realm on another host".into());
        }
        if url.scheme() != "https" && !(is_local(&self.host) && url.origin().ascii_serialization() == base(&self.host))
        {
            return Err("registry: insecure token realm".into());
        }
        {
            let mut q = url.query_pairs_mut();
            if let Some(s) = params.get("service") {
                q.append_pair("service", s);
            }
            // Only ever pull, whatever the challenge asks for.
            q.append_pair("scope", &format!("repository:{}:pull", self.name));
        }
        let mut headers = vec![];
        if let Some(t) = self.token {
            let basic = Zeroizing::new(B64.encode(format!("{}:{t}", self.user)));
            headers.push(("authorization", format!("Basic {}", basic.as_str())));
        }
        let res = self.fetch.get(url.as_str(), headers).await?;
        if res.status != 200 {
            return Err(format!("registry: token request answered {}", res.status));
        }
        let json: serde_json::Value =
            serde_json::from_slice(&res.body).map_err(|_| "registry: token answer is not JSON")?;
        let token = json["token"]
            .as_str()
            .or_else(|| json["access_token"].as_str())
            .filter(|t| !t.is_empty())
            .ok_or("registry: no token")?;
        self.bearer = Some(Zeroizing::new(token.to_owned()));
        Ok(())
    }
}

/// key="value" pairs of a WWW-Authenticate challenge.
fn auth_params(s: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut rest = s;
    while let Some(eq) = rest.find("=\"") {
        let key = rest[..eq].trim_start_matches([',', ' ']).trim().to_ascii_lowercase();
        let after = &rest[eq + 2..];
        let Some(end) = after.find('"') else { break };
        out.insert(key, after[..end].to_owned());
        rest = &after[end + 1..];
    }
    out
}

/// Resolves repo:tag to a digest and returns it only when a valid signature
/// from `key` names exactly this repository and digest.
async fn verify(fetch: &dyn Fetch, reg: &Registry, repo: &str) -> Result<String, String> {
    let (host, name) = repo.split_once('/').ok_or("invalid repository")?;
    let mut c = Client {
        fetch,
        host: host.to_owned(),
        name: name.to_owned(),
        user: &reg.user,
        token: reg.token.as_ref().map(|t| t.as_str()),
        bearer: None,
    };
    let manifest = c.get(&format!("manifests/{}", reg.tag), MANIFESTS).await?;
    let digest = sha256(&manifest.body);
    if manifest.header("docker-content-digest").is_some_and(|d| d != digest) {
        return Err("registry: manifest digest mismatch".into());
    }
    let sig = c
        .get(&format!("manifests/{}.sig", digest.replace(':', "-")), MANIFESTS)
        .await?;
    let sig: serde_json::Value =
        serde_json::from_slice(&sig.body).map_err(|_| "registry: signature manifest is not JSON")?;
    let key = ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_ASN1, &reg.key);
    for layer in sig["layers"].as_array().into_iter().flatten() {
        let (Some(signature), Some(layer_digest)) = (
            layer["annotations"][SIG_ANNOTATION].as_str(),
            layer["digest"].as_str().filter(|d| valid_digest(d)),
        ) else {
            continue;
        };
        let payload = c.get(&format!("blobs/{layer_digest}"), "*/*").await?.body;
        if sha256(&payload) != layer_digest {
            return Err("registry: signature payload digest mismatch".into());
        }
        let Ok(signature) = B64.decode(signature) else {
            continue;
        };
        if key.verify(&payload, &signature).is_err() {
            continue;
        }
        let Ok(claim) = serde_json::from_slice::<serde_json::Value>(&payload) else {
            continue;
        };
        let critical = &claim["critical"];
        if critical["type"] == "cosign container image signature"
            && critical["identity"]["docker-reference"] == repo
            && critical["image"]["docker-manifest-digest"] == digest.as_str()
        {
            return Ok(digest);
        }
    }
    Err(format!("no valid signature for {repo}:{}", reg.tag))
}

/// Puts Castor's node images on nodes (see the top of this file).
pub struct Images {
    source: Source,
    fetch: Box<dyn Fetch>,
    /// repo@digest per image, verified once per process.
    verified: Mutex<HashMap<NodeImage, String>>,
}

impl Images {
    pub fn new(source: Source, fetch: Box<dyn Fetch>) -> Self {
        Self {
            source,
            fetch,
            verified: Mutex::default(),
        }
    }

    /// Development: every image is built on the node.
    pub fn build() -> Self {
        Self::new(Source::Build, Box::new(NoFetch))
    }

    pub fn from_env() -> Result<Self, String> {
        Ok(Self::new(Source::from_env(crate::config::var)?, Box::new(Http::new()?)))
    }

    /// `repo@digest` of a signed image (production only).
    pub async fn pinned(&self, image: NodeImage) -> Result<String, String> {
        let Source::Registry(reg) = &self.source else {
            return Err("images are built, not pulled (CASTOR_IMAGE_TAG is unset)".into());
        };
        if let Some(p) = self.verified.lock().map_err(|e| e.to_string())?.get(&image) {
            return Ok(p.clone());
        }
        let repo = format!("{}-{}", reg.repo, image.name());
        let digest = verify(self.fetch.as_ref(), reg, &repo)
            .await
            .map_err(|e| format!("{repo}:{}: signature check failed: {e}", reg.tag))?;
        let pinned = format!("{repo}@{digest}");
        self.verified
            .lock()
            .map_err(|e| e.to_string())?
            .insert(image, pinned.clone());
        Ok(pinned)
    }

    /// Puts `image` on the node as `local`: pulled by verified digest in
    /// production, built with `build` (docker build arguments) in development.
    pub async fn install(&self, node: &Node, image: NodeImage, local: &str, build: Vec<String>) -> Result<(), String> {
        let Source::Registry(reg) = &self.source else {
            let mut args = vec!["build".to_owned(), "-q".into(), "-t".into(), local.to_owned()];
            args.extend(build);
            node.docker(args, false).await?;
            return Ok(());
        };
        if node.is_gatekeeper() && reg.token.is_some() {
            return Err(format!(
                "{}: a registry token can't go through gatekeeper; publish the images or log gatekeeper's Docker in",
                reg.repo
            ));
        }
        let pinned = self.pinned(image).await?;
        let pull = vec!["pull".to_owned(), "-q".into(), pinned.clone()];
        match &reg.token {
            None => node.docker(pull, false).await?,
            Some(token) => {
                let config = DockerConfig::new(&reg.repo, &reg.user, token)?;
                node.docker_env(pull, None, vec![("DOCKER_CONFIG", config.dir())], false)
                    .await?
            }
        };
        node.docker(vec!["tag".into(), pinned, local.to_owned()], false).await?;
        Ok(())
    }
}

struct NoFetch;

#[async_trait::async_trait]
impl Fetch for NoFetch {
    async fn get(&self, _: &str, _: Headers) -> Result<Response, String> {
        Err("no registry".into())
    }
}

/// A docker config directory with only the pull credential (0700/0600),
/// removed when dropped.
struct DockerConfig(std::path::PathBuf);

impl DockerConfig {
    fn new(repo: &str, user: &str, token: &str) -> Result<Self, String> {
        use std::io::Write;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        let mut rand = [0u8; 16];
        getrandom::fill(&mut rand).map_err(|e| e.to_string())?;
        let dir = std::env::temp_dir().join(format!("castor-pull-{}", hex::encode(rand)));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .map_err(|e| format!("{}: {e}", dir.display()))?;
        let config = DockerConfig(dir);
        let host = repo.split('/').next().unwrap_or_default();
        let auth = Zeroizing::new(B64.encode(format!("{user}:{token}")));
        let json = Zeroizing::new(serde_json::json!({ "auths": { host: { "auth": auth.as_str() } } }).to_string());
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(config.0.join("config.json"))
            .and_then(|mut f| f.write_all(json.as_bytes()))
            .map_err(|e| format!("docker config: {e}"))?;
        Ok(config)
    }

    fn dir(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }
}

impl Drop for DockerConfig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use ring::rand::SystemRandom;
    use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair};

    const REPO: &str = "ghcr.io/davidsteg/castorcloud-cryptd";

    fn keypair() -> EcdsaKeyPair {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
        EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng).unwrap()
    }

    /// cosign.pub for a key pair: SPKI with id-ecPublicKey and prime256v1.
    fn pem(k: &EcdsaKeyPair) -> String {
        let mut der = hex::decode("3059301306072a8648ce3d020106082a8648ce3d030107034200").unwrap();
        der.extend_from_slice(k.public_key().as_ref());
        format!(
            "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
            B64.encode(der)
        )
    }

    #[derive(Default)]
    struct Opts {
        repo: Option<&'static str>,
        other_key: bool,
        tamper: bool,
        realm: Option<&'static str>,
        bad_header: bool,
    }

    /// A fake GHCR: one tag, its signature manifest and payload, behind a token challenge.
    struct Fake {
        manifest: Vec<u8>,
        sig_manifest: Vec<u8>,
        payload: Vec<u8>,
        payload_digest: String,
        opts: Opts,
        seen: Mutex<Vec<(String, Headers)>>,
    }

    impl Fake {
        fn new(signer: &EcdsaKeyPair, opts: Opts) -> Self {
            let manifest = br#"{"schemaVersion":2,"manifests":[]}"#.to_vec();
            let digest = sha256(&manifest);
            let payload = serde_json::json!({
                "critical": {
                    "type": "cosign container image signature",
                    "identity": { "docker-reference": opts.repo.unwrap_or(REPO) },
                    "image": { "docker-manifest-digest": digest }
                },
                "optional": null
            })
            .to_string()
            .into_bytes();
            let other = keypair();
            let k = if opts.other_key { &other } else { signer };
            let signature = B64.encode(k.sign(&SystemRandom::new(), &payload).unwrap());
            let payload_digest = sha256(&payload);
            let sig_manifest = serde_json::json!({
                "layers": [{ "digest": payload_digest, "annotations": { SIG_ANNOTATION: signature } }]
            })
            .to_string()
            .into_bytes();
            Fake {
                manifest,
                sig_manifest,
                payload,
                payload_digest,
                opts,
                seen: Mutex::default(),
            }
        }

        fn digest(&self) -> String {
            sha256(&self.manifest)
        }
    }

    fn ok(body: Vec<u8>, headers: Vec<(String, String)>) -> Result<Response, String> {
        Ok(Response {
            status: 200,
            headers,
            body,
        })
    }

    #[async_trait::async_trait]
    impl Fetch for Fake {
        async fn get(&self, url: &str, headers: Headers) -> Result<Response, String> {
            self.seen.lock().unwrap().push((url.to_owned(), headers.clone()));
            if url.starts_with("https://ghcr.io/token") {
                return ok(br#"{"token":"T"}"#.to_vec(), vec![]);
            }
            if !headers.contains(&("authorization", "Bearer T".into())) {
                let realm = self.opts.realm.unwrap_or("https://ghcr.io/token");
                return Ok(Response {
                    status: 401,
                    headers: vec![(
                        "www-authenticate".into(),
                        format!(
                            r#"Bearer realm="{realm}",service="ghcr.io",scope="repository:davidsteg/castorcloud-cryptd:push""#
                        ),
                    )],
                    body: vec![],
                });
            }
            let digest = self.digest();
            let path = url
                .strip_prefix("https://ghcr.io/v2/davidsteg/castorcloud-cryptd/")
                .unwrap_or_default();
            if path == "manifests/v1" {
                let claimed = if self.opts.bad_header { sha256(b"x") } else { digest };
                return ok(self.manifest.clone(), vec![("docker-content-digest".into(), claimed)]);
            }
            if path == format!("manifests/{}.sig", digest.replace(':', "-")) {
                return ok(self.sig_manifest.clone(), vec![]);
            }
            if path == format!("blobs/{}", self.payload_digest) {
                let mut p = self.payload.clone();
                if self.opts.tamper {
                    p.push(b' ');
                }
                return ok(p, vec![]);
            }
            Ok(Response {
                status: 404,
                headers: vec![],
                body: vec![],
            })
        }
    }

    struct Shared(std::sync::Arc<Fake>);

    #[async_trait::async_trait]
    impl Fetch for Shared {
        async fn get(&self, url: &str, headers: Headers) -> Result<Response, String> {
            self.0.get(url, headers).await
        }
    }

    fn registry(k: &EcdsaKeyPair) -> Registry {
        Registry {
            repo: "ghcr.io/davidsteg/castorcloud".into(),
            tag: "v1".into(),
            key: public_key(pem(k).as_bytes()).unwrap(),
            user: "castor".into(),
            token: Some(Zeroizing::new("read-only".into())),
        }
    }

    #[tokio::test]
    async fn a_signed_tag_resolves_to_its_digest() {
        let k = keypair();
        let fake = Fake::new(&k, Opts::default());
        assert_eq!(verify(&fake, &registry(&k), REPO).await.unwrap(), fake.digest());
        // The credential goes only to the token endpoint, and only for pull.
        let seen = fake.seen.lock().unwrap();
        let basic: Vec<&String> = seen
            .iter()
            .filter(|(_, h)| h.iter().any(|(k, v)| *k == "authorization" && v.starts_with("Basic ")))
            .map(|(u, _)| u)
            .collect();
        assert_eq!(basic.len(), 1);
        assert!(basic[0].starts_with(
            "https://ghcr.io/token?service=ghcr.io&scope=repository%3Adavidsteg%2Fcastorcloud-cryptd%3Apull"
        ));
        assert!(!basic[0].contains("push"));
    }

    #[tokio::test]
    async fn wrong_key_other_repository_or_altered_payload_is_refused() {
        let k = keypair();
        let reg = registry(&k);
        let refused = |opts| {
            let fake = Fake::new(&k, opts);
            let reg = &reg;
            async move { verify(&fake, reg, REPO).await.unwrap_err() }
        };
        assert!(
            refused(Opts {
                other_key: true,
                ..Default::default()
            })
            .await
            .contains("no valid signature")
        );
        assert!(
            refused(Opts {
                repo: Some("ghcr.io/evil/castorcloud-cryptd"),
                ..Default::default()
            })
            .await
            .contains("no valid signature")
        );
        assert!(
            refused(Opts {
                tamper: true,
                ..Default::default()
            })
            .await
            .contains("payload digest mismatch")
        );
        assert!(
            refused(Opts {
                bad_header: true,
                ..Default::default()
            })
            .await
            .contains("manifest digest mismatch")
        );
    }

    #[tokio::test]
    async fn credentials_never_go_to_another_host_or_over_plain_http() {
        let k = keypair();
        let reg = registry(&k);
        let fake = Fake::new(
            &k,
            Opts {
                realm: Some("https://evil.example/token"),
                ..Default::default()
            },
        );
        assert!(verify(&fake, &reg, REPO).await.unwrap_err().contains("another host"));
        let fake = Fake::new(
            &k,
            Opts {
                realm: Some("http://ghcr.io/token"),
                ..Default::default()
            },
        );
        assert!(verify(&fake, &reg, REPO).await.unwrap_err().contains("insecure"));
        for (_, headers) in fake.seen.lock().unwrap().iter() {
            assert!(!headers.iter().any(|(k, _)| *k == "authorization"));
        }
    }

    #[test]
    fn a_registry_tag_needs_a_cosign_key_and_sane_names() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| (*v).to_owned())
        };
        assert!(matches!(Source::from_env(env(&[])), Ok(Source::Build)));
        assert!(matches!(
            Source::from_env(env(&[("CASTOR_IMAGE_TAG", " ")])),
            Ok(Source::Build)
        ));
        let err = |pairs| Source::from_env(env(pairs)).err().unwrap();
        assert!(err(&[("CASTOR_IMAGE_TAG", "abc")]).contains("CASTOR_COSIGN_PUBKEY"));
        assert!(err(&[("CASTOR_IMAGE_TAG", "a b"), ("CASTOR_COSIGN_PUBKEY", "/k")]).contains("tag"));
        assert!(
            err(&[
                ("CASTOR_IMAGE_TAG", "abc"),
                ("CASTOR_COSIGN_PUBKEY", "/k"),
                ("CASTOR_IMAGE_REPO", "ghcr.io/x;rm")
            ])
            .contains("repository")
        );
        assert!(err(&[("CASTOR_IMAGE_TAG", "abc"), ("CASTOR_COSIGN_PUBKEY", "/nonexistent")]).contains("/nonexistent"));
        for bad in [
            "",
            "ghcr.io",
            "GHCR.io/x",
            "ghcr.io/X",
            "ghcr.io//x",
            "ghcr.io:99999999/x",
            "ghcr.io/x y",
        ] {
            assert!(!valid_repo(bad), "{bad}");
        }
        assert!(valid_repo("localhost:5000/castor"));
        assert!(valid_tag(&"a".repeat(128)) && !valid_tag(&"a".repeat(129)) && !valid_tag(".a"));
    }

    #[test]
    fn only_p256_public_keys_are_accepted() {
        let k = keypair();
        assert_eq!(public_key(pem(&k).as_bytes()).unwrap(), k.public_key().as_ref());
        assert!(public_key(b"nonsense").is_err());
        let private = pem(&k).replace("PUBLIC KEY", "PRIVATE KEY");
        assert!(public_key(private.as_bytes()).unwrap_err().contains("PUBLIC KEY"));
        // An Ed25519 key.
        let ed = "-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEAGb9ECWmEzf6FQbrBZ9w7lshQhqowtrbLDFw4rXAxZuE=\n-----END PUBLIC KEY-----\n";
        assert!(public_key(ed.as_bytes()).unwrap_err().contains("P-256"));
    }

    #[test]
    fn the_pull_credential_is_private_and_removed() {
        use std::os::unix::fs::PermissionsExt;
        let c = DockerConfig::new("ghcr.io/davidsteg/castorcloud", "castor", "tok").unwrap();
        let file = std::path::Path::new(&c.dir()).join("config.json");
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(c.dir()).unwrap().permissions().mode() & 0o777, 0o700);
        let json: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(json["auths"]["ghcr.io"]["auth"], B64.encode("castor:tok"));
        let dir = c.dir();
        drop(c);
        assert!(!std::path::Path::new(&dir).exists());
    }

    /// Records docker commands; DOCKER_CONFIG's file is read while it exists.
    #[derive(Default)]
    struct Docker {
        calls: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl crate::exec::Exec for Docker {
        async fn run(&self, cmd: crate::exec::Cmd) -> Result<String, String> {
            let mut line = cmd.args.join(" ");
            for (k, v) in &cmd.env {
                if *k == "DOCKER_CONFIG" {
                    let json = std::fs::read_to_string(std::path::Path::new(v).join("config.json")).unwrap();
                    line = format!("[{json}] {line}");
                }
            }
            self.calls.lock().unwrap().push(line);
            Ok(String::new())
        }
    }

    fn node(exec: std::sync::Arc<Docker>) -> Node {
        let row = crate::node::NodeRow {
            id: "n1".into(),
            name: "nas".into(),
            kind: "MAC".into(),
            role: "TENANTS".into(),
            ssh_host: "10.0.0.5".into(),
            ssh_user: "deploy".into(),
            zfs_pool: Some("tank".into()),
            ..Default::default()
        };
        Node::new(&row, std::path::Path::new("/repo"), exec).unwrap()
    }

    #[tokio::test]
    async fn nodes_pull_the_verified_digest_once_checked() {
        let k = keypair();
        let fake = std::sync::Arc::new(Fake::new(&k, Opts::default()));
        let digest = fake.digest();
        let images = Images::new(Source::Registry(registry(&k)), Box::new(Shared(fake.clone())));
        let docker = std::sync::Arc::new(Docker::default());
        let n = node(docker.clone());
        images
            .install(&n, NodeImage::Cryptd, "pc-cryptd:1", vec!["ctx".into()])
            .await
            .unwrap();
        images
            .install(&n, NodeImage::Cryptd, "pc-cryptd:1", vec!["ctx".into()])
            .await
            .unwrap();
        let pinned = format!("{REPO}@{digest}");
        let auth = format!(
            r#"[{{"auths":{{"ghcr.io":{{"auth":"{}"}}}}}}]"#,
            B64.encode("castor:read-only")
        );
        let pull = format!("{auth} pull -q {pinned}");
        let tag = format!("tag {pinned} pc-cryptd:1");
        assert_eq!(
            *docker.calls.lock().unwrap(),
            vec![pull.clone(), tag.clone(), pull, tag]
        );
        // Verified once per process, then cached.
        let fetched = fake.seen.lock().unwrap().len();
        assert_eq!(images.pinned(NodeImage::Cryptd).await.unwrap(), pinned);
        assert_eq!(fake.seen.lock().unwrap().len(), fetched);
        // Challenge, token, manifest, signature manifest, payload.
        assert_eq!(fetched, 5);
        // Development: built from source.
        let docker = std::sync::Arc::new(Docker::default());
        Images::build()
            .install(
                &node(docker.clone()),
                NodeImage::Egress,
                "castor.local/egress:2",
                vec!["ctx".into()],
            )
            .await
            .unwrap();
        assert_eq!(
            *docker.calls.lock().unwrap(),
            vec!["build -q -t castor.local/egress:2 ctx"]
        );
    }

    #[tokio::test]
    async fn an_unsigned_image_is_never_pulled() {
        let k = keypair();
        let images = Images::new(
            Source::Registry(registry(&k)),
            Box::new(Fake::new(
                &k,
                Opts {
                    other_key: true,
                    ..Default::default()
                },
            )),
        );
        let docker = std::sync::Arc::new(Docker::default());
        let e = images
            .install(&node(docker.clone()), NodeImage::Cryptd, "pc-cryptd:1", vec![])
            .await
            .unwrap_err();
        assert!(e.contains("signature check failed"), "{e}");
        assert!(docker.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn challenges_parse() {
        let p = auth_params(r#"realm="https://ghcr.io/token",service="ghcr.io", scope="a:b:pull""#);
        assert_eq!(p["realm"], "https://ghcr.io/token");
        assert_eq!(p["service"], "ghcr.io");
        assert_eq!(p["scope"], "a:b:pull");
    }
}
