//! Which node a new tenant goes to (placement.ts, Nodes.place in nodes.ts).
//! A tenant's data lives on one node's pool, so this is decided once.

use sqlx::PgPool;

use crate::stacks::spec::{Units, stack_storage_mb};
use crate::storage::quota_mb;

/// Quotas are limits, not usage: sell up to this multiple of the pool.
const POOL_OVERCOMMIT: f64 = 2.0;
/// Never place on a pool that is already this full, whatever the quotas say.
const POOL_MAX_USED: f64 = 0.8;
/// Idle RAM of one tenant stack (shared ML/tika/gotenberg not included).
const STACK_MEMORY_MB: i64 = 3 * 1024;
/// RAM kept for the host OS and the node's shared services.
const NODE_RESERVED_MEMORY_MB: i64 = 8 * 1024;
/// A node that hasn't answered a health check for this long gets no tenants.
const NODE_STALE_SECS: f64 = 15.0 * 60.0;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Candidate {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub role: String,
    pub enabled: bool,
    pub capacity: i32,
    #[sqlx(rename = "poolSizeMb")]
    pub pool_size_mb: Option<i32>,
    #[sqlx(rename = "poolUsedMb")]
    pub pool_used_mb: Option<i32>,
    #[sqlx(rename = "poolHealth")]
    pub pool_health: Option<String>,
    #[sqlx(rename = "memoryMb")]
    pub memory_mb: Option<i32>,
    /// Seconds since the last health check answered, None if never.
    #[sqlx(rename = "seenSecsAgo")]
    pub seen_secs_ago: Option<f64>,
    /// storageUnits of the tenants already there (not deleted).
    pub tenants: Vec<i32>,
}

fn committed_mb(n: &Candidate) -> f64 {
    n.tenants
        .iter()
        .map(|u| quota_mb(u64::try_from(*u).unwrap_or(0)) as f64)
        .sum()
}

/// Why a node can't take a tenant of `storage_units`, or None if it can.
pub fn unfit(n: &Candidate, storage_units: u64) -> Option<String> {
    if n.role == "STACKS" {
        return Some("stacks node".into());
    }
    if !n.enabled {
        return Some("draining".into());
    }
    if n.tenants.len() >= usize::try_from(n.capacity).unwrap_or(0) {
        return Some("at capacity".into());
    }
    if n.kind == "LOCAL" {
        return None; // development host: no pool or health data
    }
    if n.seen_secs_ago.is_none_or(|s| s > NODE_STALE_SECS) {
        return Some("not seen recently".into());
    }
    if n.pool_health.as_deref() != Some("ONLINE") {
        return Some(format!("pool {}", n.pool_health.as_deref().unwrap_or("unknown")));
    }
    let (Some(size), Some(used)) = (n.pool_size_mb.filter(|s| *s > 0), n.pool_used_mb) else {
        return Some("pool not measured".into());
    };
    let size = f64::from(size);
    if f64::from(used) >= size * POOL_MAX_USED {
        return Some("pool almost full".into());
    }
    if committed_mb(n) + quota_mb(storage_units) as f64 > size * POOL_OVERCOMMIT {
        return Some("pool fully booked".into());
    }
    let tenants = i64::try_from(n.tenants.len()).unwrap_or(i64::MAX);
    if let Some(mem) = n.memory_mb
        && NODE_RESERVED_MEMORY_MB + (tenants + 1) * STACK_MEMORY_MB > i64::from(mem)
    {
        return Some("not enough memory".into());
    }
    None
}

/// The fitting node with the most free booked space (share of the pool), so
/// tenants spread out; remote nodes win over the local development host.
/// Err carries why each node was passed over.
pub fn pick(nodes: &[Candidate], storage_units: u64) -> Result<&Candidate, Vec<String>> {
    let mut reasons = Vec::new();
    let mut fitting: Vec<&Candidate> = nodes
        .iter()
        .filter(|n| match unfit(n, storage_units) {
            Some(why) => {
                reasons.push(format!("{}: {why}", n.name));
                false
            }
            None => true,
        })
        .collect();
    let free = |n: &Candidate| {
        if n.kind == "LOCAL" {
            f64::NEG_INFINITY
        } else {
            let size = f64::from(n.pool_size_mb.unwrap_or(1));
            1.0 - committed_mb(n) / (size * POOL_OVERCOMMIT)
        }
    };
    fitting.sort_by(|a, b| {
        free(b)
            .partial_cmp(&free(a))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.tenants.len().cmp(&b.tenants.len()))
    });
    fitting.first().copied().ok_or(reasons)
}

