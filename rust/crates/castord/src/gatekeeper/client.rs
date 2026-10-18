//! The node role's MCP client for gatekeeper's /mcp (Streamable HTTP):
//! initialize once, then one tools/call per operation. Answers come as JSON or as an
//! SSE stream; both are read.
//!
//! The bearer token comes from a file (a Docker secret), never from the
//! environment, where `docker inspect` and /proc/<pid>/environ show it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::Mutex;
use zeroize::Zeroizing;

use crate::exec::ToolCall;

const PROTOCOL: &str = "2025-06-18";
/// The longest tool runs 15 minutes (gatekeeper's timeout_seconds).
const CALL_TIMEOUT: Duration = Duration::from_secs(16 * 60);
const MAX_ANSWER: usize = 8 * 1024 * 1024;

pub struct Gatekeeper {
    url: String,
    token: Zeroizing<String>,
    http: reqwest::Client,
    session: Mutex<Option<String>>,
    next_id: AtomicU64,
}

/// What a tool printed.
#[derive(Debug, PartialEq)]
pub struct Output {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i64>,
}

/// http only to a name on a Docker network or to loopback; anything else https.
pub fn check_url(url: &str) -> Result<(), String> {
    let bad = || format!("GATEKEEPER_URL {url}: https, or http to a Docker network name");
    let (scheme, rest) = url.split_once("://").ok_or_else(bad)?;
    let host = rest.split(['/', ':']).next().unwrap_or("");
    if host.is_empty() || rest.contains('@') {
        return Err(bad());
    }
    match scheme {
        "https" => Ok(()),
        "http" if !host.contains('.') || host == "127.0.0.1" => Ok(()),
        _ => Err(bad()),
    }
}

impl Gatekeeper {
    pub fn new(url: &str, token: Zeroizing<String>, http: reqwest::Client) -> Result<Self, String> {
        check_url(url)?;
        let t = token.trim();
        if t.is_empty() || !t.bytes().all(|b| b.is_ascii_graphic()) {
            return Err("gatekeeper token: empty or not one line".into());
        }
        Ok(Self {
            url: url.to_owned(),
            token: Zeroizing::new(t.to_owned()),
            http,
            session: Mutex::new(None),
            next_id: AtomicU64::new(1),
        })
    }

    /// Runs one tool; its stdout, or an error with what it reported.
    pub async fn call(&self, call: ToolCall) -> Result<String, String> {
        let name = call.name.clone();
        let out = self.call_output(call).await.map_err(|e| format!("{name}: {e}"))?;
        Ok(out.stdout)
    }

    pub async fn call_output(&self, call: ToolCall) -> Result<Output, String> {
        let body = self.tools_call_body(&call)?;
        let id = body.1;
        let answer = match self.post_in_session(&body.0, id).await {
            // gatekeeper forgets idle sessions after an hour: start a new one once.
            Err(Retry::NewSession) => {
                *self.session.lock().await = None;
                self.post_in_session(&body.0, id).await
            }
            r => r,
        }
        .map_err(|e| match e {
            Retry::NewSession => "session expired".to_owned(),
            Retry::Failed(m) => m,
        })?;
        result(&answer)
    }

