//! A data-plane node:
//!   local       the Docker host the control plane runs on (development); LUKS
//!               volumes through the pc-cryptd image.
//!   ssh         a Mac mini: Docker over ssh://, and castor-* commands
//!               through the node's castor-ssh gate.
//!   gatekeeper  TrueNAS SCALE: every operation is one fixed tool of the
//!               castor gatekeeper instance (crate::gatekeeper), asked of the
//!               node role over the link (crate::link). castord has no
//!               gatekeeper token, no Docker socket, no SSH key and no mount
//!               of the host.
//!
//! A STACKS node runs customers' own stacks only; stacks::edge prepares it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sqlx::PgPool;
use zeroize::Zeroizing;

use crate::exec::{Cmd, Exec, ToolCall};
use crate::gatekeeper::{self as gk, Tool};
use crate::images::{Images, NodeImage};
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
    Gatekeeper {
        /// The node role's `host:port` (crate::link).
        link: String,
        /// /mnt/<pool>/castor/node: templates, node files, tenant files.
        root: String,
        /// Where tenant datasets are mounted.
        mount_root: String,
        /// <pool>/castor/t: the tenant datasets.
        datasets: String,
        tools: Vec<Tool>,
    },
}

impl PartialEq for Tool {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl Eq for Tool {}

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
            "TRUENAS" => {
                let Some(pool) = row.zfs_pool.as_deref().filter(|p| valid::pool(p)) else {
                    return Err(format!("node {}: no valid ZFS pool", row.name));
                };
                if role == Role::Stacks {
                    return Err(format!(
                        "node {}: custom stacks don't run on a gatekeeper node (one box, one boundary)",
                        row.name
                    ));
                }
                if row.edge_host.is_some() {
                    return Err(format!(
                        "node {}: a gatekeeper node has no edge proxy; Traefik reaches the stacks on pc-edge",
                        row.name
                    ));
                }
                if !crate::link::address_ok(&row.ssh_host) {
                    return Err(format!("node {}: invalid node role address {}", row.name, row.ssh_host));
                }
                Place::Gatekeeper {
                    link: row.ssh_host.clone(),
                    root: gk::root(pool),
                    mount_root: format!("/mnt/{pool}/castor/t"),
                    datasets: gk::dataset_root(pool),
                    tools: gk::catalog(pool),
                }
            }
            "MAC" => {
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
                let (root, mounts) = ("/Users/Shared/castor".to_owned(), format!("/Volumes/{pool}"));
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

    /// Reached through gatekeeper (TrueNAS).
    pub fn is_gatekeeper(&self) -> bool {
        matches!(self.place, Place::Gatekeeper { .. })
    }

    fn tools(&self) -> Result<&[Tool], String> {
        match &self.place {
            Place::Gatekeeper { tools, .. } => Ok(tools),
            _ => Err(format!("{} is not reached through gatekeeper", self.name)),
        }
    }

    /// The tenant datasets' parent on a gatekeeper node (<pool>/castor/t).
    pub fn dataset_root(&self) -> Option<&str> {
        match &self.place {
            Place::Gatekeeper { datasets, .. } => Some(datasets),
            _ => None,
        }
    }

    /// One gatekeeper tool, its arguments checked against the catalog first.
    pub async fn call(
        &self,
        id: &str,
        args: serde_json::Map<String, serde_json::Value>,
        secret: Option<Zeroizing<String>>,
    ) -> Result<String, String> {
        let call = gk::checked(self.tools()?, id, args, secret)?;
        self.tool(call).await
    }

    async fn tool(&self, call: ToolCall) -> Result<String, String> {
        match &self.place {
            Place::Gatekeeper { link, .. } => self.exec.tool(link, call).await,
            _ => Err(format!("{} is not reached through gatekeeper", self.name)),
        }
    }

    /// What castor-zfs pool and castor-info print, from the TrueNAS API:
    /// "pool <size-mb> <used-mb> <health>" and "info <os> <memory-mb> <cpus>".
    pub async fn truenas_facts(&self) -> Result<(String, String), String> {
        let tools = self.tools()?;
        let json = |text: String, what: &str| {
            serde_json::from_str::<serde_json::Value>(&text).map_err(|e| format!("{}: {what}: {e}", self.name))
        };
        let pools = json(
            self.tool(gk::plain_call(tools, "truenas.pool_query")?).await?,
            "pool.query",
        )?;
        let pool = pools
            .get(0)
            .ok_or_else(|| format!("{}: the pool is missing", self.name))?;
        let mb = |v: &serde_json::Value| v.as_u64().map(|b| b / crate::storage::BYTES_PER_MB);
        let (Some(size), Some(used), Some(status)) = (
            pool.get("size").and_then(mb),
            pool.get("allocated").and_then(mb),
            pool.get("status").and_then(|v| v.as_str()),
        ) else {
            return Err(format!("{}: unexpected pool.query answer", self.name));
        };
        let sys = json(
            self.tool(gk::plain_call(tools, "truenas.system_info")?).await?,
            "system.info",
        )?;
        let (Some(version), Some(memory), Some(cores)) = (
            sys.get("version").and_then(|v| v.as_str()),
            sys.get("physmem").and_then(mb),
            sys.get("cores").and_then(|v| v.as_u64()),
        ) else {
            return Err(format!("{}: unexpected system.info answer", self.name));
        };
        let word = |s: &str| s.split_whitespace().collect::<Vec<_>>().join("-");
        Ok((
            format!("pool {size} {used} {}", word(status)),
            format!("info {} {memory} {cores}", word(version)),
        ))
    }

    /// One of gatekeeper's file tools on `path`.
    async fn file_tool(&self, id: &str, path: &str, content: Option<&str>) -> Result<String, String> {
        let content = content.map(|c| Zeroizing::new(c.to_owned()));
        self.tool(gk::file_call(self.tools()?, id, path, content)?).await
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
        if let Place::Gatekeeper { tools, .. } = &self.place {
            let call = gk::docker_call(tools, &cmd)?;
            return match self.tool(call).await {
                Err(_) if allow_fail => Ok(String::new()),
                r => r,
            };
        }
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
        if self.is_gatekeeper() {
            return Err(format!("{}: docker cp is not available through gatekeeper", self.name));
        }
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
            // No stacks on a gatekeeper node (Node::new).
            Place::Gatekeeper { mount_root, .. } => Some((mount_root, "")),
        }
    }

