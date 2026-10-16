//! RESTORE: a tenant's data back from the backup box (worker.ts, with
//! `Lifecycle.stop` and `Backups.restore`), then the stack starts again as
//! after an UPDATE.
//!
//!   1. the stack stops and its volume is locked;
//!   2. the node sets the current dataset aside (castor-snap set-aside);
//!   3. the box's raw stream goes straight into the node's zfs receive, through
//!      the control plane, which can read neither (the data stays encrypted
//!      with the tenant's key); a failure puts the old dataset back;
//!   4. the job remembers the data is back, so a retry only restarts the stack.
//!
//! The data set aside stays on the node until someone confirms the restore
//! (`castor-snap discard`, npm run backup).

use std::sync::Arc;

use serde_json::{Value, json};

use crate::db::audit;
use crate::node::{Node, NodeRow, ssh_cmd};
use crate::storage::{Status, TenantStorage, quota_mb};
use crate::suspend::{ALL_PROFILES, compose_args, strings};
use crate::{Control, Handler, Job, JobCx, apply, valid};

pub struct Restore(pub Arc<Control>);

#[derive(sqlx::FromRow)]
struct Tenant {
    id: String,
    slug: String,
    #[sqlx(rename = "nodeId")]
    node_id: Option<String>,
    #[sqlx(rename = "storageUnits")]
    storage_units: i32,
    #[sqlx(rename = "storageVolumeMb")]
    storage_volume_mb: Option<i32>,
    has_volume_key: bool,
}

/// g<n> as castor-backup names generations.
pub fn generation(s: &str) -> bool {
    s.strip_prefix('g')
        .is_some_and(|n| (1..=9).contains(&n.len()) && !n.starts_with('0') && n.bytes().all(|b| b.is_ascii_digit()))
}

