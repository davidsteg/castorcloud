//! Rows Prisma would write with client-side defaults (ids, updatedAt).

use sqlx::PgPool;

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
}
