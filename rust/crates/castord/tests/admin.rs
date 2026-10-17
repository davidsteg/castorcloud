// The ops CLI against the real schema, with SSH faked: registering nodes,
// staff, deletion and restore requests. Runs when TEST_DATABASE_URL is set.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use castord::admin::{self, Args};
use common::*;
use sqlx::PgPool;

fn args(v: &[&str]) -> Args {
    Args::parse(&v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(), &["no-gvisor"]).unwrap()
}

async fn node_row(db: &PgPool, name: &str) -> Vec<String> {
    rows(
        db,
        r#"SELECT concat_ws(' ', kind, role, "sshUser" || '@' || "sshHost", "zfsPool", "edgeHost", "authHostIp", capacity,
                  "poolSizeMb", "poolHealth", "memoryMb", cpus, "controlPlaneIps", "uidBase", "stackRuntime", enabled)
           FROM "Node" WHERE name = $1"#,
        name,
    )
    .await
}

const PROBE: [(&str, Result<&str, &str>); 3] = [
    ("castor-zfs pool", Ok("pool 2000000 500000 ONLINE\n")),
    ("castor-info", Ok("info 24.10 65536 12\n")),
    ("docker version", Ok("27.3.1\n")),
];

#[tokio::test]
async fn registers_nodes() {
    let Some(db) = db().await else { return };
    let traefik = std::env::temp_dir().join(format!("castord-admin-{}", hex8()));
    let name = format!("n{}", hex8());
    let fake = Fake::new(PROBE.to_vec());
    let mut c = std::sync::Arc::into_inner(control(fake.clone(), &traefik, None, "http://127.0.0.1:9")).unwrap();
    // Preparing the node packs the real templates.
    c.root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    c.ca_dir = traefik.join("ca");
    std::fs::create_dir_all(&c.ca_dir).unwrap();
    let out = admin::node_add(
        &c,
        &db,
        &args(&[
            "add",
            &name,
            "castor@10.0.0.7",
            "--kind",
            "truenas",
            "--pool",
            "tank",
            "--edge",
            "10.0.0.7:8480",
            "--auth-host-ip",
            "10.0.0.2",
        ]),
    )
    .await
    .unwrap();
    assert!(out.contains("24.10, 12 CPUs, 64 GB RAM, Docker 27.3.1"), "{out}");
    assert!(out.ends_with(&format!("node {name} is ready for tenants")));
    assert_eq!(
        node_row(&db, &name).await,
        ["TRUENAS TENANTS castor@10.0.0.7 tank 10.0.0.7:8480 10.0.0.2 20 2000000 ONLINE 65536 t"]
    );
    // The node was prepared: templates and the edge proxy.
    assert!(
        fake.calls().iter().any(|c| c.contains("castor-untar")),
        "{:?}",
        fake.calls()
    );

    // Draining and listing.
    admin::node_enable(&db, &name, false).await.unwrap();
    let list = admin::node_list(&db).await.unwrap();
    let line = list.lines().find(|l| l.starts_with(&name)).unwrap();
    assert!(
        line.contains("draining") && line.contains("0/20 tenants") && line.contains("ONLINE 488 GB/2.0 TB"),
        "{line}"
    );
    assert!(admin::node_enable(&db, "no-such-node", true).await.is_err());

    // A degraded pool is refused, and nothing changes.
    let degraded = Fake::new(vec![
        ("castor-zfs pool", Ok("pool 2000000 500000 DEGRADED\n")),
        PROBE[1],
        PROBE[2],
    ]);
    let c = control(degraded, &traefik, None, "http://127.0.0.1:9");
    let err = admin::node_add(
        &c,
        &db,
        &args(&[
            "add",
            &name,
            "castor@10.0.0.7",
            "--pool",
            "tank",
            "--edge",
            "10.0.0.7:8480",
            "--auth-host-ip",
            "10.0.0.2",
        ]),
    )
    .await
    .unwrap_err();
    assert!(err.ends_with("pool is DEGRADED, not registering"), "{err}");

    // A tenants node can't become a stacks node, and a stacks node needs userns-remap.
    let docker = |info: &'static str| {
        let mut rules = PROBE.to_vec();
        rules.push(("docker info", Ok(info)));
        Fake::new(rules)
    };
    let stacks = |n: &str| {
        args(&[
            "add",
            n,
            "castor@10.0.0.8",
            "--kind",
            "mac",
            "--role",
            "stacks",
            "--pool",
            "tank",
            "--edge",
            "10.0.0.8",
            "--control-plane-ip",
            "10.0.0.2",
        ])
    };
    let isolated = r#"{"SecurityOptions":["name=userns"],"Runtimes":{"runc":{},"runsc":{}},"MemTotal":8589934592,"NCPU":4,"Architecture":"aarch64"}"#;
    let c = control(docker(isolated), &traefik, None, "http://127.0.0.1:9");
    sqlx::query(r#"INSERT INTO "Tenant" (id, slug, "displayName", "nodeId", "updatedAt") SELECT $1, $1, 'T', id, now() FROM "Node" WHERE name = $2"#)
        .bind(format!("t{}", hex8()))
        .bind(&name)
        .execute(&db)
        .await
        .unwrap();
    let err = admin::node_add(&c, &db, &stacks(&name)).await.unwrap_err();
    assert!(err.contains("a stacks node must be a separate machine"), "{err}");
    let s = format!("s{}", hex8());
    let c = control(
        docker(r#"{"SecurityOptions":[],"Runtimes":{"runc":{}}}"#),
        &traefik,
        None,
        "http://127.0.0.1:9",
    );
    assert!(
        admin::node_add(&c, &db, &stacks(&s))
            .await
            .unwrap_err()
            .contains("userns-remap")
    );
    assert!(node_row(&db, &s).await.is_empty());
    let c = control(docker(isolated), &traefik, None, "http://127.0.0.1:9");
    admin::node_add(&c, &db, &stacks(&s)).await.unwrap();
    assert_eq!(
        node_row(&db, &s).await,
        ["MAC STACKS castor@10.0.0.8 tank 10.0.0.8 0 2000000 ONLINE 8192 4 10.0.0.2 100000 runsc t"]
    );
    assert!(
        admin::node_add(
            &c,
            &db,
            &args(&[
                "add",
                &s,
                "castor@10.0.0.8",
                "--role",
                "stacks",
                "--pool",
                "tank",
                "--edge",
                "x",
                "--control-plane-ip",
                "10.0.0.2;rm"
            ])
        )
        .await
        .unwrap_err()
        .contains("invalid --control-plane-ip")
    );

    let audit: Vec<String> =
        sqlx::query_scalar(r#"SELECT action FROM "AuditLog" WHERE details->>'name' = ANY($1) ORDER BY "createdAt""#)
            .bind(vec![name.clone(), s.clone()])
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(audit, ["node.registered", "node.drained", "node.registered"]);
    sqlx::query(r#"DELETE FROM "Tenant" WHERE "nodeId" IN (SELECT id FROM "Node" WHERE name = $1)"#)
        .bind(&name)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query(r#"DELETE FROM "Node" WHERE name = ANY($1)"#)
        .bind(vec![name, s])
        .execute(&db)
        .await
        .unwrap();
    let _ = std::fs::remove_dir_all(&traefik);
}

#[tokio::test]
async fn staff_and_tenants() {
    let Some(db) = db().await else { return };
    let user = format!("u{}", hex8());
    let email = format!("{user}@x.test");
    sqlx::query(r#"INSERT INTO "User" (id, "authentikSub", email) VALUES ($1, $1, $2)"#)
        .bind(&user)
        .bind(&email)
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(
        admin::staff_set(&db, &format!(" {} ", email.to_uppercase()), true)
            .await
            .unwrap(),
        format!("{email}: now staff")
    );
    assert!(admin::staff_list(&db).await.unwrap().lines().any(|l| l == email));
    admin::staff_set(&db, &email, false).await.unwrap();
    assert!(!admin::staff_list(&db).await.unwrap().contains(&email));
    assert!(
        admin::staff_set(&db, "nobody@x.test", true)
            .await
            .unwrap_err()
            .contains("sign in to the portal once")
    );
    sqlx::query(r#"DELETE FROM "User" WHERE id = $1"#)
        .bind(&user)
        .execute(&db)
        .await
        .unwrap();

    let w = world(&db, "TRUENAS").await;
    let out = admin::delete_tenant(&db, &w.slug).await.unwrap();
    assert!(out.starts_with(&format!("queued DELETE for {} (job c", w.slug)));
    assert_eq!(
        rows(
            &db,
            r#"SELECT kind::text || ' ' || status::text FROM "Job" WHERE "tenantId" = $1"#,
            &w.tenant
        )
        .await,
        ["DELETE QUEUED"]
    );
    assert!(admin::delete_tenant(&db, "no-such-tenant").await.is_err());
    cleanup(&db, &w).await;
}

#[tokio::test]
async fn restore_requests() {
    let Some(db) = db().await else { return };
    let w = world(&db, "TRUENAS").await;
    let listing: &'static str = format!(
        "{s} g1 cb-20260101T0000Z 1767225600 1048576\n{s} g2 cb-20260102T0000Z 1767312000 0\nother g1 cb-20260101T0000Z 1767225600 0\n",
        s = w.slug
    )
    .leak();
    let fake = Fake::new(vec![
        ("castor-backup list", Ok(listing)),
        ("castor-snap discard", Ok("")),
    ]);
    let c = control(fake.clone(), &w.traefik, None, "http://127.0.0.1:9");
    let s = c.backup.clone().unwrap();

    let list = admin::backup_list(&c, &s, &w.slug).await.unwrap();
    assert_eq!(
        list,
        "g1@cb-20260101T0000Z  2026-01-01T00:00:00.000Z  1 MB\ng2@cb-20260102T0000Z  2026-01-02T00:00:00.000Z  0 MB"
    );
    assert!(
        admin::backup_restore(&c, &db, &s, &w.slug, Some("g9@cb-x"), None)
            .await
            .unwrap_err()
            .starts_with("no backup g9@cb-x")
    );
    let out = admin::backup_restore(&c, &db, &s, &w.slug, None, None).await.unwrap();
    assert!(
        out.starts_with(&format!("queued RESTORE of {} from g2@cb-20260102T0000Z", w.slug)),
        "{out}"
    );
    assert_eq!(
        rows(
            &db,
            r#"SELECT kind::text || ' ' || params::text FROM "Job" WHERE "tenantId" = $1"#,
            &w.tenant
        )
        .await,
        [r#"RESTORE {"gen": "g2", "snap": "cb-20260102T0000Z"}"#]
    );
    // Not while a job is queued.
    assert!(
        admin::backup_restore(&c, &db, &s, &w.slug, None, None)
            .await
            .unwrap_err()
            .contains("job queued or running")
    );

    admin::backup_discard(&c, &db, &w.slug, "before-restore-20260102t0000")
        .await
        .unwrap();
    assert!(
        fake.calls()
            .iter()
            .any(|l| l.ends_with(&format!("castor-snap discard {} before-restore-20260102t0000", w.slug)))
    );
    assert!(admin::backup_discard(&c, &db, &w.slug, "x;rm -rf").await.is_err());
    assert!(castord::backup::db_get_cmd(&s, "../etc/passwd").is_err());
    assert!(castord::backup::db_get_cmd(&s, "cp-20260101T0230Z.sql.gz.enc").is_ok());
    cleanup(&db, &w).await;
}