    /// Compose files where the docker CLI reads them: on the control plane,
    /// or on the node when gatekeeper runs the CLI there.
    pub fn templates(&self) -> String {
        match &self.place {
            Place::Gatekeeper { root, .. } => format!("{root}/files/templates"),
            _ => self
                .root
                .join("packages/stack-templates")
                .to_string_lossy()
                .into_owned(),
        }
    }

    /// The same templates on the node: relative bind mounts resolve there.
    pub fn node_templates(&self) -> String {
        match &self.place {
            Place::Local => self.templates(),
            Place::Ssh { root, .. } | Place::Gatekeeper { root, .. } => format!("{root}/files/templates"),
        }
    }

    /// infra/node (shared services, the LLM gateway's config) where the docker CLI reads it.
    pub fn infra(&self) -> String {
        match &self.place {
            Place::Gatekeeper { root, .. } => format!("{root}/files/node"),
            _ => self.root.join("infra/node").to_string_lossy().into_owned(),
        }
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
            Place::Ssh { root, .. } | Place::Gatekeeper { root, .. } => format!("{root}/{remote}"),
        }
    }

    /// TENANT_DIR in the stack: where write_node_file puts files.
    pub fn node_config_dir(&self, slug: &str) -> String {
        match &self.place {
            Place::Local => self.config_dir(slug),
            Place::Ssh { root, .. } | Place::Gatekeeper { root, .. } => format!("{root}/tenants/{slug}"),
        }
    }

    /// A file the docker CLI reads (tenant.env, compose.volumes.yml), owner-only:
    /// tenant.env holds every secret of the stack. On the control plane, or on
    /// the node when gatekeeper runs the CLI there.
    pub async fn write_config(&self, slug: &str, file: &str, content: &str) -> Result<String, String> {
        check(slug, file)?;
        if self.is_gatekeeper() {
            let path = format!("{}/{file}", self.config_dir(slug));
            self.file_tool("file.tenant_write", &path, Some(content)).await?;
            return Ok(path);
        }
        write_private(
            &self.root.join(".tenants").join(slug).join(file),
            content.as_bytes(),
            false,
        )
    }

    /// The env file for the shared services' compose, and the environment the
    /// docker CLI gets with it. On a gatekeeper node the CLI has no environment
    /// of castord's: versions, `vars` and the LLM gateway's settings go into
    /// <root>/shared.env (0600).
    pub async fn shared_env(
        &self,
        versions_file: &Path,
        vars: Vec<(&'static str, String)>,
    ) -> Result<(String, Vec<(&'static str, String)>), String> {
        let Place::Gatekeeper { root, .. } = &self.place else {
            return Ok((versions_file.to_string_lossy().into_owned(), vars));
        };
        let versions =
            std::fs::read_to_string(versions_file).map_err(|e| format!("{}: {e}", versions_file.display()))?;
        let mut text = Zeroizing::new(versions);
        if !text.ends_with('\n') {
            text.push('\n');
        }
        // What compose.yml reads from castord's environment.
        let from_env = [
            "AI_MODEL",
            "AI_PROVIDER",
            "LLM_BASE_URL",
            "LLM_API_KEY",
            "LITELLM_MASTER_KEY",
            "LITELLM_DB_PASSWORD",
        ];
        let set: Vec<(&str, String)> = from_env
            .iter()
            .filter_map(|k| crate::config::var(k).map(|v| (*k, v)))
            .chain(vars)
            .collect();
        for (k, v) in set {
            if v.contains(['\'', '\n', '\r']) {
                return Err(format!("{k}: quotes and line breaks can't go into an env file"));
            }
            text.push_str(&format!("{k}='{v}'\n"));
        }
        let path = format!("{root}/shared.env");
        self.file_tool("file.shared_env_write", &path, Some(&text)).await?;
        Ok((path, Vec::new()))
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
        if self.is_gatekeeper() {
            let path = format!("{}/{file}", self.node_config_dir(slug));
            let id = if readable {
                "file.tenant_write_readable"
            } else {
                "file.tenant_write"
            };
            self.file_tool(id, &path, Some(content)).await?;
            return Ok(path);
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
        if self.is_gatekeeper() {
            let path = format!("{}/{dir}", self.node_config_dir(slug));
            self.file_tool("file.tenant_delete", &path, None).await?;
            return Ok(());
        }
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
    /// the edge proxy. Both get the assistant's tools image (built, or pulled
    /// signed: crate::images).
    pub async fn prepare(&self, ca_dir: &Path, images: &Images) -> Result<(), String> {
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
                images
                    .install(self, NodeImage::Cryptd, CRYPTD_IMAGE, vec![root("infra/node/cryptd")])
                    .await?;
            }
            Place::Gatekeeper { root: node_root, .. } => {
                let ca = ca_dir.to_string_lossy().into_owned();
                self.sync_files(node_root, "templates", &root("packages/stack-templates"), &["."])
                    .await?;
                self.sync_files(node_root, "node", &root("infra/node"), &["compose.yml", "litellm"])
                    .await?;
                self.sync_files(node_root, "brand", &root("packages/brand"), &BRAND)
                    .await?;
                self.sync_files(node_root, "ca", &ca, &["ca-bundle.pem"]).await?;
                // The control-plane Traefik reaches the skin proxies on pc-edge.
                let tools = self.tools()?;
                if self
                    .tool(gk::plain_call(tools, "docker.edge_network_inspect")?)
                    .await
                    .is_err()
                {
                    self.tool(gk::plain_call(tools, "docker.edge_network_create")?).await?;
                }
            }
            Place::Ssh { .. } if self.role == Role::Stacks => return crate::stacks::edge::prepare(self, images).await,
            Place::Ssh { .. } => {
                let ca = ca_dir.to_string_lossy().into_owned();
                self.sync("templates", &root("packages/stack-templates"), &["."])
                    .await?;
                // Relative bind mounts of the shared services (the LLM gateway's config).
                self.sync("node", &root("infra/node"), &["compose.yml", "litellm"])
                    .await?;
                self.sync("brand", &root("packages/brand"), &BRAND).await?;
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
        images
            .install(
                self,
                NodeImage::AssistantTools,
                ASSISTANT_TOOLS_IMAGE,
                vec![
                    "-f".into(),
                    root("agent/tools/Dockerfile"),
                    self.root.to_string_lossy().into_owned(),
                ],
            )
            .await?;
        Ok(())
    }

    /// Replaces <root>/files/<name> with files of a control-plane directory,
    /// one gatekeeper write per file (text only: templates, configs, CSS, SVG).
    async fn sync_files(&self, node_root: &str, name: &str, dir: &str, entries: &[&str]) -> Result<(), String> {
        let base = Path::new(dir);
        if !base.is_dir() {
            return Err(format!("{dir} missing"));
        }
        let mut files: Vec<PathBuf> = Vec::new();
        for e in entries {
            collect_files(&base.join(e), &mut files)?;
        }
        let target = format!("{node_root}/files/{name}");
        self.file_tool("file.node_delete", &target, None).await?;
        for f in files {
            let rel = f
                .strip_prefix(base)
                .map_err(|e| e.to_string())?
                .to_string_lossy()
                .trim_start_matches("./")
                .to_owned();
            let content = std::fs::read_to_string(&f).map_err(|e| format!("{}: {e}", f.display()))?;
            self.file_tool("file.node_write", &format!("{target}/{rel}"), Some(&content))
                .await?;
        }
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
        if self.is_gatekeeper() {
            self.file_tool("file.tenant_delete", &self.node_config_dir(slug), None)
                .await?;
        } else if !self.is_local() {
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

    /// The tenant's env file and compose override: on the control plane, or
    /// on the node when gatekeeper runs the docker CLI there.
    pub fn config_dir(&self, slug: &str) -> String {
        match &self.place {
            Place::Gatekeeper { root, .. } => format!("{root}/tenants/{slug}"),
            _ => self.root.join(".tenants").join(slug).to_string_lossy().into_owned(),
        }
    }
}

/// What the skin proxy serves under /_brand/ (not the design sources).
const BRAND: [&str; 6] = [
    "tokens.css",
    "theme.css",
    "bridge.js",
    "logo.svg",
    "icon.svg",
    "services",
];

/// Regular files below `path` (or `path` itself), sorted; links are refused.
fn collect_files(path: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.is_file() {
        out.push(path.to_owned());
        return Ok(());
    }
    if !meta.is_dir() {
        return Err(format!("{}: not a file or directory", path.display()));
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .map(|e| e.map(|e| e.path()))
        .collect::<Result<_, _>>()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    entries.sort();
    for e in entries {
        collect_files(&e, out)?;
    }
    Ok(())
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
            &row("TRUENAS", "castor-node", "castor-node:7443", Some("tank")),
            root,
            Arc::new(Never),
        )
        .unwrap();
        // Through gatekeeper the docker CLI runs on the node: everything it reads is there.
        assert!(nas.is_gatekeeper());
        assert_eq!(nas.node_templates(), "/mnt/tank/castor/node/files/templates");
        assert_eq!(nas.templates(), "/mnt/tank/castor/node/files/templates");
        assert_eq!(nas.config_dir("anna"), "/mnt/tank/castor/node/tenants/anna");
        assert_eq!(nas.infra(), "/mnt/tank/castor/node/files/node");
        let mac = Node::new(&row("MAC", "deploy", "mini.lan", Some("castor")), root, Arc::new(Never)).unwrap();
        assert_eq!(mac.node_templates(), "/Users/Shared/castor/files/templates");
        assert_eq!(mac.templates(), "/repo/packages/stack-templates");
        assert_eq!(mac.config_dir("anna"), "/repo/.tenants/anna");
        let local = Node::new(&row("LOCAL", "deploy", "local", None), root, Arc::new(Never)).unwrap();
        assert_eq!(local.node_templates(), "/repo/packages/stack-templates");

        let mut stacks = row("TRUENAS", "castor-node", "nas:7443", Some("tank"));
        stacks.role = "STACKS".into();
        let mut edge = row("TRUENAS", "castor-node", "nas:7443", Some("tank"));
        edge.edge_host = Some("10.0.0.5:8480".into());
        for bad in [
            row("MAC", "deploy", "-oProxyCommand=x", Some("tank")),
            row("MAC", "de ploy", "nas", Some("tank")),
            row("MAC", "deploy", "nas", Some("tank;rm")),
            row("TRUENAS", "castor-node", "nas:7443", Some("tank;rm")),
            row("TRUENAS", "castor-node", "nas:7443", None),
            row("TRUENAS", "castor-node", "nas", Some("tank")),
            row("TRUENAS", "castor-node", "-x:7443", Some("tank")),
            row("VM", "deploy", "nas", Some("tank")),
            stacks,
            edge,
        ] {
            assert!(Node::new(&bad, root, Arc::new(Never)).is_err(), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn host_words_are_checked_before_ssh() {
        let nas = Node::new(
            &row("MAC", "deploy", "nas", Some("tank")),
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
