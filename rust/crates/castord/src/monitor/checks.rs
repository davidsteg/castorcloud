//! What the monitor measures (monitor.ts): each check gathers its inputs and
//! hands them to the rules.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use sqlx::PgPool;

use super::rules::{self as r, Finding, Severity};
use super::{Check, Settings};
use crate::render::{Names, STACK_SERVICES, Service};

const HOUR: i64 = 3_600_000;

fn db_err(e: sqlx::Error) -> String {
    e.to_string()
}

/// A timestamp column as milliseconds since the epoch.
fn ms(col: &str) -> String {
    format!("(extract(epoch FROM {col}) * 1000)::bigint")
}

// ------------------------------------------------------------------ nodes

pub struct Nodes(pub PgPool);

#[async_trait::async_trait]
impl Check for Nodes {
    fn scope(&self) -> &'static str {
        "node"
    }

    async fn run(&self, now: i64) -> Result<Vec<Finding>, String> {
        let rows: Vec<(
            String,
            String,
            bool,
            i64,
            Option<i64>,
            Option<String>,
            Option<i32>,
            Option<i32>,
            Option<String>,
        )> = sqlx::query_as(&format!(
            r#"SELECT n.name, n.kind::text, n.enabled,
                          (SELECT count(*) FROM "Tenant" t WHERE t."nodeId" = n.id AND t.status <> 'DELETED'),
                          {}, n."lastError", n."poolSizeMb", n."poolUsedMb", n."poolHealth"
                   FROM "Node" n ORDER BY n.name"#,
            ms(r#"n."lastSeenAt""#)
        ))
        .fetch_all(&self.0)
        .await
        .map_err(db_err)?;
        let nodes: Vec<r::NodeState> = rows
            .into_iter()
            .map(
                |(name, kind, enabled, tenants, last_seen_at, last_error, size, used, health)| r::NodeState {
                    name,
                    kind,
                    enabled,
                    tenants,
                    last_seen_at,
                    last_error,
                    pool_size_mb: size.map(i64::from),
                    pool_used_mb: used.map(i64::from),
                    pool_health: health,
                },
            )
            .collect();
        Ok(r::node_findings(&nodes, now, &r::DEFAULT_THRESHOLDS))
    }
}

// --------------------------------------------------------------- services

/// Every booked service of every active tenant, through the same route customers use.
pub struct Services {
    pub db: PgPool,
    pub http: reqwest::Client,
    pub s: Arc<Settings>,
}

#[async_trait::async_trait]
impl Check for Services {
    fn scope(&self) -> &'static str {
        "service"
    }

    async fn run(&self, _now: i64) -> Result<Vec<Finding>, String> {
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            r#"SELECT t.id, t.slug, s.kind::text FROM "Tenant" t JOIN "ServiceInstance" s ON s."tenantId" = t.id
               WHERE t.status = 'ACTIVE' AND s.enabled ORDER BY t.slug, s.kind"#,
        )
        .fetch_all(&self.db)
        .await
        .map_err(db_err)?;
        let mut targets = Vec::new();
        for (tenant_id, slug, kind) in rows {
            if !crate::valid::slug(&slug) {
                continue;
            }
            let n = Names::new(&slug, &self.s.platform);
            let service = kind.to_lowercase();
            let url = if kind == "ASSISTANT" {
                format!("{}/health", n.assistant_origin())
            } else if let Some(svc) = Service::parse(&service).filter(|s| STACK_SERVICES.contains(s)) {
                format!("{}{}", n.service_origin(svc), svc.def().health_path)
            } else {
                continue;
            };
            targets.push((tenant_id, slug, kind, service, url));
        }
        // Eight at a time.
        let limit = Arc::new(tokio::sync::Semaphore::new(8));
        let mut set = tokio::task::JoinSet::new();
        for (i, (tenant_id, slug, kind, service, url)) in targets.into_iter().enumerate() {
            let (http, db, limit) = (self.http.clone(), self.db.clone(), limit.clone());
            set.spawn(async move {
                let _permit = limit.acquire_owned().await;
                let (ok, status, error) = match http.get(&url).timeout(Duration::from_secs(15)).send().await {
                    // As the provisioner's readiness check: redirects to sign-in are fine,
                    // 404 means no route and 5xx a broken or missing backend.
                    Ok(res) => (res.status().as_u16() < 400, Some(res.status().as_u16()), None),
                    Err(e) => (false, None, Some(e.to_string())),
                };
                let saved = sqlx::query(
                    r#"UPDATE "ServiceInstance" SET healthy = $3, "lastHealthAt" = now() AT TIME ZONE 'UTC'
                       WHERE "tenantId" = $1 AND kind = $2::"ServiceKind""#,
                )
                .bind(&tenant_id)
                .bind(&kind)
                .bind(ok)
                .execute(&db)
                .await;
                (
                    i,
                    saved.map(|_| r::ProbeResult {
                        slug,
                        service,
                        ok,
                        status,
                        error,
                    }),
                )
            });
        }
        let mut probes = Vec::new();
        while let Some(done) = set.join_next().await {
            let (i, probe) = done.map_err(|e| e.to_string())?;
            probes.push((i, probe.map_err(db_err)?));
        }
        probes.sort_by_key(|(i, _)| *i);
        let probes: Vec<r::ProbeResult> = probes.into_iter().map(|(_, p)| p).collect();
        Ok(r::service_findings(&probes))
    }
}

