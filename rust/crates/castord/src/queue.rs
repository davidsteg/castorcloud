//! The tenant job queue in Postgres: the same contract and the same SQL as
//! `packages/core/src/jobs.ts`, so this worker and the TypeScript one can
//! share the queue, each taking only the job kinds it runs.
//!
//!   claim     QUEUED, runAfter passed, a kind it runs, no older QUEUED job of
//!             the same tenant, no RUNNING one (a unique index enforces that
//!             even between workers). Oldest first. Sets RUNNING, attempts+1,
//!             and a lease (lockedBy, lockedUntil).
//!   renew     every RENEW while it works.
//!   succeed   SUCCEEDED, finishedAt, lease cleared.
//!   fail      QUEUED again with exponential backoff while attempts remain,
//!             else FAILED with finishedAt.
//!   recover   a RUNNING job whose lease expired (its worker died) counts as
//!             a failed attempt.
//!
//! Timestamps are UTC without a zone (Prisma's DateTime); every comparison
//! uses `now() AT TIME ZONE 'UTC'`, so the session's time zone never matters.

use std::time::Duration;

use sqlx::PgPool;

/// A job's lease. As long as the TypeScript worker's, which a lapse would
/// hand the job to a second worker while the first still runs it.
pub const LEASE: Duration = Duration::from_secs(30 * 60);
pub const RENEW: Duration = Duration::from_secs(60);
/// First retry after this, doubling per attempt.
const BACKOFF_MS: f64 = 15_000.0;

