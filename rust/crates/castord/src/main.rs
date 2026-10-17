//! castord: see lib.rs. Settings, as for the TypeScript worker (the
//! control plane's .env):
//!
//!   DATABASE_URL            the control plane's Postgres
//!   CASTORD_KINDS           comma-separated job kinds this worker takes, e.g.
//!                           "SUSPEND"; the TypeScript worker leaves them alone
//!   CASTORD_TASKS           the same for recurring tasks, e.g. "watch-nodes"
//!   SECRETS_ENCRYPTION_KEY, VOLUME_KEK
//!   CASTOR_ROOT             the repository (default: the current directory)
//!   TRAEFIK_DYNAMIC_DIR     default infra/control-plane/traefik/dynamic
//!   LITELLM_MASTER_KEY, LLM_GATEWAY_URL (default http://localhost:4000)
//!   STACKS_DOMAIN           custom stacks are on when set
//!   BACKUP_TARGET           user@host of the backup box (RESTORE, backup tasks);
//!                           BACKUP_INTERVAL_HOURS (6), BACKUP_DB_PUBLIC_KEY,
//!                           BACKUP_PG_CONTAINER, POSTGRES_USER as for the TypeScript worker
//!   STACKS_CERT_RESOLVER    production: the ACME resolver of the stacks domain's wildcard
//!   BASE_DOMAIN, PUBLIC_SCHEME, AUTHENTIK_BOOTSTRAP_TOKEN
//!                           Authentik at <scheme>://auth.<BASE_DOMAIN>
//!   BRAND_NAME (Castor Cloud), TZ (Europe/Zurich), AUTH_HOST_IP (host-gateway)
//!   AI_USD_PER_CHF          CHF -> USD rate for AI budgets (default 1.1)
//!   CA_DIR                  where ca-bundle.pem is for remote nodes (default .dev-certs)
//!   CASTORD_EXTRA_CA_CERTS  a CA bundle to trust as well (the dev CA)
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use castord::tasks::Task;
use castord::{Control, Handler, Worker};
use sqlx::postgres::PgPoolOptions;
use zeroize::Zeroizing;

