//! castord: see lib.rs. Settings, as for the TypeScript worker (the
//! control plane's .env):
//!
//!   DATABASE_URL            the control plane's Postgres
//!   CASTORD_KINDS           comma-separated job kinds this worker takes, e.g.
//!                           "SUSPEND"; the TypeScript worker leaves them alone
//!   SECRETS_ENCRYPTION_KEY, VOLUME_KEK
//!   CASTOR_ROOT             the repository (default: the current directory)
//!   TRAEFIK_DYNAMIC_DIR     default infra/control-plane/traefik/dynamic
//!   LITELLM_MASTER_KEY, LLM_GATEWAY_URL (default http://localhost:4000)
//!   STACKS_DOMAIN           custom stacks are on when set
//!   BASE_DOMAIN, PUBLIC_SCHEME, AUTHENTIK_BOOTSTRAP_TOKEN
//!                           Authentik at <scheme>://auth.<BASE_DOMAIN>
//!   CASTORD_EXTRA_CA_CERTS  a CA bundle to trust as well (the dev CA)
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use castord::{Control, Handler, Worker};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use zeroize::Zeroizing;

/// The job kinds castord can run.
fn handlers(c: &Arc<Control>) -> Vec<Arc<dyn Handler>> {
    vec![
        Arc::new(castord::suspend::Suspend(c.clone())),
        Arc::new(castord::delete::Delete(c.clone())),
        Arc::new(castord::revoke::RevokeAccess(c.clone())),
    ]
}

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn control() -> Result<Control, String> {
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
    let http = castord::http::client(extra_ca.as_deref())?;
    let llm = var("LITELLM_MASTER_KEY").map(|k| {
        castord::llm::Gateway::new(
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
    let authentik =
        castord::authentik::Authentik::new(&format!("{scheme}://auth.{base_domain}"), Zeroizing::new(token), http);
    Ok(Control {
        root,
        traefik_dir,
        exec: Arc::new(castord::exec::Real),
        keys: castor_crypto::Keys::from_env().map_err(|e| e.to_string())?,
        llm,
        authentik,
        stacks: var("STACKS_DOMAIN").is_some(),
    })
}

/// Prisma's URLs carry `?schema=...`, which Postgres doesn't know: it
/// becomes the search_path here.
fn connect_options(url: &str) -> Result<PgConnectOptions, String> {
    let (base, query) = url.split_once('?').unwrap_or((url, ""));
    let mut schema = None;
    let rest: Vec<&str> = query
        .split('&')
        .filter(|p| !p.is_empty())
        .filter(|p| match p.strip_prefix("schema=") {
            Some(s) => {
                schema = Some(s.to_owned());
                false
            }
            None => true,
        })
        .collect();
    let url = if rest.is_empty() {
        base.to_owned()
    } else {
        format!("{base}?{}", rest.join("&"))
    };
    let opts: PgConnectOptions = url.parse().map_err(|e| format!("DATABASE_URL: {e}"))?;
    Ok(match schema {
        Some(s) if s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') => opts.options([("search_path", s)]),
        Some(_) => return Err("DATABASE_URL: bad schema".into()),
        None => opts,
    })
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("castord: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), String> {
    let url = std::env::var("DATABASE_URL").map_err(|_| "DATABASE_URL is not set".to_owned())?;
    let kinds = std::env::var("CASTORD_KINDS").unwrap_or_default();
    let take: Vec<&str> = kinds.split(',').map(str::trim).filter(|k| !k.is_empty()).collect();
    let control = Arc::new(control()?);
    let db = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(connect_options(&url)?)
        .await
        .map_err(|e| e.to_string())?;
    let host = std::fs::read_to_string("/etc/hostname")
        .map(|h| h.trim().to_owned())
        .unwrap_or_else(|_| "castord".into());
    let worker = Worker::new(
        format!("{host}:{}", std::process::id()),
        db.clone(),
        handlers(&control),
        &take,
    )
    .map_err(|e| e.to_string())?;

    let stopping = worker.stopping.clone();
    tokio::spawn(async move {
        let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => return eprintln!("castord: SIGTERM handler: {e}"),
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
        eprintln!("castord: finishing the current job, then stopping");
        stopping.store(true, Ordering::SeqCst);
    });
    eprintln!("castord {} ready (jobs: {})", worker.id, worker.kinds().join(", "));
    worker.run().await;
    db.close().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn prisma_schema_param_becomes_search_path() {
        assert!(connect_options("postgresql://pc:pw@localhost:5432/portal?schema=public").is_ok());
        assert!(connect_options("postgresql://pc:pw@localhost:5432/portal?schema=public&sslmode=require").is_ok());
        assert!(connect_options("postgresql://pc:pw@localhost:5432/portal").is_ok());
        assert!(connect_options("postgresql://pc:pw@localhost:5432/portal?schema=a;drop").is_err());
    }
}
