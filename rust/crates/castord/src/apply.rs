//! PROVISION, UPDATE and RESUME: make the tenant's stack match the booking,
//! as `Lifecycle.apply` in apps/provisioner/src/lifecycle.ts. Every step is
//! idempotent, so a retried or repeated job converges to what the database
//! says the customer has booked.
//!
//!   identity  Authentik groups, members and one OIDC app per booked service
//!   storage   the volume key, the encrypted storage opened and sized
//!   services  config files, containers, routes, health, service accounts,
//!             pending revocations
//!   ready     the tenant is ACTIVE and its storage measured

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use zeroize::Zeroizing;

use crate::assistant::{self, Member, MemberSecrets};
use crate::authentik::OidcApp;
use crate::db::{audit, new_id};
use crate::delete::{LLM_GATEWAY, SHARED_CONTAINERS};
use crate::node::{Node, NodeRow};
use crate::render::{self, ASSISTANT_CONTAINERS, EnvInput, Names, STACK_SERVICES, Service, Vars};
use crate::storage::{Status, TenantStorage, VolumeKey, fits, quota_mb};
use crate::suspend::{compose_args, strings};
use crate::{Control, Handler, Job, JobCx, placement, queue, valid};

fn sql(e: sqlx::Error) -> String {
    e.to_string()
}

#[derive(sqlx::FromRow)]
struct Tenant {
    id: String,
    slug: String,
    locale: String,
    #[sqlx(rename = "nodeId")]
    node_id: Option<String>,
    #[sqlx(rename = "storageUnits")]
    storage_units: i32,
    #[sqlx(rename = "aiCreditChf")]
    ai_credit_chf: i32,
}

#[derive(sqlx::FromRow)]
struct MemberRow {
    id: String,
    role: String,
    email: String,
    #[sqlx(rename = "authentikSub")]
    sub: String,
    name: Option<String>,
    locale: Option<String>,
}

pub struct Provision(pub Arc<Control>);
pub struct Update(pub Arc<Control>);
pub struct Resume(pub Arc<Control>);

#[async_trait::async_trait]
impl Handler for Provision {
    fn kind(&self) -> &'static str {
        "PROVISION"
    }

    async fn run(&self, cx: &JobCx, job: &Job) -> Result<(), String> {
        let node = placement::place(&cx.db, &job.tenant_id).await?;
        cx.log(job, &format!("placed on node {node}")).await;
        apply(&self.0, cx, job).await
    }

    async fn gave_up(&self, cx: &JobCx, job: &Job, error: &str) {
        let r = async {
            sqlx::query(
                r#"UPDATE "Tenant" SET status = 'FAILED', "updatedAt" = now() AT TIME ZONE 'UTC' WHERE id = $1"#,
            )
            .bind(&job.tenant_id)
            .execute(&cx.db)
            .await
            .map_err(sql)?;
            let error: String = error.chars().take(500).collect();
            audit(
                &cx.db,
                "system",
                "tenant.provision_failed",
                Some(&job.tenant_id),
                Some(json!({ "error": error })),
            )
            .await
        };
        if let Err(e) = r.await {
            eprintln!("[PROVISION {}] marking the tenant failed: {e}", job.tenant_id);
        }
    }
}

#[async_trait::async_trait]
impl Handler for Update {
    fn kind(&self) -> &'static str {
        "UPDATE"
    }

    async fn run(&self, cx: &JobCx, job: &Job) -> Result<(), String> {
        placement::place(&cx.db, &job.tenant_id).await?; // a no-op once placed
        apply(&self.0, cx, job).await?;
        follow_with_stacks(&self.0, cx, &job.tenant_id).await
    }
}

#[async_trait::async_trait]
impl Handler for Resume {
    fn kind(&self) -> &'static str {
        "RESUME"
    }

    async fn run(&self, cx: &JobCx, job: &Job) -> Result<(), String> {
        placement::place(&cx.db, &job.tenant_id).await?;
        apply(&self.0, cx, job).await?;
        follow_with_stacks(&self.0, cx, &job.tenant_id).await
    }
}

