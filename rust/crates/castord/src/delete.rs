//! DELETE: the same steps as `Lifecycle.delete` and `Stacks.deleteAll` in
//! apps/provisioner/src. Irreversible: the volume key goes, so every copy of
//! the tenant's data (and its backups) can no longer be read. Each step can
//! run again, so a retry after a failure halfway finishes the job.
//!
//!   1. the tenant is DELETING;
//!   2. custom stacks (when STACKS_DOMAIN is set): routes, containers,
//!      networks, images, the stacks dataset, their sign-in and their rows;
//!   3. the tenant's stack, networks, storage and files on its node;
//!   4. its route, LLM key, volume key, Authentik apps and groups, secrets;
//!   5. the tenant is DELETED, and the audit log says it was crypto-shredded.

use std::sync::Arc;

use serde_json::json;

use crate::db::audit;
use crate::node::{Node, NodeRow};
use crate::storage::{StacksStorage, TenantStorage};
use crate::suspend::{STACKS_EDGE, compose_args, remove_file, strings};
use crate::{Control, Handler, Job, JobCx, valid};

/// The node's shared services, joined to each tenant's shared network (lifecycle.ts).
pub const SHARED_CONTAINERS: [&str; 3] = ["pc-node-immich-ml", "pc-node-tika", "pc-node-gotenberg"];
pub const LLM_GATEWAY: &str = "pc-node-llm-gateway";
/// Services with their own OIDC application (STACK_SERVICES in tenant.ts).
pub const STACK_SERVICES: [&str; 4] = ["photos", "docs", "places", "flights"];

pub struct Delete(pub Arc<Control>);

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
    name: String,
    images: Option<String>,
}

fn sql(e: sqlx::Error) -> String {
    e.to_string()
}

#[async_trait::async_trait]
impl Handler for Delete {
    fn kind(&self) -> &'static str {
        "DELETE"
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
        .map_err(sql)?
        .ok_or_else(|| format!("tenant {} not found", job.tenant_id))?;
        let slug = t.slug.as_str();
        if !valid::slug(slug) {
            return Err(format!("invalid tenant slug {slug:?}"));
        }
        set_status(cx, &t.id, "DELETING").await?;

        // Custom stacks first: their dataset key derives from the volume key deleted below.
        if c.stacks {
            delete_stacks(c, cx, &t).await?;
        }

        // A tenant that failed before placement has nothing on any node.
        if let Some(node_id) = &t.node_id {
            let node = Node::new(&NodeRow::load(db, node_id).await?, &c.root, c.exec.clone())?;
            let project = format!("pc-t-{slug}");
            // Detach the node's shared services, or the tenant network can't be removed.
            for svc in SHARED_CONTAINERS {
                let net = format!("{project}-shared");
                node.docker(strings(&["network", "disconnect", "-f", &net, svc]), true)
                    .await?;
            }
            let llm_net = format!("{project}-llm");
            node.docker(strings(&["network", "disconnect", "-f", &llm_net, LLM_GATEWAY]), true)
                .await?;
            node.docker(
                compose_args(&node, slug, &["down", "--volumes", "--remove-orphans"]),
                false,
            )
            .await?;
            node.docker(strings(&["network", "rm", &format!("{project}-shared")]), true)
                .await?;
            node.docker(strings(&["network", "rm", &llm_net]), true).await?;
            if t.has_volume_key {
                TenantStorage::new(&node, slug)?.destroy().await?;
            }
            node.remove_config(slug).await?;
        }
        remove_file(&c.traefik_dir.join(format!("tenant-{slug}.yml")))?;

