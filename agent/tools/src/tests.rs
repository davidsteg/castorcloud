//! The server end to end: HTTP checks, the MCP exchange and tool calls
//! against fake services. The tools/list fixture is what the TypeScript
//! version answered (fixtures/tools-list.json). Tests that issue API keys
//! need Postgres and run when TEST_DATABASE_URL is set (CI).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tokio::net::TcpListener;

use crate::limits::Zone;
use crate::members::sha256_hex;
use crate::server::{App, MAX_BODY, handle};
use crate::services::Services;
use crate::upstream;

const ACCEPT: &str = "application/json, text/event-stream";

/// One request a fake service received.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    query: String,
    headers: HashMap<String, String>,
    body: String,
}

type Answer = Box<dyn Fn(&Seen) -> (u16, String) + Send + Sync>;

/// A fake service on a free local port; returns its URL and what it saw.
async fn fake(answer: Answer) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let answer = Arc::new(answer);
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (answer, log) = (answer.clone(), log.clone());
            tokio::spawn(async move {
                let svc = service_fn(move |req: Request<hyper::body::Incoming>| {
                    let (answer, log) = (answer.clone(), log.clone());
                    async move {
                        let (parts, body) = req.into_parts();
                        let body = body
                            .collect()
                            .await
                            .map(|b| b.to_bytes())
                            .unwrap_or_default();
                        let s = Seen {
                            method: parts.method.to_string(),
                            path: parts.uri.path().to_owned(),
                            query: parts.uri.query().unwrap_or_default().to_owned(),
                            headers: parts
                                .headers
                                .iter()
                                .map(|(k, v)| {
                                    (k.to_string(), v.to_str().unwrap_or_default().to_owned())
                                })
                                .collect(),
                            body: String::from_utf8_lossy(&body).into_owned(),
                        };
                        let (status, text) = answer(&s);
                        log.lock().unwrap().push(s);
                        let mut res = Response::new(Full::new(Bytes::from(text)));
                        *res.status_mut() = hyper::StatusCode::from_u16(status).unwrap();
                        if status == 302 {
                            res.headers_mut()
                                .insert("location", "https://example.com/".parse().unwrap());
                        }
                        Ok::<_, Infallible>(res)
                    }
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), svc)
                    .await;
            });
        }
    });
    (url, seen)
}

fn members_file(name: &str) -> String {
    let path =
        std::env::temp_dir().join(format!("castor-tools-{}-{name}.json", std::process::id()));
    let members = json!({ "members": [
        { "id": "anna", "email": "Anna@X.ch", "sub": "sub-anna", "name": "Anna", "tokenSha256": sha256_hex("token-a") },
        { "id": "ben", "email": "ben@x.ch", "sub": "sub-ben", "tokenSha256": sha256_hex("token-b") },
    ]});
    std::fs::write(&path, members.to_string()).unwrap();
    path.to_string_lossy().into_owned()
}

fn app(name: &str, env: &[(&str, &str)]) -> App {
    let env: HashMap<String, String> = env
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    App {
        members_file: members_file(name),
        portal_origin: "https://muster.castorcloud.ch".into(),
        services: Services::from_env(|k| env.get(k).cloned()),
        http: upstream::client().unwrap(),
        zone: Zone::new("Europe/Zurich"),
    }
}

struct Res {
    status: u16,
    content_type: Option<String>,
    body: Option<Value>,
}

async fn send(app: &App, method: &str, path: &str, headers: &[(&str, &str)], body: Vec<u8>) -> Res {
    let mut req = Request::builder().method(method).uri(path);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let res = handle(app, req.body(Full::new(Bytes::from(body))).unwrap()).await;
    let status = res.status().as_u16();
    let content_type = res
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_owned());
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let body = (!bytes.is_empty()).then(|| serde_json::from_slice(&bytes).unwrap());
    Res {
        status,
        content_type,
        body,
    }
}

const MCP_HEADERS: [(&str, &str); 3] = [
    ("authorization", "Bearer token-a"),
    ("accept", ACCEPT),
    ("content-type", "application/json"),
];

async fn rpc(app: &App, service: &str, body: Value) -> Res {
    send(
        app,
        "POST",
        &format!("/mcp/{service}"),
        &MCP_HEADERS,
        body.to_string().into_bytes(),
    )
    .await
}