// ------------------------------------------------------------------- jobs

pub struct Jobs(pub PgPool);

#[async_trait::async_trait]
impl Check for Jobs {
    fn scope(&self) -> &'static str {
        "job"
    }

    async fn run(&self, now: i64) -> Result<Vec<Finding>, String> {
        let rows: Vec<(String, String, String, String, i64, Option<String>)> = sqlx::query_as(&format!(
            r#"SELECT j.id, j.kind::text, t.slug, j.status::text, {}, j.error
               FROM "Job" j JOIN "Tenant" t ON t.id = j."tenantId"
               WHERE j.status IN ('QUEUED', 'RUNNING')
                  OR (j.status = 'FAILED' AND j."createdAt" >= to_timestamp($1::bigint / 1000.0) AT TIME ZONE 'UTC')
               ORDER BY j."createdAt", j.id"#,
            ms(r#"j."createdAt""#)
        ))
        .bind(now - 7 * 24 * HOUR)
        .fetch_all(&self.0)
        .await
        .map_err(db_err)?;
        let jobs: Vec<r::JobState> = rows
            .into_iter()
            .map(|(id, kind, slug, status, created_at, error)| r::JobState {
                id,
                kind,
                slug,
                status,
                created_at,
                error,
            })
            .collect();
        Ok(r::job_findings(&jobs, now, &r::DEFAULT_THRESHOLDS))
    }
}

// ---------------------------------------------------------------- backups

/// Backup status as recorded by the backup runs (docs/backups.md).
pub struct Backups {
    pub db: PgPool,
    pub s: Arc<Settings>,
}

