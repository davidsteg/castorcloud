//! Nodes reached through gatekeeper (docs/gatekeeper.md). Every operation on
//! such a node is one fixed tool of the castor gatekeeper instance, with
//! typed parameters. castord asks the node role for it (crate::link); only
//! the node role holds the gatekeeper token. Neither has a Docker socket, an
//! SSH key or a mount of the host.
//!
//! The tools are defined here, once: `castor-admin gatekeeper tools <pool>`
//! prints them as gatekeeper's tools.yaml (infra/gatekeeper/), castord maps
//! each docker command line it would run onto the tool whose argv template
//! produces exactly that command line, and the node role checks every call
//! against the same catalog again. A command line no tool produces is
//! refused before anything leaves castord.

pub mod client;

use serde_json::{Map, Value};
use zeroize::Zeroizing;

use crate::exec::{Cmd, ToolCall};
use crate::revoke::{ASSISTANT_KEY_NAME, DOCS_SCRIPT, FLIGHTS_SQL, PHOTOS_SQL, PLACES_SCRIPT};
use crate::storage::BYTES_PER_MB;
use crate::suspend::ALL_PROFILES;
use crate::valid;

/// The node's root on the pool, as gatekeeper and its Docker engine see it.
pub fn root(pool: &str) -> String {
    format!("/mnt/{pool}/castor/node")
}

/// Where tenant datasets live: <pool>/castor/t/<slug>, mounted at /mnt/<it>.
pub fn dataset_root(pool: &str) -> String {
    format!("{pool}/castor/t")
}

/// The containers of the node's shared services (delete::SHARED_CONTAINERS).
const SHARED: [&str; 3] = ["pc-node-immich-ml", "pc-node-tika", "pc-node-gotenberg"];
const LLM_GATEWAY: &str = "pc-node-llm-gateway";
/// What castord synchronises below <root>/files.
pub const FILE_SETS: [&str; 4] = ["templates", "node", "brand", "ca"];

/// How a parameter is checked: here before a call, and by gatekeeper with
/// the pattern or values `castor-admin gatekeeper tools` writes.
#[derive(Debug, Clone)]
pub enum Check {
    Slug,
    OneOf(Vec<String>),
    /// A person's e-mail as the apps store it: no whitespace or control characters.
    Email,
    /// toISOString(): 2026-10-18T19:38:04.000Z.
    Instant,
    /// An OIDC subject.
    Sub,
    /// The assistant's key name, or empty (a removed member keeps nothing).
    Keep,
    /// repo@sha256:<digest>, never a tag (design: castord verified the signature).
    PinnedImage,
    /// A path below `root`: `<root>/<segment>[/<path>]`, where segment is
    /// checked by `first`, and further segments are plain names.
    Path {
        root: String,
        first: FirstSegment,
        rest: Rest,
    },
    /// File content, written verbatim.
    Content,
    /// A tenant's dataset below `root` (<pool>/castor/t): `<root>/<slug>`,
    /// and with `child` one of its data directories `<root>/<slug>/<dir>`.
    Dataset {
        root: String,
        child: bool,
    },
    /// A size in bytes, sent to TrueNAS as a JSON number: at least 1 MB.
    Bytes,
    /// A dataset passphrase: 64 hex characters (HKDF of the volume key).
    Passphrase,
    /// A backup snapshot's name: cb-<UTC time> (retention::snapshot_name).
    SnapName,
    /// A backup snapshot of a tenant's dataset: <root>/<slug>@cb-<UTC time>.
    Snapshot {
        root: String,
    },
}

/// Stands for the replication task's id in a tool's params until
/// tools_yaml writes the operator's (it lives in gatekeeper's tools.yaml only).
pub const REPLICATION_TASK: &str = "<replication task>";

const SNAP_RE: &str = "^cb-[0-9]{8}T[0-9]{4}Z$";

/// cb-20260928T0605Z.
fn snap_name(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 17
        && s.starts_with("cb-")
        && b[3..11].iter().all(u8::is_ascii_digit)
        && b[11] == b'T'
        && b[12..16].iter().all(u8::is_ascii_digit)
        && b[16] == b'Z'
}

/// The largest quota a tool accepts: 1 PB.
const MAX_BYTES: u64 = 1 << 50;

