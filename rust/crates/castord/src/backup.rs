//! Tenant backups: raw ZFS sends from each node to the backup box.
//!
//!   node (castor-snap)  ──ssh──►  castord  ──ssh──►  backup box (castor-backup)
//!   zfs send -w (ciphertext)      pipes the stream     zfs receive -u, no key
//!
//! The node and the box never trust each other; the control plane holds the
//! only key for each side. Streams are raw: the box stores tenant data still
//! encrypted with the tenant's key, which never leaves the control plane.
//! See docs/backups.md.
//!
//!   backup-tenants   every BACKUP_INTERVAL_HOURS: one backup of every active
//!                    tenant on a ZFS node, then retention
//!   backup-database  02:30: the control-plane database, sealed to the offline key
//!   verify-backups   Sundays 04:00: every tenant's newest backup read back on the box

use std::io::Write as _;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;
use sqlx::PgPool;
use zeroize::Zeroizing;

use crate::db::audit;
use crate::exec::Cmd;
use crate::node::{Node, NodeRow, ssh_cmd};
use crate::retention::{self, Snapshot};
use crate::schedule::Every;
use crate::stacks::js::slice;
use crate::suspend::strings;
use crate::tasks::{Task, busy};
use crate::{Control, valid};

/// BACKUP_* settings; backups are off without BACKUP_TARGET.
#[derive(Debug, Clone)]
pub struct Settings {
    /// user@host of the backup box (its key runs castor-backup as a forced command).
    pub target: String,
    pub interval_hours: f64,
    /// X25519 public key for the control-plane database dumps.
    pub db_public_key: Option<String>,
    /// The control-plane Postgres container, for pg_dumpall.
    pub pg_container: String,
    pub pg_user: String,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// The box's castor-backup command over ssh.
pub fn box_cmd(s: &Settings, args: &[&str]) -> Result<Cmd, String> {
    let mut all = vec!["castor-backup"];
    all.extend_from_slice(args);
    ssh_cmd(&s.target, strings(&all))
}

async fn on_box(c: &Control, s: &Settings, args: &[&str]) -> Result<String, String> {
    c.exec.run(box_cmd(s, args)?).await
}

pub async fn list(c: &Control, s: &Settings) -> Result<Vec<(String, Vec<Snapshot>)>, String> {
    Ok(retention::parse_list(&on_box(c, s, &["list"]).await?))
}

/// The box's pool: size and use in MB, health.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pool {
    pub size_mb: u64,
    pub used_mb: u64,
    pub health: String,
}

/// "pool <size-mb> <alloc-mb> <health>".
pub async fn pool(c: &Control, s: &Settings) -> Result<Pool, String> {
    let out = on_box(c, s, &["pool"]).await?;
    let w: Vec<&str> = out.split_whitespace().collect();
    match w.as_slice() {
        ["pool", size, used, rest @ ..] => Ok(Pool {
            size_mb: size.parse().unwrap_or(0),
            used_mb: used.parse().unwrap_or(0),
            health: rest.first().map_or_else(|| "UNKNOWN".to_owned(), |h| (*h).to_owned()),
        }),
        _ => Err("unexpected output from the backup box".into()),
    }
}

pub async fn pool_health(c: &Control, s: &Settings) -> Result<String, String> {
    Ok(pool(c, s).await?.health)
}

/// The database dumps on the box: (name, bytes), oldest first.
pub async fn db_list(c: &Control, s: &Settings) -> Result<Vec<(String, u64)>, String> {
    Ok(on_box(c, s, &["db-list"])
        .await?
        .lines()
        .filter_map(|l| {
            let (name, bytes) = l.split_once(' ')?;
            Some((name.to_owned(), bytes.trim().parse().unwrap_or(0)))
        })
        .collect())
}

/// The command that writes one dump (still sealed) to stdout.
pub fn db_get_cmd(s: &Settings, name: &str) -> Result<Cmd, String> {
    let ok = name.starts_with("cp-")
        && name.ends_with(".sql.gz.enc")
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.');
    if !ok {
        return Err(format!("not a dump name: {name:?} (see db-list)"));
    }
    box_cmd(s, &["db-get", name])
}

