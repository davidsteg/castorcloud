//! A tenant's encrypted storage on its node (storage.ts, volume.ts, zfs.ts):
//! a LUKS file through the pc-cryptd image on the local node, an encrypted
//! ZFS dataset through castor-zfs on TrueNAS and Mac nodes, and the tenant's
//! custom-stack dataset on a STACKS node.
//!
//! The volume key is unwrapped only for the call that needs it and handed to
//! the helper on stdin: the raw key to pc-cryptd, a key derived from it
//! (castor_crypto::dataset_key) to castor-zfs. Both are wiped afterwards.

use castor_crypto::{Dataset, Keys};
use sqlx::PgPool;
use zeroize::Zeroizing;

use crate::node::{CRYPTD_IMAGE, Node, Role};
use crate::render::STACK_SERVICES;
use crate::valid;

/// One booked storage unit is 100 GB (@pc/pricing catalog.ts).
const STORAGE_UNIT_GB: u64 = 100;
const MB_PER_GB: u64 = 1024;

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

/// Used MB per service, from the per-directory usage (usageByService in lifecycle.ts).
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

    fn args(&self, cmd: &str, rest: &[String]) -> Vec<String> {
        let mut a = vec![cmd.to_owned(), self.slug.to_owned()];
        a.extend_from_slice(rest);
        a
    }

    /// Creates the storage if needed, unlocks and mounts it, creates the data directories.
    pub async fn ensure_open(&self, storage_units: u64, dirs: &[&str]) -> Result<String, String> {
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
        let rest = [quota_mb(storage_units).to_string()];
        if self.node.is_local() {
            self.run(self.args(luks_cmd, &rest), true).await
        } else {
            self.run(self.args("quota", &rest), false).await
        }
    }

    pub async fn status(&self) -> Result<Status, String> {
        Ok(parse_status(&self.run(self.args("status", &[]), false).await?))
    }

    /// Used MB per data directory (compose volume name), empty when closed.
    pub async fn usage(&self) -> Result<Vec<(String, u64)>, String> {
        Ok(parse_usage(&self.run(self.args("usage", &[]), false).await?))
    }

    /// Unmounts and locks (suspend).
    pub async fn close(&self) -> Result<String, String> {
        self.run(self.args("close", &[]), false).await
    }

    /// Irreversible. The caller then deletes the key (crypto-shredding).
    pub async fn destroy(&self) -> Result<String, String> {
        self.run(self.args("destroy", &[]), false).await
    }
}

/// Records the tenant's storage usage, total and per service (measure in
/// lifecycle.ts). Read-only on the node; closed storage is skipped.
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
    use super::*;

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