#[derive(Debug, Clone, Copy)]
pub enum FirstSegment {
    Slug,
    FileSet,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Rest {
    None,
    Optional,
    Required,
}

/// A data directory's name (a compose volume): what castor-zfs allows too.
fn data_dir_name(s: &str) -> bool {
    (1..=40).contains(&s.len())
        && !s.starts_with('-')
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn plain_segment(s: &str) -> bool {
    !s.is_empty() && !s.starts_with('.') && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._=-".contains(&b))
}

const SEGMENT_RE: &str = "[A-Za-z0-9_=-][A-Za-z0-9._=-]*";

impl Check {
    pub fn ok(&self, v: &str) -> bool {
        match self {
            Check::Slug => valid::slug(v),
            Check::OneOf(values) => values.iter().any(|x| x == v),
            Check::Email => (3..=254).contains(&v.len()) && !v.chars().any(|c| c.is_whitespace() || c.is_control()),
            Check::Instant => {
                let b = v.as_bytes();
                let digit = |i: usize| b[i].is_ascii_digit();
                b.len() == 24
                    && [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18, 20, 21, 22]
                        .into_iter()
                        .all(digit)
                    && b[4] == b'-'
                    && b[7] == b'-'
                    && b[10] == b'T'
                    && b[13] == b':'
                    && b[16] == b':'
                    && b[19] == b'.'
                    && b[23] == b'Z'
            }
            Check::Sub => {
                (1..=255).contains(&v.len()) && v.bytes().all(|b| b.is_ascii_alphanumeric() || b"._:@|-".contains(&b))
            }
            Check::Keep => v.is_empty() || v == ASSISTANT_KEY_NAME,
            Check::PinnedImage => {
                let Some((repo, digest)) = v.split_once("@sha256:") else {
                    return false;
                };
                digest.len() == 64
                    && digest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    && !repo.starts_with(['-', '.', '/'])
                    && repo.split('/').count() >= 2
                    && repo.split('/').all(|s| {
                        !s.is_empty()
                            && s.bytes()
                                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
                    })
            }
            Check::Path { root, first, rest } => {
                let Some(below) = v.strip_prefix(root.as_str()).and_then(|r| r.strip_prefix('/')) else {
                    return false;
                };
                let (head, tail) = match below.split_once('/') {
                    Some((h, t)) => (h, Some(t)),
                    None => (below, None),
                };
                let head_ok = match first {
                    FirstSegment::Slug => valid::slug(head),
                    FirstSegment::FileSet => FILE_SETS.contains(&head),
                };
                let tail_ok = match (rest, tail) {
                    (Rest::None, None) | (Rest::Optional, None) => true,
                    (Rest::None, Some(_)) | (Rest::Required, None) => false,
                    (_, Some(t)) => t.split('/').all(plain_segment),
                };
                head_ok && tail_ok
            }
            Check::Content => true,
            Check::Dataset { root, child } => {
                let Some(below) = v.strip_prefix(root.as_str()).and_then(|r| r.strip_prefix('/')) else {
                    return false;
                };
                match (below.split_once('/'), child) {
                    (None, false) => valid::slug(below),
                    (Some((slug, dir)), true) => valid::slug(slug) && data_dir_name(dir),
                    _ => false,
                }
            }
            Check::Bytes => false,
            Check::SnapName => snap_name(v),
            Check::Snapshot { root } => v.split_once('@').is_some_and(|(ds, snap)| {
                snap_name(snap)
                    && Check::Dataset {
                        root: root.clone(),
                        child: false,
                    }
                    .ok(ds)
            }),
            Check::Passphrase => v.len() == 64 && v.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        }
    }

    /// A parameter's value as it travels: numbers for Bytes, strings otherwise.
    pub fn ok_value(&self, v: &Value) -> bool {
        match self {
            Check::Bytes => v.as_u64().is_some_and(|n| (BYTES_PER_MB..=MAX_BYTES).contains(&n)),
            _ => v.as_str().is_some_and(|s| self.ok(s)),
        }
    }

    /// gatekeeper's parameter definition (type and pattern or values).
    fn spec(&self) -> Vec<(&'static str, Value)> {
        let pattern = |p: String| vec![("type", Value::from("string")), ("pattern", Value::from(p))];
        match self {
            Check::Slug => pattern("^[a-z0-9](?:[a-z0-9]|-(?!-)){1,28}[a-z0-9]$".into()),
            Check::OneOf(values) => vec![
                ("type", Value::from("enum")),
                (
                    "values",
                    Value::Array(values.iter().map(|v| Value::from(v.as_str())).collect()),
                ),
            ],
            Check::Email => pattern(r"^[^\s\x00-\x1f\x7f]{3,254}$".into()),
            Check::Instant => pattern(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{3}Z$".into()),
            Check::Sub => pattern(r"^[A-Za-z0-9._:@|-]{1,255}$".into()),
            Check::Keep => pattern(format!("^(?:{})?$", regex_escape(ASSISTANT_KEY_NAME))),
            Check::PinnedImage => pattern(r"^[a-z0-9._-]+(?:/[a-z0-9._-]+)+@sha256:[0-9a-f]{64}$".into()),
            Check::Path { root, first, rest } => {
                let head = match first {
                    FirstSegment::Slug => "[a-z0-9](?:[a-z0-9]|-(?!-)){1,28}[a-z0-9]".to_owned(),
                    FirstSegment::FileSet => format!("(?:{})", FILE_SETS.join("|")),
                };
                let tail = match rest {
                    Rest::None => String::new(),
                    Rest::Optional => format!("(?:/{SEGMENT_RE})*"),
                    Rest::Required => format!("(?:/{SEGMENT_RE})+"),
                };
                pattern(format!("^{}/{head}{tail}$", regex_escape(root)))
            }
            Check::Content => {
                let mut v = pattern(r"^[\s\S]*$".into());
                v.push(("allow_control_characters", Value::Bool(true)));
                v
            }
            Check::Dataset { root, child } => {
                let slug = "[a-z0-9](?:[a-z0-9]|-(?!-)){1,28}[a-z0-9]";
                let dir = if *child { "/[a-z0-9-]{1,40}" } else { "" };
                pattern(format!("^{}/{slug}{dir}$", regex_escape(root)))
            }
            Check::Bytes => vec![
                ("type", Value::from("integer")),
                ("minimum", Value::from(BYTES_PER_MB)),
                ("maximum", Value::from(MAX_BYTES)),
            ],
            Check::Passphrase => pattern("^[0-9a-f]{64}$".into()),
            Check::SnapName => pattern(SNAP_RE.into()),
            Check::Snapshot { root } => pattern(format!(
                "^{}/[a-z0-9](?:[a-z0-9]|-(?!-)){{1,28}}[a-z0-9]@{}$",
                regex_escape(root),
                &SNAP_RE[1..SNAP_RE.len() - 1]
            )),
        }
    }
}

fn regex_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if r"\.+*?()|[]{}^$-#&~ ".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[derive(Debug, Clone)]
pub struct Param {
    pub name: &'static str,
    pub check: Check,
    /// gatekeeper keeps it out of its audit log and logs, and masks it in output.
    pub secret: bool,
    pub description: &'static str,
}

/// A path gatekeeper builds from the other parameters and resolves below `under`.
#[derive(Debug, Clone)]
pub struct Derived {
    pub name: &'static str,
    pub template: String,
    pub under: String,
}

#[derive(Debug, Clone)]
pub enum Run {
    /// `docker <argv>`; `stdin` is fixed text fed to the command.
    Docker {
        argv: Vec<String>,
        stdin: Option<&'static str>,
    },
    /// One operation of gatekeeper's file executor on `path`.
    File {
        op: &'static str,
        mode: Option<&'static str>,
    },
    /// A TrueNAS middleware call; `params` is the template, its "{name}"
    /// strings filled in by gatekeeper. `job`: wait for the job's result.
    Truenas {
        method: &'static str,
        params: Value,
        job: bool,
    },
}

#[derive(Debug, Clone)]
pub struct Tool {
    pub id: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub write: bool,
    pub idempotent: bool,
    pub run: Run,
    pub params: Vec<Param>,
    pub derived: Vec<Derived>,
    pub timeout_s: u32,
}

impl Tool {
    pub fn toolkit(&self) -> &'static str {
        match self.run {
            Run::Docker { .. } => "docker",
            Run::File { .. } => "file",
            Run::Truenas { .. } => "truenas",
        }
    }
}

fn param(name: &'static str, check: Check, description: &'static str) -> Param {
    Param {
        name,
        check,
        secret: false,
        description,
    }
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|a| (*a).to_owned()).collect()
}

fn one_of(v: &[&str]) -> Check {
    Check::OneOf(s(v))
}

/// Every tool castord calls on a node with this pool.
pub fn catalog(pool: &str) -> Vec<Tool> {
    let r = root(pool);
    let templates = format!("{r}/files/templates/tenant");
    let tenants = format!("{r}/tenants");
    let slug = || param("slug", Check::Slug, "The tenant's slug.");
    let tenant_files = || {
        vec![
            Derived {
                name: "volumes_file",
                template: format!("{tenants}/{{slug}}/compose.volumes.yml"),
                under: tenants.clone(),
            },
            Derived {
                name: "env_file",
                template: format!("{tenants}/{{slug}}/tenant.env"),
                under: tenants.clone(),
            },
        ]
    };
    // compose_args: the booked profiles come from COMPOSE_PROFILES in tenant.env,
    // stopping and removing name every profile.
    let compose = |all_profiles: bool, tail: &[&str]| {
        let mut v = s(&[
            "compose",
            "-p",
            "pc-t-{slug}",
            "-f",
            &format!("{templates}/compose.yml"),
            "--project-directory",
            &templates,
            "-f",
            "{volumes_file}",
            "--env-file",
            "{env_file}",
        ]);
        if all_profiles {
            for p in ALL_PROFILES {
                v.push("--profile".into());
                v.push(p.into());
            }
        }
        v.extend(s(tail));
        v
    };
    let docker = |argv: Vec<String>| Run::Docker { argv, stdin: None };
    let email = || param("email", Check::Email, "The person's e-mail address.");
    let scope = || param("scope", one_of(&["WEB", "DEVICES", "REMOVED"]), "What to end.");
    let web = || param("web", one_of(&["true", "false"]), "Browser sessions only.");
    let keep = || param("keep", Check::Keep, "API key name to keep, or empty.");
    let services: Vec<&str> = crate::render::STACK_SERVICES
        .iter()
        .flat_map(|s| s.def().containers.iter().copied())
        .chain(crate::render::ASSISTANT_CONTAINERS)
        .collect();
    let mut tools = vec![
        Tool {
            id: "docker.tenant_up",
            title: "Start a tenant's stack",
            description: "docker compose up -d for pc-t-<slug>, with the booked profiles from its tenant.env.",
            write: true,
            idempotent: true,
            run: docker(compose(false, &["up", "-d", "--remove-orphans"])),
            params: vec![slug()],
            derived: tenant_files(),
            timeout_s: 900,
        },
        Tool {
            id: "docker.tenant_recreate_assistant",
            title: "Restart a tenant's assistant",
            description: "Recreates assistant-init and assistant, so Hermes reads its rendered config again.",
            write: true,
            idempotent: true,
            run: docker(compose(
                false,
                &["up", "-d", "--force-recreate", "assistant-init", "assistant"],
            )),
            params: vec![slug()],
            derived: tenant_files(),
            timeout_s: 600,
        },
        Tool {
            id: "docker.tenant_stop",
            title: "Stop a tenant's stack",
            description: "docker compose stop for every service of pc-t-<slug>. Data stays.",
            write: true,
            idempotent: true,
            run: docker(compose(true, &["stop"])),
            params: vec![slug()],
            derived: tenant_files(),
            timeout_s: 600,
        },
        Tool {
            id: "docker.tenant_down",
            title: "Remove a tenant's stack",
            description: "docker compose down --volumes for pc-t-<slug> (tenant deletion; data is on the dataset).",
            write: true,
            idempotent: true,
            run: docker(compose(true, &["down", "--volumes", "--remove-orphans"])),
            params: vec![slug()],
            derived: tenant_files(),
            timeout_s: 600,
        },
        Tool {
            id: "docker.tenant_rm_service",
            title: "Remove an unbooked service",
            description: "Stops and removes one service's containers of pc-t-<slug>; its data stays.",
            write: true,
            idempotent: true,
            run: docker(compose(true, &["rm", "-s", "-f", "{service}"])),
            params: vec![slug(), param("service", one_of(&services), "The compose service.")],
            derived: tenant_files(),
            timeout_s: 300,
        },
        Tool {
            id: "docker.revoke_photos",
            title: "End a person's Immich sessions",
            description: "Deletes the person's Immich sessions (and API keys unless web) in the tenant's database.",
            write: true,
            idempotent: true,
            run: Run::Docker {
                argv: compose(
                    false,
                    &[
                        "exec",
                        "-T",
                        "immich-db",
                        "psql",
                        "-X",
                        "-q",
                        "-v",
                        "ON_ERROR_STOP=1",
                        "-U",
                        "immich",
                        "-d",
                        "immich",
                        "-v",
                        "email={email}",
                        "-v",
                        "before={before}",
                        "-v",
                        "web={web}",
                        "-v",
                        "keep={keep}",
                    ],
                ),
                stdin: Some(PHOTOS_SQL),
            },
            params: vec![
                slug(),
                email(),
                param("before", Check::Instant, "Sessions created before this end."),
                web(),
                keep(),
            ],
            derived: tenant_files(),
            timeout_s: 120,
        },
        Tool {
            id: "docker.revoke_docs",
            title: "End a person's Paperless sessions",
            description: "Sets an unusable password (and deletes API tokens unless WEB) for the person in Paperless.",
            write: true,
            idempotent: true,
            run: Run::Docker {
                argv: compose(
                    false,
                    &[
                        "exec",
                        "-T",
                        "-e",
                        "PC_EMAIL={email}",
                        "-e",
                        "PC_SCOPE={scope}",
                        "paperless",
                        "python3",
                        "manage.py",
                        "shell",
                        "--no-imports",
                    ],
                ),
                stdin: Some(DOCS_SCRIPT),
            },
            params: vec![slug(), email(), scope()],
            derived: tenant_files(),
            timeout_s: 120,
        },
        Tool {
            id: "docker.revoke_places",
            title: "End a person's Dawarich sessions",
            description: "Sets a random password (and API key unless WEB) for the person in Dawarich.",
            write: true,
            idempotent: true,
            run: docker(compose(
                false,
                &[
                    "exec",
                    "-T",
                    "-e",
                    "PC_EMAIL={email}",
                    "-e",
                    "PC_SCOPE={scope}",
                    "dawarich",
                    "bin/rails",
                    "runner",
                    PLACES_SCRIPT,
                ],
            )),
            params: vec![slug(), email(), scope()],
            derived: tenant_files(),
            timeout_s: 300,
        },
        Tool {
            id: "docker.revoke_flights",
            title: "End a person's AirTrail sessions",
            description: "Deletes the person's AirTrail sessions (and keys and grants unless web).",
            write: true,
            idempotent: true,
            run: Run::Docker {
                argv: compose(
                    false,
                    &[
                        "exec",
                        "-T",
                        "db",
                        "psql",
                        "-X",
                        "-q",
                        "-v",
                        "ON_ERROR_STOP=1",
                        "-U",
                        "postgres",
                        "-d",
                        "airtrail",
                        "-v",
                        "sub={sub}",
                        "-v",
                        "web={web}",
                        "-v",
                        "keep={keep}",
                    ],
                ),
                stdin: Some(FLIGHTS_SQL),
            },
            params: vec![
                slug(),
                param("sub", Check::Sub, "The person's OIDC subject."),
                web(),
                keep(),
            ],
            derived: tenant_files(),
            timeout_s: 120,
        },
        Tool {
            id: "docker.shared_up",
            title: "Start the node's shared services",
            description: "docker compose up -d for pc-node (ML, Tika, Gotenberg, LLM gateway).",
            write: true,
            idempotent: true,
            run: docker(s(&[
                "compose",
                "-f",
                &format!("{r}/files/node/compose.yml"),
                "--project-directory",
                &format!("{r}/files/node"),
                "--env-file",
                &format!("{r}/shared.env"),
                "up",
                "-d",
            ])),
            params: vec![],
            derived: vec![],
            timeout_s: 900,
        },
    ];
    let network = |id, title, description, argv: &[&str], params: Vec<Param>, idempotent| Tool {
        id,
        title,
        description,
        write: true,
        idempotent,
        run: docker(s(argv)),
        params,
        derived: vec![],
        timeout_s: 60,
    };
    let shared = || param("container", one_of(&SHARED), "A shared service's container.");
    tools.extend([
        network(
            "docker.network_connect_shared",
            "Attach a shared service to a tenant",
            "Connects a shared service to pc-t-<slug>-shared.",
            &["network", "connect", "pc-t-{slug}-shared", "{container}"],
            vec![slug(), shared()],
            false,
        ),
        network(
            "docker.network_disconnect_shared",
            "Detach a shared service from a tenant",
            "Disconnects a shared service from pc-t-<slug>-shared.",
            &["network", "disconnect", "-f", "pc-t-{slug}-shared", "{container}"],
            vec![slug(), shared()],
            true,
        ),
        network(
            "docker.network_connect_llm",
            "Attach the LLM gateway to a tenant",
            "Connects the LLM gateway to pc-t-<slug>-llm.",
            &["network", "connect", "pc-t-{slug}-llm", LLM_GATEWAY],
            vec![slug()],
            false,
        ),
        network(
            "docker.network_disconnect_llm",
            "Detach the LLM gateway from a tenant",
            "Disconnects the LLM gateway from pc-t-<slug>-llm.",
            &["network", "disconnect", "-f", "pc-t-{slug}-llm", LLM_GATEWAY],
            vec![slug()],
            true,
        ),
        network(
            "docker.network_rm_shared",
            "Remove a tenant's shared network",
            "Removes pc-t-<slug>-shared.",
            &["network", "rm", "pc-t-{slug}-shared"],
            vec![slug()],
            true,
        ),
        network(
            "docker.network_rm_llm",
            "Remove a tenant's LLM network",
            "Removes pc-t-<slug>-llm.",
            &["network", "rm", "pc-t-{slug}-llm"],
            vec![slug()],
            true,
        ),
        network(
            "docker.edge_network_create",
            "Create pc-edge",
            "Creates the network the control-plane Traefik reaches the tenants' skin proxies on.",
            &["network", "create", "pc-edge"],
            vec![],
            false,
        ),
    ]);
    tools.extend([
        Tool {
            id: "docker.edge_network_inspect",
            title: "Look at pc-edge",
            description: "docker network inspect pc-edge.",
            write: false,
            idempotent: true,
            run: docker(s(&["network", "inspect", "pc-edge"])),
            params: vec![],
            derived: vec![],
            timeout_s: 30,
        },
        Tool {
            id: "docker.version",
            title: "Docker version",
            description: "The Docker engine's version.",
            write: false,
            idempotent: true,
            run: docker(s(&["version", "--format", "{{.Server.Version}}"])),
            params: vec![],
            derived: vec![],
            timeout_s: 30,
        },
        Tool {
            id: "docker.image_pull",
            title: "Pull a signed image",
            description: "Pulls an image by digest; castord verified its signature first.",
            write: true,
            idempotent: true,
            run: docker(s(&["pull", "-q", "{image}"])),
            params: vec![param("image", Check::PinnedImage, "repo@sha256:<digest>.")],
            derived: vec![],
            timeout_s: 900,
        },
        Tool {
            id: "docker.image_tag",
            title: "Tag a pulled image",
            description: "Gives a pulled image the local name the stacks use.",
            write: true,
            idempotent: true,
            run: docker(s(&["tag", "{image}", "{tag}"])),
            params: vec![
                param("image", Check::PinnedImage, "repo@sha256:<digest>."),
                param("tag", one_of(&[crate::node::ASSISTANT_TOOLS_IMAGE]), "The local name."),
            ],
            derived: vec![],
            timeout_s: 60,
        },
    ]);
    tools.extend([
        Tool {
            id: "truenas.pool_query",
            title: "Castor's pool",
            description: "Health, size and allocation of the pool tenant datasets live on.",
            write: false,
            idempotent: true,
            run: Run::Truenas {
                method: "pool.query",
                params: serde_json::json!({"filters": [["name", "=", pool]]}),
                job: false,
            },
            params: vec![],
            derived: vec![],
            timeout_s: 30,
        },
        Tool {
            id: "truenas.system_info",
            title: "The NAS",
            description: "TrueNAS version, memory and cores.",
            write: false,
            idempotent: true,
            run: Run::Truenas {
                method: "system.info",
                params: serde_json::json!({}),
                job: false,
            },
            params: vec![],
            derived: vec![],
            timeout_s: 30,
        },
    ]);
    let datasets = dataset_root(pool);
    let dataset = |child: bool, description| {
        param(
            "dataset",
            Check::Dataset {
                root: datasets.clone(),
                child,
            },
            description,
        )
    };
    let passphrase = Param {
        name: "passphrase",
        check: Check::Passphrase,
        secret: true,
        description: "HKDF of the tenant's volume key; TrueNAS never stores it.",
    };
    let quota = param("quota", Check::Bytes, "The quota in bytes.");
    let truenas = |id, title, description, write, method, params: Value, params_list: Vec<Param>| Tool {
        id,
        title,
        description,
        write,
        idempotent: true,
        run: Run::Truenas {
            method,
            params,
            job: false,
        },
        params: params_list,
        derived: vec![],
        timeout_s: 120,
    };
    tools.extend([
        truenas(
            "truenas.dataset_query",
            "A tenant's dataset",
            "Whether it exists and is unlocked, its quota, and what it and each data directory use.",
            false,
            "pool.dataset.query",
            serde_json::json!({"filters": [["id", "=", "{dataset}"]]}),
            vec![dataset(false, "<pool>/castor/t/<slug>.")],
        ),
        truenas(
            "truenas.dataset_create",
            "Create a tenant's dataset",
            "An encryption root of its own (AES-256-GCM, passphrase), with the booked quota.",
            true,
            "pool.dataset.create",
            serde_json::json!({"data": {
                "name": "{dataset}",
                "quota": "{quota}",
                "encryption": true,
                "inherit_encryption": false,
                "encryption_options": {"algorithm": "AES-256-GCM", "passphrase": "{passphrase}"},
            }}),
            vec![
                dataset(false, "<pool>/castor/t/<slug>."),
                quota.clone(),
                passphrase.clone(),
            ],
        ),
        truenas(
            "truenas.dataset_create_dir",
            "Create a data directory",
            "A child dataset of an unlocked tenant dataset; it inherits the key and the mount.",
            true,
            "pool.dataset.create",
            serde_json::json!({"data": {"name": "{dataset}"}}),
            vec![dataset(true, "<pool>/castor/t/<slug>/<dir>.")],
        ),
        truenas(
            "truenas.dataset_unlock",
            "Unlock a tenant's dataset",
            "Unlocks and mounts it with its children. A job: castord confirms with dataset_query.",
            true,
            "pool.dataset.unlock",
            serde_json::json!({
                "id": "{dataset}",
                "options": {"recursive": true, "datasets": [{"name": "{dataset}", "passphrase": "{passphrase}"}]},
            }),
            vec![dataset(false, "<pool>/castor/t/<slug>."), passphrase.clone()],
        ),
        truenas(
            "truenas.dataset_lock",
            "Lock a tenant's dataset",
            "Unmounts and locks it; its stacks are stopped first. A job: castord confirms with dataset_query.",
            true,
            "pool.dataset.lock",
            serde_json::json!({"id": "{dataset}", "options": {"force_umount": false}}),
            vec![dataset(false, "<pool>/castor/t/<slug>.")],
        ),
        truenas(
            "truenas.dataset_quota",
            "Set a tenant's quota",
            "Online; TrueNAS refuses a quota below what is used, and castord checks first.",
            true,
            "pool.dataset.update",
            serde_json::json!({"id": "{dataset}", "data": {"quota": "{quota}"}}),
            vec![dataset(false, "<pool>/castor/t/<slug>."), quota],
        ),
        truenas(
            "truenas.dataset_delete",
            "Destroy a tenant's dataset",
            "With its data directories and snapshots. Irreversible; castord then deletes the key.",
            true,
            "pool.dataset.delete",
            serde_json::json!({"id": "{dataset}", "options": {"recursive": true, "force": true}}),
            vec![dataset(false, "<pool>/castor/t/<slug>.")],
        ),
        truenas(
            "truenas.snapshot_create",
            "Snapshot a tenant for backup",
            "A recursive snapshot cb-<UTC time> of the tenant's dataset, for the replication task to send.",
            true,
            "zfs.snapshot.create",
            serde_json::json!({"data": {"dataset": "{dataset}", "name": "{snapshot}", "recursive": true}}),
            vec![
                dataset(false, "<pool>/castor/t/<slug>."),
                param("snapshot", Check::SnapName, "cb-<UTC time>."),
            ],
        ),
        truenas(
            "truenas.snapshot_query",
            "A tenant's backup snapshots",
            "The snapshots of the tenant's dataset.",
            false,
            "zfs.snapshot.query",
            serde_json::json!({"filters": [["dataset", "=", "{dataset}"]]}),
            vec![dataset(false, "<pool>/castor/t/<slug>.")],
        ),
        truenas(
            "truenas.snapshot_delete",
            "Drop a backup snapshot",
            "Only cb-* snapshots of tenant datasets; what the backup box keeps is its replication task's retention.",
            true,
            "zfs.snapshot.delete",
            serde_json::json!({"id": "{snapshot}", "options": {"recursive": true}}),
            vec![param(
                "snapshot",
                Check::Snapshot { root: datasets.clone() },
                "<pool>/castor/t/<slug>@cb-<UTC time>.",
            )],
        ),
        truenas(
            "truenas.replication_run",
            "Send the backups",
            "Runs the replication task to the backup box that the operator set up (raw, encrypted).",
            true,
            "replication.run",
            serde_json::json!({"id": REPLICATION_TASK}),
            vec![],
        ),
        truenas(
            "truenas.replication_query",
            "How the backups went",
            "The replication task's state: running, finished or failed, and when.",
            false,
            "replication.query",
            serde_json::json!({"filters": [["id", "=", REPLICATION_TASK]]}),
            vec![],
        ),
    ]);
    let content = |description| Param {
        name: "content",
        check: Check::Content,
        secret: true,
        description,
    };
    let path = |first, rest, description| {
        param(
            "path",
            Check::Path {
                root: tenants.clone(),
                first,
                rest,
            },
            description,
        )
    };
    let files_path = |rest| {
        param(
            "path",
            Check::Path {
                root: format!("{r}/files"),
                first: FirstSegment::FileSet,
                rest,
            },
            "Below <root>/files.",
        )
    };
    tools.extend([
        Tool {
            id: "file.tenant_write",
            title: "Write a tenant file (owner only)",
            description: "Writes a file below tenants/<slug> with mode 0600 (tenant.env, immich.json, ...).",
            write: true,
            idempotent: true,
            run: Run::File {
                op: "write",
                mode: Some("0600"),
            },
            params: vec![
                path(FirstSegment::Slug, Rest::Required, "tenants/<slug>/<file>."),
                content("The file's content."),
            ],
            derived: vec![],
            timeout_s: 30,
        },
        Tool {
            id: "file.tenant_write_readable",
            title: "Write a tenant file (readable)",
            description: "Writes a file below tenants/<slug> with mode 0644, for unprivileged containers.",
            write: true,
            idempotent: true,
            run: Run::File {
                op: "write",
                mode: Some("0644"),
            },
            params: vec![
                path(FirstSegment::Slug, Rest::Required, "tenants/<slug>/<file>."),
                content("The file's content."),
            ],
            derived: vec![],
            timeout_s: 30,
        },
        Tool {
            id: "file.tenant_delete",
            title: "Remove tenant files",
            description: "Removes tenants/<slug> or a directory below it.",
            write: true,
            idempotent: true,
            run: Run::File {
                op: "delete",
                mode: None,
            },
            params: vec![path(FirstSegment::Slug, Rest::Optional, "tenants/<slug>[/<dir>].")],
            derived: vec![],
            timeout_s: 60,
        },
        Tool {
            id: "file.node_write",
            title: "Write a node file",
            description: "Writes one file of the templates, shared services, brand assets or CA bundle (0644).",
            write: true,
            idempotent: true,
            run: Run::File {
                op: "write",
                mode: Some("0644"),
            },
            params: vec![files_path(Rest::Required), content("The file's content.")],
            derived: vec![],
            timeout_s: 30,
        },
        Tool {
            id: "file.node_delete",
            title: "Remove a node file set",
            description: "Removes files/<templates|node|brand|ca> before castord writes it again.",
            write: true,
            idempotent: true,
            run: Run::File {
                op: "delete",
                mode: None,
            },
            params: vec![files_path(Rest::None)],
            derived: vec![],
            timeout_s: 60,
        },
        Tool {
            id: "file.shared_env_write",
            title: "Write the shared services' settings",
            description: "Writes <root>/shared.env (0600): versions and the LLM gateway's keys.",
            write: true,
            idempotent: true,
            run: Run::File {
                op: "write",
                mode: Some("0600"),
            },
            params: vec![
                param("path", one_of(&[&format!("{r}/shared.env")]), "<root>/shared.env."),
                content("The env file."),
            ],
            derived: vec![],
            timeout_s: 30,
        },
    ]);
    tools
}

fn find<'a>(tools: &'a [Tool], id: &str) -> Result<&'a Tool, String> {
    tools
        .iter()
        .find(|t| t.id == id)
        .ok_or_else(|| format!("no gatekeeper tool {id}"))
}

