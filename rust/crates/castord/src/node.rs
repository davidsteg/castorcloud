//! A data-plane node:
//!   local  the Docker host the control plane runs on (development); LUKS
//!          volumes through the pc-cryptd image.
//!   ssh    TrueNAS SCALE or a Mac mini: Docker over ssh://, and castor-*
//!          commands through the node's castor-ssh gate.
//!
//! A STACKS node runs customers' own stacks only; stacks::edge prepares it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sqlx::PgPool;
use zeroize::Zeroizing;

use crate::exec::{Cmd, Exec};
use crate::valid;

/// The tag the pc-cryptd image is built as.
pub const CRYPTD_IMAGE: &str = "pc-cryptd:1";
/// Same tag as ASSISTANT_TOOLS_IMAGE in packages/stack-templates/versions.env.
pub const ASSISTANT_TOOLS_IMAGE: &str = "pc-assistant-tools:2";

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
        /// castor-ssh's ROOT.
        root: String,
        /// castor-zfs's MOUNT_ROOT and STACKS_MOUNT_ROOT: where datasets are mounted.
        mount_root: String,
        stacks_mount_root: String,
    },
}

/// The columns of "Node" a driver needs.
#[derive(Debug, Clone, Default, sqlx::FromRow)]
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
    #[sqlx(rename = "authHostIp")]
    pub auth_host_ip: Option<String>,
    #[sqlx(rename = "edgeHost")]
    pub edge_host: Option<String>,
    /// STACKS: where the control plane connects from, comma-separated.
    #[sqlx(rename = "controlPlaneIps")]
    pub control_plane_ips: Option<String>,
    /// STACKS: the host uid root in a container maps to (userns-remap).
    #[sqlx(rename = "uidBase")]
    pub uid_base: Option<i32>,
    /// STACKS: "runsc" (gVisor) when the node has it, else "runc".
    #[sqlx(rename = "stackRuntime")]
    pub stack_runtime: Option<String>,
}

impl NodeRow {
    pub async fn load(db: &PgPool, id: &str) -> Result<Self, String> {
        sqlx::query_as::<_, NodeRow>(
            r#"SELECT id, name, kind::text AS kind, role::text AS role, "sshHost", "sshUser", "zfsPool",
                      "authHostIp", "edgeHost", "controlPlaneIps", "uidBase", "stackRuntime"
               FROM "Node" WHERE id = $1"#,
        )
        .bind(id)
        .fetch_optional(db)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("node {id} not found"))
    }
}

/// Where the control plane reaches a node's edge proxy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    pub host: String,
    pub port: u16,
    pub url: String,
}

/// "10.0.0.5:8480" or "10.0.0.5" (port 80).
/// ssh to `target` (user@host) running one forced command's words: castor-ssh
/// and castor-backup split on spaces and allow only these.
pub fn ssh_cmd(target: &str, args: Vec<String>) -> Result<Cmd, String> {
    if !valid::ssh_target(target) {
        return Err(format!("invalid ssh target: {target}"));
    }
    if let Some(bad) = args.iter().find(|a| !valid::ssh_word(a)) {
        return Err(format!("argument not allowed over ssh: {bad}"));
    }
    let mut all = vec!["-o".into(), "BatchMode=yes".into(), "-T".into(), target.to_owned()];
    all.extend(args);
    Ok(Cmd::new("ssh", all))
}

pub fn parse_edge(edge_host: &str) -> Result<Edge, String> {
    let bad = || format!("invalid edge host: {edge_host}");
    let (host, port) = match edge_host.split_once(':') {
        Some((h, p)) => {
            if p.is_empty() || p.len() > 5 || !p.bytes().all(|b| b.is_ascii_digit()) {
                return Err(bad());
            }
            (h, p.parse::<u16>().map_err(|_| bad())?)
        }
        None => (edge_host, 80),
    };
    if host.is_empty() || port == 0 || !host.bytes().all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b)) {
        return Err(bad());
    }
    let url = if port == 80 {
        format!("http://{host}")
    } else {
        format!("http://{host}:{port}")
    };
    Ok(Edge {
        host: host.to_owned(),
        port,
        url,
    })
}

