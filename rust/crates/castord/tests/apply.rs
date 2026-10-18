// PROVISION/UPDATE/RESUME against the real schema, with Docker, SSH,
// Authentik, the LLM gateway and the apps faked: the commands and requests
// they make, and the rows they leave.
// Runs when TEST_DATABASE_URL is set.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;

use castord::{Control, Handler, JobCx};
use common::*;
use sqlx::PgPool;

fn authentik(method: &str, path: &str) -> (u16, String) {
    let body = match (method, path) {
        ("DELETE", _) => return (204, String::new()),
        ("GET", p) if p.starts_with("/api/v3/core/users/?") => r#"{"results":[{"pk":42}]}"#,
        ("GET", p) if p.starts_with("/api/v3/core/groups/?") => r#"{"results":[{"pk":"g1"}]}"#,
        ("GET", "/api/v3/core/groups/g1/?include_users=true") => {
            r#"{"users_obj":[{"pk":42,"email":"anna@example.ch","uid":"sub-a"},{"pk":43,"email":"old@example.ch","uid":"sub-old"}]}"#
        }
        ("GET", p) if p.starts_with("/api/v3/flows/") => r#"{"results":[{"pk":"flow"}]}"#,
        ("GET", p) if p.starts_with("/api/v3/propertymappings/") => r#"{"results":[{"pk":"map"}]}"#,
        ("GET", p) if p.starts_with("/api/v3/crypto/") => r#"{"results":[{"pk":"key"}]}"#,
        ("GET", _) => r#"{"results":[]}"#,
        ("POST", "/api/v3/providers/oauth2/") => r#"{"pk":7}"#,
        ("POST", "/api/v3/core/applications/") => r#"{"pk":"app-1"}"#,
        _ => "{}",
    };
    (200, body.into())
}

fn llm(_: &str, path: &str) -> (u16, String) {
    if path.starts_with("/key/info") {
        (404, "{}".into())
    } else {
        (200, "{}".into())
    }
}

/// Every app answers its health check; Immich already has its admin.
fn apps(method: &str, _: &str) -> (u16, String) {
    if method == "POST" {
        (400, "{}".into())
    } else {
        (200, "{}".into())
    }
}