/// A call of tool `id`, checked the way gatekeeper will check it: every
/// parameter given, nothing more, each value passing its check, the secret
/// only where the tool has one (and then its value).
pub fn checked(
    tools: &[Tool],
    id: &str,
    args: Map<String, Value>,
    secret: Option<Zeroizing<String>>,
) -> Result<ToolCall, String> {
    let t = tools
        .iter()
        .find(|t| t.id == id)
        .ok_or_else(|| format!("no tool {id}"))?;
    let refuse = |what: &str| format!("{id}: {what}");
    let mut secret_param = None;
    for p in &t.params {
        if p.secret {
            secret_param = Some(p);
            continue;
        }
        match args.get(p.name) {
            Some(v) if p.check.ok_value(v) => {}
            Some(_) => return Err(refuse(&format!("{} not allowed", p.name))),
            None => return Err(refuse(&format!("{} missing", p.name))),
        }
    }
    if args.keys().any(|k| !t.params.iter().any(|p| !p.secret && p.name == k)) {
        return Err(refuse("unknown parameter"));
    }
    let secret = match (secret_param, secret) {
        (Some(p), Some(s)) if p.check.ok(&s) => Some((p.name, s)),
        (None, None) => None,
        _ => return Err(refuse("secret does not fit the tool")),
    };
    Ok(ToolCall {
        name: t.id.to_owned(),
        args,
        secret,
    })
}

