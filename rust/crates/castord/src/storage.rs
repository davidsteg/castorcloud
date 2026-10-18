//! A tenant's encrypted storage on its node:
//! a LUKS file through the pc-cryptd image on the local node, an encrypted
//! ZFS dataset through castor-zfs on TrueNAS and Mac nodes, and the tenant's
//! custom-stack dataset on a STACKS node.
//!
//! The volume key is unwrapped only for the call that needs it and handed to
//! the helper on stdin: the raw key to pc-cryptd, a key derived from it
//! (castor_crypto::dataset_key) to castor-zfs. Both are wiped afterwards.
//!
//! On a gatekeeper node (TrueNAS) the dataset is managed through the TrueNAS
//! API (crate::gatekeeper): an encryption root with a passphrase, the same
//! derived key in hex, which TrueNAS never stores; each data directory a
//! child dataset, so usage comes from the API too.

use std::time::Duration;

use castor_crypto::{Dataset, Keys};
use serde_json::{Map, Value};
use sqlx::PgPool;
use zeroize::Zeroizing;

use crate::node::{CRYPTD_IMAGE, Node, Role};
use crate::render::STACK_SERVICES;
use crate::valid;

/// One booked storage unit is 100 GB (@pc/pricing catalog.ts).
const STORAGE_UNIT_GB: u64 = 100;
const MB_PER_GB: u64 = 1024;
pub const BYTES_PER_MB: u64 = 1024 * 1024;

/// Size of the encrypted volume for a booking, in MB (storageQuotaMb).
pub fn quota_mb(storage_units: u64) -> u64 {
    storage_units * STORAGE_UNIT_GB * MB_PER_GB
}

/// Whether a booking can hold `used_mb` with the 10 % downgrade headroom
/// (fitsStorage). In floating point, as JavaScript computes it.
pub fn fits(used_mb: u64, storage_units: u64) -> bool {
    used_mb as f64 * (1.0 + 0.1) <= quota_mb(storage_units) as f64
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Missing,
    Closed,
    Open { used_mb: u64, size_mb: u64, volume_mb: u64 },
}

/// "open <used> <size> <volume>" / "closed" / "missing" (parseStatus).
pub fn parse_status(text: &str) -> Status {
    let w: Vec<&str> = text.split_whitespace().collect();
    match w.first() {
        Some(&"open") => {
            let n = |i: usize| w.get(i).and_then(|s| s.parse().ok()).unwrap_or(0);
            Status::Open {
                used_mb: n(1),
                size_mb: n(2),
                volume_mb: n(3),
            }
        }
        Some(&"closed") => Status::Closed,
        _ => Status::Missing,
    }
}

/// "<dir> <mb>" lines printed by the helpers' usage commands (parseUsage).
pub fn parse_usage(text: &str) -> Vec<(String, u64)> {
    text.lines()
        .filter_map(|line| {
            let mut w = line.split_whitespace();
            let (dir, mb) = (w.next()?, w.next()?);
            if !mb.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            Some((dir.to_owned(), mb.parse().ok()?))
        })
        .collect()
}

/// Used MB per service, from the per-directory usage.
pub fn usage_by_service(by_dir: &[(String, u64)]) -> serde_json::Value {
    let mut out: Vec<(&str, u64)> = STACK_SERVICES.iter().map(|s| (s.key(), 0)).collect();
    out.push(("system", 0));
    for (dir, mb) in by_dir {
        let owner = STACK_SERVICES
            .iter()
            .find(|s| s.def().volumes.contains(&dir.as_str()))
            .map_or("system", |s| s.key());
        if let Some(slot) = out.iter_mut().find(|(k, _)| *k == owner) {
            slot.1 += mb;
        }
    }
    serde_json::Value::Object(out.into_iter().map(|(k, v)| (k.to_owned(), v.into())).collect())
}

/// What unwraps a tenant's volume key: the KEK, the tenant it is bound to,
/// and the wrapped key from "VolumeKey".
#[derive(Clone, Copy)]
pub struct VolumeKey<'a> {
    pub keys: &'a Keys,
    pub tenant_id: &'a str,
    pub wrapped: &'a str,
}

