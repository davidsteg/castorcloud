// The queue against a real Postgres with the Prisma migrations applied:
// the same cases as packages/core/src/jobs.db.test.ts, so both workers keep
// one contract. Runs when TEST_DATABASE_URL is set (CI); passes trivially
// otherwise. Each test uses its own tenants.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use castord::queue;
use castord::{Handler, Job, JobCx, Worker};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

async fn db() -> Option<PgPool> {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        // CI sets REQUIRE_TEST_DATABASE so these tests can't pass by skipping.
        assert!(
            std::env::var_os("REQUIRE_TEST_DATABASE").is_none(),
            "TEST_DATABASE_URL is not set"
        );
        return None;
    };
    // A session far from UTC: every deadline must still hold.
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .after_connect(|c, _| {
            Box::pin(async move {
                sqlx::query("SET TIME ZONE 'Pacific/Kiritimati'").execute(c).await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .unwrap();
    Some(pool)
}

async fn tenant(db: &PgPool, name: &str) -> String {
    let id = format!("rs{}{name}{}", std::process::id(), rand_suffix());
    sqlx::query(r#"INSERT INTO "Tenant" (id, slug, "displayName", "updatedAt") VALUES ($1, $1, $2, now())"#)
        .bind(&id)
        .bind(name)
        .execute(db)
        .await
        .unwrap();
    id
}

fn rand_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    format!("{:x}", n % 0xffff_ffff)
}

async fn enqueue(db: &PgPool, tenant: &str, kind: &str) -> String {
    // What Prisma's create does: createdAt and runAfter from the client, in UTC.
    let id = format!("job{}", rand_suffix());
    sqlx::query(
        r#"INSERT INTO "Job" (id, "tenantId", kind, "createdAt", "runAfter")
           VALUES ($1, $2, $3::"JobKind", clock_timestamp() AT TIME ZONE 'UTC', now() AT TIME ZONE 'UTC')"#,
    )
    .bind(&id)
    .bind(tenant)
    .bind(kind)
    .execute(db)
    .await
    .unwrap();
    id
}

async fn status(db: &PgPool, id: &str) -> String {
    sqlx::query_scalar(r#"SELECT status::text FROM "Job" WHERE id = $1"#)
        .bind(id)
        .fetch_one(db)
        .await
        .unwrap()
}

async fn cleanup(db: &PgPool, tenants: &[&str]) {
    let t: Vec<String> = tenants.iter().map(|s| (*s).to_owned()).collect();
    sqlx::query(r#"DELETE FROM "Tenant" WHERE id = ANY($1)"#)
        .bind(&t)
        .execute(db)
        .await
        .unwrap();
}

#[tokio::test]
async fn one_job_per_tenant_in_order() {
    let Some(db) = db().await else { return };
    let (a, b) = (tenant(&db, "a").await, tenant(&db, "b").await);
    let a1 = enqueue(&db, &a, "PROVISION").await;
    let a2 = enqueue(&db, &a, "UPDATE").await;
    let b1 = enqueue(&db, &b, "PROVISION").await;
    // Each test uses its own job kinds: they run in parallel on one table.
    let claim = |w: &'static str| {
        let db = db.clone();
        async move { queue::claim(&db, w, &["PROVISION", "UPDATE"]).await.unwrap() }
    };
    let first = claim("w1").await.unwrap();
    assert_eq!((first.id.as_str(), first.attempts), (a1.as_str(), 1));
    assert_eq!(claim("w2").await.unwrap().id, b1);
    assert!(claim("w3").await.is_none());
    queue::succeed(&db, &a1, "w1").await.unwrap();
    queue::succeed(&db, &b1, "w2").await.unwrap();
    assert_eq!(claim("w1").await.unwrap().id, a2);
    queue::succeed(&db, &a2, "w1").await.unwrap();
    for id in [&a1, &a2, &b1] {
        assert_eq!(status(&db, id).await, "SUCCEEDED");
    }
    cleanup(&db, &[&a, &b]).await;
}

#[tokio::test]
async fn kinds_backoff_and_final_failure() {
    let Some(db) = db().await else { return };
    let a = tenant(&db, "k").await;
    let j = enqueue(&db, &a, "RESTORE").await;
    sqlx::query(r#"UPDATE "Job" SET "maxAttempts" = 2 WHERE id = $1"#)
        .bind(&j)
        .execute(&db)
        .await
        .unwrap();
    // A worker for other kinds doesn't take it.
    let mine = |job: Option<Job>| job.filter(|x| x.id == j);
    assert!(mine(queue::claim(&db, "w", &["RESUME"]).await.unwrap()).is_none());
    assert_eq!(
        mine(queue::claim(&db, "w", &["RESTORE"]).await.unwrap())
            .unwrap()
            .attempts,
        1
    );
    assert!(!queue::fail(&db, &j, "w", "boom").await.unwrap());
    let wait: f64 = sqlx::query_scalar(
        r#"SELECT EXTRACT(EPOCH FROM ("runAfter" - (now() AT TIME ZONE 'UTC')))::float8 FROM "Job" WHERE id = $1"#,
    )
    .bind(&j)
    .fetch_one(&db)
    .await
    .unwrap();
    assert!((10.0..=15.0).contains(&wait), "first retry in ~15 s, got {wait}");
    assert_eq!(status(&db, &j).await, "QUEUED");
    assert!(
        mine(queue::claim(&db, "w", &["RESTORE"]).await.unwrap()).is_none(),
        "not before its time"
    );

    sqlx::query(r#"UPDATE "Job" SET "runAfter" = (now() AT TIME ZONE 'UTC') - interval '1 second' WHERE id = $1"#)
        .bind(&j)
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(
        mine(queue::claim(&db, "w", &["RESTORE"]).await.unwrap())
            .unwrap()
            .attempts,
        2
    );
    assert!(queue::fail(&db, &j, "w", "boom again").await.unwrap());
    assert_eq!(status(&db, &j).await, "FAILED");
    assert_eq!(queue::backoff(1).as_secs(), 15);
    assert_eq!(queue::backoff(4).as_secs(), 120);
    cleanup(&db, &[&a]).await;
}

#[tokio::test]
async fn dead_workers_jobs_are_recovered_and_late_results_ignored() {
    let Some(db) = db().await else { return };
    let a = tenant(&db, "r").await;
    let j = enqueue(&db, &a, "SUSPEND").await;
    let got = queue::claim(&db, "dead", &["SUSPEND"]).await.unwrap().unwrap();
    assert_eq!(got.id, j);
    assert_eq!(got.kind, "SUSPEND");
    assert!(queue::renew(&db, &j, "dead").await.unwrap());
    assert!(!queue::recover_expired(&db).await.unwrap().iter().any(|r| r.job.id == j));
    sqlx::query(r#"UPDATE "Job" SET "lockedUntil" = (now() AT TIME ZONE 'UTC') - interval '1 second' WHERE id = $1"#)
        .bind(&j)
        .execute(&db)
        .await
        .unwrap();
    let rec = queue::recover_expired(&db).await.unwrap();
    assert!(rec.iter().any(|r| r.job.id == j && !r.failed));
    assert!(!queue::renew(&db, &j, "dead").await.unwrap());
    assert_eq!(status(&db, &j).await, "QUEUED");
    cleanup(&db, &[&a]).await;
}

#[tokio::test]
async fn racing_claims_take_one_job_per_tenant() {
    let Some(db) = db().await else { return };
    let a = tenant(&db, "race").await;
    let mut jobs = Vec::new();
    for _ in 0..4 {
        jobs.push(enqueue(&db, &a, "DELETE").await);
    }
    let mut set = tokio::task::JoinSet::new();
    for n in 0..6 {
        let db = db.clone();
        set.spawn(async move { queue::claim(&db, &format!("racer{n}"), &["DELETE"]).await.unwrap() });
    }
    let mut ours = 0;
    while let Some(r) = set.join_next().await {
        if let Some(j) = r.unwrap()
            && jobs.contains(&j.id)
        {
            ours += 1;
            assert_eq!(j.id, jobs[0], "the oldest first");
        }
    }
    assert_eq!(ours, 1);
    cleanup(&db, &[&a]).await;
}

struct Fake {
    kind: &'static str,
    ok: bool,
    runs: AtomicUsize,
    gave_up: AtomicUsize,
}

#[async_trait::async_trait]
impl Handler for Fake {
    fn kind(&self) -> &'static str {
        self.kind
    }
    async fn run(&self, cx: &JobCx, job: &Job) -> Result<(), String> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        cx.log(job, "working").await;
        if self.ok { Ok(()) } else { Err("it broke".into()) }
    }
    async fn gave_up(&self, _cx: &JobCx, _job: &Job, _error: &str) {
        self.gave_up.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn worker_runs_its_kinds_and_reports() {
    let Some(db) = db().await else { return };
    // REVOKE_ACCESS is used by this test only.
    let a = tenant(&db, "w").await;
    let ok = enqueue(&db, &a, "REVOKE_ACCESS").await;
    let fake = Arc::new(Fake {
        kind: "REVOKE_ACCESS",
        ok: true,
        runs: AtomicUsize::new(0),
        gave_up: AtomicUsize::new(0),
    });
    let w = Worker::new(
        format!("rs-test-{}", std::process::id()),
        db.clone(),
        vec![fake.clone()],
        &["REVOKE_ACCESS"],
    )
    .unwrap();
    while w.step().await.unwrap() {}
    assert_eq!(status(&db, &ok).await, "SUCCEEDED");
    let log: String = sqlx::query_scalar(r#"SELECT log FROM "Job" WHERE id = $1"#)
        .bind(&ok)
        .fetch_one(&db)
        .await
        .unwrap();
    assert!(log.ends_with(" working\n"), "{log}");
    let kinds: Vec<String> = sqlx::query_scalar(r#"SELECT unnest(kinds)::text FROM "WorkerHeartbeat" WHERE id = $1"#)
        .bind(&w.id)
        .fetch_all(&db)
        .await
        .unwrap();
    assert_eq!(kinds, ["REVOKE_ACCESS"]);

    let bad = enqueue(&db, &a, "REVOKE_ACCESS").await;
    sqlx::query(r#"UPDATE "Job" SET "maxAttempts" = 1 WHERE id = $1"#)
        .bind(&bad)
        .execute(&db)
        .await
        .unwrap();
    let failing = Arc::new(Fake {
        kind: "REVOKE_ACCESS",
        ok: false,
        runs: AtomicUsize::new(0),
        gave_up: AtomicUsize::new(0),
    });
    let w2 = Worker::new(
        format!("rs-test2-{}", std::process::id()),
        db.clone(),
        vec![failing.clone()],
        &["REVOKE_ACCESS"],
    )
    .unwrap();
    while w2.step().await.unwrap() {}
    assert_eq!(status(&db, &bad).await, "FAILED");
    assert_eq!(failing.gave_up.load(Ordering::SeqCst), 1);
    sqlx::query(r#"DELETE FROM "WorkerHeartbeat" WHERE id = ANY($1)"#)
        .bind([&w.id, &w2.id])
        .execute(&db)
        .await
        .unwrap();
    cleanup(&db, &[&a]).await;
}

#[test]
fn config_refuses_what_it_cannot_run() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let db = PgPoolOptions::new().connect_lazy("postgresql://x@localhost/x").unwrap();
        let fake: Arc<dyn Handler> = Arc::new(Fake {
            kind: "SUSPEND",
            ok: true,
            runs: AtomicUsize::new(0),
            gave_up: AtomicUsize::new(0),
        });
        let make = |take: &[&str]| {
            Worker::new("w".into(), db.clone(), vec![fake.clone()], take)
                .map(|_| ())
                .map_err(|e| e.to_string())
        };
        assert_eq!(make(&["SUSPEND"]), Ok(()));
        assert_eq!(make(&["SUSPND"]), Err("unknown job kind SUSPND".into()));
        assert_eq!(make(&["DELETE"]), Err("castord has no handler for DELETE yet".into()));
        assert!(make(&[]).is_err());
    });
}
