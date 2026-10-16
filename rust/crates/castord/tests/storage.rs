// Tenant and custom-stack storage with the volume key: the commands of
// volume.ts and zfs.ts, and what goes in on stdin (the raw key to pc-cryptd,
// the derived dataset key to castor-zfs, nothing otherwise).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::path::Path;

use castor_crypto::{Dataset, dataset_key};
use castord::node::{Node, NodeRow};
use castord::storage::{StacksStorage, Status, TenantStorage, VolumeKey};
use common::*;

fn node(kind: &str, role: &str, fake: std::sync::Arc<Fake>) -> Node {
    let row = NodeRow {
        id: "n1".into(),
        name: "nas".into(),
        kind: kind.into(),
        role: role.into(),
        ssh_host: "10.0.0.5".into(),
        ssh_user: "deploy".into(),
        zfs_pool: Some("tank".into()),
        ..Default::default()
    };
    Node::new(&row, Path::new("/repo"), fake).unwrap()
}

const ZFS: &str = "ssh -o BatchMode=yes -T deploy@10.0.0.5 castor-zfs";
const CRYPTD: &str = "docker run --rm -i --privileged -v /srv/pc:/srv/pc:rshared pc-cryptd:1";

#[tokio::test]
async fn zfs_gets_the_derived_key_only_to_open() {
    let keys = keys();
    let wrapped = keys.new_wrapped_volume_key("ct1").unwrap();
    let fake = Fake::new(vec![("status", Ok("open 10 100 102\n"))]);
    let n = node("TRUENAS", "TENANTS", fake.clone());
    let key = VolumeKey {
        keys: &keys,
        tenant_id: "ct1",
        wrapped: &wrapped,
    };
    let s = TenantStorage::new(&n, "anna").unwrap().with_key(key);
    assert_eq!(s.data_dir(), "/mnt/tank/castor/t/anna/data");
    assert!(!s.offline_shrink());
    s.ensure_open(2, &["db", "photos"]).await.unwrap();
    s.grow(3).await.unwrap();
    s.shrink(1).await.unwrap();
    assert_eq!(
        s.status().await.unwrap(),
        Status::Open {
            used_mb: 10,
            size_mb: 100,
            volume_mb: 102
        }
    );
    assert_eq!(
        fake.calls(),
        vec![
            format!("{ZFS} create anna 204800 db photos"),
            format!("{ZFS} quota anna 307200"),
            format!("{ZFS} quota anna 102400"),
            format!("{ZFS} status anna"),
        ]
    );
    let dek = keys.unwrap_volume_key("ct1", &wrapped).unwrap();
    let derived = dataset_key(&dek, Dataset::Tenant).unwrap().to_vec();
    assert_eq!(fake.inputs(), vec![Some(derived), None, None, None]);
}

#[tokio::test]
async fn luks_gets_the_raw_key_to_open_and_resize() {
    let keys = keys();
    let wrapped = keys.new_wrapped_volume_key("ct1").unwrap();
    let fake = Fake::new(vec![]);
    let n = node("LOCAL", "TENANTS", fake.clone());
    let key = VolumeKey {
        keys: &keys,
        tenant_id: "ct1",
        wrapped: &wrapped,
    };
    let s = TenantStorage::new(&n, "anna").unwrap().with_key(key);
    assert_eq!(s.data_dir(), "/srv/pc/tenants/anna/data");
    assert!(s.offline_shrink());
    s.ensure_open(1, &["db", "photos"]).await.unwrap();
    s.grow(2).await.unwrap();
    s.shrink(1).await.unwrap();
    s.close().await.unwrap();
    assert_eq!(
        fake.calls(),
        vec![
            format!("{CRYPTD} create anna 102400 data/db data/photos"),
            format!("{CRYPTD} grow anna 204800"),
            format!("{CRYPTD} shrink anna 102400"),
            format!("{CRYPTD} close anna"),
        ]
    );
    let dek = keys.unwrap_volume_key("ct1", &wrapped).unwrap().to_vec();
    assert_eq!(
        fake.inputs(),
        vec![Some(dek.clone()), Some(dek.clone()), Some(dek), None]
    );
}