impl VolumeKey<'_> {
    fn raw(&self) -> Result<Zeroizing<Vec<u8>>, String> {
        self.keys
            .unwrap_volume_key(self.tenant_id, self.wrapped)
            .map_err(|e| format!("volume key: {e}"))
    }

    fn for_dataset(&self, d: Dataset) -> Result<Zeroizing<Vec<u8>>, String> {
        castor_crypto::dataset_key(&self.raw()?, d).map_err(|e| format!("dataset key: {e}"))
    }

    /// The same key as a TrueNAS passphrase: its 64 hex characters.
    fn passphrase(&self, d: Dataset) -> Result<Zeroizing<String>, String> {
        let key = self.for_dataset(d)?;
        let mut out = Zeroizing::new(String::with_capacity(64));
        out.extend(key.iter().take_while(|b| **b != b'\n').map(|b| char::from(*b)));
        Ok(out)
    }
}

/// A tenant dataset as pool.dataset.query reports it.
#[derive(Debug, PartialEq)]
pub struct NasDataset {
    pub locked: bool,
    pub used_mb: u64,
    pub quota_mb: u64,
    /// The data directories (child datasets) and what each uses.
    pub dirs: Vec<(String, u64)>,
}

/// pool.dataset.query's answer for one dataset; None when it doesn't exist.
pub fn parse_dataset(text: &str, name: &str) -> Result<Option<NasDataset>, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("pool.dataset.query: {e}"))?;
    let Some(d) = v
        .as_array()
        .and_then(|a| a.iter().find(|d| d.get("id").and_then(Value::as_str) == Some(name)))
    else {
        return Ok(None);
    };
    // Properties come as {"parsed": n, "rawvalue": "n", ...}.
    let bytes = |d: &Value, key: &str| -> u64 {
        let p = d.get(key);
        p.and_then(|p| p.get("parsed"))
            .and_then(|n| n.as_u64().or_else(|| n.as_str().and_then(|s| s.parse().ok())))
            .or_else(|| {
                p.and_then(|p| p.get("rawvalue"))
                    .and_then(Value::as_str)
                    .and_then(|s| s.parse().ok())
            })
            .unwrap_or(0)
    };
    let prefix = format!("{name}/");
    let dirs = d
        .get("children")
        .and_then(Value::as_array)
        .map(|c| {
            c.iter()
                .filter_map(|c| {
                    let dir = c.get("id")?.as_str()?.strip_prefix(&prefix)?;
                    Some((dir.to_owned(), bytes(c, "used") / BYTES_PER_MB))
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Some(NasDataset {
        locked: d.get("locked").and_then(Value::as_bool).unwrap_or(false),
        used_mb: bytes(d, "used") / BYTES_PER_MB,
        quota_mb: bytes(d, "quota") / BYTES_PER_MB,
        dirs,
    }))
}

/// pool.dataset.unlock and .lock are middleware jobs; they are confirmed by
/// querying until the dataset's state changes.
const JOB_TRIES: u32 = 60;
const JOB_EVERY: Duration = Duration::from_secs(2);

fn dataset_args(name: &str) -> Map<String, Value> {
    let mut a = Map::new();
    a.insert("dataset".into(), Value::from(name));
    a
}

pub struct TenantStorage<'a> {
    node: &'a Node,
    slug: &'a str,
    key: Option<VolumeKey<'a>>,
}

impl<'a> TenantStorage<'a> {
    pub fn new(node: &'a Node, slug: &'a str) -> Result<Self, String> {
        if node.role == Role::Stacks {
            return Err(format!("{} is a stacks node: no tenant data here", node.name));
        }
        if !valid::slug(slug) {
            return Err(format!("invalid tenant slug {slug:?}"));
        }
        Ok(Self { node, slug, key: None })
    }

    /// Needed to create, open and (on LUKS) resize the storage.
    pub fn with_key(mut self, key: VolumeKey<'a>) -> Self {
        self.key = Some(key);
        self
    }

    /// Where the data directories live, as the node's Docker engine sees them.
    /// On a Mac, Colima mounts the pool into its VM at the same place.
    pub fn data_dir(&self) -> String {
        // On TrueNAS each data directory is a child dataset, mounted below the tenant's.
        if self.node.dataset_root().is_some()
            && let Some((tenants, _)) = self.node.mount_roots()
        {
            return format!("{tenants}/{}", self.slug);
        }
        match self.node.mount_roots() {
            None => format!("/srv/pc/tenants/{}/data", self.slug),
            Some((tenants, _)) => format!("{tenants}/{}/data", self.slug),
        }
    }

    /// ext4 can't shrink while mounted; ZFS quotas change online.
    pub fn offline_shrink(&self) -> bool {
        self.node.is_local()
    }

    fn key(&self) -> Result<VolumeKey<'a>, String> {
        self.key
            .ok_or_else(|| format!("{}: storage needs the volume key", self.slug))
    }

    /// `keyed`: the LUKS helper gets the volume key, castor-zfs a key derived from it.
    async fn run(&self, args: Vec<String>, keyed: bool) -> Result<String, String> {
        let out = if self.node.is_local() {
            let input = if keyed { Some(self.key()?.raw()?) } else { None };
            let mut all: Vec<String> = [
                "run",
                "--rm",
                "-i",
                "--privileged",
                "-v",
                "/srv/pc:/srv/pc:rshared",
                CRYPTD_IMAGE,
            ]
            .iter()
            .map(|a| (*a).to_owned())
            .collect();
            all.extend(args);
            self.node.docker_with(all, input, false).await?
        } else {
            let input = if keyed {
                Some(self.key()?.for_dataset(Dataset::Tenant)?)
            } else {
                None
            };
            let mut all = vec!["castor-zfs".to_owned()];
            all.extend(args);
            self.node.host(all, input).await?
        };
        Ok(out.trim().to_owned())
    }

    /// <pool>/castor/t/<slug> on a gatekeeper node.
    fn nas(&self) -> Option<String> {
        self.node.dataset_root().map(|r| format!("{r}/{}", self.slug))
    }

    async fn query(&self, ds: &str) -> Result<Option<NasDataset>, String> {
        let out = self.node.call("truenas.dataset_query", dataset_args(ds), None).await?;
        parse_dataset(&out, ds)
    }

    /// Queries until `done` holds for the dataset (a lock or unlock job ended).
    async fn await_state(&self, ds: &str, what: &str, done: fn(&NasDataset) -> bool) -> Result<NasDataset, String> {
        for i in 0..JOB_TRIES {
            match self.query(ds).await? {
                Some(d) if done(&d) => return Ok(d),
                None => return Err(format!("{ds} is missing")),
                Some(_) => {}
            }
            if i + 1 < JOB_TRIES {
                tokio::time::sleep(JOB_EVERY).await;
            }
        }
        Err(format!(
            "{ds}: not {what} after {}s",
            JOB_TRIES as u64 * JOB_EVERY.as_secs()
        ))
    }

    async fn nas_open(&self, ds: &str, quota_mb: u64, dirs: &[&str]) -> Result<String, String> {
        let passphrase = || self.key()?.passphrase(Dataset::Tenant);
        match self.query(ds).await? {
            None => {
                let mut a = dataset_args(ds);
                a.insert("quota".into(), Value::from(quota_mb * BYTES_PER_MB));
                self.node.call("truenas.dataset_create", a, Some(passphrase()?)).await?;
            }
            Some(d) if d.locked => {
                self.node
                    .call("truenas.dataset_unlock", dataset_args(ds), Some(passphrase()?))
                    .await?;
            }
            Some(_) => {}
        }
        let open = self.await_state(ds, "unlocked", |d| !d.locked).await?;
        for dir in dirs {
            if !open.dirs.iter().any(|(d, _)| d == dir) {
                self.node
                    .call("truenas.dataset_create_dir", dataset_args(&format!("{ds}/{dir}")), None)
                    .await?;
            }
        }
        Ok("open".into())
    }

    fn args(&self, cmd: &str, rest: &[String]) -> Vec<String> {
        let mut a = vec![cmd.to_owned(), self.slug.to_owned()];
        a.extend_from_slice(rest);
        a
    }

    /// Creates the storage if needed, unlocks and mounts it, creates the data directories.
    pub async fn ensure_open(&self, storage_units: u64, dirs: &[&str]) -> Result<String, String> {
        if let Some(ds) = self.nas() {
            return self.nas_open(&ds, quota_mb(storage_units), dirs).await;
        }
        let mut rest = vec![quota_mb(storage_units).to_string()];
        let local = self.node.is_local();
        rest.extend(
            dirs.iter()
                .map(|d| if local { format!("data/{d}") } else { (*d).to_owned() }),
        );
        self.run(self.args("create", &rest), true).await
    }

    pub async fn grow(&self, storage_units: u64) -> Result<String, String> {
        self.resize("grow", storage_units).await
    }

    /// Offline on LUKS: every container using the volume must be stopped first.
    /// The caller checks the data fits (`fits`).
    pub async fn shrink(&self, storage_units: u64) -> Result<String, String> {
        self.resize("shrink", storage_units).await
    }

    async fn resize(&self, luks_cmd: &str, storage_units: u64) -> Result<String, String> {
        if let Some(ds) = self.nas() {
            let mut a = dataset_args(&ds);
            a.insert("quota".into(), Value::from(quota_mb(storage_units) * BYTES_PER_MB));
            self.node.call("truenas.dataset_quota", a, None).await?;
            return Ok(format!("quota {}", quota_mb(storage_units)));
        }
        let rest = [quota_mb(storage_units).to_string()];
        if self.node.is_local() {
            self.run(self.args(luks_cmd, &rest), true).await
        } else {
            self.run(self.args("quota", &rest), false).await
        }
    }

    pub async fn status(&self) -> Result<Status, String> {
        if let Some(ds) = self.nas() {
            return Ok(match self.query(&ds).await? {
                None => Status::Missing,
                Some(d) if d.locked => Status::Closed,
                Some(d) => Status::Open {
                    used_mb: d.used_mb,
                    size_mb: d.quota_mb,
                    volume_mb: d.quota_mb,
                },
            });
        }
        Ok(parse_status(&self.run(self.args("status", &[]), false).await?))
    }

    /// Used MB per data directory (compose volume name), empty when closed.
    pub async fn usage(&self) -> Result<Vec<(String, u64)>, String> {
        if let Some(ds) = self.nas() {
            return Ok(match self.query(&ds).await? {
                Some(d) if !d.locked => d.dirs,
                _ => Vec::new(),
            });
        }
        Ok(parse_usage(&self.run(self.args("usage", &[]), false).await?))
    }

    /// Unmounts and locks (suspend).
    pub async fn close(&self) -> Result<String, String> {
        if let Some(ds) = self.nas() {
            if let Some(d) = self.query(&ds).await?
                && !d.locked
            {
                self.node.call("truenas.dataset_lock", dataset_args(&ds), None).await?;
                self.await_state(&ds, "locked", |d| d.locked).await?;
            }
            return Ok("closed".into());
        }
        self.run(self.args("close", &[]), false).await
    }

    /// Irreversible. The caller then deletes the key (crypto-shredding).
    pub async fn destroy(&self) -> Result<String, String> {
        if let Some(ds) = self.nas() {
            if self.query(&ds).await?.is_some() {
                self.node
                    .call("truenas.dataset_delete", dataset_args(&ds), None)
                    .await?;
            }
            return Ok("destroyed".into());
        }
        self.run(self.args("destroy", &[]), false).await
    }
}