/// Removes data a restore set aside on the tenant's node ("before-restore-…").
pub async fn discard(c: &Control, db: &PgPool, tenant_id: &str, tag: &str) -> Result<String, String> {
    let ok = (1..=64).contains(&tag.len())
        && tag
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !ok {
        return Err(format!("invalid tag {tag:?}"));
    }
    let (slug, node_id): (String, Option<String>) =
        sqlx::query_as(r#"SELECT slug, "nodeId" FROM "Tenant" WHERE id = $1"#)
            .bind(tenant_id)
            .fetch_one(db)
            .await
            .map_err(|e| e.to_string())?;
    if !crate::valid::slug(&slug) {
        return Err(format!("invalid tenant slug {slug:?}"));
    }
    let node_id = node_id.ok_or_else(|| format!("{slug} has no node"))?;
    let node = Node::new(&NodeRow::load(db, &node_id).await?, &c.root, c.exec.clone())?;
    node.host(strings(&["castor-snap", "discard", &slug, tag]), None)
        .await?;
    Ok(slug)
}

#[derive(sqlx::FromRow)]
struct Backed {
    id: String,
    slug: String,
    #[sqlx(rename = "nodeId")]
    node_id: String,
    generation: Option<i32>,
    #[sqlx(rename = "lastSnapshot")]
    last_snapshot: Option<String>,
    failures: Option<i32>,
}

/// One backup of every active tenant on a ZFS node, then retention.
pub async fn run_all(c: &Control, db: &PgPool, s: &Settings) -> Result<(), String> {
    let health = pool_health(c, s).await?;
    if health != "ONLINE" {
        audit(
            db,
            "system",
            "backup.box_unhealthy",
            None,
            Some(json!({ "health": health })),
        )
        .await?;
        return Err(format!("backup box pool is {health}; not sending"));
    }
    let listing = list(c, s).await?;
    let tenants = sqlx::query_as::<_, Backed>(
        r#"SELECT t.id, t.slug, t."nodeId", b.generation, b."lastSnapshot", b.failures
           FROM "Tenant" t LEFT JOIN "TenantBackup" b ON b."tenantId" = t.id
           WHERE t.status = 'ACTIVE' AND t."nodeId" IS NOT NULL ORDER BY t.slug"#,
    )
    .fetch_all(db)
    .await
    .map_err(|e| e.to_string())?;
    let mut ok = 0;
    for t in tenants {
        let node = match NodeRow::load(db, &t.node_id).await {
            Ok(row) => Node::new(&row, &c.root, c.exec.clone()),
            Err(e) => Err(e),
        };
        let node = match node {
            Ok(n) => n,
            Err(e) => {
                eprintln!("[backup-tenants] {}: {e}", t.slug);
                continue;
            }
        };
        // Only ssh nodes keep tenants on ZFS (the local development node uses LUKS files).
        if node.is_local() {
            continue;
        }
        // Not while a lifecycle job (restore, delete, update) is changing the dataset.
        if busy(db, &t.id).await? {
            eprintln!("[backup-tenants] {}: busy, next run", t.slug);
            continue;
        }
        let empty = Vec::new();
        let on = listing
            .iter()
            .find(|(slug, _)| *slug == t.slug)
            .map_or(&empty, |(_, l)| l);
        // A tenant deleted meanwhile, or a database hiccup, costs that tenant's backup only.
        match backup_tenant(c, db, s, &t, &node, on).await {
            Ok(true) => ok += 1,
            Ok(false) => {}
            Err(e) => eprintln!("[backup-tenants] {}: {e}", t.slug),
        }
    }
    eprintln!("[backup-tenants] backed up {ok} tenant(s)");
    prune(c, db, s).await
}

/// One tenant now (the admin CLI's `run <slug>`); Ok(false) when it failed.
pub async fn backup_one(c: &Control, db: &PgPool, s: &Settings, tenant_id: &str) -> Result<bool, String> {
    let t = sqlx::query_as::<_, Backed>(
        r#"SELECT t.id, t.slug, t."nodeId", b.generation, b."lastSnapshot", b.failures
           FROM "Tenant" t LEFT JOIN "TenantBackup" b ON b."tenantId" = t.id
           WHERE t.id = $1 AND t."nodeId" IS NOT NULL"#,
    )
    .bind(tenant_id)
    .fetch_optional(db)
    .await
    .map_err(|e| e.to_string())?
    .ok_or("the tenant has no node")?;
    let node = Node::new(&NodeRow::load(db, &t.node_id).await?, &c.root, c.exec.clone())?;
    if node.is_local() {
        return Err(format!("{} isn't on a ZFS node", t.slug));
    }
    if busy(db, &t.id).await? {
        return Err(format!(
            "{} has a job queued or running; try again when it's done",
            t.slug
        ));
    }
    let listing = list(c, s).await?;
    let empty = Vec::new();
    let on = listing
        .iter()
        .find(|(slug, _)| *slug == t.slug)
        .map_or(&empty, |(_, l)| l);
    backup_tenant(c, db, s, &t, &node, on).await
}

/// Snapshot, send (incremental when the chain is intact), bookmark. Records
/// the outcome; Ok(false) when this tenant's backup failed.
async fn backup_tenant(
    c: &Control,
    db: &PgPool,
    s: &Settings,
    t: &Backed,
    node: &Node,
    on: &[Snapshot],
) -> Result<bool, String> {
    if !valid::slug(&t.slug) {
        eprintln!("[backup-tenants] invalid tenant slug {:?}", t.slug);
        return Ok(false);
    }
    let snap = retention::snapshot_name(now());
    let state_gen = i64::from(t.generation.unwrap_or(0));
    let failures = t.failures.unwrap_or(0);
    sqlx::query(
        r#"INSERT INTO "TenantBackup" ("tenantId", "lastAttemptAt", "updatedAt") VALUES ($1, now() AT TIME ZONE 'UTC', now() AT TIME ZONE 'UTC')
           ON CONFLICT ("tenantId") DO UPDATE SET "lastAttemptAt" = now() AT TIME ZONE 'UTC', "updatedAt" = now() AT TIME ZONE 'UTC'"#,
    )
    .bind(&t.id)
    .execute(db)
    .await
    .map_err(|e| e.to_string())?;
    let sent = async {
        node.host(strings(&["castor-snap", "snapshot", &t.slug, &snap]), None)
            .await?;
        let bookmarks = node.host(strings(&["castor-snap", "bookmarks", &t.slug]), None).await?;
        let base = t.last_snapshot.as_deref().filter(|b| valid::ssh_word(b));
        let current = format!("g{state_gen}");
        let incremental = state_gen > 0
            && base.is_some_and(|b| {
                bookmarks.lines().any(|l| l.trim() == b) && on.iter().any(|x| x.gen_ == current && x.snap == b)
            });
        let generation = if incremental {
            state_gen
        } else {
            on.iter()
                .map(|x| retention::gen_number(Some(&x.gen_)))
                .fold(state_gen, i64::max)
                + 1
        };
        let mut send = vec!["castor-snap", "send", &t.slug, &snap];
        if let (true, Some(b)) = (incremental, base) {
            send.push(b);
        }
        let g = format!("g{generation}");
        let bytes = c
            .exec
            .pipe(node.host_cmd(strings(&send))?, box_cmd(s, &["recv", &t.slug, &g])?)
            .await?;
        node.host(strings(&["castor-snap", "sent", &t.slug, &snap]), None)
            .await?;
        Ok::<_, String>((generation, bytes, incremental))
    };
    match sent.await {
        Ok((generation, bytes, incremental)) => {
            sqlx::query(
                r#"UPDATE "TenantBackup" SET generation = $2, "lastSnapshot" = $3, "lastBytes" = $4,
                          "lastSuccessAt" = now() AT TIME ZONE 'UTC', "lastError" = NULL, failures = 0,
                          "updatedAt" = now() AT TIME ZONE 'UTC'
                   WHERE "tenantId" = $1"#,
            )
            .bind(&t.id)
            .bind(i32::try_from(generation).map_err(|e| e.to_string())?)
            .bind(&snap)
            .bind(i64::try_from(bytes).map_err(|e| e.to_string())?)
            .execute(db)
            .await
            .map_err(|e| e.to_string())?;
            if failures > 0 {
                audit(
                    db,
                    "system",
                    "backup.recovered",
                    Some(&t.id),
                    Some(json!({ "snap": snap })),
                )
                .await?;
            }
            eprintln!(
                "[backup-tenants] {}: {} g{generation}@{snap}, {} MB",
                t.slug,
                if incremental { "incremental" } else { "full" },
                (bytes + 524_288) / 1_048_576
            );
            Ok(true)
        }
        Err(e) => {
            let message = slice(&e, 2000);
            // Don't let failed snapshots eat the quota; if the node is down, `sent` clears leftovers next time.
            let _ = node.host(strings(&["castor-snap", "drop", &t.slug, &snap]), None).await;
            sqlx::query(
                r#"UPDATE "TenantBackup" SET "lastError" = $2, failures = failures + 1, "updatedAt" = now() AT TIME ZONE 'UTC'
                   WHERE "tenantId" = $1"#,
            )
            .bind(&t.id)
            .bind(message)
            .execute(db)
            .await
            .map_err(|e| e.to_string())?;
            if failures == 0 {
                audit(
                    db,
                    "system",
                    "backup.failed",
                    Some(&t.id),
                    Some(json!({ "error": slice(message, 500) })),
                )
                .await?;
            }
            eprintln!("[backup-tenants] {}: backup failed: {message}", t.slug);
            Ok(false)
        }
    }
}

/// The box refuses to delete anything younger than its own floor; those
/// refusals are expected.
fn refused(e: &str) -> bool {
    e.contains("refused")
}

/// Applies the retention policy, and forgets tenants that were deleted (their
/// key is gone, so what's left is unreadable).
pub async fn prune(c: &Control, db: &PgPool, s: &Settings) -> Result<(), String> {
    let listing = list(c, s).await?;
    let slugs: Vec<&str> = listing.iter().map(|(slug, _)| slug.as_str()).collect();
    let status: Vec<(String, String)> =
        sqlx::query_as(r#"SELECT slug, status::text FROM "Tenant" WHERE slug = ANY($1)"#)
            .bind(&slugs)
            .fetch_all(db)
            .await
            .map_err(|e| e.to_string())?;
    let now = now();
    let mut pruned = 0;
    for (slug, snaps) in &listing {
        // The box checks every word too; a slug from its listing is still checked here.
        if !valid::slug(slug) {
            eprintln!("[backup-tenants] skipping unexpected name {slug:?} on the box");
            continue;
        }
        let st = status.iter().find(|(s, _)| s == slug).map(|(_, st)| st.as_str());
        if st.is_none() || st == Some("DELETED") {
            match on_box(c, s, &["forget", slug]).await {
                Ok(_) => eprintln!("[backup-tenants] {slug}: deleted tenant, backups forgotten"),
                Err(e) if refused(&e) => {}
                Err(e) => eprintln!("[backup-tenants] {slug}: forget failed: {e}"),
            }
            continue;
        }
        for x in retention::to_prune(snaps, now, retention::DEFAULT) {
            match on_box(c, s, &["prune", slug, &x.gen_, &x.snap]).await {
                Ok(_) => pruned += 1,
                Err(e) if refused(&e) => {}
                Err(e) => eprintln!("[backup-tenants] {slug}: prune {}@{} failed: {e}", x.gen_, x.snap),
            }
        }
    }
    if pruned > 0 {
        eprintln!("[backup-tenants] pruned {pruned} old backup(s)");
    }
    Ok(())
}

/// Reads each tenant's newest backup back on the box; ZFS checks every block.
pub async fn verify_all(c: &Control, db: &PgPool, s: &Settings) -> Result<(), String> {
    let listing = list(c, s).await?;
    let slugs: Vec<&str> = listing.iter().map(|(slug, _)| slug.as_str()).collect();
    let tenants: Vec<(String, String)> =
        sqlx::query_as(r#"SELECT id, slug FROM "Tenant" WHERE slug = ANY($1) AND status <> 'DELETED' ORDER BY slug"#)
            .bind(&slugs)
            .fetch_all(db)
            .await
            .map_err(|e| e.to_string())?;
    for (id, slug) in tenants {
        let Some(newest) = listing.iter().find(|(s, _)| *s == slug).and_then(|(_, l)| l.last()) else {
            continue;
        };
        match on_box(c, s, &["verify", &slug, &newest.gen_, &newest.snap]).await {
            Ok(_) => {
                sqlx::query(
                    r#"INSERT INTO "TenantBackup" ("tenantId", "lastVerifiedAt", "updatedAt") VALUES ($1, now() AT TIME ZONE 'UTC', now() AT TIME ZONE 'UTC')
                       ON CONFLICT ("tenantId") DO UPDATE SET "lastVerifiedAt" = now() AT TIME ZONE 'UTC', "verifyError" = NULL,
                                                             "updatedAt" = now() AT TIME ZONE 'UTC'"#,
                )
                .bind(&id)
                .execute(db)
                .await
                .map_err(|e| e.to_string())?;
            }
            Err(e) => {
                let message = slice(&e, 2000);
                sqlx::query(
                    r#"INSERT INTO "TenantBackup" ("tenantId", "verifyError", "updatedAt") VALUES ($1, $2, now() AT TIME ZONE 'UTC')
                       ON CONFLICT ("tenantId") DO UPDATE SET "verifyError" = $2, "updatedAt" = now() AT TIME ZONE 'UTC'"#,
                )
                .bind(&id)
                .bind(message)
                .execute(db)
                .await
                .map_err(|e| e.to_string())?;
                audit(
                    db,
                    "system",
                    "backup.verify_failed",
                    Some(&id),
                    Some(json!({ "snap": format!("{}@{}", newest.gen_, newest.snap), "error": slice(message, 500) })),
                )
                .await?;
                eprintln!("[verify-backups] {slug}: verify failed: {message}");
            }
        }
    }
    Ok(())
}

#[derive(sqlx::FromRow)]
struct Overdue {
    slug: String,
    since: Option<String>,
    overdue: bool,
    #[sqlx(rename = "lastError")]
    last_error: Option<String>,
    failures: Option<i32>,
    #[sqlx(rename = "verifyError")]
    verify_error: Option<String>,
    kind: String,
}

/// Tenants whose backups need attention: failing, overdue (no success for two
/// intervals), or failing verification.
pub async fn problems(db: &PgPool, s: &Settings) -> Result<Vec<(String, Vec<String>)>, String> {
    let due_ms = 2.0 * s.interval_hours * 3_600_000.0 + 30.0 * 60_000.0;
    let rows = sqlx::query_as::<_, Overdue>(
        r#"WITH due AS (SELECT now() AT TIME ZONE 'UTC' - make_interval(secs => $1 / 1000.0) AS at)
           SELECT t.slug, n.kind::text AS kind,
                  to_char(b."lastSuccessAt", 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') AS since,
                  (b."lastSuccessAt" IS NULL OR b."lastSuccessAt" < due.at) AS overdue,
                  b."lastError", b.failures, b."verifyError"
           FROM "Tenant" t JOIN "Node" n ON n.id = t."nodeId" CROSS JOIN due
           LEFT JOIN "TenantBackup" b ON b."tenantId" = t.id
           WHERE t.status = 'ACTIVE' AND n."zfsPool" IS NOT NULL AND t."createdAt" < due.at
           ORDER BY t.slug"#,
    )
    .bind(due_ms)
    .fetch_all(db)
    .await
    .map_err(|e| e.to_string())?;
    Ok(rows
        .into_iter()
        .filter(|r| r.kind != "LOCAL")
        .filter_map(|r| {
            let mut out = Vec::new();
            if r.overdue {
                out.push(format!("no backup since {}", r.since.as_deref().unwrap_or("ever")));
            }
            if let Some(e) = &r.last_error {
                out.push(format!(
                    "last run failed ({}x): {}",
                    r.failures.unwrap_or(0),
                    slice(e, 200)
                ));
            }
            if let Some(e) = &r.verify_error {
                out.push(format!("verify failed: {}", slice(e, 200)));
            }
            (!out.is_empty()).then_some((r.slug, out))
        })
        .collect())
}

/// pg_dumpall of the control plane (portal and Authentik, with the wrapped
/// tenant keys), gzipped and sealed to the offline key, stored on the box.
pub async fn dump_database(c: &Control, s: &Settings) -> Result<String, String> {
    let Some(public) = &s.db_public_key else {
        return Err("BACKUP_DB_PUBLIC_KEY is not set: the control-plane database is NOT backed up".into());
    };
    let recipient = castor_crypto::dump::public_key(public).map_err(|e| e.to_string())?;
    let sql = Zeroizing::new(
        c.exec
            .run_bytes(Cmd::new(
                "docker",
                strings(&[
                    "exec",
                    &s.pg_container,
                    "pg_dumpall",
                    "-U",
                    &s.pg_user,
                    "--clean",
                    "--if-exists",
                ]),
            ))
            .await?,
    );
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&sql).map_err(|e| format!("gzip: {e}"))?;
    let plain = Zeroizing::new(gz.finish().map_err(|e| format!("gzip: {e}"))?);
    let sealed = castor_crypto::dump::seal(&plain, &recipient).map_err(|e| e.to_string())?;
    let stamp = retention::snapshot_name(now());
    // cp-20261016t0230.sql.gz.enc.
    let name = format!(
        "cp-{}.sql.gz.enc",
        stamp.trim_start_matches("cb-").trim_end_matches('Z').to_lowercase()
    );
    let kb = (sealed.len() + 512) / 1024;
    let mut put = box_cmd(s, &["db-put", &name])?;
    put.input = Some(Zeroizing::new(sealed));
    c.exec.run(put).await?;
    eprintln!("[backup-database] database dump {name}, {kb} KB");
    Ok(name)
}

// ------------------------------------------------------------ the tasks

pub struct BackupTenants(pub Arc<Control>, pub Settings);
pub struct BackupDatabase(pub Arc<Control>, pub Settings);
pub struct VerifyBackups(pub Arc<Control>, pub Settings);

#[async_trait::async_trait]
impl Task for BackupTenants {
    fn name(&self) -> &'static str {
        "backup-tenants"
    }

    fn every(&self) -> Every {
        // Whole milliseconds of the interval.
        #[allow(clippy::cast_possible_truncation)]
        Every::Ms((self.1.interval_hours * 3_600_000.0) as i64)
    }

    async fn run(&self, db: &PgPool) -> Result<(), String> {
        run_all(&self.0, db, &self.1).await?;
        for (slug, issues) in problems(db, &self.1).await? {
            eprintln!("[backup {slug}] {}", issues.join("; "));
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Task for BackupDatabase {
    fn name(&self) -> &'static str {
        "backup-database"
    }

    fn every(&self) -> Every {
        Every::At {
            time: "02:30",
            tz: self.0.platform.time_zone.clone(),
            weekday: None,
        }
    }

    async fn run(&self, _db: &PgPool) -> Result<(), String> {
        dump_database(&self.0, &self.1).await.map(|_| ())
    }
}

#[async_trait::async_trait]
impl Task for VerifyBackups {
    fn name(&self) -> &'static str {
        "verify-backups"
    }

    fn every(&self) -> Every {
        Every::At {
            time: "04:00",
            tz: self.0.platform.time_zone.clone(),
            weekday: Some(0),
        }
    }

    async fn run(&self, db: &PgPool) -> Result<(), String> {
        verify_all(&self.0, db, &self.1).await
    }
}
