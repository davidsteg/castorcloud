// RESTORE against the real schema, with SSH faked: the commands it
// runs and the rows it leaves.
// Runs when TEST_DATABASE_URL is set.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use castord::{Handler, JobCx};
use common::*;
use sqlx::PgPool;

const GATE: &str = "ssh -o BatchMode=yes -T deploy@10.0.0.5";

async fn restore_job(db: &PgPool, w: &World, params: &str) -> castord::Job {
    let j = job(db, &w.tenant, "RESTORE").await;
    sqlx::query(r#"UPDATE "Job" SET params = $2::jsonb WHERE id = $1"#)
        .bind(&j.id)
        .bind(params)
        .execute(db)
        .await
        .unwrap();
    // The stack restarts as after an UPDATE; here it stops at a known refusal.
    sqlx::query(
        r#"INSERT INTO "ServiceInstance" (id, "tenantId", kind, enabled) VALUES ($1, $1, 'ASSISTANT', true)
           ON CONFLICT DO NOTHING"#,
    )
    .bind(&w.tenant)
    .execute(db)
    .await
    .unwrap();
    sqlx::query(r#"UPDATE "Tenant" SET "storageUnits" = 2 WHERE id = $1"#)
        .bind(&w.tenant)
        .execute(db)
        .await
        .unwrap();
    j
}

fn restore(fake: std::sync::Arc<Fake>, w: &World) -> castord::restore::Restore {
    let c = control(fake, &w.traefik, None, "http://127.0.0.1:9");
    c.prepared.lock().unwrap().insert(w.nodes[0].clone());
    castord::restore::Restore(c)
}

#[tokio::test]
async fn restores_like_the_typescript_worker() {
    let Some(db) = db().await else { return };
    let w = world(&db, "MAC").await;
    let s = &w.slug;
    let fake = Fake::new(vec![
        ("castor-zfs status", Ok("open 10 20 30\n")),
        ("castor-snap set-aside", Ok("set aside x\n")),
        ("castor-backup send", Ok("5242880")),
    ]);
    let j = restore_job(&db, &w, r#"{"gen": "g3", "snap": "cb-20261001T0200Z"}"#).await;
    let cx = JobCx { db: db.clone() };
    let h = restore(fake.clone(), &w);
    let err = h.run(&cx, &j).await.unwrap_err();
    assert_eq!(err, "assistant booked but the LLM gateway is not configured");

    let calls = fake.calls();
    let tag = calls[3].rsplit(' ').next().unwrap().to_owned();
    assert!(
        tag.starts_with("before-restore-2") && tag.len() == "before-restore-20261016t2219".len(),
        "{tag}"
    );
    assert_eq!(
        calls,
        vec![
            format!("{GATE} castor-zfs status {s}"),
            format!(
                "DOCKER_HOST=ssh://deploy@10.0.0.5 DOCKER_CONTEXT= docker compose -p pc-t-{s} \
                 -f /repo/packages/stack-templates/tenant/compose.yml \
                 --project-directory /Users/Shared/castor/files/templates/tenant \
                 -f /repo/.tenants/{s}/compose.volumes.yml --env-file /repo/.tenants/{s}/tenant.env \
                 --profile photos --profile docs --profile places --profile flights --profile assistant stop"
            ),
            format!("{GATE} castor-zfs close {s}"),
            format!("{GATE} castor-snap set-aside {s} {tag}"),
            format!(
                "ssh -o BatchMode=yes -T backup@box.test castor-backup send {s} g3 cb-20261001T0200Z | \
                 {GATE} castor-snap receive {s} 204800"
            ),
        ]
    );
    assert_eq!(
        rows(&db, r#"SELECT action || ' ' || details::text FROM "AuditLog" WHERE "tenantId" = $1 ORDER BY "createdAt", action"#, &w.tenant).await,
        vec![
            r#"volume.lock {"reason": "restore"}"#.to_owned(),
            format!(r#"backup.restored {{"from": "g3@cb-20261001T0200Z", "node": "{}", "setAside": "{tag}"}}"#, w.nodes[0]),
        ]
    );
    assert_eq!(
        rows(
            &db,
            r#"SELECT coalesce("lastSnapshot", 'none') FROM "TenantBackup" WHERE "tenantId" = $1"#,
            &w.tenant
        )
        .await,
        vec!["none"]
    );
    assert_eq!(
        rows(&db, r#"SELECT params::text FROM "Job" WHERE id = $1"#, &j.id).await,
        vec![r#"{"gen": "g3", "snap": "cb-20261001T0200Z", "received": true}"#]
    );
    let log = rows(&db, r#"SELECT log FROM "Job" WHERE id = $1"#, &j.id).await;
    assert!(
        log[0].contains(&format!("current data set aside as {s}.{tag}")),
        "{log:?}"
    );
    assert!(log[0].contains("received g3@cb-20261001T0200Z, 5 MB"), "{log:?}");

    // A retry once the data is back only restarts the stack.
    let again = Fake::new(vec![]);
    let err = restore(again.clone(), &w).run(&cx, &j).await.unwrap_err();
    assert_eq!(err, "assistant booked but the LLM gateway is not configured");
    assert!(
        again.calls().iter().all(|c| !c.contains("castor-snap")),
        "{:?}",
        again.calls()
    );
    cleanup(&db, &w).await;
}

#[tokio::test]
async fn a_failed_receive_puts_the_data_back() {
    let Some(db) = db().await else { return };
    let w = world(&db, "MAC").await;
    let s = &w.slug;
    let fake = Fake::new(vec![
        ("castor-zfs status", Ok("closed\n")),
        ("castor-snap set-aside", Ok("set aside x\n")),
        ("castor-backup send", Err("receive failed: cannot receive")),
    ]);
    let j = restore_job(&db, &w, r#"{"gen": "g1", "snap": "cb-1"}"#).await;
    let err = restore(fake.clone(), &w)
        .run(&JobCx { db: db.clone() }, &j)
        .await
        .unwrap_err();
    assert_eq!(err, "receive failed: cannot receive");
    let calls = fake.calls();
    let tag = calls[3].rsplit(' ').next().unwrap();
    assert_eq!(calls.last().unwrap(), &format!("{GATE} castor-snap put-back {s} {tag}"));
    assert_eq!(
        rows(&db, r#"SELECT params::text FROM "Job" WHERE id = $1"#, &j.id).await,
        vec![r#"{"gen": "g1", "snap": "cb-1"}"#]
    );
    assert!(
        rows(
            &db,
            r#"SELECT action FROM "AuditLog" WHERE "tenantId" = $1 AND action = 'backup.restored'"#,
            &w.tenant
        )
        .await
        .is_empty()
    );
    cleanup(&db, &w).await;
}

#[tokio::test]
async fn refuses_bad_params_and_the_local_node() {
    let Some(db) = db().await else { return };
    let w = world(&db, "MAC").await;
    let cx = JobCx { db: db.clone() };
    for (params, msg) in [
        (r#"{"gen": "g1"}"#, "RESTORE needs params { gen, snap }"),
        (r#"{"gen": "g0", "snap": "cb-1"}"#, "invalid backup g0@cb-1"),
        (r#"{"gen": "g1", "snap": "../x"}"#, "invalid backup g1@../x"),
    ] {
        let fake = Fake::new(vec![]);
        let j = restore_job(&db, &w, params).await;
        assert_eq!(restore(fake.clone(), &w).run(&cx, &j).await.unwrap_err(), msg);
        assert!(fake.calls().is_empty());
        sqlx::query(r#"UPDATE "Job" SET status = 'FAILED' WHERE id = $1"#)
            .bind(&j.id)
            .execute(&db)
            .await
            .unwrap();
    }
    cleanup(&db, &w).await;

    let w = world(&db, "LOCAL").await;
    let fake = Fake::new(vec![("status", Ok("missing\n"))]);
    let j = restore_job(&db, &w, r#"{"gen": "g1", "snap": "cb-1"}"#).await;
    let err = restore(fake.clone(), &w).run(&cx, &j).await.unwrap_err();
    assert_eq!(err, format!("node {} has no ZFS datasets", w.nodes[0]));
    cleanup(&db, &w).await;
}