/// Records the tenant's storage usage, total and per service.
/// Read-only on the node; closed storage is skipped.
pub async fn measure(db: &PgPool, tenant_id: &str, storage: &TenantStorage<'_>) -> Result<(), String> {
    let Status::Open { used_mb, volume_mb, .. } = storage.status().await? else {
        return Ok(());
    };
    let by_service = usage_by_service(&storage.usage().await?);
    sqlx::query(
        r#"UPDATE "Tenant" SET "storageUsedMb" = $2, "storageVolumeMb" = $3, "storageByService" = $4::jsonb,
                  "storageMeasuredAt" = now() AT TIME ZONE 'UTC', "updatedAt" = now() AT TIME ZONE 'UTC'
           WHERE id = $1"#,
    )
    .bind(tenant_id)
    .bind(i32::try_from(used_mb).map_err(|e| e.to_string())?)
    .bind(i32::try_from(volume_mb).map_err(|e| e.to_string())?)
    .bind(by_service.to_string())
    .execute(db)
    .await
    .map_err(|e| format!("measure: {e}"))?;
    Ok(())
}

/// The tenant's custom-stack data on its STACKS node (castor-zfs stacks-* / stack-*).
pub struct StacksStorage<'a> {
    node: &'a Node,
    slug: &'a str,
    key: Option<VolumeKey<'a>>,
}

