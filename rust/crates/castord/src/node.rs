//! A data-plane node, as in `apps/provisioner/src/node.ts`:
//!   local  the Docker host the control plane runs on (development); LUKS
//!          volumes through the pc-cryptd image.
//!   ssh    TrueNAS SCALE or a Mac mini: Docker over ssh://, and castor-*
//!          commands through the node's castor-ssh gate.
//!
//! Preparing a node (templates, edge proxy) stays with the TypeScript worker
//! until the jobs that need it move here; stopping and locking need none.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sqlx::PgPool;
use zeroize::Zeroizing;

use crate::exec::{Cmd, Exec};
use crate::valid;

/// Same tag as CRYPTD_IMAGE in node.ts.
pub const CRYPTD_IMAGE: &str = "pc-cryptd:1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    Tenants,
    Stacks,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Place {
    Local,
    Ssh {
        target: String,
        /// castor-ssh's ROOT (nodeLayout in layout.ts).
        root: String,
        /// castor-zfs's MOUNT_ROOT and STACKS_MOUNT_ROOT: where datasets are mounted.
        mount_root: String,
        stacks_mount_root: String,
    },
}

/// The columns of "Node" a driver needs.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct NodeRow {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub role: String,
    #[sqlx(rename = "sshHost")]
    pub ssh_host: String,
    #[sqlx(rename = "sshUser")]
    pub ssh_user: String,
    #[sqlx(rename = "zfsPool")]
    pub zfs_pool: Option<String>,
}

impl NodeRow {
    pub async fn load(db: &PgPool, id: &str) -> Result<Self, String> {
        sqlx::query_as::<_, NodeRow>(
            r#"SELECT id, name, kind::text AS kind, role::text AS role, "sshHost", "sshUser", "zfsPool"
               FROM "Node" WHERE id = $1"#,
        )
        .bind(id)
        .fetch_optional(db)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("node {id} not found"))
    }
}

pub struct Node {
    pub name: String,
    pub role: Role,
    place: Place,
    /// The repository on the control plane (templates, .tenants/).
    root: PathBuf,
    exec: Arc<dyn Exec>,
}

impl Node {
    pub fn new(row: &NodeRow, root: &Path, exec: Arc<dyn Exec>) -> Result<Self, String> {
        let role = match row.role.as_str() {
            "TENANTS" => Role::Tenants,
            "STACKS" => Role::Stacks,
            r => return Err(format!("node {}: unknown role {r}", row.name)),
        };
        let place = match row.kind.as_str() {
            "LOCAL" => Place::Local,
            kind @ ("MAC" | "TRUENAS") => {
                let target = format!("{}@{}", row.ssh_user, row.ssh_host);
                if !valid::ssh_target(&target) {
                    return Err(format!("invalid ssh target: {target}"));
                }
                let Some(pool) = row.zfs_pool.as_deref() else {
                    return Err(format!("node {} has no ZFS pool", row.name));
                };
                if !valid::pool(pool) {
                    return Err(format!("invalid pool name: {pool}"));
                }
                // macOS: the pool is mounted into Colima's VM at the same paths.
                let (root, mounts) = if kind == "TRUENAS" {
                    (format!("/mnt/{pool}/castor/node"), format!("/mnt/{pool}/castor"))
                } else {
                    ("/Users/Shared/castor".to_owned(), format!("/Volumes/{pool}"))
                };
                Place::Ssh {
                    target,
                    root,
                    mount_root: format!("{mounts}/t"),
                    stacks_mount_root: format!("{mounts}/s"),
                }
            }
            k => return Err(format!("node {}: unknown kind {k}", row.name)),
        };
        Ok(Self {
            name: row.name.clone(),
            role,
            place,
            root: root.to_owned(),
            exec,
        })
    }

    pub fn is_local(&self) -> bool {
        self.place == Place::Local
    }

    /// Runs docker on the node; with `allow_fail` a failure returns "".
    pub async fn docker(&self, args: Vec<String>, allow_fail: bool) -> Result<String, String> {
        self.docker_with(args, None, allow_fail).await
    }

    pub async fn docker_with(
        &self,
        args: Vec<String>,
        input: Option<Zeroizing<Vec<u8>>>,
        allow_fail: bool,
    ) -> Result<String, String> {
        let mut cmd = Cmd::new("docker", args);
        cmd.input = input;
        if let Place::Ssh { target, .. } = &self.place {
            cmd.env.push(("DOCKER_HOST", format!("ssh://{target}")));
            cmd.env.push(("DOCKER_CONTEXT", String::new()));
        }
        match self.exec.run(cmd).await {
            Err(_) if allow_fail => Ok(String::new()),
            r => r,
        }
    }

