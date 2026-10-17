//! MCP over stateless Streamable HTTP with JSON responses, the way the
//! TypeScript version ran it (@modelcontextprotocol/sdk 1.30 with
//! `sessionIdGenerator: undefined, enableJsonResponse: true`).
//!
//! Hand-rolled on purpose: a stateless JSON-only server needs initialize,
//! ping, tools/list and tools/call, which is far less code than an MCP SDK
//! (rmcp brings sessions, SSE, a macro layer and many dependencies) and lets
//! the wire format stay exactly as the SDK wrote it: message validation, error
//! codes and texts, batch handling and protocol version negotiation.

use futures_util::future::join_all;
use serde_json::{Map, Value, json};

use crate::limits::{MAX_SAFE_INTEGER, ToolError};
use crate::schema::type_name;
use crate::services::{Ctx, Key, Services};

pub const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";
pub const SUPPORTED_PROTOCOL_VERSIONS: [&str; 5] = [
    LATEST_PROTOCOL_VERSION,
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
    "2024-10-07",
];
/// Messages in one batch.
pub const MAX_BATCH: usize = 100;

/// An HTTP answer: status and JSON body (None: no body).
pub type Reply = (u16, Option<Value>);

/// A transport error, answered with HTTP status `status` and no request id.
pub fn transport_error(status: u16, code: i64, message: &str) -> Reply {
    (
        status,
        Some(
            json!({ "jsonrpc": "2.0", "error": { "code": code, "message": message }, "id": null }),
        ),
    )
}

fn is_request_id(v: Option<&Value>) -> bool {
    match v {
        Some(Value::String(_)) => true,
        Some(Value::Number(n)) => n
            .as_f64()
            .is_some_and(|f| f.fract() == 0.0 && f.abs() <= MAX_SAFE_INTEGER),
        _ => false,
    }
}

/// `_meta` of params and results: progressToken and related-task, if given,
/// must have their types.
fn valid_meta(params: &Map<String, Value>) -> bool {
    let Some(meta) = params.get("_meta") else {
        return true;
    };
    let Value::Object(meta) = meta else {
        return false;
    };
    let token_ok = meta
        .get("progressToken")
        .is_none_or(|t| is_request_id(Some(t)));
    let task_ok = meta
        .get("io.modelcontextprotocol/related-task")
        .is_none_or(|t| match t {
            Value::Object(t) => matches!(t.get("taskId"), Some(Value::String(_))),
            _ => false,
        });
    token_ok && task_ok
}

fn valid_params(m: &Map<String, Value>) -> bool {
    match m.get("params") {
        None => true,
        Some(Value::Object(p)) => valid_meta(p),
        Some(_) => false,
    }
}

pub enum Message {
    Request { id: Value, method: String },
    Notification,
    Response,
}

/// One JSON-RPC message, checked like the SDK's JSONRPCMessageSchema (keys
/// are strict: a request has exactly jsonrpc, id, method and maybe params).
pub fn parse_message(v: &Value) -> Option<Message> {
    let Value::Object(m) = v else {
        return None;
    };
    if m.get("jsonrpc") != Some(&Value::from("2.0")) {
        return None;
    }
    let only = |keys: &[&str]| m.keys().all(|k| keys.contains(&k.as_str()));
    let method = match m.get("method") {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    };
    if let Some(method) = &method {
        if only(&["jsonrpc", "id", "method", "params"])
            && is_request_id(m.get("id"))
            && valid_params(m)
        {
            return Some(Message::Request {
                id: m.get("id").cloned().unwrap_or_default(),
                method: method.clone(),
            });
        }
        if only(&["jsonrpc", "method", "params"]) && valid_params(m) {
            return Some(Message::Notification);
        }
        return None;
    }
    if only(&["jsonrpc", "id", "result"]) && is_request_id(m.get("id")) {
        if let Some(Value::Object(r)) = m.get("result") {
            return valid_meta(r).then_some(Message::Response);
        }
        return None;
    }
    if only(&["jsonrpc", "id", "error"])
        && (m.get("id").is_none() || is_request_id(m.get("id")))
        && let Some(Value::Object(e)) = m.get("error")
    {
        let code_ok =
            is_request_id(e.get("code")) && matches!(e.get("code"), Some(Value::Number(_)));
        return (code_ok && matches!(e.get("message"), Some(Value::String(_))))
            .then_some(Message::Response);
    }
    None
}