async fn call(app: &App, service: &str, tool: &str, args: Value) -> (bool, Value) {
    let res = rpc(app, service, json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": { "name": tool, "arguments": args } })).await;
    assert_eq!(res.status, 200);
    let body = res.body.unwrap();
    assert_eq!(body["id"], 7);
    let result = &body["result"];
    let text = result["content"][0]["text"].as_str().unwrap().to_owned();
    let is_error = result["isError"] == true;
    (
        is_error,
        if is_error {
            Value::String(text)
        } else {
            serde_json::from_str(&text).unwrap()
        },
    )
}

fn docs_env(url: &str) -> Vec<(&'static str, String)> {
    vec![
        ("PAPERLESS_URL", url.to_owned()),
        ("PAPERLESS_USER", "pc-service".into()),
        ("PAPERLESS_PASSWORD", "pw".into()),
    ]
}

fn with_env<'a>(env: &'a [(&'static str, String)]) -> Vec<(&'a str, &'a str)> {
    env.iter().map(|(k, v)| (*k, v.as_str())).collect()
}

#[tokio::test]
async fn http_requests_are_checked_before_mcp() {
    let env = docs_env("http://127.0.0.1:9");
    let app = app("http", &with_env(&env));
    let r = send(&app, "GET", "/healthz", &[], Vec::new()).await;
    assert_eq!(
        (r.status, r.body),
        (200, Some(json!({ "ok": true, "services": ["docs"] })))
    );

    let r = send(&app, "POST", "/mcp/music", &MCP_HEADERS, b"{}".to_vec()).await;
    assert_eq!(
        (r.status, r.body),
        (404, Some(json!({ "error": "unknown service" })))
    );
    // Not booked: no endpoint.
    assert_eq!(
        send(&app, "POST", "/mcp/photos", &MCP_HEADERS, Vec::new())
            .await
            .status,
        404
    );
    let r = send(&app, "GET", "/mcp/docs", &MCP_HEADERS, Vec::new()).await;
    assert_eq!(
        (r.status, r.body),
        (405, Some(json!({ "error": "stateless server: POST only" })))
    );

    let unauthorized = Some(json!({ "error": "unauthorized" }));
    let init = json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" })
        .to_string()
        .into_bytes();
    let r = send(
        &app,
        "POST",
        "/mcp/docs",
        &[("accept", ACCEPT), ("content-type", "application/json")],
        init.clone(),
    )
    .await;
    assert_eq!((r.status, &r.body), (401, &unauthorized));
    for auth in [
        "Bearer token-x",
        "token-a",
        "Basic token-a",
        "Bearer token-a x",
    ] {
        let r = send(
            &app,
            "POST",
            "/mcp/docs",
            &[("authorization", auth), ("accept", ACCEPT)],
            init.clone(),
        )
        .await;
        assert_eq!((r.status, &r.body), (401, &unauthorized), "{auth}");
    }
    // An unreadable members file lets nobody in.
    let mut broken = self::app("http-broken", &with_env(&env));
    broken.members_file = "/nonexistent/members.json".into();
    assert_eq!(
        rpc(
            &broken,
            "docs",
            json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" })
        )
        .await
        .status,
        401
    );

    let invalid = Some(json!({ "error": "invalid body" }));
    let big = format!("\"{}\"", "x".repeat(MAX_BODY - 1)).into_bytes();
    assert_eq!(big.len(), MAX_BODY + 1);
    let r = send(&app, "POST", "/mcp/docs", &MCP_HEADERS, big).await;
    assert_eq!((r.status, &r.body), (400, &invalid));
    let r = send(&app, "POST", "/mcp/docs", &MCP_HEADERS, b"{nope".to_vec()).await;
    assert_eq!((r.status, &r.body), (400, &invalid));
    // Exactly the limit is read (and then not a JSON-RPC message).
    let r = send(
        &app,
        "POST",
        "/mcp/docs",
        &MCP_HEADERS,
        format!("\"{}\"", "x".repeat(MAX_BODY - 2)).into_bytes(),
    )
    .await;
    assert_eq!(r.status, 400);
    assert_eq!(
        r.body.unwrap()["error"]["message"],
        "Parse error: Invalid JSON-RPC message"
    );
    let r = send(&app, "POST", "/mcp/docs", &MCP_HEADERS, Vec::new()).await;
    assert_eq!(
        r.body.unwrap()["error"],
        json!({ "code": -32700, "message": "Parse error: Invalid JSON" })
    );

    let r = send(
        &app,
        "POST",
        "/mcp/docs",
        &[
            ("authorization", "Bearer token-a"),
            ("content-type", "application/json"),
        ],
        init.clone(),
    )
    .await;
    assert_eq!(r.status, 406);
    let r = send(
        &app,
        "POST",
        "/mcp/docs",
        &[
            ("authorization", "bearer token-a"),
            ("accept", ACCEPT),
            ("content-type", "text/plain"),
        ],
        init,
    )
    .await;
    assert_eq!(r.status, 415);
    assert_eq!(r.content_type.as_deref(), Some("application/json"));
    assert_eq!(
        r.body,
        Some(
            json!({ "jsonrpc": "2.0", "error": { "code": -32000, "message": "Unsupported Media Type: Content-Type must be application/json" }, "id": null })
        )
    );
}

#[tokio::test]
async fn the_mcp_exchange_follows_the_sdk() {
    let env = docs_env("http://127.0.0.1:9");
    let app = app("mcp", &with_env(&env));
    let init = |v: &str| {
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": v, "capabilities": {}, "clientInfo": { "name": "hermes", "version": "1" } } })
    };
    let r = rpc(&app, "docs", init("2025-06-18")).await;
    assert_eq!(
        r.body.unwrap().to_string(),
        r#"{"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{"listChanged":true}},"serverInfo":{"name":"castor-docs","version":"1.0.0"}},"jsonrpc":"2.0","id":1}"#
    );
    // An unknown version is answered with the latest.
    assert_eq!(
        rpc(&app, "docs", init("1999-01-01")).await.body.unwrap()["result"]["protocolVersion"],
        "2025-11-25"
    );

    let r = rpc(
        &app,
        "docs",
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;
    assert_eq!((r.status, r.content_type, r.body), (202, None, None));
    let r = rpc(
        &app,
        "docs",
        json!({ "jsonrpc": "2.0", "id": "p", "method": "ping" }),
    )
    .await;
    assert_eq!(
        r.body,
        Some(json!({ "result": {}, "jsonrpc": "2.0", "id": "p" }))
    );
    let r = rpc(
        &app,
        "docs",
        json!({ "jsonrpc": "2.0", "id": 2, "method": "resources/list" }),
    )
    .await;
    assert_eq!(
        r.body,
        Some(
            json!({ "jsonrpc": "2.0", "id": 2, "error": { "code": -32601, "message": "Method not found" } })
        )
    );
    let r = rpc(
        &app,
        "docs",
        json!({ "jsonrpc": "2.0", "id": 1.5, "method": "ping" }),
    )
    .await;
    assert_eq!(
        (r.status, r.body.unwrap()["error"]["code"].clone()),
        (400, json!(-32700))
    );

    // Batches: one answer per request, in order; a lone request is answered alone.
    let r = rpc(
        &app,
        "docs",
        json!([
            { "jsonrpc": "2.0", "id": 1, "method": "ping" },
            { "jsonrpc": "2.0", "method": "notifications/initialized" },
            { "jsonrpc": "2.0", "id": 2, "method": "nope" },
        ]),
    )
    .await;
    let b = r.body.unwrap();
    assert_eq!(
        (
            b[0]["id"].clone(),
            b[1]["error"]["code"].clone(),
            b.as_array().unwrap().len()
        ),
        (json!(1), json!(-32601), 2)
    );
    let r = rpc(
        &app,
        "docs",
        json!([{ "jsonrpc": "2.0", "id": 9, "method": "ping" }]),
    )
    .await;
    assert_eq!(
        r.body,
        Some(json!({ "result": {}, "jsonrpc": "2.0", "id": 9 }))
    );
    let many: Vec<Value> = (0..101)
        .map(|i| json!({ "jsonrpc": "2.0", "id": i, "method": "ping" }))
        .collect();
    let r = rpc(&app, "docs", Value::Array(many)).await;
    assert_eq!(
        (r.status, r.body.unwrap()["error"]["code"].clone()),
        (400, json!(-32600))
    );
    let r = rpc(
        &app,
        "docs",
        json!([init("2025-06-18"), { "jsonrpc": "2.0", "id": 2, "method": "ping" }]),
    )
    .await;
    assert_eq!(
        (r.status, r.body.unwrap()["error"]["message"].clone()),
        (
            400,
            json!("Invalid Request: Only one initialization request is allowed")
        )
    );

    // The protocol version header is checked after initialization.
    let mut headers = MCP_HEADERS.to_vec();
    headers.push(("mcp-protocol-version", "2099-01-01"));
    let ping = json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" })
        .to_string()
        .into_bytes();
    assert_eq!(
        send(&app, "POST", "/mcp/docs", &headers, ping).await.status,
        400
    );
    assert_eq!(
        send(
            &app,
            "POST",
            "/mcp/docs",
            &headers,
            init("2025-06-18").to_string().into_bytes()
        )
        .await
        .status,
        200
    );

    // Bad calls are tool errors, as in the SDK.
    let (err, text) = call(&app, "docs", "nope", json!({})).await;
    assert!(err);
    assert_eq!(text, "MCP error -32602: Tool nope not found");
    let (err, text) = call(&app, "docs", "read_document", json!({ "id": 0 })).await;
    assert!(err);
    assert_eq!(
        text,
        "MCP error -32602: Input validation error: Invalid arguments for tool read_document: Too small: expected number to be >0 at id"
    );
    let r = rpc(&app, "docs", json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "x", "task": {} } })).await;
    assert_eq!(
        r.body.unwrap()["error"]["message"],
        "Server does not support task creation (required for tools/call)"
    );
}