    /// A castor-* command on the node's host, through the castor-ssh gate.
    pub async fn host(&self, args: Vec<String>, input: Option<Zeroizing<Vec<u8>>>) -> Result<String, String> {
        let Place::Ssh { target, .. } = &self.place else {
            return Err(format!("{} has no host commands", self.name));
        };
        // castor-ssh splits on spaces and allows only these words.
        if let Some(bad) = args.iter().find(|a| !valid::ssh_word(a)) {
            return Err(format!("argument not allowed over ssh: {bad}"));
        }
        let mut all = vec!["-o".into(), "BatchMode=yes".into(), "-T".into(), target.clone()];
        all.extend(args);
        let mut cmd = Cmd::new("ssh", all);
        cmd.input = input;
        self.exec.run(cmd).await
    }

    /// Where castor-zfs mounts tenant datasets, and custom-stack datasets; None on the local node.
    pub fn mount_roots(&self) -> Option<(&str, &str)> {
        match &self.place {
            Place::Local => None,
            Place::Ssh {
                mount_root,
                stacks_mount_root,
                ..
            } => Some((mount_root, stacks_mount_root)),
        }
    }

    /// Compose files on the control plane, read by the docker CLI.
    pub fn templates(&self) -> String {
        self.root
            .join("packages/stack-templates")
            .to_string_lossy()
            .into_owned()
    }

    /// The same templates on the node: relative bind mounts resolve there.
    pub fn node_templates(&self) -> String {
        match &self.place {
            Place::Local => self.templates(),
            Place::Ssh { root, .. } => format!("{root}/files/templates"),
        }
    }

    /// infra/node (shared services, the LLM gateway's config) on the control plane.
    pub fn infra(&self) -> String {
        self.root.join("infra/node").to_string_lossy().into_owned()
    }

    /// The same on the node: relative bind mounts of the shared services resolve here.
    pub fn node_infra(&self) -> String {
        self.on_node("infra/node", "files/node")
    }

    /// What the skin proxy serves under /_brand/.
    pub fn brand(&self) -> String {
        self.on_node("packages/brand", "files/brand")
    }

    /// The CA bundle tenant containers trust.
    pub fn ca(&self) -> String {
        self.on_node(".dev-certs", "files/ca")
    }

    fn on_node(&self, local: &str, remote: &str) -> String {
        match &self.place {
            Place::Local => self.root.join(local).to_string_lossy().into_owned(),
            Place::Ssh { root, .. } => format!("{root}/{remote}"),
        }
    }

    /// TENANT_DIR in the stack: where write_node_file puts files.
    pub fn node_config_dir(&self, slug: &str) -> String {
        match &self.place {
            Place::Local => self.config_dir(slug),
            Place::Ssh { root, .. } => format!("{root}/tenants/{slug}"),
        }
    }

    /// A file the docker CLI reads (tenant.env, compose.volumes.yml). Stays on
    /// the control plane, owner-only: tenant.env holds every secret of the stack.
    pub fn write_config(&self, slug: &str, file: &str, content: &str) -> Result<String, String> {
        check(slug, file)?;
        write_private(
            &self.root.join(".tenants").join(slug).join(file),
            content.as_bytes(),
            false,
        )
    }

    /// A file tenant containers mount, owner-only unless `readable`; returns its path on the node.
    pub async fn write_node_file(
        &self,
        slug: &str,
        file: &str,
        content: &str,
        readable: bool,
    ) -> Result<String, String> {
        check(slug, file)?;
        if self.is_local() {
            return write_private(
                &self.root.join(".tenants").join(slug).join(file),
                content.as_bytes(),
                readable,
            );
        }
        let mut args = vec!["castor-put".to_owned(), format!("tenants/{slug}/{file}")];
        if readable {
            args.push("644".into());
        }
        self.host(args, Some(Zeroizing::new(content.as_bytes().to_vec())))
            .await?;
        Ok(format!("{}/{file}", self.node_config_dir(slug)))
    }

