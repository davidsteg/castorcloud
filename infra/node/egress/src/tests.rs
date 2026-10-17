//! The proxy end to end on loopback, against upstreams that record what reached them.
#![allow(clippy::unwrap_used)]

use super::*;

async fn proxy(rules: Rules) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let rules = Arc::new(rules);
    tokio::spawn(async move {
        loop {
            let (c, _) = listener.accept().await.unwrap();
            tokio::spawn(serve(c, rules.clone()));
        }
    });
    addr
}

fn allow(names: &[&str]) -> Rules {
    Rules::new(names.iter().map(|a| (*a).to_owned()).collect())
}

async fn ask(addr: SocketAddr, request: &str) -> String {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(request.as_bytes()).await.unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    out
}

/// An upstream on loopback that records what reached it and answers.
async fn upstream(answer: &'static str) -> (u16, tokio::sync::mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tokio::spawn(async move {
        loop {
            let (mut c, _) = listener.accept().await.unwrap();
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut got = Vec::new();
                let mut buf = [0u8; 4096];
                // Until the client stops sending (or 200 ms of quiet).
                while let Ok(Ok(n)) =
                    tokio::time::timeout(Duration::from_millis(200), c.read(&mut buf)).await
                {
                    if n == 0 {
                        break;
                    }
                    got.extend_from_slice(&buf[..n]);
                }
                let _ = tx.send(String::from_utf8_lossy(&got).into_owned()).await;
                let _ = c.write_all(answer.as_bytes()).await;
            });
        }
    });
    (port, rx)
}

#[tokio::test]
async fn an_allowed_name_that_resolves_to_the_node_is_refused() {
    let addr = proxy(allow(&["localhost"])).await;
    let r = ask(
        addr,
        "CONNECT localhost:443 HTTP/1.1\r\nHost: localhost:443\r\n\r\n",
    )
    .await;
    assert!(r.starts_with("HTTP/1.1 403"), "{r}");
    let r = ask(
        addr,
        "GET http://localhost/ HTTP/1.1\r\nHost: localhost\r\n\r\n",
    )
    .await;
    assert!(r.starts_with("HTTP/1.1 403"), "{r}");
}

#[tokio::test]
async fn only_allowed_names_ports_and_forms() {
    let addr = proxy(allow(&["example.com"])).await;
    for (req, status) in [
        ("CONNECT other.com:443 HTTP/1.1\r\n\r\n", "403"),
        ("CONNECT example.com:22 HTTP/1.1\r\n\r\n", "403"),
        ("CONNECT 1.1.1.1:443 HTTP/1.1\r\n\r\n", "403"),
        ("GET http://example.com:8080/ HTTP/1.1\r\n\r\n", "403"),
        ("GET http://other.com/ HTTP/1.1\r\n\r\n", "403"),
        ("GET / HTTP/1.1\r\nHost: example.com\r\n\r\n", "400"),
        ("GET https://example.com/ HTTP/1.1\r\n\r\n", "400"),
        (
            "POST http://example.com/ HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n",
            "501",
        ),
        (
            "POST http://example.com/ HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n",
            "400",
        ),
        ("NOT HTTP\r\n\r\n", "400"),
    ] {
        let r = ask(addr, req).await;
        assert!(r.starts_with(&format!("HTTP/1.1 {status}")), "{req}: {r}");
    }
}

#[tokio::test]
async fn plain_http_is_one_request_with_its_body_and_no_proxy_headers() {
    let (port, mut seen) = upstream("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok").await;
    let mut rules = allow(&["localhost"]);
    (rules.http, rules.any_address) = (port, true);
    let addr = proxy(rules).await;
    let r = ask(
        addr,
        &format!(
            "POST http://localhost:{port}/x?y HTTP/1.1\r\nHost: evil.test\r\nProxy-Authorization: Basic a\r\n\
             Connection: keep-alive\r\nContent-Length: 4\r\n\r\nbody\
             GET http://other.test/ HTTP/1.1\r\n\r\n"
        ),
    )
    .await;
    assert_eq!(r, "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
    let got = seen.recv().await.unwrap();
    assert_eq!(
        got,
        "POST /x?y HTTP/1.1\r\nHost: localhost\r\ncontent-length: 4\r\nConnection: close\r\n\r\nbody"
    );
}

#[tokio::test]
async fn https_tunnels_to_the_checked_address() {
    let (port, mut seen) = upstream("hello").await;
    let mut rules = allow(&["localhost"]);
    (rules.https, rules.any_address) = (port, true);
    let addr = proxy(rules).await;
    let r = ask(
        addr,
        &format!("CONNECT localhost:{port} HTTP/1.1\r\n\r\nclient-hello"),
    )
    .await;
    assert_eq!(r, "HTTP/1.1 200 Connection Established\r\n\r\nhello");
    assert_eq!(seen.recv().await.unwrap(), "client-hello");
}
