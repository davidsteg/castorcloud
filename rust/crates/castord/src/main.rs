//! castord: see lib.rs. Settings (the control plane's .env):
//!
//!   DATABASE_URL            the control plane's Postgres
//!   CASTORD_KINDS           unset: every job kind; else the comma-separated
//!                           kinds this worker takes, or "none"
//!   CASTORD_TASKS           the same for recurring tasks, e.g. "watch-nodes"
//!   LOCAL_NODE              "off": the Docker host castord runs on takes no tenants
//!   AUTHENTIK_EMAIL__HOST   set: sign-up verifies the e-mail address
//!   SECRETS_ENCRYPTION_KEY, VOLUME_KEK
//!   CASTOR_ROOT             the repository (default: the current directory)
//!   TRAEFIK_DYNAMIC_DIR     default infra/control-plane/traefik/dynamic
//!   LITELLM_MASTER_KEY, LLM_GATEWAY_URL (default http://localhost:4000)
//!   STACKS_DOMAIN           custom stacks are on when set
//!   BACKUP_TARGET           user@host of the backup box (RESTORE, backup tasks);
//!                           BACKUP_INTERVAL_HOURS (6), BACKUP_DB_PUBLIC_KEY,
//!                           BACKUP_PG_CONTAINER, POSTGRES_USER
//!   STACKS_CERT_RESOLVER    production: the ACME resolver of the stacks domain's wildcard
//!   BASE_DOMAIN, PUBLIC_SCHEME, AUTHENTIK_BOOTSTRAP_TOKEN
//!                           Authentik at <scheme>://auth.<BASE_DOMAIN>
//!   BRAND_NAME (Castor Cloud), TZ (Europe/Zurich), AUTH_HOST_IP (host-gateway)
//!   AI_USD_PER_CHF          CHF -> USD rate for AI budgets (default 1.1)
//!   CA_DIR                  where ca-bundle.pem is for remote nodes (default .dev-certs)
//!   CASTORD_EXTRA_CA_CERTS  a CA bundle to trust as well (the dev CA)
//!   LINK_CERT_FILE, LINK_KEY_FILE   castord's certificate and key for the
//!                           link to TrueNAS node roles (link.rs, docs/gatekeeper.md)
//!   LINK_PEERS_FILE         the node roles' certificates, the only ones trusted
//!   CASTOR_IMAGE_TAG        set: nodes pull Castor's images signed from GHCR
//!                           (images.rs, docs/ci.md): CASTOR_COSIGN_PUBKEY,
//!                           CASTOR_IMAGE_REPO, GHCR_PULL_USER, GHCR_PULL_TOKEN
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

/// The recurring tasks: maintenance, and the backups in a loop of their own
/// so a long first send never holds up the node watcher. `names` (from
/// CASTORD_TASKS) picks some; None runs all, the backups only with
/// BACKUP_TARGET.
fn tasks(c: &Arc<Control>, names: Option<&[&str]>) -> Result<[Vec<Arc<dyn Task>>; 2], String> {
    let maintenance: Vec<Arc<dyn Task>> = vec![
        Arc::new(castord::tasks::WatchNodes(c.clone())),
        Arc::new(castord::tasks::MeasureStorage(c.clone())),
        Arc::new(castord::tasks::SweepDeletions),
    ];
    let backups: Vec<Arc<dyn Task>> = match &c.backup {
        Some(b) => vec![
            Arc::new(castord::backup::BackupTenants(c.clone(), b.clone())),
            Arc::new(castord::backup::BackupDatabase(c.clone(), b.clone())),
            Arc::new(castord::backup::VerifyBackups(c.clone(), b.clone())),
        ],
        None => vec![],
    };
    let Some(names) = names else {
        if c.backup.is_none() {
            eprintln!("castord: BACKUP_TARGET is not set: tenant backups are OFF");
        }
        return Ok([maintenance, backups]);
    };
    for n in names {
        if !castord::tasks::ALL_TASKS.contains(n) {
            return Err(format!("CASTORD_TASKS: unknown task {n}"));
        }
        if c.backup.is_none() && !maintenance.iter().any(|t| t.name() == *n) {
            return Err(format!("CASTORD_TASKS: {n} needs BACKUP_TARGET"));
        }
    }
    let pick = |all: Vec<Arc<dyn Task>>| all.into_iter().filter(|t| names.contains(&t.name())).collect();
    Ok([pick(maintenance), pick(backups)])
}

/// A list setting read once for the process's lifetime: None when unset
/// (everything), empty for "none".
fn list(name: &str) -> Option<Vec<&'static str>> {
    let v: &'static str = std::env::var(name).ok().filter(|v| !v.trim().is_empty())?.leak();
    if v.trim() == "none" {
        return Some(vec![]);
    }
    Some(v.split(',').map(str::trim).filter(|k| !k.is_empty()).collect())
}

async fn run() -> Result<(), String> {
    let url = std::env::var("DATABASE_URL").map_err(|_| "DATABASE_URL is not set".to_owned())?;
    let take = list("CASTORD_KINDS").unwrap_or_else(|| castord::queue::ALL_KINDS.to_vec());
    let task_names = list("CASTORD_TASKS");
    let control = Arc::new(castord::config::control()?);
    let [maintenance, backups] = tasks(&control, task_names.as_deref())?;
    if take.is_empty() && maintenance.is_empty() && backups.is_empty() {
        return Err("CASTORD_KINDS and CASTORD_TASKS leave nothing to do".into());
    }
    let db = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(castord::db::connect_options(&url)?)
        .await
        .map_err(|e| e.to_string())?;
    // Neither stops castord: without the flow sign-up waits, and a job
    // prepares the local node again before it uses it.
    if let Err(e) =
        castord::startup::enrollment(&control, castord::config::var("AUTHENTIK_EMAIL__HOST").is_some()).await
    {
        eprintln!("castord: the self-registration flow: {e}");
    }
    let local = castord::config::var("LOCAL_NODE").as_deref() != Some("off");
    if let Err(e) = castord::startup::local_node(&control, &db, local).await {
        eprintln!("castord: the local node: {e}");
    }
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
        match maintenance.iter().chain(&backups).map(|t| t.name()).collect::<Vec<_>>() {
            n if n.is_empty() => "none".into(),
            n => n.join(", "),
        }
    );
    let maintenance = castord::tasks::run(db.clone(), maintenance, stopping.clone());
    let backups = castord::tasks::run(db.clone(), backups, stopping);
    match &worker {
        Some(w) => {
            tokio::join!(w.run(), maintenance, backups);
        }
        None => {
            tokio::join!(maintenance, backups);
        }
    }
    db.close().await;
    Ok(())
}
