//! castord: the control plane's worker in Rust. It takes tenant lifecycle
//! jobs from the Postgres queue next to the TypeScript worker, and runs only
//! the job kinds it has a handler for and is told to take (CASTORD_KINDS).
//! A kind moves here by adding its handler and removing it from the
//! TypeScript worker's KINDS in the same deploy.

pub mod apply;
pub mod assistant;
pub mod authentik;
pub mod db;
pub mod delete;
pub mod exec;
pub mod http;
pub mod llm;
pub mod node;
pub mod placement;
pub mod queue;
pub mod render;
pub mod restore;
pub mod revoke;
pub mod stacks;
pub mod storage;
pub mod suspend;
pub mod valid;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sqlx::PgPool;

pub use queue::Job;

/// Runs one job kind. An Err fails the attempt with that message (it is
/// shown in the admin console and retried with backoff).
#[async_trait::async_trait]
pub trait Handler: Send + Sync {
    fn kind(&self) -> &'static str;
    async fn run(&self, cx: &JobCx, job: &Job) -> Result<(), String>;
    /// After the last attempt failed (PROVISION marks the tenant FAILED).
    async fn gave_up(&self, _cx: &JobCx, _job: &Job, _error: &str) {}
}

/// What the job handlers share: the control plane's settings and keys.
pub struct Control {
    /// The repository on the control plane (templates, .tenants/).
    pub root: PathBuf,
    /// The control plane Traefik's file-provider directory.
    pub traefik_dir: PathBuf,
    pub exec: Arc<dyn exec::Exec>,
    pub keys: castor_crypto::Keys,
    /// Unset without LITELLM_MASTER_KEY, as in the TypeScript worker.
    pub llm: Option<llm::Gateway>,
    pub authentik: authentik::Authentik,
    /// Custom stacks are on when STACKS_DOMAIN is set.
    pub stacks: Option<stacks::Platform>,
    /// user@host of the backup box, for RESTORE (BACKUP_TARGET).
    pub backup_target: Option<String>,
    pub platform: render::Platform,
    /// packages/stack-templates/versions.env: image versions of every stack.
    pub versions: render::Vars,
    pub versions_file: PathBuf,
    /// Conservative CHF to USD rate for AI budgets (AI_USD_PER_CHF).
    pub usd_per_chf: f64,
    /// Where ca-bundle.pem comes from for remote nodes (CA_DIR).
    pub ca_dir: PathBuf,
    /// Nodes prepared by this process (node files and edge are current).
    pub prepared: Mutex<HashSet<String>>,
    /// For health checks and service accounts: no redirects, 30 s timeout.
    pub http: reqwest::Client,
    pub health_timeout: Duration,
    pub health_every: Duration,
}

/// What a handler gets besides the job.
pub struct JobCx {
    pub db: PgPool,
}

