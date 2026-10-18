// Backups against the real schema, with SSH and the backup box faked: the
// commands and the rows it leaves. Runs when TEST_DATABASE_URL is set.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::io::Read as _;

use castord::backup;
use common::*;
use sqlx::PgPool;

const NODE: &str = "ssh -o BatchMode=yes -T deploy@10.0.0.5";
const BOX: &str = "ssh -o BatchMode=yes -T backup@box.test castor-backup";

fn settings() -> backup::Settings {
    backup::Settings {
        target: "backup@box.test".into(),
        interval_hours: 6.0,
        db_public_key: None,
        pg_container: "pc-control-plane-postgres-1".into(),
        pg_user: "pc".into(),
    }
}

/// Only this test's tenant: other tests share the database.
fn mine(fake: &Fake, slug: &str) -> Vec<String> {
    fake.calls()
        .into_iter()
        .filter(|c| {
            c.contains(&format!(" {slug} "))
                || c.ends_with(&format!(" {slug}"))
                || c.ends_with(" list")
                || c.ends_with(" pool")
        })
        .collect()
}

async fn backup_row(db: &PgPool, tenant: &str) -> Vec<String> {
    rows(
        db,
        r#"SELECT concat_ws(' ', generation, coalesce("lastSnapshot", '-'), coalesce("lastBytes"::text, '-'), "lastSuccessAt" IS NOT NULL, failures, coalesce("lastError", '-'))
           FROM "TenantBackup" WHERE "tenantId" = $1"#,
        tenant,
    )
    .await
}

/// run_all backs up every active tenant in the database: one such test at a time.
static RUN_ALL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

