// SUSPEND against the real schema, with Docker and SSH faked: the commands
// must be the ones the TypeScript worker runs (lifecycle.ts, stacks.ts),
// and the rows must end the same. Runs when TEST_DATABASE_URL is set.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use castor_crypto::{Key, Keys};
use castord::exec::{Cmd, Exec};
use castord::{Control, Handler, Job, JobCx};
use sqlx::PgPool;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zeroize::Zeroizing;

async fn db() -> Option<PgPool> {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        assert!(
            std::env::var_os("REQUIRE_TEST_DATABASE").is_none(),
            "TEST_DATABASE_URL is not set"
        );
        return None;
    };
    Some(PgPool::connect(&url).await.unwrap())
}

fn hex8() -> String {
    let mut b = [0u8; 4];
    getrandom::fill(&mut b).unwrap();
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Records every command; answers by the first rule whose words it contains.
struct Fake {
    calls: Mutex<Vec<String>>,
    rules: Vec<(&'static str, Result<&'static str, &'static str>)>,
}

impl Fake {
    fn new(rules: Vec<(&'static str, Result<&'static str, &'static str>)>) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            rules,
        })
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl Exec for Fake {
    async fn run(&self, cmd: Cmd) -> Result<String, String> {
        let env: Vec<String> = cmd.env.iter().map(|(k, v)| format!("{k}={v} ")).collect();
        let line = format!("{}{} {}", env.concat(), cmd.program, cmd.args.join(" "));
        self.calls.lock().unwrap().push(line.clone());
        for (needle, answer) in &self.rules {
            if line.contains(needle) {
                return answer.map(str::to_owned).map_err(str::to_owned);
            }
        }
        Ok(String::new())
    }
}

/// A gateway that knows every key; records the requests.
async fn gateway() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let mut n = 0;
            // One request per connection: headers, then Content-Length bytes.
            loop {
                n += s.read(&mut buf[n..]).await.unwrap();
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                if let Some(end) = text.find("\r\n\r\n") {
                    let len = text
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if n >= end + 4 + len {
                        let first = text.lines().next().unwrap().to_owned();
                        let auth = text.lines().any(|l| l == "authorization: Bearer master");
                        log.lock()
                            .unwrap()
                            .push(format!("{first} auth={auth} {}", &text[end + 4..]));
                        break;
                    }
                }
            }
            let body = r#"{"info":{"spend":0,"max_budget":5}}"#;
            let res = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            s.write_all(res.as_bytes()).await.unwrap();
        }
    });
    (url, seen)
}

fn keys() -> Keys {
    Keys {
        secrets: Key::from_hex("S", &"11".repeat(32)).unwrap(),
        volume_kek: Key::from_hex("V", &"22".repeat(32)).unwrap(),
    }
}

struct World {
    tenant: String,
    slug: String,
    nodes: Vec<String>,
    traefik: PathBuf,
}

async fn node(db: &PgPool, kind: &str, role: &str) -> String {
    let id = format!("n{}", hex8());
    sqlx::query(
        r#"INSERT INTO "Node" (id, name, kind, role, "sshHost", "zfsPool", capacity)
           VALUES ($1, $1, $2::"NodeKind", $3::"NodeRole", $4, 'tank', 10)"#,
    )
    .bind(&id)
    .bind(kind)
    .bind(role)
    .bind(if kind == "LOCAL" { "local" } else { "10.0.0.5" })
    .execute(db)
    .await
    .unwrap();
    id
}