/// A file tool's call, its parameters checked as gatekeeper will check them.
pub fn file_call(tools: &[Tool], id: &str, path: &str, content: Option<Zeroizing<String>>) -> Result<ToolCall, String> {
    let tool = find(tools, id)?;
    let Run::File { .. } = tool.run else {
        return Err(format!("{id} is not a file tool"));
    };
    let path_param = tool.params.iter().find(|p| p.name == "path").ok_or("no path")?;
    if !path_param.check.ok(path) {
        return Err(format!("{id}: path not allowed: {path}"));
    }
    let mut args = Map::new();
    args.insert("path".into(), Value::from(path));
    let secret = match (tool.params.iter().any(|p| p.name == "content"), content) {
        (true, Some(c)) => Some(("content", c)),
        (false, None) => None,
        _ => return Err(format!("{id}: content does not fit the tool")),
    };
    Ok(ToolCall {
        name: tool.id.to_owned(),
        args,
        secret,
    })
}

/// A tool without parameters.
pub fn plain_call(tools: &[Tool], id: &str) -> Result<ToolCall, String> {
    let tool = find(tools, id)?;
    if !tool.params.is_empty() {
        return Err(format!("{id} takes parameters"));
    }
    Ok(ToolCall {
        name: tool.id.to_owned(),
        args: Map::new(),
        secret: None,
    })
}