    /// The tools/call request; the secret is written into a buffer that is
    /// wiped afterwards (reqwest keeps its own copy until the request is sent).
    fn tools_call_body(&self, call: &ToolCall) -> Result<(Zeroizing<Vec<u8>>, u64), String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let args = Value::Object(call.args.clone()).to_string();
        let head =
            json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": call.name}}).to_string();
        // {"jsonrpc":..,"params":{"name":".."}} -> insert ,"arguments":{..} before the last two braces.
        let Some(open) = head.strip_suffix("}}") else {
            return Err("tools/call: unexpected request shape".into());
        };
        let secret_len = call.secret.as_ref().map_or(0, |(_, s)| s.len() * 6 + 64);
        let mut buf = Zeroizing::new(Vec::with_capacity(head.len() + args.len() + secret_len + 32));
        buf.extend_from_slice(open.as_bytes());
        buf.extend_from_slice(b",\"arguments\":");
        match &call.secret {
            None => buf.extend_from_slice(args.as_bytes()),
            Some((key, value)) => {
                if call.args.contains_key(*key) {
                    return Err(format!("{key} given twice"));
                }
                let inner = args.strip_suffix('}').ok_or("arguments")?;
                buf.extend_from_slice(inner.as_bytes());
                if inner.len() > 1 {
                    buf.push(b',');
                }
                serde_json::to_writer(&mut *buf, key).map_err(|e| e.to_string())?;
                buf.push(b':');
                serde_json::to_writer(&mut *buf, value.as_str()).map_err(|e| e.to_string())?;
                buf.push(b'}');
            }
        }
        buf.extend_from_slice(b"}}");
        Ok((buf, id))
    }

    async fn post_in_session(&self, body: &[u8], id: u64) -> Result<Value, Retry> {
        let session = self.session().await.map_err(Retry::Failed)?;
        let (status, answer) = self.post(body, Some(&session), Some(id)).await.map_err(Retry::Failed)?;
        if status == 404 {
            return Err(Retry::NewSession);
        }
        if !(200..300).contains(&status) {
            return Err(Retry::Failed(format!("gatekeeper answered {status}")));
        }
        answer.ok_or_else(|| Retry::Failed("no answer".into()))
    }

    async fn session(&self) -> Result<String, String> {
        let mut held = self.session.lock().await;
        if let Some(s) = held.as_ref() {
            return Ok(s.clone());
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let init = json!({
            "jsonrpc": "2.0", "id": id, "method": "initialize",
            "params": {
                "protocolVersion": PROTOCOL,
                "capabilities": {},
                "clientInfo": {"name": "castor-node", "version": env!("CARGO_PKG_VERSION")},
            },
        });
        let resp = self
            .request(init.to_string().as_bytes(), None)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        if status == 401 || status == 403 {
            return Err(format!("gatekeeper refused the token ({status})"));
        }
        let session = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .ok_or_else(|| format!("gatekeeper initialize: no session ({status})"))?;
        let answer = read(resp, id).await?.ok_or("gatekeeper initialize: no answer")?;
        if let Some(e) = answer.get("error") {
            return Err(format!("gatekeeper initialize: {e}"));
        }
        let note = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        let (status, _) = self.post(note.to_string().as_bytes(), Some(&session), None).await?;
        if !(200..300).contains(&status) {
            return Err(format!("gatekeeper initialized: {status}"));
        }
        *held = Some(session.clone());
        Ok(session)
    }

    fn request(&self, body: &[u8], session: Option<&str>) -> reqwest::RequestBuilder {
        let mut r = self
            .http
            .post(&self.url)
            .timeout(CALL_TIMEOUT)
            .bearer_auth(self.token.as_str())
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(body.to_vec());
        if let Some(s) = session {
            r = r.header("mcp-session-id", s).header("mcp-protocol-version", PROTOCOL);
        }
        r
    }

    async fn post(&self, body: &[u8], session: Option<&str>, id: Option<u64>) -> Result<(u16, Option<Value>), String> {
        let resp = self.request(body, session).send().await.map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            return Ok((status, None));
        }
        match id {
            None => Ok((status, None)),
            Some(id) => Ok((status, read(resp, id).await?)),
        }
    }
}

enum Retry {
    NewSession,
    Failed(String),
}

/// The JSON-RPC response with this id, from a JSON body or an SSE stream.
async fn read(resp: reqwest::Response, id: u64) -> Result<Option<Value>, String> {
    let sse = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"));
    let mut resp = resp;
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_ANSWER {
            return Err("gatekeeper answer too large".into());
        }
        if sse && let Some(v) = find_in_sse(&body, id) {
            return Ok(Some(v));
        }
    }
    if body.is_empty() {
        return Ok(None);
    }
    if sse {
        return Ok(find_in_sse(&body, id));
    }
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| format!("gatekeeper answer: {e}"))
}

/// SSE events are separated by a blank line; `data:` lines make up the message.
pub fn find_in_sse(body: &[u8], id: u64) -> Option<Value> {
    let text = String::from_utf8_lossy(body).replace("\r\n", "\n");
    let complete = &text[..text.rfind("\n\n")? + 2];
    for event in complete.split("\n\n") {
        let data: Vec<&str> = event
            .lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .map(|d| d.strip_prefix(' ').unwrap_or(d))
            .collect();
        if data.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(&data.join("\n"))
            && v.get("id").and_then(Value::as_u64) == Some(id)
        {
            return Some(v);
        }
    }
    None
}