/// The job kinds castord can run.
fn handlers(c: &Arc<Control>) -> Vec<Arc<dyn Handler>> {
    vec![
        Arc::new(castord::suspend::Suspend(c.clone())),
        Arc::new(castord::stacks::run::Stacks(c.clone())),
        Arc::new(castord::delete::Delete(c.clone())),
        Arc::new(castord::revoke::RevokeAccess(c.clone())),
        Arc::new(castord::apply::Provision(c.clone())),
        Arc::new(castord::apply::Update(c.clone())),
        Arc::new(castord::apply::Resume(c.clone())),
        Arc::new(castord::restore::Restore(c.clone())),
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
    let authentik = castord::authentik::Authentik::new(
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
    let versions = castord::render::parse_env_file(
        &std::fs::read_to_string(&versions_file).map_err(|e| format!("{}: {e}", versions_file.display()))?,
    );
    let usd_per_chf: f64 = var("AI_USD_PER_CHF")
        .unwrap_or_else(|| "1.1".into())
        .parse()
        .ok()
        .filter(|r: &f64| r.is_finite() && *r > 0.0)
        .ok_or("AI_USD_PER_CHF must be a positive number")?;
    let stacks = stacks_domain.map(|domain| castord::stacks::Platform {
        domain,
        scheme: scheme.clone(),
        cert_resolver: var("STACKS_CERT_RESOLVER"),
    });
    let platform = castord::render::Platform {
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
        exec: Arc::new(castord::exec::Real),
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

/// BACKUP_* as backupSettings in backup.ts; None without BACKUP_TARGET.
fn backup_settings() -> Result<Option<castord::backup::Settings>, String> {
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
    Ok(Some(castord::backup::Settings {
        target,
        interval_hours,
        db_public_key,
        pg_container: var("BACKUP_PG_CONTAINER").unwrap_or_else(|| "pc-control-plane-postgres-1".into()),
        pg_user: var("POSTGRES_USER").unwrap_or_else(|| "pc".into()),
    }))
}

/// The tasks CASTORD_TASKS names, each known (a typo would leave it unrun
/// once the TypeScript worker skips it).
fn tasks(c: &Arc<Control>, names: &[&str]) -> Result<Vec<Arc<dyn Task>>, String> {
    let all: Vec<Arc<dyn Task>> = vec![
        Arc::new(castord::tasks::WatchNodes(c.clone())),
        Arc::new(castord::tasks::MeasureStorage(c.clone())),
        Arc::new(castord::tasks::SweepDeletions),
    ];
    let mut all = all;
    if let Some(b) = &c.backup {
        all.push(Arc::new(castord::backup::BackupTenants(c.clone(), b.clone())));
        all.push(Arc::new(castord::backup::BackupDatabase(c.clone(), b.clone())));
        all.push(Arc::new(castord::backup::VerifyBackups(c.clone(), b.clone())));
    } else if names.iter().any(|n| n.starts_with("backup-") || *n == "verify-backups") {
        return Err("CASTORD_TASKS: the backup tasks need BACKUP_TARGET".into());
    }
    names
        .iter()
        .map(|n| {
            if !castord::tasks::ALL_TASKS.contains(n) {
                return Err(format!("CASTORD_TASKS: unknown task {n}"));
            }
            all.iter()
                .find(|t| t.name() == *n)
                .cloned()
                .ok_or_else(|| format!("CASTORD_TASKS: castord has no {n} yet"))
        })
        .collect()
}

/// A setting read once for the process's lifetime.
fn list(name: &str) -> &'static str {
    std::env::var(name).unwrap_or_default().leak()
}

async fn run() -> Result<(), String> {
    let url = std::env::var("DATABASE_URL").map_err(|_| "DATABASE_URL is not set".to_owned())?;
    let (kinds, task_names) = (list("CASTORD_KINDS"), list("CASTORD_TASKS"));
    let split = |s: &'static str| {
        s.split(',')
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .collect::<Vec<_>>()
    };
    let (take, task_names) = (split(kinds), split(task_names));
    if take.is_empty() && task_names.is_empty() {
        return Err("CASTORD_KINDS and CASTORD_TASKS list nothing: nothing to do".into());
    }
    let control = Arc::new(control()?);
    let tasks = tasks(&control, &task_names)?;
    let db = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(castord::db::connect_options(&url)?)
        .await
        .map_err(|e| e.to_string())?;
    let host = std::fs::read_to_string("/etc/hostname")
        .map(|h| h.trim().to_owned())
        .unwrap_or_else(|_| "castord".into());
    let id = format!("{host}:{}", std::process::id());
    let worker = if take.is_empty() {
        None
    } else {
        Some(Worker::new(id.clone(), db.clone(), handlers(&control), &take).map_err(|e| e.to_string())?)
    };

    let stopping = worker
        .as_ref()
        .map_or_else(|| Arc::new(AtomicBool::new(false)), |w| w.stopping.clone());
    let on_signal = stopping.clone();
    tokio::spawn(async move {
        let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => return eprintln!("castord: SIGTERM handler: {e}"),
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
        eprintln!("castord: finishing the current job and task, then stopping");
        on_signal.store(true, Ordering::SeqCst);
    });
    eprintln!(
        "castord {id} ready (jobs: {}; tasks: {})",
        worker.as_ref().map_or_else(|| "none".into(), |w| w.kinds().join(", ")),
        if task_names.is_empty() {
            "none".into()
        } else {
            task_names.join(", ")
        }
    );
    let task_loop = castord::tasks::run(db.clone(), tasks, stopping);
    match &worker {
        Some(w) => {
            tokio::join!(w.run(), task_loop);
        }
        None => task_loop.await,
    }
    db.close().await;
    Ok(())
}
