//! The ops CLIs of apps/provisioner (node-admin.ts, backup-admin.ts,
//! staff-admin.ts, delete-tenant.ts), as functions returning what they
//! print: castor-admin calls them, the tests check them.

use std::collections::HashMap;
use std::fmt::Write as _;

use serde_json::json;
use sqlx::PgPool;

use crate::db::audit;
use crate::node::{Node, NodeRow};
use crate::{Control, backup, queue, valid};

/// Positionals and --options ("--name value", "--name=value", or a flag).
#[derive(Debug, Default)]
pub struct Args {
    pub positional: Vec<String>,
    pub options: HashMap<String, Vec<String>>,
}

impl Args {
    pub fn parse(args: &[String], flags: &[&str]) -> Result<Self, String> {
        let mut out = Self::default();
        let mut it = args.iter();
        while let Some(a) = it.next() {
            let Some(name) = a.strip_prefix("--") else {
                out.positional.push(a.clone());
                continue;
            };
            let (name, value) = match name.split_once('=') {
                Some((n, v)) => (n.to_owned(), v.to_owned()),
                None if flags.contains(&name) => (name.to_owned(), "true".to_owned()),
                None => (
                    name.to_owned(),
                    it.next().ok_or_else(|| format!("--{name} needs a value"))?.clone(),
                ),
            };
            out.options.entry(name).or_default().push(value);
        }
        Ok(out)
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.options.get(name).and_then(|v| v.last()).map(String::as_str)
    }

    pub fn all(&self, name: &str) -> Vec<&str> {
        self.options
            .get(name)
            .map(|v| v.iter().map(String::as_str).collect())
            .unwrap_or_default()
    }

    pub fn arg(&self, i: usize) -> Option<&str> {
        self.positional.get(i).map(String::as_str)
    }
}

fn sql(e: sqlx::Error) -> String {
    e.to_string()
}

/// As formatStorage in @pc/pricing.
pub fn format_storage(mb: f64) -> String {
    if mb < 1024.0 {
        return format!("{} MB", js_round(mb));
    }
    let gb = mb / 1024.0;
    if gb < 1000.0 {
        if gb < 10.0 {
            format!("{gb:.1} GB")
        } else {
            format!("{} GB", js_round(gb))
        }
    } else {
        format!("{:.1} TB", gb / 1000.0)
    }
}

#[allow(clippy::cast_possible_truncation)]
fn js_round(x: f64) -> i64 {
    (x + 0.5).floor() as i64
}

// ------------------------------------------------------------------ nodes

