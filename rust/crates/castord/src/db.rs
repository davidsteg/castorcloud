//! Rows Prisma would write with client-side defaults (ids, updatedAt).

use sqlx::PgPool;
use sqlx::postgres::PgConnectOptions;

/// A cuid-shaped id ("c" + 24 base-36 characters): Prisma's own ids are made
/// by the client, so the database has no default to fall back on.
pub fn new_id() -> Result<String, String> {
    const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut bytes = [0u8; 24];
    getrandom::fill(&mut bytes).map_err(|e| format!("random id: {e}"))?;
    let mut id = String::with_capacity(25);
    id.push('c');
    // 252 = 7 * 36: no modulo bias.
    for mut b in bytes {
        while b >= 252 {
            let mut one = [0u8; 1];
            getrandom::fill(&mut one).map_err(|e| format!("random id: {e}"))?;
            b = one[0];
        }
        id.push(char::from(ALPHABET[usize::from(b % 36)]));
    }
    Ok(id)
}

/// The audit log, as `audit()` in packages/core/src/jobs.ts.
pub async fn audit(
    db: &PgPool,
    actor: &str,
    action: &str,
    tenant_id: Option<&str>,
    details: Option<serde_json::Value>,
) -> Result<(), String> {
    sqlx::query(
        r#"INSERT INTO "AuditLog" (id, actor, action, "tenantId", details, "createdAt")
           VALUES ($1, $2, $3, $4, $5::jsonb, now() AT TIME ZONE 'UTC')"#,
    )
    .bind(new_id()?)
    .bind(actor)
    .bind(action)
    .bind(tenant_id)
    .bind(details.map(|d| d.to_string()))
    .execute(db)
    .await
    .map_err(|e| format!("audit {action}: {e}"))?;
    Ok(())
}

/// Prisma's URLs carry `?schema=...`, which Postgres doesn't know: it
/// becomes the search_path here.
pub fn connect_options(url: &str) -> Result<PgConnectOptions, String> {
    let (base, query) = url.split_once('?').unwrap_or((url, ""));
    let mut schema = None;
    let rest: Vec<&str> = query
        .split('&')
        .filter(|p| !p.is_empty())
        .filter(|p| match p.strip_prefix("schema=") {
            Some(s) => {
                schema = Some(s.to_owned());
                false
            }
            None => true,
        })
        .collect();
    let url = if rest.is_empty() {
        base.to_owned()
    } else {
        format!("{base}?{}", rest.join("&"))
    };
    let opts: PgConnectOptions = url.parse().map_err(|e| format!("DATABASE_URL: {e}"))?;
    Ok(match schema {
        Some(s) if s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') => opts.options([("search_path", s)]),
        Some(_) => return Err("DATABASE_URL: bad schema".into()),
        None => opts,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn ids() {
        let a = new_id().unwrap();
        assert_eq!(a.len(), 25);
        assert!(a.starts_with('c') && a.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()));
        assert_ne!(a, new_id().unwrap());
    }

    #[test]
    fn prisma_schema_param_becomes_search_path() {
        assert!(connect_options("postgresql://pc:pw@localhost:5432/portal?schema=public").is_ok());
        assert!(connect_options("postgresql://pc:pw@localhost:5432/portal?schema=public&sslmode=require").is_ok());
        assert!(connect_options("postgresql://pc:pw@localhost:5432/portal").is_ok());
        assert!(connect_options("postgresql://pc:pw@localhost:5432/portal?schema=a;drop").is_err());
    }
}