/// After the tenant's own stack, its custom stacks, in their own job: a
/// stacks node outage never fails the tenant's.
async fn follow_with_stacks(c: &Control, cx: &JobCx, tenant_id: &str) -> Result<(), String> {
    if !c.stacks {
        return Ok(());
    }
    let has: bool = sqlx::query_scalar(
        r#"SELECT t."stacksNodeId" IS NOT NULL OR EXISTS (SELECT 1 FROM "Stack" s WHERE s."tenantId" = t.id)
           FROM "Tenant" t WHERE t.id = $1"#,
    )
    .bind(tenant_id)
    .fetch_one(&cx.db)
    .await
    .map_err(sql)?;
    if has {
        queue::enqueue(&cx.db, tenant_id, "STACKS").await?;
    }
    Ok(())
}

async fn load(cx: &JobCx, tenant_id: &str) -> Result<Tenant, String> {
    let t = sqlx::query_as::<_, Tenant>(
        r#"SELECT id, slug, locale, "nodeId", "storageUnits", "aiCreditChf" FROM "Tenant" WHERE id = $1"#,
    )
    .bind(tenant_id)
    .fetch_optional(&cx.db)
    .await
    .map_err(sql)?
    .ok_or_else(|| format!("tenant {tenant_id} not found"))?;
    if !valid::slug(&t.slug) {
        return Err(format!("invalid tenant slug {:?}", t.slug));
    }
    Ok(t)
}

