//! What a tenant stack consists of and how its config files are rendered,
//! as `apps/provisioner/src/tenant.ts`. The outputs are checked against the
//! TypeScript ones (tests/fixtures/render.json, made by gen-render.mts).

use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    Photos,
    Docs,
    Places,
    Flights,
}

pub const STACK_SERVICES: [Service; 4] = [Service::Photos, Service::Docs, Service::Places, Service::Flights];

pub struct ServiceDef {
    pub label: &'static str,
    /// Compose services started by this service's profile.
    pub containers: &'static [&'static str],
    /// Named volumes (compose.yml) holding this service's data.
    pub volumes: &'static [&'static str],
    pub redirect_paths: &'static [&'static str],
    pub extra_redirects: &'static [&'static str],
    /// Answered with < 400 once the service is up.
    pub health_path: &'static str,
}

impl Service {
    pub fn key(self) -> &'static str {
        match self {
            Self::Photos => "photos",
            Self::Docs => "docs",
            Self::Places => "places",
            Self::Flights => "flights",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        STACK_SERVICES.into_iter().find(|x| x.key() == s)
    }

    pub fn def(self) -> ServiceDef {
        match self {
            Self::Photos => ServiceDef {
                label: "Photos",
                containers: &["immich-server", "immich-db"],
                volumes: &["immich-db", "photos"],
                redirect_paths: &["/auth/login", "/user-settings"],
                extra_redirects: &["app.immich:///oauth-callback"],
                health_path: "/api/server/ping",
            },
            Self::Docs => ServiceDef {
                label: "Documents",
                containers: &["paperless"],
                volumes: &["docs-data", "docs-media", "docs-consume"],
                redirect_paths: &["/accounts/oidc/authentik/login/callback/"],
                extra_redirects: &[],
                health_path: "/accounts/login/",
            },
            Self::Places => ServiceDef {
                label: "Timeline",
                containers: &["dawarich", "dawarich-sidekiq"],
                volumes: &["places-public", "places-watched", "places-storage"],
                redirect_paths: &["/users/auth/openid_connect/callback"],
                extra_redirects: &[],
                health_path: "/api/v1/health",
            },
            Self::Flights => ServiceDef {
                label: "Flights",
                containers: &["airtrail"],
                volumes: &["flights-uploads"],
                redirect_paths: &["/login"],
                extra_redirects: &[],
                health_path: "/login",
            },
        }
    }
}

/// Every named volume: the base ones, each service's, the assistant's.
pub fn all_volumes() -> Vec<&'static str> {
    let mut v = vec!["db", "redis"];
    for s in STACK_SERVICES {
        v.extend(s.def().volumes);
    }
    v.push("assistant");
    v
}

/// Compose services of the assistant profile.
pub const ASSISTANT_CONTAINERS: [&str; 3] = ["assistant", "assistant-init", "assistant-tools"];

#[derive(Debug, Clone)]
pub struct Platform {
    pub base_domain: String,
    pub scheme: String,
    pub brand_name: String,
    pub time_zone: String,
    /// How containers reach the OIDC issuer host: "host-gateway" in dev.
    pub auth_host_ip: String,
}

pub struct Names<'a> {
    slug: &'a str,
    p: &'a Platform,
}

impl<'a> Names<'a> {
    pub fn new(slug: &'a str, p: &'a Platform) -> Self {
        Self { slug, p }
    }
    pub fn project(&self) -> String {
        format!("pc-t-{}", self.slug)
    }
    pub fn members_group(&self) -> String {
        format!("cust-{}", self.slug)
    }
    pub fn owners_group(&self) -> String {
        format!("cust-{}-owners", self.slug)
    }
    pub fn auth_host(&self) -> String {
        format!("auth.{}", self.p.base_domain)
    }
    pub fn portal_origin(&self) -> String {
        format!("{}://{}.{}", self.p.scheme, self.slug, self.p.base_domain)
    }
    /// Flat hostnames: one *.domain certificate covers every tenant.
    pub fn service_host(&self, s: Service) -> String {
        format!("{}-{}.{}", s.key(), self.slug, self.p.base_domain)
    }
    pub fn service_origin(&self, s: Service) -> String {
        format!("{}://{}", self.p.scheme, self.service_host(s))
    }
    pub fn assistant_host(&self) -> String {
        format!("assistant-{}.{}", self.slug, self.p.base_domain)
    }
    pub fn assistant_origin(&self) -> String {
        format!("{}://{}", self.p.scheme, self.assistant_host())
    }
    pub fn app_slug(&self, s: Service) -> String {
        format!("{}-{}", self.slug, s.key())
    }
    pub fn issuer(&self, s: Service) -> String {
        format!(
            "{}://auth.{}/application/o/{}-{}/",
            self.p.scheme,
            self.p.base_domain,
            self.slug,
            s.key()
        )
    }
}