#[tokio::test]
async fn tools_list_matches_the_typescript_version() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/tools-list.json")).unwrap();
    let env = [
        ("IMMICH_URL", "http://immich"),
        ("IMMICH_DB_URL", "postgres://x@db/immich"),
        ("PAPERLESS_URL", "http://paperless"),
        ("PAPERLESS_USER", "pc-service"),
        ("PAPERLESS_PASSWORD", "pw"),
        ("DAWARICH_URL", "http://dawarich"),
        ("DAWARICH_DB_URL", "postgres://x@db/dawarich"),
        ("AIRTRAIL_URL", "http://airtrail"),
        ("AIRTRAIL_DB_URL", "postgres://x@db/airtrail"),
    ];
    let app = app("list", &env);
    for service in ["photos", "docs", "places", "flights"] {
        let r = rpc(
            &app,
            service,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
        )
        .await;
        let tools = &r.body.unwrap()["result"]["tools"];
        // Compared as text, so key order counts too.
        assert_eq!(tools.to_string(), fixture[service].to_string(), "{service}");
    }
}

fn paperless_answer() -> Answer {
    Box::new(|s: &Seen| {
        if s.headers.get("authorization").map(String::as_str) != Some("Basic cGMtc2VydmljZTpwdw==")
        {
            return (401, "{}".into());
        }
        let doc = |id: i64, owner: Value, extra: Value| {
            let mut d = json!({ "id": id, "title": format!("Doc {id}"), "created": "2026-05-01T00:00:00+02:00", "owner": owner });
            if let (Value::Object(d), Value::Object(e)) = (&mut d, extra) {
                d.extend(e);
            }
            d
        };
        let body = match s.path.as_str() {
            "/api/users/" => {
                json!({ "results": [{ "id": 2, "username": "anna", "email": "anna@x.ch", "groups": [7] }] })
            }
            "/api/correspondents/" => json!({ "results": [{ "id": 1, "name": "Hotel Lisboa" }] }),
            "/api/document_types/" => json!({ "results": [{ "id": 2, "name": "Invoice" }] }),
            "/api/tags/" => json!({ "results": [{ "id": 1, "name": "travel" }] }),
            "/api/documents/" if s.query.contains("query=boom") => return (500, "{}".into()),
            "/api/documents/" if s.query.contains("query=moved") => return (302, String::new()),
            "/api/documents/" => json!({ "results": [
                doc(10, json!(2), json!({ "correspondent": 1, "document_type": 2, "tags": [1, 99],
                    "__search_hit__": { "highlights": "the <span class=\"match\">hotel</span>  in Lisbon" } })),
                doc(11, json!(4), json!({})),
                doc(12, json!(4), json!({ "permissions": { "view": { "groups": [7] } } })),
            ]}),
            "/api/documents/10/" => doc(
                10,
                json!(2),
                json!({ "content": format!("Dear Anna,\n\n\n\n  the   invoice. {}", "x".repeat(7000)) }),
            ),
            "/api/documents/11/" => doc(11, json!(4), json!({ "content": "secret" })),
            _ => return (404, "{}".into()),
        };
        (200, body.to_string())
    })
}