pub async fn apply(c: &Control, cx: &JobCx, job: &Job) -> Result<(), String> {
    let db = &cx.db;
    let t = load(cx, &job.tenant_id).await?;
    let node_id = t.node_id.as_deref().ok_or("tenant has no node yet")?;
    let node = Node::new(&NodeRow::load(db, node_id).await?, &c.root, c.exec.clone())?;
    prepare_once(c, &node).await?;
    let n = Names::new(&t.slug, &c.platform);

    let booked: Vec<String> =
        sqlx::query_scalar(r#"SELECT lower(kind::text) FROM "ServiceInstance" WHERE "tenantId" = $1 AND enabled"#)
            .bind(&t.id)
            .fetch_all(db)
            .await
            .map_err(sql)?;
    let enabled: Vec<Service> = STACK_SERVICES
        .into_iter()
        .filter(|s| booked.iter().any(|b| b == s.key()))
        .collect();
    let keys: Vec<&str> = enabled.iter().map(|s| s.key()).collect();
    let with_assistant = booked.iter().any(|b| b == "assistant");
    if with_assistant && c.llm.is_none() {
        return Err("assistant booked but the LLM gateway is not configured".into());
    }
    let mut profiles = keys.clone();
    if with_assistant {
        profiles.push("assistant");
    }
    let members = sqlx::query_as::<_, MemberRow>(
        r#"SELECT m.id, m.role::text AS role, u.email, u."authentikSub", u.name, u.locale
           FROM "Member" m JOIN "User" u ON u.id = m."userId" WHERE m."tenantId" = $1 ORDER BY m."createdAt", m.id"#,
    )
    .bind(&t.id)
    .fetch_all(db)
    .await
    .map_err(sql)?;

    // ------------------------------------------------------------ identity
    cx.report(job, "identity", "authentik groups and apps").await?;
    let ak = &c.authentik;
    let members_pk = ak.ensure_group(&n.members_group()).await?;
    let owners_pk = ak.ensure_group(&n.owners_group()).await?;
    for m in &members {
        // Signed up through the portal, so normally there.
        let Some(user_pk) = ak.find_user_pk_by_email(&m.email).await? else {
            continue;
        };
        ak.add_user_to_group(&members_pk, user_pk).await?;
        if m.role == "OWNER" {
            ak.add_user_to_group(&owners_pk, user_pk).await?;
        }
    }
    remove_former_members(c, cx, &t, &members, &members_pk, &owners_pk).await?;

    let secrets = tenant_secrets(c, cx, &t.id).await?;
    let defaults = ak.provider_defaults().await?;
    for s in STACK_SERVICES {
        let slug = n.app_slug(s);
        if !enabled.contains(&s) {
            ak.delete_oidc_app(&slug).await?;
            continue;
        }
        let secret_key = format!("{}_OIDC_CLIENT_SECRET", s.key().to_uppercase());
        ak.ensure_oidc_app(
            &OidcApp {
                slug: &slug,
                name: &format!("{} ({})", s.def().label, t.slug),
                client_id: &slug,
                client_secret: render::get(&secrets, &secret_key),
                redirect_uris: &render::redirect_uris(&t.slug, &c.platform, s),
                launch_url: &format!("{}/{}", n.portal_origin(), s.key()),
                allowed_group_pk: &members_pk,
            },
            &defaults,
        )
        .await?;
    }

    // ------------------------------------------------------------- storage
    cx.report(job, "storage", &format!("encrypted storage on {}", node.name))
        .await?;
    let wrapped = volume_key(c, cx, &t.id).await?;
    let key = VolumeKey {
        keys: &c.keys,
        tenant_id: &t.id,
        wrapped: &wrapped,
    };
    let storage = TenantStorage::new(&node, &t.slug)?.with_key(key);
    let units = u64::try_from(t.storage_units).map_err(|e| e.to_string())?;
    storage.ensure_open(units, &render::all_volumes()).await?;
    audit(
        db,
        "system",
        "volume.unlock",
        Some(&t.id),
        Some(json!({ "reason": "apply", "node": node.name })),
    )
    .await?;
    resize(cx, job, &node, &t, &storage, units).await?;

    // ------------------------------------------------------------ services
    cx.report(job, "services", "starting containers").await?;
    let immich = serde_json::to_string_pretty(&render::immich_config(&t.slug, &c.platform, &secrets))
        .map_err(|e| e.to_string())?;
    node.write_node_file(&t.slug, "immich.json", &immich, false).await?;
    node.write_config(
        &t.slug,
        "compose.volumes.yml",
        &render::volumes_override(&storage.data_dir()),
    )?;
    let mut p = c.platform.clone();
    if let Some(ip) = &node.auth_host_ip {
        p.auth_host_ip = ip.clone();
    }
    let assistant_services: &[Service] = if with_assistant { &enabled } else { &[] };
    let env = render::tenant_env(&EnvInput {
        slug: &t.slug,
        p: &p,
        secrets: &secrets,
        versions: &c.versions,
        config_dir: &node.node_config_dir(&t.slug),
        brand_dir: &node.brand(),
        ca_dir: &node.ca(),
        assistant_services,
    })?;
    node.write_config(&t.slug, "tenant.env", &env)?;
    let llm_key = render::get(&secrets, "LLM_VIRTUAL_KEY");
    if with_assistant {
        prepare_assistant(c, cx, &node, &t, &members, &secrets, &enabled).await?;
    } else if let Some(gw) = &c.llm {
        gw.block(llm_key).await?;
    }

    // The node's shared services; the LLM gateway's admin API listens on the node's LAN address.
    let shared = vec![
        "compose".into(),
        "-f".into(),
        format!("{}/compose.yml", node.infra()),
        "--project-directory".into(),
        node.node_infra(),
        "--env-file".into(),
        c.versions_file.to_string_lossy().into_owned(),
        "up".into(),
        "-d".into(),
    ];
    let env = node
        .edge
        .as_ref()
        .map(|e| vec![("LLM_GATEWAY_BIND", e.host.clone())])
        .unwrap_or_default();
    node.docker_env(shared, None, env, false).await?;
    node.docker(
        compose_args(&node, &t.slug, &profiles, &["up", "-d", "--remove-orphans"]),
        false,
    )
    .await?;
    let project = n.project();
    for sc in SHARED_CONTAINERS {
        node.docker(strings(&["network", "connect", &format!("{project}-shared"), sc]), true)
            .await?;
    }
    if with_assistant {
        // The gateway is the assistant's only way out of the stack.
        node.docker(
            strings(&["network", "connect", &format!("{project}-llm"), LLM_GATEWAY]),
            true,
        )
        .await?;
        // Re-copy the rendered config (members, services) and restart Hermes.
        let args = ["up", "-d", "--force-recreate", "assistant-init", "assistant"];
        node.docker(compose_args(&node, &t.slug, &profiles, &args), false)
            .await?;
    }
    // Unbooked services: their containers go, their data stays on the storage.
    let mut unbooked: Vec<&str> = STACK_SERVICES
        .into_iter()
        .filter(|s| !enabled.contains(s))
        .flat_map(|s| s.def().containers.iter().copied())
        .collect();
    if !with_assistant {
        unbooked.extend(ASSISTANT_CONTAINERS);
    }
    for svc in unbooked {
        let ps = strings(&[
            "ps",
            "-aq",
            "--filter",
            &format!("label=com.docker.compose.project={project}"),
            "--filter",
            &format!("label=com.docker.compose.service={svc}"),
        ]);
        let ids = node.docker(ps, false).await?;
        let ids: Vec<&str> = ids.split_whitespace().collect();
        if !ids.is_empty() {
            let mut rm = strings(&["rm", "-f"]);
            rm.extend(strings(&ids));
            node.docker(rm, false).await?;
        }
    }

    // Remote nodes: the control plane forwards the tenant's hosts to the node's edge.
    let route = c.traefik_dir.join(format!("tenant-{}.yml", t.slug));
    match &node.edge {
        Some(e) => write_route(&route, &render::edge_route(&t.slug, &c.platform, &e.url))?,
        None => crate::suspend::remove_file(&route)?,
    }

    for s in &enabled {
        wait_healthy(c, &format!("{}{}", n.service_origin(*s), s.def().health_path), s.key()).await?;
    }
    if enabled.contains(&Service::Photos) {
        immich_service_account(c, &n, &t.slug, &secrets).await?;
    }
    if with_assistant {
        wait_healthy(c, &format!("{}/health", n.assistant_origin()), "assistant").await?;
    }
    for s in &enabled {
        let version = match s {
            Service::Photos => "IMMICH_VERSION",
            Service::Docs => "PAPERLESS_VERSION",
            Service::Places => "DAWARICH_VERSION",
            Service::Flights => "AIRTRAIL_VERSION",
        };
        healthy(cx, &t.id, &s.key().to_uppercase(), version_of(&c.versions, version)).await?;
    }
    if with_assistant {
        let hermes = version_of(&c.versions, "HERMES_IMAGE").and_then(|i| i.rsplit(':').next());
        healthy(cx, &t.id, "ASSISTANT", hermes).await?;
    }

    // Sign-outs and removals that waited for the apps to run.
    crate::revoke::process(c, cx, &node, &t.id, &t.slug, &keys).await?;

    // --------------------------------------------------------------- ready
    sqlx::query(r#"UPDATE "Tenant" SET status = 'ACTIVE', "updatedAt" = now() AT TIME ZONE 'UTC' WHERE id = $1"#)
        .bind(&t.id)
        .execute(db)
        .await
        .map_err(sql)?;
    crate::storage::measure(db, &t.id, &storage).await?;
    cx.report(job, "ready", "active").await
}

/// Once per node and process: templates, brand assets and the edge proxy
/// come up to date with this deployment (Nodes.driver in nodes.ts).
async fn prepare_once(c: &Control, node: &Node) -> Result<(), String> {
    if c.prepared
        .lock()
        .map_err(|_| "prepared nodes: poisoned")?
        .contains(&node.id)
    {
        return Ok(());
    }
    node.prepare(&c.ca_dir).await?;
    c.prepared
        .lock()
        .map_err(|_| "prepared nodes: poisoned")?
        .insert(node.id.clone());
    Ok(())
}

fn version_of<'a>(versions: &'a Vars, key: &str) -> Option<&'a str> {
    Some(render::get(versions, key)).filter(|v| !v.is_empty())
}

