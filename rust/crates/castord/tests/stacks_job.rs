// STACKS against the real schema, with Docker, SSH and Authentik faked: the
// commands and requests it makes, and the rows it leaves. Runs when TEST_DATABASE_URL is set.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;

use castord::{Control, Handler, JobCx};
use common::*;
use serde_json::{Value, json};
use sqlx::PgPool;

const DIGEST_L: &str = "sha256:e35cb50e0581178f245125ffaa909c565416c94c9f22ace305a7d234a4345522";
const DIGEST_T: &str = "sha256:8b8128748339583ca951af03dfe02a9a4d7363f61a216226fc28030731a5a61f";

fn authentik(method: &str, path: &str) -> (u16, String) {
    let body = match (method, path) {
        ("DELETE", _) => return (204, String::new()),
        ("GET", p) if p.starts_with("/api/v3/core/groups/?") => r#"{"results":[{"pk":"g1"}]}"#,
        ("GET", p) if p.starts_with("/api/v3/flows/") => r#"{"results":[{"pk":"flow"}]}"#,
        ("GET", p) if p.starts_with("/api/v3/propertymappings/") => r#"{"results":[{"pk":"map"}]}"#,
        ("GET", p) if p.starts_with("/api/v3/crypto/") => r#"{"results":[{"pk":"key"}]}"#,
        ("GET", p) if p.starts_with("/api/v3/outposts/instances/") => r#"{"results":[{"pk":"out-1","providers":[1]}]}"#,
        ("GET", _) => r#"{"results":[]}"#,
        ("POST", "/api/v3/providers/proxy/") => r#"{"pk":9}"#,
        ("POST", "/api/v3/core/applications/") => r#"{"pk":"app-1"}"#,
        _ => "{}",
    };
    (200, body.into())
}

fn sealed(tenant: &str, stack: &str, definition: &str) -> String {
    keys()
        .seal_secret(tenant, &format!("stack-definition:{stack}"), definition)
        .unwrap()
}

fn linkding() -> Value {
    json!({
        "services": { "app": {
            "image": format!("sissbruecker/linkding:1.47.0@{DIGEST_L}"),
            "environment": {
                "LD_ENABLE_AUTH_PROXY": "True",
                "LD_CSRF_TRUSTED_ORIGINS": "${castor:origin}",
                "LD_SECRET": "${secret:key}",
            },
            "volumes": ["data:/etc/linkding/data"],
            "mem_limit": "256m",
            "cpus": 0.5,
        }},
        "volumes": { "data": null },
        "x-castor": { "web": "app:9090", "storage_gb": 10 },
    })
}

fn tools() -> Value {
    json!({
        "services": { "app": {
            "image": format!("corentinth/it-tools:2024.10.22-7ca5933@{DIGEST_T}"),
            "tmpfs": ["/var/cache/nginx"],
            "mem_limit": "128m",
            "cpus": 0.25,
        }},
        "x-castor": { "web": "app:80" },
    })
}