#[tokio::test]
async fn documents_round_trip_against_a_fake_paperless() {
    let (url, seen) = fake(paperless_answer()).await;
    let env = docs_env(&url);
    let app = app("docs", &with_env(&env));

    let r = rpc(
        &app,
        "docs",
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
    )
    .await;
    let names: Vec<Value> = r.body.unwrap()["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].clone())
        .collect();
    assert_eq!(names, [json!("search_documents"), json!("read_document")]);

    let (err, v) = call(
        &app,
        "docs",
        "search_documents",
        json!({ "query": "hotel lisbon", "from": "2026-05-01", "to": "2026-05-31" }),
    )
    .await;
    assert!(!err, "{v}");
    assert_eq!(
        v.to_string(),
        r#"{"count":2,"items":[{"id":10,"title":"Doc 10","date":"2026-05-01","from":"Hotel Lisboa","type":"Invoice","tags":["travel"],"excerpt":"the hotel in Lisbon","link":"https://muster.castorcloud.ch/docs/documents/10/details"},{"id":12,"title":"Doc 12","date":"2026-05-01","link":"https://muster.castorcloud.ch/docs/documents/12/details"}]}"#
    );
    {
        let seen = seen.lock().unwrap();
        let search = seen.iter().find(|s| s.path == "/api/documents/").unwrap();
        assert_eq!(search.method, "GET");
        assert_eq!(
            search.query,
            "query=hotel+lisbon&full_perms=true&fields=id%2Ctitle%2Ccreated%2Ccreated_date%2Ccorrespondent%2Cdocument_type%2Ctags%2Cowner%2Cpermissions%2C__search_hit__&page_size=50&created__date__gte=2026-05-01&created__date__lte=2026-05-31"
        );
        assert_eq!(search.headers["accept"], "application/json");
        assert_eq!(search.headers["x-forwarded-proto"], "https");
        assert!(!search.headers.contains_key("content-type"));
        assert!(search.body.is_empty());
    }

    let (err, v) = call(
        &app,
        "docs",
        "read_document",
        json!({ "id": 10, "maxChars": 200 }),
    )
    .await;
    assert!(!err);
    assert_eq!(v["text"].as_str().unwrap().chars().count(), 200);
    assert!(
        v["text"]
            .as_str()
            .unwrap()
            .starts_with("Dear Anna,\n\n the invoice. xx")
    );
    assert_eq!(
        (v["continuesAt"].clone(), v["totalChars"].clone()),
        (json!(200), json!(7026))
    );
    let (err, v) = call(
        &app,
        "docs",
        "read_document",
        json!({ "id": 10, "offset": 7000 }),
    )
    .await;
    assert!(!err);
    assert_eq!(
        (v["text"].as_str().unwrap().len(), v.get("continuesAt")),
        (26, None)
    );

    // Someone else's document reads like a missing one.
    assert_eq!(
        call(&app, "docs", "read_document", json!({ "id": 11 })).await,
        (true, json!("not found in Documents"))
    );
    assert_eq!(
        call(&app, "docs", "read_document", json!({ "id": 99 })).await,
        (true, json!("not found in Documents"))
    );
    assert_eq!(
        call(&app, "docs", "search_documents", json!({ "query": "boom" })).await,
        (true, json!("Documents answered 500"))
    );
    assert_eq!(
        call(
            &app,
            "docs",
            "search_documents",
            json!({ "query": "moved" })
        )
        .await,
        (true, json!("Documents is not reachable right now"))
    );
    assert_eq!(
        call(
            &app,
            "docs",
            "search_documents",
            json!({ "query": "x", "from": "last tuesday" })
        )
        .await,
        (
            true,
            json!("\"last tuesday\" is not a date (use YYYY-MM-DD)")
        )
    );
    // Ben has no Paperless account.
    let r = send(&app, "POST", "/mcp/docs", &[("authorization", "Bearer token-b"), ("accept", ACCEPT), ("content-type", "application/json")],
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "search_documents", "arguments": { "query": "x" } } }).to_string().into_bytes()).await;
    assert_eq!(
        r.body.unwrap()["result"]["content"][0]["text"],
        "Documents has not been opened by this person yet, so there is nothing to search"
    );

    // Unreachable service.
    let env = docs_env("http://127.0.0.1:9");
    let down = self::app("docs-down", &with_env(&env));
    assert_eq!(
        call(&down, "docs", "search_documents", json!({ "query": "x" })).await,
        (true, json!("Documents is not reachable right now"))
    );
}