/// A zod "invalid_type" issue, as the SDK reports bad params.
struct Issues(Vec<Value>);

impl Issues {
    fn expect(&mut self, path: &[&str], expected: &str, got: Option<&Value>) {
        self.0.push(json!({
            "expected": expected,
            "code": "invalid_type",
            "path": path,
            "message": format!("Invalid input: expected {expected}, received {}", type_name(got)),
        }));
    }
    fn string(&mut self, path: &[&str], v: Option<&Value>, optional: bool) {
        if !(matches!(v, Some(Value::String(_))) || (optional && v.is_none())) {
            self.expect(path, "string", v);
        }
    }
    /// Checks `v` is an object; returns it when it is.
    fn object<'a>(
        &mut self,
        path: &[&str],
        v: Option<&'a Value>,
        expected: &str,
    ) -> Option<&'a Map<String, Value>> {
        match v {
            Some(Value::Object(o)) => Some(o),
            _ => {
                self.expect(path, expected, v);
                None
            }
        }
    }
    /// ZodError's message: the issues as indented JSON.
    fn result(self) -> Result<(), String> {
        if self.0.is_empty() {
            return Ok(());
        }
        Err(serde_json::to_string_pretty(&self.0).unwrap_or_default())
    }
}

/// InitializeRequestSchema's params (the client's capabilities are not used
/// here, so only their being an object is checked).
fn check_initialize(params: Option<&Value>) -> Result<(), String> {
    let mut i = Issues(Vec::new());
    if let Some(p) = i.object(&["params"], params, "object") {
        i.string(
            &["params", "protocolVersion"],
            p.get("protocolVersion"),
            false,
        );
        i.object(&["params", "capabilities"], p.get("capabilities"), "object");
        if let Some(c) = i.object(&["params", "clientInfo"], p.get("clientInfo"), "object") {
            i.string(&["params", "clientInfo", "name"], c.get("name"), false);
            i.string(&["params", "clientInfo", "title"], c.get("title"), true);
            i.string(
                &["params", "clientInfo", "version"],
                c.get("version"),
                false,
            );
            i.string(
                &["params", "clientInfo", "websiteUrl"],
                c.get("websiteUrl"),
                true,
            );
            i.string(
                &["params", "clientInfo", "description"],
                c.get("description"),
                true,
            );
        }
    }
    i.result()
}

/// isInitializeRequest: any message (even a notification) with method
/// "initialize" and valid initialize params.
pub fn is_initialize(v: &Value) -> bool {
    v.get("method") == Some(&Value::from("initialize")) && check_initialize(v.get("params")).is_ok()
}

fn check_list(params: Option<&Value>) -> Result<(), String> {
    let mut i = Issues(Vec::new());
    if let Some(Value::Object(p)) = params {
        i.string(&["params", "cursor"], p.get("cursor"), true);
    }
    i.result()
}

fn check_call(params: Option<&Value>) -> Result<(), String> {
    let mut i = Issues(Vec::new());
    if let Some(p) = i.object(&["params"], params, "object") {
        if let Some(task) = p.get("task")
            && let Some(t) = i.object(&["params", "task"], Some(task), "object")
            && let Some(ttl) = t.get("ttl").filter(|v| !v.is_number())
        {
            i.expect(&["params", "task", "ttl"], "number", Some(ttl));
        }
        i.string(&["params", "name"], p.get("name"), false);
        if let Some(a) = p.get("arguments").filter(|a| !a.is_object()) {
            i.expect(&["params", "arguments"], "record", Some(a));
        }
    }
    i.result()
}

/// Params asking for a task (an object `task` with a numeric ttl if any):
/// this server supports none.
fn asks_for_task(params: Option<&Value>) -> bool {
    match params.and_then(|p| p.get("task")) {
        Some(Value::Object(t)) => t.get("ttl").is_none_or(Value::is_number),
        _ => false,
    }
}

pub struct Session<'a> {
    pub key: Key,
    pub services: &'a Services,
    pub ctx: Ctx<'a>,
}

fn result(id: &Value, result: Value) -> Value {
    json!({ "result": result, "jsonrpc": "2.0", "id": id })
}

fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn text(text: String, is_error: bool) -> Value {
    let mut r = json!({ "content": [{ "type": "text", "text": text }] });
    if is_error {
        r["isError"] = true.into();
    }
    r
}

