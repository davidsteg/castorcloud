//! castord: see lib.rs.
//!
//!   DATABASE_URL    the control plane's Postgres (as for the portal)
//!   CASTORD_KINDS   comma-separated job kinds this worker takes, e.g. "SUSPEND"
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use castord::{Handler, Worker};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

/// Job kinds castord can run. Empty until the first one moves over.
fn handlers() -> Vec<Arc<dyn Handler>> {
    Vec::new()
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
    let db = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(connect_options(&url)?)
        .await
        .map_err(|e| e.to_string())?;
    let host = std::fs::read_to_string("/etc/hostname")
        .map(|h| h.trim().to_owned())
        .unwrap_or_else(|_| "castord".into());
    let worker = Worker::new(format!("{host}:{}", std::process::id()), db.clone(), handlers(), &take)
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
