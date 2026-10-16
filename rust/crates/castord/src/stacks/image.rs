//! Image references a custom stack may use (packages/stacks/src/image.ts):
//! public images from a short list of registries, no local or LAN registry,
//! no credentials. At deploy the image is pulled and pinned to its digest.

use serde::Serialize;

pub const ALLOWED_REGISTRIES: [&str; 8] = [
    "docker.io",
    "ghcr.io",
    "quay.io",
    "lscr.io",
    "registry.gitlab.com",
    "codeberg.org",
    "gcr.io",
    "mcr.microsoft.com",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImageRef {
    pub registry: String,
    pub repository: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    pub canonical: String,
}

fn alnum(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit()
}

/// [a-z0-9]+ joined by ".", "_", "__" or one or more "-".
fn component(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    loop {
        let start = i;
        while i < b.len() && alnum(b[i]) {
            i += 1;
        }
        if i == start {
            return false;
        }
        if i == b.len() {
            return true;
        }
        let sep = i;
        while i < b.len() && !alnum(b[i]) {
            i += 1;
        }
        let ok = match &s[sep..i] {
            "." | "_" | "__" => true,
            d => d.bytes().all(|c| c == b'-'),
        };
        if !ok {
            return false;
        }
    }
}

fn repository(s: &str) -> bool {
    let parts: Vec<&str> = s.split('/').collect();
    parts.len() <= 5 && parts.iter().all(|p| component(p))
}

fn tag(s: &str) -> bool {
    let b = s.as_bytes();
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    (1..=128).contains(&b.len()) && word(b[0]) && b.iter().all(|c| word(*c) || *c == b'.' || *c == b'-')
}

pub fn digest(s: &str) -> bool {
    s.strip_prefix("sha256:")
        .is_some_and(|h| h.len() == 64 && h.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)))
}

/// Ends in @sha256:<64 hex> (what render accepts).
pub fn is_pinned(s: &str) -> bool {
    s.rfind('@').is_some_and(|at| digest(&s[at + 1..]))
}

pub fn parse(input: &str) -> Result<ImageRef, String> {
    let r = super::js::trim(input);
    if r.is_empty() || r.encode_utf16().count() > 255 {
        return Err("invalid image reference".into());
    }
    let mut rest = r;
    let mut dig = None;
    if let Some(at) = rest.find('@') {
        let d = &rest[at + 1..];
        rest = &rest[..at];
        if !digest(d) {
            return Err("invalid digest (expected sha256:<64 hex>)".into());
        }
        dig = Some(d.to_owned());
    }
    // The first component is a registry when it has a dot or a colon (or is localhost).
    let mut registry = "docker.io";
    if let Some(slash) = rest.find('/').filter(|i| *i > 0) {
        let first = &rest[..slash];
        if first.contains('.') || first.contains(':') || first == "localhost" {
            registry = first;
            rest = &rest[slash + 1..];
        }
    }
    if registry == "index.docker.io" || registry == "registry-1.docker.io" {
        registry = "docker.io";
    }
    if !ALLOWED_REGISTRIES.contains(&registry) {
        return Err(format!(
            "registry {registry} is not allowed (allowed: {})",
            ALLOWED_REGISTRIES.join(", ")
        ));
    }
    let mut t = None;
    if let Some(colon) = rest.rfind(':') {
        let v = &rest[colon + 1..];
        rest = &rest[..colon];
        if !tag(v) {
            return Err("invalid tag".into());
        }
        t = Some(v.to_owned());
    }
    if !repository(rest) {
        return Err("invalid repository name".into());
    }
    let repository = if registry == "docker.io" && !rest.contains('/') {
        format!("library/{rest}")
    } else {
        rest.to_owned()
    };
    if t.is_none() && dig.is_none() {
        t = Some("latest".into());
    }
    let canonical = format!(
        "{registry}/{repository}{}{}",
        t.as_ref().map(|t| format!(":{t}")).unwrap_or_default(),
        dig.as_ref().map(|d| format!("@{d}")).unwrap_or_default()
    );
    Ok(ImageRef {
        registry: registry.to_owned(),
        repository,
        tag: t,
        digest: dig,
        canonical,
    })
}

/// The reference pinned to `digest` (the tag stays for reading; Docker uses the digest).
pub fn pinned(r: &ImageRef, d: &str) -> Result<String, String> {
    if !digest(d) {
        return Err(format!("invalid digest {d}"));
    }
    let t = r.tag.as_ref().map(|t| format!(":{t}")).unwrap_or_default();
    Ok(format!("{}/{}{t}@{d}", r.registry, r.repository))
}

/// This repository's digest from `docker image inspect`'s RepoDigests.
pub fn digest_for(r: &ImageRef, repo_digests: &[String]) -> Option<String> {
    let mut names = vec![format!("{}/{}", r.registry, r.repository)];
    if r.registry == "docker.io" {
        names.push(r.repository.clone());
        names.push(
            r.repository
                .strip_prefix("library/")
                .unwrap_or(&r.repository)
                .to_owned(),
        );
    }
    repo_digests.iter().find_map(|d| {
        let at = d.rfind('@').filter(|at| *at > 0)?;
        (names.iter().any(|n| *n == d[..at]) && digest(&d[at + 1..])).then(|| d[at + 1..].to_owned())
    })
}
