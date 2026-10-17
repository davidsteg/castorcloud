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
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use castord::tasks::Task;
use castord::{Control, Handler, Worker};
use sqlx::postgres::PgPoolOptions;

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
    let control = Arc::new(castord::config::control()?);
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
