// Recurring tasks against the real schema, with SSH faked: the slots of
// schedule.ts and what the maintenance tasks do.
// Runs when TEST_DATABASE_URL is set.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use castord::node::NodeRow;
use castord::schedule::{Every, claim};
use castord::tasks::{MeasureStorage, SweepDeletions, Task, WatchNodes};
use common::*;
use sqlx::PgPool;

async fn next_run(db: &PgPool, name: &str) -> String {
    rows(
        db,
        r#"SELECT to_char("nextRunAt", 'YYYY-MM-DD HH24:MI') FROM "Schedule" WHERE name = $1"#,
        name,
    )
    .await
    .remove(0)
}

#[tokio::test]
async fn one_worker_per_slot() {
    let Some(db) = db().await else { return };
    let name = format!("test-{}", hex8());
    let every = Every::Ms(60_000);
    // A new interval task runs right away, once.
    assert!(claim(&db, &name, &every).await.unwrap());
    assert!(!claim(&db, &name, &every).await.unwrap());
    let due: bool = sqlx::query_scalar(
        r#"SELECT "nextRunAt" BETWEEN (now() AT TIME ZONE 'UTC') + interval '59 seconds' AND (now() AT TIME ZONE 'UTC') + interval '61 seconds' FROM "Schedule" WHERE name = $1"#,
    )
    .bind(&name)
    .fetch_one(&db)
    .await
    .unwrap();
    assert!(due);

    // A time of day runs first at its next slot, in the task's zone.
    let daily = format!("test-{}", hex8());
    let at = Every::At {
        time: "02:30",
        tz: "Europe/Zurich".into(),
        weekday: None,
    };
    assert!(!claim(&db, &daily, &at).await.unwrap());
    let local: String = sqlx::query_scalar(
        r#"SELECT to_char(("nextRunAt" AT TIME ZONE 'UTC') AT TIME ZONE 'Europe/Zurich', 'HH24:MI') FROM "Schedule" WHERE name = $1"#,
    )
    .bind(&daily)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(local, "02:30");
    // Due: the winner moves it to the next day's slot.
    sqlx::query(
        r#"UPDATE "Schedule" SET "nextRunAt" = now() AT TIME ZONE 'UTC' - interval '1 minute' WHERE name = $1"#,
    )
    .bind(&daily)
    .execute(&db)
    .await
    .unwrap();
    assert!(claim(&db, &daily, &at).await.unwrap());
    assert!(!claim(&db, &daily, &at).await.unwrap());
    let ahead: bool = sqlx::query_scalar(
        r#"SELECT "nextRunAt" > (now() AT TIME ZONE 'UTC') AND "nextRunAt" <= (now() AT TIME ZONE 'UTC') + interval '25 hours' FROM "Schedule" WHERE name = $1"#,
    )
    .bind(&daily)
    .fetch_one(&db)
    .await
    .unwrap();
    assert!(ahead, "{}", next_run(&db, &daily).await);

    // Weekly: on Sunday.
    let weekly = format!("test-{}", hex8());
    let sunday = Every::At {
        time: "04:00",
        tz: "Europe/Zurich".into(),
        weekday: Some(0),
    };
    assert!(!claim(&db, &weekly, &sunday).await.unwrap());
    let dow: f64 = sqlx::query_scalar(
        r#"SELECT extract(dow FROM ("nextRunAt" AT TIME ZONE 'UTC') AT TIME ZONE 'Europe/Zurich')::float8 FROM "Schedule" WHERE name = $1"#,
    )
    .bind(&weekly)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(dow, 0.0);

    assert!(
        claim(
            &db,
            "x",
            &Every::At {
                time: "25:00",
                tz: "UTC".into(),
                weekday: None
            }
        )
        .await
        .is_err()
    );
    sqlx::query(r#"DELETE FROM "Schedule" WHERE name = ANY($1)"#)
        .bind(vec![name, daily, weekly])
        .execute(&db)
        .await
        .unwrap();
}

#[tokio::test]
async fn sweeps_tenants_past_their_grace_period() {
    let Some(db) = db().await else { return };
    let w = world(&db, "TRUENAS").await;
    let job_count = || async {
        rows(
            &db,
            r#"SELECT count(*)::text FROM "Job" WHERE "tenantId" = $1 AND kind = 'DELETE'"#,
            &w.tenant,
        )
        .await
    };
    sqlx::query(r#"UPDATE "Tenant" SET status = 'SUSPENDED', "deleteAfter" = now() + interval '1 day' WHERE id = $1"#)
        .bind(&w.tenant)
        .execute(&db)
        .await
        .unwrap();
    SweepDeletions.run(&db).await.unwrap();
    assert_eq!(job_count().await, vec!["0"]);
    sqlx::query(r#"UPDATE "Tenant" SET "deleteAfter" = now() - interval '1 day' WHERE id = $1"#)
        .bind(&w.tenant)
        .execute(&db)
        .await
        .unwrap();
    SweepDeletions.run(&db).await.unwrap();
    assert_eq!(job_count().await, vec!["1"]);
    sqlx::query(r#"DELETE FROM "Job" WHERE "tenantId" = $1"#)
        .bind(&w.tenant)
        .execute(&db)
        .await
        .unwrap();
    cleanup(&db, &w).await;
}

#[tokio::test]
async fn measures_open_volumes() {
    let Some(db) = db().await else { return };
    let w = world(&db, "TRUENAS").await;
    let fake = Fake::new(vec![
        ("castor-zfs status", Ok("open 300 1024 102400\n")),
        ("castor-zfs usage", Ok("immich-data 250\npaperless-data 50\n")),
    ]);
    MeasureStorage(control(fake.clone(), &w.traefik, None, "http://127.0.0.1:9"))
        .run(&db)
        .await
        .unwrap();
    assert_eq!(
        rows(&db, r#"SELECT "storageUsedMb" || ' ' || "storageVolumeMb" || ' ' || ("storageMeasuredAt" IS NOT NULL) FROM "Tenant" WHERE id = $1"#, &w.tenant).await,
        vec!["300 102400 true"]
    );
    cleanup(&db, &w).await;
}

#[tokio::test]
async fn watching_records_nodes_and_reopens_locked_storage() {
    let Some(db) = db().await else { return };
    let w = world(&db, "TRUENAS").await;
    let w_ids = w.nodes.clone();
    let w_nodes = |ids: &Vec<String>| ids.clone();
    let fake = Fake::new(vec![
        ("castor-zfs pool", Ok("pool 2000000 500000 ONLINE\n")),
        ("castor-info", Ok("info 24.10 65536 12\n")),
        (
            "docker info",
            Ok(r#"{"SecurityOptions":["name=userns"],"Runtimes":{"runc":{}},"MemTotal":8589934592,"NCPU":4}"#),
        ),
        (" status ", Ok("closed\n")),
        ("stacks-status", Ok("closed\n")),
    ]);
    // Other tests' nodes share the database: only this test's own are watched.
    let watch = |fake| {
        let w = WatchNodes(control(fake, &w.traefik, None, "http://127.0.0.1:9"));
        let (db, nodes) = (db.clone(), w_nodes(&w_ids));
        async move {
            let mut errors = Vec::new();
            for id in nodes {
                if let Err(e) = w.watch(&db, &NodeRow::load(&db, &id).await.unwrap()).await {
                    errors.push(e);
                }
            }
            errors
        }
    };
    assert!(watch(fake.clone()).await.is_empty());
    let node = |id: &str| {
        let db = db.clone();
        let id = id.to_owned();
        async move {
            rows(&db, r#"SELECT concat_ws(' ', "poolSizeMb", "poolUsedMb", "poolHealth", "memoryMb", cpus, "lastSeenAt" IS NOT NULL, "lastError") FROM "Node" WHERE id = $1"#, &id).await
        }
    };
    // The tenant node by its host, the stacks node by its Docker engine.
    assert_eq!(node(&w.nodes[0]).await, vec!["2000000 500000 ONLINE 65536 12 t"]);
    assert_eq!(node(&w.nodes[1]).await, vec!["2000000 500000 ONLINE 8192 4 t"]);
    // The tenant's storage is reopened by an UPDATE; its stacks wait while that job is queued.
    assert_eq!(
        rows(
            &db,
            r#"SELECT kind::text FROM "Job" WHERE "tenantId" = $1 ORDER BY kind"#,
            &w.tenant
        )
        .await,
        vec!["UPDATE"]
    );
    sqlx::query(r#"DELETE FROM "Job" WHERE "tenantId" = $1"#)
        .bind(&w.tenant)
        .execute(&db)
        .await
        .unwrap();

    // With the tenant idle, its locked stacks (one RUNNING with storage) get a STACKS job.
    let row = NodeRow::load(&db, &w.nodes[1]).await.unwrap();
    WatchNodes(control(fake.clone(), &w.traefik, None, "http://127.0.0.1:9"))
        .watch(&db, &row)
        .await
        .unwrap();
    assert_eq!(
        rows(&db, r#"SELECT kind::text FROM "Job" WHERE "tenantId" = $1"#, &w.tenant).await,
        vec!["STACKS"]
    );
    sqlx::query(r#"DELETE FROM "Job" WHERE "tenantId" = $1"#)
        .bind(&w.tenant)
        .execute(&db)
        .await
        .unwrap();

    // A node that doesn't answer keeps its last facts and gets the error.
    let down = Fake::new(vec![(
        "castor-zfs pool",
        Err("ssh: connect to host 10.0.0.5: timed out"),
    )]);
    assert_eq!(watch(down).await.len(), 2);
    assert_eq!(
        node(&w.nodes[0]).await,
        vec!["2000000 500000 ONLINE 65536 12 t ssh: connect to host 10.0.0.5: timed out"]
    );
    cleanup(&db, &w).await;
}