/// The tests share the users "sub-a" and "sub-b" (the fake Authentik knows them).
static USERS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn setup(db: &PgPool, w: &World) -> Vec<String> {
    let t = &w.tenant;
    sqlx::query(r#"UPDATE "Tenant" SET status = 'PENDING', "aiCreditChf" = 10 WHERE id = $1"#)
        .bind(t)
        .execute(db)
        .await
        .unwrap();
    sqlx::query(r#"UPDATE "Node" SET "edgeHost" = '10.0.0.5:8480', "authHostIp" = '10.0.0.1' WHERE id = $1"#)
        .bind(&w.nodes[0])
        .execute(db)
        .await
        .unwrap();
    // The first apply creates the volume key.
    sqlx::query(r#"DELETE FROM "VolumeKey" WHERE "tenantId" = $1"#)
        .bind(t)
        .execute(db)
        .await
        .unwrap();
    for (kind, enabled) in [("PHOTOS", true), ("DOCS", false), ("ASSISTANT", true)] {
        sqlx::query(
            r#"INSERT INTO "ServiceInstance" (id, "tenantId", kind, enabled) VALUES ($1, $2, $3::"ServiceKind", $4)"#,
        )
        .bind(format!("{t}{kind}"))
        .bind(t)
        .bind(kind)
        .bind(enabled)
        .execute(db)
        .await
        .unwrap();
    }
    let mut users = Vec::new();
    // Subs and e-mails are unique: a run that failed may have left them.
    sqlx::query(r#"DELETE FROM "User" WHERE "authentikSub" IN ('sub-a', 'sub-b')"#)
        .execute(db)
        .await
        .unwrap();
    for (i, (email, sub, role)) in [
        ("anna@example.ch", "sub-a", "OWNER"),
        ("bert@example.ch", "sub-b", "MEMBER"),
    ]
    .into_iter()
    .enumerate()
    {
        let user = format!("u{}", hex8());
        sqlx::query(r#"INSERT INTO "User" (id, "authentikSub", email, name) VALUES ($1, $2, $3, 'N')"#)
            .bind(&user)
            .bind(sub)
            .bind(email)
            .execute(db)
            .await
            .unwrap();
        sqlx::query(
            r#"INSERT INTO "Member" (id, "tenantId", "userId", role, "createdAt")
               VALUES ($1, $2, $3, $4::"MemberRole", now() + make_interval(secs => $5))"#,
        )
        .bind(format!("m{i}{}", hex8()))
        .bind(t)
        .bind(&user)
        .bind(role)
        .bind(i as f64)
        .execute(db)
        .await
        .unwrap();
        users.push(user);
    }
    users
}

async fn teardown(db: &PgPool, w: &World, users: &[String]) {
    cleanup(db, w).await;
    sqlx::query(r#"DELETE FROM "User" WHERE id = ANY($1)"#)
        .bind(users)
        .execute(db)
        .await
        .unwrap();
}

/// The apps' hosts resolve to the fake apps server; the control plane's
/// files go to `root`.
fn with_apps(c: Arc<Control>, slug: &str, apps_url: &str, root: &std::path::Path) -> Arc<Control> {
    let addr: std::net::SocketAddr = apps_url.trim_start_matches("http://").parse().unwrap();
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .resolve(&format!("photos-{slug}.castor.test"), addr)
        .resolve(&format!("assistant-{slug}.castor.test"), addr)
        .build()
        .unwrap();
    let mut c = Arc::into_inner(c).unwrap();
    c.http = http;
    c.root = root.to_path_buf();
    Arc::new(c)
}

/// The revocation's time is the row's.
fn mask(line: &str) -> String {
    match line.find("before=") {
        Some(i) => {
            let end = line[i..].find(' ').map_or(line.len(), |e| i + e);
            format!("{}before=*{}", &line[..i], &line[end..])
        }
        None => line.to_owned(),
    }
}

#[tokio::test]
async fn provisions_like_the_typescript_worker() {
    let Some(db) = db().await else { return };
    let _one = USERS.lock().await;
    let w = world(&db, "TRUENAS").await;
    let (s, t) = (&w.slug, &w.tenant);
    let users = setup(&db, &w).await;
    let fake = Fake::new(vec![
        ("castor-zfs status", Ok("open 10 102400 102400\n")),
        ("castor-zfs usage", Ok("photos 5\nimmich-db 2\ndb 1\n")),
        ("com.docker.compose.service=paperless", Ok("abc\ndef\n")),
    ]);
    let (ak, ak_seen) = http("ak", authentik).await;
    let (gw, gw_seen) = http("master", llm).await;
    let (apps_url, apps_seen) = http("none", apps).await;
    let root = std::env::temp_dir().join(format!("castord-root-{s}"));
    let r = root.to_string_lossy().into_owned();
    let c = with_apps(control(fake.clone(), &w.traefik, Some(&gw), &ak), s, &apps_url, &root);
    c.prepared.lock().unwrap().insert(w.nodes[0].clone());
    let j = job(&db, t, "PROVISION").await;
    castord::apply::Provision(c.clone())
        .run(&JobCx { db: db.clone() }, &j)
        .await
        .unwrap();

    let ssh = "DOCKER_HOST=ssh://deploy@10.0.0.5 DOCKER_CONTEXT= docker";
    let gate = "ssh -o BatchMode=yes -T deploy@10.0.0.5";
    let compose = format!(
        "{ssh} compose -p pc-t-{s} -f {r}/packages/stack-templates/tenant/compose.yml \
         --project-directory /mnt/tank/castor/node/files/templates/tenant \
         -f {r}/.tenants/{s}/compose.volumes.yml --env-file {r}/.tenants/{s}/tenant.env \
         --profile photos --profile assistant"
    );
    let calls = fake.calls();
    let expect = [
        format!(
            "{gate} castor-zfs create {s} 102400 db redis immich-db photos docs-data docs-media docs-consume places-public places-watched places-storage flights-uploads assistant"
        ),
        format!("{gate} castor-zfs status {s}"),
        format!("{gate} castor-zfs quota {s} 102400"),
        format!("{gate} castor-put tenants/{s}/immich.json"),
        format!("{gate} castor-rm tenants/{s}/assistant/hermes/profiles"),
    ];
    assert_eq!(calls[..5], expect[..], "{calls:#?}");
    let puts: Vec<&String> = calls
        .iter()
        .filter(|c| c.contains("castor-put tenants/") && c.contains("/assistant/"))
        .collect();
    assert_eq!(puts.len(), 4 + 3 * 2, "{puts:#?}");
    assert!(puts.iter().any(|p| p.ends_with("assistant/members.json 644")));
    let rest: Vec<String> = calls.iter().skip(5 + puts.len()).map(|l| mask(l)).collect();
    let photos_only = compose.replace(" --profile assistant", "");
    assert_eq!(
        rest,
        vec![
            format!(
                "LLM_GATEWAY_BIND=10.0.0.5 {ssh} compose -f {r}/infra/node/compose.yml --project-directory /mnt/tank/castor/node/files/node --env-file /repo/packages/stack-templates/versions.env up -d"
            ),
            format!("{compose} up -d --remove-orphans"),
            format!("{ssh} network connect pc-t-{s}-shared pc-node-immich-ml"),
            format!("{ssh} network connect pc-t-{s}-shared pc-node-tika"),
            format!("{ssh} network connect pc-t-{s}-shared pc-node-gotenberg"),
            format!("{ssh} network connect pc-t-{s}-llm pc-node-llm-gateway"),
            format!("{compose} up -d --force-recreate assistant-init assistant"),
            format!(
                "{ssh} ps -aq --filter label=com.docker.compose.project=pc-t-{s} --filter label=com.docker.compose.service=paperless"
            ),
            format!("{ssh} rm -f abc def"),
            format!(
                "{ssh} ps -aq --filter label=com.docker.compose.project=pc-t-{s} --filter label=com.docker.compose.service=dawarich"
            ),
            format!(
                "{ssh} ps -aq --filter label=com.docker.compose.project=pc-t-{s} --filter label=com.docker.compose.service=dawarich-sidekiq"
            ),
            format!(
                "{ssh} ps -aq --filter label=com.docker.compose.project=pc-t-{s} --filter label=com.docker.compose.service=airtrail"
            ),
            // The removed member, in the booked app; the assistant's key goes too.
            format!(
                "{photos_only} exec -T immich-db psql -X -q -v ON_ERROR_STOP=1 -U immich -d immich -v email=old@example.ch -v before=* -v web=false -v keep="
            ),
            format!("{gate} castor-zfs status {s}"),
            format!("{gate} castor-zfs usage {s}"),
        ],
        "{rest:#?}"
    );

    // tenant.env stays on the control plane, owner-only, with the node's auth host.
    use std::os::unix::fs::PermissionsExt as _;
    let env_file = root.join(format!(".tenants/{s}/tenant.env"));
    assert_eq!(
        std::fs::metadata(&env_file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let env = std::fs::read_to_string(&env_file).unwrap();
    assert!(env.contains("AUTH_HOST_IP='10.0.0.1'"), "{env}");
    let volumes = std::fs::read_to_string(root.join(format!(".tenants/{s}/compose.volumes.yml"))).unwrap();
    assert!(
        volumes.contains(&format!("/mnt/tank/castor/t/{s}/data/photos")),
        "{volumes}"
    );
    let route = std::fs::read_to_string(w.traefik.join(format!("tenant-{s}.yml"))).unwrap();
    assert!(route.contains("http://10.0.0.5:8480"), "{route}");

    let writes: Vec<String> = ak_seen
        .lock()
        .unwrap()
        .iter()
        .filter(|l| !l.starts_with("GET "))
        .cloned()
        .collect();
    assert_eq!(
        writes[..3],
        [
            r#"POST /api/v3/core/groups/g1/add_user/ auth=true {"pk":42}"#.to_owned(),
            r#"POST /api/v3/core/groups/g1/add_user/ auth=true {"pk":42}"#.to_owned(),
            r#"POST /api/v3/core/groups/g1/add_user/ auth=true {"pk":42}"#.to_owned(),
        ],
        "{writes:#?}"
    );
    assert!(writes.contains(&r#"POST /api/v3/core/groups/g1/remove_user/ auth=true {"pk":43}"#.to_owned()));
    assert!(writes.iter().any(
        |l| l.starts_with(r#"POST /api/v3/providers/oauth2/ auth=true {"name":""#)
            && l.contains(&format!("{s}-photos"))
    ));
    let gw = gw_seen.lock().unwrap().clone();
    assert!(
        gw[1].starts_with(&format!(
            r#"POST /key/generate auth=true {{"key":"sk-tenant key","key_alias":"tenant-{s}-{t}","max_budget":9.77,"#
        )),
        "{gw:?}"
    );
    let apps = apps_seen.lock().unwrap().clone();
    assert_eq!(apps[0], "GET /api/server/ping auth=false ");
    assert!(apps[1].starts_with(&format!(
        r#"POST /api/auth/admin-sign-up auth=false {{"email":"service@{s}.castor.test","password":""#
    )));
    assert_eq!(apps[2], "GET /health auth=false ");

    assert_eq!(
        rows(&db, r#"SELECT status::text FROM "Tenant" WHERE id = $1"#, t).await,
        vec!["ACTIVE"]
    );
    assert_eq!(
        rows(
            &db,
            r#"SELECT count(*)::text FROM "VolumeKey" WHERE "tenantId" = $1"#,
            t
        )
        .await,
        vec!["1"]
    );
    assert_eq!(
        rows(&db, r#"SELECT kind::text || ' ' || healthy || ' ' || coalesce(version, '-') FROM "ServiceInstance" WHERE "tenantId" = $1 ORDER BY kind"#, t).await.len(),
        3
    );
    assert_eq!(
        rows(
            &db,
            r#"SELECT "storageUsedMb" || ' ' || "storageByService"::text FROM "Tenant" WHERE id = $1"#,
            t
        )
        .await,
        vec![r#"10 {"docs": 0, "photos": 7, "places": 0, "system": 1, "flights": 0}"#]
    );
    assert!(
        rows(&db, r#"SELECT id FROM "AccessRevocation" WHERE "tenantId" = $1"#, t)
            .await
            .is_empty()
    );
    let audit = rows(
        &db,
        r#"SELECT action FROM "AuditLog" WHERE "tenantId" = $1 ORDER BY "createdAt", action"#,
        t,
    )
    .await;
    for a in [
        "tenant.placed",
        "volume.key_created",
        "volume.unlock",
        "member.removed_from_apps",
        "access.revoked",
    ] {
        assert_eq!(
            audit.iter().filter(|x| *x == a).count(),
            usize::from(a != "tenant.placed"),
            "{a}: {audit:?}"
        );
    }
    let secrets = rows(&db, r#"SELECT key FROM "Secret" WHERE "tenantId" = $1 ORDER BY key"#, t).await;
    assert_eq!(
        secrets
            .iter()
            .filter(|k| k.starts_with("ASSISTANT_KEY_") || k.starts_with("ASSISTANT_TOOLS_"))
            .count(),
        4
    );
    assert_eq!(
        rows(&db, r#"SELECT step FROM "Job" WHERE id = $1"#, &j.id).await,
        vec!["ready"]
    );
    teardown(&db, &w, &users).await;
    std::fs::remove_dir_all(&root).unwrap();
}

/// A tenant on the local node with Photos only, the booking smaller than its
/// volume: the stack stops for the LUKS shrink, the assistant's key is
/// blocked, the route points at the skin proxy, and UPDATE queues the custom stacks.
#[tokio::test]
async fn update_on_the_local_node_shrinks_offline_and_follows_with_stacks() {
    let Some(db) = db().await else { return };
    let _one = USERS.lock().await;
    let w = world(&db, "LOCAL").await;
    let (s, t) = (&w.slug, &w.tenant);
    let users = setup(&db, &w).await;
    sqlx::query(r#"UPDATE "ServiceInstance" SET enabled = false WHERE "tenantId" = $1 AND kind = 'ASSISTANT'"#)
        .bind(t)
        .execute(&db)
        .await
        .unwrap();
    std::fs::write(w.traefik.join(format!("tenant-{s}.yml")), "old").unwrap();
    // 1 unit booked, 2 on the volume, 50 GB used: fits with headroom.
    let fake = Fake::new(vec![("status", Ok("open 51200 204800 204800\n"))]);
    let (ak, _) = http("ak", authentik).await;
    let (gw, gw_seen) = gateway().await;
    let (apps_url, _) = http("none", apps).await;
    let root = std::env::temp_dir().join(format!("castord-root-{s}"));
    let r = root.to_string_lossy().into_owned();
    let c = with_apps(control(fake.clone(), &w.traefik, Some(&gw), &ak), s, &apps_url, &root);
    c.prepared.lock().unwrap().insert(w.nodes[0].clone());
    let j = job(&db, t, "UPDATE").await;
    castord::apply::Update(c.clone())
        .run(&JobCx { db: db.clone() }, &j)
        .await
        .unwrap();
    let calls = fake.calls();
    let cryptd = "docker run --rm -i --privileged -v /srv/pc:/srv/pc:rshared pc-cryptd:1";
    let at = |needle: &str| {
        calls
            .iter()
            .position(|c| c.contains(needle))
            .unwrap_or_else(|| panic!("{needle}: {calls:#?}"))
    };
    assert_eq!(
        calls[0],
        format!(
            "{cryptd} create {s} 102400 data/db data/redis data/immich-db data/photos data/docs-data data/docs-media data/docs-consume data/places-public data/places-watched data/places-storage data/flights-uploads data/assistant"
        )
    );
    let stop = at(" stop");
    assert!(
        calls[stop].ends_with("--profile photos --profile docs --profile places --profile flights stop"),
        "{}",
        calls[stop]
    );
    assert!(calls[stop].starts_with(&format!("docker compose -p pc-t-{s} -f {r}/packages/stack-templates/tenant/compose.yml --project-directory {r}/packages/stack-templates/tenant ")));
    assert_eq!(calls[stop + 1], format!("{cryptd} shrink {s} 102400"));
    assert!(at("compose -f") > stop && !calls.iter().any(|c| c.contains("LLM_GATEWAY_BIND")));
    assert!(
        calls
            .iter()
            .any(|c| c.ends_with("--profile photos up -d --remove-orphans"))
    );
    assert!(
        calls
            .iter()
            .any(|c| c.contains("com.docker.compose.service=assistant-tools"))
    );
    let route = std::fs::read_to_string(w.traefik.join(format!("tenant-{s}.yml"))).unwrap();
    assert!(
        route.contains(&format!("url: \"http://pc-t-{s}-skin-1:80\"")),
        "{route}"
    );
    assert!(gw_seen.lock().unwrap().iter().any(|l| l.starts_with("POST /key/block")));
    let volumes = std::fs::read_to_string(root.join(format!(".tenants/{s}/compose.volumes.yml"))).unwrap();
    assert!(
        volumes.contains(&format!("/srv/pc/tenants/{s}/data/photos")),
        "{volumes}"
    );
    assert_eq!(
        rows(
            &db,
            r#"SELECT details::text FROM "AuditLog" WHERE "tenantId" = $1 AND action = 'volume.shrunk'"#,
            t
        )
        .await,
        vec![r#"{"toMb": 102400, "fromMb": 204800}"#]
    );
    // The tenant has custom stacks: their own job follows.
    assert_eq!(
        rows(
            &db,
            r#"SELECT status::text FROM "Job" WHERE "tenantId" = $1 AND kind = 'STACKS'"#,
            t
        )
        .await,
        vec!["QUEUED"]
    );
    teardown(&db, &w, &users).await;
    std::fs::remove_dir_all(&root).unwrap();
}

/// No LLM gateway but the assistant booked: nothing runs. A downgrade whose
/// data doesn't fit is held back. A PROVISION that gave up marks the tenant FAILED.
#[tokio::test]
async fn refusals_and_held_back_downgrades() {
    let Some(db) = db().await else { return };
    let _one = USERS.lock().await;
    let w = world(&db, "TRUENAS").await;
    let (s, t) = (&w.slug, &w.tenant);
    let users = setup(&db, &w).await;
    let fake = Fake::new(vec![("castor-zfs status", Ok("open 150000 204800 204800\n"))]);
    let (ak, _) = http("ak", authentik).await;
    let (apps_url, _) = http("none", apps).await;
    let root = std::env::temp_dir().join(format!("castord-root-{s}"));
    let c = with_apps(control(fake.clone(), &w.traefik, None, &ak), s, &apps_url, &root);
    c.prepared.lock().unwrap().insert(w.nodes[0].clone());
    let cx = JobCx { db: db.clone() };
    let j = job(&db, t, "PROVISION").await;
    let p = castord::apply::Provision(c.clone());
    let e = p.run(&cx, &j).await.unwrap_err();
    assert_eq!(e, "assistant booked but the LLM gateway is not configured");
    assert!(fake.calls().is_empty());
    p.gave_up(&cx, &j, &e).await;
    assert_eq!(
        rows(&db, r#"SELECT status::text FROM "Tenant" WHERE id = $1"#, t).await,
        vec!["FAILED"]
    );
    assert_eq!(
        rows(
            &db,
            r#"SELECT details->>'error' FROM "AuditLog" WHERE "tenantId" = $1 AND action = 'tenant.provision_failed'"#,
            t
        )
        .await,
        vec![e]
    );

    sqlx::query(r#"UPDATE "ServiceInstance" SET enabled = false WHERE "tenantId" = $1 AND kind = 'ASSISTANT'"#)
        .bind(t)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query(r#"UPDATE "Job" SET status = 'FAILED' WHERE id = $1"#)
        .bind(&j.id)
        .execute(&db)
        .await
        .unwrap();
    let j = job(&db, t, "UPDATE").await;
    castord::apply::Update(c.clone()).run(&cx, &j).await.unwrap();
    assert!(
        !fake.calls().iter().any(|c| c.contains("castor-zfs quota")),
        "{:#?}",
        fake.calls()
    );
    assert_eq!(
        rows(
            &db,
            r#"SELECT details::text FROM "AuditLog" WHERE "tenantId" = $1 AND action = 'volume.shrink_held'"#,
            t
        )
        .await,
        vec![r#"{"usedMb": 150000, "targetMb": 102400, "volumeMb": 204800}"#]
    );
    let log = rows(&db, r#"SELECT log FROM "Job" WHERE id = $1"#, &j.id).await;
    assert!(
        log[0].contains(" storage: downgrade held back: 150000 MB used, 102400 MB booked\n"),
        "{log:?}"
    );
    assert_eq!(
        rows(&db, r#"SELECT status::text FROM "Tenant" WHERE id = $1"#, t).await,
        vec!["ACTIVE"]
    );
    teardown(&db, &w, &users).await;
    std::fs::remove_dir_all(&root).unwrap();
}