/// (matching_mode, url) for the service's OIDC provider.
pub fn redirect_uris(slug: &str, p: &Platform, s: Service) -> Vec<Value> {
    let origin = Names::new(slug, p).service_origin(s);
    let d = s.def();
    d.redirect_paths
        .iter()
        .map(|path| json!({ "matching_mode": "strict", "url": format!("{origin}{path}") }))
        .chain(
            d.extra_redirects
                .iter()
                .map(|u| json!({ "matching_mode": "strict", "url": u })),
        )
        .collect()
}

pub const SECRET_KEYS: [&str; 15] = [
    "IMMICH_DB_PASSWORD",
    "DB_ROOT_PASSWORD",
    "PAPERLESS_DB_PASSWORD",
    "DAWARICH_DB_PASSWORD",
    "AIRTRAIL_DB_PASSWORD",
    "PAPERLESS_SECRET_KEY",
    "PAPERLESS_SERVICE_PASSWORD",
    "IMMICH_SERVICE_PASSWORD",
    "PHOTOS_OIDC_CLIENT_SECRET",
    "DOCS_OIDC_CLIENT_SECRET",
    "PLACES_OIDC_CLIENT_SECRET",
    "FLIGHTS_OIDC_CLIENT_SECRET",
    "DAWARICH_SECRET_KEY_BASE",
    "ASSISTANT_ADMIN_KEY",
    "LLM_VIRTUAL_KEY",
];

/// Ordered name -> value, as JavaScript objects keep insertion order.
pub type Vars = Vec<(String, String)>;

pub fn get<'a>(vars: &'a Vars, key: &str) -> &'a str {
    vars.iter().find(|(k, _)| k == key).map_or("", |(_, v)| v.as_str())
}

/// Sets `key`, keeping its first position like an object spread does.
fn set(vars: &mut Vars, key: &str, value: String) {
    match vars.iter_mut().find(|(k, _)| k == key) {
        Some(entry) => entry.1 = value,
        None => vars.push((key.to_owned(), value)),
    }
}

