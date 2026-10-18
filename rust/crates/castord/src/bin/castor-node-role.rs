//! castor-node-role: the node role (docs/gatekeeper.md). The one holder of the
//! castor gatekeeper instance's token. It serves castord's calls over the
//! link (castord::link): each must be a tool of the catalog for this pool,
//! with parameters that pass the catalog's checks, and goes on to
//! gatekeeper unchanged. It has no database, no tenant keys and no other
//! secrets of the control plane, and refuses to start with any.
//!
//!   NODE_POOL               the ZFS pool (the catalog's paths and datasets)
//!   GATEKEEPER_URL          default http://gatekeeper-castor:8080/mcp
//!   GATEKEEPER_TOKEN_FILE   default /run/secrets/gatekeeper_token
//!   LINK_CERT_FILE, LINK_KEY_FILE   this role's certificate and key (PEM)
//!   LINK_PEERS_FILE         castord's certificate, the only one let in
//!   LINK_LISTEN             default 0.0.0.0:7443
use std::process::ExitCode;
use std::sync::Arc;

use castord::config::var;
use castord::gatekeeper::client::Gatekeeper;
use castord::link;
use zeroize::Zeroizing;

async fn run() -> Result<(), String> {
    castord::config::node_role_only(std::env::vars_os().map(|(k, _)| k.to_string_lossy().into_owned()))?;
    let pool = var("NODE_POOL").ok_or("NODE_POOL is not set")?;
    if !castord::valid::pool(&pool) {
        return Err(format!("invalid NODE_POOL {pool}"));
    }
    let url = var("GATEKEEPER_URL").unwrap_or_else(|| "http://gatekeeper-castor:8080/mcp".into());
    let gk_file = var("GATEKEEPER_TOKEN_FILE").unwrap_or_else(|| "/run/secrets/gatekeeper_token".into());
    let token = Zeroizing::new(std::fs::read_to_string(&gk_file).map_err(|e| format!("{gk_file}: {e}"))?);
    let gatekeeper = Gatekeeper::new(&url, token, castord::http::client(None)?)?;

    let file = |name: &str| var(name).ok_or_else(|| format!("{name} is not set"));
    let identity = link::Identity::from_files(&file("LINK_CERT_FILE")?, &file("LINK_KEY_FILE")?)?;
    let peers_file = file("LINK_PEERS_FILE")?;
    let peers = std::fs::read(&peers_file).map_err(|e| format!("{peers_file}: {e}"))?;
    let tls = link::acceptor(identity, &peers)?;
    let listen = var("LINK_LISTEN").unwrap_or_else(|| "0.0.0.0:7443".into());
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .map_err(|e| format!("{listen}: {e}"))?;
    eprintln!("castor-node-role: pool {pool}, serving castord on {listen}, gatekeeper at {url}");
    let tools = Arc::new(castord::gatekeeper::catalog(&pool));
    tokio::select! {
        r = link::serve(listener, tls, tools, Arc::new(gatekeeper)) => r,
        _ = tokio::signal::ctrl_c() => Ok(()),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("castor-node-role: {e}");
            ExitCode::FAILURE
        }
    }
}