async fn healthy(cx: &JobCx, tenant_id: &str, kind: &str, version: Option<&str>) -> Result<(), String> {
    sqlx::query(
        r#"UPDATE "ServiceInstance" SET healthy = true, "lastHealthAt" = now() AT TIME ZONE 'UTC', version = $3
           WHERE "tenantId" = $1 AND kind = $2::"ServiceKind""#,
    )
    .bind(tenant_id)
    .bind(kind)
    .bind(version)
    .execute(&cx.db)
    .await
    .map_err(sql)?;
    Ok(())
}

/// Write then rename, so Traefik never reads a half-written file (routes.ts).
fn write_route(path: &std::path::Path, content: &str) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;
    let err = |e: std::io::Error| format!("{}: {e}", path.display());
    let tmp = path.with_extension("yml.tmp");
    std::fs::write(&tmp, content).map_err(err)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).map_err(err)?;
    std::fs::rename(&tmp, path).map_err(err)
}

/// People in the tenant's Authentik groups who are no longer (owner)
/// members: out of the group first, so they can't sign in to the apps
/// again; what they still hold in the apps is revoked afterwards.
async fn remove_former_members(
    c: &Control,
    cx: &JobCx,
    t: &Tenant,
    members: &[MemberRow],
    members_pk: &str,
    owners_pk: &str,
) -> Result<(), String> {
    let ak = &c.authentik;
    let subs: HashSet<&str> = members.iter().map(|m| m.sub.as_str()).collect();
    let owners: HashSet<&str> = members
        .iter()
        .filter(|m| m.role == "OWNER")
        .map(|m| m.sub.as_str())
        .collect();
    for u in ak.group_users(owners_pk).await? {
        if !owners.contains(u.uid.as_str()) {
            ak.remove_user_from_group(owners_pk, u.pk).await?;
        }
    }
    for u in ak.group_users(members_pk).await? {
        if subs.contains(u.uid.as_str()) {
            continue;
        }
        sqlx::query(
            r#"INSERT INTO "AccessRevocation" (id, "tenantId", email, sub, scope, "createdAt")
               VALUES ($1, $2, $3, $4, 'REMOVED', now() AT TIME ZONE 'UTC')"#,
        )
        .bind(new_id()?)
        .bind(&t.id)
        .bind(&u.email)
        .bind(&u.uid)
        .execute(&cx.db)
        .await
        .map_err(sql)?;
        ak.remove_user_from_group(members_pk, u.pk).await?;
        audit(
            &cx.db,
            "system",
            "member.removed_from_apps",
            Some(&t.id),
            Some(json!({ "sub": u.uid })),
        )
        .await?;
    }
    Ok(())
}

