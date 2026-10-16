//! A checked Spec as the compose file Docker runs, plus the Traefik routes in
//! front of it (packages/stacks/src/render.ts). Every security setting is
//! decided here, not by the customer: images pinned by digest, capabilities
//! dropped, limits on every container, volumes only inside the stack's own
//! dataset, internal networks, egress only through the proxy, gVisor when the
//! node has it. Customer strings are escaped for Compose ($ -> $$).

use serde_json::{Map, Value, json};

use super::image;
use super::js;
use super::spec::{Args, Spec, secret_placeholders};

pub const KEPT_CAPABILITIES: [&str; 7] = [
    "CHOWN",
    "DAC_OVERRIDE",
    "FOWNER",
    "SETGID",
    "SETUID",
    "NET_BIND_SERVICE",
    "KILL",
];
pub const PIDS_LIMIT: u32 = 512;
pub const EGRESS_SERVICE: &str = "castor-egress";
pub const EGRESS_PORT: u16 = 3128;
pub const TMPFS_MB: u32 = 64;
pub const DEFAULT_TMPFS: [&str; 3] = ["/tmp", "/run", "/var/tmp"];

/// Names on the node. "--" separates slug and stack: slugs never contain it
/// and stack names have no hyphen, so no tenant can produce another
/// tenant's project, network or router name.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Names {
    pub project: String,
    pub app_network: String,
    pub edge_network: String,
    pub out_network: String,
    /// The web service on the edge network; a dot, which no service name has.
    pub web_alias: String,
    pub auth_slug: String,
}

pub fn names(slug: &str, stack: &str) -> Names {
    let project = format!("pc-s-{slug}--{stack}");
    Names {
        app_network: project.clone(),
        edge_network: format!("{project}-edge"),
        out_network: format!("{project}-out"),
        web_alias: format!("web.{project}"),
        auth_slug: format!("stack-{slug}--{stack}"),
        project,
    }
}

/// pc-s-<slug>--<stack>-edge (the networks prepare reconnects the proxy to).
pub fn is_edge_network(n: &str) -> bool {
    let Some(rest) = n.strip_prefix("pc-s-").and_then(|r| r.strip_suffix("-edge")) else {
        return false;
    };
    let Some((slug, stack)) = rest.split_once("--") else {
        return false;
    };
    let alnum = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    alnum(stack) && slug.split('-').all(alnum)
}

pub fn host(slug: &str, stack: &str, stacks_domain: &str) -> String {
    format!("{stack}-{slug}.{stacks_domain}")
}

pub struct Context<'a> {
    pub slug: &'a str,
    pub stack: &'a str,
    /// https://<stack>-<slug>.<stacks domain>
    pub origin: &'a str,
    /// Where the volumes live on the node: <data_dir>/<volume>.
    pub data_dir: &'a str,
    /// Service -> image pinned to a digest.
    pub images: &'a [(String, String)],
    /// Values of ${secret:name}.
    pub secrets: &'a [(String, String)],
    pub runtime: Option<&'a str>,
    pub egress_image: &'a str,
}

fn esc(s: &str) -> String {
    s.replace('$', "$$")
}

/// new URL(origin).host
fn origin_host(origin: &str) -> String {
    let (scheme, rest) = origin.split_once("://").unwrap_or(("", origin));
    let h = rest.split(['/', '?', '#']).next().unwrap_or("").to_lowercase();
    let default = if scheme == "https" { ":443" } else { ":80" };
    h.strip_suffix(default).map(str::to_owned).unwrap_or(h)
}

fn fill(value: &str, ctx: &Context) -> Result<String, String> {
    let mut out = String::new();
    let mut last = 0;
    for (start, end, name) in secret_placeholders(value) {
        let v = ctx
            .secrets
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| format!("secret {name} missing"))?;
        out.push_str(&value[last..start]);
        out.push_str(v);
        last = end;
    }
    out.push_str(&value[last..]);
    // Then ${castor:origin} / ${castor:host}, as the second replace does.
    let host = origin_host(ctx.origin);
    Ok(replace_castor(&out, ctx.origin, &host))
}

