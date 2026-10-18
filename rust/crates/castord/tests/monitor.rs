// The monitor's rounds and checks against the real schema, with the checks'
// inputs and the alert channel faked. Runs when TEST_DATABASE_URL is set.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::{Arc, Mutex};

use castord::monitor::rules::{Finding, Kind, Notice, Severity};
use castord::monitor::{self, Check, Deliver, Settings, checks};
use common::*;
use sqlx::PgPool;

struct Fixed(&'static str, Mutex<Result<Vec<Finding>, String>>);

#[async_trait::async_trait]
impl Check for Fixed {
    fn scope(&self) -> &'static str {
        self.0
    }
    async fn run(&self, _now: i64) -> Result<Vec<Finding>, String> {
        self.1.lock().unwrap().clone()
    }
}

#[derive(Default)]
struct Inbox {
    got: Mutex<Vec<Vec<Notice>>>,
    down: Mutex<bool>,
}

#[async_trait::async_trait]
impl Deliver for Inbox {
    async fn deliver(&self, notices: &[Notice]) -> bool {
        if *self.down.lock().unwrap() {
            return false;
        }
        self.got.lock().unwrap().push(notices.to_vec());
        true
    }
}

impl Inbox {
    fn take(&self) -> Vec<(Kind, String)> {
        self.got
            .lock()
            .unwrap()
            .drain(..)
            .flatten()
            .map(|n| (n.kind, n.summary))
            .collect()
    }
}