/// The tenant's secrets (encrypted at rest); missing ones are generated and
/// stored. Custom stacks' secrets ("stack:...") stay out of the tenant stack.
async fn tenant_secrets(c: &Control, cx: &JobCx, tenant_id: &str) -> Result<Vars, String> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        r#"SELECT key, ciphertext FROM "Secret" WHERE "tenantId" = $1 AND key NOT LIKE 'stack:%' ORDER BY "createdAt", id"#,
    )
    .bind(tenant_id)
    .fetch_all(&cx.db)
    .await
    .map_err(sql)?;
    let mut existing = Vars::new();
    for (key, sealed) in rows {
        let value = c
            .keys
            .open_secret(tenant_id, &key, &sealed)
            .map_err(|e| format!("secret {key}: {e}"))?;
        existing.push((key, value.to_string()));
    }
    let (secrets, created) = render::ensure_secrets(&existing)?;
    for key in created {
        store_secret(c, cx, tenant_id, key, render::get(&secrets, key)).await?;
    }
    Ok(secrets)
}

async fn store_secret(c: &Control, cx: &JobCx, tenant_id: &str, key: &str, value: &str) -> Result<(), String> {
    let sealed = c
        .keys
        .seal_secret(tenant_id, key, value)
        .map_err(|e| format!("secret {key}: {e}"))?;
    sqlx::query(
        r#"INSERT INTO "Secret" (id, "tenantId", key, ciphertext, "createdAt", "updatedAt")
           VALUES ($1, $2, $3, $4, now() AT TIME ZONE 'UTC', now() AT TIME ZONE 'UTC')"#,
    )
    .bind(new_id()?)
    .bind(tenant_id)
    .bind(key)
    .bind(sealed)
    .execute(&cx.db)
    .await
    .map_err(sql)?;
    Ok(())
}

