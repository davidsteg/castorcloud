// REVOKE_ACCESS against the real schema, with Docker, SSH and Authentik
// faked: one run per person with the strongest scope, the access
// commands in the booked apps, Authentik sessions ended for "sign out
// everywhere", the requests gone. Runs when TEST_DATABASE_URL is set.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use castord::{Handler, JobCx};
use common::*;
use sqlx::PgPool;

fn authentik(method: &str, path: &str) -> (u16, String) {
    let body = match (method, path) {
        ("DELETE", _) => return (204, String::new()),
        (_, p) if p.starts_with("/api/v3/core/users/?") => r#"{"results":[{"pk":42}]}"#,
        (_, "/api/v3/core/users/42/") => r#"{"pk":42,"username":"anna"}"#,
        (_, p) if p.starts_with("/api/v3/core/authenticated_sessions/?") => r#"{"results":[{"uuid":"s-1"}]}"#,
        (_, p) if p.starts_with("/api/v3/oauth2/refresh_tokens/?") => r#"{"results":[{"id":5}]}"#,
        _ => return (404, "{}".into()),
    };
    (200, body.into())
}

async fn setup(db: &PgPool, w: &World) {
    for (kind, enabled) in [
        ("PHOTOS", true),
        ("DOCS", false),
        ("FLIGHTS", true),
        ("ASSISTANT", true),
    ] {
        sqlx::query(
            r#"INSERT INTO "ServiceInstance" (id, "tenantId", kind, enabled) VALUES ($1, $2, $3::"ServiceKind", $4)"#,
        )
        .bind(format!("{}{kind}", w.tenant))
        .bind(&w.tenant)
        .bind(kind)
        .bind(enabled)
        .execute(db)
        .await
        .unwrap();
    }
    for (i, (sub, email, scope)) in [
        ("sub-a", "old@example.ch", "WEB"),
        ("sub-b", "bert@example.ch", "REMOVED"),
        ("sub-a", "anna@example.ch", "DEVICES"),
    ]
    .into_iter()
    .enumerate()
    {
        sqlx::query(
            r#"INSERT INTO "AccessRevocation" (id, "tenantId", email, sub, scope, "createdAt")
               VALUES ($1, $2, $3, $4, $5::"RevocationScope", TIMESTAMP '2026-10-16 10:00:00.123' + make_interval(secs => $6))"#,
        )
        .bind(format!("{}r{i}", w.tenant))
        .bind(&w.tenant)
        .bind(email)
        .bind(sub)
        .bind(scope)
        .bind(i as f64)
        .execute(db)
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn revokes_like_the_typescript_worker() {
    let Some(db) = db().await else { return };
    let w = world(&db, "MAC").await;
    let s = &w.slug;
    setup(&db, &w).await;
    let fake = Fake::new(vec![]);
    let (ak, ak_seen) = http("ak", authentik).await;
    let j = job(&db, &w.tenant, "REVOKE_ACCESS").await;
    castord::revoke::RevokeAccess(control(fake.clone(), &w.traefik, None, &ak))
        .run(&JobCx { db: db.clone() }, &j)
        .await
        .unwrap();

    let compose = format!(
        "DOCKER_HOST=ssh://deploy@10.0.0.5 DOCKER_CONTEXT= docker compose -p pc-t-{s} \
         -f /repo/packages/stack-templates/tenant/compose.yml \
         --project-directory /Users/Shared/castor/files/templates/tenant \
         -f /repo/.tenants/{s}/compose.volumes.yml --env-file /repo/.tenants/{s}/tenant.env \
         --profile photos --profile flights exec -T"
    );
    let immich = "immich-db psql -X -q -v ON_ERROR_STOP=1 -U immich -d immich";
    let airtrail = "db psql -X -q -v ON_ERROR_STOP=1 -U postgres -d airtrail";
    let keep = "keep=Castor assistant (read-only)";
    assert_eq!(
        fake.calls(),
        vec![
            // Anna: WEB and DEVICES -> DEVICES, with her latest address and time.
            format!(
                "{compose} {immich} -v email=anna@example.ch -v before=2026-10-16T10:00:02.123Z -v web=false -v {keep}"
            ),
            format!("{compose} {airtrail} -v sub=sub-a -v web=false -v {keep}"),
            // Bert left: the assistant's key goes too.
            format!(
                "{compose} {immich} -v email=bert@example.ch -v before=2026-10-16T10:00:01.123Z -v web=false -v keep="
            ),
            format!("{compose} {airtrail} -v sub=sub-b -v web=false -v keep="),
        ]
    );
    let writes: Vec<String> = ak_seen
        .lock()
        .unwrap()
        .iter()
        .filter(|l| !l.starts_with("GET "))
        .cloned()
        .collect();
    assert_eq!(
        writes,
        vec![
            "DELETE /api/v3/core/authenticated_sessions/s-1/ auth=true ",
            "DELETE /api/v3/oauth2/refresh_tokens/5/ auth=true ",
        ]
    );
    let t = &w.tenant;
    assert!(
        rows(&db, r#"SELECT id FROM "AccessRevocation" WHERE "tenantId" = $1"#, t)
            .await
            .is_empty()
    );
    assert_eq!(
        rows(&db, r#"SELECT details::text FROM "AuditLog" WHERE "tenantId" = $1 AND action = 'access.revoked' ORDER BY "createdAt""#, t).await,
        vec![
            r#"{"sub": "sub-a", "apps": ["photos", "flights"], "scope": "DEVICES"}"#,
            r#"{"sub": "sub-b", "apps": ["photos", "flights"], "scope": "REMOVED"}"#,
        ]
    );
    assert_eq!(
        rows(&db, r#"SELECT step FROM "Job" WHERE id = $1"#, &j.id).await,
        vec!["ready"]
    );
    cleanup(&db, &w).await;
}

#[tokio::test]
async fn requests_wait_while_the_apps_are_stopped() {
    let Some(db) = db().await else { return };
    let w = world(&db, "MAC").await;
    setup(&db, &w).await;
    sqlx::query(r#"UPDATE "Tenant" SET status = 'SUSPENDED' WHERE id = $1"#)
        .bind(&w.tenant)
        .execute(&db)
        .await
        .unwrap();
    let fake = Fake::new(vec![]);
    let j = job(&db, &w.tenant, "REVOKE_ACCESS").await;
    castord::revoke::RevokeAccess(control(fake.clone(), &w.traefik, None, "http://127.0.0.1:9"))
        .run(&JobCx { db: db.clone() }, &j)
        .await
        .unwrap();
    assert!(fake.calls().is_empty());
    assert_eq!(
        rows(
            &db,
            r#"SELECT id FROM "AccessRevocation" WHERE "tenantId" = $1"#,
            &w.tenant
        )
        .await
        .len(),
        3
    );
    let log = rows(&db, r#"SELECT log FROM "Job" WHERE id = $1"#, &j.id).await;
    assert!(
        log[0].ends_with(" identity: apps not running (SUSPENDED); revocations wait for the next start\n"),
        "{log:?}"
    );
    cleanup(&db, &w).await;
}

/// On a TrueNAS node every step is a tool of the castor gatekeeper catalog,
/// asked of the node role: the SQL is fixed in the tool, only typed values travel.
#[tokio::test]
async fn revokes_through_the_node_role_on_truenas() {
    let Some(db) = db().await else { return };
    let w = world(&db, "TRUENAS").await;
    let s = &w.slug;
    setup(&db, &w).await;
    let fake = Fake::new(vec![]);
    let (ak, _) = http("ak", authentik).await;
    let j = job(&db, &w.tenant, "REVOKE_ACCESS").await;
    castord::revoke::RevokeAccess(control(fake.clone(), &w.traefik, None, &ak))
        .run(&JobCx { db: db.clone() }, &j)
        .await
        .unwrap();
    let keep = "Castor assistant (read-only)";
    assert_eq!(
        fake.calls(),
        vec![
            format!(
                r#"gk docker.revoke_photos {{"slug":"{s}","email":"anna@example.ch","before":"2026-10-16T10:00:02.123Z","web":"false","keep":"{keep}"}}"#
            ),
            format!(r#"gk docker.revoke_flights {{"slug":"{s}","sub":"sub-a","web":"false","keep":"{keep}"}}"#),
            format!(
                r#"gk docker.revoke_photos {{"slug":"{s}","email":"bert@example.ch","before":"2026-10-16T10:00:01.123Z","web":"false","keep":""}}"#
            ),
            format!(r#"gk docker.revoke_flights {{"slug":"{s}","sub":"sub-b","web":"false","keep":""}}"#),
        ]
    );
    cleanup(&db, &w).await;
}