impl<'a> StacksStorage<'a> {
    pub fn new(node: &'a Node, slug: &'a str) -> Result<Self, String> {
        if node.role != Role::Stacks {
            return Err(format!("{} is not a stacks node", node.name));
        }
        if !valid::slug(slug) {
            return Err(format!("invalid tenant slug {slug:?}"));
        }
        Ok(Self { node, slug, key: None })
    }

    /// Needed to create and unlock the dataset.
    pub fn with_key(mut self, key: VolumeKey<'a>) -> Self {
        self.key = Some(key);
        self
    }

    /// Where each stack's data directories are: <root>/<stack>/data/<dir>.
    pub fn root(&self) -> Result<String, String> {
        let (_, stacks) = self.node.mount_roots().ok_or("a stacks node is never local")?;
        Ok(format!("{stacks}/{}", self.slug))
    }

    pub fn data_dir(&self, stack: &str) -> Result<String, String> {
        Ok(format!("{}/{stack}/data", self.root()?))
    }

    async fn run(&self, cmd: &str, rest: Vec<String>, keyed: bool) -> Result<String, String> {
        let input = match (keyed, self.key) {
            (false, _) => None,
            (true, Some(k)) => Some(k.for_dataset(Dataset::Stacks)?),
            (true, None) => return Err(format!("{}: stacks storage needs the volume key", self.slug)),
        };
        let mut args = vec!["castor-zfs".to_owned(), cmd.to_owned(), self.slug.to_owned()];
        args.extend(rest);
        Ok(self.node.host(args, input).await?.trim().to_owned())
    }

