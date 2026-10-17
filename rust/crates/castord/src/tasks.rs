//! The worker's recurring tasks (worker.ts): each runs once per slot across
//! every worker, TypeScript or Rust (schedule.rs). CASTORD_TASKS names the
//! ones castord runs; the TypeScript worker then leaves them alone.
//!
//!   watch-nodes      every 5 min: probe remote nodes; reopen tenants whose
//!                    storage came back locked (a node restart: keys are never
//!                    stored on a node)
//!   measure-storage  hourly: every active tenant's storage use, for the portal
//!   sweep-deletions  hourly: delete tenants whose grace period has ended

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use sqlx::PgPool;

use crate::node::{Node, NodeRow, Role};
use crate::schedule::{self, Every};
use crate::storage::{StacksStorage, Status, TenantStorage};
use crate::suspend::strings;
use crate::{Control, queue, valid};

/// Every task the workers run, by name (worker.ts).
pub const ALL_TASKS: [&str; 6] = [
    "watch-nodes",
    "measure-storage",
    "sweep-deletions",
    "backup-tenants",
    "backup-database",
    "verify-backups",
];

#[async_trait::async_trait]
pub trait Task: Send + Sync {
    fn name(&self) -> &'static str;
    fn every(&self) -> Every;
    async fn run(&self, db: &PgPool) -> Result<(), String>;
}