/// Assigns a node to the tenant unless it has one; returns the node's name.
/// Two jobs racing can't place twice: only an unplaced tenant is updated.
pub async fn place(db: &PgPool, tenant_id: &str) -> Result<String, String> {
    let err = |e: sqlx::Error| format!("placement: {e}");
    let (slug, units, node): (String, i32, Option<String>) =
        sqlx::query_as(r#"SELECT slug, "storageUnits", "nodeId" FROM "Tenant" WHERE id = $1"#)
            .bind(tenant_id)
            .fetch_one(db)
            .await
            .map_err(err)?;
    if let Some(id) = node {
        return node_name(db, &id).await;
    }
    let nodes: Vec<Candidate> = sqlx::query_as(
        r#"SELECT n.id, n.name, n.kind::text AS kind, n.role::text AS role, n.enabled, n.capacity,
                  n."poolSizeMb", n."poolUsedMb", n."poolHealth", n."memoryMb",
                  extract(epoch FROM (now() AT TIME ZONE 'UTC') - n."lastSeenAt")::float8 AS "seenSecsAgo",
                  coalesce(array_agg(t."storageUnits") FILTER (WHERE t.id IS NOT NULL), '{}') AS tenants
           FROM "Node" n LEFT JOIN "Tenant" t ON t."nodeId" = n.id AND t.status <> 'DELETED'
           GROUP BY n.id ORDER BY n."createdAt", n.id"#,
    )
    .fetch_all(db)
    .await
    .map_err(err)?;
    let chosen = pick(&nodes, u64::try_from(units).unwrap_or(0)).map_err(|reasons| {
        let why = if reasons.is_empty() {
            "no nodes registered".to_owned()
        } else {
            reasons.join("; ")
        };
        format!("no node can take {slug}: {why}")
    })?;
    let placed = sqlx::query(
        r#"UPDATE "Tenant" SET "nodeId" = $2, "updatedAt" = now() AT TIME ZONE 'UTC' WHERE id = $1 AND "nodeId" IS NULL"#,
    )
    .bind(tenant_id)
    .bind(&chosen.id)
    .execute(db)
    .await
    .map_err(err)?
    .rows_affected();
    if placed == 1 {
        crate::db::audit(
            db,
            "system",
            "tenant.placed",
            Some(tenant_id),
            Some(serde_json::json!({ "node": chosen.name })),
        )
        .await?;
        return Ok(chosen.name.clone());
    }
    let (now,): (Option<String>,) = sqlx::query_as(r#"SELECT "nodeId" FROM "Tenant" WHERE id = $1"#)
        .bind(tenant_id)
        .fetch_one(db)
        .await
        .map_err(err)?;
    node_name(db, &now.ok_or("placement: tenant lost its node")?).await
}

async fn node_name(db: &PgPool, id: &str) -> Result<String, String> {
    sqlx::query_scalar(r#"SELECT name FROM "Node" WHERE id = $1"#)
        .bind(id)
        .fetch_one(db)
        .await
        .map_err(|e| format!("placement: {e}"))
}

// ------------------------------------------------------------ custom stacks

/// Memory of a STACKS node's Docker kept for its edge proxy, the egress proxies and the system.
const STACKS_NODE_RESERVED_MEMORY_MB: i64 = 2 * 1024;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct StacksCandidate {
    pub id: String,
    pub name: String,
    pub role: String,
    pub enabled: bool,
    #[sqlx(rename = "poolSizeMb")]
    pub pool_size_mb: Option<i32>,
    #[sqlx(rename = "poolUsedMb")]
    pub pool_used_mb: Option<i32>,
    #[sqlx(rename = "poolHealth")]
    pub pool_health: Option<String>,
    /// Docker's memory on the node (the VM's on a Mac).
    #[sqlx(rename = "memoryMb")]
    pub memory_mb: Option<i32>,
    #[sqlx(rename = "uidBase")]
    pub uid_base: Option<i32>,
    #[sqlx(rename = "seenSecsAgo")]
    pub seen_secs_ago: Option<f64>,
    /// stackMemoryUnits and stackStorageUnits of the tenants whose stacks are there.
    pub memory_units: Vec<i32>,
    pub storage_units: Vec<i32>,
}

fn stacks_memory_mb(n: &StacksCandidate) -> i64 {
    n.memory_units.iter().map(|u| i64::from(*u) * 256).sum()
}

/// Why a STACKS node can't take a tenant's stacks of `need`, or None.
/// Reserved memory is a hard limit: no overcommit.
pub fn unfit_for_stacks(n: &StacksCandidate, need: Units) -> Option<String> {
    if n.role != "STACKS" {
        return Some("not a stacks node".into());
    }
    if !n.enabled {
        return Some("draining".into());
    }
    if n.uid_base.is_none() {
        return Some("no userns-remap".into());
    }
    if n.seen_secs_ago.is_none_or(|s| s > NODE_STALE_SECS) {
        return Some("not seen recently".into());
    }
    if n.pool_health.as_deref() != Some("ONLINE") {
        return Some(format!("pool {}", n.pool_health.as_deref().unwrap_or("unknown")));
    }
    let (Some(size), Some(used), Some(mem)) = (
        n.pool_size_mb.filter(|s| *s != 0),
        n.pool_used_mb,
        n.memory_mb.filter(|m| *m != 0),
    ) else {
        return Some("not measured".into());
    };
    if f64::from(used) >= f64::from(size) * POOL_MAX_USED {
        return Some("pool almost full".into());
    }
    let memory = stacks_memory_mb(n) + need.memory_units * 256;
    if STACKS_NODE_RESERVED_MEMORY_MB + memory > i64::from(mem) {
        return Some("not enough memory".into());
    }
    let storage: i64 = n
        .storage_units
        .iter()
        .map(|u| stack_storage_mb(i64::from(*u)))
        .sum::<i64>()
        + stack_storage_mb(need.storage_units);
    // Both sides are far below 2^53: exact as f64.
    #[allow(clippy::cast_precision_loss)]
    let storage = storage as f64;
    if storage > f64::from(size) * POOL_OVERCOMMIT {
        return Some("pool fully booked".into());
    }
    None
}

/// The fitting STACKS node with the most free memory; Err carries why each was passed over.
pub fn pick_stacks(nodes: &[StacksCandidate], need: Units) -> Result<&StacksCandidate, Vec<String>> {
    let mut reasons = Vec::new();
    let mut fitting: Vec<&StacksCandidate> = nodes
        .iter()
        .filter(|n| match unfit_for_stacks(n, need) {
            Some(why) => {
                if n.role == "STACKS" {
                    reasons.push(format!("{}: {why}", n.name));
                }
                false
            }
            None => true,
        })
        .collect();
    let free = |n: &StacksCandidate| i64::from(n.memory_mb.unwrap_or(0)) - stacks_memory_mb(n);
    fitting.sort_by_key(|n| std::cmp::Reverse(free(n)));
    fitting.first().copied().ok_or(reasons)
}

/// The tenant's STACKS node id, placed now if it has none.
pub async fn place_stacks(db: &PgPool, tenant_id: &str) -> Result<String, String> {
    let err = |e: sqlx::Error| format!("stacks placement: {e}");
    let (slug, node, need): (String, Option<String>, (i32, i32, i32)) = sqlx::query_as::<_, (String, Option<String>, i32, i32, i32)>(
        r#"SELECT slug, "stacksNodeId", "stackMemoryUnits", "stackCpuUnits", "stackStorageUnits" FROM "Tenant" WHERE id = $1"#,
    )
    .bind(tenant_id)
    .fetch_one(db)
    .await
    .map(|(s, n, m, c, st)| (s, n, (m, c, st)))
    .map_err(err)?;
    if let Some(id) = node {
        return Ok(id);
    }
    let need = Units {
        memory_units: i64::from(need.0),
        cpu_units: i64::from(need.1),
        storage_units: i64::from(need.2),
    };
    let nodes: Vec<StacksCandidate> = sqlx::query_as(
        r#"SELECT n.id, n.name, n.role::text AS role, n.enabled, n."poolSizeMb", n."poolUsedMb", n."poolHealth",
                  n."memoryMb", n."uidBase",
                  extract(epoch FROM (now() AT TIME ZONE 'UTC') - n."lastSeenAt")::float8 AS "seenSecsAgo",
                  coalesce(array_agg(t."stackMemoryUnits") FILTER (WHERE t.id IS NOT NULL), '{}') AS memory_units,
                  coalesce(array_agg(t."stackStorageUnits") FILTER (WHERE t.id IS NOT NULL), '{}') AS storage_units
           FROM "Node" n LEFT JOIN "Tenant" t ON t."stacksNodeId" = n.id AND t.status <> 'DELETED'
           WHERE n.role = 'STACKS'
           GROUP BY n.id ORDER BY n."createdAt", n.id"#,
    )
    .fetch_all(db)
    .await
    .map_err(err)?;
    let chosen = pick_stacks(&nodes, need).map_err(|reasons| {
        let why = if reasons.is_empty() {
            "no stacks nodes registered".to_owned()
        } else {
            reasons.join("; ")
        };
        format!("no stacks node can take {slug}'s stacks: {why}")
    })?;
    // Only if still unplaced: two jobs racing must not place twice.
    let placed = sqlx::query(
        r#"UPDATE "Tenant" SET "stacksNodeId" = $2, "updatedAt" = now() AT TIME ZONE 'UTC' WHERE id = $1 AND "stacksNodeId" IS NULL"#,
    )
    .bind(tenant_id)
    .bind(&chosen.id)
    .execute(db)
    .await
    .map_err(err)?
    .rows_affected();
    if placed == 1 {
        crate::db::audit(
            db,
            "system",
            "stacks.placed",
            Some(tenant_id),
            Some(serde_json::json!({ "node": chosen.name })),
        )
        .await?;
        return Ok(chosen.id.clone());
    }
    let now: Option<String> = sqlx::query_scalar(r#"SELECT "stacksNodeId" FROM "Tenant" WHERE id = $1"#)
        .bind(tenant_id)
        .fetch_one(db)
        .await
        .map_err(err)?;
    now.ok_or_else(|| "stacks placement: tenant lost its stacks node".into())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const GB: i32 = 1024;

    fn mac(name: &str) -> Candidate {
        Candidate {
            id: name.into(),
            name: name.into(),
            kind: "MAC".into(),
            role: "TENANTS".into(),
            enabled: true,
            capacity: 20,
            pool_size_mb: Some(2000 * GB),
            pool_used_mb: Some(100 * GB),
            pool_health: Some("ONLINE".into()),
            memory_mb: Some(64 * GB),
            seen_secs_ago: Some(60.0),
            tenants: vec![],
        }
    }

    fn with(name: &str, f: impl FnOnce(&mut Candidate)) -> Candidate {
        let mut n = mac(name);
        f(&mut n);
        n
    }

    // The cases of placement.test.ts.
    #[test]
    fn picks_the_node_with_the_most_free_booked_space() {
        let a = with("a", |n| n.tenants = vec![10]);
        let b = with("b", |n| n.tenants = vec![2, 2]);
        assert_eq!(pick(&[a, b], 1).unwrap().name, "b");
    }

    #[test]
    fn prefers_remote_nodes_over_the_local_host() {
        let local = with("local", |n| {
            n.kind = "LOCAL".into();
            n.pool_size_mb = None;
            n.pool_used_mb = None;
            n.pool_health = None;
            n.seen_secs_ago = None;
        });
        assert_eq!(pick(&[local.clone(), mac("a")], 1).unwrap().name, "a");
        assert_eq!(pick(&[local], 1).unwrap().name, "local");
    }

    #[test]
    fn skips_unfit_nodes_with_reasons() {
        let nodes = [
            with("draining", |n| n.enabled = false),
            with("stale", |n| n.seen_secs_ago = Some(3600.0)),
            with("degraded", |n| n.pool_health = Some("DEGRADED".into())),
            with("full", |n| n.pool_used_mb = Some(1700 * GB)),
            with("booked", |n| {
                n.pool_size_mb = Some(1000 * GB);
                n.tenants = vec![1; 19];
            }),
            with("capacity", |n| {
                n.capacity = 1;
                n.tenants = vec![1];
            }),
        ];
        assert_eq!(
            pick(&nodes, 2).unwrap_err(),
            vec![
                "draining: draining",
                "stale: not seen recently",
                "degraded: pool DEGRADED",
                "full: pool almost full",
                "booked: pool fully booked",
                "capacity: at capacity",
            ]
        );
    }

    #[test]
    fn memory_limits_the_number_of_stacks() {
        let two = with("m", |n| {
            n.memory_mb = Some(16 * GB);
            n.tenants = vec![1];
        });
        assert_eq!(unfit(&two, 1), None);
        let three = with("m", |n| {
            n.memory_mb = Some(16 * GB);
            n.tenants = vec![1, 1];
        });
        assert_eq!(unfit(&three, 1).unwrap(), "not enough memory");
    }

    #[test]
    fn tenants_never_go_to_a_stacks_node() {
        let s = with("s", |n| n.role = "STACKS".into());
        assert_eq!(unfit(&s, 1).unwrap(), "stacks node");
        assert_eq!(pick(&[s, mac("a")], 1).unwrap().name, "a");
    }

    fn stacks_node(name: &str, f: impl FnOnce(&mut StacksCandidate)) -> StacksCandidate {
        let mut n = StacksCandidate {
            id: name.into(),
            name: name.into(),
            role: "STACKS".into(),
            enabled: true,
            pool_size_mb: Some(1000 * GB),
            pool_used_mb: Some(10 * GB),
            pool_health: Some("ONLINE".into()),
            memory_mb: Some(16 * GB),
            uid_base: Some(100_000),
            seen_secs_ago: Some(60.0),
            memory_units: vec![],
            storage_units: vec![],
        };
        f(&mut n);
        n
    }

    // The stacks cases of placement.test.ts.
    #[test]
    fn stacks_go_to_the_node_with_the_most_free_memory_never_overcommitted() {
        let need = Units {
            memory_units: 8,
            cpu_units: 2,
            storage_units: 1,
        };
        let busy = stacks_node("busy", |n| {
            n.memory_units = vec![40];
            n.storage_units = vec![0];
        });
        let idle = stacks_node("idle", |_| {});
        let tenants = stacks_node("tenants", |n| n.role = "TENANTS".into());
        assert_eq!(pick_stacks(&[busy, idle, tenants], need).unwrap().name, "idle");
        let full = stacks_node("full", |n| {
            n.memory_units = vec![48];
            n.storage_units = vec![0];
        });
        let more = Units {
            memory_units: 9,
            ..need
        };
        assert_eq!(unfit_for_stacks(&full, more).unwrap(), "not enough memory");
        assert_eq!(
            unfit_for_stacks(&stacks_node("x", |n| n.uid_base = None), need).unwrap(),
            "no userns-remap"
        );
        assert_eq!(
            unfit_for_stacks(&stacks_node("t", |n| n.role = "TENANTS".into()), need).unwrap(),
            "not a stacks node"
        );
        assert_eq!(
            pick_stacks(&[stacks_node("s", |n| n.seen_secs_ago = None)], need).unwrap_err(),
            ["s: not seen recently"]
        );
        let booked = stacks_node("b", |n| {
            n.pool_size_mb = Some(10 * GB);
            n.pool_used_mb = Some(GB);
            n.storage_units = vec![2];
        });
        assert_eq!(unfit_for_stacks(&booked, need).unwrap(), "pool fully booked");
    }
}
