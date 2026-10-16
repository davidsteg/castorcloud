// Node preparation and placement, as node.ts and nodes.ts do them: the
// commands that bring a node up to date, and a tenant placed once.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::path::{Path, PathBuf};

use castord::node::{Node, NodeRow};
use common::*;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .canonicalize()
        .unwrap()
}

fn node(kind: &str, edge: Option<&str>, fake: std::sync::Arc<Fake>) -> Node {
    let row = NodeRow {
        id: "n1".into(),
        name: "nas".into(),
        kind: kind.into(),
        role: "TENANTS".into(),
        ssh_host: "10.0.0.5".into(),
        ssh_user: "deploy".into(),
        zfs_pool: Some("tank".into()),
        edge_host: edge.map(str::to_owned),
        auth_host_ip: Some("10.0.0.1".into()),
    };
    Node::new(&row, &repo(), fake).unwrap()
}

#[tokio::test]
async fn remote_node_gets_files_then_edge_and_tools() {
    let r = repo().to_string_lossy().into_owned();
    let ca = std::env::temp_dir().join(format!("castord-ca-{}", hex8()));
    std::fs::create_dir_all(&ca).unwrap();
    let fake = Fake::new(vec![("tar -C", Ok("TAR")), ("network inspect", Err("no such network"))]);
    let n = node("TRUENAS", Some("10.0.0.5:8480"), fake.clone());
    assert_eq!(n.edge.as_ref().unwrap().url, "http://10.0.0.5:8480");
    assert_eq!(n.auth_host_ip.as_deref(), Some("10.0.0.1"));
    n.prepare(&ca).await.unwrap();
    let ssh = "DOCKER_HOST=ssh://deploy@10.0.0.5 DOCKER_CONTEXT= docker";
    let gate = "ssh -o BatchMode=yes -T deploy@10.0.0.5 castor-untar";
    let c = ca.to_string_lossy();
    assert_eq!(
        fake.calls(),
        vec![
            format!("tar -C {r}/packages/stack-templates -cf - ."),
            format!("{gate} templates"),
            format!("tar -C {r}/infra/node -cf - compose.yml litellm"),
            format!("{gate} node"),
            format!("tar -C {r}/packages/brand -cf - tokens.css theme.css bridge.js logo.svg icon.svg services"),
            format!("{gate} brand"),
            format!("tar -C {c} -cf - ca-bundle.pem"),
            format!("{gate} ca"),
            format!("{ssh} network inspect pc-edge"),
            format!("{ssh} network create pc-edge"),
            format!("EDGE_PORT=8480 {ssh} compose -p pc-edge -f {r}/infra/node/edge.yml up -d --remove-orphans"),
            format!("{ssh} build -q -t pc-assistant-tools:1 -f {r}/agent/tools/Dockerfile {r}"),
        ]
    );
    assert_eq!(fake.inputs()[1].as_deref(), Some(&b"TAR"[..]));
    std::fs::remove_dir_all(&ca).unwrap();
    // A missing CA bundle stops before it is sent.
    let fake = Fake::new(vec![]);
    let e = node("TRUENAS", None, fake.clone())
        .prepare(Path::new("/nonexistent-ca"))
        .await
        .unwrap_err();
    assert_eq!(e, "/nonexistent-ca missing");
    assert_eq!(fake.calls().len(), 6);
}

#[tokio::test]
async fn local_node_gets_the_shared_mount_and_images() {
    let r = repo().to_string_lossy().into_owned();
    let fake = Fake::new(vec![]);
    let n = node("LOCAL", Some("10.0.0.5"), fake.clone());
    assert!(n.edge.is_none() && n.auth_host_ip.is_none());
    n.prepare(Path::new("/unused")).await.unwrap();
    let calls = fake.calls();
    assert!(calls[0].starts_with(
        "docker run --rm --privileged --pid=host alpine:3.22 nsenter -t 1 -m -- sh -c mkdir -p /srv/pc/volumes"
    ));
    assert_eq!(
        calls[1..],
        [
            format!("docker build -q -t pc-cryptd:1 {r}/infra/node/cryptd"),
            format!("docker build -q -t pc-assistant-tools:1 -f {r}/agent/tools/Dockerfile {r}"),
        ]
    );
}

#[tokio::test]
async fn edge_hosts() {
    use castord::node::parse_edge;
    assert_eq!(parse_edge("10.0.0.5").unwrap().url, "http://10.0.0.5");
    assert_eq!(parse_edge("nas.lan:80").unwrap().url, "http://nas.lan");
    assert_eq!(parse_edge("nas.lan:8480").unwrap().port, 8480);
    for bad in ["", "nas:", ":80", "nas:0", "nas:65536", "nas:1:2", "na s", "nas/x"] {
        assert!(parse_edge(bad).is_err(), "{bad}");
    }
}

#[tokio::test]
async fn places_once() {
    let Some(db) = db().await else { return };
    let w = world(&db, "TRUENAS").await;
    let placed = rows(
        &db,
        r#"SELECT name FROM "Node" WHERE id = (SELECT "nodeId" FROM "Tenant" WHERE id = $1)"#,
        &w.tenant,
    )
    .await;
    assert_eq!(vec![castord::placement::place(&db, &w.tenant).await.unwrap()], placed);
    // Unplaced: some node takes it (others' test nodes may be there too), with an audit entry.
    sqlx::query(r#"UPDATE "Tenant" SET "nodeId" = NULL WHERE id = $1"#)
        .bind(&w.tenant)
        .execute(&db)
        .await
        .unwrap();
    let name = castord::placement::place(&db, &w.tenant).await.unwrap();
    assert_eq!(
        rows(
            &db,
            r#"SELECT n.name FROM "Tenant" t JOIN "Node" n ON n.id = t."nodeId" WHERE t.id = $1"#,
            &w.tenant
        )
        .await,
        vec![name.clone()]
    );
    assert_eq!(
        rows(
            &db,
            r#"SELECT details->>'node' FROM "AuditLog" WHERE "tenantId" = $1 AND action = 'tenant.placed'"#,
            &w.tenant
        )
        .await,
        vec![name]
    );
    cleanup(&db, &w).await;
}