/// Alphanumeric only: some upstreams (Immich, AirTrail) reject special characters.
fn token(bytes: usize) -> Result<String, String> {
    let mut b = vec![0u8; bytes];
    getrandom::fill(&mut b).map_err(|e| format!("random: {e}"))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// Fills in any missing secret; existing values are kept so re-runs are safe.
/// Returns the secrets and the names created.
pub fn ensure_secrets(existing: &Vars) -> Result<(Vars, Vec<&'static str>), String> {
    let mut secrets = existing.clone();
    let mut created = Vec::new();
    for key in SECRET_KEYS {
        if !get(&secrets, key).is_empty() {
            continue;
        }
        let value = match key {
            "DAWARICH_SECRET_KEY_BASE" => token(64)?,
            "LLM_VIRTUAL_KEY" => format!("sk-castor-{}", token(32)?),
            _ => token(24)?,
        };
        set(&mut secrets, key, value);
        created.push(key);
    }
    Ok((secrets, created))
}

pub fn immich_config(slug: &str, p: &Platform, secrets: &Vars) -> Value {
    let n = Names::new(slug, p);
    json!({
        "oauth": {
            "enabled": true,
            "issuerUrl": n.issuer(Service::Photos),
            "clientId": n.app_slug(Service::Photos),
            "clientSecret": get(secrets, "PHOTOS_OIDC_CLIENT_SECRET"),
            "scope": "openid email profile",
            "autoLaunch": true,
            "autoRegister": true,
            "buttonText": "Continue",
            "tokenEndpointAuthMethod": "client_secret_post",
            "signingAlgorithm": "RS256",
            "mobileOverrideEnabled": false,
        },
        "passwordLogin": { "enabled": false },
        "machineLearning": { "urls": ["http://pc-node-immich-ml:3003"] },
        "server": { "externalDomain": n.service_origin(Service::Photos) },
        "newVersionCheck": { "enabled": false },
    })
}

pub fn paperless_providers(slug: &str, p: &Platform, secrets: &Vars) -> Value {
    let n = Names::new(slug, p);
    json!({
        "openid_connect": {
            "SCOPE": ["openid", "profile", "email"],
            "OAUTH_PKCE_ENABLED": true,
            "APPS": [{
                "provider_id": "authentik",
                "name": p.brand_name,
                "client_id": n.app_slug(Service::Docs),
                "secret": get(secrets, "DOCS_OIDC_CLIENT_SECRET"),
                "settings": { "server_url": format!("{}.well-known/openid-configuration", n.issuer(Service::Docs)) },
            }],
        },
    })
}

/// Where the assistant's tools reach each booked service, and the database
/// logins used to issue members read-only API keys. Unbooked: empty.
pub fn assistant_tools_env(services: &[Service], secrets: &Vars) -> Vars {
    let on = |s: Service, v: String| if services.contains(&s) { v } else { String::new() };
    vec![
        (
            "ASSISTANT_IMMICH_URL".into(),
            on(Service::Photos, "http://immich-server:2283".into()),
        ),
        (
            "ASSISTANT_IMMICH_DB_URL".into(),
            on(
                Service::Photos,
                format!(
                    "postgres://immich:{}@immich-db:5432/immich",
                    get(secrets, "IMMICH_DB_PASSWORD")
                ),
            ),
        ),
        (
            "ASSISTANT_PAPERLESS_URL".into(),
            on(Service::Docs, "http://paperless:8000".into()),
        ),
        (
            "ASSISTANT_DAWARICH_URL".into(),
            on(Service::Places, "http://dawarich:3000".into()),
        ),
        (
            "ASSISTANT_DAWARICH_DB_URL".into(),
            on(
                Service::Places,
                format!(
                    "postgres://dawarich:{}@db:5432/dawarich",
                    get(secrets, "DAWARICH_DB_PASSWORD")
                ),
            ),
        ),
        (
            "ASSISTANT_AIRTRAIL_URL".into(),
            on(Service::Flights, "http://airtrail:3000".into()),
        ),
        (
            "ASSISTANT_AIRTRAIL_DB_URL".into(),
            on(
                Service::Flights,
                format!(
                    "postgres://airtrail:{}@db:5432/airtrail",
                    get(secrets, "AIRTRAIL_DB_PASSWORD")
                ),
            ),
        ),
    ]
}

pub struct EnvInput<'a> {
    pub slug: &'a str,
    pub p: &'a Platform,
    pub secrets: &'a Vars,
    pub versions: &'a Vars,
    pub config_dir: &'a str,
    pub brand_dir: &'a str,
    pub ca_dir: &'a str,
    /// Booked services the assistant's tools may read; empty = no assistant.
    pub assistant_services: &'a [Service],
}

/// The env file consumed by packages/stack-templates/tenant/compose.yml.
/// Values are single-quoted (compose takes them literally); one that would
/// end the quote or the line is refused.
pub fn tenant_env(o: &EnvInput) -> Result<String, String> {
    let n = Names::new(o.slug, o.p);
    let mut vars: Vars = o.versions.clone();
    for (k, v) in [
        ("SLUG", o.slug.to_owned()),
        ("BASE_DOMAIN", o.p.base_domain.clone()),
        ("SCHEME", o.p.scheme.clone()),
        ("BRAND_NAME", o.p.brand_name.clone()),
        ("TZ", o.p.time_zone.clone()),
        ("AUTH_HOST", n.auth_host()),
        ("AUTH_HOST_IP", o.p.auth_host_ip.clone()),
        ("PORTAL_ORIGIN", n.portal_origin()),
        ("OWNERS_GROUP", n.owners_group()),
        ("TENANT_DIR", o.config_dir.to_owned()),
        ("BRAND_DIR", o.brand_dir.to_owned()),
        ("CA_DIR", o.ca_dir.to_owned()),
        ("OAUTH_ALLOW_INSECURE_HTTP", (o.p.scheme == "http").to_string()),
        ("PLACES_OIDC_CLIENT_ID", n.app_slug(Service::Places)),
        ("FLIGHTS_OIDC_CLIENT_ID", n.app_slug(Service::Flights)),
        (
            "PAPERLESS_SOCIALACCOUNT_PROVIDERS",
            paperless_providers(o.slug, o.p, o.secrets).to_string(),
        ),
    ] {
        set(&mut vars, k, v);
    }
    for (k, v) in assistant_tools_env(o.assistant_services, o.secrets)
        .into_iter()
        .chain(o.secrets.iter().cloned())
    {
        set(&mut vars, &k, v);
    }
    let mut out = String::new();
    for (k, v) in vars {
        if v.contains(['\'', '\n', '\r']) {
            return Err(format!("{k}: a quote or line break can't go into tenant.env"));
        }
        out.push_str(&format!("{k}='{v}'\n"));
    }
    Ok(out)
}

