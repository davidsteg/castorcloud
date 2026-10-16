// Fixtures for the job-kind tests: a tenant with its nodes, custom stacks
// and secrets in the real schema; Docker, SSH and HTTP APIs faked.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, dead_code)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use castor_crypto::{Key, Keys};
use castord::exec::{Cmd, Exec};
use castord::{Control, Job};
use sqlx::PgPool;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zeroize::Zeroizing;

pub async fn db() -> Option<PgPool> {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        assert!(
            std::env::var_os("REQUIRE_TEST_DATABASE").is_none(),
            "TEST_DATABASE_URL is not set"
        );
        return None;
    };
    Some(PgPool::connect(&url).await.unwrap())
}

pub fn hex8() -> String {
    let mut b = [0u8; 4];
    getrandom::fill(&mut b).unwrap();
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Records every command; answers by the first rule whose words it contains.
pub struct Fake {
    calls: Mutex<Vec<String>>,
    /// What each call got on stdin, by call.
    inputs: Mutex<Vec<Option<Vec<u8>>>>,
    pub rules: Vec<(&'static str, Result<&'static str, &'static str>)>,
}

impl Fake {
    pub fn new(rules: Vec<(&'static str, Result<&'static str, &'static str>)>) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            inputs: Mutex::new(Vec::new()),
            rules,
        })
    }
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
    pub fn inputs(&self) -> Vec<Option<Vec<u8>>> {
        self.inputs.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl Exec for Fake {
    async fn run(&self, cmd: Cmd) -> Result<String, String> {
        let env: Vec<String> = cmd.env.iter().map(|(k, v)| format!("{k}={v} ")).collect();
        let line = format!("{}{} {}", env.concat(), cmd.program, cmd.args.join(" "));
        self.calls.lock().unwrap().push(line.clone());
        self.inputs.lock().unwrap().push(cmd.input.as_ref().map(|i| i.to_vec()));
        for (needle, answer) in &self.rules {
            if line.contains(needle) {
                return answer.map(str::to_owned).map_err(str::to_owned);
            }
        }
        Ok(String::new())
    }
}

/// An HTTP API answering `answer(method, path)` -> (status, body); records
/// "<METHOD> <path> auth=<bearer> <body>" per request.
pub async fn http(token: &'static str, answer: fn(&str, &str) -> (u16, String)) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 65536];
            let mut n = 0;
            // One request per connection: headers, then Content-Length bytes.
            let (method, path) = loop {
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
                        let mut first = text.lines().next().unwrap().split(' ');
                        let (m, p) = (first.next().unwrap().to_owned(), first.next().unwrap().to_owned());
                        let auth = text.lines().any(|l| l == format!("authorization: Bearer {token}"));
                        log.lock()
                            .unwrap()
                            .push(format!("{m} {p} auth={auth} {}", &text[end + 4..]));
                        break (m, p);
                    }
                }
            };
            let (status, body) = answer(&method, &path);
            let res = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            s.write_all(res.as_bytes()).await.unwrap();
        }
    });
    (url, seen)
}

/// A gateway that knows every key.
pub async fn gateway() -> (String, Arc<Mutex<Vec<String>>>) {
    http("master", |_, _| (200, r#"{"info":{"spend":0,"max_budget":5}}"#.into())).await
}

/// An Authentik that has nothing: every lookup is empty.
pub async fn empty_authentik() -> (String, Arc<Mutex<Vec<String>>>) {
    http("ak", |_, _| (200, r#"{"results":[]}"#.into())).await
}

pub fn keys() -> Keys {
    Keys {
        secrets: Key::from_hex("S", &"11".repeat(32)).unwrap(),
        volume_kek: Key::from_hex("V", &"22".repeat(32)).unwrap(),
    }
}

pub struct World {
    pub tenant: String,
    pub slug: String,
    pub nodes: Vec<String>,
    pub traefik: PathBuf,
}

pub async fn node(db: &PgPool, kind: &str, role: &str) -> String {
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
pub async fn world(db: &PgPool, kind: &str) -> World {
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
            r#"INSERT INTO "Stack" (id, "tenantId", name, source, definition, "memoryMb", cpus, "storageGb", status, error, images, "createdAt", "updatedAt")
               VALUES ($1, $2, $3, 'CATALOG', '', 256, 0.25, 1, $4::"StackStatus", 'old',
                       CASE WHEN $3 = 'linkding' THEN '{"web": "ghcr.io/x/linkding@sha256:ab", "db": "postgres:17"}'::jsonb END,
                       now() + make_interval(secs => $5), now())"#,
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

pub async fn cleanup(db: &PgPool, w: &World) {
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

pub async fn job(db: &PgPool, tenant: &str, kind: &str) -> Job {
    let id = format!("j{}", hex8());
    sqlx::query(
        r#"INSERT INTO "Job" (id, "tenantId", kind, status, attempts) VALUES ($1, $2, $3::"JobKind", 'RUNNING', 1)"#,
    )
    .bind(&id)
    .bind(tenant)
    .bind(kind)
    .execute(db)
    .await
    .unwrap();
    Job {
        id,
        tenant_id: tenant.to_owned(),
        kind: kind.into(),
        attempts: 1,
        max_attempts: 5,
    }
}

pub fn control(exec: Arc<Fake>, traefik: &Path, llm: Option<&str>, authentik: &str) -> Arc<Control> {
    let http = castord::http::client(None).unwrap();
    Arc::new(Control {
        root: PathBuf::from("/repo"),
        traefik_dir: traefik.to_path_buf(),
        exec,
        keys: keys(),
        llm: llm.map(|u| castord::llm::Gateway::new(u, Zeroizing::new("master".into()), http.clone())),
        authentik: castord::authentik::Authentik::new(authentik, Zeroizing::new("ak".into()), http),
        stacks: true,
    })
}

pub async fn rows(db: &PgPool, sql: &str, id: &str) -> Vec<String> {
    sqlx::query_scalar(sql).bind(id).fetch_all(db).await.unwrap()
}
