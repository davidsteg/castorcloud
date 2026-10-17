//! The control plane's settings (its .env), shared by castord and the CLIs:
//! see main.rs for the list.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use zeroize::Zeroizing;

use crate::Control;

pub fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// The control plane's Postgres (DATABASE_URL).
pub async fn connect(max: u32) -> Result<PgPool, String> {
    let url = var("DATABASE_URL").ok_or("DATABASE_URL is not set")?;
    PgPoolOptions::new()
        .max_connections(max)
        .acquire_timeout(Duration::from_secs(30))
        .connect_with(crate::db::connect_options(&url)?)
        .await
        .map_err(|e| format!("database: {e}"))
}

pub fn control() -> Result<Control, String> {
    let root = match var("CASTOR_ROOT") {
        Some(r) => PathBuf::from(r),
        None => std::env::current_dir().map_err(|e| e.to_string())?,
    };
    if !root.join("packages/stack-templates/tenant/compose.yml").is_file() {
        return Err(format!(
            "{} is not the castorcloud repository (set CASTOR_ROOT)",
            root.display()
        ));
    }
    let traefik_dir = var("TRAEFIK_DYNAMIC_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("infra/control-plane/traefik/dynamic"));
    let extra_ca = match var("CASTORD_EXTRA_CA_CERTS") {
        Some(p) => Some(std::fs::read(&p).map_err(|e| format!("CASTORD_EXTRA_CA_CERTS {p}: {e}"))?),
        None => None,
    };
    let http = crate::http::client(extra_ca.as_deref())?;
    let llm = var("LITELLM_MASTER_KEY").map(|k| {
        crate::llm::Gateway::new(
            &var("LLM_GATEWAY_URL").unwrap_or_else(|| "http://localhost:4000".into()),
            Zeroizing::new(k),
            http.clone(),
        )
    });
    let scheme = var("PUBLIC_SCHEME").unwrap_or_else(|| "https".into());
    if scheme != "https" && scheme != "http" {
        return Err(format!("PUBLIC_SCHEME must be http or https, not {scheme}"));
    }
    let base_domain = var("BASE_DOMAIN").unwrap_or_else(|| "brand.localhost".into());
    let token = var("AUTHENTIK_BOOTSTRAP_TOKEN").ok_or("AUTHENTIK_BOOTSTRAP_TOKEN is not set")?;
    let authentik = crate::authentik::Authentik::new(
        &format!("{scheme}://auth.{base_domain}"),
        Zeroizing::new(token),
        http.clone(),
    );
    let stacks_domain = var("STACKS_DOMAIN");
    // Code we don't control must never share cookies with the portal (*.BASE_DOMAIN).
    if stacks_domain.as_deref() == Some(base_domain.as_str()) && scheme == "https" {
        return Err("STACKS_DOMAIN must not be BASE_DOMAIN in production".into());
    }
    let versions_file = root.join("packages/stack-templates/versions.env");
    let versions = crate::render::parse_env_file(
        &std::fs::read_to_string(&versions_file).map_err(|e| format!("{}: {e}", versions_file.display()))?,
    );
    let usd_per_chf: f64 = var("AI_USD_PER_CHF")
        .unwrap_or_else(|| "1.1".into())
        .parse()
        .ok()
        .filter(|r: &f64| r.is_finite() && *r > 0.0)
        .ok_or("AI_USD_PER_CHF must be a positive number")?;
    let stacks = stacks_domain.map(|domain| crate::stacks::Platform {
        domain,
        scheme: scheme.clone(),
        cert_resolver: var("STACKS_CERT_RESOLVER"),
    });
    let platform = crate::render::Platform {
        base_domain,
        scheme,
        brand_name: var("BRAND_NAME").unwrap_or_else(|| "Castor Cloud".into()),
        time_zone: var("TZ").unwrap_or_else(|| "Europe/Zurich".into()),
        auth_host_ip: var("AUTH_HOST_IP").unwrap_or_else(|| "host-gateway".into()),
    };
    Ok(Control {
        ca_dir: var("CA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join(".dev-certs")),
        root,
        traefik_dir,
        exec: Arc::new(crate::exec::Real),
        keys: castor_crypto::Keys::from_env().map_err(|e| e.to_string())?,
        llm,
        authentik,
        stacks,
        backup: backup_settings()?,
        platform,
        versions,
        versions_file,
        usd_per_chf,
        prepared: Default::default(),
        http,
        health_timeout: std::time::Duration::from_secs(900),
        health_every: std::time::Duration::from_secs(5),
    })
}

/// BACKUP_*; None without BACKUP_TARGET.
pub fn backup_settings() -> Result<Option<crate::backup::Settings>, String> {
    let Some(target) = var("BACKUP_TARGET") else {
        return Ok(None);
    };
    let interval_hours: f64 = var("BACKUP_INTERVAL_HOURS")
        .unwrap_or_else(|| "6".into())
        .parse()
        .map_err(|_| "BACKUP_INTERVAL_HOURS must be a number")?;
    if !(interval_hours.is_finite() && interval_hours > 0.0) {
        return Err("BACKUP_INTERVAL_HOURS must be above 0".into());
    }
    let db_public_key = var("BACKUP_DB_PUBLIC_KEY");
    if let Some(k) = &db_public_key {
        castor_crypto::dump::public_key(k).map_err(|e| e.to_string())?;
    }
    Ok(Some(crate::backup::Settings {
        target,
        interval_hours,
        db_public_key,
        pg_container: var("BACKUP_PG_CONTAINER").unwrap_or_else(|| "pc-control-plane-postgres-1".into()),
        pg_user: var("POSTGRES_USER").unwrap_or_else(|| "pc".into()),
    }))
}