/// Postgres' enum "JobKind", as the TypeScript side names them.
pub const ALL_KINDS: [&str; 8] = [
    "PROVISION",
    "UPDATE",
    "SUSPEND",
    "RESUME",
    "DELETE",
    "REVOKE_ACCESS",
    "RESTORE",
    "STACKS",
];

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Job {
    pub id: String,
    #[sqlx(rename = "tenantId")]
    pub tenant_id: String,
    pub kind: String,
    pub attempts: i32,
    #[sqlx(rename = "maxAttempts")]
    pub max_attempts: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Recovered {
    #[sqlx(flatten)]
    pub job: Job,
    /// It was the last attempt: the job is FAILED.
    pub failed: bool,
}

fn lease_interval() -> String {
    format!("{} milliseconds", LEASE.as_millis())
}

/// Takes the next job this worker may run.
pub async fn claim(db: &PgPool, worker: &str, kinds: &[&str]) -> sqlx::Result<Option<Job>> {
    let r = sqlx::query_as::<_, Job>(
        r#"
        UPDATE "Job" SET status = 'RUNNING', attempts = attempts + 1, error = NULL,
          "lockedBy" = $1, "lockedUntil" = (now() AT TIME ZONE 'UTC') + $2::interval
        WHERE id = (
          SELECT j.id FROM "Job" j
          WHERE j.status = 'QUEUED' AND j."runAfter" <= (now() AT TIME ZONE 'UTC')
            AND j.kind::text = ANY($3)
            AND NOT EXISTS (SELECT 1 FROM "Job" r WHERE r."tenantId" = j."tenantId" AND r.status = 'RUNNING')
            AND NOT EXISTS (SELECT 1 FROM "Job" o WHERE o."tenantId" = j."tenantId" AND o.status = 'QUEUED'
                              AND (o."createdAt", o.id) < (j."createdAt", j.id))
          ORDER BY j."createdAt", j.id
          LIMIT 1
          FOR UPDATE SKIP LOCKED
        )
        RETURNING id, "tenantId", kind::text AS kind, attempts, "maxAttempts""#,
    )
    .bind(worker)
    .bind(lease_interval())
    .bind(kinds)
    .fetch_optional(db)
    .await;
    match r {
        // Another worker started a job of the same tenant at the same moment.
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("23505") => Ok(None),
        other => other,
    }
}

/// Extends the lease; false when the job is no longer this worker's.
pub async fn renew(db: &PgPool, job_id: &str, worker: &str) -> sqlx::Result<bool> {
    let r = sqlx::query(
        r#"UPDATE "Job" SET "lockedUntil" = (now() AT TIME ZONE 'UTC') + $3::interval
           WHERE id = $1 AND status = 'RUNNING' AND "lockedBy" = $2"#,
    )
    .bind(job_id)
    .bind(worker)
    .bind(lease_interval())
    .execute(db)
    .await?;
    Ok(r.rows_affected() == 1)
}

pub async fn succeed(db: &PgPool, job_id: &str, worker: &str) -> sqlx::Result<()> {
    sqlx::query(
        r#"UPDATE "Job" SET status = 'SUCCEEDED', "finishedAt" = (now() AT TIME ZONE 'UTC'),
             "lockedBy" = NULL, "lockedUntil" = NULL
           WHERE id = $1 AND "lockedBy" = $2"#,
    )
    .bind(job_id)
    .bind(worker)
    .execute(db)
    .await?;
    Ok(())
}

/// Returns whether this was the last attempt (the job is now FAILED).
pub async fn fail(db: &PgPool, job_id: &str, worker: &str, error: &str) -> sqlx::Result<bool> {
    let error: String = error.chars().take(4000).collect();
    let status: Option<String> = sqlx::query_scalar(
        r#"UPDATE "Job" SET
             status = CASE WHEN attempts >= "maxAttempts" THEN 'FAILED'::"JobStatus" ELSE 'QUEUED'::"JobStatus" END,
             "finishedAt" = CASE WHEN attempts >= "maxAttempts" THEN (now() AT TIME ZONE 'UTC') END,
             "runAfter" = (now() AT TIME ZONE 'UTC') + ($4::float8 * power(2, greatest(attempts - 1, 0))) * interval '1 millisecond',
             error = $3, "lockedBy" = NULL, "lockedUntil" = NULL
           WHERE id = $1 AND "lockedBy" = $2
           RETURNING status::text"#,
    )
    .bind(job_id)
    .bind(worker)
    .bind(error)
    .bind(BACKOFF_MS)
    .fetch_optional(db)
    .await?;
    Ok(status.as_deref() == Some("FAILED"))
}

/// Jobs whose worker died: that attempt failed.
pub async fn recover_expired(db: &PgPool) -> sqlx::Result<Vec<Recovered>> {
    sqlx::query_as::<_, Recovered>(
        r#"UPDATE "Job" SET
             status = CASE WHEN attempts >= "maxAttempts" THEN 'FAILED'::"JobStatus" ELSE 'QUEUED'::"JobStatus" END,
             "finishedAt" = CASE WHEN attempts >= "maxAttempts" THEN (now() AT TIME ZONE 'UTC') END,
             "runAfter" = (now() AT TIME ZONE 'UTC'),
             error = 'the worker running it stopped (lease expired)', "lockedBy" = NULL, "lockedUntil" = NULL
           WHERE status = 'RUNNING' AND ("lockedUntil" IS NULL OR "lockedUntil" < (now() AT TIME ZONE 'UTC'))
           RETURNING id, "tenantId", kind::text AS kind, attempts, "maxAttempts", status = 'FAILED' AS failed"#,
    )
    .fetch_all(db)
    .await
}

/// The worker is alive and runs these kinds.
pub async fn heartbeat(db: &PgPool, worker: &str, kinds: &[&str]) -> sqlx::Result<()> {
    sqlx::query(
        r#"INSERT INTO "WorkerHeartbeat" (id, kinds, "seenAt")
           VALUES ($1, $2::text[]::"JobKind"[], now() AT TIME ZONE 'UTC')
           ON CONFLICT (id) DO UPDATE SET kinds = EXCLUDED.kinds, "seenAt" = EXCLUDED."seenAt""#,
    )
    .bind(worker)
    .bind(kinds)
    .execute(db)
    .await?;
    Ok(())
}

/// Queues a job for the tenant (enqueueTenantJob in jobs.ts).
pub async fn enqueue(db: &PgPool, tenant_id: &str, kind: &str) -> Result<(), String> {
    if !ALL_KINDS.contains(&kind) {
        return Err(format!("unknown job kind {kind}"));
    }
    sqlx::query(
        r#"INSERT INTO "Job" (id, "tenantId", kind, "createdAt", "runAfter")
           VALUES ($1, $2, $3::"JobKind", now() AT TIME ZONE 'UTC', now() AT TIME ZONE 'UTC')"#,
    )
    .bind(crate::db::new_id()?)
    .bind(tenant_id)
    .bind(kind)
    .execute(db)
    .await
    .map_err(|e| format!("enqueue {kind}: {e}"))?;
    Ok(())
}

/// Appends a line to the job's log, which the portal shows.
pub async fn log(db: &PgPool, job_id: &str, line: &str) -> sqlx::Result<()> {
    sqlx::query(
        r#"UPDATE "Job" SET log = log || to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') || ' ' || $2 || E'\n'
           WHERE id = $1"#,
    )
    .bind(job_id)
    .bind(line)
    .execute(db)
    .await?;
    Ok(())
}

/// Backoff before the retry after `attempts` failed attempts (for tests and logs).
pub fn backoff(attempts: i32) -> Duration {
    let exp = attempts.saturating_sub(1).clamp(0, 20);
    Duration::from_millis((BACKOFF_MS as u64) << exp)
}
