//! The Authentik API, as `apps/provisioner/src/authentik.ts` uses it. Only
//! what castord's job kinds need so far: removing a tenant's groups and
//! applications. Every call is idempotent (look up, then act).

use serde_json::Value;
use zeroize::Zeroizing;

pub struct Authentik {
    base: String,
    token: Zeroizing<String>,
    http: reqwest::Client,
}

#[derive(Clone, Copy)]
enum Method {
    Get,
    Patch,
    Delete,
}

impl Authentik {
    pub fn new(base: &str, token: Zeroizing<String>, http: reqwest::Client) -> Self {
        Self {
            base: base.trim_end_matches('/').to_owned(),
            token,
            http,
        }
    }

    async fn request(&self, method: Method, path: &str, body: Option<Value>) -> Result<Option<Value>, String> {
        let url = format!("{}/api/v3{path}", self.base);
        let (name, req) = match method {
            Method::Get => ("GET", self.http.get(&url)),
            Method::Patch => ("PATCH", self.http.patch(&url)),
            Method::Delete => ("DELETE", self.http.delete(&url)),
        };
        let mut req = req
            .bearer_auth(self.token.as_str())
            .header("Accept", "application/json");
        if let Some(b) = body {
            req = req.json(&b);
        }
        let what = format!("Authentik {name} {}", path.split('?').next().unwrap_or(path));
        let res = req.send().await.map_err(|e| format!("{what}: {}", e.without_url()))?;
        let status = res.status();
        if status.as_u16() == 204 {
            return Ok(None);
        }
        let text = res.text().await.map_err(|e| format!("{what}: {}", e.without_url()))?;
        if !status.is_success() {
            let head: String = text.chars().take(500).collect();
            return Err(format!("{what} -> {}: {head}", status.as_u16()));
        }
        if text.is_empty() {
            return Ok(None);
        }
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| format!("{what}: {e}"))
    }

    /// The first result of a list call filtered by `query`.
    async fn find_one(&self, path: &str, query: &[(&str, &str)]) -> Result<Option<Value>, String> {
        let qs: Vec<String> = query
            .iter()
            .map(|(k, v)| format!("{}={}", crate::llm::encode(k), crate::llm::encode(v)))
            .collect();
        let page = self
            .request(Method::Get, &format!("{path}?{}", qs.join("&")), None)
            .await?;
        Ok(page.and_then(|p| {
            p.get("results")
                .and_then(|r| r.as_array())
                .and_then(|r| r.first().cloned())
        }))
    }

    async fn find_pk(&self, path: &str, query: &[(&str, &str)]) -> Result<Option<String>, String> {
        Ok(self.find_one(path, query).await?.and_then(|v| pk(&v)))
    }

    pub async fn delete_group(&self, name: &str) -> Result<(), String> {
        if let Some(pk) = self.find_pk("/core/groups/", &[("name", name)]).await? {
            self.request(Method::Delete, &format!("/core/groups/{}/", segment(&pk)?), None)
                .await?;
        }
        Ok(())
    }

    /// Removes an application and its OIDC provider, so the service can't be signed in to.
    pub async fn delete_oidc_app(&self, slug: &str) -> Result<(), String> {
        self.delete_app(slug).await?;
        if let Some(pk) = self.find_pk("/providers/oauth2/", &[("name", slug)]).await? {
            self.request(Method::Delete, &format!("/providers/oauth2/{}/", segment(&pk)?), None)
                .await?;
        }
        Ok(())
    }

    /// A custom stack's forward-auth application, provider and outpost entry.
    pub async fn delete_proxy_app(&self, slug: &str) -> Result<(), String> {
        let provider = self.find_one("/providers/proxy/", &[("name", slug)]).await?;
        let provider_pk = provider.as_ref().and_then(|p| p.get("pk")).and_then(Value::as_i64);
        if let Some(ppk) = provider_pk {
            let outpost = self
                .find_one(
                    "/outposts/instances/",
                    &[("managed__iexact", "goauthentik.io/outposts/embedded")],
                )
                .await?
                .ok_or("Authentik: embedded outpost not found")?;
            let opk = pk(&outpost).ok_or("Authentik: embedded outpost has no pk")?;
            let providers: Vec<i64> = outpost
                .get("providers")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_i64).collect())
                .unwrap_or_default();
            if providers.contains(&ppk) {
                let keep: Vec<i64> = providers.into_iter().filter(|p| *p != ppk).collect();
                self.request(
                    Method::Patch,
                    &format!("/outposts/instances/{}/", segment(&opk)?),
                    Some(serde_json::json!({ "providers": keep })),
                )
                .await?;
            }
        }
        self.delete_app(slug).await?;
        if let Some(ppk) = provider_pk {
            self.request(Method::Delete, &format!("/providers/proxy/{ppk}/"), None)
                .await?;
        }
        Ok(())
    }

    async fn delete_app(&self, slug: &str) -> Result<(), String> {
        // Tenant apps are bound to tenant groups: only a full list shows them.
        let app = self
            .find_one(
                "/core/applications/",
                &[("slug", slug), ("superuser_full_list", "true")],
            )
            .await?;
        if app.is_some() {
            self.request(Method::Delete, &format!("/core/applications/{}/", segment(slug)?), None)
                .await?;
        }
        Ok(())
    }
}

fn pk(v: &Value) -> Option<String> {
    match v.get("pk")? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// A primary key or slug placed in a URL path: never a separator or a dot segment.
fn segment(s: &str) -> Result<&str, String> {
    if !s.is_empty() && s != "." && s != ".." && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)) {
        Ok(s)
    } else {
        Err(format!("Authentik: unexpected id {s:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_segments() {
        assert!(segment("3f2a-11").is_ok() && segment("anna-photos").is_ok() && segment("12").is_ok());
        assert!(segment("../x").is_err() && segment("a/b").is_err() && segment("..").is_err() && segment("").is_err());
    }
}
