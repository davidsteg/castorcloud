//! Egress proxy of one custom stack. The stack's containers have no route to
//! anywhere (internal network); this proxy is their only way out, and it lets
//! them reach exactly the hosts the stack lists (EGRESS_ALLOW), over HTTPS
//! (CONNECT to port 443) or plain HTTP (port 80). Never the LAN, the node or
//! the control plane: every address a name resolves to is checked, and the
//! proxy connects to the address it checked (no DNS rebinding).
//!
//! Plain HTTP carries one request per connection (Connection: close both
//! ways), so a second request can never reach a host that wasn't checked.

mod policy;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

const MAX_SOCKETS: usize = 256;
const IDLE: Duration = Duration::from_secs(5 * 60);
const HEAD_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_HEAD: usize = 16 * 1024;
const MAX_HEADERS: usize = 100;

/// What this proxy lets through.
struct Rules {
    allow: Vec<String>,
    /// 443 and 80; tests use local servers.
    https: u16,
    http: u16,
    /// Tests only: loopback upstreams count as public.
    #[cfg(test)]
    any_address: bool,
}

impl Rules {
    fn new(allow: Vec<String>) -> Self {
        Self {
            allow,
            https: 443,
            http: 80,
            #[cfg(test)]
            any_address: false,
        }
    }

    fn public(&self, addr: SocketAddr) -> bool {
        #[cfg(test)]
        if self.any_address {
            return true;
        }
        policy::is_public(addr.ip())
    }
}

fn deny(what: &str, why: &str) {
    println!(
        "{{\"denied\":{},\"why\":{}}}",
        policy::json_str(what),
        policy::json_str(why)
    );
}

/// The addresses of `host` if every address it has is public; else None.
async fn resolve(rules: &Rules, host: &str, port: u16) -> Option<Vec<SocketAddr>> {
    let addrs: Vec<SocketAddr> =
        tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::lookup_host((host, port)))
            .await
            .ok()?
            .ok()?
            .collect();
    if addrs.is_empty() || !addrs.iter().all(|a| rules.public(*a)) {
        return None;
    }
    Some(addrs)
}

/// A parsed request head and the bytes read after it.
struct Head {
    method: String,
    target: String,
    headers: Vec<(String, Vec<u8>)>,
    rest: Vec<u8>,
}

async fn read_head(client: &mut TcpStream) -> Result<Head, &'static str> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        let n = client.read(&mut chunk).await.map_err(|_| "read")?;
        if n == 0 {
            return Err("closed");
        }
        buf.extend_from_slice(&chunk[..n]);
        let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
        let mut req = httparse::Request::new(&mut headers);
        match req.parse(&buf) {
            Ok(httparse::Status::Complete(len)) => {
                let head = Head {
                    method: req.method.unwrap_or_default().to_owned(),
                    target: req.path.unwrap_or_default().to_owned(),
                    headers: req
                        .headers
                        .iter()
                        .map(|h| (h.name.to_ascii_lowercase(), h.value.to_vec()))
                        .collect(),
                    rest: buf[len..].to_vec(),
                };
                return Ok(head);
            }
            Ok(httparse::Status::Partial) if buf.len() < MAX_HEAD => {}
            _ => return Err("bad request"),
        }
    }
}

async fn reply(client: &mut TcpStream, status: &str, body: &str) {
    let msg = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = client.write_all(msg.as_bytes()).await;
    let _ = client.shutdown().await;
}

/// The first of the checked addresses that answers (IPv6 first if the
/// name has it, then IPv4, as the resolver orders them).
async fn connect(addrs: Vec<SocketAddr>) -> Option<TcpStream> {
    for addr in addrs {
        if let Ok(Ok(s)) = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
            return Some(s);
        }
    }
    None
}

/// Copies both ways until both sides are done or nothing moved for IDLE.
async fn pump(a: TcpStream, b: TcpStream) {
    let (ar, aw) = a.into_split();
    let (br, bw) = b.into_split();
    pump_halves(ar, bw, br, aw, true).await;
}

/// `up_from` -> `up_to` and `down_from` -> `down_to`, with one idle clock.
/// `half_close`: pass the end of the upstream direction on (a tunnel); a
/// plain HTTP request leaves the connection open for the response.
async fn pump_halves(
    up_from: impl AsyncRead + Unpin,
    up_to: impl AsyncWrite + Unpin,
    down_from: impl AsyncRead + Unpin,
    down_to: impl AsyncWrite + Unpin,
    half_close: bool,
) {
    let start = Instant::now();
    let last = Arc::new(AtomicU64::new(0));
    let one = copy(up_from, up_to, last.clone(), start, half_close);
    let two = copy(down_from, down_to, last.clone(), start, true);
    let watch = async {
        loop {
            tokio::time::sleep(Duration::from_secs(10)).await;
            let idle = start
                .elapsed()
                .saturating_sub(Duration::from_millis(last.load(Ordering::Relaxed)));
            if idle >= IDLE {
                return;
            }
        }
    };
    tokio::select! {
        _ = async { tokio::join!(one, two) } => {}
        () = watch => {}
    }
}