/// Control-plane Traefik route for a tenant on a remote node: TLS ends on
/// the control plane, which forwards the tenant's hosts to the node's edge.
pub fn edge_route(slug: &str, p: &Platform, edge_url: &str) -> String {
    let n = Names::new(slug, p);
    let mut hosts: Vec<String> = STACK_SERVICES.iter().map(|s| n.service_host(*s)).collect();
    hosts.push(n.assistant_host());
    let rule: Vec<String> = hosts.iter().map(|h| format!("Host(`{h}`)")).collect();
    let project = n.project();
    [
        format!("# Generated by the provisioner: {slug}'s services run on the node at {edge_url}."),
        "http:".into(),
        "  routers:".into(),
        format!("    {project}:"),
        format!("      rule: '{}'", rule.join(" || ")),
        "      entryPoints: [websecure]".into(),
        format!("      service: {project}"),
        "  services:".into(),
        format!("    {project}:"),
        "      loadBalancer:".into(),
        "        passHostHeader: true".into(),
        "        servers:".into(),
        format!("          - url: \"{edge_url}\""),
        String::new(),
    ]
    .join("\n")
}

/// Binds every named volume into the tenant's encrypted storage.
pub fn volumes_override(data_dir: &str) -> String {
    let mut lines = vec![
        "# Generated by the provisioner: volumes live on the tenant's encrypted storage.".to_owned(),
        "volumes:".to_owned(),
    ];
    for v in all_volumes() {
        lines.push(format!("  {v}:"));
        lines.push("    driver: local".into());
        lines.push("    driver_opts:".into());
        lines.push("      type: none".into());
        lines.push("      o: bind".into());
        lines.push(format!("      device: {data_dir}/{v}"));
    }
    lines.join("\n") + "\n"
}

/// versions.env and the like: KEY=value lines, optional single quotes.
pub fn parse_env_file(text: &str) -> Vars {
    let mut out = Vars::new();
    for line in text.lines() {
        let line = line.trim();
        let Some((k, v)) = line.split_once('=') else { continue };
        let k = k.trim();
        if k.is_empty()
            || !k
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        {
            continue;
        }
        let v = v.trim();
        let v = v.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')).unwrap_or(v);
        set(&mut out, k, v.to_owned());
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn secrets_are_kept_and_filled() {
        let (s, created) = ensure_secrets(&vec![("IMMICH_DB_PASSWORD".into(), "keep".into())]).unwrap();
        assert_eq!(get(&s, "IMMICH_DB_PASSWORD"), "keep");
        assert_eq!(created.len(), 14);
        assert_eq!(get(&s, "DB_ROOT_PASSWORD").len(), 48);
        assert_eq!(get(&s, "DAWARICH_SECRET_KEY_BASE").len(), 128);
        assert!(get(&s, "LLM_VIRTUAL_KEY").starts_with("sk-castor-") && get(&s, "LLM_VIRTUAL_KEY").len() == 74);
        let (again, none) = ensure_secrets(&s).unwrap();
        assert!(none.is_empty() && again == s);
    }

    #[test]
    fn env_refuses_quotes() {
        let p = Platform {
            base_domain: "x.ch".into(),
            scheme: "https".into(),
            brand_name: "Bob's".into(),
            time_zone: "UTC".into(),
            auth_host_ip: "1.2.3.4".into(),
        };
        let e = tenant_env(&EnvInput {
            slug: "anna",
            p: &p,
            secrets: &Vars::new(),
            versions: &Vars::new(),
            config_dir: "/c",
            brand_dir: "/b",
            ca_dir: "/ca",
            assistant_services: &[],
        });
        assert!(e.unwrap_err().starts_with("BRAND_NAME"));
    }
}