// ---- With a database -----------------------------------------------------

/// A schema of its own in the test database, for the tables a service has.
async fn test_schema(name: &str, ddl: &str) -> Option<String> {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        // CI sets REQUIRE_TEST_DATABASE so these tests can't pass by skipping.
        assert!(
            std::env::var_os("REQUIRE_TEST_DATABASE").is_none(),
            "TEST_DATABASE_URL is not set"
        );
        return None;
    };
    let schema = format!("assistant_tools_{name}");
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    sqlx::query(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(&format!("SET search_path TO {schema}; {ddl}"))
        .execute(&pool)
        .await
        .unwrap();
    let sep = if url.contains('?') { '&' } else { '?' };
    Some(format!("{url}{sep}options=-c%20search_path%3D{schema}"))
}

async fn rows(url: &str, sql: &str) -> Vec<String> {
    let pool = sqlx::PgPool::connect(url).await.unwrap();
    sqlx::query_scalar(sql).fetch_all(&pool).await.unwrap()
}

#[tokio::test]
async fn photos_keys_are_issued_and_replaced() {
    let Some(db) = test_schema(
        "photos",
        r#"CREATE TABLE "user" (id uuid PRIMARY KEY, email varchar NOT NULL, "deletedAt" timestamptz);
           CREATE TABLE api_key (id serial PRIMARY KEY, name varchar NOT NULL, key bytea NOT NULL, "userId" uuid NOT NULL REFERENCES "user"(id), permissions varchar[] NOT NULL);
           INSERT INTO "user" VALUES ('11111111-1111-1111-1111-111111111111', 'anna@x.ch', NULL), ('22222222-2222-2222-2222-222222222222', 'ben@x.ch', now());
           INSERT INTO api_key (name, key, "userId", permissions) VALUES ('Castor assistant (read-only)', '\x00', '11111111-1111-1111-1111-111111111111', '{x}'), ('phone', '\x01', '11111111-1111-1111-1111-111111111111', '{all}')"#,
    )
    .await
    else {
        return;
    };
    let keys = Arc::new(Mutex::new(Vec::new()));
    let k = keys.clone();
    let (url, seen) = fake(Box::new(move |s: &Seen| {
        k.lock().unwrap().push(s.headers.get("x-api-key").cloned().unwrap_or_default());
        match s.path.as_str() {
            "/api/search/person" => (200, json!([{ "id": "p1", "name": "Ben" }]).to_string()),
            "/api/search/smart" | "/api/search/metadata" => (200, json!({ "assets": { "items": [
                { "id": "a1", "type": "IMAGE", "localDateTime": "2026-05-02T18:30:00.000Z", "exifInfo": { "city": "Lisbon", "country": "Portugal" } }
            ], "total": 1 } }).to_string()),
            "/api/people" => (401, "{}".into()),
            _ => (404, "{}".into()),
        }
    }))
    .await;
    let env = [("IMMICH_URL", url.as_str()), ("IMMICH_DB_URL", db.as_str())];
    let app = app("photos", &env);
    let (err, v) = call(
        &app,
        "photos",
        "search_photos",
        json!({ "query": "sunset", "person": "Ben", "from": "2026-05-01", "limit": 3 }),
    )
    .await;
    assert!(!err, "{v}");
    assert_eq!(v["items"][0]["place"], "Lisbon, Portugal");
    {
        let seen = seen.lock().unwrap();
        let smart = seen.iter().find(|s| s.path == "/api/search/smart").unwrap();
        assert_eq!(smart.headers["content-type"], "application/json");
        assert_eq!(
            smart.body,
            r#"{"takenAfter":"2026-04-30T22:00:00.000Z","personIds":["p1"],"withExif":true,"withPeople":true,"size":3,"query":"sunset"}"#
        );
    }
    let used = keys.lock().unwrap().clone();
    assert!(
        used.iter().all(|k| k == &used[0]),
        "one key per member while it works"
    );
    let hashes = rows(&db, r#"SELECT encode(key, 'hex') || ' ' || array_to_string(permissions, ',') FROM api_key WHERE name = 'Castor assistant (read-only)'"#).await;
    assert_eq!(
        hashes,
        [format!(
            "{} asset.read,person.read,album.read",
            sha256_hex(&used[0])
        )]
    );
    assert_eq!(
        rows(&db, "SELECT name FROM api_key WHERE name = 'phone'")
            .await
            .len(),
        1
    );

    // A refused key is dropped and the next call issues a new one.
    assert_eq!(
        call(&app, "photos", "list_people", json!({})).await,
        (true, json!("Photos refused access"))
    );
    call(&app, "photos", "search_photos", json!({})).await;
    let used = keys.lock().unwrap().clone();
    let last = used.last().unwrap();
    assert_ne!(last, &used[0]);
    let hashes = rows(
        &db,
        r#"SELECT encode(key, 'hex') FROM api_key WHERE name = 'Castor assistant (read-only)'"#,
    )
    .await;
    assert_eq!(hashes, [sha256_hex(last)]);

    // Ben's account is deleted.
    let r = send(&app, "POST", "/mcp/photos", &[("authorization", "Bearer token-b"), ("accept", ACCEPT), ("content-type", "application/json")],
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "list_people", "arguments": {} } }).to_string().into_bytes()).await;
    assert_eq!(
        r.body.unwrap()["result"]["content"][0]["text"],
        "Photos has not been opened by this person yet, so there is nothing to search"
    );
}