    /// Creates or unlocks the stacks dataset; `quota_mb` is the booked stack
    /// storage, never 0 (0 means no quota in ZFS).
    pub async fn open(&self, quota_mb: u64) -> Result<String, String> {
        if quota_mb < 1 {
            return Err("stacks dataset needs a quota".into());
        }
        self.run("stacks-open", vec![quota_mb.to_string()], true).await
    }

    /// `owners`: data directory, host uid and gid (userns-mapped).
    pub async fn create_stack(
        &self,
        stack: &str,
        quota_mb: u64,
        owners: &[(&str, u32, u32)],
    ) -> Result<String, String> {
        if quota_mb < 1 {
            return Err("stack dataset needs a quota".into());
        }
        let mut rest = vec![stack.to_owned(), quota_mb.to_string()];
        rest.extend(owners.iter().map(|(d, uid, gid)| format!("{d}={uid}.{gid}")));
        self.run("stack-create", rest, false).await
    }

    pub async fn destroy_stack(&self, stack: &str) -> Result<String, String> {
        self.run("stack-destroy", vec![stack.to_owned()], false).await
    }

    pub async fn status(&self) -> Result<Status, String> {
        Ok(parse_status(&self.run("stacks-status", vec![], false).await?))
    }

    /// Used MB per stack.
    pub async fn usage(&self) -> Result<Vec<(String, u64)>, String> {
        Ok(parse_usage(&self.run("stack-usage", vec![], false).await?))
    }