/// An active tenant on a TrueNAS node (or the local one), with two custom
/// stacks on a stacks node (one being removed) and an LLM key.
async fn world(db: &PgPool, kind: &str) -> World {
    let slug = format!("t{}", hex8());
    let tenant = format!("c{}", hex8());
    let n = node(db, kind, "TENANTS").await;
    let s = node(db, "TRUENAS", "STACKS").await;
    sqlx::query(
        r#"INSERT INTO "Tenant" (id, slug, "displayName", status, "nodeId", "stacksNodeId", "updatedAt")
           VALUES ($1, $2, 'T', 'ACTIVE', $3, $4, now())"#,
    )
    .bind(&tenant)
    .bind(&slug)
    .bind(&n)
    .bind(&s)
    .execute(db)
    .await
    .unwrap();
    sqlx::query(r#"INSERT INTO "VolumeKey" (id, "tenantId", "wrappedKey") VALUES ($1, $1, 'v1.x.y.z')"#)
        .bind(&tenant)
        .execute(db)
        .await
        .unwrap();
    for (name, status, at) in [("linkding", "RUNNING", 1), ("ittools", "REMOVING", 2)] {
        sqlx::query(
            r#"INSERT INTO "Stack" (id, "tenantId", name, source, definition, "memoryMb", cpus, "storageGb", status, error, "createdAt", "updatedAt")
               VALUES ($1, $2, $3, 'CATALOG', '', 256, 0.25, 1, $4::"StackStatus", 'old', now() + make_interval(secs => $5), now())"#,
        )
        .bind(format!("{tenant}{name}"))
        .bind(&tenant)
        .bind(name)
        .bind(status)
        .bind(f64::from(at))
        .execute(db)
        .await
        .unwrap();
    }
    let sealed = keys().seal_secret(&tenant, "LLM_VIRTUAL_KEY", "sk-tenant key").unwrap();
    sqlx::query(r#"INSERT INTO "Secret" (id, "tenantId", key, ciphertext, "updatedAt") VALUES ($1, $1, 'LLM_VIRTUAL_KEY', $2, now())"#)
        .bind(&tenant)
        .bind(sealed)
        .execute(db)
        .await
        .unwrap();
    let traefik = std::env::temp_dir().join(format!("castord-{slug}"));
    std::fs::create_dir_all(&traefik).unwrap();
    std::fs::write(traefik.join(format!("stacks-{slug}.yml")), "x").unwrap();
    World {
        tenant,
        slug,
        nodes: vec![n, s],
        traefik,
    }
}

async fn cleanup(db: &PgPool, w: &World) {
    sqlx::query(r#"DELETE FROM "Tenant" WHERE id = $1"#)
        .bind(&w.tenant)
        .execute(db)
        .await
        .unwrap();
    sqlx::query(r#"DELETE FROM "AuditLog" WHERE "tenantId" = $1"#)
        .bind(&w.tenant)
        .execute(db)
        .await
        .unwrap();
    sqlx::query(r#"DELETE FROM "Node" WHERE id = ANY($1)"#)
        .bind(&w.nodes)
        .execute(db)
        .await
        .unwrap();
    let _ = std::fs::remove_dir_all(&w.traefik);
}

async fn job(db: &PgPool, tenant: &str) -> Job {
    let id = format!("j{}", hex8());
    sqlx::query(
        r#"INSERT INTO "Job" (id, "tenantId", kind, status, attempts) VALUES ($1, $2, 'SUSPEND', 'RUNNING', 1)"#,
    )
    .bind(&id)
    .bind(tenant)
    .execute(db)
    .await
    .unwrap();
    Job {
        id,
        tenant_id: tenant.to_owned(),
        kind: "SUSPEND".into(),
        attempts: 1,
        max_attempts: 5,
    }
}

fn control(exec: Arc<Fake>, traefik: &Path, llm: Option<&str>) -> Arc<Control> {
    Arc::new(Control {
        root: PathBuf::from("/repo"),
        traefik_dir: traefik.to_path_buf(),
        exec,
        keys: keys(),
        llm: llm.map(|u| castord::llm::Gateway::new(u, Zeroizing::new("master".into())).unwrap()),
        stacks: true,
    })
}

async fn rows(db: &PgPool, sql: &str, id: &str) -> Vec<String> {
    sqlx::query_scalar(sql).bind(id).fetch_all(db).await.unwrap()
}

#[tokio::test]
async fn suspends_like_the_typescript_worker() {
    let Some(db) = db().await else { return };
    let w = world(&db, "TRUENAS").await;
    let s = &w.slug;
    let fake = Fake::new(vec![("stacks-status", Ok("open 1 1024 1024\n"))]);
    let (url, seen) = gateway().await;
    let c = control(fake.clone(), &w.traefik, Some(&url));
    let j = job(&db, &w.tenant).await;
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
                 --project-directory /mnt/tank/castor/node/files/templates/tenant \
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
            "GET /key/info?key=sk-tenant%20key HTTP/1.1 auth=true ".to_owned(),
            r#"POST /key/block HTTP/1.1 auth=true {"key":"sk-tenant key"}"#.to_owned(),
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
    let j = job(&db, &w.tenant).await;
    castord::suspend::Suspend(control(fake.clone(), &w.traefik, None))
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
    let w = world(&db, "TRUENAS").await;
    let fake = Fake::new(vec![
        ("stacks-status", Ok("closed")),
        ("castor-zfs close", Err("ssh … failed: dataset busy")),
    ]);
    let j = job(&db, &w.tenant).await;
    let e = castord::suspend::Suspend(control(fake, &w.traefik, None))
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
