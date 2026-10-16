//! STACKS: one idempotent job converges a tenant's custom stacks on its
//! STACKS node to what the database says (Stacks.apply in
//! apps/provisioner/src/stacks.ts): deploy new and changed ones, stop what
//! the booking no longer covers, remove what the owner removed. Nothing from
//! the customer is trusted: the stored definition is checked again here,
//! images are pulled and pinned to a digest, and the compose file is
//! rendered by castord itself.

use std::sync::Arc;

use serde_json::{Value, json};

use super::edge::{self, EGRESS_IMAGE};
use super::render::{self, DEFAULT_TMPFS};
use super::spec::{self, Spec, Units, stack_storage_mb};
use super::{IdFiles, Platform, covered, image, js, needs_names, resolve_user, uncovered_volumes, untar_file};
use crate::authentik::{ProviderDefaults, ProxyApp};
use crate::db::{audit, new_id};
use crate::node::{Node, NodeRow, Role, parse_edge};
use crate::storage::{StacksStorage, VolumeKey};
use crate::suspend::{remove_file, stop_stacks, strings};
use crate::{Control, Handler, Job, JobCx, placement, valid};

/// Biggest image a stack may pull (compressed layers are bigger on disk).
pub const MAX_IMAGE_BYTES: u64 = 4 * 1024 * 1024 * 1024;

fn sql(e: sqlx::Error) -> String {
    e.to_string()
}

pub fn definition_key(stack: &str) -> String {
    format!("stack-definition:{stack}")
}

pub fn secret_key(stack: &str, name: &str) -> String {
    format!("stack:{stack}:{name}")
}

#[derive(sqlx::FromRow)]
struct Tenant {
    id: String,
    slug: String,
    status: String,
    #[sqlx(rename = "stacksNodeId")]
    stacks_node_id: Option<String>,
    #[sqlx(rename = "stackMemoryUnits")]
    memory_units: i32,
    #[sqlx(rename = "stackCpuUnits")]
    cpu_units: i32,
    #[sqlx(rename = "stackStorageUnits")]
    storage_units: i32,
    wrapped: Option<String>,
}

#[derive(sqlx::FromRow)]
struct Stack {
    id: String,
    name: String,
    status: String,
    definition: String,
    #[sqlx(rename = "storageGb")]
    storage_gb: i32,
    images: Option<String>,
}

pub struct Stacks(pub Arc<Control>);

