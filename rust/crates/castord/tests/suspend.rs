// SUSPEND against the real schema, with Docker and SSH faked: the commands
// it runs and the rows it leaves. Runs when TEST_DATABASE_URL is set.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use castord::{Handler, JobCx};
use common::*;
#[tokio::test]
async fn suspends_like_the_typescript_worker() {
    let Some(db) = db().await else { return };
    let w = world(&db, "MAC").await;
    let s = &w.slug;
    let fake = Fake::new(vec![("stacks-status", Ok("open 1 1024 1024\n"))]);
    let (url, seen) = gateway().await;
    let c = control(fake.clone(), &w.traefik, Some(&url), "http://127.0.0.1:9");
    let j = job(&db, &w.tenant, "SUSPEND").await;
    castord::suspend::Suspend(c)
        .run(&JobCx { db: db.clone() }, &j)
        .await
        .unwrap();

    let ssh = "DOCKER_HOST=ssh://deploy@10.0.0.5 DOCKER_CONTEXT= docker";
    let gate = "ssh -o BatchMode=yes -T deploy@10.0.0.5 castor-zfs";
    assert_eq!(
        fake.calls(),
        vec![
            format!("{ssh} compose -p pc-s-{s}--linkding stop"),
            format!("{ssh} exec pc-stacks-edge-traefik-1 rm -f /routes/pc-s-{s}--linkding.yml"),
            format!("{ssh} compose -p pc-s-{s}--ittools stop"),
            format!("{ssh} exec pc-stacks-edge-traefik-1 rm -f /routes/pc-s-{s}--ittools.yml"),
            format!("{gate} stacks-status {s}"),
            format!("{gate} stacks-close {s}"),
            format!(
                "{ssh} compose -p pc-t-{s} -f /repo/packages/stack-templates/tenant/compose.yml \
                 --project-directory /Users/Shared/castor/files/templates/tenant \
                 -f /repo/.tenants/{s}/compose.volumes.yml --env-file /repo/.tenants/{s}/tenant.env \
                 --profile photos --profile docs --profile places --profile flights --profile assistant stop"
            ),
            format!("{gate} close {s}"),
        ]
    );
    assert!(!w.traefik.join(format!("stacks-{s}.yml")).exists());
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            "GET /key/info?key=sk-tenant%20key auth=true ".to_owned(),
            r#"POST /key/block auth=true {"key":"sk-tenant key"}"#.to_owned(),
        ]
    );
    assert_eq!(
        rows(&db, r#"SELECT status::text || ':' || coalesce(error, '') FROM "Stack" WHERE "tenantId" = $1 ORDER BY "createdAt""#, &w.tenant).await,
        vec!["STOPPED:", "REMOVING:old"]
    );
    assert_eq!(
        rows(&db, r#"SELECT action || ' ' || details::text FROM "AuditLog" WHERE "tenantId" = $1 ORDER BY "createdAt", action"#, &w.tenant).await,
        vec![r#"stacks.lock {"reason": "suspend"}"#, r#"volume.lock {"reason": "suspend"}"#]
    );
    assert_eq!(
        rows(&db, r#"SELECT status::text FROM "Tenant" WHERE id = $1"#, &w.tenant).await,
        vec!["SUSPENDED"]
    );
    let log = rows(&db, r#"SELECT log FROM "Job" WHERE id = $1"#, &j.id).await;
    assert!(log[0].ends_with(" suspended\n"), "{log:?}");
    cleanup(&db, &w).await;
}

#[tokio::test]
async fn local_node_locks_through_cryptd_and_locked_stacks_stay_locked() {
    let Some(db) = db().await else { return };
    let w = world(&db, "LOCAL").await;
    let s = &w.slug;
    let fake = Fake::new(vec![("stacks-status", Ok("closed\n"))]);
    let j = job(&db, &w.tenant, "SUSPEND").await;
    castord::suspend::Suspend(control(fake.clone(), &w.traefik, None, "http://127.0.0.1:9"))
        .run(&JobCx { db: db.clone() }, &j)
        .await
        .unwrap();
    let calls = fake.calls();
    assert!(!calls.iter().any(|c| c.contains("stacks-close")), "{calls:?}");
    assert!(calls[5].starts_with(&format!("docker compose -p pc-t-{s} -f /repo/packages/stack-templates/tenant/compose.yml --project-directory /repo/packages/stack-templates/tenant ")), "{calls:?}");
    assert_eq!(
        calls[6],
        format!("docker run --rm -i --privileged -v /srv/pc:/srv/pc:rshared pc-cryptd:1 close {s}")
    );
    assert_eq!(
        rows(&db, r#"SELECT action FROM "AuditLog" WHERE "tenantId" = $1"#, &w.tenant).await,
        vec!["volume.lock"]
    );
    cleanup(&db, &w).await;
}

#[tokio::test]
async fn a_failed_lock_fails_the_attempt_and_leaves_the_tenant_active() {
    let Some(db) = db().await else { return };
    let w = world(&db, "MAC").await;
    let fake = Fake::new(vec![
        ("stacks-status", Ok("closed")),
        ("castor-zfs close", Err("ssh … failed: dataset busy")),
    ]);
    let j = job(&db, &w.tenant, "SUSPEND").await;
    let e = castord::suspend::Suspend(control(fake, &w.traefik, None, "http://127.0.0.1:9"))
        .run(&JobCx { db: db.clone() }, &j)
        .await
        .unwrap_err();
    assert_eq!(e, "ssh … failed: dataset busy");
    assert_eq!(
        rows(&db, r#"SELECT status::text FROM "Tenant" WHERE id = $1"#, &w.tenant).await,
        vec!["ACTIVE"]
    );
    cleanup(&db, &w).await;
}