/// A tools/call response: its output, or the error gatekeeper or the tool reported.
pub fn result(answer: &Value) -> Result<Output, String> {
    if let Some(e) = answer.get("error") {
        let msg = e.get("message").and_then(Value::as_str).unwrap_or("error");
        return Err(format!("gatekeeper: {msg}"));
    }
    let r = answer.get("result").ok_or("gatekeeper: no result")?;
    let text: String = r
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    let is_error = r.get("isError").and_then(Value::as_bool).unwrap_or(false);
    if let Some(s) = r.get("structuredContent").filter(|s| s.get("outcome").is_some()) {
        let field = |k: &str| s.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
        let out = Output {
            stdout: field("stdout"),
            stderr: field("stderr"),
            exit_code: s.get("exit_code").and_then(Value::as_i64),
        };
        return match field("outcome").as_str() {
            "ok" if !is_error => Ok(out),
            "unknown" => Err(format!("outcome unknown (timed out): {}", tail(&out.stderr))),
            _ if out.stderr.trim().is_empty() => Err(tail(&text)),
            _ => Err(tail(&out.stderr)),
        };
    }
    // gatekeeper's text rendering: "exit=N\n<stdout>\n<stderr>", or the output alone.
    if is_error {
        return Err(tail(&text));
    }
    if text.starts_with("OUTCOME UNKNOWN") {
        return Err(format!("outcome unknown (timed out): {}", tail(&text)));
    }
    let (exit_code, body) = match text.split_once('\n') {
        Some((head, body)) if head.starts_with("exit=") => {
            (head[5..].split(' ').next().and_then(|n| n.parse().ok()), body)
        }
        _ if text.starts_with("exit=") => (text[5..].split(' ').next().and_then(|n| n.parse().ok()), ""),
        _ => (None, text.as_str()),
    };
    let stdout = if body == "(no output)" {
        String::new()
    } else {
        body.to_owned()
    };
    Ok(Output {
        stdout,
        stderr: String::new(),
        exit_code,
    })
}

fn tail(s: &str) -> String {
    let s = s.trim();
    let mut start = s.len().saturating_sub(2000);
    while !s.is_char_boundary(start) {
        start += 1;
    }
    s[start..].to_owned()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn urls() {
        assert!(check_url("http://gatekeeper-castor:8080/mcp").is_ok());
        assert!(check_url("https://gk.lan/mcp").is_ok());
        assert!(check_url("http://127.0.0.1:8080/mcp").is_ok());
        assert!(check_url("http://gk.lan/mcp").is_err());
        assert!(check_url("http://user:pw@gk/mcp").is_err());
        assert!(check_url("ftp://gk/mcp").is_err());
    }

    #[test]
    fn results() {
        let r = |v: Value| result(&json!({"jsonrpc": "2.0", "id": 1, "result": v}));
        let text = |t: &str, e: bool| json!({"content": [{"type": "text", "text": t}], "isError": e});
        assert_eq!(r(text("exit=0\n[]", false)).unwrap().stdout, "[]");
        assert_eq!(r(text("(no output)", false)).unwrap().stdout, "");
        assert_eq!(r(text("exit=0", false)).unwrap().exit_code, Some(0));
        assert_eq!(r(text("exit=1\nboom", true)).unwrap_err(), "exit=1\nboom");
        assert!(
            r(text("OUTCOME UNKNOWN", false))
                .unwrap_err()
                .starts_with("outcome unknown")
        );
        let structured = json!({"content": [], "isError": false,
            "structuredContent": {"outcome": "ok", "exit_code": 0, "stdout": "[1]", "stderr": "pulling"}});
        assert_eq!(
            r(structured).unwrap(),
            Output {
                stdout: "[1]".into(),
                stderr: "pulling".into(),
                exit_code: Some(0)
            }
        );
        let failed = json!({"content": [{"type": "text", "text": "exit=1"}], "isError": true,
            "structuredContent": {"outcome": "failed", "exit_code": 1, "stdout": "", "stderr": "no such network"}});
        assert_eq!(r(failed).unwrap_err(), "no such network");
        let denied = json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32602, "message": "Unknown tool"}});
        assert_eq!(result(&denied).unwrap_err(), "gatekeeper: Unknown tool");
    }

    #[test]
    fn sse() {
        let body = b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"x\"}\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{}}\n\n";
        assert_eq!(find_in_sse(body, 7).unwrap()["id"], 7);
        assert!(find_in_sse(body, 8).is_none());
        assert!(find_in_sse(b"data: {\"id\":7}", 7).is_none());
    }

    #[test]
    fn the_secret_goes_into_the_arguments() {
        let g = Gatekeeper::new(
            "http://gk:8080/mcp",
            Zeroizing::new("gk_x\n".into()),
            reqwest::Client::new(),
        )
        .unwrap();
        let mut args = serde_json::Map::new();
        args.insert("path".into(), "/mnt/t/a".into());
        let call = ToolCall {
            name: "file.tenant_write".into(),
            args,
            secret: Some(("content", Zeroizing::new("A=\"1\"\n".into()))),
        };
        let (body, id) = g.tools_call_body(&call).unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["id"], id);
        assert_eq!(
            v["params"]["arguments"],
            json!({"path": "/mnt/t/a", "content": "A=\"1\"\n"})
        );
        let empty = ToolCall {
            name: "docker.version".into(),
            args: serde_json::Map::new(),
            secret: Some(("k", Zeroizing::new("v".into()))),
        };
        let (body, _) = g.tools_call_body(&empty).unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["params"], json!({"name": "docker.version", "arguments": {"k": "v"}}));
        assert_eq!(
            call.label(),
            r#"file.tenant_write {"path":"/mnt/t/a","content":"[secret]"}"#
        );
    }
}