/// cb-<...> as castor-backup names snapshots (snapshotName in retention.ts).
pub fn snapshot(s: &str) -> bool {
    s.strip_prefix("cb-")
        .is_some_and(|r| (1..=64).contains(&r.len()) && r.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
}

#[async_trait::async_trait]
impl Handler for Restore {
    fn kind(&self) -> &'static str {
        "RESTORE"
    }

    async fn run(&self, cx: &JobCx, job: &Job) -> Result<(), String> {
        let c = &self.0;
        let db = &cx.db;
        let target = c.backup_target.as_deref().ok_or("BACKUP_TARGET is not set")?;
        let params: Option<Value> = sqlx::query_scalar(r#"SELECT params FROM "Job" WHERE id = $1"#)
            .bind(&job.id)
            .fetch_optional(db)
            .await
            .map_err(|e| e.to_string())?
            .flatten();
        let params = params.unwrap_or(Value::Null);
        let (Some(gen_), Some(snap)) = (params["gen"].as_str(), params["snap"].as_str()) else {
            return Err("RESTORE needs params { gen, snap }".into());
        };
        if !generation(gen_) || !snapshot(snap) {
            return Err(format!("invalid backup {gen_}@{snap}"));
        }
        // A retry after the data is back only restarts the stack.
        if params["received"] != Value::Bool(true) {
            let t = sqlx::query_as::<_, Tenant>(
                r#"SELECT t.id, t.slug, t."nodeId", t."storageUnits", t."storageVolumeMb",
                          (v.id IS NOT NULL) AS has_volume_key
                   FROM "Tenant" t LEFT JOIN "VolumeKey" v ON v."tenantId" = t.id WHERE t.id = $1"#,
            )
            .bind(&job.tenant_id)
            .fetch_optional(db)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("tenant {} not found", job.tenant_id))?;
            if !valid::slug(&t.slug) {
                return Err(format!("invalid tenant slug {:?}", t.slug));
            }
            let node_id = t.node_id.as_deref().ok_or("tenant has no node")?;
            let node = Node::new(&NodeRow::load(db, node_id).await?, &c.root, c.exec.clone())?;
            stop(cx, &node, &t).await?;
            receive(cx, job, &node, &t, target, gen_, snap).await?;
            sqlx::query(r#"UPDATE "Job" SET params = params || '{"received": true}'::jsonb WHERE id = $1"#)
                .bind(&job.id)
                .execute(db)
                .await
                .map_err(|e| e.to_string())?;
        }
        apply::apply(c, cx, job).await
    }
}

/// Lifecycle.stop: nothing to stop when the volume is gone.
async fn stop(cx: &JobCx, node: &Node, t: &Tenant) -> Result<(), String> {
    let storage = TenantStorage::new(node, &t.slug)?;
    if t.has_volume_key && storage.status().await? == Status::Missing {
        return Ok(());
    }
    node.docker(compose_args(node, &t.slug, &ALL_PROFILES, &["stop"]), false)
        .await?;
    if t.has_volume_key {
        storage.close().await?;
        audit(
            &cx.db,
            "system",
            "volume.lock",
            Some(&t.id),
            Some(json!({ "reason": "restore" })),
        )
        .await?;
    }
    Ok(())
}

/// Backups.restore.
async fn receive(
    cx: &JobCx,
    job: &Job,
    node: &Node,
    t: &Tenant,
    target: &str,
    gen_: &str,
    snap: &str,
) -> Result<(), String> {
    let db = &cx.db;
    // Only ssh nodes keep tenants on ZFS (a node without a pool is refused when loaded).
    if node.is_local() {
        return Err(format!("node {} has no ZFS datasets", node.name));
    }
    let stamp: String = sqlx::query_scalar(r#"SELECT to_char(now() AT TIME ZONE 'UTC', 'YYYYMMDD"t"HH24MI')"#)
        .fetch_one(db)
        .await
        .map_err(|e| e.to_string())?;
    let tag = format!("before-restore-{stamp}");
    let aside = node
        .host(strings(&["castor-snap", "set-aside", &t.slug, &tag]), None)
        .await?;
    let had_data = aside.trim() != "missing";
    cx.log(
        job,
        &if had_data {
            format!("current data set aside as {}.{tag}", t.slug)
        } else {
            "no data on the node, restoring into an empty slot".into()
        },
    )
    .await;
    let quota = match t.storage_volume_mb {
        Some(mb) => u64::try_from(mb).map_err(|_| format!("invalid storageVolumeMb {mb}"))?,
        None => {
            quota_mb(u64::try_from(t.storage_units).map_err(|_| format!("invalid storageUnits {}", t.storage_units))?)
        }
    };
    let send = ssh_cmd(target, strings(&["castor-backup", "send", &t.slug, gen_, snap]))?;
    let received = node
        .pipe_into(send, strings(&["castor-snap", "receive", &t.slug, &quota.to_string()]))
        .await;
    let bytes = match received {
        Ok(b) => b,
        Err(e) if had_data => {
            return match node
                .host(strings(&["castor-snap", "put-back", &t.slug, &tag]), None)
                .await
            {
                Ok(_) => Err(e),
                Err(back) => Err(format!("{e}; putting the old data back failed too: {back}")),
            };
        }
        Err(e) => return Err(e),
    };
    cx.log(
        job,
        &format!("received {gen_}@{snap}, {} MB", (bytes + 524_288) / 1_048_576),
    )
    .await;
    // The node has no bookmark for the restored data: the next backup starts a new chain.
    sqlx::query(
        r#"INSERT INTO "TenantBackup" ("tenantId", "updatedAt") VALUES ($1, now() AT TIME ZONE 'UTC')
           ON CONFLICT ("tenantId") DO UPDATE SET "lastSnapshot" = NULL, "updatedAt" = now() AT TIME ZONE 'UTC'"#,
    )
    .bind(&t.id)
    .execute(db)
    .await
    .map_err(|e| e.to_string())?;
    audit(
        db,
        "system",
        "backup.restored",
        Some(&t.id),
        Some(json!({
            "node": node.name,
            "from": format!("{gen_}@{snap}"),
            "setAside": if had_data { Value::from(tag) } else { Value::Null },
        })),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_from_the_box() {
        assert!(generation("g1") && generation("g42"));
        for g in ["g", "g0", "g01", "1", "g1a", "g1/../x"] {
            assert!(!generation(g), "{g}");
        }
        assert!(snapshot("cb-20261016T2219Z"));
        for s in ["cb-", "x-1", "cb-a/b", "cb-a b", "cb-a.b"] {
            assert!(!snapshot(s), "{s}");
        }
    }
}