    pub async fn close(&self) -> Result<String, String> {
        self.run("stacks-close", vec![], false).await
    }

    /// Every stack's data. Irreversible.
    pub async fn destroy(&self) -> Result<String, String> {
        self.run("stacks-destroy", vec![], false).await
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// A TrueNAS that keeps one dataset's state, as the node role would reach it.
    #[derive(Default)]
    struct Nas {
        /// None: missing; Some(locked).
        state: std::sync::Mutex<(Option<bool>, Vec<String>)>,
        calls: std::sync::Mutex<Vec<String>>,
        secrets: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl crate::exec::Exec for Nas {
        async fn run(&self, cmd: crate::exec::Cmd) -> Result<String, String> {
            Err(format!("no commands on a TrueNAS node: {}", cmd.label()))
        }

        async fn tool(&self, node: &str, call: crate::exec::ToolCall) -> Result<String, String> {
            assert_eq!(node, "castor-node:7443");
            self.calls.lock().unwrap().push(call.label());
            if let Some((_, s)) = &call.secret {
                self.secrets.lock().unwrap().push(s.to_string());
            }
            let ds = call.args["dataset"].as_str().unwrap().to_owned();
            let mut st = self.state.lock().unwrap();
            match call.name.as_str() {
                "truenas.dataset_query" => Ok(match st.0 {
                    None => "[]".into(),
                    Some(locked) => {
                        let children: Vec<Value> = st
                            .1
                            .iter()
                            .map(|d| serde_json::json!({"id": format!("{ds}/{d}"), "used": {"parsed": 3 * BYTES_PER_MB}}))
                            .collect();
                        serde_json::json!([{"id": ds, "locked": locked,
                            "used": {"parsed": 7 * BYTES_PER_MB, "rawvalue": "7340032"},
                            "quota": {"parsed": null, "rawvalue": (102400 * BYTES_PER_MB).to_string()},
                            "children": children}])
                        .to_string()
                    }
                }),
                "truenas.dataset_create" => {
                    st.0 = Some(false);
                    Ok("{}".into())
                }
                "truenas.dataset_create_dir" => {
                    st.1.push(ds.rsplit('/').next().unwrap().to_owned());
                    Ok("{}".into())
                }
                "truenas.dataset_unlock" => {
                    st.0 = Some(false);
                    Ok("1".into())
                }
                "truenas.dataset_lock" => {
                    st.0 = Some(true);
                    Ok("2".into())
                }
                "truenas.dataset_delete" => {
                    *st = (None, Vec::new());
                    Ok("true".into())
                }
                _ => Ok(String::new()),
            }
        }
    }

    #[tokio::test]
    async fn datasets_through_the_truenas_api() {
        let row = crate::node::NodeRow {
            id: "n".into(),
            name: "nas".into(),
            kind: "TRUENAS".into(),
            role: "TENANTS".into(),
            ssh_user: "castor-node".into(),
            ssh_host: "castor-node:7443".into(),
            zfs_pool: Some("tank".into()),
            ..Default::default()
        };
        let nas = std::sync::Arc::new(Nas::default());
        let node = Node::new(&row, std::path::Path::new("/r"), nas.clone()).unwrap();
        let keys = Keys {
            secrets: castor_crypto::Key::from_hex("S", &"11".repeat(32)).unwrap(),
            volume_kek: castor_crypto::Key::from_hex("V", &"22".repeat(32)).unwrap(),
        };
        let wrapped = keys.new_wrapped_volume_key("t1").unwrap();
        let key = VolumeKey {
            keys: &keys,
            tenant_id: "t1",
            wrapped: &wrapped,
        };
        let storage = TenantStorage::new(&node, "anna").unwrap().with_key(key);
        assert_eq!(storage.data_dir(), "/mnt/tank/castor/t/anna");
        assert_eq!(storage.status().await.unwrap(), Status::Missing);

        storage.ensure_open(1, &["photos", "db"]).await.unwrap();
        assert_eq!(
            storage.status().await.unwrap(),
            Status::Open {
                used_mb: 7,
                size_mb: 102400,
                volume_mb: 102400
            }
        );
        assert_eq!(
            storage.usage().await.unwrap(),
            vec![("photos".into(), 3), ("db".into(), 3)]
        );
        storage.grow(2).await.unwrap();
        storage.close().await.unwrap();
        assert_eq!(storage.status().await.unwrap(), Status::Closed);
        assert!(storage.usage().await.unwrap().is_empty());
        // Opening again unlocks; the directories are there already.
        storage.ensure_open(2, &["photos", "db"]).await.unwrap();
        storage.destroy().await.unwrap();
        assert_eq!(storage.status().await.unwrap(), Status::Missing);

        let ds = r#"{"dataset":"tank/castor/t/anna"}"#;
        let calls: Vec<String> = nas
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| !c.starts_with("truenas.dataset_query"))
            .cloned()
            .collect();
        assert_eq!(
            calls,
            vec![
                format!(
                    r#"truenas.dataset_create {{"dataset":"tank/castor/t/anna","quota":{},"passphrase":"[secret]"}}"#,
                    102400 * BYTES_PER_MB
                ),
                r#"truenas.dataset_create_dir {"dataset":"tank/castor/t/anna/photos"}"#.to_owned(),
                r#"truenas.dataset_create_dir {"dataset":"tank/castor/t/anna/db"}"#.to_owned(),
                format!(
                    r#"truenas.dataset_quota {{"dataset":"tank/castor/t/anna","quota":{}}}"#,
                    204800 * BYTES_PER_MB
                ),
                format!("truenas.dataset_lock {ds}"),
                format!(r#"truenas.dataset_unlock {{"dataset":"tank/castor/t/anna","passphrase":"[secret]"}}"#),
                format!("truenas.dataset_delete {ds}"),
            ]
        );
        // The passphrase is the hex of the key castor-zfs gets, the same both times.
        let secrets = nas.secrets.lock().unwrap().clone();
        let zfs_key = key.for_dataset(Dataset::Tenant).unwrap();
        assert_eq!(secrets.len(), 2);
        assert_eq!(secrets[0], secrets[1]);
        assert_eq!(format!("{}\n", secrets[0]).as_bytes(), zfs_key.as_slice());
    }

    #[test]
    fn dataset_answers() {
        assert_eq!(parse_dataset("[]", "tank/castor/t/a").unwrap(), None);
        assert!(parse_dataset("nope", "tank/castor/t/a").is_err());
        let other = r#"[{"id": "tank/castor/t/ab", "locked": false}]"#;
        assert_eq!(parse_dataset(other, "tank/castor/t/a").unwrap(), None);
    }

    #[test]
    fn statuses() {
        assert_eq!(
            parse_status("open 10 100 102\n"),
            Status::Open {
                used_mb: 10,
                size_mb: 100,
                volume_mb: 102
            }
        );
        assert_eq!(parse_status("closed"), Status::Closed);
        assert_eq!(parse_status(""), Status::Missing);
        assert_eq!(parse_status("missing"), Status::Missing);
    }

    #[test]
    fn quotas_as_in_pricing() {
        const GB: u64 = 1024;
        assert_eq!(quota_mb(1), 100 * GB);
        assert_eq!(quota_mb(5), 500 * GB);
        assert!(fits(90 * GB, 1));
        assert!(!fits(91 * GB, 1));
    }

    #[test]
    fn usage_per_service() {
        let by_dir = parse_usage("immich-db 5\nphotos 100\ndocs-media 7\nweird x\nassistant 3\n\nflights-uploads 1\n");
        assert_eq!(by_dir.len(), 5);
        assert_eq!(
            usage_by_service(&by_dir).to_string(),
            r#"{"photos":105,"docs":7,"places":0,"flights":1,"system":3}"#
        );
    }
}