#[async_trait::async_trait]
impl Check for Backups {
    fn scope(&self) -> &'static str {
        "backup"
    }

    async fn run(&self, now: i64) -> Result<Vec<Finding>, String> {
        let remote: i64 = sqlx::query_scalar(
            r#"SELECT count(*) FROM "Tenant" t JOIN "Node" n ON n.id = t."nodeId"
               WHERE t.status = 'ACTIVE' AND n.kind <> 'LOCAL'"#,
        )
        .fetch_one(&self.db)
        .await
        .map_err(db_err)?;
        if remote == 0 {
            return Ok(Vec::new());
        }
        if !self.s.backups_on {
            return Ok(vec![Finding::new(
                "backup:off",
                Severity::Warning,
                format!("Tenant backups are off ({remote} active tenants)"),
            )]);
        }
        let rows: Vec<(String, i64, Option<i64>, i32, Option<String>, Option<String>)> = sqlx::query_as(&format!(
            r#"SELECT t.slug, {}, {}, COALESCE(b.failures, 0)::int, b."lastError", b."verifyError"
               FROM "Tenant" t
               JOIN "Node" n ON n.id = t."nodeId"
               LEFT JOIN "TenantBackup" b ON b."tenantId" = t.id
               WHERE t.status = 'ACTIVE' AND n.kind <> 'LOCAL' AND n."zfsPool" IS NOT NULL
               ORDER BY t.slug"#,
            ms(r#"t."createdAt""#),
            ms(r#"b."lastSuccessAt""#)
        ))
        .fetch_all(&self.db)
        .await
        .map_err(db_err)?;
        let rows: Vec<r::BackupState> = rows
            .into_iter()
            .map(
                |(slug, tenant_created_at, last_success_at, failures, last_error, verify_error)| r::BackupState {
                    slug,
                    tenant_created_at,
                    last_success_at,
                    failures: i64::from(failures),
                    last_error,
                    verify_error,
                },
            )
            .collect();
        Ok(r::backup_findings(&rows, self.s.backup_interval_hours, now))
    }
}

// ------------------------------------------------------------ certificates

pub struct Certs(pub Arc<Settings>);

#[async_trait::async_trait]
impl Check for Certs {
    fn scope(&self) -> &'static str {
        "cert"
    }

    async fn run(&self, now: i64) -> Result<Vec<Finding>, String> {
        let s = &self.0;
        if s.platform.scheme != "https" {
            return Ok(Vec::new());
        }
        let mut set = tokio::task::JoinSet::new();
        for (i, host) in s.tls_hosts.iter().enumerate() {
            let (host, address, roots) = (host.clone(), s.tls_address.clone(), s.roots.clone());
            set.spawn(async move { (i, super::tls::certificate_of(&host, address.as_deref(), roots).await) });
        }
        let mut results = Vec::new();
        while let Some(done) = set.join_next().await {
            results.push(done.map_err(|e| e.to_string())?);
        }
        results.sort_by_key(|(i, _)| *i);
        let results: Vec<r::CertResult> = results.into_iter().map(|(_, c)| c).collect();
        Ok(r::cert_findings(&results, now, &r::DEFAULT_THRESHOLDS))
    }
}

// --------------------------------------------------------------- security

/// Authentik, with the monitor's own read-only token (40-monitor.yaml).
pub struct Auth {
    pub http: reqwest::Client,
    pub s: Arc<Settings>,
}

#[derive(Deserialize)]
struct Page<T> {
    results: Vec<T>,
}

#[derive(Deserialize)]
struct AkUser {
    pk: i64,
    username: String,
}

#[derive(Deserialize)]
struct AkEventUser {
    pk: Option<serde_json::Value>,
    username: Option<String>,
}

#[derive(Deserialize)]
struct AkEvent {
    pk: String,
    action: String,
    created: String,
    client_ip: Option<String>,
    user: Option<AkEventUser>,
    context: Option<serde_json::Value>,
}

/// An ISO 8601 time as Authentik writes it ("2026-09-28T12:00:00.123456Z"
/// or with an offset), in ms.
pub fn parse_time(s: &str) -> Option<i64> {
    let (date, rest) = s.split_once('T')?;
    let mut d = date.split('-');
    let (y, m, day): (i64, u32, u32) = (
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
    );
    let (clock, offset_min) = if let Some(c) = rest.strip_suffix('Z') {
        (c, 0)
    } else {
        let at = rest.rfind(['+', '-'])?;
        let (c, off) = rest.split_at(at);
        let sign = if off.starts_with('-') { -1 } else { 1 };
        let (oh, om) = off.get(1..)?.split_once(':')?;
        (c, sign * (oh.parse::<i64>().ok()? * 60 + om.parse::<i64>().ok()?))
    };
    let (hms, frac) = clock.split_once('.').unwrap_or((clock, ""));
    let mut t = hms.split(':');
    let (h, mi, sec): (i64, i64, i64) = (
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
    );
    let millis: i64 = format!("{frac:0<3}").get(..3)?.parse().ok()?;
    let days = crate::retention::days_from_civil(y, m, day);
    Some(((days * 86_400 + h * 3600 + mi * 60 + sec) - offset_min * 60) * 1000 + millis)
}