#[tokio::test]
async fn flights_keys_are_issued_and_flights_listed() {
    let Some(db) = test_schema(
        "flights",
        r#"CREATE TABLE "user" (id text PRIMARY KEY, oauth_id text);
           CREATE TABLE api_key (id serial PRIMARY KEY, name text NOT NULL, user_id text NOT NULL REFERENCES "user"(id), key text NOT NULL, scopes text[] NOT NULL);
           INSERT INTO "user" VALUES ('u_anna', 'sub-anna')"#,
    )
    .await
    else {
        return;
    };
    let (url, seen) = fake(Box::new(|s: &Seen| {
        let zrh = json!({ "iata": "ZRH", "icao": "LSZH", "name": "Zurich Airport", "municipality": "Zurich", "country": "CH" });
        let lis = json!({ "iata": "LIS", "icao": "LPPT", "name": "Humberto Delgado", "municipality": "Lisbon", "country": "PT" });
        match (s.path.as_str(), s.query.contains("cursor=c2")) {
            ("/api/v1/flights", false) => (200, json!({ "data": [{ "id": 1, "date": "2025-01-10", "from": zrh, "to": lis }], "page": { "nextCursor": "c2" } }).to_string()),
            ("/api/v1/flights", true) => (200, json!({ "data": [{ "id": 2, "date": "2026-05-01", "from": lis, "to": zrh, "duration": 9900 }], "page": { "nextCursor": null } }).to_string()),
            ("/api/v1/stats", _) => (200, json!({ "data": { "flights": 2, "distanceKm": 3600.5, "durationSeconds": 19800, "airports": 2,
                "topRoute": { "from": { "name": "Zurich", "iata": "ZRH" }, "to": { "name": "Lisbon", "iata": "LIS" }, "count": 1 } } }).to_string()),
            _ => (404, "{}".into()),
        }
    }))
    .await;
    let env = [
        ("AIRTRAIL_URL", url.as_str()),
        ("AIRTRAIL_DB_URL", db.as_str()),
    ];
    let app = app("flights", &env);
    let (err, v) = call(
        &app,
        "flights",
        "list_flights",
        json!({ "airport": "lisbon" }),
    )
    .await;
    assert!(!err, "{v}");
    assert_eq!(
        v.to_string(),
        r#"{"total":2,"items":[{"date":"2026-05-01","from":"Lisbon (LIS, PT)","to":"Zurich (ZRH, CH)","minutes":165},{"date":"2025-01-10","from":"Zurich (ZRH, CH)","to":"Lisbon (LIS, PT)"}],"link":"https://muster.castorcloud.ch/flights/"}"#
    );
    let (err, v) = call(&app, "flights", "flight_stats", json!({})).await;
    assert!(!err);
    assert_eq!(
        v.to_string(),
        r#"{"flights":2,"distanceKm":3600.5,"hoursInAir":5.5,"airports":2,"topRoute":"ZRH – LIS (1×)"}"#
    );
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen[0].query, "scope=mine&limit=100");
    assert_eq!(seen[1].query, "scope=mine&limit=100&cursor=c2");
    let token = seen[0].headers["authorization"]
        .strip_prefix("Bearer ")
        .unwrap()
        .to_owned();
    use base64::Engine;
    let want = base64::engine::general_purpose::STANDARD.encode(crate::db::sha256(&token));
    let stored = rows(
        &db,
        "SELECT key || ' ' || user_id || ' ' || array_to_string(scopes, ',') FROM api_key",
    )
    .await;
    assert_eq!(
        stored,
        [format!(
            "{want} u_anna profile.read,flight.read.own,stats.read,reference_data.read"
        )]
    );
}