fn replace_castor(s: &str, origin: &str, host: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("${castor:") {
        out.push_str(&rest[..i]);
        let after = &rest[i + "${castor:".len()..];
        if let Some(r) = after.strip_prefix("origin}") {
            out.push_str(origin);
            rest = r;
        } else if let Some(r) = after.strip_prefix("host}") {
            out.push_str(host);
            rest = r;
        } else {
            out.push_str("${castor:");
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

fn literal(a: &Args) -> Value {
    match a {
        Args::One(s) => Value::String(esc(s)),
        Args::Many(v) => v.iter().map(|s| Value::String(esc(s))).collect(),
    }
}

fn hardening(o: &mut Map<String, Value>, memory_mb: f64, cpus: f64, runtime: Option<&str>) {
    let mem = format!("{}m", js::number_string(memory_mb));
    o.insert("cap_drop".into(), json!(["ALL"]));
    o.insert("cap_add".into(), json!(KEPT_CAPABILITIES));
    o.insert("security_opt".into(), json!(["no-new-privileges:true"]));
    o.insert("mem_limit".into(), json!(mem));
    o.insert("memswap_limit".into(), json!(mem));
    o.insert("cpus".into(), js::num(cpus));
    o.insert("pids_limit".into(), json!(PIDS_LIMIT));
    o.insert("restart".into(), json!("unless-stopped"));
    o.insert(
        "logging".into(),
        json!({ "driver": "json-file", "options": { "max-size": "10m", "max-file": "3" } }),
    );
    if let Some(r) = runtime {
        o.insert("runtime".into(), json!(r));
    }
}

/// The compose file for one stack (written as JSON, which Compose reads).
pub fn compose(spec: &Spec, ctx: &Context) -> Result<Value, String> {
    let n = names(ctx.slug, ctx.stack);
    let with_egress = !spec.egress.is_empty();
    let proxy = format!("http://{EGRESS_SERVICE}:{EGRESS_PORT}");
    let label = format!("{}/{}", ctx.slug, ctx.stack);
    let mut services = Map::new();

    for (name, svc) in &spec.services {
        let img = ctx
            .images
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
            .filter(|i| image::is_pinned(i))
            .ok_or_else(|| format!("{name}: image not pinned to a digest"))?;
        let mut environment = Map::new();
        for (k, v) in &svc.environment {
            environment.insert(k.clone(), Value::String(esc(&fill(v, ctx)?)));
        }
        if with_egress {
            let mut no_proxy = vec!["localhost", "127.0.0.1"];
            no_proxy.extend(spec.services.iter().map(|(s, _)| s.as_str()));
            let no_proxy = no_proxy.join(",");
            for k in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
                environment.insert(k.into(), json!(proxy));
            }
            environment.insert("NO_PROXY".into(), json!(no_proxy));
            environment.insert("no_proxy".into(), json!(no_proxy));
        }
        let mut tmpfs: Vec<&str> = Vec::new();
        for p in DEFAULT_TMPFS
            .iter()
            .copied()
            .chain(svc.tmpfs.iter().map(String::as_str))
        {
            if !tmpfs.contains(&p) {
                tmpfs.push(p);
            }
        }
        let mut o = Map::new();
        o.insert("image".into(), json!(img));
        if let Some(c) = &svc.command {
            o.insert("command".into(), literal(c));
        }
        if let Some(e) = &svc.entrypoint {
            o.insert("entrypoint".into(), literal(e));
        }
        o.insert("environment".into(), Value::Object(environment));
        if let Some(w) = svc.working_dir.as_deref().filter(|w| !w.is_empty()) {
            o.insert("working_dir".into(), json!(esc(w)));
        }
        if let Some(u) = svc.user.as_deref().filter(|u| !u.is_empty()) {
            o.insert("user".into(), json!(u));
        }
        // Always: the writable layer is outside every quota. Writes go to the
        // stack's volumes (its dataset) or to tmpfs, which counts as memory.
        o.insert("read_only".into(), json!(true));
        o.insert(
            "tmpfs".into(),
            tmpfs.iter().map(|p| json!(format!("{p}:size={TMPFS_MB}m"))).collect(),
        );
        o.insert(
            "volumes".into(),
            svc.volumes
                .iter()
                .map(|m| {
                    json!({
                        "type": "bind",
                        "source": format!("{}/{}", ctx.data_dir, m.volume),
                        "target": m.target,
                        "read_only": m.read_only,
                        // castor-zfs creates the directory in the dataset; Docker never creates one elsewhere.
                        "bind": { "create_host_path": false },
                    })
                })
                .collect(),
        );
        hardening(&mut o, svc.memory_mb, svc.cpus, ctx.runtime);
        if let Some(h) = &svc.healthcheck {
            o.insert(
                "healthcheck".into(),
                json!({
                    "test": h.test.iter().map(|t| esc(t)).collect::<Vec<_>>(),
                    "interval": format!("{}s", js::number_string(h.interval_s)),
                    "timeout": format!("{}s", js::number_string(h.timeout_s)),
                    "retries": js::num(h.retries),
                    "start_period": format!("{}s", js::number_string(h.start_period_s)),
                }),
            );
        }
        if !svc.depends_on.is_empty() {
            o.insert("depends_on".into(), json!(svc.depends_on));
        }
        let mut networks = Map::new();
        networks.insert("app".into(), json!({}));
        if spec.web.service == *name {
            networks.insert("edge".into(), json!({ "aliases": [n.web_alias] }));
        }
        o.insert("networks".into(), Value::Object(networks));
        o.insert("labels".into(), json!({ "ch.castorcloud.stack": label }));
        services.insert(name.clone(), Value::Object(o));
    }

    if with_egress {
        let mut o = Map::new();
        o.insert("image".into(), json!(ctx.egress_image));
        o.insert(
            "environment".into(),
            json!({ "EGRESS_ALLOW": spec.egress.join(","), "EGRESS_PORT": EGRESS_PORT.to_string() }),
        );
        o.insert("user".into(), json!("65534:65534"));
        o.insert("read_only".into(), json!(true));
        hardening(&mut o, 96.0, 0.25, ctx.runtime);
        o.insert("cap_add".into(), json!([]));
        o.insert("networks".into(), json!({ "app": {}, "out": {} }));
        o.insert("labels".into(), json!({ "ch.castorcloud.stack": label }));
        services.insert(EGRESS_SERVICE.into(), Value::Object(o));
    }

    let mut networks = Map::new();
    networks.insert("app".into(), json!({ "name": n.app_network, "internal": true }));
    networks.insert("edge".into(), json!({ "name": n.edge_network, "internal": true }));
    if with_egress {
        networks.insert("out".into(), json!({ "name": n.out_network }));
    }
    Ok(json!({ "name": n.project, "services": services, "networks": networks }))
}

/// Traefik route on the stacks node: control plane -> the stack's web service.
pub fn node_route(slug: &str, stack: &str, host: &str, port: f64) -> String {
    let n = names(slug, stack);
    let p = &n.project;
    [
        format!("# Generated by the provisioner: stack {stack} of {slug}."),
        "http:".into(),
        "  routers:".into(),
        format!("    {p}:"),
        format!("      rule: 'Host(`{host}`)'"),
        "      entryPoints: [web]".into(),
        // Only the control plane may call it: it did the sign-in check.
        "      middlewares: [castor-control-plane]".into(),
        format!("      service: {p}"),
        "  services:".into(),
        format!("    {p}:"),
        "      loadBalancer:".into(),
        "        passHostHeader: true".into(),
        "        servers:".into(),
        format!(
            "          - url: \"http://{}:{}\"",
            n.web_alias,
            js::number_string(port)
        ),
        String::new(),
    ]
    .join("\n")
}

/// The node's allowlist middleware (written once per node, see stacks-edge.yml).
pub fn node_middlewares(control_plane_ips: &[String]) -> Result<String, String> {
    if control_plane_ips.is_empty() {
        return Err("no control-plane address for the stack node".into());
    }
    let ips: Vec<String> = control_plane_ips.iter().map(|ip| format!("\"{ip}\"")).collect();
    Ok([
        "# Generated by the provisioner: requests must come from the control plane.".to_owned(),
        "http:".into(),
        "  middlewares:".into(),
        "    castor-control-plane:".into(),
        "      ipAllowList:".into(),
        format!("        sourceRange: [{}]", ips.join(", ")),
        String::new(),
    ]
    .join("\n"))
}

/// Control-plane routes for a tenant's stacks: TLS, Authentik's forward
/// auth (only the tenant's members get through), then the stacks node.
pub fn control_plane_routes(
    slug: &str,
    stacks: &[(String, String)],
    edge_url: &str,
    cert_resolver: Option<&str>,
    stacks_domain: &str,
) -> String {
    let tls: Vec<String> = match cert_resolver.filter(|r| !r.is_empty()) {
        Some(r) => vec![
            "      tls:".into(),
            format!("        certResolver: {r}"),
            "        domains:".into(),
            format!("          - main: '{stacks_domain}'"),
            format!("            sans: ['*.{stacks_domain}']"),
        ],
        None => Vec::new(),
    };
    let service = format!("pc-s-{slug}--node");
    let mut lines = vec![
        format!("# Generated by the provisioner: {slug}'s custom stacks, on the node at {edge_url}."),
        "http:".into(),
        "  routers:".into(),
    ];
    for (stack, host) in stacks {
        let p = names(slug, stack).project;
        lines.push(format!("    {p}:"));
        lines.push(format!("      rule: 'Host(`{host}`)'"));
        lines.push("      entryPoints: [websecure]".into());
        lines.push("      middlewares: [castor-stack-strip@file, castor-stack-auth@file]".into());
        lines.push(format!("      service: {service}"));
        lines.extend(tls.iter().cloned());
        lines.push(format!("    {p}-outpost:"));
        lines.push(format!(
            "      rule: 'Host(`{host}`) && PathPrefix(`/outpost.goauthentik.io/`)'"
        ));
        lines.push("      entryPoints: [websecure]".into());
        lines.push("      priority: 10000".into());
        lines.push("      service: authentik@docker".into());
        lines.extend(tls.iter().cloned());
    }
    lines.extend([
        "  services:".into(),
        format!("    {service}:"),
        "      loadBalancer:".into(),
        "        passHostHeader: true".into(),
        "        servers:".into(),
        format!("          - url: \"{edge_url}\""),
        String::new(),
    ]);
    lines.join("\n")
}