impl Auth {
    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str, query: &[(&str, &str)]) -> Result<T, String> {
        let token = self.s.authentik_token.as_deref().ok_or("no token")?;
        let res = self
            .http
            .get(format!("{}/api/v3{path}", self.s.auth_url))
            .query(query)
            .bearer_auth(token)
            .header("Accept", "application/json")
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| format!("Authentik GET {path}: {e}"))?;
        if !res.status().is_success() {
            return Err(format!("Authentik GET {path} -> {}", res.status().as_u16()));
        }
        res.json().await.map_err(|e| format!("Authentik GET {path}: {e}"))
    }

    /// Authentik's action filter also matches longer actions ("login" finds
    /// "login_failed"), so results are filtered to the exact action here.
    async fn events(&self, action: &str, username: Option<&str>) -> Result<Vec<AkEvent>, String> {
        let mut q = vec![("ordering", "-created"), ("page_size", "500"), ("action", action)];
        if let Some(u) = username {
            q.push(("username", u));
        }
        let page: Page<AkEvent> = self.get("/events/events/", &q).await?;
        Ok(page.results.into_iter().filter(|e| e.action == action).collect())
    }
}

#[async_trait::async_trait]
impl Check for Auth {
    fn scope(&self) -> &'static str {
        "auth"
    }

    async fn run(&self, now: i64) -> Result<Vec<Finding>, String> {
        if self.s.authentik_token.is_none() {
            return Ok(vec![Finding::new(
                "auth:off",
                Severity::Warning,
                "Sign-in security checks are off (no monitor token for Authentik)",
            )]);
        }
        let since = now - 24 * HOUR;
        let admins: Page<AkUser> = self
            .get("/core/users/", &[("is_superuser", "true"), ("page_size", "100")])
            .await?;
        let mut raw = self.events("login_failed", None).await?;
        raw.extend(self.events("suspicious_request", None).await?);
        raw.extend(self.events("impersonation_started", None).await?);
        // Admin sign-ins, per admin: customers' sign-ins would crowd them out.
        for a in &admins.results {
            raw.extend(self.events("login", Some(&a.username)).await?);
        }
        let events: Vec<r::AuthEvent> = raw
            .into_iter()
            .filter_map(|e| {
                let created = parse_time(&e.created)?;
                // A failed sign-in has no user; the name that was tried is in the context.
                let tried = e
                    .context
                    .as_ref()
                    .and_then(|c| c.get("username"))
                    .and_then(|u| u.as_str())
                    .map(str::to_owned);
                let user_pk = e
                    .user
                    .as_ref()
                    .and_then(|u| u.pk.as_ref())
                    .and_then(serde_json::Value::as_i64);
                Some(r::AuthEvent {
                    pk: e.pk,
                    action: e.action,
                    created,
                    client_ip: e.client_ip,
                    username: tried.or_else(|| e.user.and_then(|u| u.username)),
                    user_pk,
                })
            })
            .filter(|e| e.created >= since)
            .collect();
        let superusers: HashSet<i64> = admins.results.iter().map(|a| a.pk).collect();
        Ok(r::auth_findings(&events, &superusers, now, &r::DEFAULT_THRESHOLDS))
    }
}

pub struct Audit(pub PgPool);

#[async_trait::async_trait]
impl Check for Audit {
    fn scope(&self) -> &'static str {
        "audit"
    }

    async fn run(&self, now: i64) -> Result<Vec<Finding>, String> {
        let rows: Vec<(String, String, String, Option<String>, i64, Option<serde_json::Value>)> =
            sqlx::query_as(&format!(
                r#"SELECT a.id, a.action, a.actor, t.slug, {}, a.details
                   FROM "AuditLog" a LEFT JOIN "Tenant" t ON t.id = a."tenantId"
                   WHERE a.action = ANY($1) AND a."createdAt" >= to_timestamp($2::bigint / 1000.0) AT TIME ZONE 'UTC'
                   ORDER BY a."createdAt", a.id"#,
                ms(r#"a."createdAt""#)
            ))
            .bind(r::AUDITED_ACTIONS.to_vec())
            .bind(now - 24 * HOUR)
            .fetch_all(&self.0)
            .await
            .map_err(db_err)?;
        let rows: Vec<r::AuditRow> = rows
            .into_iter()
            .map(|(id, action, actor, slug, created_at, details)| r::AuditRow {
                id,
                action,
                actor,
                slug,
                created_at,
                details,
            })
            .collect();
        Ok(r::audit_findings(&rows))
    }
}

// --------------------------------------------------------------- platform