async fn copy(
    mut from: impl AsyncRead + Unpin,
    mut to: impl AsyncWrite + Unpin,
    last: Arc<AtomicU64>,
    start: Instant,
    shutdown: bool,
) {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = match from.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if to.write_all(&buf[..n]).await.is_err() {
            break;
        }
        last.store(
            u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }
    if shutdown {
        let _ = to.shutdown().await;
    } else {
        let _ = to.flush().await;
    }
}

async fn tunnel(mut client: TcpStream, head: Head, rules: &Rules) {
    let target = policy::connect_target(&head.target);
    let Some((host, _)) =
        target.filter(|(h, p)| *p == rules.https && policy::allowed(h, &rules.allow))
    else {
        deny(&head.target, "not allowed");
        return reply(&mut client, "403 Forbidden", "").await;
    };
    let Some(addrs) = resolve(rules, &host, rules.https).await else {
        deny(&host, "not a public address");
        return reply(&mut client, "403 Forbidden", "").await;
    };
    let Some(mut upstream) = connect(addrs).await else {
        return reply(&mut client, "502 Bad Gateway", "").await;
    };
    if client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await
        .is_err()
        || (!head.rest.is_empty() && upstream.write_all(&head.rest).await.is_err())
    {
        return;
    }
    pump(client, upstream).await;
}

/// Headers that belong to this hop, not the next.
const HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authorization",
    "proxy-authenticate",
    "te",
    "trailer",
    "upgrade",
];

async fn forward(mut client: TcpStream, head: Head, rules: &Rules) {
    let Some((host, port, path)) = policy::absolute_http(&head.target) else {
        return reply(&mut client, "400 Bad Request", "").await;
    };
    if port != rules.http || !policy::allowed(&host, &rules.allow) {
        deny(&host, "not allowed");
        return reply(&mut client, "403 Forbidden", "egress not allowed\n").await;
    }
    // Bodies only with a length: one request per connection, nothing after it.
    if head.headers.iter().any(|(n, _)| n == "transfer-encoding") {
        return reply(
            &mut client,
            "501 Not Implemented",
            "chunked requests are not supported\n",
        )
        .await;
    }
    let lengths: Vec<&[u8]> = head
        .headers
        .iter()
        .filter(|(n, _)| n == "content-length")
        .map(|(_, v)| v.as_slice())
        .collect();
    let length: u64 = match lengths.as_slice() {
        [] => 0,
        [v] => match std::str::from_utf8(v)
            .ok()
            .and_then(|s| s.trim().parse().ok())
        {
            Some(n) => n,
            None => return reply(&mut client, "400 Bad Request", "").await,
        },
        _ => return reply(&mut client, "400 Bad Request", "").await,
    };
    let Some(addrs) = resolve(rules, &host, port).await else {
        deny(&host, "not a public address");
        return reply(&mut client, "403 Forbidden", "egress not allowed\n").await;
    };
    let Some(mut upstream) = connect(addrs).await else {
        return reply(&mut client, "502 Bad Gateway", "").await;
    };
    let mut out = format!("{} {path} HTTP/1.1\r\nHost: {host}\r\n", head.method).into_bytes();
    for (name, value) in &head.headers {
        if name == "host" || HOP.contains(&name.as_str()) {
            continue;
        }
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"Connection: close\r\n\r\n");
    let early = usize::try_from(length)
        .unwrap_or(usize::MAX)
        .min(head.rest.len());
    out.extend_from_slice(&head.rest[..early]);
    if upstream.write_all(&out).await.is_err() {
        return reply(&mut client, "502 Bad Gateway", "").await;
    }
    // The rest of the body, then nothing more from the client.
    let (cr, cw) = client.into_split();
    let (ur, uw) = upstream.into_split();
    pump_halves(cr.take(length - early as u64), uw, ur, cw, false).await;
}

async fn serve(mut client: TcpStream, rules: Arc<Rules>) {
    let head = match tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut client)).await {
        Ok(Ok(h)) => h,
        Ok(Err("closed")) | Err(_) => return,
        Ok(Err(_)) => return reply(&mut client, "400 Bad Request", "").await,
    };
    if head.method == "CONNECT" {
        tunnel(client, head, &rules).await;
    } else {
        forward(client, head, &rules).await;
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    let rules = Arc::new(Rules::new(policy::parse_allow(
        &std::env::var("EGRESS_ALLOW").unwrap_or_default(),
    )));
    let port: u16 = std::env::var("EGRESS_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3128);
    let listener = match TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("egress: port {port}: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let list: Vec<String> = rules.allow.iter().map(|a| policy::json_str(a)).collect();
    println!("{{\"listening\":{port},\"allow\":[{}]}}", list.join(","));
    let slots = Arc::new(Semaphore::new(MAX_SOCKETS));
    loop {
        let client = match listener.accept().await {
            Ok((c, _)) => c,
            // Out of file descriptors and the like: don't spin.
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        // Over the limit: the connection is closed at once.
        let Ok(slot) = slots.clone().try_acquire_owned() else {
            drop(client);
            continue;
        };
        let rules = rules.clone();
        tokio::spawn(async move {
            serve(client, rules).await;
            drop(slot);
        });
    }
}

#[cfg(test)]
mod tests;