fn node_name(name: &str) -> bool {
    (1..=63).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

const ADD_USAGE: &str = "usage: node add <name> <user@host> --kind truenas|mac --pool <pool> --edge <host[:port]> --auth-host-ip <ip> [--capacity 20]\n       node add <name> <user@host> --kind mac|truenas --role stacks --pool <pool> --edge <host[:port]> --control-plane-ip <ip> [--uid-base 100000] [--no-gvisor]";

/// `node add`: probes the node, registers it and prepares it (templates,
/// edge proxy; on a stacks node the egress proxy). Refuses a degraded pool,
/// and a stacks node whose Docker would not isolate customers' containers.
pub async fn node_add(c: &Control, db: &PgPool, a: &Args) -> Result<String, String> {
    let (Some(name), Some(target)) = (a.arg(1), a.arg(2)) else {
        return Err(ADD_USAGE.into());
    };
    let stacks = a.get("role").unwrap_or("tenants") == "stacks";
    let kind = a.get("kind").unwrap_or("truenas").to_uppercase();
    let (Some(pool), Some(edge), Some((ssh_user, ssh_host))) = (a.get("pool"), a.get("edge"), target.split_once('@'))
    else {
        return Err(ADD_USAGE.into());
    };
    if !node_name(name) || !["TRUENAS", "MAC"].contains(&kind.as_str()) {
        return Err(ADD_USAGE.into());
    }
    let ips = a.all("control-plane-ip");
    let auth_host_ip = a.get("auth-host-ip");
    let capacity: i32 = a
        .get("capacity")
        .unwrap_or("20")
        .parse()
        .map_err(|_| "--capacity must be a number")?;
    let uid_base: i32 = a
        .get("uid-base")
        .unwrap_or("100000")
        .parse()
        .map_err(|_| "--uid-base must be a number")?;
    if stacks {
        if ips.is_empty() {
            return Err(ADD_USAGE.into());
        }
        for ip in &ips {
            let (addr, bits) = ip.split_once('/').map_or((*ip, None), |(a, b)| (a, Some(b)));
            let ok = !addr.is_empty()
                && addr.bytes().all(|b| b.is_ascii_hexdigit() || b == b'.' || b == b':')
                && bits.is_none_or(|b| (1..=3).contains(&b.len()) && b.bytes().all(|d| d.is_ascii_digit()));
            if !ok {
                return Err(format!("invalid --control-plane-ip {ip}"));
            }
        }
        if uid_base < 65536 {
            return Err("--uid-base must be at least 65536".into());
        }
    } else if auth_host_ip.is_none() {
        return Err(ADD_USAGE.into());
    }
    if let Some(ip) = auth_host_ip
        && !valid::ssh_word(ip)
    {
        return Err(format!("invalid --auth-host-ip {ip}"));
    }

    let existing: Option<(String, String, i64, i64)> = sqlx::query_as(
        r#"SELECT n.id, n.role::text,
                  (SELECT count(*) FROM "Tenant" t WHERE t."nodeId" = n.id),
                  (SELECT count(*) FROM "Tenant" t WHERE t."stacksNodeId" = n.id)
           FROM "Node" n WHERE n.name = $1"#,
    )
    .bind(name)
    .fetch_optional(db)
    .await
    .map_err(sql)?;
    match &existing {
        Some((_, role, tenants, _)) if stacks && (role != "STACKS" || *tenants > 0) => {
            return Err(format!(
                "{name} already holds tenants; a stacks node must be a separate machine"
            ));
        }
        Some((_, role, _, stack_tenants)) if !stacks && role == "STACKS" && *stack_tenants > 0 => {
            return Err(format!("{name} runs custom stacks; it can't take tenants"));
        }
        _ => {}
    }
    let id = match &existing {
        Some((id, ..)) => id.clone(),
        None => crate::db::new_id()?,
    };
    let row = NodeRow {
        id: id.clone(),
        name: name.to_owned(),
        kind: kind.clone(),
        role: if stacks { "STACKS" } else { "TENANTS" }.into(),
        ssh_host: ssh_host.to_owned(),
        ssh_user: ssh_user.to_owned(),
        zfs_pool: Some(pool.to_owned()),
        auth_host_ip: if stacks { None } else { auth_host_ip.map(str::to_owned) },
        edge_host: Some(edge.to_owned()),
        control_plane_ips: stacks.then(|| ips.join(",")),
        uid_base: stacks.then_some(uid_base),
        stack_runtime: None,
    };
    let node = Node::new(&row, &c.root, c.exec.clone())?;
    let mut out = format!("probing {target}…\n");
    let p = crate::tasks::probe_host(&node).await?;
    let (mut memory, mut cpus, mut runtime) = (p.memory_mb, p.cpus, None);
    if stacks {
        let f = crate::stacks::edge::facts(&node).await?;
        let _ = writeln!(
            out,
            "  {}, Docker {}: {} CPUs, {} RAM, {}",
            p.os,
            p.docker,
            f.cpus,
            format_storage(f.memory_mb as f64),
            f.arch
        );
        let _ = writeln!(
            out,
            "  pool {pool}: {}, {} of {} used",
            p.pool_health,
            format_storage(f64::from(p.pool_used_mb)),
            format_storage(f64::from(p.pool_size_mb))
        );
        let _ = writeln!(
            out,
            "  userns-remap: {}, gVisor: {}",
            if f.userns { "yes" } else { "NO" },
            if f.runsc { "yes" } else { "no" }
        );
        if p.pool_health != "ONLINE" {
            return Err(format!("{out}pool is {}, not registering", p.pool_health));
        }
        if !f.userns {
            return Err(format!(
                "{out}Docker must run with userns-remap on a stacks node (root in a container must not be root on the node)"
            ));
        }
        if !f.runsc && a.get("no-gvisor").is_none() {
            return Err(format!(
                "{out}gVisor (runsc) is missing; install it, or pass --no-gvisor to accept plain runc"
            ));
        }
        memory = i32::try_from(f.memory_mb).map_err(|e| e.to_string())?;
        cpus = i32::try_from(f.cpus).map_err(|e| e.to_string())?;
        runtime = Some(if f.runsc { "runsc" } else { "runc" });
    } else {
        let _ = writeln!(
            out,
            "  {}, {} CPUs, {} RAM, Docker {}",
            p.os,
            p.cpus,
            format_storage(f64::from(p.memory_mb)),
            p.docker
        );
        let _ = writeln!(
            out,
            "  pool {pool}: {}, {} of {} used",
            p.pool_health,
            format_storage(f64::from(p.pool_used_mb)),
            format_storage(f64::from(p.pool_size_mb))
        );
        if p.pool_health != "ONLINE" {
            return Err(format!("{out}pool is {}, not registering", p.pool_health));
        }
    }
    sqlx::query(
        r#"INSERT INTO "Node" (id, name, kind, role, "sshHost", "sshUser", "zfsPool", "authHostIp", "edgeHost",
                              capacity, enabled, "controlPlaneIps", "uidBase", "stackRuntime",
                              "poolSizeMb", "poolUsedMb", "poolHealth", "memoryMb", cpus, "lastSeenAt", "lastError")
           VALUES ($1, $2, $3::"NodeKind", $4::"NodeRole", $5, $6, $7, $8, $9, $10, true, $11, $12, $13,
                   $14, $15, $16, $17, $18, now() AT TIME ZONE 'UTC', NULL)
           ON CONFLICT (name) DO UPDATE SET
             kind = EXCLUDED.kind, role = EXCLUDED.role, "sshHost" = EXCLUDED."sshHost", "sshUser" = EXCLUDED."sshUser",
             "zfsPool" = EXCLUDED."zfsPool", "authHostIp" = COALESCE(EXCLUDED."authHostIp", "Node"."authHostIp"),
             "edgeHost" = EXCLUDED."edgeHost",
             capacity = EXCLUDED.capacity, enabled = true,
             -- What only one role sets is kept when the other registers.
             "controlPlaneIps" = COALESCE(EXCLUDED."controlPlaneIps", "Node"."controlPlaneIps"),
             "uidBase" = COALESCE(EXCLUDED."uidBase", "Node"."uidBase"),
             "stackRuntime" = COALESCE(EXCLUDED."stackRuntime", "Node"."stackRuntime"),
             "poolSizeMb" = EXCLUDED."poolSizeMb", "poolUsedMb" = EXCLUDED."poolUsedMb",
             "poolHealth" = EXCLUDED."poolHealth", "memoryMb" = EXCLUDED."memoryMb",
             cpus = COALESCE(EXCLUDED.cpus, "Node".cpus),
             "lastSeenAt" = EXCLUDED."lastSeenAt", "lastError" = NULL"#,
    )
    .bind(&id)
    .bind(name)
    .bind(&kind)
    .bind(&row.role)
    .bind(ssh_host)
    .bind(ssh_user)
    .bind(pool)
    .bind(&row.auth_host_ip)
    .bind(edge)
    .bind(if stacks { 0 } else { capacity })
    .bind(&row.control_plane_ips)
    .bind(row.uid_base)
    .bind(runtime)
    .bind(p.pool_size_mb)
    .bind(p.pool_used_mb)
    .bind(&p.pool_health)
    .bind(memory)
    .bind(stacks.then_some(cpus))
    .execute(db)
    .await
    .map_err(sql)?;
    out.push_str(if stacks {
        "building the egress proxy and starting the edge proxy…\n"
    } else {
        "syncing templates and starting the edge proxy…\n"
    });
    node.prepare(&c.ca_dir).await?;
    let mut details = json!({ "name": name, "target": target, "pool": pool });
    if stacks {
        details["role"] = json!("STACKS");
        details["runtime"] = json!(runtime);
        details["controlPlaneIps"] = json!(ips);
    }
    audit(db, "ops-cli", "node.registered", None, Some(details)).await?;
    let _ = write!(
        out,
        "node {name} is ready for {}",
        if stacks { "custom stacks" } else { "tenants" }
    );
    Ok(out)
}