/// The platform itself: the workers, Authentik.
pub struct Platform {
    pub db: PgPool,
    pub http: reqwest::Client,
    pub s: Arc<Settings>,
}

impl Platform {
    /// Job kinds no live worker takes (none: no worker at all).
    async fn uncovered(&self) -> Result<Option<(Vec<String>, Vec<String>)>, String> {
        // As liveWorkers(): a heartbeat within 2 min, or a job lease held.
        let workers: Vec<(String, Vec<String>)> = sqlx::query_as(
            r#"WITH busy AS (
                 SELECT "lockedBy" AS id, array_agg(DISTINCT kind::text) AS kinds FROM "Job"
                 WHERE status = 'RUNNING' AND "lockedUntil" >= now() AT TIME ZONE 'UTC' GROUP BY "lockedBy")
               SELECT COALESCE(h.id, b.id), COALESCE(h.kinds::text[], b.kinds)
               FROM (SELECT * FROM "WorkerHeartbeat" WHERE "seenAt" >= now() AT TIME ZONE 'UTC' - interval '2 minutes') h
               FULL JOIN busy b ON b.id = h.id"#,
        )
        .fetch_all(&self.db)
        .await
        .map_err(db_err)?;
        if workers.is_empty() {
            return Ok(None);
        }
        let all: Vec<String> = sqlx::query_scalar(r#"SELECT unnest(enum_range(NULL::"JobKind"))::text"#)
            .fetch_all(&self.db)
            .await
            .map_err(db_err)?;
        let covered: HashSet<&str> = workers.iter().flat_map(|(_, k)| k.iter().map(String::as_str)).collect();
        let missing = all.into_iter().filter(|k| !covered.contains(k.as_str())).collect();
        Ok(Some((missing, workers.into_iter().map(|(id, _)| id).collect())))
    }
}

#[async_trait::async_trait]
impl Check for Platform {
    fn scope(&self) -> &'static str {
        "platform"
    }

    async fn run(&self, _now: i64) -> Result<Vec<Finding>, String> {
        let mut out = Vec::new();
        let queue = "platform:queue";
        match tokio::time::timeout(Duration::from_secs(10), self.uncovered()).await {
            Ok(Ok(None)) => {
                out.push(Finding::new(queue, Severity::Critical, "The provisioner worker is not running").after(2))
            }
            // The TypeScript and Rust workers split the kinds: one of them is down.
            Ok(Ok(Some((missing, workers)))) if !missing.is_empty() => out.push(
                Finding::new(
                    queue,
                    Severity::Critical,
                    format!("No worker takes {} jobs", missing.join(", ")),
                )
                .details(json!({ "workers": workers }))
                .after(2),
            ),
            Ok(Ok(Some(_))) => {}
            // Same key: without the database the worker can't be seen either,
            // and its alert must not read as resolved.
            Ok(Err(e)) => out.push(
                Finding::new(queue, Severity::Critical, "The job queue's database is not reachable")
                    .details(json!({ "error": e }))
                    .after(2),
            ),
            Err(_) => out.push(
                Finding::new(queue, Severity::Critical, "The job queue's database is not reachable")
                    .details(json!({ "error": "database timed out after 10s" }))
                    .after(2),
            ),
        }
        let live = self
            .http
            .get(format!("{}/-/health/live/", self.s.auth_url))
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| {
                if r.status().is_success() {
                    Ok(())
                } else {
                    Err(format!("HTTP {}", r.status().as_u16()))
                }
            });
        if let Err(e) = live {
            out.push(
                Finding::new(
                    "platform:authentik",
                    Severity::Critical,
                    "Authentik (sign-in) is not responding",
                )
                .details(json!({ "error": e }))
                .after(2),
            );
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authentik_times() {
        assert_eq!(parse_time("2026-09-28T12:00:00Z"), Some(1_790_596_800_000));
        assert_eq!(parse_time("2026-09-28T12:00:00.123456Z"), Some(1_790_596_800_123));
        assert_eq!(parse_time("2026-09-28T14:00:00.5+02:00"), Some(1_790_596_800_500));
        assert_eq!(parse_time("2026-09-28T10:30:00-01:30"), Some(1_790_596_800_000));
        assert_eq!(parse_time("yesterday"), None);
    }
}
