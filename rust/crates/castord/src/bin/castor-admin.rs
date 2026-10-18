//! castor-admin: the control plane's ops CLI in Rust, with the settings of
//! castord (the control plane's .env; see main.rs).
//!
//!   castor-admin node add <name> <castor-node:port> --kind truenas --pool <pool> --auth-host-ip <ip> [--capacity 20]
//!   castor-admin node add <name> <user@host> --kind mac --pool <pool> --edge <host[:port]> --auth-host-ip <ip> [--capacity 20]
//!   castor-admin node add <name> <user@host> --kind mac --role stacks --pool <pool> --edge <host[:port]> --control-plane-ip <ip> [--uid-base 100000] [--no-gvisor]
//!   castor-admin node list | drain <name> | enable <name>
//!   castor-admin backup status | list <slug> | run [<slug>] | verify
//!   castor-admin backup restore <slug> [<gen>@<snap>] [--node <name>]
//!   castor-admin backup discard <slug> <tag>
//!   castor-admin backup db-keygen | db-dump | db-list | db-get <name> <out-file>
//!   castor-admin backup db-open <file> <secret-key-file> <out.sql.gz>
//!   castor-admin staff list | add <email> | remove <email>
//!   castor-admin delete-tenant <slug>
//!   castor-admin e2e [--delete]
//!   castor-admin gatekeeper tools <pool>   the castor gatekeeper instance's tools.yaml
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::process::ExitCode;

use castord::admin::{self, Args};
use castord::{Control, backup, config};

const USAGE: &str =
    "usage: castor-admin node|backup|staff|delete-tenant|e2e|gatekeeper … (see the top of castor-admin.rs)";

/// A new file only the operator can read (dumps hold every tenant's wrapped key).
fn private_file(path: &str) -> Result<std::fs::File, String> {
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("{path}: {e}"))
}

async fn backup_cmd(a: &Args) -> Result<String, String> {
    let cmd = a.arg(1).unwrap_or("status");
    // Commands that need neither the database nor the backup box.
    match cmd {
        "db-keygen" => {
            let (public, secret) = castor_crypto::dump::key_pair().map_err(|e| e.to_string())?;
            return Ok(format!(
                "Public key, for .env on the control plane:\n  BACKUP_DB_PUBLIC_KEY={public}\n\nSecret key. Keep it OFFLINE (password manager, printed copy in a safe),\nnever on the control plane or the backup box. Without it no dump can be read:\n  {}",
                secret.as_str()
            ));
        }
        "db-open" => {
            let (Some(file), Some(key_file), Some(out)) = (a.arg(2), a.arg(3), a.arg(4)) else {
                return Err("usage: backup db-open <file> <secret-key-file> <out.sql.gz>".into());
            };
            let sealed = std::fs::read(file).map_err(|e| format!("{file}: {e}"))?;
            let key =
                zeroize::Zeroizing::new(std::fs::read_to_string(key_file).map_err(|e| format!("{key_file}: {e}"))?);
            let plain = castor_crypto::dump::open(&sealed, &key).map_err(|e| e.to_string())?;
            private_file(out)?
                .write_all(&plain)
                .map_err(|e| format!("{out}: {e}"))?;
            return Ok(format!(
                "decrypted to {out} (gzip'd SQL: gunzip -c {out} | psql -U <user> -d postgres)"
            ));
        }
        _ => {}
    }
    let c = config::control()?;
    let s = c.backup.clone().ok_or("BACKUP_TARGET is not set in .env")?;
    let db = config::connect(4).await?;
    let slug = || a.arg(2).ok_or_else(|| format!("usage: backup {cmd} <slug>"));
    match cmd {
        "status" => admin::backup_status(&c, &db, &s).await,
        "list" => admin::backup_list(&c, &s, slug()?).await,
        "run" => match a.arg(2) {
            Some(slug) => {
                let id: String = sqlx::query_scalar(r#"SELECT id FROM "Tenant" WHERE slug = $1"#)
                    .bind(slug)
                    .fetch_optional(&db)
                    .await
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("no tenant {slug}"))?;
                if backup::backup_one(&c, &db, &s, &id).await? {
                    Ok(format!("backed up {slug}"))
                } else {
                    Err(format!("the backup of {slug} failed (see above)"))
                }
            }
            None => backup::run_all(&c, &db, &s).await.map(|()| String::new()),
        },
        "verify" => backup::verify_all(&c, &db, &s).await.map(|()| String::new()),
        "restore" => admin::backup_restore(&c, &db, &s, slug()?, a.arg(3), a.get("node")).await,
        "discard" => {
            let tag = a.arg(3).ok_or("usage: backup discard <slug> <tag>")?;
            admin::backup_discard(&c, &db, slug()?, tag).await
        }
        "db-dump" => backup::dump_database(&c, &s).await,
        "db-list" => Ok(backup::db_list(&c, &s)
            .await?
            .iter()
            .map(|(name, bytes)| format!("{name}  {} KB", (bytes + 512) / 1024))
            .collect::<Vec<_>>()
            .join("\n")),
        "db-get" => {
            let (Some(name), Some(out)) = (a.arg(2), a.arg(3)) else {
                return Err("usage: backup db-get <name> <out-file>".into());
            };
            let cmd = backup::db_get_cmd(&s, name)?;
            let file = private_file(out)?;
            // Streamed to the file: a dump may be larger than memory allows.
            let status = tokio::process::Command::new(cmd.program)
                .args(&cmd.args)
                .stdin(std::process::Stdio::null())
                .stdout(file)
                .status()
                .await
                .map_err(|e| format!("ssh: {e}"))?;
            if !status.success() {
                let _ = std::fs::remove_file(out);
                return Err(format!("db-get {name} failed ({status})"));
            }
            Ok(format!(
                "saved {out} (still encrypted; open it with db-open on a trusted machine)"
            ))
        }
        _ => Err(format!("unknown command: backup {cmd}")),
    }
}