    /// Removes a directory below the tenant's node files.
    pub async fn remove_node_files(&self, slug: &str, dir: &str) -> Result<(), String> {
        check(slug, dir)?;
        if !self.is_local() {
            self.host(vec!["castor-rm".into(), format!("tenants/{slug}/{dir}")], None)
                .await?;
            return Ok(());
        }
        let path = self.root.join(".tenants").join(slug).join(dir);
        match std::fs::remove_dir_all(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("{}: {e}", path.display())),
            _ => Ok(()),
        }
    }

    /// Removes the tenant's files on the node and on the control plane (delete).
    pub async fn remove_config(&self, slug: &str) -> Result<(), String> {
        if !valid::slug(slug) {
            return Err(format!("invalid tenant slug {slug:?}"));
        }
        if !self.is_local() {
            self.host(vec!["castor-rm".into(), format!("tenants/{slug}")], None)
                .await?;
        }
        let dir = self.root.join(".tenants").join(slug);
        match std::fs::remove_dir_all(&dir) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("{}: {e}", dir.display())),
            _ => Ok(()),
        }
    }

    /// The tenant's env file and compose override, on the control plane.
    pub fn config_dir(&self, slug: &str) -> String {
        self.root.join(".tenants").join(slug).to_string_lossy().into_owned()
    }
}

fn check(slug: &str, file: &str) -> Result<(), String> {
    if !valid::slug(slug) {
        return Err(format!("invalid tenant slug {slug:?}"));
    }
    if !valid::rel_path(file) {
        return Err(format!("invalid tenant file {file:?}"));
    }
    Ok(())
}

/// Writes with mode 0600 (0644 when `readable`), also over an existing file's mode.
fn write_private(path: &Path, content: &[u8], readable: bool) -> Result<String, String> {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    let mode = if readable { 0o644 } else { 0o600 };
    let err = |e: std::io::Error| format!("{}: {e}", path.display());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(err)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(path)
        .map_err(err)?;
    f.set_permissions(std::fs::Permissions::from_mode(mode)).map_err(err)?;
    f.write_all(content).map_err(err)?;
    Ok(path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn row(kind: &str, user: &str, host: &str, pool: Option<&str>) -> NodeRow {
        NodeRow {
            id: "n1".into(),
            name: "nas".into(),
            kind: kind.into(),
            role: "TENANTS".into(),
            ssh_host: host.into(),
            ssh_user: user.into(),
            zfs_pool: pool.map(Into::into),
        }
    }

    struct Never;
    #[async_trait::async_trait]
    impl Exec for Never {
        async fn run(&self, _: Cmd) -> Result<String, String> {
            Err("not expected".into())
        }
    }

    #[test]
    fn layouts_and_refusals() {
        let root = Path::new("/repo");
        let nas = Node::new(
            &row("TRUENAS", "deploy", "10.0.0.5", Some("tank")),
            root,
            Arc::new(Never),
        )
        .unwrap();
        assert_eq!(nas.node_templates(), "/mnt/tank/castor/node/files/templates");
        assert_eq!(nas.templates(), "/repo/packages/stack-templates");
        assert_eq!(nas.config_dir("anna"), "/repo/.tenants/anna");
        let mac = Node::new(&row("MAC", "deploy", "mini.lan", Some("castor")), root, Arc::new(Never)).unwrap();
        assert_eq!(mac.node_templates(), "/Users/Shared/castor/files/templates");
        let local = Node::new(&row("LOCAL", "deploy", "local", None), root, Arc::new(Never)).unwrap();
        assert_eq!(local.node_templates(), "/repo/packages/stack-templates");

        for bad in [
            row("TRUENAS", "deploy", "-oProxyCommand=x", Some("tank")),
            row("TRUENAS", "de ploy", "nas", Some("tank")),
            row("TRUENAS", "deploy", "nas", Some("tank;rm")),
            row("TRUENAS", "deploy", "nas", None),
            row("VM", "deploy", "nas", Some("tank")),
        ] {
            assert!(Node::new(&bad, root, Arc::new(Never)).is_err(), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn host_words_are_checked_before_ssh() {
        let nas = Node::new(
            &row("TRUENAS", "deploy", "nas", Some("tank")),
            Path::new("/r"),
            Arc::new(Never),
        )
        .unwrap();
        let e = nas
            .host(vec!["castor-zfs".into(), "close".into(), "a b".into()], None)
            .await;
        assert_eq!(e.unwrap_err(), "argument not allowed over ssh: a b");
        let local = Node::new(&row("LOCAL", "d", "local", None), Path::new("/r"), Arc::new(Never)).unwrap();
        assert!(local.host(vec!["castor-zfs".into()], None).await.is_err());
    }
}
