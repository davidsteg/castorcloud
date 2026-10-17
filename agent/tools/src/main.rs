//! Read-only MCP tools over one tenant's services, for that tenant's Hermes
//! (docs/assistant.md). One MCP endpoint per booked service; runs only on the
//! tenant's private network, never exposed through Traefik.
//!
//! Environment (packages/stack-templates/tenant/compose.yml): MEMBERS_FILE,
//! PORTAL_ORIGIN, PORT, TZ, and per service IMMICH_URL/IMMICH_DB_URL,
//! PAPERLESS_URL/PAPERLESS_USER/PAPERLESS_PASSWORD, DAWARICH_URL/DAWARICH_DB_URL,
//! AIRTRAIL_URL/AIRTRAIL_DB_URL (empty: not booked, no tools).
//!
//! `assistant-tools --health` checks a running server (the image's HEALTHCHECK).

mod db;
mod limits;
mod mcp;
mod members;
mod schema;
mod server;
mod services;
mod upstream;

#[cfg(test)]
mod tests;

use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::TcpListener;

use crate::limits::Zone;
use crate::server::App;
use crate::services::{Key, Services};

fn env(name: &str) -> Option<String> {
    std::env::var_os(name).map(|v| v.to_string_lossy().into_owned())
}

fn port() -> Result<u16, String> {
    let raw = env("PORT").unwrap_or_else(|| "8080".into());
    raw.parse()
        .map_err(|_| format!("PORT {raw:?} is not a port"))
}

/// GET /healthz on the local server; exit 0 when it answers 200.
fn health(port: u16) -> ExitCode {
    let check = || -> std::io::Result<bool> {
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
        s.set_read_timeout(Some(Duration::from_secs(2)))?;
        s.set_write_timeout(Some(Duration::from_secs(2)))?;
        s.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;
        let mut head = [0u8; 12];
        s.read_exact(&mut head)?;
        Ok(head.starts_with(b"HTTP/1.1 200"))
    };
    match check() {
        Ok(true) => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    }
}

/// All interfaces, IPv6 and IPv4 where the container has IPv6, else IPv4.
async fn bind(port: u16) -> std::io::Result<TcpListener> {
    match TcpListener::bind(SocketAddr::from(([0u16; 8], port))).await {
        Ok(l) => Ok(l),
        Err(_) => TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port))).await,
    }
}

async fn shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let (Ok(mut term), Ok(mut int)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) {
            tokio::select! {
                _ = term.recv() => {}
                _ = int.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

async fn serve(app: Arc<App>, listener: TcpListener) {
    let stop = shutdown();
    tokio::pin!(stop);
    loop {
        let stream = tokio::select! {
            _ = &mut stop => return,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => stream,
                Err(e) => {
                    eprintln!("accept: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
        };
        let app = app.clone();
        tokio::spawn(async move {
            let service = service_fn(move |req| {
                let app = app.clone();
                async move { Ok::<_, Infallible>(server::handle(&app, req).await) }
            });
            let _ = http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(Duration::from_secs(60))
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let port = match port() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    if std::env::args().nth(1).as_deref() == Some("--health") {
        return health(port);
    }
    let http = match upstream::client() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("http client: {e}");
            return ExitCode::FAILURE;
        }
    };
    let zone = Zone::new(&env("TZ").unwrap_or_default());
    if !zone.is_valid() {
        eprintln!(
            "unknown time zone {:?}: tools that need dates will fail",
            zone.name
        );
    }
    let app = Arc::new(App {
        members_file: env("MEMBERS_FILE").unwrap_or_else(|| "/config/members.json".into()),
        portal_origin: env("PORTAL_ORIGIN").unwrap_or_default(),
        services: Services::from_env(env),
        http,
        zone,
    });
    let listener = match bind(port).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot listen on :{port}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let names: Vec<&str> = app
        .services
        .configured()
        .into_iter()
        .map(Key::as_str)
        .collect();
    println!(
        "assistant tools on :{port} ({})",
        if names.is_empty() {
            "no services".into()
        } else {
            names.join(", ")
        }
    );
    serve(app, listener).await;
    ExitCode::SUCCESS
}
