//! Who is asking. Every household member has an own Hermes profile whose MCP
//! config carries that member's bearer token; the token decides whose photos,
//! documents, places and flights the tools may read. The file is rendered by
//! the provisioner (castord's assistant.rs) and holds only token hashes.

use serde::Deserialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::limits::js_space;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Member {
    /// Member id in the control plane (also the Hermes profile name).
    pub id: String,
    pub email: String,
    /// Authentik "sub": how OIDC-only services (AirTrail) know the user.
    pub sub: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub name: Option<String>,
    pub token_sha256: String,
}

#[derive(Deserialize)]
struct File {
    #[serde(default)]
    members: Option<Vec<Member>>,
}

#[cfg(test)]
pub fn sha256_hex(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

/// Parses members.json. A file that does not have the expected shape is an
/// error, so nobody is let in on a half-understood file.
pub fn parse_members(bytes: &[u8]) -> Result<Vec<Member>, String> {
    let text = String::from_utf8_lossy(bytes);
    let file: File = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    Ok(file.members.unwrap_or_default())
}

/// Re-read on every request: the provisioner rewrites it when members change.
pub async fn load_members(path: &str) -> Vec<Member> {
    match tokio::fs::read(path)
        .await
        .map_err(|e| e.to_string())
        .and_then(|b| parse_members(&b))
    {
        Ok(m) => m,
        Err(e) => {
            eprintln!("cannot read {path}: {e}");
            Vec::new()
        }
    }
}

/// The token of `Authorization: Bearer <token>` (scheme case-insensitive).
/// The header is read as Latin-1, as Node.js does.
fn bearer_token(authorization: &[u8]) -> Option<String> {
    let value: String = authorization.iter().map(|&b| char::from(b)).collect();
    let scheme = value.get(..6)?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let rest = &value[6..];
    let token = rest.trim_start_matches(js_space);
    if token.len() == rest.len() || token.is_empty() || token.chars().any(js_space) {
        return None;
    }
    Some(token.to_owned())
}

/// The member the bearer token belongs to. The SHA-256 of the token is
/// compared in constant time with every member's hash.
pub fn member_for_token(members: &[Member], authorization: Option<&[u8]>) -> Option<Member> {
    let token = bearer_token(authorization?)?;
    let hash = Sha256::digest(token.as_bytes());
    let mut found = None;
    for m in members {
        let Ok(expected) = hex::decode(&m.token_sha256) else {
            continue;
        };
        if expected.len() == hash.len()
            && bool::from(expected.ct_eq(hash.as_slice()))
            && found.is_none()
        {
            found = Some(m.clone());
        }
    }
    found
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
pub mod tests {
    use super::*;

    pub fn member(id: &str, token: &str) -> Member {
        Member {
            id: id.into(),
            email: format!("{id}@x.ch"),
            sub: format!("sub-{id}"),
            name: None,
            token_sha256: sha256_hex(token),
        }
    }

    fn who(members: &[Member], auth: Option<&str>) -> Option<String> {
        member_for_token(members, auth.map(str::as_bytes)).map(|m| m.id)
    }

    #[test]
    fn a_token_only_identifies_its_own_member() {
        let members = [member("anna", "token-a"), member("ben", "token-b")];
        assert_eq!(
            who(&members, Some("Bearer token-a")).as_deref(),
            Some("anna")
        );
        assert_eq!(
            who(&members, Some("Bearer token-b")).as_deref(),
            Some("ben")
        );
        assert_eq!(who(&members, Some("Bearer token-c")), None);
        assert_eq!(who(&members, Some("token-a")), None);
        assert_eq!(who(&members, None), None);
    }

    #[test]
    fn the_header_is_parsed_strictly() {
        let members = [member("anna", "token-a")];
        assert_eq!(
            who(&members, Some("bearer token-a")).as_deref(),
            Some("anna")
        );
        assert_eq!(
            who(&members, Some("BEARER \t token-a")).as_deref(),
            Some("anna")
        );
        assert_eq!(who(&members, Some("Bearertoken-a")), None);
        assert_eq!(who(&members, Some("Bearer token-a extra")), None);
        assert_eq!(who(&members, Some("Bearer token-a ")), None);
        assert_eq!(who(&members, Some("Bearer ")), None);
        assert_eq!(who(&members, Some("Basic token-a")), None);
        assert_eq!(who(&[], Some("Bearer token-a")), None);
        // Hashes are compared as bytes, whatever the hex case.
        let upper = Member {
            token_sha256: sha256_hex("token-a").to_uppercase(),
            ..member("anna", "x")
        };
        assert_eq!(
            who(&[upper], Some("Bearer token-a")).as_deref(),
            Some("anna")
        );
        let broken = Member {
            token_sha256: "zz".into(),
            ..member("x", "x")
        };
        assert_eq!(who(&[broken], Some("Bearer token-a")), None);
    }

    #[test]
    fn the_members_file_is_read_as_rendered() {
        let file = br#"{"members":[{"id":"cm1","email":"a@x.ch","sub":"s1","name":"Anna","tokenSha256":"00"},
            {"id":"cm2","email":"b@x.ch","sub":"s2","tokenSha256":"11"}]}"#;
        let m = parse_members(file).unwrap();
        assert_eq!(
            (m.len(), m[0].name.as_deref(), m[1].name.as_deref()),
            (2, Some("Anna"), None)
        );
        assert!(parse_members(br#"{}"#).unwrap().is_empty());
        assert!(parse_members(br#"{"members":null}"#).unwrap().is_empty());
        assert!(parse_members(br#"{"members":[{"id":"x"}]}"#).is_err());
        assert!(parse_members(b"nope").is_err());
    }
}