impl JobCx {
    /// A step the portal's progress page shows, and a log line for it.
    pub async fn report(&self, job: &Job, step: &str, message: &str) -> Result<(), String> {
        sqlx::query(r#"UPDATE "Job" SET step = $2 WHERE id = $1"#)
            .bind(&job.id)
            .bind(step)
            .execute(&self.db)
            .await
            .map_err(|e| e.to_string())?;
        self.log(job, &format!("{step}: {message}")).await;
        Ok(())
    }

    /// A line in the job's log (the portal's progress page shows it).
    pub async fn log(&self, job: &Job, line: &str) {
        eprintln!("[{} {}] {line}", job.kind, job.tenant_id);
        if let Err(e) = queue::log(&self.db, &job.id, line).await {
            eprintln!("[{} {}] writing the log: {e}", job.kind, job.tenant_id);
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("unknown job kind {0}")]
    UnknownKind(String),
    #[error("castord has no handler for {0} yet")]
    NoHandler(String),
    #[error("CASTORD_KINDS lists no job kinds: nothing to do")]
    Empty,
}

pub struct Worker {
    pub id: String,
    pub db: PgPool,
    handlers: HashMap<&'static str, Arc<dyn Handler>>,
    kinds: Vec<&'static str>,
    pub stopping: Arc<AtomicBool>,
    pub idle: Duration,
}

impl Worker {
    /// `take`: the kinds this worker is told to run (CASTORD_KINDS). Each
    /// must be a real kind with a handler, so a typo can't leave jobs unrun
    /// while the TypeScript worker no longer takes them.
    pub fn new(id: String, db: PgPool, handlers: Vec<Arc<dyn Handler>>, take: &[&str]) -> Result<Self, ConfigError> {
        let handlers: HashMap<_, _> = handlers.into_iter().map(|h| (h.kind(), h)).collect();
        let mut kinds = Vec::new();
        for k in take {
            let Some(kind) = queue::ALL_KINDS.iter().find(|a| *a == k) else {
                return Err(ConfigError::UnknownKind((*k).to_owned()));
            };
            if !handlers.contains_key(kind) {
                return Err(ConfigError::NoHandler((*k).to_owned()));
            }
            kinds.push(*kind);
        }
        if kinds.is_empty() {
            return Err(ConfigError::Empty);
        }
        Ok(Self {
            id,
            db,
            handlers,
            kinds,
            stopping: Arc::new(AtomicBool::new(false)),
            idle: Duration::from_secs(2),
        })
    }

    pub fn kinds(&self) -> &[&'static str] {
        &self.kinds
    }

    /// One round: heartbeat, recover dead workers' jobs, run at most one job.
    /// Returns whether a job ran.
    pub async fn step(&self) -> sqlx::Result<bool> {
        queue::heartbeat(&self.db, &self.id, &self.kinds).await?;
        for r in queue::recover_expired(&self.db).await? {
            eprintln!(
                "[{} {}] attempt {} lost: its worker stopped",
                r.job.kind, r.job.tenant_id, r.job.attempts
            );
            // Only for kinds this worker owns; the TypeScript worker handles its own.
            if r.failed
                && let Some(h) = self.handlers.get(r.job.kind.as_str())
                && self.kinds.contains(&h.kind())
            {
                h.gave_up(&JobCx { db: self.db.clone() }, &r.job, "the worker running it stopped")
                    .await;
            }
        }
        let Some(job) = queue::claim(&self.db, &self.id, &self.kinds).await? else {
            return Ok(false);
        };
        self.work(&job).await?;
        Ok(true)
    }

    async fn work(&self, job: &Job) -> sqlx::Result<()> {
        let Some(handler) = self.handlers.get(job.kind.as_str()).cloned() else {
            // Not possible: claim only returns our kinds. Fail it visibly anyway.
            queue::fail(&self.db, &job.id, &self.id, "castord has no handler for this kind").await?;
            return Ok(());
        };
        let cx = JobCx { db: self.db.clone() };
        let renewing = {
            let (db, id, worker) = (self.db.clone(), job.id.clone(), self.id.clone());
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(queue::RENEW);
                tick.tick().await;
                loop {
                    tick.tick().await;
                    match queue::renew(&db, &id, &worker).await {
                        Ok(true) => {}
                        Ok(false) => eprintln!("lost the lease on job {id}"),
                        Err(e) => eprintln!("renewing the lease on job {id}: {e}"),
                    }
                }
            })
        };
        let result = handler.run(&cx, job).await;
        renewing.abort();
        match result {
            Ok(()) => queue::succeed(&self.db, &job.id, &self.id).await,
            Err(msg) => {
                let last = queue::fail(&self.db, &job.id, &self.id, &msg).await?;
                eprintln!(
                    "[{} {}] attempt {} failed{}: {msg}",
                    job.kind,
                    job.tenant_id,
                    job.attempts,
                    if last { " (last)" } else { "" }
                );
                if last {
                    handler.gave_up(&cx, job, &msg).await;
                }
                Ok(())
            }
        }
    }

    /// Until `stopping` is set: finishes the job in hand, then returns.
    pub async fn run(&self) {
        while !self.stopping.load(Ordering::SeqCst) {
            match self.step().await {
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => eprintln!("[queue] {e}"),
            }
            tokio::time::sleep(self.idle).await;
        }
    }
}
