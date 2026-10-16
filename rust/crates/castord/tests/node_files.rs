// Tenant files, as node.ts writes them: the env file and compose override on
// the control plane (owner-only), files the containers mount on the node
// (castor-put over ssh, owner-only unless readable), and nothing outside the
// tenant's directory.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use castord::node::{Node, NodeRow};
use common::*;

fn node(kind: &str, root: &Path, fake: std::sync::Arc<Fake>) -> Node {
    let row = NodeRow {
        id: "n1".into(),
        name: "nas".into(),
        kind: kind.into(),
        role: "TENANTS".into(),
        ssh_host: "10.0.0.5".into(),
        ssh_user: "deploy".into(),
        zfs_pool: Some("tank".into()),
    };
    Node::new(&row, root, fake).unwrap()
}

fn mode(p: &str) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[tokio::test]
async fn local_files_are_owner_only_unless_readable() {
    let root = std::env::temp_dir().join(format!("castord-files-{}", hex8()));
    let n = node("LOCAL", &root, Fake::new(vec![]));
    let env = n.write_config("anna", "tenant.env", "A=1\n").unwrap();
    assert_eq!(env, root.join(".tenants/anna/tenant.env").to_string_lossy());
    assert_eq!(mode(&env), 0o600);
    let m = n
        .write_node_file("anna", "assistant/members.json", "{}", true)
        .await
        .unwrap();
    assert_eq!(mode(&m), 0o644);
    // A rewrite tightens a file that was readable.
    let m = n
        .write_node_file("anna", "assistant/members.json", "{}", false)
        .await
        .unwrap();
    assert_eq!((mode(&m), std::fs::read_to_string(&m).unwrap().as_str()), (0o600, "{}"));
    n.remove_node_files("anna", "assistant").await.unwrap();
    assert!(!root.join(".tenants/anna/assistant").exists());
    assert_eq!(n.node_config_dir("anna"), n.config_dir("anna"));
    std::fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn remote_files_go_through_castor_put() {
    let fake = Fake::new(vec![]);
    let n = node("TRUENAS", Path::new("/repo"), fake.clone());
    let p = n
        .write_node_file("anna", "immich.json", "{\"a\":1}", false)
        .await
        .unwrap();
    assert_eq!(p, "/mnt/tank/castor/node/tenants/anna/immich.json");
    n.write_node_file("anna", "assistant/members.json", "{}", true)
        .await
        .unwrap();
    n.remove_node_files("anna", "assistant/hermes/profiles").await.unwrap();
    let gate = "ssh -o BatchMode=yes -T deploy@10.0.0.5";
    assert_eq!(
        fake.calls(),
        vec![
            format!("{gate} castor-put tenants/anna/immich.json"),
            format!("{gate} castor-put tenants/anna/assistant/members.json 644"),
            format!("{gate} castor-rm tenants/anna/assistant/hermes/profiles"),
        ]
    );
    assert_eq!(fake.inputs()[0].as_deref(), Some(&b"{\"a\":1}"[..]));
    assert_eq!(
        (n.node_infra(), n.brand(), n.ca()),
        (
            "/mnt/tank/castor/node/files/node".to_owned(),
            "/mnt/tank/castor/node/files/brand".to_owned(),
            "/mnt/tank/castor/node/files/ca".to_owned()
        )
    );
}

#[tokio::test]
async fn nothing_outside_the_tenant() {
    let fake = Fake::new(vec![]);
    let n = node("TRUENAS", Path::new("/repo"), fake.clone());
    for (slug, file) in [
        ("anna", "../bert/tenant.env"),
        ("anna", "a//b"),
        ("anna", "./x"),
        ("anna", "/etc/passwd"),
        ("anna", "a b"),
        ("../x", "tenant.env"),
    ] {
        assert!(n.write_node_file(slug, file, "", false).await.is_err(), "{slug} {file}");
        assert!(n.write_config(slug, file, "").is_err(), "{slug} {file}");
        assert!(n.remove_node_files(slug, file).await.is_err(), "{slug} {file}");
    }
    assert!(fake.calls().is_empty());
}