/// The tool whose argv template is exactly this docker command line, with its
/// parameters. Commands with extra environment, or stdin that isn't the
/// tool's own, have none.
pub fn docker_call(tools: &[Tool], cmd: &Cmd) -> Result<ToolCall, String> {
    let refuse = || format!("not available through gatekeeper: {}", cmd.label());
    if cmd.program != "docker" || !cmd.env.is_empty() {
        return Err(refuse());
    }
    let input = cmd.input.as_ref().filter(|i| !i.is_empty());
    for tool in tools {
        let Run::Docker { argv, stdin } = &tool.run else {
            continue;
        };
        if argv.len() != cmd.args.len() || input.map(|i| i.as_slice()) != stdin.map(str::as_bytes) {
            continue;
        }
        if let Some(args) = match_argv(tool, argv, &cmd.args) {
            return Ok(ToolCall {
                name: tool.id.to_owned(),
                args,
                secret: None,
            });
        }
    }
    Err(refuse())
}

/// `{name}` in an element: (prefix, name, suffix).
fn placeholder(element: &str) -> Option<(&str, &str, &str)> {
    let start = element.find('{')?;
    let end = start + element[start..].find('}')?;
    let name = &element[start + 1..end];
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return None;
    }
    Some((&element[..start], name, &element[end + 1..]))
}