/// The tenant's wrapped volume key, created on the first apply.
async fn volume_key(c: &Control, cx: &JobCx, tenant_id: &str) -> Result<String, String> {
    let q = r#"SELECT "wrappedKey" FROM "VolumeKey" WHERE "tenantId" = $1"#;
    if let Some(w) = sqlx::query_scalar(q)
        .bind(tenant_id)
        .fetch_optional(&cx.db)
        .await
        .map_err(sql)?
    {
        return Ok(w);
    }
    let wrapped = c
        .keys
        .new_wrapped_volume_key(tenant_id)
        .map_err(|e| format!("volume key: {e}"))?;
    sqlx::query(
        r#"INSERT INTO "VolumeKey" (id, "tenantId", "wrappedKey", "createdAt") VALUES ($1, $2, $3, now() AT TIME ZONE 'UTC')"#,
    )
    .bind(new_id()?)
    .bind(tenant_id)
    .bind(&wrapped)
    .execute(&cx.db)
    .await
    .map_err(sql)?;
    audit(&cx.db, "system", "volume.key_created", Some(tenant_id), None).await?;
    Ok(wrapped)
}

/// Sizes the storage to the booking. Growing is online. Shrinking only when
/// the data fits with headroom; otherwise the storage keeps its size and the
/// portal says why. LUKS stops the stack for a moment; ZFS shrinks online.
async fn resize(
    cx: &JobCx,
    job: &Job,
    node: &Node,
    t: &Tenant,
    storage: &TenantStorage<'_>,
    units: u64,
) -> Result<(), String> {
    let target = quota_mb(units);
    let (used, volume) = match storage.status().await? {
        Status::Open { used_mb, volume_mb, .. } if volume_mb > target => (used_mb, volume_mb),
        _ => {
            storage.grow(units).await?;
            return Ok(());
        }
    };
    if !fits(used, units) {
        let msg = format!("downgrade held back: {used} MB used, {target} MB booked");
        cx.report(job, "storage", &msg).await?;
        let details = json!({ "usedMb": used, "volumeMb": volume, "targetMb": target });
        return audit(&cx.db, "system", "volume.shrink_held", Some(&t.id), Some(details)).await;
    }
    cx.report(job, "storage", "shrinking volume").await?;
    if storage.offline_shrink() {
        let services: Vec<&str> = STACK_SERVICES.iter().map(|s| s.key()).collect();
        node.docker(compose_args(node, &t.slug, &services, &["stop"]), false)
            .await?;
    }
    storage.shrink(units).await?;
    audit(
        &cx.db,
        "system",
        "volume.shrunk",
        Some(&t.id),
        Some(json!({ "fromMb": volume, "toMb": target })),
    )
    .await
}

/// Until the URL answers below 400 (404: no route yet, 5xx: still booting).
async fn wait_healthy(c: &Control, url: &str, label: &str) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + c.health_timeout;
    while tokio::time::Instant::now() < deadline {
        if let Ok(res) = c.http.get(url).send().await
            && res.status().as_u16() < 400
        {
            return Ok(());
        }
        tokio::time::sleep(c.health_every).await;
    }
    Err(format!(
        "{label} not healthy after {}s ({url})",
        c.health_timeout.as_secs()
    ))
}

/// The Immich admin the dashboard and assistant use; customers sign in via SSO.
async fn immich_service_account(c: &Control, n: &Names<'_>, slug: &str, secrets: &Vars) -> Result<(), String> {
    let url = format!("{}/api/auth/admin-sign-up", n.service_origin(Service::Photos));
    let body = json!({
        "email": format!("service@{slug}.{}", c.platform.base_domain),
        "password": render::get(secrets, "IMMICH_SERVICE_PASSWORD"),
        "name": "Service",
    });
    let res = c
        .http
        .post(&url)
        .json(&body)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("immich admin sign-up: {}", e.without_url()))?;
    let status = res.status().as_u16();
    // 400: an admin exists already (a re-run).
    if !res.status().is_success() && status != 400 {
        return Err(format!("immich admin sign-up failed: {status}"));
    }
    Ok(())
}