#[tokio::test]
async fn incremental_when_the_chain_is_intact_else_a_new_generation() {
    let Some(db) = db().await else { return };
    let _one = RUN_ALL.lock().await;
    let w = world(&db, "MAC").await;
    let s = w.slug.clone();
    sqlx::query(r#"INSERT INTO "TenantBackup" ("tenantId", generation, "lastSnapshot", "updatedAt") VALUES ($1, 2, 'cb-20261015T0000Z', now())"#)
        .bind(&w.tenant)
        .execute(&db)
        .await
        .unwrap();
    let list = leak(format!(
        "{s} g1 cb-20261001T0000Z 1790000000 10\n{s} g2 cb-20261015T0000Z 1791000000 20\n"
    ));
    let fake = Fake::new(vec![
        ("castor-backup pool", Ok("pool 1000000 1000 ONLINE\n")),
        ("castor-backup list", Ok(list)),
        ("castor-snap bookmarks", Ok("cb-20261015T0000Z\n")),
        ("castor-backup recv", Ok("3145728")),
    ]);
    let c = control(fake.clone(), &w.traefik, None, "http://127.0.0.1:9");
    backup::run_all(&c, &db, &settings()).await.unwrap();
    let calls = mine(&fake, &s);
    let snap = calls
        .iter()
        .find_map(|c| c.strip_prefix(&format!("{NODE} castor-snap snapshot {s} ")))
        .unwrap()
        .to_owned();
    assert!(snap.starts_with("cb-2") && snap.ends_with('Z'), "{snap}");
    assert_eq!(
        calls,
        vec![
            format!("{BOX} pool"),
            format!("{BOX} list"),
            format!("{NODE} castor-snap snapshot {s} {snap}"),
            format!("{NODE} castor-snap bookmarks {s}"),
            format!("{NODE} castor-snap send {s} {snap} cb-20261015T0000Z | {BOX} recv {s} g2"),
            format!("{NODE} castor-snap sent {s} {snap}"),
            format!("{BOX} list"),
        ]
    );
    assert_eq!(
        backup_row(&db, &w.tenant).await,
        vec![format!("2 {snap} 3145728 t 0 -")]
    );

    // The node lost the bookmark: a full send into the next generation.
    let fake = Fake::new(vec![
        ("castor-backup pool", Ok("pool 1000000 1000 ONLINE\n")),
        ("castor-backup list", Ok(list)),
        ("castor-snap bookmarks", Ok("\n")),
        ("castor-backup recv", Ok("9")),
    ]);
    let c = control(fake.clone(), &w.traefik, None, "http://127.0.0.1:9");
    backup::run_all(&c, &db, &settings()).await.unwrap();
    let send = mine(&fake, &s)
        .into_iter()
        .find(|c| c.contains("castor-snap send"))
        .unwrap();
    assert!(
        send.ends_with(&format!("| {BOX} recv {s} g3")) && !send.contains("cb-20261015T0000Z"),
        "{send}"
    );

    // A tenant with a job in flight is left for the next run.
    let j = job(&db, &w.tenant, "UPDATE").await;
    let fake = Fake::new(vec![("castor-backup pool", Ok("pool 1 1 ONLINE\n"))]);
    backup::run_all(
        &control(fake.clone(), &w.traefik, None, "http://127.0.0.1:9"),
        &db,
        &settings(),
    )
    .await
    .unwrap();
    assert!(!fake.calls().iter().any(|c| c.contains(&format!("snapshot {s} "))));
    sqlx::query(r#"DELETE FROM "Job" WHERE id = $1"#)
        .bind(&j.id)
        .execute(&db)
        .await
        .unwrap();
    cleanup(&db, &w).await;
}

#[tokio::test]
async fn a_failed_send_is_recorded_once_and_its_snapshot_dropped() {
    let Some(db) = db().await else { return };
    let _one = RUN_ALL.lock().await;
    let w = world(&db, "MAC").await;
    let s = w.slug.clone();
    let fake = Fake::new(vec![
        ("castor-backup pool", Ok("pool 1000000 1000 ONLINE\n")),
        ("castor-backup recv", Err("cannot receive: out of space")),
    ]);
    let c = control(fake.clone(), &w.traefik, None, "http://127.0.0.1:9");
    for _ in 0..2 {
        backup::run_all(&c, &db, &settings()).await.unwrap();
    }
    assert_eq!(
        backup_row(&db, &w.tenant).await,
        vec!["0 - - f 2 cannot receive: out of space"]
    );
    assert!(
        mine(&fake, &s)
            .iter()
            .any(|c| c.starts_with(&format!("{NODE} castor-snap drop {s} cb-")))
    );
    // Only the first failure is audited.
    assert_eq!(
        rows(
            &db,
            r#"SELECT action || ' ' || details::text FROM "AuditLog" WHERE "tenantId" = $1"#,
            &w.tenant
        )
        .await,
        vec![r#"backup.failed {"error": "cannot receive: out of space"}"#]
    );
    // Overdue and failing, for monitoring.
    sqlx::query(r#"UPDATE "Tenant" SET "createdAt" = now() - interval '2 days' WHERE id = $1"#)
        .bind(&w.tenant)
        .execute(&db)
        .await
        .unwrap();
    let p = backup::problems(&db, &settings()).await.unwrap();
    let (_, issues) = p.iter().find(|(slug, _)| *slug == s).unwrap();
    assert_eq!(
        issues,
        &vec![
            "no backup since ever".to_owned(),
            "last run failed (2x): cannot receive: out of space".to_owned()
        ]
    );
    cleanup(&db, &w).await;
}

#[tokio::test]
async fn an_unhealthy_box_gets_nothing() {
    let Some(db) = db().await else { return };
    let _one = RUN_ALL.lock().await;
    let w = world(&db, "MAC").await;
    let fake = Fake::new(vec![("castor-backup pool", Ok("pool 1000000 1000 DEGRADED\n"))]);
    let c = control(fake.clone(), &w.traefik, None, "http://127.0.0.1:9");
    let err = backup::run_all(&c, &db, &settings()).await.unwrap_err();
    assert_eq!(err, "backup box pool is DEGRADED; not sending");
    assert_eq!(fake.calls(), vec![format!("{BOX} pool")]);
    sqlx::query(r#"DELETE FROM "AuditLog" WHERE action = 'backup.box_unhealthy'"#)
        .execute(&db)
        .await
        .unwrap();
    cleanup(&db, &w).await;
}

#[tokio::test]
async fn prunes_by_policy_forgets_deleted_tenants_and_verifies() {
    let Some(db) = db().await else { return };
    // Its tenant is active, so a concurrent run_all would back it up too.
    let _one = RUN_ALL.lock().await;
    let w = world(&db, "MAC").await;
    let s = w.slug.clone();
    let gone = format!("gone{}", hex8());
    // Two old backups on one day: the older goes; the box refuses one, which is fine.
    let list = leak(format!(
        "{s} g1 cb-20250101T0000Z 1735689600 1\n{s} g1 cb-20250101T0600Z 1735711200 1\n{s} g1 cb-20250102T0000Z 1735776000 1\n{gone} g1 cb-20250101T0000Z 1735689600 1\n"
    ));
    let fake = Fake::new(vec![
        ("castor-backup list", Ok(list)),
        (
            "castor-backup forget",
            Err("refused: newest backup younger than 7 days"),
        ),
        ("castor-backup verify", Err("checksum mismatch")),
    ]);
    let c = control(fake.clone(), &w.traefik, None, "http://127.0.0.1:9");
    backup::prune(&c, &db, &settings()).await.unwrap();
    assert_eq!(
        fake.calls(),
        vec![
            format!("{BOX} list"),
            format!("{BOX} prune {s} g1 cb-20250101T0000Z"),
            format!("{BOX} forget {gone}"),
        ]
    );
    backup::verify_all(&c, &db, &settings()).await.unwrap();
    assert_eq!(
        fake.calls().last().unwrap(),
        &format!("{BOX} verify {s} g1 cb-20250102T0000Z")
    );
    assert_eq!(
        rows(
            &db,
            r#"SELECT coalesce("verifyError", '-') FROM "TenantBackup" WHERE "tenantId" = $1"#,
            &w.tenant
        )
        .await,
        vec!["checksum mismatch"]
    );
    assert_eq!(
        rows(
            &db,
            r#"SELECT action || ' ' || details::text FROM "AuditLog" WHERE "tenantId" = $1"#,
            &w.tenant
        )
        .await,
        vec![r#"backup.verify_failed {"snap": "g1@cb-20250102T0000Z", "error": "checksum mismatch"}"#]
    );
    cleanup(&db, &w).await;
}

#[tokio::test]
async fn database_dumps_are_sealed_to_the_offline_key() {
    let Some(db) = db().await else { return };
    let w = world(&db, "MAC").await;
    let keys: serde_json::Value =
        serde_json::from_str(include_str!("../../crypto/tests/fixtures/db-dump.json")).unwrap();
    let fake = Fake::new(vec![("pg_dumpall", Ok("-- PostgreSQL database cluster dump\n"))]);
    let c = control(fake.clone(), &w.traefik, None, "http://127.0.0.1:9");
    let mut s = settings();
    assert!(
        backup::dump_database(&c, &s)
            .await
            .unwrap_err()
            .contains("NOT backed up")
    );
    s.db_public_key = Some(keys["publicKey"].as_str().unwrap().into());
    let name = backup::dump_database(&c, &s).await.unwrap();
    assert!(
        name.starts_with("cp-2") && name.ends_with(".sql.gz.enc") && name.contains('t'),
        "{name}"
    );
    let calls = fake.calls();
    assert_eq!(
        calls,
        vec![
            "docker exec pc-control-plane-postgres-1 pg_dumpall -U pc --clean --if-exists".to_owned(),
            format!("{BOX} db-put {name}"),
        ]
    );
    let sealed = fake.inputs()[1].clone().unwrap();
    let gz = castor_crypto::dump::open(&sealed, keys["secretKey"].as_str().unwrap()).unwrap();
    let mut sql = String::new();
    flate2::read::GzDecoder::new(gz.as_slice())
        .read_to_string(&mut sql)
        .unwrap();
    assert_eq!(sql, "-- PostgreSQL database cluster dump\n");
    cleanup(&db, &w).await;
}