#[async_trait::async_trait]
impl Handler for Stacks {
    fn kind(&self) -> &'static str {
        "STACKS"
    }

    /// Per-stack failures are recorded on the stack; node trouble fails the job.
    async fn run(&self, cx: &JobCx, job: &Job) -> Result<(), String> {
        let c = &self.0;
        let p = c.stacks.as_ref().ok_or("STACKS_DOMAIN is not set")?;
        let db = &cx.db;
        let t = sqlx::query_as::<_, Tenant>(
            r#"SELECT t.id, t.slug, t.status::text AS status, t."stacksNodeId", t."stackMemoryUnits",
                      t."stackCpuUnits", t."stackStorageUnits", v."wrappedKey" AS wrapped
               FROM "Tenant" t LEFT JOIN "VolumeKey" v ON v."tenantId" = t.id WHERE t.id = $1"#,
        )
        .bind(&job.tenant_id)
        .fetch_optional(db)
        .await
        .map_err(sql)?
        .ok_or_else(|| format!("tenant {} not found", job.tenant_id))?;
        if !valid::slug(&t.slug) {
            return Err(format!("invalid tenant slug {:?}", t.slug));
        }
        if t.status != "ACTIVE" {
            return stop_stacks(c, cx, &t.id, &t.slug, t.stacks_node_id.as_deref(), t.wrapped.is_some()).await;
        }
        let stacks = sqlx::query_as::<_, Stack>(
            r#"SELECT id, name, status::text AS status, definition, "storageGb", images::text AS images
               FROM "Stack" WHERE "tenantId" = $1 ORDER BY "createdAt", id"#,
        )
        .bind(&t.id)
        .fetch_all(db)
        .await
        .map_err(sql)?;
        let (removing, wanted): (Vec<Stack>, Vec<Stack>) = stacks.into_iter().partition(|s| s.status == "REMOVING");
        if wanted.is_empty() && t.stacks_node_id.is_none() {
            for s in &removing {
                delete_row(cx, &s.id).await?;
            }
            return Ok(());
        }

        let node_id = placement::place_stacks(db, &t.id).await?;
        let row = NodeRow::load(db, &node_id).await?;
        let node = Node::new(&row, &c.root, c.exec.clone())?;
        if node.role != Role::Stacks || node.is_local() {
            return Err(format!("{} is not a stacks node", node.name));
        }
        crate::apply::prepare_once(c, &node).await?;
        cx.report(job, "stacks", &format!("custom stacks on {}", node.name))
            .await?;
        for s in &removing {
            remove(c, cx, &node, &t, s).await?;
        }

        // Check first: a stored definition that no longer passes is a failed stack, not a failed job.
        let mut parsed = Vec::new();
        for s in wanted {
            match definition(c, &t.id, &s) {
                Ok(spec) => {
                    let units = spec.units();
                    parsed.push(((s, spec), units));
                }
                Err(e) => fail(cx, &s, &e).await?,
            }
        }
        let booked = Units {
            memory_units: i64::from(t.memory_units),
            cpu_units: i64::from(t.cpu_units),
            storage_units: i64::from(t.storage_units),
        };
        let (ok, over) = covered(parsed, booked);
        for (s, _) in &over {
            stop(cx, &node, &t, s, "not covered by the booked resources").await?;
        }

        let members = c.authentik.ensure_group(&format!("cust-{}", t.slug)).await?;
        let defaults = c.authentik.provider_defaults().await?;
        let mut storage = None;
        if ok.iter().any(|(_, spec)| spec.storage_gb > 0.0) {
            let wrapped = t.wrapped.as_deref().ok_or("tenant has no volume key yet")?;
            let s = StacksStorage::new(&node, &t.slug)?.with_key(VolumeKey {
                keys: &c.keys,
                tenant_id: &t.id,
                wrapped,
            });
            let units = u64::try_from(t.storage_units.max(1)).unwrap_or(1);
            s.open(u64::try_from(stack_storage_mb(i64::try_from(units).unwrap_or(1))).unwrap_or(0))
                .await?;
            audit(
                db,
                "system",
                "stacks.unlock",
                Some(&t.id),
                Some(json!({ "node": node.name })),
            )
            .await?;
            storage = Some(s);
        }

        let n = ok.len();
        for (s, spec) in &ok {
            sqlx::query(
                r#"UPDATE "Stack" SET status = 'DEPLOYING', error = NULL, "updatedAt" = now() AT TIME ZONE 'UTC' WHERE id = $1"#,
            )
            .bind(&s.id)
            .execute(db)
            .await
            .map_err(sql)?;
            let d = Deploy {
                c,
                p,
                cx,
                node: &node,
                row: &row,
                t: &t,
                storage: storage.as_ref(),
                members: &members,
                defaults: &defaults,
            };
            if let Err(e) = d.deploy(s, spec).await {
                fail(cx, s, &e).await?;
                audit(
                    db,
                    "system",
                    "stack.failed",
                    Some(&t.id),
                    Some(json!({ "stack": s.name, "error": js::slice(&e, 300) })),
                )
                .await?;
            }
        }
        control_plane_route(c, p, cx, &t.id, &t.slug).await?;
        cx.report(job, "stacks", &format!("{n} stack(s) applied")).await?;
        Ok(())
    }
}