/// The assistant: the tenant's LLM key with this month's budget, one Hermes
/// profile per member (own API key and tools token), the rendered files.
async fn prepare_assistant(
    c: &Control,
    cx: &JobCx,
    node: &Node,
    t: &Tenant,
    members: &[MemberRow],
    secrets: &Vars,
    services: &[Service],
) -> Result<(), String> {
    let gw = c.llm.as_ref().ok_or("the LLM gateway is not configured")?;
    let budget = assistant::ai_budget_usd(i64::from(t.ai_credit_chf), c.usd_per_chf);
    let llm_key = render::get(secrets, "LLM_VIRTUAL_KEY");
    gw.ensure_tenant_key(llm_key, &t.slug, &t.id, budget).await?;

    let mut with_secrets = Vec::new();
    for m in members {
        with_secrets.push(MemberSecrets {
            member: Member {
                id: m.id.clone(),
                email: m.email.clone(),
                authentik_sub: m.sub.clone(),
                name: m.name.clone(),
                locale: m.locale.clone().unwrap_or_else(|| t.locale.clone()),
            },
            api_key: member_secret(c, cx, &t.id, &assistant::member_key_name(&m.id))
                .await?
                .to_string(),
            tools_token: member_secret(c, cx, &t.id, &assistant::member_tools_token_name(&m.id))
                .await?
                .to_string(),
        });
    }
    // Members who left lose their keys; assistant-init removes their memory.
    let current: HashSet<String> = members
        .iter()
        .flat_map(|m| {
            [
                assistant::member_key_name(&m.id),
                assistant::member_tools_token_name(&m.id),
            ]
        })
        .collect();
    let keys: Vec<String> = sqlx::query_scalar(r#"SELECT key FROM "Secret" WHERE "tenantId" = $1"#)
        .bind(&t.id)
        .fetch_all(&cx.db)
        .await
        .map_err(sql)?;
    let gone: Vec<String> = keys
        .into_iter()
        .filter(|k| assistant::is_member_secret(k) && !current.contains(k))
        .collect();
    if !gone.is_empty() {
        sqlx::query(r#"DELETE FROM "Secret" WHERE "tenantId" = $1 AND key = ANY($2)"#)
            .bind(&t.id)
            .bind(&gone)
            .execute(&cx.db)
            .await
            .map_err(sql)?;
    }

    let files = assistant::files(&assistant::FilesInput {
        brand_name: &c.platform.brand_name,
        services,
        admin_key: render::get(secrets, "ASSISTANT_ADMIN_KEY"),
        llm_key,
        members: &with_secrets,
    })?;
    node.remove_node_files(&t.slug, "assistant/hermes/profiles").await?;
    for (file, content) in files {
        // The tools container runs unprivileged; members.json holds only token hashes.
        let readable = file == "members.json";
        node.write_node_file(&t.slug, &format!("assistant/{file}"), &content, readable)
            .await?;
    }
    Ok(())
}

async fn member_secret(c: &Control, cx: &JobCx, tenant_id: &str, key: &str) -> Result<Zeroizing<String>, String> {
    let sealed: Option<String> =
        sqlx::query_scalar(r#"SELECT ciphertext FROM "Secret" WHERE "tenantId" = $1 AND key = $2"#)
            .bind(tenant_id)
            .bind(key)
            .fetch_optional(&cx.db)
            .await
            .map_err(sql)?;
    if let Some(s) = sealed {
        return c
            .keys
            .open_secret(tenant_id, key, &s)
            .map_err(|e| format!("secret {key}: {e}"));
    }
    let value = Zeroizing::new(assistant::new_secret()?);
    store_secret(c, cx, tenant_id, key, &value).await?;
    Ok(value)
}