async fn run(args: Vec<String>) -> Result<String, String> {
    let a = Args::parse(&args, &["no-gvisor", "delete"])?;
    match a.arg(0) {
        // Needs neither the database nor the settings: the catalog is in the binary.
        Some("gatekeeper") => match (a.arg(1), a.arg(2)) {
            (Some("tools"), Some(pool)) if castord::valid::pool(pool) => castord::gatekeeper::tools_yaml(pool),
            _ => Err("usage: castor-admin gatekeeper tools <pool>".into()),
        },
        Some("node") => {
            let db = config::connect(4).await?;
            match a.arg(1).unwrap_or("list") {
                "add" => {
                    let c: Control = config::control()?;
                    admin::node_add(&c, &db, &a).await
                }
                "list" => admin::node_list(&db).await,
                cmd @ ("drain" | "enable") => {
                    let name = a.arg(2).ok_or_else(|| format!("usage: node {cmd} <name>"))?;
                    admin::node_enable(&db, name, cmd == "enable").await
                }
                cmd => Err(format!("unknown command: node {cmd}")),
            }
        }
        Some("backup") => backup_cmd(&a).await,
        Some("staff") => {
            let db = config::connect(2).await?;
            match (a.arg(1), a.arg(2)) {
                (Some("list"), _) => admin::staff_list(&db).await,
                (Some(cmd @ ("add" | "remove")), Some(email)) => admin::staff_set(&db, email, cmd == "add").await,
                _ => Err("usage: staff list | add <email> | remove <email>".into()),
            }
        }
        Some("delete-tenant") => {
            let slug = a.arg(1).ok_or("usage: delete-tenant <slug>")?;
            admin::delete_tenant(&config::connect(2).await?, slug).await
        }
        Some("e2e") => {
            let c = config::control()?;
            admin::e2e(&c, &config::connect(4).await?, a.get("delete").is_some())
                .await
                .map(|()| String::new())
        }
        _ => Err(USAGE.into()),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(std::env::args().skip(1).collect()).await {
        Ok(out) => {
            if !out.is_empty() {
                println!("{out}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("castor-admin: {e}");
            ExitCode::FAILURE
        }
    }
}