#[tokio::test]
async fn no_key_no_command_and_a_key_of_another_tenant_fails() {
    let keys = keys();
    let wrapped = keys.new_wrapped_volume_key("ct1").unwrap();
    let fake = Fake::new(vec![]);
    let n = node("TRUENAS", "TENANTS", fake.clone());
    let e = TenantStorage::new(&n, "anna")
        .unwrap()
        .ensure_open(1, &[])
        .await
        .unwrap_err();
    assert!(e.contains("needs the volume key"), "{e}");
    let other = VolumeKey {
        keys: &keys,
        tenant_id: "ct2",
        wrapped: &wrapped,
    };
    let e = TenantStorage::new(&n, "anna")
        .unwrap()
        .with_key(other)
        .ensure_open(1, &[])
        .await
        .unwrap_err();
    assert_eq!(e, "volume key: decryption failed");
    assert!(fake.calls().is_empty());
    assert!(TenantStorage::new(&n, "../x").is_err());
    assert!(StacksStorage::new(&n, "anna").is_err());
}

#[tokio::test]
async fn stacks_dataset_gets_its_own_key() {
    let keys = keys();
    let wrapped = keys.new_wrapped_volume_key("ct1").unwrap();
    let fake = Fake::new(vec![("stack-usage", Ok("linkding 12\nittools 0\n"))]);
    let n = node("TRUENAS", "STACKS", fake.clone());
    let key = VolumeKey {
        keys: &keys,
        tenant_id: "ct1",
        wrapped: &wrapped,
    };
    let s = StacksStorage::new(&n, "anna").unwrap().with_key(key);
    assert_eq!(s.data_dir("linkding").unwrap(), "/mnt/tank/castor/s/anna/linkding/data");
    assert!(s.open(0).await.is_err());
    s.open(10240).await.unwrap();
    s.create_stack("linkding", 1024, &[("data", 100999, 100999)])
        .await
        .unwrap();
    assert_eq!(
        s.usage().await.unwrap(),
        vec![("linkding".to_owned(), 12), ("ittools".to_owned(), 0)]
    );
    s.destroy_stack("ittools").await.unwrap();
    assert_eq!(
        fake.calls(),
        vec![
            format!("{ZFS} stacks-open anna 10240"),
            format!("{ZFS} stack-create anna linkding 1024 data=100999.100999"),
            format!("{ZFS} stack-usage anna"),
            format!("{ZFS} stack-destroy anna ittools"),
        ]
    );
    let dek = keys.unwrap_volume_key("ct1", &wrapped).unwrap();
    let derived = dataset_key(&dek, Dataset::Stacks).unwrap().to_vec();
    assert_eq!(fake.inputs(), vec![Some(derived), None, None, None]);
    assert!(TenantStorage::new(&n, "anna").is_err());
}

#[tokio::test]
async fn measure_records_usage_per_service() {
    let Some(db) = db().await else { return };
    let w = world(&db, "TRUENAS").await;
    let fake = Fake::new(vec![
        ("castor-zfs status", Ok("open 120 102400 102400\n")),
        ("castor-zfs usage", Ok("db 3\nphotos 100\nimmich-db 5\ndocs-media 12\n")),
    ]);
    let n = node("TRUENAS", "TENANTS", fake);
    let s = TenantStorage::new(&n, &w.slug).unwrap();
    castord::storage::measure(&db, &w.tenant, &s).await.unwrap();
    assert_eq!(
        rows(
            &db,
            r#"SELECT "storageUsedMb" || ' ' || "storageVolumeMb" || ' ' || "storageByService"::text || ' ' || ("storageMeasuredAt" IS NOT NULL)
               FROM "Tenant" WHERE id = $1"#,
            &w.tenant
        )
        .await,
        vec![r#"120 102400 {"docs": 12, "photos": 105, "places": 0, "system": 3, "flights": 0} true"#]
    );
    cleanup(&db, &w).await;
}