/// Until `stopping` is set: every 30 s, each due task whose slot this worker wins.
pub async fn run(db: PgPool, tasks: Vec<Arc<dyn Task>>, stopping: Arc<AtomicBool>) {
    while !stopping.load(Ordering::SeqCst) {
        for t in &tasks {
            if stopping.load(Ordering::SeqCst) {
                break;
            }
            match schedule::claim(&db, t.name(), &t.every()).await {
                Ok(true) => {
                    if let Err(e) = t.run(&db).await {
                        eprintln!("[{}] {e}", t.name());
                    }
                }
                Ok(false) => {}
                Err(e) => eprintln!("[{}] {e}", t.name()),
            }
        }
        // Wakes early to stop.
        for _ in 0..30 {
            if stopping.load(Ordering::SeqCst) {
                return;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

/// Whether a lifecycle job of the tenant is queued or running (it changes the
/// datasets, so tasks leave the tenant alone until it's done).
pub async fn busy(db: &PgPool, tenant_id: &str) -> Result<bool, String> {
    sqlx::query_scalar(
        r#"SELECT EXISTS (SELECT 1 FROM "Job" WHERE "tenantId" = $1 AND status IN ('QUEUED', 'RUNNING'))"#,
    )
    .bind(tenant_id)
    .fetch_one(db)
    .await
    .map_err(|e| e.to_string())
}

#[derive(sqlx::FromRow)]
struct TenantRef {
    id: String,
    slug: String,
}

// ------------------------------------------------------------ watch-nodes

pub struct WatchNodes(pub Arc<Control>);

#[async_trait::async_trait]
impl Task for WatchNodes {
    fn name(&self) -> &'static str {
        "watch-nodes"
    }

    fn every(&self) -> Every {
        Every::Ms(5 * 60_000)
    }

    async fn run(&self, db: &PgPool) -> Result<(), String> {
        let ids: Vec<String> =
            sqlx::query_scalar(r#"SELECT id FROM "Node" WHERE kind IN ('MAC', 'TRUENAS') ORDER BY name, id"#)
                .fetch_all(db)
                .await
                .map_err(|e| e.to_string())?;
        for id in ids {
            // A node removed meanwhile is skipped.
            let watched = match NodeRow::load(db, &id).await {
                Ok(row) => self
                    .watch(db, &row)
                    .await
                    .map_err(|e| format!("[node {}] {e}", row.name)),
                Err(e) => Err(format!("[node {id}] {e}")),
            };
            if let Err(e) = watched {
                eprintln!("{e}");
            }
        }
        Ok(())
    }
}

/// pool <size-mb> <used-mb> <health> (castor-zfs pool).
fn parse_pool(text: &str) -> Option<(i32, i32, String)> {
    let w: Vec<&str> = text.split_whitespace().collect();
    match w.as_slice() {
        ["pool", size, used, health, ..] => Some((size.parse().ok()?, used.parse().ok()?, (*health).to_owned())),
        _ => None,
    }
}

/// info <os> <memory-mb> <cpus> (castor-info).
fn parse_info(text: &str) -> Option<(i32, i32)> {
    let w: Vec<&str> = text.split_whitespace().collect();
    match w.as_slice() {
        ["info", _os, memory, cpus, ..] => Some((memory.parse().ok()?, cpus.parse().ok()?)),
        _ => None,
    }
}

impl WatchNodes {
    /// One node: probed, its facts recorded, its locked tenants reopened.
    pub async fn watch(&self, db: &PgPool, row: &NodeRow) -> Result<(), String> {
        let c = &self.0;
        let probed = match Node::new(row, &c.root, c.exec.clone()) {
            Ok(node) => probe(&node, row).await.map(|facts| (node, facts)),
            Err(e) => Err(e),
        };
        let (node, (size, used, health, memory, cpus)) = match probed {
            Ok(p) => p,
            Err(e) => {
                sqlx::query(r#"UPDATE "Node" SET "lastError" = $2 WHERE id = $1"#)
                    .bind(&row.id)
                    .bind(crate::stacks::js::slice(&e, 2000))
                    .execute(db)
                    .await
                    .map_err(|e| e.to_string())?;
                return Err(e);
            }
        };
        sqlx::query(
            r#"UPDATE "Node" SET "poolSizeMb" = $2, "poolUsedMb" = $3, "poolHealth" = $4, "memoryMb" = $5, cpus = $6,
                      "lastSeenAt" = now() AT TIME ZONE 'UTC', "lastError" = NULL
               WHERE id = $1"#,
        )
        .bind(&row.id)
        .bind(size)
        .bind(used)
        .bind(&health)
        .bind(memory)
        .bind(cpus)
        .execute(db)
        .await
        .map_err(|e| e.to_string())?;

        // A restarted node's datasets come back locked; the stack can't start until they're reopened.
        let stacks = node.role == Role::Stacks;
        let tenants = sqlx::query_as::<_, TenantRef>(if stacks {
            r#"SELECT t.id, t.slug FROM "Tenant" t JOIN "VolumeKey" v ON v."tenantId" = t.id
               WHERE t."stacksNodeId" = $1 AND t.status = 'ACTIVE'
                 AND EXISTS (SELECT 1 FROM "Stack" s WHERE s."tenantId" = t.id AND s."storageGb" > 0 AND s.status = 'RUNNING')
               ORDER BY t.slug"#
        } else {
            r#"SELECT t.id, t.slug FROM "Tenant" t JOIN "VolumeKey" v ON v."tenantId" = t.id
               WHERE t."nodeId" = $1 AND t.status = 'ACTIVE' ORDER BY t.slug"#
        })
        .bind(&row.id)
        .fetch_all(db)
        .await
        .map_err(|e| e.to_string())?;
        for t in tenants {
            if !valid::slug(&t.slug) || busy(db, &t.id).await? {
                continue;
            }
            let status = if stacks {
                StacksStorage::new(&node, &t.slug)?.status().await?
            } else {
                TenantStorage::new(&node, &t.slug)?.status().await?
            };
            if status == Status::Closed {
                let (what, kind) = if stacks {
                    ("stacks", "STACKS")
                } else {
                    ("storage", "UPDATE")
                };
                eprintln!("[node {}] {}: {what} locked, reopening", row.name, t.slug);
                queue::enqueue(db, &t.id, kind).await?;
            }
        }
        Ok(())
    }
}

/// What `castor-zfs pool`, `castor-info` and Docker say of a node.
#[derive(Debug, Clone)]
pub struct Probed {
    pub pool_size_mb: i32,
    pub pool_used_mb: i32,
    pub pool_health: String,
    pub os: String,
    pub memory_mb: i32,
    pub cpus: i32,
    pub docker: String,
}

pub async fn probe_host(node: &Node) -> Result<Probed, String> {
    let pool = node.host(strings(&["castor-zfs", "pool"]), None).await?;
    let info = node.host(strings(&["castor-info"]), None).await?;
    let (Some((pool_size_mb, pool_used_mb, pool_health)), Some((memory_mb, cpus))) =
        (parse_pool(&pool), parse_info(&info))
    else {
        return Err(format!("unexpected probe output from {}", node.name));
    };
    let docker = node
        .docker(strings(&["version", "--format", "{{.Server.Version}}"]), false)
        .await?
        .trim()
        .to_owned();
    Ok(Probed {
        pool_size_mb,
        pool_used_mb,
        pool_health,
        os: info.split_whitespace().nth(1).unwrap_or("").to_owned(),
        memory_mb,
        cpus,
        docker,
    })
}

/// Pool size, use and health, memory and CPUs. A stacks node is measured by
/// its Docker engine (on a Mac: the VM) and must still isolate containers;
/// the runtime it was registered with is never downgraded here.
async fn probe(node: &Node, row: &NodeRow) -> Result<(i32, i32, String, i32, i32), String> {
    let p = probe_host(node).await?;
    let (mut memory, mut cpus) = (p.memory_mb, p.cpus);
    if node.role == Role::Stacks {
        let f = crate::stacks::edge::check(node, row.stack_runtime.as_deref()).await?;
        memory = i32::try_from(f.memory_mb).map_err(|e| e.to_string())?;
        cpus = i32::try_from(f.cpus).map_err(|e| e.to_string())?;
    }
    Ok((p.pool_size_mb, p.pool_used_mb, p.pool_health, memory, cpus))
}

// ------------------------------------------------------------ measure-storage

pub struct MeasureStorage(pub Arc<Control>);

#[derive(sqlx::FromRow)]
struct Placed {
    id: String,
    slug: String,
    #[sqlx(rename = "nodeId")]
    node_id: String,
}

#[async_trait::async_trait]
impl Task for MeasureStorage {
    fn name(&self) -> &'static str {
        "measure-storage"
    }

    fn every(&self) -> Every {
        Every::Ms(60 * 60_000)
    }

    async fn run(&self, db: &PgPool) -> Result<(), String> {
        let c = &self.0;
        let tenants = sqlx::query_as::<_, Placed>(
            r#"SELECT t.id, t.slug, t."nodeId" FROM "Tenant" t JOIN "VolumeKey" v ON v."tenantId" = t.id
               WHERE t.status = 'ACTIVE' AND t."nodeId" IS NOT NULL ORDER BY t.slug"#,
        )
        .fetch_all(db)
        .await
        .map_err(|e| e.to_string())?;
        for t in tenants {
            let measured = async {
                if !valid::slug(&t.slug) {
                    return Err(format!("invalid tenant slug {:?}", t.slug));
                }
                let node = Node::new(&NodeRow::load(db, &t.node_id).await?, &c.root, c.exec.clone())?;
                crate::storage::measure(db, &t.id, &TenantStorage::new(&node, &t.slug)?).await
            };
            if let Err(e) = measured.await {
                eprintln!("[measure {}] {e}", t.id);
            }
        }
        Ok(())
    }
}

// ------------------------------------------------------------ sweep-deletions

pub struct SweepDeletions;

#[async_trait::async_trait]
impl Task for SweepDeletions {
    fn name(&self) -> &'static str {
        "sweep-deletions"
    }

    fn every(&self) -> Every {
        Every::Ms(60 * 60_000)
    }

    async fn run(&self, db: &PgPool) -> Result<(), String> {
        let due: Vec<String> = sqlx::query_scalar(
            r#"SELECT id FROM "Tenant" WHERE status = 'SUSPENDED' AND "deleteAfter" < now() AT TIME ZONE 'UTC' ORDER BY id"#,
        )
        .fetch_all(db)
        .await
        .map_err(|e| e.to_string())?;
        for id in &due {
            queue::enqueue(db, id, "DELETE").await?;
        }
        if !due.is_empty() {
            eprintln!("scheduled {} deletion(s)", due.len());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_output() {
        assert_eq!(parse_pool("pool 1000 250 ONLINE\n"), Some((1000, 250, "ONLINE".into())));
        assert_eq!(parse_pool("pool x 250 ONLINE"), None);
        assert_eq!(parse_pool("nope"), None);
        assert_eq!(parse_info("info 15.1 16384 8\n"), Some((16384, 8)));
        assert_eq!(parse_info("info 15.1 16384"), None);
    }
}