/// The stored definition, opened and checked again.
fn definition(c: &Control, tenant_id: &str, s: &Stack) -> Result<Spec, String> {
    if !valid::stack_name(&s.name) {
        return Err("invalid stack name".into());
    }
    let text = c
        .keys
        .open_secret(tenant_id, &definition_key(&s.name), &s.definition)
        .map_err(|e| format!("stack definition: {e}"))?;
    spec::parse_stored(&text).map_err(|e| e.to_string())
}

async fn fail(cx: &JobCx, s: &Stack, error: &str) -> Result<(), String> {
    eprintln!("[stack {}] {error}", s.name);
    sqlx::query(
        r#"UPDATE "Stack" SET status = 'FAILED', error = $2, "updatedAt" = now() AT TIME ZONE 'UTC' WHERE id = $1"#,
    )
    .bind(&s.id)
    .bind(js::slice(error, 2000))
    .execute(&cx.db)
    .await
    .map_err(sql)?;
    Ok(())
}

async fn delete_row(cx: &JobCx, id: &str) -> Result<(), String> {
    sqlx::query(r#"DELETE FROM "Stack" WHERE id = $1"#)
        .bind(id)
        .execute(&cx.db)
        .await
        .map_err(sql)?;
    Ok(())
}

/// Drops pinned images; Docker keeps any another stack still runs.
async fn remove_images(node: &Node, images: &[String]) -> Result<(), String> {
    let mut seen: Vec<&str> = Vec::new();
    for i in images {
        if seen.contains(&i.as_str()) {
            continue;
        }
        seen.push(i);
        node.docker(strings(&["image", "rm", i]), true).await?;
    }
    Ok(())
}

/// Containers stop, data stays.
async fn stop(cx: &JobCx, node: &Node, t: &Tenant, s: &Stack, reason: &str) -> Result<(), String> {
    let n = render::names(&t.slug, &s.name);
    node.docker(strings(&["compose", "-p", &n.project, "stop"]), true)
        .await?;
    edge::remove_route(node, &n.project).await?;
    sqlx::query(
        r#"UPDATE "Stack" SET status = 'STOPPED', error = $2, "updatedAt" = now() AT TIME ZONE 'UTC' WHERE id = $1"#,
    )
    .bind(&s.id)
    .bind(reason)
    .execute(&cx.db)
    .await
    .map_err(sql)?;
    Ok(())
}

/// The owner removed it: containers, networks, route, sign-in, secrets and data go.
async fn remove(c: &Control, cx: &JobCx, node: &Node, t: &Tenant, s: &Stack) -> Result<(), String> {
    if !valid::stack_name(&s.name) {
        return Err(format!("invalid stack name {:?}", s.name));
    }
    let n = render::names(&t.slug, &s.name);
    edge::remove_route(node, &n.project).await?;
    edge::disconnect(node, &n.edge_network).await?;
    node.docker(
        strings(&["compose", "-p", &n.project, "down", "--volumes", "--remove-orphans"]),
        true,
    )
    .await?;
    remove_images(node, &crate::delete::images(s.images.as_deref())?).await?;
    if s.storage_gb > 0 && t.wrapped.is_some() {
        StacksStorage::new(node, &t.slug)?.destroy_stack(&s.name).await?;
    }
    node.remove_local_config(&t.slug, &format!("stacks/{}", s.name))?;
    c.authentik.delete_proxy_app(&n.auth_slug).await?;
    sqlx::query(r#"DELETE FROM "Secret" WHERE "tenantId" = $1 AND key LIKE $2"#)
        .bind(&t.id)
        .bind(format!("stack:{}:%", s.name))
        .execute(&cx.db)
        .await
        .map_err(sql)?;
    delete_row(cx, &s.id).await?;
    audit(
        &cx.db,
        "system",
        "stack.removed",
        Some(&t.id),
        Some(json!({ "stack": s.name })),
    )
    .await
}

/// The control plane's routes for the tenant's running stacks.
async fn control_plane_route(c: &Control, p: &Platform, cx: &JobCx, tenant_id: &str, slug: &str) -> Result<(), String> {
    let file = c.traefik_dir.join(format!("stacks-{slug}.yml"));
    let running: Vec<String> = sqlx::query_scalar(
        r#"SELECT name FROM "Stack" WHERE "tenantId" = $1 AND status = 'RUNNING' ORDER BY "createdAt", id"#,
    )
    .bind(tenant_id)
    .fetch_all(&cx.db)
    .await
    .map_err(sql)?;
    let edge_host: Option<String> = sqlx::query_scalar(
        r#"SELECT n."edgeHost" FROM "Tenant" t JOIN "Node" n ON n.id = t."stacksNodeId" WHERE t.id = $1"#,
    )
    .bind(tenant_id)
    .fetch_optional(&cx.db)
    .await
    .map_err(sql)?
    .flatten()
    .filter(|h: &String| !h.is_empty());
    let Some(edge_host) = edge_host.filter(|_| !running.is_empty()) else {
        return remove_file(&file);
    };
    let edge = parse_edge(&edge_host)?;
    let stacks: Vec<(String, String)> = running
        .iter()
        .map(|s| (s.clone(), render::host(slug, s, &p.domain)))
        .collect();
    let routes = render::control_plane_routes(slug, &stacks, &edge.url, p.cert_resolver.as_deref(), &p.domain);
    crate::apply::write_route(&file, &routes)
}

struct Pulled {
    image: String,
    user: String,
    volumes: Vec<String>,
}

struct Deploy<'a> {
    c: &'a Control,
    p: &'a Platform,
    cx: &'a JobCx,
    node: &'a Node,
    row: &'a NodeRow,
    t: &'a Tenant,
    storage: Option<&'a StacksStorage<'a>>,
    members: &'a str,
    defaults: &'a ProviderDefaults,
}

impl Deploy<'_> {
    fn origin(&self, stack: &str) -> String {
        format!(
            "{}://{}",
            self.p.scheme,
            render::host(&self.t.slug, stack, &self.p.domain)
        )
    }

    /// Pulls the image on the node and pins it to the digest it got.
    async fn pull(&self, img: &str) -> Result<Pulled, String> {
        let r = image::parse(img)?;
        let node = self.node;
        node.docker(strings(&["pull", "--quiet", &r.canonical]), false).await?;
        let out = node
            .docker(
                strings(&["image", "inspect", "--format", "{{json .}}", &r.canonical]),
                false,
            )
            .await?;
        let info: Value = serde_json::from_str(&out).map_err(|e| format!("{}: image inspect: {e}", r.canonical))?;
        if info["Size"].as_u64().unwrap_or(0) > MAX_IMAGE_BYTES {
            node.docker(strings(&["image", "rm", &r.canonical]), true).await?;
            return Err(format!("{} is larger than {} GB", r.canonical, MAX_IMAGE_BYTES >> 30));
        }
        let repo_digests: Vec<String> = info["RepoDigests"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
            .unwrap_or_default();
        let digest = r
            .digest
            .clone()
            .or_else(|| image::digest_for(&r, &repo_digests))
            .ok_or_else(|| format!("no digest for {}", r.canonical))?;
        Ok(Pulled {
            image: image::pinned(&r, &digest)?,
            user: info["Config"]["User"].as_str().unwrap_or("").to_owned(),
            volumes: info["Config"]["Volumes"]
                .as_object()
                .map(|v| v.keys().cloned().collect())
                .unwrap_or_default(),
        })
    }

    /// /etc/passwd and /etc/group of an image, read without running it.
    async fn image_ids(&self, img: &str) -> Result<(String, String), String> {
        let mut b = [0u8; 4];
        getrandom::fill(&mut b).map_err(|e| format!("random name: {e}"))?;
        let name = format!("pc-s-ids-{}", hex::encode(b));
        let node = self.node;
        // Never started; the entrypoint only keeps `create` from failing on images without a CMD.
        node.docker(
            strings(&[
                "create",
                "--name",
                &name,
                "--network",
                "none",
                "--entrypoint",
                "/nonexistent",
                img,
            ]),
            false,
        )
        .await?;
        let mut files = Vec::new();
        for f in ["/etc/passwd", "/etc/group"] {
            let tar = node.docker_bytes(strings(&["cp", &format!("{name}:{f}"), "-"])).await;
            files.push(tar.and_then(|t| untar_file(&t)).unwrap_or_default());
        }
        node.docker(strings(&["rm", "-f", &name]), true).await?;
        let group = files.pop().unwrap_or_default();
        Ok((files.pop().unwrap_or_default(), group))
    }

    async fn secrets(&self, stack: &str, names: &[String]) -> Result<Vec<(String, String)>, String> {
        let (c, db, tenant) = (self.c, &self.cx.db, &self.t.id);
        let mut out = Vec::new();
        for name in names {
            let key = secret_key(stack, name);
            let sealed: Option<String> =
                sqlx::query_scalar(r#"SELECT ciphertext FROM "Secret" WHERE "tenantId" = $1 AND key = $2"#)
                    .bind(tenant)
                    .bind(&key)
                    .fetch_optional(db)
                    .await
                    .map_err(sql)?;
            let value = match sealed {
                Some(s) => c
                    .keys
                    .open_secret(tenant, &key, &s)
                    .map_err(|e| format!("secret {key}: {e}"))?
                    .to_string(),
                None => {
                    let mut b = [0u8; 24];
                    getrandom::fill(&mut b).map_err(|e| format!("random secret: {e}"))?;
                    let v = hex::encode(b);
                    let sealed = c
                        .keys
                        .seal_secret(tenant, &key, &v)
                        .map_err(|e| format!("secret {key}: {e}"))?;
                    sqlx::query(
                        r#"INSERT INTO "Secret" (id, "tenantId", key, ciphertext, "createdAt", "updatedAt")
                           VALUES ($1, $2, $3, $4, now() AT TIME ZONE 'UTC', now() AT TIME ZONE 'UTC')"#,
                    )
                    .bind(new_id()?)
                    .bind(tenant)
                    .bind(&key)
                    .bind(sealed)
                    .execute(db)
                    .await
                    .map_err(sql)?;
                    v
                }
            };
            out.push((name.clone(), value));
        }
        Ok(out)
    }

    async fn deploy(&self, s: &Stack, spec: &Spec) -> Result<(), String> {
        let (node, row, slug) = (self.node, self.row, self.t.slug.as_str());
        let n = render::names(slug, &s.name);
        let host = render::host(slug, &s.name, &self.p.domain);
        let base = i64::from(
            row.uid_base
                .ok_or_else(|| format!("{} has no userns uid base", row.name))?,
        );
        let mapped =
            |id: u32| u32::try_from(base + i64::from(id)).map_err(|_| format!("{}: uid base out of range", row.name));

        // Images, pinned; data directories owned by the (userns-mapped) container user.
        let mut images: Vec<(String, String)> = Vec::new();
        let mut owners: Vec<(String, u32, u32)> = Vec::new();
        for (name, svc) in &spec.services {
            let pulled = self.pull(&svc.image).await?;
            images.push((name.clone(), pulled.image.clone()));
            // Docker would back an image's VOLUME with an anonymous volume: writable, outside the quota.
            let mut mounts: Vec<&str> = svc.volumes.iter().map(|m| m.target.as_str()).collect();
            mounts.extend(svc.tmpfs.iter().map(String::as_str));
            mounts.extend(DEFAULT_TMPFS);
            let open = uncovered_volumes(&pulled.volumes, &mounts);
            if !open.is_empty() {
                return Err(format!(
                    "{name}: the image declares {}; mount a volume or a tmpfs there",
                    open.join(", ")
                ));
            }
            let user = svc.user.clone().unwrap_or(pulled.user);
            if svc.volumes.is_empty() {
                continue;
            }
            let files = if needs_names(&user) {
                Some(self.image_ids(&pulled.image).await?)
            } else {
                None
            };
            let ids = files.as_ref().map(|(passwd, group)| IdFiles { passwd, group });
            let (uid, gid) = resolve_user(&user, ids.as_ref())?;
            for m in &svc.volumes {
                let o = (m.volume.clone(), mapped(uid)?, mapped(gid)?);
                match owners.iter_mut().find(|(v, ..)| *v == m.volume) {
                    Some(e) => *e = o,
                    None => owners.push(o),
                }
            }
        }
        for v in &spec.volumes {
            if !owners.iter().any(|(o, ..)| o == v) {
                owners.push((v.clone(), mapped(0)?, mapped(0)?));
            }
        }

        if spec.storage_gb > 0.0 {
            let storage = self.storage.ok_or("stacks dataset not open")?;
            let owners: Vec<(&str, u32, u32)> = owners.iter().map(|(v, u, g)| (v.as_str(), *u, *g)).collect();
            // Whole GB, at most 2000 (checked).
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let quota = spec.storage_gb as u64 * 1024;
            storage.create_stack(&s.name, quota, &owners).await?;
        }

        // Checked again right before anything runs: no stack ever starts
        // without userns-remap or below its registered sandbox.
        edge::check(node, row.stack_runtime.as_deref()).await?;

        let origin = self.origin(&s.name);
        self.c
            .authentik
            .ensure_proxy_app(
                &ProxyApp {
                    slug: &n.auth_slug,
                    name: &format!("{} ({slug})", s.name),
                    external_host: &origin,
                    allowed_group_pk: self.members,
                },
                self.defaults,
            )
            .await?;

        let data_dir = match self.storage {
            Some(st) => st.data_dir(&s.name)?,
            None => "/nonexistent".to_owned(),
        };
        let secrets = self.secrets(&s.name, &spec.secret_names()).await?;
        let compose = render::compose(
            spec,
            &render::Context {
                slug,
                stack: &s.name,
                origin: &origin,
                data_dir: &data_dir,
                images: &images,
                secrets: &secrets,
                runtime: (row.stack_runtime.as_deref() == Some("runsc")).then_some("runsc"),
                egress_image: EGRESS_IMAGE,
            },
        )?;
        let text = serde_json::to_string_pretty(&compose).map_err(|e| e.to_string())?;
        let file = node.write_config(slug, &format!("stacks/{}/compose.json", s.name), &text)?;
        node.docker(
            strings(&[
                "compose",
                "-p",
                &n.project,
                "-f",
                &file,
                "up",
                "-d",
                "--remove-orphans",
                "--pull",
                "never",
                "--wait",
                "--wait-timeout",
                "300",
            ]),
            false,
        )
        .await?;
        edge::connect(node, &n.edge_network).await?;
        edge::put_route(
            node,
            &n.project,
            &render::node_route(slug, &s.name, &host, spec.web.port),
        )
        .await?;

        let pinned: serde_json::Map<String, Value> = images
            .iter()
            .map(|(k, v)| (k.clone(), Value::from(v.clone())))
            .collect();
        sqlx::query(
            r#"UPDATE "Stack" SET status = 'RUNNING', error = NULL, images = $2::jsonb,
                      "deployedAt" = now() AT TIME ZONE 'UTC', "updatedAt" = now() AT TIME ZONE 'UTC' WHERE id = $1"#,
        )
        .bind(&s.id)
        .bind(Value::Object(pinned.clone()).to_string())
        .execute(&self.cx.db)
        .await
        .map_err(sql)?;
        // Old versions take disk outside every quota.
        let old: Vec<String> = crate::delete::images(s.images.as_deref())?
            .into_iter()
            .filter(|i| !images.iter().any(|(_, n)| n == i))
            .collect();
        remove_images(node, &old).await?;
        audit(
            &self.cx.db,
            "system",
            "stack.deployed",
            Some(&self.t.id),
            Some(json!({ "stack": s.name, "images": pinned, "node": row.name })),
        )
        .await
    }
}