impl Session<'_> {
    /// Answers one request.
    async fn request(&self, id: &Value, method: &str, params: Option<&Value>) -> Value {
        let check: fn(Option<&Value>) -> Result<(), String> = match method {
            "initialize" => check_initialize,
            "ping" => |_| Ok(()),
            "tools/list" => check_list,
            "tools/call" => check_call,
            _ => return error(id, -32601, "Method not found"),
        };
        if asks_for_task(params) {
            return error(
                id,
                -32603,
                &format!("Server does not support task creation (required for {method})"),
            );
        }
        if let Err(issues) = check(params) {
            return error(id, -32603, &issues);
        }
        let p = params.and_then(Value::as_object);
        match method {
            "initialize" => {
                let requested = p
                    .and_then(|p| p.get("protocolVersion"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let version = if SUPPORTED_PROTOCOL_VERSIONS.contains(&requested) {
                    requested
                } else {
                    LATEST_PROTOCOL_VERSION
                };
                result(
                    id,
                    json!({
                        "protocolVersion": version,
                        "capabilities": { "tools": { "listChanged": true } },
                        "serverInfo": { "name": format!("castor-{}", self.key.as_str()), "version": "1.0.0" },
                    }),
                )
            }
            "ping" => result(id, json!({})),
            "tools/list" => {
                let tools = self.services.tools(self.key).unwrap_or_default();
                result(
                    id,
                    json!({ "tools": tools.iter().map(|t| t.definition()).collect::<Vec<_>>() }),
                )
            }
            _ => {
                let name = p
                    .and_then(|p| p.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                result(
                    id,
                    self.call_tool(name, p.and_then(|p| p.get("arguments")))
                        .await,
                )
            }
        }
    }

    /// A tools/call result; failures are results with isError, as in the SDK.
    async fn call_tool(&self, name: &str, args: Option<&Value>) -> Value {
        let tools = self.services.tools(self.key).unwrap_or_default();
        let Some(tool) = tools.iter().find(|t| t.name == name) else {
            return text(format!("MCP error -32602: Tool {name} not found"), true);
        };
        let args = match tool.validate(args) {
            Ok(a) => a,
            Err(e) => {
                return text(
                    format!(
                        "MCP error -32602: Input validation error: Invalid arguments for tool {name}: {e}"
                    ),
                    true,
                );
            }
        };
        // Expected failures become a short message for the model.
        match self.services.call(self.key, &self.ctx, name, &args).await {
            Ok(v) => text(v.to_string(), false),
            Err(ToolError::User(m)) => text(m, true),
            Err(ToolError::Internal(m)) => {
                eprintln!("{} {name}: {m}", self.key.as_str());
                text("internal error in the tool".into(), true)
            }
        }
    }

    /// Handles a POST body (already parsed JSON) after the HTTP checks.
    /// `protocol_version` is the Mcp-Protocol-Version header, if sent.
    pub async fn handle(&self, body: Option<Value>, protocol_version: Option<&str>) -> Reply {
        let Some(body) = body else {
            return transport_error(400, -32700, "Parse error: Invalid JSON");
        };
        let messages: Vec<Value> = match body {
            Value::Array(a) if a.len() > MAX_BATCH => {
                return transport_error(
                    400,
                    -32600,
                    &format!("Invalid Request: Batch must not exceed {MAX_BATCH} messages"),
                );
            }
            Value::Array(a) => a,
            other => vec![other],
        };
        let Some(parsed) = messages
            .iter()
            .map(parse_message)
            .collect::<Option<Vec<_>>>()
        else {
            return transport_error(400, -32700, "Parse error: Invalid JSON-RPC message");
        };
        let init = messages.iter().any(is_initialize);
        if init && messages.len() > 1 {
            return transport_error(
                400,
                -32600,
                "Invalid Request: Only one initialization request is allowed",
            );
        }
        if let Some(v) =
            protocol_version.filter(|v| !init && !SUPPORTED_PROTOCOL_VERSIONS.contains(v))
        {
            let msg = format!(
                "Bad Request: Unsupported protocol version: {v} (supported versions: {})",
                SUPPORTED_PROTOCOL_VERSIONS.join(", ")
            );
            return transport_error(400, -32000, &msg);
        }
        // Notifications (notifications/initialized and others) and responses
        // need no answer.
        let requests: Vec<(&Value, &str, Option<&Value>)> = parsed
            .iter()
            .zip(&messages)
            .filter_map(|(p, m)| match p {
                Message::Request { id, method } => Some((id, method.as_str(), m.get("params"))),
                Message::Notification | Message::Response => None,
            })
            .collect();
        if requests.is_empty() {
            return (202, None);
        }
        let answers = join_all(
            requests
                .iter()
                .map(|(id, method, params)| self.request(id, method, *params)),
        )
        .await;
        // One answer per request id (the SDK keeps the first of duplicates).
        let mut seen: Vec<&Value> = Vec::new();
        let mut out = Vec::new();
        for ((id, _, _), answer) in requests.iter().zip(answers) {
            if !seen.contains(id) {
                seen.push(id);
                out.push(answer);
            }
        }
        let body = if out.len() == 1 {
            out.into_iter().next()
        } else {
            Some(Value::Array(out))
        };
        (200, body)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_checked_like_the_sdk() {
        let ok = |v: Value| parse_message(&v).is_some();
        assert!(ok(json!({"jsonrpc": "2.0", "id": 1, "method": "ping"})));
        assert!(ok(
            json!({"jsonrpc": "2.0", "id": "a", "method": "ping", "params": {"_meta": {"progressToken": "t"}}})
        ));
        assert!(ok(
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
        ));
        assert!(ok(json!({"jsonrpc": "2.0", "id": 1, "result": {}})));
        assert!(ok(
            json!({"jsonrpc": "2.0", "error": {"code": 1, "message": "x"}})
        ));
        assert!(!ok(json!({"jsonrpc": "2.0", "id": 1.5, "method": "ping"})));
        assert!(!ok(json!({"jsonrpc": "2.0", "id": null, "method": "ping"})));
        assert!(!ok(
            json!({"jsonrpc": "2.0", "id": 12_345_678_901_234_567_890_u64, "method": "ping"})
        ));
        assert!(!ok(json!({"jsonrpc": "1.0", "id": 1, "method": "ping"})));
        assert!(!ok(
            json!({"jsonrpc": "2.0", "id": 1, "method": "ping", "extra": true})
        ));
        assert!(!ok(
            json!({"jsonrpc": "2.0", "id": 1, "method": "ping", "params": [1]})
        ));
        assert!(!ok(
            json!({"jsonrpc": "2.0", "id": 1, "method": "ping", "params": {"_meta": 5}})
        ));
        assert!(!ok(
            json!({"jsonrpc": "2.0", "id": 1, "method": "ping", "params": {"_meta": {"progressToken": 1.5}}})
        ));
        assert!(!ok(
            json!({"jsonrpc": "2.0", "method": "x", "params": null})
        ));
        assert!(!ok(json!({"jsonrpc": "2.0", "id": 1, "result": 5})));
        assert!(!ok(
            json!({"jsonrpc": "2.0", "id": 1, "error": {"code": 1.5, "message": "x"}})
        ));
        assert!(!ok(json!([])));
        assert!(!ok(json!(null)));
    }

    #[test]
    fn bad_params_are_reported_as_zod_issues() {
        assert_eq!(
            check_call(None).unwrap_err(),
            "[\n  {\n    \"expected\": \"object\",\n    \"code\": \"invalid_type\",\n    \"path\": [\n      \"params\"\n    ],\n    \"message\": \"Invalid input: expected object, received undefined\"\n  }\n]"
        );
        assert!(
            check_call(Some(&json!({"name": "x", "arguments": null})))
                .unwrap_err()
                .contains("expected record, received null")
        );
        assert!(check_call(Some(&json!({"name": "x"}))).is_ok());
        assert!(check_initialize(Some(&json!({"protocolVersion": "x", "capabilities": {}, "clientInfo": {"name": "h", "version": "1"}}))).is_ok());
        let e = check_initialize(Some(&json!({}))).unwrap_err();
        assert!(e.find("protocolVersion").unwrap() < e.find("capabilities").unwrap());
        assert!(asks_for_task(Some(&json!({"task": {"ttl": 1000}}))));
        assert!(asks_for_task(Some(&json!({"task": {}}))));
        assert!(!asks_for_task(Some(&json!({"task": {"ttl": "x"}}))));
        assert!(!asks_for_task(Some(&json!({"task": 5}))));
    }
}
