//! SUSPEND: stops a tenant and its custom stacks. Every step can run again: a retry after a failure
//! halfway stops what is still running and locks what is still open.
//!
//!   1. custom stacks (when STACKS_DOMAIN is set): the control plane's route
//!      goes, each stack stops and leaves the node's edge, the stacks dataset
//!      is locked;
//!   2. the tenant's stack stops and its storage is locked;
//!   3. the tenant's LLM key is blocked;
//!   4. the tenant is SUSPENDED.

use std::sync::Arc;

use serde_json::json;

use crate::db::audit;
use crate::node::{Node, NodeRow};
use crate::storage::{StacksStorage, Status, TenantStorage};
use crate::{Control, Handler, Job, JobCx, valid};

/// Compose profiles: one per stack service, plus the assistant (ALL_PROFILES).
pub const ALL_PROFILES: [&str; 5] = ["photos", "docs", "places", "flights", "assistant"];
pub use crate::stacks::edge::STACKS_EDGE;

pub struct Suspend(pub Arc<Control>);

#[derive(sqlx::FromRow)]
struct Tenant {
    id: String,
    slug: String,
    #[sqlx(rename = "nodeId")]
    node_id: Option<String>,
    #[sqlx(rename = "stacksNodeId")]
    stacks_node_id: Option<String>,
    has_volume_key: bool,
}

#[derive(sqlx::FromRow)]
struct Stack {
    id: String,
    name: String,
    status: String,
}

#[async_trait::async_trait]
impl Handler for Suspend {
    fn kind(&self) -> &'static str {
        "SUSPEND"
    }

    async fn run(&self, cx: &JobCx, job: &Job) -> Result<(), String> {
        let c = &self.0;
        let db = &cx.db;
        let t = sqlx::query_as::<_, Tenant>(
            r#"SELECT t.id, t.slug, t."nodeId", t."stacksNodeId", (v.id IS NOT NULL) AS has_volume_key
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

        if c.stacks.is_some() {
            stop_stacks(c, cx, &t.id, &t.slug, t.stacks_node_id.as_deref(), t.has_volume_key).await?;
        }

        if let Some(node_id) = &t.node_id {
            let node = Node::new(&NodeRow::load(db, node_id).await?, &c.root, c.exec.clone())?;
            node.docker(compose_args(&node, &t.slug, &ALL_PROFILES, &["stop"]), false)
                .await?;
            if t.has_volume_key {
                TenantStorage::new(&node, &t.slug)?.close().await?;
                audit(
                    db,
                    "system",
                    "volume.lock",
                    Some(&t.id),
                    Some(json!({ "reason": "suspend" })),
                )
                .await?;
            }
        }

        if let Some(gateway) = &c.llm {
            let row: Option<String> = sqlx::query_scalar(
                r#"SELECT ciphertext FROM "Secret" WHERE "tenantId" = $1 AND key = 'LLM_VIRTUAL_KEY'"#,
            )
            .bind(&t.id)
            .fetch_optional(db)
            .await
            .map_err(|e| e.to_string())?;
            if let Some(sealed) = row {
                let key = c
                    .keys
                    .open_secret(&t.id, "LLM_VIRTUAL_KEY", &sealed)
                    .map_err(|e| format!("LLM_VIRTUAL_KEY: {e}"))?;
                gateway.block(&key).await?;
            }
        }

        sqlx::query(
            r#"UPDATE "Tenant" SET status = 'SUSPENDED', "updatedAt" = now() AT TIME ZONE 'UTC' WHERE id = $1"#,
        )
        .bind(&t.id)
        .execute(db)
        .await
        .map_err(|e| e.to_string())?;
        cx.log(job, "suspended").await;
        Ok(())
    }
}

/// Every custom stack stops and the stacks dataset is locked (Stacks.stopAll).
pub(crate) async fn stop_stacks(
    c: &Control,
    cx: &JobCx,
    tenant_id: &str,
    slug: &str,
    stacks_node_id: Option<&str>,
    has_volume_key: bool,
) -> Result<(), String> {
    let db = &cx.db;
    remove_file(&c.traefik_dir.join(format!("stacks-{slug}.yml")))?;
    let Some(node_id) = stacks_node_id else {
        return Ok(());
    };
    let node = Node::new(&NodeRow::load(db, node_id).await?, &c.root, c.exec.clone())?;
    let storage = StacksStorage::new(&node, slug)?;
    let stacks = sqlx::query_as::<_, Stack>(
        r#"SELECT id, name, status::text AS status FROM "Stack" WHERE "tenantId" = $1 ORDER BY "createdAt", id"#,
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await
    .map_err(|e| e.to_string())?;
    for s in stacks {
        if !valid::stack_name(&s.name) {
            return Err(format!("invalid stack name {:?}", s.name));
        }
        let project = format!("pc-s-{}--{}", slug, s.name);
        node.docker(strings(&["compose", "-p", &project, "stop"]), true).await?;
        let route = format!("/routes/{project}.yml");
        node.docker(strings(&["exec", STACKS_EDGE, "rm", "-f", &route]), true)
            .await?;
        if s.status != "REMOVING" {
            sqlx::query(
                r#"UPDATE "Stack" SET status = 'STOPPED', error = NULL, "updatedAt" = now() AT TIME ZONE 'UTC' WHERE id = $1"#,
            )
            .bind(&s.id)
            .execute(db)
            .await
            .map_err(|e| e.to_string())?;
        }
    }
    if has_volume_key && matches!(storage.status().await?, Status::Open { .. }) {
        storage.close().await?;
        audit(
            db,
            "system",
            "stacks.lock",
            Some(tenant_id),
            Some(json!({ "reason": "suspend" })),
        )
        .await?;
    }
    Ok(())
}

/// `docker compose` for the tenant's stack (Lifecycle.compose) with these profiles.
/// Through gatekeeper the booked profiles come from COMPOSE_PROFILES in
/// tenant.env (a tool's argv is fixed); only "every profile" is spelled out.
pub fn compose_args(node: &Node, slug: &str, profiles: &[&str], args: &[&str]) -> Vec<String> {
    let dir = node.config_dir(slug);
    let profiles = if node.is_gatekeeper() && profiles != ALL_PROFILES {
        &[][..]
    } else {
        profiles
    };
    let mut out = strings(&[
        "compose",
        "-p",
        &format!("pc-t-{slug}"),
        "-f",
        &format!("{}/tenant/compose.yml", node.templates()),
        // Relative bind mounts (skin config, db init) resolve on the node.
        "--project-directory",
        &format!("{}/tenant", node.node_templates()),
        "-f",
        &format!("{dir}/compose.volumes.yml"),
        "--env-file",
        &format!("{dir}/tenant.env"),
    ]);
    for p in profiles {
        out.push("--profile".into());
        out.push((*p).into());
    }
    out.extend(strings(args));
    out
}

pub(crate) fn strings(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| (*s).to_owned()).collect()
}

pub(crate) fn remove_file(path: &std::path::Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("{}: {e}", path.display())),
        _ => Ok(()),
    }
}