fn match_argv(tool: &Tool, argv: &[String], actual: &[String]) -> Option<Map<String, Value>> {
    let mut values: Map<String, Value> = Map::new();
    let mut derived_seen: Vec<(&str, &str)> = Vec::new();
    for (template, arg) in argv.iter().zip(actual) {
        match placeholder(template) {
            None => {
                if template != arg {
                    return None;
                }
            }
            Some((prefix, name, suffix)) => {
                let value = arg.strip_prefix(prefix)?.strip_suffix(suffix)?;
                if tool.derived.iter().any(|d| d.name == name) {
                    derived_seen.push((name, value));
                    continue;
                }
                let p = tool.params.iter().find(|p| p.name == name)?;
                if !p.check.ok(value) {
                    return None;
                }
                match values.get(name) {
                    Some(v) if v.as_str() != Some(value) => return None,
                    _ => {
                        values.insert(name.to_owned(), Value::from(value));
                    }
                }
            }
        }
    }
    for (name, value) in derived_seen {
        let d = tool.derived.iter().find(|d| d.name == name)?;
        if substitute(&d.template, &values)? != value {
            return None;
        }
    }
    if tool.params.iter().any(|p| !values.contains_key(p.name)) {
        return None;
    }
    Some(values)
}

fn substitute(template: &str, values: &Map<String, Value>) -> Option<String> {
    let mut out = String::new();
    let mut rest = template;
    while let Some((prefix, name, _)) = placeholder(rest) {
        out.push_str(prefix);
        out.push_str(values.get(name)?.as_str()?);
        rest = &rest[prefix.len() + name.len() + 2..];
    }
    out.push_str(rest);
    Some(out)
}