#[tokio::test]
async fn places_read_the_members_dawarich_key() {
    let Some(db) = test_schema(
        "places",
        r#"CREATE TABLE users (id bigserial PRIMARY KEY, email varchar NOT NULL, api_key varchar);
           INSERT INTO users (email, api_key) VALUES ('ANNA@x.ch', 'dawarich-key'), ('ben@x.ch', NULL)"#,
    )
    .await
    else {
        return;
    };
    let (url, seen) = fake(Box::new(|s: &Seen| {
        if s.headers.get("authorization").map(String::as_str) != Some("Bearer dawarich-key") {
            return (401, "{}".into());
        }
        match s.path.as_str() {
            "/api/v1/points" => (200, json!([
                { "timestamp": 1_777_622_400, "city": "Zürich", "country_name": "Switzerland" },
                { "timestamp": 1_777_640_000, "city": "Lisbon", "country_name": "Portugal" },
            ]).to_string()),
            "/api/v1/visits" => (200, json!([
                { "started_at": "2026-05-01T10:00:00.000+02:00", "ended_at": "2026-05-01T12:30:00.000+02:00", "duration": 150,
                  "name": "Café Odeon", "status": "confirmed", "place": { "latitude": "47.366123456", "longitude": 8.54559 } },
                { "started_at": "2026-05-01T13:00:00Z", "ended_at": "2026-05-01T14:00:00Z", "name": "Nope", "status": "declined" },
            ]).to_string()),
            _ => (404, "{}".into()),
        }
    }))
    .await;
    let env = [
        ("DAWARICH_URL", url.as_str()),
        ("DAWARICH_DB_URL", db.as_str()),
    ];
    let app = app("places", &env);
    let (err, v) = call(
        &app,
        "places",
        "where_was_i",
        json!({ "date": "2026-05-01" }),
    )
    .await;
    assert!(!err, "{v}");
    assert_eq!(
        v.to_string(),
        r#"{"places":[{"place":"Zürich, Switzerland","from":"2026-05-01T10:00","to":"2026-05-01T10:00"},{"place":"Lisbon, Portugal","from":"2026-05-01T14:53","to":"2026-05-01T14:53"}],"visits":[{"name":"Café Odeon","from":"2026-05-01T10:00","to":"2026-05-01T12:30"}],"link":"https://muster.castorcloud.ch/places/map?start_at=2026-05-01&end_at=2026-05-01"}"#
    );
    let (_, v) = call(
        &app,
        "places",
        "list_visits",
        json!({ "from": "2026-05-01", "to": "2026-05-02" }),
    )
    .await;
    assert_eq!(
        v["items"][0].to_string(),
        r#"{"name":"Café Odeon","from":"2026-05-01T10:00","to":"2026-05-01T12:30","minutes":150,"lat":47.3661,"lon":8.5456}"#
    );
    {
        let seen = seen.lock().unwrap();
        let points = seen.iter().find(|s| s.path == "/api/v1/points").unwrap();
        assert_eq!(
            points.query,
            "start_at=1777586400&end_at=1777672799&per_page=5000&order=asc"
        );
    }
    assert_eq!(
        call(
            &app,
            "places",
            "where_was_i",
            json!({ "from": "2020-01-01", "to": "2026-01-01" })
        )
        .await,
        (true, json!("ask about at most about a year at a time"))
    );
    let r = send(&app, "POST", "/mcp/places", &[("authorization", "Bearer token-b"), ("accept", ACCEPT), ("content-type", "application/json")],
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "travel_stats", "arguments": {} } }).to_string().into_bytes()).await;
    assert_eq!(
        r.body.unwrap()["result"]["content"][0]["text"],
        "Timeline has not been opened by this person yet, so there is nothing to search"
    );
}
