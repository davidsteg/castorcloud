//! The HTTP side: /healthz, and one MCP endpoint per booked service
//! (POST /mcp/photos, /mcp/docs, ...), so Hermes shows them as separate
//! servers. Stateless: every request is authenticated on its own and gets
//! tools bound to its member.

use std::time::Duration;

use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Body, Bytes};
use hyper::header::{CONTENT_TYPE, HeaderValue};
use hyper::{HeaderMap, Method, Request, Response, StatusCode};
use reqwest::{Client, Url};
use serde_json::{Value, json};

use crate::limits::{Zone, js_trim};
use crate::mcp::{Reply, Session, transport_error};
use crate::members::{load_members, member_for_token};
use crate::services::{Ctx, Key, Services};

pub const MAX_BODY: usize = 256 * 1024;
/// Time to send the request body (Node's default requestTimeout).
const BODY_TIMEOUT: Duration = Duration::from_secs(300);

pub struct App {
    pub members_file: String,
    pub portal_origin: String,
    pub services: Services,
    pub http: Client,
    pub zone: Zone,
}

fn reply(status: u16, body: Option<&Value>) -> Response<Full<Bytes>> {
    let mut res = Response::new(Full::new(
        body.map(|b| Bytes::from(b.to_string())).unwrap_or_default(),
    ));
    *res.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    if body.is_some() {
        res.headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    }
    res
}

fn error(status: u16, message: &str) -> Response<Full<Bytes>> {
    reply(status, Some(&json!({ "error": message })))
}

fn latin1(v: &HeaderValue) -> String {
    v.as_bytes().iter().map(|&b| char::from(b)).collect()
}

/// A header as the Fetch API's Headers.get gives it: all values joined.
fn joined(headers: &HeaderMap, name: &str) -> Option<String> {
    let all: Vec<String> = headers.get_all(name).iter().map(latin1).collect();
    (!all.is_empty()).then(|| all.join(", "))
}

/// The path as `new URL(req.url, "http://tools").pathname` gives it.
fn pathname(target: &str) -> Option<String> {
    let base = Url::parse("http://tools/").ok()?;
    Some(base.join(target).ok()?.path().to_owned())
}

/// The service of /mcp/<service> (lowercase letters, optional trailing slash).
fn service_of(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("/mcp/")?;
    let name = rest.strip_suffix('/').unwrap_or(rest);
    (!name.is_empty() && name.bytes().all(|b| b.is_ascii_lowercase())).then_some(name)
}

/// One `; key=value` parameter of a media type at `i`, as the content-type
/// package's PARAM_REGEXP matches it; returns the index after it.
fn media_param(h: &[char], mut i: usize) -> Option<usize> {
    let token = |c: char| c.is_ascii_alphanumeric() || "!#$%&'*+.^_`|~-".contains(c);
    let spaces = |i: &mut usize| {
        while h.get(*i) == Some(&' ') {
            *i += 1;
        }
    };
    if h.get(i) != Some(&';') {
        return None;
    }
    i += 1;
    spaces(&mut i);
    let start = i;
    while h.get(i).is_some_and(|c| token(*c)) {
        i += 1;
    }
    if i == start {
        return None;
    }
    spaces(&mut i);
    if h.get(i) != Some(&'=') {
        return None;
    }
    i += 1;
    spaces(&mut i);
    if h.get(i) == Some(&'"') {
        i += 1;
        loop {
            match h.get(i)? {
                '"' => break,
                '\\' => {
                    let c = *h.get(i + 1)?;
                    if !(c == '\u{b}' || ('\u{20}'..='\u{ff}').contains(&c)) {
                        return None;
                    }
                    i += 2;
                }
                c if *c == '\u{b}'
                    || *c == ' '
                    || *c == '!'
                    || ('\u{23}'..='\u{5b}').contains(c)
                    || ('\u{5d}'..='\u{7e}').contains(c)
                    || ('\u{80}'..='\u{ff}').contains(c) =>
                {
                    i += 1
                }
                _ => return None,
            }
        }
        i += 1;
    } else {
        let start = i;
        while h.get(i).is_some_and(|c| token(*c)) {
            i += 1;
        }
        if i == start {
            return None;
        }
    }
    spaces(&mut i);
    Some(i)
}