/// gatekeeper's tools.yaml for the castor instance on this pool. Every
/// scalar is a JSON string, which YAML reads as a double-quoted string.
/// `replication_task`: the id of the replication task to the backup box;
/// without it the backup tools are left out.
pub fn tools_yaml(pool: &str, replication_task: Option<u64>) -> Result<String, String> {
    if !valid::pool(pool) {
        return Err(format!("invalid pool name: {pool}"));
    }
    let q = |v: &str| Value::from(v).to_string();
    let mut out = vec![
        "# Generated by `castor-admin gatekeeper tools <pool>` (castord's gatekeeper/mod.rs).".to_owned(),
        "# The node role calls exactly these tools; do not edit by hand. docs/gatekeeper.md".to_owned(),
        "tools:".to_owned(),
    ];
    for t in catalog(pool) {
        let mut t = t;
        if let Run::Truenas { params, .. } = &mut t.run {
            let text = params.to_string();
            if text.contains(REPLICATION_TASK) {
                let Some(task) = replication_task else { continue };
                *params = serde_json::from_str(&text.replace(&format!("\"{REPLICATION_TASK}\""), &task.to_string()))
                    .map_err(|e| e.to_string())?;
            }
        }
        out.push(format!("  - id: {}", q(t.id)));
        out.push(format!("    toolkit: {}", q(t.toolkit())));
        out.push("    version: 1".into());
        out.push(format!("    title: {}", q(t.title)));
        out.push(format!("    description: {}", q(t.description)));
        out.push(format!("    category: {}", q(if t.write { "write" } else { "read" })));
        out.push(format!("    idempotent: {}", t.idempotent));
        out.push("    enabled: true".into());
        match &t.run {
            Run::Docker { argv, stdin } => {
                out.push(format!("    binary: {}", q("/usr/bin/docker")));
                out.push("    argv:".into());
                for a in argv {
                    out.push(format!("      - {}", q(a)));
                }
                if let Some(text) = stdin {
                    out.push(format!("    stdin: {}", q(text)));
                }
            }
            Run::File { op, mode } => {
                out.push(format!("    executor: {}", q("file")));
                out.push(format!("    file_operation: {}", q(op)));
                if let Some(m) = mode {
                    out.push(format!("    file_mode: {}", q(m)));
                }
            }
            Run::Truenas { method, params, job } => {
                out.push(format!("    method: {}", q(method)));
                match params.as_object() {
                    Some(m) if !m.is_empty() => {
                        out.push("    params:".into());
                        for (k, v) in m {
                            out.push(format!("      {k}: {v}"));
                        }
                    }
                    _ => out.push("    params: {}".into()),
                }
                if *job {
                    out.push("    job: true".into());
                }
            }
        }
        if t.params.is_empty() && t.derived.is_empty() {
            out.push("    parameters: {}".into());
        } else {
            out.push("    parameters:".into());
        }
        for p in &t.params {
            out.push(format!("      {}:", p.name));
            for (k, v) in p.check.spec() {
                out.push(format!("        {k}: {v}"));
            }
            out.push("        required: true".into());
            if p.secret {
                out.push("        secret: true".into());
            }
            out.push(format!("        description: {}", q(p.description)));
        }
        for d in &t.derived {
            out.push(format!("      {}:", d.name));
            out.push(format!("        type: {}", q("path")));
            out.push(format!("        derived: {}", q(&d.template)));
            out.push(format!("        must_resolve_under: {}", q(&d.under)));
            out.push(format!(
                "        description: {}",
                q("Built by gatekeeper from the slug.")
            ));
        }
        out.push("    required_scopes: []".into());
        out.push(format!("    timeout_seconds: {}", t.timeout_s));
        out.push(format!("    max_output_bytes: {}", 1024 * 1024));
    }
    Ok(out.join("\n") + "\n")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn cmd(args: &[&str]) -> Cmd {
        Cmd::new("docker", s(args))
    }

    #[test]
    fn the_example_tools_yaml_is_current() {
        // Regenerate: castor-admin gatekeeper tools tank 1 > infra/gatekeeper/tools.tank.yaml
        assert_eq!(
            tools_yaml("tank", Some(1)).unwrap(),
            include_str!("../../../../../infra/gatekeeper/tools.tank.yaml")
        );
    }

    #[test]
    fn matches_only_exact_command_lines() {
        let tools = catalog("tank");
        let c = docker_call(
            &tools,
            &cmd(&["network", "connect", "pc-t-anna-shared", "pc-node-tika"]),
        )
        .unwrap();
        assert_eq!(c.name, "docker.network_connect_shared");
        assert_eq!(
            Value::Object(c.args).to_string(),
            r#"{"slug":"anna","container":"pc-node-tika"}"#
        );
        for bad in [
            &["network", "connect", "pc-t-anna-shared", "pc-node-llm-gateway"][..],
            &["network", "connect", "pc-t-An-shared", "pc-node-tika"],
            &["network", "connect", "pc-t-anna-shared", "pc-node-tika", "x"],
            &["network", "connect", "--alias=x", "pc-t-anna-shared", "pc-node-tika"],
            &["run", "--privileged", "alpine"],
            &["pull", "-q", "ghcr.io/x/y:latest"],
        ] {
            assert!(docker_call(&tools, &cmd(bad)).is_err(), "{bad:?}");
        }
        let mut env = cmd(&["network", "create", "pc-edge"]);
        env.env.push(("DOCKER_CONFIG", "/tmp/x".into()));
        assert!(docker_call(&tools, &env).is_err());
        let mut fed = cmd(&["network", "create", "pc-edge"]);
        fed.input = Some(Zeroizing::new(b"x".to_vec()));
        assert!(docker_call(&tools, &fed).is_err());
    }

    #[test]
    fn derived_paths_must_be_the_tenants_own() {
        let tools = catalog("tank");
        let r = "/mnt/tank/castor/node";
        let line = |slug: &str, other: &str| {
            s(&[
                "compose",
                "-p",
                &format!("pc-t-{slug}"),
                "-f",
                &format!("{r}/files/templates/tenant/compose.yml"),
                "--project-directory",
                &format!("{r}/files/templates/tenant"),
                "-f",
                &format!("{r}/tenants/{other}/compose.volumes.yml"),
                "--env-file",
                &format!("{r}/tenants/{slug}/tenant.env"),
                "up",
                "-d",
                "--remove-orphans",
            ])
        };
        let c = docker_call(&tools, &Cmd::new("docker", line("anna", "anna"))).unwrap();
        assert_eq!(c.name, "docker.tenant_up");
        assert!(docker_call(&tools, &Cmd::new("docker", line("anna", "bert"))).is_err());
    }

    #[test]
    fn no_command_line_fits_two_tools() {
        // Every docker template filled with sample values matches its own tool only.
        let tools = catalog("tank");
        let sample = |p: &Param| match &p.check {
            Check::Slug => "anna".to_owned(),
            Check::OneOf(v) => v[0].clone(),
            Check::Email => "a@b.ch".into(),
            Check::Instant => "2026-10-18T19:38:04.000Z".into(),
            Check::Sub => "abc".into(),
            Check::Keep => String::new(),
            Check::PinnedImage => format!("ghcr.io/a/b@sha256:{}", "0".repeat(64)),
            _ => "x".into(),
        };
        for t in &tools {
            let Run::Docker { argv, stdin } = &t.run else { continue };
            let mut values = Map::new();
            for p in &t.params {
                values.insert(p.name.into(), Value::from(sample(p)));
            }
            for d in &t.derived {
                let v = substitute(&d.template, &values).unwrap();
                values.insert(d.name.into(), Value::from(v));
            }
            let mut c = Cmd::new("docker", argv.iter().map(|a| substitute(a, &values).unwrap()).collect());
            c.input = stdin.map(|t| Zeroizing::new(t.as_bytes().to_vec()));
            assert_eq!(docker_call(&tools, &c).unwrap().name, t.id);
        }
    }

    #[test]
    fn checks() {
        let r = "/mnt/tank/castor/node/tenants".to_owned();
        let p = |first, rest| Check::Path {
            root: r.clone(),
            first,
            rest,
        };
        let file = p(FirstSegment::Slug, Rest::Required);
        assert!(file.ok(&format!("{r}/anna/tenant.env")) && file.ok(&format!("{r}/anna/assistant/x.yaml")));
        for bad in ["anna", "anna/../bert/x", "anna/.env", "anna//x", "an--na/x", "anna/x y"] {
            assert!(!file.ok(&format!("{r}/{bad}")), "{bad}");
        }
        assert!(!file.ok("/mnt/tank/castor/node/tenantsx/anna/a"));
        let dir = p(FirstSegment::Slug, Rest::Optional);
        assert!(dir.ok(&format!("{r}/anna")) && dir.ok(&format!("{r}/anna/assistant/hermes")));
        assert!(Check::Instant.ok("2026-10-18T19:38:04.000Z") && !Check::Instant.ok("2026-10-18 19:38:04"));
        assert!(Check::Keep.ok("") && Check::Keep.ok(ASSISTANT_KEY_NAME) && !Check::Keep.ok("x"));
        let digest = "a".repeat(64);
        assert!(Check::PinnedImage.ok(&format!("ghcr.io/davidsteg/castor-x@sha256:{digest}")));
        assert!(!Check::PinnedImage.ok(&format!("castor@sha256:{digest}")));
        assert!(!Check::PinnedImage.ok("ghcr.io/a/b:1"));
        assert!(!Check::PinnedImage.ok(&format!("-x/b@sha256:{digest}")));
        assert!(!Check::Email.ok("a b@c") && Check::Email.ok("anna@example.ch"));
    }

    #[test]
    fn yaml_is_valid_json_scalars() {
        let y = tools_yaml("tank", None).unwrap();
        assert!(!y.contains("replication_run"));
        assert!(tools_yaml("tank", Some(4)).unwrap().contains("      id: 4"));
        assert!(y.contains(r#"  - id: "docker.tenant_up""#));
        assert!(y.contains(r#"        secret: true"#));
        assert!(tools_yaml("tank/x", None).is_err());
    }
}
