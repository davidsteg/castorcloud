//! Direct database access, used only to look up a member's account in a
//! service and to issue that member a read-only API key. The services have no
//! admin API for either: accounts appear on first SSO login, and API keys can
//! only be created by the user themselves.
//!
//! Values reach SQL only as bind parameters. Ids are compared as text and
//! taken from the service's own table, so the queries work whatever type the
//! service gives its ids (Immich: uuid, AirTrail: text).

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tokio::sync::OnceCell;

use crate::limits::ToolError;

/// Name of every key we issue, so re-issuing replaces the previous one
/// (castord's revoke.rs keeps keys of this name when it signs a person out).
pub const KEY_NAME: &str = "Castor assistant (read-only)";

/// 30 random bytes, base64url: 40 characters.
pub fn new_api_key() -> Result<String, ToolError> {
    let mut b = [0u8; 30];
    getrandom::fill(&mut b).map_err(|e| ToolError::internal(format!("random key: {e}")))?;
    Ok(URL_SAFE_NO_PAD.encode(b))
}

pub fn sha256(value: &str) -> [u8; 32] {
    Sha256::digest(value.as_bytes()).into()
}

pub fn sql_error(e: sqlx::Error) -> ToolError {
    ToolError::internal(format!("database: {e}"))
}

/// A small pool, connected on first use.
pub struct Db {
    url: String,
    pool: OnceCell<PgPool>,
}

impl Db {
    pub fn new(url: String) -> Self {
        Self {
            url,
            pool: OnceCell::new(),
        }
    }

    pub async fn pool(&self) -> Result<&PgPool, ToolError> {
        self.pool
            .get_or_try_init(|| async {
                PgPoolOptions::new()
                    .max_connections(2)
                    .min_connections(0)
                    .idle_timeout(Duration::from_secs(30))
                    .connect_lazy(&self.url)
                    .map_err(sql_error)
            })
            .await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_random_base64url() {
        let (a, b) = (new_api_key().unwrap(), new_api_key().unwrap());
        assert_ne!(a, b);
        assert_eq!(a.len(), 40);
        assert!(
            a.bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        );
        assert_eq!(
            hex::encode(sha256("abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