/// `node drain|enable <name>`.
pub async fn node_enable(db: &PgPool, name: &str, enable: bool) -> Result<String, String> {
    let done = sqlx::query(r#"UPDATE "Node" SET enabled = $2 WHERE name = $1"#)
        .bind(name)
        .bind(enable)
        .execute(db)
        .await
        .map_err(sql)?;
    if done.rows_affected() == 0 {
        return Err(format!("no node {name}"));
    }
    let action = if enable { "node.enabled" } else { "node.drained" };
    audit(db, "ops-cli", action, None, Some(json!({ "name": name }))).await?;
    Ok(format!(
        "{name}: {}",
        if enable {
            "takes new tenants"
        } else {
            "draining (no new tenants)"
        }
    ))
}

/// `node list`.
pub async fn node_list(db: &PgPool) -> Result<String, String> {
    #[allow(clippy::type_complexity)]
    let rows: Vec<(
        String,
        String,
        bool,
        String,
        Option<String>,
        i64,
        i32,
        Option<i32>,
        Option<i32>,
        Option<String>,
        Option<String>,
        Option<String>,
    )> = sqlx::query_as(
        r#"SELECT n.name, n.kind::text, n.enabled, n.role::text, n."stackRuntime",
                      (SELECT count(*) FROM "Tenant" t WHERE t."nodeId" = n.id AND t.status <> 'DELETED'),
                      n.capacity, n."poolSizeMb", n."poolUsedMb", n."poolHealth",
                      to_char(n."lastSeenAt", 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'), n."lastError"
               FROM "Node" n ORDER BY n.name"#,
    )
    .fetch_all(db)
    .await
    .map_err(sql)?;
    let mut out = String::new();
    for (name, kind, enabled, role, runtime, tenants, capacity, size, used, health, seen, error) in rows {
        let pool = match size.filter(|s| *s != 0) {
            Some(s) => format!(
                "{} {}/{}",
                health.unwrap_or_default(),
                format_storage(f64::from(used.unwrap_or(0))),
                format_storage(f64::from(s))
            ),
            None => "-".into(),
        };
        let load = if role == "STACKS" {
            format!("stacks ({})", runtime.as_deref().unwrap_or("?"))
        } else {
            format!("{tenants}/{capacity} tenants")
        };
        let error = error.map_or_else(String::new, |e| {
            format!("  error: {}", crate::stacks::js::slice(&e, 80))
        });
        let _ = writeln!(
            out,
            "{name:<12} {kind:<5} {:<8} {load:<14} {pool:<24} seen {}{error}",
            if enabled { "active" } else { "draining" },
            seen.as_deref().unwrap_or("never"),
        );
    }
    Ok(out.trim_end().to_owned())
}

// ----------------------------------------------------------------- staff

pub async fn staff_list(db: &PgPool) -> Result<String, String> {
    let staff: Vec<String> = sqlx::query_scalar(r#"SELECT email FROM "User" WHERE "isStaff" ORDER BY email"#)
        .fetch_all(db)
        .await
        .map_err(sql)?;
    Ok(if staff.is_empty() {
        "no staff".into()
    } else {
        staff.join("\n")
    })
}

/// `staff add|remove <email>`: the person must have signed in once.
pub async fn staff_set(db: &PgPool, email: &str, staff: bool) -> Result<String, String> {
    let lower = email.trim().to_lowercase();
    let user: Option<(String, String)> = sqlx::query_as(
        r#"SELECT id, email FROM "User" WHERE email = $1
           UNION ALL SELECT id, email FROM "User" WHERE email = $2 LIMIT 1"#,
    )
    .bind(&lower)
    .bind(email.trim())
    .fetch_optional(db)
    .await
    .map_err(sql)?;
    let Some((id, email)) = user else {
        return Err(format!("no user {email}: they must sign in to the portal once first"));
    };
    sqlx::query(r#"UPDATE "User" SET "isStaff" = $2 WHERE id = $1"#)
        .bind(&id)
        .bind(staff)
        .execute(db)
        .await
        .map_err(sql)?;
    let action = if staff { "staff.grant" } else { "staff.revoke" };
    audit(
        db,
        "system",
        action,
        None,
        Some(json!({ "userId": id, "email": email })),
    )
    .await?;
    Ok(format!(
        "{email}: {}",
        if staff { "now staff" } else { "no longer staff" }
    ))
}

// --------------------------------------------------------------- tenants

async fn tenant_id(db: &PgPool, slug: &str) -> Result<String, String> {
    sqlx::query_scalar(r#"SELECT id FROM "Tenant" WHERE slug = $1"#)
        .bind(slug)
        .fetch_optional(db)
        .await
        .map_err(sql)?
        .ok_or_else(|| format!("no tenant {slug}"))
}

/// `delete-tenant <slug>`: queues DELETE (irreversible once the job runs:
/// stack, encrypted volume and volume key are destroyed).
pub async fn delete_tenant(db: &PgPool, slug: &str) -> Result<String, String> {
    let id = tenant_id(db, slug).await?;
    audit(
        db,
        "ops-cli",
        "tenant.delete_requested",
        Some(&id),
        Some(json!({ "slug": slug })),
    )
    .await?;
    let job = queue::enqueue_with(db, &id, "DELETE", None).await?;
    Ok(format!("queued DELETE for {slug} (job {job})"))
}

// --------------------------------------------------------------- backups

/// `backup status`: the box, every tenant's last backup, problems, dumps.
pub async fn backup_status(c: &Control, db: &PgPool, s: &backup::Settings) -> Result<String, String> {
    let pool = backup::pool(c, s).await?;
    let mut out = format!(
        "backup box {}: {}, {} of {} MB used\n\n",
        s.target, pool.health, pool.used_mb, pool.size_mb
    );
    #[allow(clippy::type_complexity)]
    let rows: Vec<(
        String,
        String,
        Option<String>,
        Option<String>,
        Option<i32>,
        Option<String>,
    )> = sqlx::query_as(
        r#"SELECT t.slug, t.status::text, n.kind::text,
                  to_char(b."lastSuccessAt", 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'), b.generation,
                  to_char(b."lastVerifiedAt", 'YYYY-MM-DD')
           FROM "Tenant" t LEFT JOIN "Node" n ON n.id = t."nodeId"
           LEFT JOIN "TenantBackup" b ON b."tenantId" = t.id
           WHERE t.status <> 'DELETED' ORDER BY t.slug"#,
    )
    .fetch_all(db)
    .await
    .map_err(sql)?;
    for (slug, status, kind, last, generation, verified) in rows {
        let zfs = kind.as_deref().is_some_and(|k| k != "LOCAL");
        let state = match (&last, zfs) {
            (_, false) => "no ZFS node".to_owned(),
            (Some(l), true) => format!("last {l} g{}", generation.unwrap_or(0)),
            (None, true) => "never backed up".to_owned(),
        };
        let verified = verified.map_or_else(String::new, |v| format!("verified {v}"));
        let _ = writeln!(out, "{slug:<24} {status:<10} {state} {verified}");
    }
    let problems = backup::problems(db, s).await?;
    out.push_str(if problems.is_empty() {
        "\nNo problems.\n"
    } else {
        "\nNeeds attention:\n"
    });
    for (slug, issues) in problems {
        let _ = writeln!(out, "  {slug}: {}", issues.join("; "));
    }
    if s.db_public_key.is_some() {
        let dumps = backup::db_list(c, s).await?;
        let _ = write!(
            out,
            "\ndatabase dumps: {}, newest {}",
            dumps.len(),
            dumps.last().map_or("none", |(n, _)| n.as_str())
        );
    } else {
        out.push_str("\nBACKUP_DB_PUBLIC_KEY is not set: the control-plane database is NOT backed up");
    }
    Ok(out)
}

/// `backup list <slug>`.
pub async fn backup_list(c: &Control, s: &backup::Settings, slug: &str) -> Result<String, String> {
    let listing = backup::list(c, s).await?;
    let snaps = listing
        .iter()
        .find(|(t, _)| t == slug)
        .map(|(_, l)| l.as_slice())
        .unwrap_or_default();
    Ok(snaps
        .iter()
        .map(|b| {
            #[allow(clippy::cast_precision_loss)]
            let mb = js_round(b.used_bytes as f64 / 1_048_576.0);
            format!(
                "{}@{}  {}  {mb} MB",
                b.gen_,
                b.snap,
                crate::monitor::rules::iso(b.created * 1000)
            )
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

/// `backup restore <slug> [<gen>@<snap>] [--node <name>]`: queues a restore
/// (default: the newest backup). The current data is set aside, not deleted.
pub async fn backup_restore(
    c: &Control,
    db: &PgPool,
    s: &backup::Settings,
    slug: &str,
    pick: Option<&str>,
    onto: Option<&str>,
) -> Result<String, String> {
    let id = tenant_id(db, slug).await?;
    let listing = backup::list(c, s).await?;
    let snaps = listing
        .iter()
        .find(|(t, _)| t == slug)
        .map(|(_, l)| l.as_slice())
        .unwrap_or_default();
    let chosen = match pick {
        Some(p) => snaps.iter().find(|b| format!("{}@{}", b.gen_, b.snap) == p),
        None => snaps.last(),
    };
    let Some(b) = chosen else {
        return Err(match pick {
            Some(p) => format!("no backup {p} of {slug} (see: list {slug})"),
            None => format!("no backups of {slug}"),
        });
    };
    if crate::tasks::busy(db, &id).await? {
        return Err(format!("{slug} has a job queued or running; try again when it's done"));
    }
    let mut out = String::new();
    let current: Option<String> =
        sqlx::query_scalar(r#"SELECT n.name FROM "Tenant" t JOIN "Node" n ON n.id = t."nodeId" WHERE t.id = $1"#)
            .bind(&id)
            .fetch_optional(db)
            .await
            .map_err(sql)?;
    if let Some(name) = onto {
        let (node_id, kind, pool): (String, String, Option<String>) =
            sqlx::query_as(r#"SELECT id, kind::text, "zfsPool" FROM "Node" WHERE name = $1"#)
                .bind(name)
                .fetch_optional(db)
                .await
                .map_err(sql)?
                .ok_or_else(|| format!("no node {name}"))?;
        if kind == "LOCAL" || pool.is_none() {
            return Err(format!("node {name} has no ZFS pool"));
        }
        sqlx::query(r#"UPDATE "Tenant" SET "nodeId" = $2, "updatedAt" = now() AT TIME ZONE 'UTC' WHERE id = $1"#)
            .bind(&id)
            .bind(&node_id)
            .execute(db)
            .await
            .map_err(sql)?;
        let _ = writeln!(
            out,
            "{slug} now runs on {name}{}",
            current.as_ref().map_or_else(String::new, |c| format!(
                " (was {c}; clean that node up by hand if it comes back)"
            ))
        );
    }
    let from = format!("{}@{}", b.gen_, b.snap);
    audit(
        db,
        "ops-cli",
        "backup.restore_requested",
        Some(&id),
        Some(json!({ "from": from, "node": onto.map(str::to_owned).or(current) })),
    )
    .await?;
    let job = queue::enqueue_with(db, &id, "RESTORE", Some(json!({ "gen": b.gen_, "snap": b.snap }))).await?;
    let _ = writeln!(
        out,
        "queued RESTORE of {slug} from {from} ({}), job {job}",
        crate::monitor::rules::iso(b.created * 1000)
    );
    let _ = write!(
        out,
        "the job log names the tag the current data is set aside under; remove it with: discard {slug} <tag>"
    );
    Ok(out)
}

/// `backup discard <slug> <tag>`.
pub async fn backup_discard(c: &Control, db: &PgPool, slug: &str, tag: &str) -> Result<String, String> {
    let id = tenant_id(db, slug).await?;
    backup::discard(c, db, &id, tag).await?;
    audit(
        db,
        "ops-cli",
        "backup.set_aside_discarded",
        Some(&id),
        Some(json!({ "tag": tag })),
    )
    .await?;
    Ok(format!("removed {slug}.{tag}"))
}

// ------------------------------------------------------------------- e2e

/// `e2e [--delete]`: the provisioning pipeline end to end, without browser
/// or payment: a test tenant is created, provisioned by the workers, its
/// encrypted volume and every service checked, and optionally deleted.
pub async fn e2e(c: &Control, db: &PgPool, delete: bool) -> Result<(), String> {
    let p = &c.platform;
    let stamp = crate::monitor::now_ms();
    let slug = format!("e2e-{}", radix36(u64::try_from(stamp).unwrap_or(0)));
    let user: String = sqlx::query_scalar(
        r#"INSERT INTO "User" (id, "authentikSub", email, name) VALUES ($1, 'e2e-test', 'e2e@example.invalid', 'E2E')
           ON CONFLICT ("authentikSub") DO UPDATE SET "authentikSub" = EXCLUDED."authentikSub" RETURNING id"#,
    )
    .bind(crate::db::new_id()?)
    .fetch_one(db)
    .await
    .map_err(sql)?;
    let tenant = crate::db::new_id()?;
    let mut tx = db.begin().await.map_err(sql)?;
    sqlx::query(
        r#"INSERT INTO "Tenant" (id, slug, "displayName", status, "updatedAt")
           VALUES ($1, $2, 'E2E test', 'PROVISIONING', now() AT TIME ZONE 'UTC')"#,
    )
    .bind(&tenant)
    .bind(&slug)
    .execute(&mut *tx)
    .await
    .map_err(sql)?;
    sqlx::query(
        r#"INSERT INTO "Member" (id, "tenantId", "userId", role, "createdAt") VALUES ($1, $2, $3, 'OWNER', now() AT TIME ZONE 'UTC')"#,
    )
    .bind(crate::db::new_id()?)
    .bind(&tenant)
    .bind(&user)
    .execute(&mut *tx)
    .await
    .map_err(sql)?;
    for kind in ["PHOTOS", "DOCS", "PLACES", "FLIGHTS"] {
        sqlx::query(r#"INSERT INTO "ServiceInstance" (id, "tenantId", kind) VALUES ($1, $2, $3::"ServiceKind")"#)
            .bind(crate::db::new_id()?)
            .bind(&tenant)
            .bind(kind)
            .execute(&mut *tx)
            .await
            .map_err(sql)?;
    }
    tx.commit().await.map_err(sql)?;
    println!("tenant {slug} created, provisioning…");
    queue::enqueue(db, &tenant, "PROVISION").await?;
    let started = std::time::Instant::now();
    let status = wait_for(db, &tenant, &["ACTIVE", "FAILED"]).await?;
    println!("status {status} after {}s", started.elapsed().as_secs());

    let node_id: String = sqlx::query_scalar(r#"SELECT "nodeId" FROM "Tenant" WHERE id = $1 AND "nodeId" IS NOT NULL"#)
        .bind(&tenant)
        .fetch_optional(db)
        .await
        .map_err(sql)?
        .ok_or("the tenant was not placed on a node")?;
    let row = NodeRow::load(db, &node_id).await?;
    let node = Node::new(&row, &c.root, c.exec.clone())?;
    let storage = crate::storage::TenantStorage::new(&node, &slug)?;
    println!("node: {}", row.name);
    println!("volume: {:?}", storage.status().await?);
    let (count, sealed): (i64, bool) = sqlx::query_as(
        r#"SELECT count(*), COALESCE(bool_and(ciphertext LIKE 'v1.%'), true) FROM "Secret" WHERE "tenantId" = $1"#,
    )
    .bind(&tenant)
    .fetch_one(db)
    .await
    .map_err(sql)?;
    println!("secrets: {count} stored, encrypted: {sealed}");
    let names = crate::render::Names::new(&slug, p);
    for svc in crate::render::STACK_SERVICES {
        let res = c
            .http
            .get(format!("{}/", names.service_origin(svc)))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let csp = res
            .headers()
            .get("content-security-policy")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let want = format!("frame-ancestors {}", names.portal_origin());
        println!(
            "{:<8} {}  frame-ancestors ok: {}",
            svc.key(),
            res.status().as_u16(),
            csp.as_deref() == Some(want.as_str())
        );
    }
    if delete {
        println!("deleting…");
        queue::enqueue(db, &tenant, "DELETE").await?;
        wait_for(db, &tenant, &["DELETED"]).await?;
        let key_left: bool = sqlx::query_scalar(r#"SELECT EXISTS (SELECT 1 FROM "VolumeKey" WHERE "tenantId" = $1)"#)
            .bind(&tenant)
            .fetch_one(db)
            .await
            .map_err(sql)?;
        println!(
            "deleted. volume: {:?}, key destroyed: {}",
            storage.status().await?,
            !key_left
        );
    }
    Ok(())
}

fn radix36(mut n: u64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    loop {
        out.push(DIGITS[usize::try_from(n % 36).unwrap_or(0)]);
        n /= 36;
        if n == 0 {
            break;
        }
    }
    out.reverse();
    String::from_utf8_lossy(&out).into_owned()
}

/// Until the tenant has one of `status` (30 minutes at most), printing each new step.
async fn wait_for(db: &PgPool, tenant: &str, status: &[&str]) -> Result<String, String> {
    let mut last_step = String::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1800);
    while std::time::Instant::now() < deadline {
        let (now, step, job_status, error): (String, Option<String>, Option<String>, Option<String>) = sqlx::query_as(
            r#"SELECT t.status::text, j.step, j.status::text, j.error FROM "Tenant" t
               LEFT JOIN LATERAL (SELECT step, status, error FROM "Job" WHERE "tenantId" = t.id
                                  ORDER BY "createdAt" DESC LIMIT 1) j ON true
               WHERE t.id = $1"#,
        )
        .bind(tenant)
        .fetch_one(db)
        .await
        .map_err(sql)?;
        if let Some(step) = step.filter(|s| *s != last_step) {
            println!("  step: {step}");
            last_step = step;
        }
        if status.contains(&now.as_str()) {
            return Ok(now);
        }
        if job_status.as_deref() == Some("FAILED") {
            return Err(format!("job failed: {}", error.unwrap_or_default()));
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    Err("timeout".into())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn arguments() {
        let v: Vec<String> = [
            "add",
            "n1",
            "u@h",
            "--pool",
            "tank",
            "--control-plane-ip=10.0.0.1",
            "--no-gvisor",
            "--control-plane-ip",
            "10.0.0.2",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        let a = Args::parse(&v, &["no-gvisor"]).unwrap();
        assert_eq!(a.positional, ["add", "n1", "u@h"]);
        assert_eq!(a.get("pool"), Some("tank"));
        assert_eq!(a.get("no-gvisor"), Some("true"));
        assert_eq!(a.all("control-plane-ip"), ["10.0.0.1", "10.0.0.2"]);
        assert!(Args::parse(&["--pool".to_owned()], &[]).is_err());
    }

    #[test]
    fn storage_sizes() {
        // As formatStorage.
        assert_eq!(format_storage(512.0), "512 MB");
        assert_eq!(format_storage(2560.0), "2.5 GB");
        assert_eq!(format_storage(500.0 * 1024.0), "500 GB");
        assert_eq!(format_storage(2_000_000.0), "2.0 TB");
    }
}