/// (hits, notified, resolved) of an alert.
async fn alert(db: &PgPool, key: &str) -> (i32, bool, bool) {
    sqlx::query_as(r#"SELECT hits, "notifiedAt" IS NOT NULL, "resolvedAt" IS NOT NULL FROM "Alert" WHERE key = $1"#)
        .bind(key)
        .fetch_one(db)
        .await
        .unwrap()
}

/// Rounds over the shared Alert table: one test, so they don't race.
#[tokio::test]
async fn rounds() {
    let Some(db) = db().await else { return };
    let scope: &'static str = format!("mt{}", hex8()).leak();
    let key = |s: &str| format!("{scope}:{s}");
    // Earlier runs' leftovers would resolve here: start clean.
    sqlx::query(r#"DELETE FROM "Alert" WHERE key LIKE 'mt%'"#)
        .execute(&db)
        .await
        .unwrap();
    let check = Arc::new(Fixed(scope, Mutex::new(Ok(vec![]))));
    let checks: Vec<Arc<dyn Check>> = vec![check.clone()];
    let set = |f: Result<Vec<Finding>, String>| *check.1.lock().unwrap() = f;
    let inbox = Inbox::default();
    let renotify = 12 * 3_600_000;
    let now = monitor::now_ms();
    let space = Finding::new(key("space"), Severity::Warning, "pool space low");
    let flaky = Finding::new(key("flaky"), Severity::Critical, "service down").after(2);

    // New: notified once, the debounced one not yet.
    set(Ok(vec![space.clone(), flaky.clone()]));
    let r = monitor::round(&db, &checks, &inbox, renotify, now).await.unwrap();
    assert!(r.delivered);
    assert_eq!(inbox.take(), [(Kind::New, "pool space low".into())]);
    assert_eq!(alert(&db, &key("space")).await, (1, true, false));
    assert_eq!(alert(&db, &key("flaky")).await, (1, false, false));

    // Still there: quiet; the debounced one fires on its second hit.
    monitor::round(&db, &checks, &inbox, renotify, now + 1000)
        .await
        .unwrap();
    assert_eq!(inbox.take(), [(Kind::New, "service down".into())]);
    assert_eq!(alert(&db, &key("space")).await, (2, true, false));

    // The check fails: its alerts stay open, the failure is reported later (after 2).
    set(Err("boom".into()));
    let r = monitor::round(&db, &checks, &inbox, renotify, now + 2000)
        .await
        .unwrap();
    assert_eq!(r.skipped, [scope]);
    assert!(inbox.take().is_empty());
    assert!(!alert(&db, &key("space")).await.2);

    // Cleared, but the channel is down: not resolved until it was announced.
    set(Ok(vec![]));
    *inbox.down.lock().unwrap() = true;
    monitor::round(&db, &checks, &inbox, renotify, now + 3000)
        .await
        .unwrap();
    assert!(!alert(&db, &key("space")).await.2);
    *inbox.down.lock().unwrap() = false;
    monitor::round(&db, &checks, &inbox, renotify, now + 4000)
        .await
        .unwrap();
    let mut got = inbox.take();
    got.sort_by(|a, b| a.1.cmp(&b.1));
    assert_eq!(
        got,
        [
            (Kind::Resolved, "pool space low".into()),
            (Kind::Resolved, "service down".into()),
        ]
    );
    assert_eq!(alert(&db, &key("space")).await, (0, true, true));
    // Back again: new, with its notification reset.
    set(Ok(vec![space.clone()]));
    monitor::round(&db, &checks, &inbox, renotify, now + 5000)
        .await
        .unwrap();
    assert_eq!(inbox.take(), [(Kind::New, "pool space low".into())]);
    assert_eq!(alert(&db, &key("space")).await, (1, true, false));
    // A reminder after the renotify interval.
    monitor::round(&db, &checks, &inbox, renotify, now + 5000 + renotify)
        .await
        .unwrap();
    assert_eq!(inbox.take(), [(Kind::Reminder, "pool space low".into())]);

    // A one-off event: sent once, recorded as over.
    let event = Finding::new(key("event"), Severity::Critical, "break-glass").once();
    set(Ok(vec![event.clone()]));
    monitor::round(&db, &checks, &inbox, renotify, now + 6000 + renotify)
        .await
        .unwrap();
    monitor::round(&db, &checks, &inbox, renotify, now + 7000 + renotify)
        .await
        .unwrap();
    let got = inbox.take();
    assert!(got.contains(&(Kind::New, "break-glass".into())));
    assert_eq!(got.iter().filter(|(_, s)| s == "break-glass").count(), 1);
    assert!(alert(&db, &key("event")).await.1);
    assert!(alert(&db, &key("event")).await.2);

    sqlx::query(r#"DELETE FROM "Alert" WHERE key LIKE $1 OR key = $2"#)
        .bind(format!("{scope}:%"))
        .bind(format!("monitor:{scope}"))
        .execute(&db)
        .await
        .unwrap();
}

fn settings() -> Arc<Settings> {
    Arc::new(Settings {
        platform: platform(),
        // Nothing listens here.
        auth_url: "http://127.0.0.1:9".into(),
        authentik_token: None,
        backups_on: true,
        backup_interval_hours: 6.0,
        tls_hosts: vec![],
        tls_address: None,
        roots: monitor::tls::roots(None).unwrap(),
    })
}

#[tokio::test]
async fn checks_read_the_schema() {
    let Some(db) = db().await else { return };
    let w = world(&db, "MAC").await;
    let now = monitor::now_ms();
    // A node never reached.
    let nodes = checks::Nodes(db.clone()).run(now).await.unwrap();
    let mine = format!("node:{}:reachable", w.nodes[0]);
    let f = nodes.iter().find(|f| f.key == mine).unwrap();
    assert_eq!(f.summary, format!("Node {} has never been reached", w.nodes[0]));

    // A failed job, a stuck one.
    let j = job(&db, &w.tenant, "UPDATE").await;
    sqlx::query(r#"UPDATE "Job" SET status = 'FAILED', error = 'boom' WHERE id = $1"#)
        .bind(&j.id)
        .execute(&db)
        .await
        .unwrap();
    let jobs = checks::Jobs(db.clone()).run(now).await.unwrap();
    let f = jobs.iter().find(|f| f.key == format!("job:{}:failed", j.id)).unwrap();
    assert_eq!(f.summary, format!("UPDATE job for {} failed after all retries", w.slug));
    assert!(f.once);

    // Never backed up, created two days ago.
    sqlx::query(r#"UPDATE "Tenant" SET "createdAt" = now() AT TIME ZONE 'UTC' - interval '2 days' WHERE id = $1"#)
        .bind(&w.tenant)
        .execute(&db)
        .await
        .unwrap();
    let backups = checks::Backups {
        db: db.clone(),
        s: settings(),
    }
    .run(now)
    .await
    .unwrap();
    assert!(backups.iter().any(|f| f.key == format!("backup:{}:overdue", w.slug)));

    // Break-glass access in the audit log.
    castord::db::audit(&db, "u1", "staff.break_glass", Some(&w.tenant), None)
        .await
        .unwrap();
    castord::db::audit(&db, "u1", "volume.unlock", Some(&w.tenant), None)
        .await
        .unwrap();
    let audit = checks::Audit(db.clone()).run(now + 1000).await.unwrap();
    let mine: Vec<&Finding> = audit
        .iter()
        .filter(|f| f.summary == format!("Staff break-glass access to {}", w.slug))
        .collect();
    assert_eq!(mine.len(), 1);

    // Authentik unreachable; workers judged from the heartbeats.
    let platform = checks::Platform {
        db: db.clone(),
        http: castord::http::client(None).unwrap(),
        s: settings(),
    }
    .run(now)
    .await
    .unwrap();
    assert!(platform.iter().any(|f| f.key == "platform:authentik"));
    // Without a monitor token, the sign-in checks say they're off.
    let auth = checks::Auth {
        http: castord::http::client(None).unwrap(),
        s: settings(),
    }
    .run(now)
    .await
    .unwrap();
    assert_eq!(auth[0].key, "auth:off");

    cleanup(&db, &w).await;
}
