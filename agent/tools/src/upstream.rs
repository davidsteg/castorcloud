//! Small JSON client for the tenant's services (reached on the private stack
//! network, never via the internet), and a cache for keys and lookup tables.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Client, Method, Url};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::limits::ToolError;

/// Each request, from connecting to the last byte of the answer.
const TIMEOUT: Duration = Duration::from_secs(20);
/// No answer we read is anywhere near this (5000 location points are a few
/// MB); a larger one is refused rather than held in memory.
const MAX_ANSWER: usize = 64 * 1024 * 1024;

pub fn client() -> Result<Client, String> {
    Client::builder()
        // Redirects are an error, as with fetch's redirect: "error".
        .redirect(reqwest::redirect::Policy::none())
        .timeout(TIMEOUT)
        // Only the stack's own services, never through a proxy from the environment.
        .no_proxy()
        .build()
        .map_err(|e| e.to_string())
}

pub struct Request<'a> {
    pub base: &'a str,
    pub path: &'a str,
    pub label: &'a str,
    pub method: Method,
    pub headers: Vec<(&'static str, String)>,
    /// Set in order; None values are left out.
    pub query: Vec<(&'static str, Option<String>)>,
    pub body: Option<Value>,
}

impl<'a> Request<'a> {
    pub fn get(base: &'a str, path: &'a str, label: &'a str) -> Self {
        Self {
            base,
            path,
            label,
            method: Method::GET,
            headers: Vec::new(),
            query: Vec::new(),
            body: None,
        }
    }
    pub fn header(mut self, name: &'static str, value: String) -> Self {
        self.headers.push((name, value));
        self
    }
    pub fn query(mut self, name: &'static str, value: impl Into<Option<String>>) -> Self {
        self.query.push((name, value.into()));
        self
    }
    pub fn post(mut self, body: Value) -> Self {
        self.method = Method::POST;
        self.body = Some(body);
        self
    }
}

fn url(r: &Request) -> Result<Url, ToolError> {
    let mut url = Url::parse(r.base)
        .and_then(|b| b.join(r.path))
        .map_err(|e| ToolError::internal(format!("{} URL: {e}", r.label)))?;
    let pairs: Vec<_> = r
        .query
        .iter()
        .filter_map(|(k, v)| v.as_ref().map(|v| (k, v)))
        .collect();
    if !pairs.is_empty() {
        let mut q = url.query_pairs_mut();
        q.clear();
        for (k, v) in pairs {
            q.append_pair(k, v);
        }
    }
    Ok(url)
}

pub async fn get_json<T: DeserializeOwned>(
    client: &Client,
    r: Request<'_>,
) -> Result<T, ToolError> {
    let label = r.label;
    let url = url(&r)?;
    let unreachable = || ToolError::user(format!("{label} is not reachable right now"));
    let mut headers = HeaderMap::new();
    headers.insert("accept", HeaderValue::from_static("application/json"));
    // The services sit behind TLS-terminating proxies and redirect plain
    // HTTP to https (Dawarich: force_ssl); this is what the skin proxy sends.
    headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
    if r.body.is_some() {
        headers.insert("content-type", HeaderValue::from_static("application/json"));
    }
    for (name, value) in &r.headers {
        let mut v = HeaderValue::from_str(value).map_err(|_| unreachable())?;
        v.set_sensitive(true);
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| unreachable())?;
        headers.insert(name, v);
    }
    let mut req = client.request(r.method.clone(), url).headers(headers);
    if let Some(body) = &r.body {
        req = req.body(body.to_string());
    }
    let mut res = req.send().await.map_err(|_| unreachable())?;
    let status = res.status().as_u16();
    match status {
        // fetch with redirect: "error" fails on these.
        301 | 302 | 303 | 307 | 308 => return Err(unreachable()),
        401 | 403 => return Err(ToolError::user(format!("{label} refused access"))),
        404 => return Err(ToolError::user(format!("not found in {label}"))),
        200..=299 => {}
        _ => return Err(ToolError::user(format!("{label} answered {status}"))),
    }
    let mut body = Vec::new();
    while let Some(chunk) = res
        .chunk()
        .await
        .map_err(|e| ToolError::internal(format!("{label}: {}", e.without_url())))?
    {
        if body.len() + chunk.len() > MAX_ANSWER {
            return Err(ToolError::internal(format!(
                "{label}: answer larger than {MAX_ANSWER} bytes"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body)
        .map_err(|e| ToolError::internal(format!("{label}: unexpected answer: {e}")))
}

/// Remembers a value per key for a while (API keys, lookup tables).
pub struct TtlCache<V> {
    ttl: Duration,
    entries: Mutex<HashMap<String, (V, Instant)>>,
}

impl<V: Clone> TtlCache<V> {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            entries: Mutex::new(HashMap::new()),
        }
    }

    fn entries(&self) -> std::sync::MutexGuard<'_, HashMap<String, (V, Instant)>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub async fn get<F, Fut>(&self, key: &str, load: F) -> Result<V, ToolError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<V, ToolError>>,
    {
        if let Some((v, until)) = self.entries().get(key)
            && *until > Instant::now()
        {
            return Ok(v.clone());
        }
        let value = load().await?;
        self.entries()
            .insert(key.to_owned(), (value.clone(), Instant::now() + self.ttl));
        Ok(value)
    }

    pub fn delete(&self, key: &str) {
        self.entries().remove(key);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_built_like_the_url_api() {
        let r = Request::get("http://immich-server:2283", "/api/people", "Photos")
            .query("size", "100".to_owned())
            .query("cursor", None)
            .query("name", "Anna Müller & co".to_owned());
        assert_eq!(
            url(&r).unwrap().as_str(),
            "http://immich-server:2283/api/people?size=100&name=Anna+M%C3%BCller+%26+co"
        );
        let r = Request::get("http://x:1/prefix/", "/api/v1/stats", "T");
        assert_eq!(url(&r).unwrap().as_str(), "http://x:1/api/v1/stats");
        assert!(matches!(
            url(&Request::get("", "/a", "T")),
            Err(ToolError::Internal(_))
        ));
    }

    #[tokio::test]
    async fn the_cache_keeps_values_for_a_while() {
        let c = TtlCache::new(Duration::from_secs(60));
        assert_eq!(c.get("k", || async { Ok(1) }).await.unwrap(), 1);
        assert_eq!(c.get("k", || async { Ok(2) }).await.unwrap(), 1);
        c.delete("k");
        assert!(
            c.get("k", || async { Err(ToolError::user("no")) })
                .await
                .is_err()
        );
        assert_eq!(c.get("k", || async { Ok(3) }).await.unwrap(), 3);
        let short = TtlCache::new(Duration::ZERO);
        assert_eq!(short.get("k", || async { Ok(1) }).await.unwrap(), 1);
        assert_eq!(short.get("k", || async { Ok(2) }).await.unwrap(), 2);
    }
}
