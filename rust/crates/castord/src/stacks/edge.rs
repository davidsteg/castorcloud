//! A STACKS node's edge proxy and Docker checks (stacks-node.ts). The proxy
//! (infra/node/stacks-edge.yml) has no Docker socket: its routes are files
//! copied into its routes volume, and it joins each stack's edge network,
//! where only that stack's web service is.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use zeroize::Zeroizing;

use super::render;
use crate::node::Node;
use crate::suspend::strings;

/// Container name of the stacks edge proxy (compose project pc-stacks-edge, service traefik).
pub const STACKS_EDGE: &str = "pc-stacks-edge-traefik-1";
/// Built on each STACKS node from infra/node/egress; no registry serves "castor.local/".
pub const EGRESS_IMAGE: &str = "castor.local/egress:1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    /// Docker runs containers in a user namespace (userns-remap).
    pub userns: bool,
    /// gVisor is installed as a Docker runtime.
    pub runsc: bool,
    pub memory_mb: u64,
    pub cpus: u64,
    pub arch: String,
}

pub async fn facts(node: &Node) -> Result<Facts, String> {
    let out = node.docker(strings(&["info", "--format", "{{json .}}"]), false).await?;
    let info: Value = serde_json::from_str(&out).map_err(|e| format!("{}: docker info: {e}", node.name))?;
    let options: Vec<&str> = info["SecurityOptions"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    Ok(Facts {
        userns: options
            .iter()
            .any(|o| *o == "name=userns" || o.starts_with("name=userns,")),
        runsc: info["Runtimes"].as_object().is_some_and(|r| r.contains_key("runsc")),
        memory_mb: info["MemTotal"].as_u64().unwrap_or(0) / (1024 * 1024),
        cpus: info["NCPU"].as_u64().unwrap_or(0),
        arch: info["Architecture"].as_str().unwrap_or("").to_owned(),
    })
}

/// Refuses a node whose Docker no longer isolates stacks: userns-remap must
/// be on, and a node registered with gVisor must still have it (never a
/// silent fall back to runc).
pub async fn check(node: &Node, runtime: Option<&str>) -> Result<Facts, String> {
    let f = facts(node).await?;
    if !f.userns {
        return Err(format!(
            "{}: Docker runs without userns-remap; refusing to run stacks",
            node.name
        ));
    }
    if runtime == Some("runsc") && !f.runsc {
        return Err(format!(
            "{}: the gVisor runtime (runsc) is gone; refusing to run stacks",
            node.name
        ));
    }
    Ok(f)
}

fn route_name(name: &str) -> Result<(), String> {
    let ok = (1..=100).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(format!("invalid route name {name}"))
    }
}

/// Writes a route file into the stacks edge proxy (it watches the directory).
pub async fn put_route(node: &Node, name: &str, content: &str) -> Result<(), String> {
    route_name(name)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let tar = super::tar_file(&format!("{name}.yml"), content.as_bytes(), 0o644, now)?;
    node.docker_with(
        strings(&["cp", "-", &format!("{STACKS_EDGE}:/routes")]),
        Some(Zeroizing::new(tar)),
        false,
    )
    .await?;
    Ok(())
}

pub async fn remove_route(node: &Node, name: &str) -> Result<(), String> {
    route_name(name)?;
    node.docker(
        strings(&["exec", STACKS_EDGE, "rm", "-f", &format!("/routes/{name}.yml")]),
        true,
    )
    .await?;
    Ok(())
}

/// Lets the edge proxy reach a stack's web service (idempotent).
pub async fn connect(node: &Node, network: &str) -> Result<(), String> {
    let joined = node
        .docker(
            strings(&["inspect", "--format", "{{json .NetworkSettings.Networks}}", STACKS_EDGE]),
            false,
        )
        .await?;
    let joined: Value = serde_json::from_str(&joined).map_err(|e| format!("{STACKS_EDGE} networks: {e}"))?;
    if joined.as_object().is_some_and(|n| n.contains_key(network)) {
        return Ok(());
    }
    node.docker(strings(&["network", "connect", network, STACKS_EDGE]), false)
        .await?;
    Ok(())
}

/// Before a stack's networks are removed.
pub async fn disconnect(node: &Node, network: &str) -> Result<(), String> {
    node.docker(strings(&["network", "disconnect", "-f", network, STACKS_EDGE]), true)
        .await?;
    Ok(())
}

/// Once per process: builds the egress proxy image, (re)starts the edge
/// proxy, writes its allowlist and reconnects it to every stack.
pub async fn prepare(node: &Node) -> Result<(), String> {
    if node.control_plane_ips.is_empty() {
        return Err(format!("{}: stacks node without control-plane addresses", node.name));
    }
    check(node, None).await?;
    let infra = node.root().join("infra/node");
    let infra = infra.to_string_lossy();
    node.docker(
        strings(&["build", "-q", "-t", EGRESS_IMAGE, &format!("{infra}/egress")]),
        false,
    )
    .await?;
    node.docker_env(
        strings(&[
            "compose",
            "-p",
            "pc-stacks-edge",
            "-f",
            &format!("{infra}/stacks-edge.yml"),
            "up",
            "-d",
            "--remove-orphans",
        ]),
        None,
        vec![
            ("EDGE_PORT", node.edge_port().to_string()),
            ("CONTROL_PLANE_IPS", node.control_plane_ips.join(",")),
        ],
        false,
    )
    .await?;
    put_route(node, "castor", &render::node_middlewares(&node.control_plane_ips)?).await?;
    let networks = node
        .docker(
            strings(&["network", "ls", "--filter", "name=^pc-s-", "--format", "{{.Name}}"]),
            false,
        )
        .await?;
    for n in networks.split('\n').map(str::trim) {
        if render::is_edge_network(n) {
            connect(node, n).await?;
        }
    }
    Ok(())
}