/// Stacks node s with its facts; the tenant books 8/8/2 units and has, in
/// order: linkding (running, older images), ittools (being removed), big
/// (more than the booking), broken (stored before castord: YAML), tools.
async fn setup(db: &PgPool, w: &World) {
    let t = &w.tenant;
    sqlx::query(
        r#"UPDATE "Node" SET "uidBase" = 100000, "controlPlaneIps" = '10.0.0.1, 10.0.0.2',
                  "edgeHost" = '10.0.0.40:8480', "stackRuntime" = 'runsc' WHERE id = $1"#,
    )
    .bind(&w.nodes[1])
    .execute(db)
    .await
    .unwrap();
    sqlx::query(
        r#"UPDATE "Tenant" SET "stackMemoryUnits" = 8, "stackCpuUnits" = 8, "stackStorageUnits" = 2 WHERE id = $1"#,
    )
    .bind(t)
    .execute(db)
    .await
    .unwrap();
    sqlx::query(r#"UPDATE "Stack" SET definition = $2, "storageGb" = 10 WHERE id = $1"#)
        .bind(format!("{t}linkding"))
        .bind(sealed(t, "linkding", &linkding().to_string()))
        .execute(db)
        .await
        .unwrap();
    let mut big = tools();
    big["services"]["app"]["mem_limit"] = json!("16g");
    for (i, (name, def)) in [
        ("big", big.to_string()),
        ("broken", "services:\n  app: {}\n".to_owned()),
        ("tools", tools().to_string()),
    ]
    .into_iter()
    .enumerate()
    {
        sqlx::query(
            r#"INSERT INTO "Stack" (id, "tenantId", name, source, definition, "memoryMb", cpus, "storageGb", status, "createdAt", "updatedAt")
               VALUES ($1, $2, $3, 'CUSTOM', $4, 128, 0.25, 0, 'PENDING', now() + make_interval(secs => $5), now())"#,
        )
        .bind(format!("{t}{name}"))
        .bind(t)
        .bind(name)
        .bind(sealed(t, name, &def))
        .bind(3.0 + i as f64)
        .execute(db)
        .await
        .unwrap();
    }
    sqlx::query(r#"UPDATE "VolumeKey" SET "wrappedKey" = $2 WHERE "tenantId" = $1"#)
        .bind(t)
        .bind(keys().new_wrapped_volume_key(t).unwrap())
        .execute(db)
        .await
        .unwrap();
    // A secret of the removed stack, and one of the tenant's own.
    for key in ["stack:ittools:db", "stack:linkding:key"] {
        sqlx::query(
            r#"INSERT INTO "Secret" (id, "tenantId", key, ciphertext, "updatedAt") VALUES ($1, $2, $3, $4, now())"#,
        )
        .bind(format!("{t}{key}"))
        .bind(t)
        .bind(key)
        .bind(keys().seal_secret(t, key, "kept-secret").unwrap())
        .execute(db)
        .await
        .unwrap();
    }
}

fn with_root(c: Arc<Control>, root: &std::path::Path) -> Arc<Control> {
    let mut c = Arc::into_inner(c).unwrap();
    c.root = root.to_path_buf();
    Arc::new(c)
}

const INFO: &str = r#"{"SecurityOptions":["name=seccomp,profile=builtin","name=userns"],"Runtimes":{"runc":{},"runsc":{}},"MemTotal":17179869184,"NCPU":8,"Architecture":"x86_64"}"#;

#[tokio::test]
async fn deploys_like_the_typescript_worker() {
    let Some(db) = db().await else { return };
    let w = world(&db, "TRUENAS").await;
    let (s, t) = (w.slug.clone(), w.tenant.clone());
    setup(&db, &w).await;
    let fake = Fake::new(vec![
        ("info --format", Ok(INFO)),
        ("NetworkSettings.Networks", Ok(r#"{"bridge":{}}"#)),
        ("network ls", Ok("pc-s-other--x-edge\npc-s-bad\n")),
        (
            "image inspect --format {{json .}} docker.io/sissbruecker",
            Ok(r#"{"Size":100,"RepoDigests":[],"Config":{"User":"","Volumes":{"/etc/linkding/data/":{}}}}"#),
        ),
        (
            "image inspect",
            Ok(r#"{"Size":100,"Config":{"User":"101","Volumes":null}}"#),
        ),
    ]);
    let (ak, ak_seen) = http("ak", authentik).await;
    let root = std::env::temp_dir().join(format!("castord-root-{s}"));
    let r = root.to_string_lossy().into_owned();
    let c = with_root(control(fake.clone(), &w.traefik, None, &ak), &root);
    let j = job(&db, &t, "STACKS").await;
    castord::stacks::run::Stacks(c.clone())
        .run(&JobCx { db: db.clone() }, &j)
        .await
        .unwrap();

    let ssh = "DOCKER_HOST=ssh://deploy@10.0.0.5 DOCKER_CONTEXT= docker";
    let zfs = "ssh -o BatchMode=yes -T deploy@10.0.0.5 castor-zfs";
    let edge = "pc-stacks-edge-traefik-1";
    let p = |stack: &str| format!("pc-s-{s}--{stack}");
    let lp = p("linkding");
    let tp = p("tools");
    let expect = vec![
        // The node, once per process.
        format!("{ssh} info --format {{{{json .}}}}"),
        format!("{ssh} build -q -t castor.local/egress:1 {r}/infra/node/egress"),
        format!(
            "EDGE_PORT=8480 CONTROL_PLANE_IPS=10.0.0.1,10.0.0.2 {ssh} compose -p pc-stacks-edge -f {r}/infra/node/stacks-edge.yml up -d --remove-orphans"
        ),
        format!("{ssh} cp - {edge}:/routes"),
        format!("{ssh} network ls --filter name=^pc-s- --format {{{{.Name}}}}"),
        format!("{ssh} inspect --format {{{{json .NetworkSettings.Networks}}}} {edge}"),
        format!("{ssh} network connect pc-s-other--x-edge {edge}"),
        // ittools: removed.
        format!("{ssh} exec {edge} rm -f /routes/{}.yml", p("ittools")),
        format!("{ssh} network disconnect -f {}-edge {edge}", p("ittools")),
        format!("{ssh} compose -p {} down --volumes --remove-orphans", p("ittools")),
        format!("{zfs} stack-destroy {s} ittools"),
        // big: stopped, the booking doesn't cover it.
        format!("{ssh} compose -p {} stop", p("big")),
        format!("{ssh} exec {edge} rm -f /routes/{}.yml", p("big")),
        format!("{zfs} stacks-open {s} 20480"),
        // linkding
        format!("{ssh} pull --quiet docker.io/sissbruecker/linkding:1.47.0@{DIGEST_L}"),
        format!("{ssh} image inspect --format {{{{json .}}}} docker.io/sissbruecker/linkding:1.47.0@{DIGEST_L}"),
        format!("{zfs} stack-create {s} linkding 10240 data=100000.100000"),
        format!("{ssh} info --format {{{{json .}}}}"),
        format!(
            "{ssh} compose -p {lp} -f {r}/.tenants/{s}/stacks/linkding/compose.json up -d --remove-orphans --pull never --wait --wait-timeout 300"
        ),
        format!("{ssh} inspect --format {{{{json .NetworkSettings.Networks}}}} {edge}"),
        format!("{ssh} network connect {lp}-edge {edge}"),
        format!("{ssh} cp - {edge}:/routes"),
        // jsonb keeps shorter keys first: "db" before "web".
        format!("{ssh} image rm postgres:17"),
        format!("{ssh} image rm ghcr.io/x/linkding@sha256:ab"),
        // tools: no storage of its own.
        format!("{ssh} pull --quiet docker.io/corentinth/it-tools:2024.10.22-7ca5933@{DIGEST_T}"),
        format!(
            "{ssh} image inspect --format {{{{json .}}}} docker.io/corentinth/it-tools:2024.10.22-7ca5933@{DIGEST_T}"
        ),
        format!("{ssh} info --format {{{{json .}}}}"),
        format!(
            "{ssh} compose -p {tp} -f {r}/.tenants/{s}/stacks/tools/compose.json up -d --remove-orphans --pull never --wait --wait-timeout 300"
        ),
        format!("{ssh} inspect --format {{{{json .NetworkSettings.Networks}}}} {edge}"),
        format!("{ssh} network connect {tp}-edge {edge}"),
        format!("{ssh} cp - {edge}:/routes"),
    ];
    let calls = fake.calls();
    assert_eq!(calls, expect);

    // What went in on stdin: the route files as tar, the stacks dataset's key.
    let inputs = fake.inputs();
    let tar_of = |i: usize| castord::stacks::untar_file(inputs[i].as_deref().unwrap()).unwrap();
    assert!(tar_of(3).contains(r#"sourceRange: ["10.0.0.1", "10.0.0.2"]"#));
    assert_eq!(
        tar_of(21),
        castord::stacks::render::node_route(&s, "linkding", &format!("linkding-{s}.stacks.test"), 9090.0)
    );
    assert_eq!(inputs[13].as_ref().unwrap().len(), 65, "derived key, hex and a newline");

    // The compose file: rendered by castord, owner-only on the control plane.
    let file = root.join(format!(".tenants/{s}/stacks/linkding/compose.json"));
    use std::os::unix::fs::PermissionsExt as _;
    assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
    let compose: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    let app = &compose["services"]["app"];
    assert_eq!(
        app["image"],
        format!("docker.io/sissbruecker/linkding:1.47.0@{DIGEST_L}")
    );
    assert_eq!(app["runtime"], "runsc");
    assert_eq!(
        app["volumes"][0]["source"],
        format!("/mnt/tank/castor/s/{s}/linkding/data/data")
    );
    assert_eq!(
        app["environment"]["LD_CSRF_TRUSTED_ORIGINS"],
        format!("http://linkding-{s}.stacks.test")
    );
    assert_eq!(app["environment"]["LD_SECRET"], "kept-secret");

    // Rows: removed, failed, stopped, running.
    let status = |name: &str| {
        let db = db.clone();
        let id = format!("{t}{name}");
        async move {
            sqlx::query_as::<_, (String, Option<String>)>(r#"SELECT status::text, error FROM "Stack" WHERE id = $1"#)
                .bind(id)
                .fetch_optional(&db)
                .await
                .unwrap()
        }
    };
    assert_eq!(status("ittools").await, None);
    assert_eq!(
        status("broken").await,
        Some((
            "FAILED".into(),
            Some("the stored definition is not JSON yet (run npm run stacks:json -w @pc/portal once)".into())
        ))
    );
    assert_eq!(
        status("big").await,
        Some(("STOPPED".into(), Some("not covered by the booked resources".into())))
    );
    assert_eq!(status("linkding").await, Some(("RUNNING".into(), None)));
    assert_eq!(status("tools").await, Some(("RUNNING".into(), None)));
    assert_eq!(
        rows(
            &db,
            r#"SELECT images::text FROM "Stack" WHERE id = $1"#,
            &format!("{t}tools")
        )
        .await,
        vec![format!(
            r#"{{"app": "docker.io/corentinth/it-tools:2024.10.22-7ca5933@{DIGEST_T}"}}"#
        )]
    );
    assert_eq!(
        rows(
            &db,
            r#"SELECT key FROM "Secret" WHERE "tenantId" = $1 AND key LIKE 'stack:%' ORDER BY key"#,
            &t
        )
        .await,
        vec!["stack:linkding:key".to_owned()]
    );
    assert_eq!(
        rows(
            &db,
            r#"SELECT action FROM "AuditLog" WHERE "tenantId" = $1 ORDER BY "createdAt", id"#,
            &t
        )
        .await,
        ["stack.removed", "stacks.unlock", "stack.deployed", "stack.deployed"]
    );
    assert_eq!(
        rows(&db, r#"SELECT step FROM "Job" WHERE id = $1"#, &j.id).await,
        vec!["stacks".to_owned()]
    );

    // The control plane's route: both running stacks, to the node's edge.
    let route = std::fs::read_to_string(w.traefik.join(format!("stacks-{s}.yml"))).unwrap();
    assert_eq!(
        route,
        castord::stacks::render::control_plane_routes(
            &s,
            &[
                ("linkding".into(), format!("linkding-{s}.stacks.test")),
                ("tools".into(), format!("tools-{s}.stacks.test")),
            ],
            "http://10.0.0.40:8480",
            None,
            "stacks.test"
        )
    );

    // Authentik: the tenant's group, the removed stack's app gone, one forward-auth app per deployed stack.
    let seen = ak_seen.lock().unwrap().clone();
    let posts: Vec<&String> = seen
        .iter()
        .filter(|l| l.starts_with("POST /api/v3/providers/proxy/"))
        .collect();
    assert_eq!(posts.len(), 2, "{seen:#?}");
    assert!(posts[0].contains(&format!(r#""name":"stack-{s}--linkding""#)));
    assert!(posts[0].contains(&format!(r#""external_host":"http://linkding-{s}.stacks.test""#)));
    assert!(
        seen.iter()
            .any(|l| l.starts_with(&format!("GET /api/v3/providers/proxy/?name=stack-{s}--ittools")))
    );
    assert!(seen.iter().any(|l| l.contains(&format!(r#""name":"linkding ({s})""#))));

    let _ = std::fs::remove_dir_all(&root);
    cleanup(&db, &w).await;
}

#[tokio::test]
async fn refuses_a_node_without_userns_and_stops_when_suspended() {
    let Some(db) = db().await else { return };
    let w = world(&db, "TRUENAS").await;
    let (s, t) = (w.slug.clone(), w.tenant.clone());
    setup(&db, &w).await;
    let fake = Fake::new(vec![("info --format", Ok(r#"{"SecurityOptions":["name=seccomp"]}"#))]);
    let (ak, _) = http("ak", authentik).await;
    let root = std::env::temp_dir().join(format!("castord-root-{s}"));
    let c = with_root(control(fake.clone(), &w.traefik, None, &ak), &root);
    let j = job(&db, &t, "STACKS").await;
    let e = castord::stacks::run::Stacks(c.clone())
        .run(&JobCx { db: db.clone() }, &j)
        .await
        .unwrap_err();
    assert_eq!(
        e,
        format!(
            "{}: Docker runs without userns-remap; refusing to run stacks",
            w.nodes[1]
        )
    );
    assert_eq!(fake.calls().len(), 1, "nothing ran");

    // Not active: everything stops and the data is locked.
    sqlx::query(r#"UPDATE "Tenant" SET status = 'SUSPENDED' WHERE id = $1"#)
        .bind(&t)
        .execute(&db)
        .await
        .unwrap();
    let fake = Fake::new(vec![("stacks-status", Ok("open 1 2 3\n"))]);
    let c = with_root(control(fake.clone(), &w.traefik, None, &ak), &root);
    castord::stacks::run::Stacks(c)
        .run(&JobCx { db: db.clone() }, &j)
        .await
        .unwrap();
    let calls = fake.calls();
    assert!(
        calls
            .iter()
            .any(|l| l.ends_with(&format!("castor-zfs stacks-close {s}"))),
        "{calls:#?}"
    );
    assert!(!w.traefik.join(format!("stacks-{s}.yml")).exists());
    let _ = std::fs::remove_dir_all(&root);
    cleanup(&db, &w).await;
}

#[tokio::test]
async fn a_failing_deploy_fails_only_that_stack() {
    let Some(db) = db().await else { return };
    let w = world(&db, "TRUENAS").await;
    let (s, t) = (w.slug.clone(), w.tenant.clone());
    setup(&db, &w).await;
    // linkding runs as "node", found in the image's own passwd; tools' image
    // declares a VOLUME nothing covers.
    let passwd = castord::stacks::tar_file("passwd", b"node:x:1000:1001::/home/node:/bin/sh\n", 0o644, 1).unwrap();
    let passwd: &'static str = Box::leak(String::from_utf8(passwd).unwrap().into_boxed_str());
    let fake = Fake::new(vec![
        ("info --format", Ok(INFO)),
        ("NetworkSettings.Networks", Ok("{}")),
        (
            "image inspect --format {{json .}} docker.io/sissbruecker",
            Ok(r#"{"Size":100,"Config":{"User":"node","Volumes":null}}"#),
        ),
        ("image inspect", Ok(r#"{"Size":100,"Config":{"Volumes":{"/data":{}}}}"#)),
        (":/etc/passwd -", Ok(passwd)),
        (":/etc/group -", Err("no such file")),
    ]);
    let (ak, _) = http("ak", authentik).await;
    let root = std::env::temp_dir().join(format!("castord-root-{s}"));
    let c = with_root(control(fake.clone(), &w.traefik, None, &ak), &root);
    c.prepared.lock().unwrap().insert(w.nodes[1].clone());
    let j = job(&db, &t, "STACKS").await;
    castord::stacks::run::Stacks(c)
        .run(&JobCx { db: db.clone() }, &j)
        .await
        .unwrap();
    let calls = fake.calls();
    let ssh = "DOCKER_HOST=ssh://deploy@10.0.0.5 DOCKER_CONTEXT= docker";
    let ids = calls.iter().find(|l| l.contains(" create --name pc-s-ids-")).unwrap();
    let name = ids.split(' ').nth(5).unwrap();
    assert_eq!(
        *ids,
        format!(
            "{ssh} create --name {name} --network none --entrypoint /nonexistent docker.io/sissbruecker/linkding:1.47.0@{DIGEST_L}"
        )
    );
    assert!(calls.contains(&format!("{ssh} rm -f {name}")));
    // uid 1000, primary group 1001 from passwd, both mapped above 100000.
    assert!(calls.iter().any(|l| l.ends_with(&format!(
        "castor-zfs stack-create {s} linkding 10240 data=101000.101001"
    ))));
    let failed: Vec<(String, Option<String>)> =
        sqlx::query_as(r#"SELECT status::text, error FROM "Stack" WHERE id = $1"#)
            .bind(format!("{t}tools"))
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(
        failed,
        [(
            "FAILED".to_owned(),
            Some("app: the image declares /data; mount a volume or a tmpfs there".to_owned())
        )]
    );
    assert!(
        !calls
            .iter()
            .any(|l| l.contains("compose -p pc-s-") && l.contains("--tools -f"))
    );
    assert_eq!(
        rows(
            &db,
            r#"SELECT details->>'stack' FROM "AuditLog" WHERE "tenantId" = $1 AND action = 'stack.failed'"#,
            &t
        )
        .await,
        vec!["tools".to_owned()]
    );
    // Only the running stack is routed.
    let route = std::fs::read_to_string(w.traefik.join(format!("stacks-{s}.yml"))).unwrap();
    assert!(route.contains("--linkding:") && !route.contains("--tools:"));
    let _ = std::fs::remove_dir_all(&root);
    cleanup(&db, &w).await;
}
