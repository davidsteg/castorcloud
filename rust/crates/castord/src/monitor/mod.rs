//! The monitor: checks the platform every few minutes and alerts
//! the operator on Telegram and/or by e-mail (docs/monitoring.md). Its own
//! process (castor-monitor), so it keeps reporting when the workers are down.
//! Alert state is the Alert table.

pub mod checks;
pub mod deliver;
pub mod rules;
pub mod tls;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use sqlx::PgPool;
use zeroize::Zeroizing;

use rules::{Change, Finding, Notice, Severity, StoredAlert};

#[async_trait::async_trait]
pub trait Check: Send + Sync {
    /// The first segment of its findings' keys.
    fn scope(&self) -> &'static str;
    async fn run(&self, now: i64) -> Result<Vec<Finding>, String>;
}

/// Where alerts go. Returns whether the message reached the operator.
#[async_trait::async_trait]
pub trait Deliver: Send + Sync {
    async fn deliver(&self, notices: &[Notice]) -> bool;
}

pub struct Settings {
    pub platform: crate::render::Platform,
    /// Authentik's address (MONITOR_AUTHENTIK_URL, else its public URL).
    pub auth_url: String,
    pub authentik_token: Option<Zeroizing<String>>,
    pub backups_on: bool,
    pub backup_interval_hours: f64,
    pub tls_hosts: Vec<String>,
    /// Where to connect for TLS when the public address isn't reachable from
    /// the LAN (no hairpin NAT); the name is still checked.
    pub tls_address: Option<String>,
    pub roots: Arc<tokio_rustls::rustls::RootCertStore>,
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Every check, in the order the digest lists them.
pub fn all_checks(db: &PgPool, http: &reqwest::Client, s: &Arc<Settings>) -> Vec<Arc<dyn Check>> {
    vec![
        Arc::new(checks::Platform {
            db: db.clone(),
            http: http.clone(),
            s: s.clone(),
        }),
        Arc::new(checks::Nodes(db.clone())),
        Arc::new(checks::Services {
            db: db.clone(),
            http: http.clone(),
            s: s.clone(),
        }),
        Arc::new(checks::Jobs(db.clone())),
        Arc::new(checks::Backups {
            db: db.clone(),
            s: s.clone(),
        }),
        Arc::new(checks::Certs(s.clone())),
        Arc::new(checks::Auth {
            http: http.clone(),
            s: s.clone(),
        }),
        Arc::new(checks::Audit(db.clone())),
    ]
}

#[derive(sqlx::FromRow)]
struct AlertRow {
    key: String,
    severity: String,
    summary: String,
    once: bool,
    hits: i32,
    notified_at: Option<i64>,
    resolved_at: Option<i64>,
}

const STORED: &str = r#"SELECT key, severity::text AS severity, summary, once, hits,
       (extract(epoch FROM "notifiedAt") * 1000)::bigint AS notified_at,
       (extract(epoch FROM "resolvedAt") * 1000)::bigint AS resolved_at
FROM "Alert""#;

/// A ms time as the columns' UTC timestamp.
const AT: &str = "to_timestamp($1::bigint / 1000.0) AT TIME ZONE 'UTC'";

/// What one round found.
#[derive(Debug)]
pub struct Round {
    pub findings: usize,
    pub open: i64,
    pub notices: Vec<Notice>,
    pub delivered: bool,
    pub skipped: Vec<String>,
}

/// One round: every check, then the alert changes, then the notifications.
pub async fn round(
    db: &PgPool,
    checks: &[Arc<dyn Check>],
    out: &dyn Deliver,
    renotify_ms: i64,
    now: i64,
) -> Result<Round, String> {
    let ran = run_checks(checks, now).await;
    let mut findings: Vec<Finding> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    for (scope, result) in ran {
        match result {
            Ok(f) => findings.extend(f),
            Err(e) => {
                // Its open alerts stay open; the failure itself is reported.
                eprintln!("[monitor] {scope} check: {e}");
                skipped.push(scope.to_owned());
                findings.push(
                    Finding::new(
                        format!("monitor:{scope}"),
                        Severity::Warning,
                        format!("The {scope} check could not run"),
                    )
                    .details(json!({ "error": crate::stacks::js::slice(&e, 2000) }))
                    .after(2),
                );
            }
        }
    }

    let keys: Vec<&str> = findings.iter().map(|f| f.key.as_str()).collect();
    let rows: Vec<AlertRow> = sqlx::query_as(&format!(
        r#"{STORED} WHERE "resolvedAt" IS NULL OR key = ANY($1) ORDER BY "firstSeenAt", key"#
    ))
    .bind(&keys)
    .fetch_all(db)
    .await
    .map_err(|e| e.to_string())?;
    let stored: Vec<StoredAlert> = rows
        .into_iter()
        .map(|r| StoredAlert {
            key: r.key,
            severity: Severity::parse(&r.severity).unwrap_or(Severity::Warning),
            summary: r.summary,
            once: r.once,
            hits: r.hits,
            notified_at: r.notified_at,
            resolved_at: r.resolved_at,
        })
        .collect();
    let skipped_set: HashSet<String> = skipped.iter().cloned().collect();
    let (changes, notices) = rules::reconcile(&findings, &stored, now, renotify_ms, &skipped_set);
    let delivered = !notices.is_empty() && out.deliver(&notices).await;

    for c in changes {
        match c {
            Change::Resolve { key, notify } => {
                if notify && !delivered {
                    continue; // announce it next round
                }
                sqlx::query(&format!(
                    r#"UPDATE "Alert" SET "resolvedAt" = {AT}, hits = 0 WHERE key = $2"#
                ))
                .bind(now)
                .bind(&key)
                .execute(db)
                .await
                .map_err(|e| e.to_string())?;
            }
            Change::Upsert {
                finding: f,
                hits,
                reopen,
                notify,
            } => {
                let sent = notify.is_some() && delivered;
                sqlx::query(&format!(
                    r#"INSERT INTO "Alert" AS a (id, key, severity, summary, details, once, hits,
                                               "firstSeenAt", "lastSeenAt", "notifiedAt", "resolvedAt")
                       VALUES ($2, $3, $4::"AlertSeverity", $5, $6::jsonb, $7, $8, {AT}, {AT},
                               CASE WHEN $9 THEN {AT} END, CASE WHEN $7 THEN {AT} END)
                       ON CONFLICT (key) DO UPDATE SET
                         severity = EXCLUDED.severity, summary = EXCLUDED.summary, details = EXCLUDED.details,
                         once = EXCLUDED.once, hits = EXCLUDED.hits, "lastSeenAt" = EXCLUDED."lastSeenAt",
                         "resolvedAt" = EXCLUDED."resolvedAt",
                         "firstSeenAt" = CASE WHEN $10 THEN EXCLUDED."firstSeenAt" ELSE a."firstSeenAt" END,
                         "notifiedAt" = CASE WHEN $9 THEN EXCLUDED."notifiedAt"
                                             WHEN $10 THEN NULL ELSE a."notifiedAt" END"#
                ))
                .bind(now)
                .bind(crate::db::new_id()?)
                .bind(&f.key)
                .bind(f.severity.as_str())
                .bind(&f.summary)
                .bind(f.details.as_ref().map(serde_json::Value::to_string))
                // One-off events are over as soon as they're recorded.
                .bind(f.once)
                .bind(hits)
                .bind(sent)
                .bind(reopen)
                .execute(db)
                .await
                .map_err(|e| e.to_string())?;
            }
        }
    }
    // Keep a quarter of history.
    sqlx::query(&format!(
        r#"DELETE FROM "Alert" WHERE "resolvedAt" < {AT} - interval '90 days'"#
    ))
    .bind(now)
    .execute(db)
    .await
    .map_err(|e| e.to_string())?;
    let open: i64 = sqlx::query_scalar(r#"SELECT count(*) FROM "Alert" WHERE "resolvedAt" IS NULL"#)
        .fetch_one(db)
        .await
        .map_err(|e| e.to_string())?;
    Ok(Round {
        findings: findings.len(),
        open,
        notices,
        delivered,
        skipped,
    })
}

/// Every check at once, each given two minutes; results in the checks' order.
async fn run_checks(checks: &[Arc<dyn Check>], now: i64) -> Vec<(&'static str, Result<Vec<Finding>, String>)> {
    let mut set = tokio::task::JoinSet::new();
    for (i, c) in checks.iter().enumerate() {
        let c = c.clone();
        set.spawn(async move {
            let r = match tokio::time::timeout(Duration::from_secs(120), c.run(now)).await {
                Ok(r) => r,
                Err(_) => Err(format!("{} check timed out after 120s", c.scope())),
            };
            (i, c.scope(), r)
        });
    }
    let mut out = Vec::new();
    while let Some(done) = set.join_next().await {
        match done {
            Ok(r) => out.push(r),
            Err(e) => eprintln!("[monitor] check: {e}"),
        }
    }
    out.sort_by_key(|(i, _, _)| *i);
    out.into_iter().map(|(_, s, r)| (s, r)).collect()
}