        if let Some(gateway) = &c.llm {
            let row: Option<String> = sqlx::query_scalar(
                r#"SELECT ciphertext FROM "Secret" WHERE "tenantId" = $1 AND key = 'LLM_VIRTUAL_KEY'"#,
            )
            .bind(&t.id)
            .fetch_optional(db)
            .await
            .map_err(sql)?;
            if let Some(sealed) = row {
                let key = c
                    .keys
                    .open_secret(&t.id, "LLM_VIRTUAL_KEY", &sealed)
                    .map_err(|e| format!("LLM_VIRTUAL_KEY: {e}"))?;
                gateway.delete(&key).await?;
            }
        }
        // Without the key, any copy of the volume (and its backups) is unreadable.
        sqlx::query(r#"DELETE FROM "VolumeKey" WHERE "tenantId" = $1"#)
            .bind(&t.id)
            .execute(db)
            .await
            .map_err(sql)?;
        for s in STACK_SERVICES {
            c.authentik.delete_oidc_app(&format!("{slug}-{s}")).await?;
        }
        c.authentik.delete_group(&format!("cust-{slug}-owners")).await?;
        c.authentik.delete_group(&format!("cust-{slug}")).await?;
        sqlx::query(r#"DELETE FROM "Secret" WHERE "tenantId" = $1"#)
            .bind(&t.id)
            .execute(db)
            .await
            .map_err(sql)?;

        set_status(cx, &t.id, "DELETED").await?;
        audit(
            db,
            "system",
            "tenant.deleted",
            Some(&t.id),
            Some(json!({ "slug": slug, "cryptoShredded": true })),
        )
        .await?;
        cx.log(job, "deleted").await;
        Ok(())
    }
}

async fn set_status(cx: &JobCx, tenant: &str, status: &str) -> Result<(), String> {
    sqlx::query(
        r#"UPDATE "Tenant" SET status = $2::"TenantStatus", "updatedAt" = now() AT TIME ZONE 'UTC' WHERE id = $1"#,
    )
    .bind(tenant)
    .bind(status)
    .execute(&cx.db)
    .await
    .map_err(sql)?;
    Ok(())
}

async fn delete_stacks(c: &Control, cx: &JobCx, t: &Tenant) -> Result<(), String> {
    let db = &cx.db;
    remove_file(&c.traefik_dir.join(format!("stacks-{}.yml", t.slug)))?;
    let stacks = sqlx::query_as::<_, Stack>(
        r#"SELECT name, images::text AS images FROM "Stack" WHERE "tenantId" = $1 ORDER BY "createdAt", id"#,
    )
    .bind(&t.id)
    .fetch_all(db)
    .await
    .map_err(sql)?;
    for s in &stacks {
        if !valid::stack_name(&s.name) {
            return Err(format!("invalid stack name {:?}", s.name));
        }
    }
    if let Some(node_id) = &t.stacks_node_id {
        let node = Node::new(&NodeRow::load(db, node_id).await?, &c.root, c.exec.clone())?;
        let storage = StacksStorage::new(&node, &t.slug)?;
        for s in &stacks {
            let project = format!("pc-s-{}--{}", t.slug, s.name);
            let route = format!("/routes/{project}.yml");
            node.docker(strings(&["exec", STACKS_EDGE, "rm", "-f", &route]), true)
                .await?;
            let edge = format!("{project}-edge");
            node.docker(strings(&["network", "disconnect", "-f", &edge, STACKS_EDGE]), true)
                .await?;
            node.docker(
                strings(&["compose", "-p", &project, "down", "--volumes", "--remove-orphans"]),
                true,
            )
            .await?;
            // Docker keeps an image another stack still runs.
            for image in images(s.images.as_deref())? {
                node.docker(strings(&["image", "rm", &image]), true).await?;
            }
        }
        if t.has_volume_key {
            storage.destroy().await?;
        }
    }
    for s in &stacks {
        c.authentik
            .delete_proxy_app(&format!("stack-{}--{}", t.slug, s.name))
            .await?;
    }
    sqlx::query(r#"DELETE FROM "Stack" WHERE "tenantId" = $1"#)
        .bind(&t.id)
        .execute(db)
        .await
        .map_err(sql)?;
    Ok(())
}

/// The pinned images of a stack (`images`: service -> reference), each once.
fn images(json: Option<&str>) -> Result<Vec<String>, String> {
    let Some(text) = json else { return Ok(Vec::new()) };
    let v: serde_json::Value = serde_json::from_str(text).map_err(|e| format!("stack images: {e}"))?;
    let mut out: Vec<String> = Vec::new();
    if let Some(map) = v.as_object() {
        for i in map.values().filter_map(|i| i.as_str()) {
            if !valid::image(i) {
                return Err(format!("invalid image reference {i:?}"));
            }
            if !out.iter().any(|o| o == i) {
                out.push(i.to_owned());
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn images_once_and_checked() {
        assert_eq!(images(None).unwrap(), Vec::<String>::new());
        assert_eq!(images(Some("[1]")).unwrap(), Vec::<String>::new());
        let mut got = images(Some(r#"{"web":"a@sha256:1","worker":"a@sha256:1","db":"pg:17"}"#)).unwrap();
        got.sort();
        assert_eq!(got, vec!["a@sha256:1", "pg:17"]);
        assert!(images(Some(r#"{"web":"--rm"}"#)).is_err());
    }
}