pub struct Node {
    pub id: String,
    pub name: String,
    pub role: Role,
    /// Overrides the platform's AUTH_HOST_IP for stacks on this node.
    pub auth_host_ip: Option<String>,
    /// Unset for the local node, whose stacks sit on the control plane Traefik's network.
    pub edge: Option<Edge>,
    /// STACKS: the only addresses its edge proxy lets in.
    pub control_plane_ips: Vec<String>,
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
        let edge = match (&place, row.edge_host.as_deref()) {
            (Place::Ssh { .. }, Some(e)) => Some(parse_edge(e)?),
            _ => None,
        };
        Ok(Self {
            id: row.id.clone(),
            name: row.name.clone(),
            role,
            auth_host_ip: row.auth_host_ip.clone().filter(|_| place != Place::Local),
            edge,
            control_plane_ips: row
                .control_plane_ips
                .as_deref()
                .unwrap_or("")
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect(),
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
        self.docker_env(args, input, vec![], allow_fail).await
    }

    /// With extra environment for the docker CLI (compose variables).
    pub async fn docker_env(
        &self,
        args: Vec<String>,
        input: Option<Zeroizing<Vec<u8>>>,
        env: Vec<(&'static str, String)>,
        allow_fail: bool,
    ) -> Result<String, String> {
        let mut cmd = Cmd::new("docker", args);
        cmd.input = input;
        cmd.env = env;
        if let Place::Ssh { target, .. } = &self.place {
            cmd.env.push(("DOCKER_HOST", format!("ssh://{target}")));
            cmd.env.push(("DOCKER_CONTEXT", String::new()));
        }
        match self.exec.run(cmd).await {
            Err(_) if allow_fail => Ok(String::new()),
            r => r,
        }
    }

    /// Like docker(), for binary output (docker cp ... -).
    pub async fn docker_bytes(&self, args: Vec<String>) -> Result<Vec<u8>, String> {
        let mut cmd = Cmd::new("docker", args);
        if let Place::Ssh { target, .. } = &self.place {
            cmd.env.push(("DOCKER_HOST", format!("ssh://{target}")));
            cmd.env.push(("DOCKER_CONTEXT", String::new()));
        }
        self.exec.run_bytes(cmd).await
    }

    /// Edge port on the LAN (the stacks edge publishes the same one).
    pub fn edge_port(&self) -> u16 {
        self.edge.as_ref().map_or(80, |e| e.port)
    }

    /// The repository on the control plane.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// A castor-* command on the node's host, through the castor-ssh gate.
    pub async fn host(&self, args: Vec<String>, input: Option<Zeroizing<Vec<u8>>>) -> Result<String, String> {
        let mut cmd = self.host_cmd(args)?;
        cmd.input = input;
        self.exec.run(cmd).await
    }

    /// The ssh command `host` runs, for a pipe between two hosts.
    pub fn host_cmd(&self, args: Vec<String>) -> Result<Cmd, String> {
        let Place::Ssh { target, .. } = &self.place else {
            return Err(format!("{} has no host commands", self.name));
        };
        ssh_cmd(target, args)
    }

    /// `from`'s stdout into a castor-* command on this host.
    pub async fn pipe_into(&self, from: Cmd, args: Vec<String>) -> Result<u64, String> {
        let to = self.host_cmd(args)?;
        self.exec.pipe(from, to).await
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

    /// Brings the node up to date with this deployment before stacks start
    /// (idempotent). Local: the shared /srv/pc mount and the
    /// cryptd image. Remote: templates, the shared services' files, brand
    /// assets and the CA bundle (only ca-bundle.pem, never a CA key), then
    /// the edge proxy. Both build the assistant's tools image.
    pub async fn prepare(&self, ca_dir: &Path) -> Result<(), String> {
        let root = |p: &str| self.root.join(p).to_string_lossy().into_owned();
        let s = |v: &[&str]| v.iter().map(|a| (*a).to_owned()).collect::<Vec<String>>();
        match &self.place {
            Place::Local => {
                // /srv/pc must be a shared mount so volumes opened by cryptd are
                // visible to tenant containers; Docker Desktop's VM loses it on restart.
                self.docker(
                    s(&[
                        "run",
                        "--rm",
                        "--privileged",
                        "--pid=host",
                        "alpine:3.22",
                        "nsenter",
                        "-t",
                        "1",
                        "-m",
                        "--",
                        "sh",
                        "-c",
                        "mkdir -p /srv/pc/volumes /srv/pc/tenants && (findmnt -no PROPAGATION /srv/pc | grep -q shared || (mount --bind /srv/pc /srv/pc && mount --make-rshared /srv/pc))",
                    ]),
                    false,
                )
                .await?;
                self.docker(
                    vec![
                        "build".into(),
                        "-q".into(),
                        "-t".into(),
                        CRYPTD_IMAGE.into(),
                        root("infra/node/cryptd"),
                    ],
                    false,
                )
                .await?;
            }
            Place::Ssh { .. } if self.role == Role::Stacks => return crate::stacks::edge::prepare(self).await,
            Place::Ssh { .. } => {
                let ca = ca_dir.to_string_lossy().into_owned();
                self.sync("templates", &root("packages/stack-templates"), &["."])
                    .await?;
                // Relative bind mounts of the shared services (the LLM gateway's config).
                self.sync("node", &root("infra/node"), &["compose.yml", "litellm"])
                    .await?;
                // What the skin proxy serves under /_brand/ (not the design sources).
                let brand = [
                    "tokens.css",
                    "theme.css",
                    "bridge.js",
                    "logo.svg",
                    "icon.svg",
                    "services",
                ];
                self.sync("brand", &root("packages/brand"), &brand).await?;
                self.sync("ca", &ca, &["ca-bundle.pem"]).await?;
                if self
                    .docker(s(&["network", "inspect", "pc-edge"]), true)
                    .await?
                    .is_empty()
                {
                    self.docker(s(&["network", "create", "pc-edge"]), false).await?;
                }
                let port = self.edge_port();
                let up = vec![
                    "compose".into(),
                    "-p".into(),
                    "pc-edge".into(),
                    "-f".into(),
                    root("infra/node/edge.yml"),
                    "up".into(),
                    "-d".into(),
                    "--remove-orphans".into(),
                ];
                self.docker_env(up, None, vec![("EDGE_PORT", port.to_string())], false)
                    .await?;
            }
        }
        self.docker(
            vec![
                "build".into(),
                "-q".into(),
                "-t".into(),
                ASSISTANT_TOOLS_IMAGE.into(),
                "-f".into(),
                root("agent/tools/Dockerfile"),
                self.root.to_string_lossy().into_owned(),
            ],
            false,
        )
        .await?;
        Ok(())
    }

    /// Sends files of a control-plane directory to the node (castor-untar <name>).
    async fn sync(&self, name: &str, dir: &str, files: &[&str]) -> Result<(), String> {
        if !Path::new(dir).is_dir() {
            return Err(format!("{dir} missing"));
        }
        let mut args = vec!["-C".to_owned(), dir.to_owned(), "-cf".into(), "-".into()];
        args.extend(files.iter().map(|f| (*f).to_owned()));
        let tar = self.exec.run_bytes(Cmd::new("tar", args)).await?;
        self.host(vec!["castor-untar".into(), name.into()], Some(Zeroizing::new(tar)))
            .await?;
        Ok(())
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

    /// Removes rendered files of a tenant on the control plane (stacks/<name>).
    pub fn remove_local_config(&self, slug: &str, dir: &str) -> Result<(), String> {
        check(slug, dir)?;
        let path = self.root.join(".tenants").join(slug).join(dir);
        match std::fs::remove_dir_all(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("{}: {e}", path.display())),
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
            ..Default::default()
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