/// The SDK's isJsonContentType: the media type is application/json.
fn is_json_content_type(header: Option<&str>) -> bool {
    let Some(header) = header else {
        return false;
    };
    if header == "application/json" {
        return true;
    }
    let chars: Vec<char> = header.chars().collect();
    let semi = chars.iter().position(|c| *c == ';');
    let first: String = chars[..semi.unwrap_or(chars.len())].iter().collect();
    let essence = js_trim(&first).to_lowercase();
    if essence != "application/json" {
        return false;
    }
    // Parsed strictly first; when that fails, the essence still counts
    // unless a comma suggests joined duplicate headers.
    let mut parsed = true;
    if let Some(mut i) = semi {
        while i < chars.len() {
            match media_param(&chars, i) {
                Some(next) => i = next,
                None => {
                    parsed = false;
                    break;
                }
            }
        }
    }
    parsed
        || !chars
            .iter()
            .skip(essence.chars().count())
            .any(|c| *c == ',')
}

pub async fn handle<B>(app: &App, req: Request<B>) -> Response<Full<Bytes>>
where
    B: Body<Data = Bytes>,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let path = pathname(&req.uri().to_string()).unwrap_or_default();
    if path == "/healthz" {
        let services: Vec<&str> = app
            .services
            .configured()
            .into_iter()
            .map(Key::as_str)
            .collect();
        return reply(200, Some(&json!({ "ok": true, "services": services })));
    }
    let Some(key) = service_of(&path)
        .and_then(Key::parse)
        .filter(|k| app.services.tools(*k).is_some())
    else {
        return error(404, "unknown service");
    };
    if req.method() != Method::POST {
        return error(405, "stateless server: POST only");
    }
    let members = load_members(&app.members_file).await;
    let Some(member) = member_for_token(
        &members,
        req.headers()
            .get("authorization")
            .map(HeaderValue::as_bytes),
    ) else {
        return error(401, "unauthorized");
    };

    let headers = req.headers().clone();
    let body = match tokio::time::timeout(
        BODY_TIMEOUT,
        Limited::new(req.into_body(), MAX_BODY).collect(),
    )
    .await
    {
        Err(_) => return reply(408, None),
        Ok(Err(_)) => return error(400, "invalid body"),
        Ok(Ok(c)) => c.to_bytes(),
    };
    let body: Option<Value> = if body.is_empty() {
        None
    } else {
        match serde_json::from_str(&String::from_utf8_lossy(&body)) {
            Ok(v) => Some(v),
            Err(_) => return error(400, "invalid body"),
        }
    };

    // The client must take JSON and SSE answers (it gets JSON).
    let accept = joined(&headers, "accept").unwrap_or_default();
    if !(accept.contains("application/json") && accept.contains("text/event-stream")) {
        let r: Reply = transport_error(
            406,
            -32000,
            "Not Acceptable: Client must accept both application/json and text/event-stream",
        );
        return reply(r.0, r.1.as_ref());
    }
    if !is_json_content_type(joined(&headers, "content-type").as_deref()) {
        let r = transport_error(
            415,
            -32000,
            "Unsupported Media Type: Content-Type must be application/json",
        );
        return reply(r.0, r.1.as_ref());
    }
    let session = Session {
        key,
        services: &app.services,
        ctx: Ctx {
            member: &member,
            portal_origin: &app.portal_origin,
            http: &app.http,
            zone: &app.zone,
        },
    };
    let version = joined(&headers, "mcp-protocol-version");
    let (status, body) = session.handle(body, version.as_deref()).await;
    reply(status, body.as_ref())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_read_like_the_url_api() {
        assert_eq!(pathname("/healthz?x=1").as_deref(), Some("/healthz"));
        assert_eq!(pathname("/mcp/x/../photos").as_deref(), Some("/mcp/photos"));
        assert_eq!(
            pathname("http://other/healthz").as_deref(),
            Some("/healthz")
        );
        assert_eq!(service_of("/mcp/photos"), Some("photos"));
        assert_eq!(service_of("/mcp/photos/"), Some("photos"));
        assert_eq!(service_of("/mcp/photos//"), None);
        assert_eq!(service_of("/mcp/PHOTOS"), None);
        assert_eq!(service_of("/mcp/"), None);
        assert_eq!(service_of("/mcp/%70hotos"), None);
    }

    #[test]
    fn json_content_types_are_recognised() {
        for ok in [
            "application/json",
            "application/json; charset=utf-8",
            "Application/JSON",
            "application/json; charset",
            "application/json;a=\"x,y\"",
        ] {
            assert!(is_json_content_type(Some(ok)), "{ok}");
        }
        for bad in [
            "text/plain",
            "application/json-seq",
            "application/json, text/plain",
            "application/json;x, y",
            "",
        ] {
            assert!(!is_json_content_type(Some(bad)), "{bad}");
        }
        assert!(!is_json_content_type(None));
    }
}
