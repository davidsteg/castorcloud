//! The Authentik API as castord uses it: tenant
//! groups and their members, one OIDC provider and application per tenant
//! service, a forward-auth application per custom-stack host, and policy
//! bindings that keep each application to the tenant's group. Every call is
//! idempotent (look up by name or slug, then create, update or delete).

use serde_json::{Value, json};
use zeroize::Zeroizing;

/// Flows, scope mappings and signing key of every provider (providerDefaults).
pub struct ProviderDefaults {
    pub authorization_flow: Value,
    pub invalidation_flow: Value,
    pub property_mappings: Vec<Value>,
    pub signing_key: Value,
}

pub struct GroupUser {
    pub pk: i64,
    pub email: String,
    pub uid: String,
}

pub struct OidcApp<'a> {
    pub slug: &'a str,
    pub name: &'a str,
    pub client_id: &'a str,
    pub client_secret: &'a str,
    pub redirect_uris: &'a [Value],
    pub launch_url: &'a str,
    pub allowed_group_pk: &'a str,
}

pub struct ProxyApp<'a> {
    pub slug: &'a str,
    pub name: &'a str,
    pub external_host: &'a str,
    pub allowed_group_pk: &'a str,
}

pub struct Authentik {
    base: String,
    token: Zeroizing<String>,
    http: reqwest::Client,
}

#[derive(Clone, Copy)]
enum Method {
    Get,
    Post,
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
            Method::Post => ("POST", self.http.post(&url)),
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

    async fn must_find(&self, what: &str, path: &str, query: &[(&str, &str)]) -> Result<Value, String> {
        self.find_one(path, query)
            .await?
            .ok_or_else(|| format!("Authentik: {what} not found"))
    }

    /// Flows, scope mappings and signing key shared by every provider castord creates.
    pub async fn provider_defaults(&self) -> Result<ProviderDefaults, String> {
        let scope = |name: &'static str| ("managed", name);
        let get = |what: &'static str, path: &'static str, q: (&'static str, &'static str)| async move {
            let v = self.must_find(what, path, &[q]).await?;
            v.get("pk")
                .cloned()
                .ok_or_else(|| format!("Authentik: {what} has no pk"))
        };
        let flow = "/flows/instances/";
        let mappings = "/propertymappings/provider/scope/";
        Ok(ProviderDefaults {
            authorization_flow: get(
                "authorization flow",
                flow,
                ("slug", "default-provider-authorization-implicit-consent"),
            )
            .await?,
            invalidation_flow: get(
                "invalidation flow",
                flow,
                ("slug", "default-provider-invalidation-flow"),
            )
            .await?,
            property_mappings: vec![
                get(
                    "openid scope",
                    mappings,
                    scope("goauthentik.io/providers/oauth2/scope-openid"),
                )
                .await?,
                get(
                    "email scope",
                    mappings,
                    scope("goauthentik.io/providers/oauth2/scope-email"),
                )
                .await?,
                get(
                    "profile scope",
                    mappings,
                    scope("goauthentik.io/providers/oauth2/scope-profile"),
                )
                .await?,
            ],
            signing_key: get(
                "signing key",
                "/crypto/certificatekeypairs/",
                ("name", "authentik Self-signed Certificate"),
            )
            .await?,
        })
    }

    /// The group's pk, created if missing.
    pub async fn ensure_group(&self, name: &str) -> Result<String, String> {
        if let Some(pk) = self.find_pk("/core/groups/", &[("name", name)]).await? {
            return Ok(pk);
        }
        let created = self
            .request(
                Method::Post,
                "/core/groups/",
                Some(json!({ "name": name, "is_superuser": false })),
            )
            .await?
            .ok_or("Authentik: empty group")?;
        pk(&created).ok_or_else(|| "Authentik: created group has no pk".to_owned())
    }

    pub async fn add_user_to_group(&self, group_pk: &str, user_pk: i64) -> Result<(), String> {
        let path = format!("/core/groups/{}/add_user/", segment(group_pk)?);
        self.request(Method::Post, &path, Some(json!({ "pk": user_pk })))
            .await?;
        Ok(())
    }

    pub async fn remove_user_from_group(&self, group_pk: &str, user_pk: i64) -> Result<(), String> {
        let path = format!("/core/groups/{}/remove_user/", segment(group_pk)?);
        self.request(Method::Post, &path, Some(json!({ "pk": user_pk })))
            .await?;
        Ok(())
    }

    /// Members of a group; `uid` is the OIDC "sub" (providers use hashed_user_id).
    pub async fn group_users(&self, group_pk: &str) -> Result<Vec<GroupUser>, String> {
        let path = format!("/core/groups/{}/?include_users=true", segment(group_pk)?);
        let group = self.request(Method::Get, &path, None).await?;
        let users = group
            .as_ref()
            .and_then(|g| g.get("users_obj"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        users
            .iter()
            .map(|u| {
                let s = |k: &str| u.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
                Ok(GroupUser {
                    pk: u
                        .get("pk")
                        .and_then(Value::as_i64)
                        .ok_or("Authentik: group user without pk")?,
                    email: s("email"),
                    uid: s("uid"),
                })
            })
            .collect()
    }

    /// Creates or updates a confidential OIDC provider and its application,
    /// open to the members of `allowed_group_pk` only.
    pub async fn ensure_oidc_app(&self, o: &OidcApp<'_>, d: &ProviderDefaults) -> Result<(), String> {
        let body = json!({
            "name": o.slug,
            "client_type": "confidential",
            "client_id": o.client_id,
            "client_secret": o.client_secret,
            "redirect_uris": o.redirect_uris,
            "sub_mode": "hashed_user_id",
            "include_claims_in_id_token": true,
            "authorization_flow": d.authorization_flow,
            "invalidation_flow": d.invalidation_flow,
            "property_mappings": d.property_mappings,
            "signing_key": d.signing_key,
        });
        let provider = self.upsert_provider("/providers/oauth2/", o.slug, body).await?;
        self.ensure_app(o.slug, o.name, provider, o.launch_url, o.allowed_group_pk)
            .await
    }

    /// Creates or updates the forward-auth provider and application of one
    /// custom-stack host, and adds it to the embedded outpost.
    pub async fn ensure_proxy_app(&self, o: &ProxyApp<'_>, d: &ProviderDefaults) -> Result<(), String> {
        let body = json!({
            "name": o.slug,
            "mode": "forward_single",
            "external_host": o.external_host,
            "authorization_flow": d.authorization_flow,
            "invalidation_flow": d.invalidation_flow,
            // The app sees who is signed in (X-authentik-*), never a token it could replay elsewhere.
            "intercept_header_auth": false,
            "basic_auth_enabled": false,
            "access_token_validity": "hours=12",
        });
        let provider = self.upsert_provider("/providers/proxy/", o.slug, body).await?;
        self.ensure_app(o.slug, o.name, provider, o.external_host, o.allowed_group_pk)
            .await?;
        let outpost = self.embedded_outpost().await?;
        let opk = pk(&outpost).ok_or("Authentik: embedded outpost has no pk")?;
        let mut providers = outpost_providers(&outpost);
        if !providers.contains(&provider) {
            providers.push(provider);
            self.request(
                Method::Patch,
                &format!("/outposts/instances/{}/", segment(&opk)?),
                Some(json!({ "providers": providers })),
            )
            .await?;
        }
        Ok(())
    }

    async fn embedded_outpost(&self) -> Result<Value, String> {
        self.must_find(
            "embedded outpost",
            "/outposts/instances/",
            &[("managed__iexact", "goauthentik.io/outposts/embedded")],
        )
        .await
    }

    /// PATCHes the provider named `slug`, or creates it; returns its pk.
    async fn upsert_provider(&self, base: &str, slug: &str, body: Value) -> Result<i64, String> {
        let existing = self.find_one(base, &[("name", slug)]).await?;
        let existing_pk = existing.as_ref().and_then(|p| p.get("pk")).and_then(Value::as_i64);
        let saved = match existing_pk {
            Some(ppk) => {
                self.request(Method::Patch, &format!("{base}{ppk}/"), Some(body))
                    .await?
            }
            None => self.request(Method::Post, base, Some(body)).await?,
        };
        saved
            .as_ref()
            .and_then(|p| p.get("pk"))
            .and_then(Value::as_i64)
            .ok_or_else(|| format!("Authentik: provider {slug} has no pk"))
    }

    async fn ensure_app(
        &self,
        slug: &str,
        name: &str,
        provider: i64,
        launch: &str,
        group_pk: &str,
    ) -> Result<(), String> {
        let body = json!({ "name": name, "slug": slug, "provider": provider, "meta_launch_url": launch });
        let found = self
            .find_one(
                "/core/applications/",
                &[("slug", slug), ("superuser_full_list", "true")],
            )
            .await?;
        let app = match found {
            Some(a) => a,
            None => self
                .request(Method::Post, "/core/applications/", Some(body.clone()))
                .await?
                .ok_or("Authentik: empty application")?,
        };
        self.request(
            Method::Patch,
            &format!("/core/applications/{}/", segment(slug)?),
            Some(body),
        )
        .await?;
        let app_pk = pk(&app).ok_or("Authentik: application has no pk")?;
        // Only members of the tenant group may use this application.
        let bound = self
            .find_one("/policies/bindings/", &[("target", &app_pk), ("group", group_pk)])
            .await?;
        if bound.is_none() {
            let binding = json!({ "target": app_pk, "group": group_pk, "order": 0, "enabled": true });
            self.request(Method::Post, "/policies/bindings/", Some(binding)).await?;
        }
        Ok(())
    }

    pub async fn find_user_pk_by_email(&self, email: &str) -> Result<Option<i64>, String> {
        let user = self.find_one("/core/users/", &[("email", email)]).await?;
        Ok(user.and_then(|u| u.get("pk").and_then(Value::as_i64)))
    }

    /// Signs the user out of Authentik in every browser and drops their refresh tokens.
    pub async fn end_user_sessions(&self, user_pk: i64) -> Result<(), String> {
        let user = self
            .request(Method::Get, &format!("/core/users/{user_pk}/"), None)
            .await?
            .ok_or("Authentik: empty user")?;
        let username = user
            .get("username")
            .and_then(Value::as_str)
            .ok_or("Authentik: user without username")?;
        let path = format!(
            "/core/authenticated_sessions/?user__username={}&page_size=1000",
            crate::llm::encode(username)
        );
        for s in results(self.request(Method::Get, &path, None).await?) {
            if let Some(uuid) = s.get("uuid").and_then(Value::as_str) {
                self.request(
                    Method::Delete,
                    &format!("/core/authenticated_sessions/{}/", segment(uuid)?),
                    None,
                )
                .await?;
            }
        }
        let path = format!("/oauth2/refresh_tokens/?user={user_pk}&page_size=1000");
        for t in results(self.request(Method::Get, &path, None).await?) {
            if let Some(id) = t.get("id").and_then(Value::as_i64) {
                self.request(Method::Delete, &format!("/oauth2/refresh_tokens/{id}/"), None)
                    .await?;
            }
        }
        Ok(())
    }

    /// Applies one of Authentik's bundled blueprints under `name` and waits
    /// for the flow it creates; then re-applies the blueprint `reapply`
    /// (ours, which points the brand at that flow).
    pub async fn ensure_blueprint_instance(
        &self,
        name: &str,
        path: &str,
        flow_slug: &str,
        reapply: Option<&str>,
    ) -> Result<(), String> {
        let body = match self.find_pk("/managed/blueprints/", &[("name", name)]).await? {
            Some(pk) => {
                self.request(
                    Method::Patch,
                    &format!("/managed/blueprints/{}/", segment(&pk)?),
                    Some(json!({ "path": path, "enabled": true })),
                )
                .await?
            }
            None => {
                self.request(
                    Method::Post,
                    "/managed/blueprints/",
                    Some(json!({ "name": name, "path": path, "enabled": true, "context": {} })),
                )
                .await?
            }
        };
        let pk = body
            .as_ref()
            .and_then(pk)
            .ok_or("Authentik: blueprint instance without pk")?;
        self.request(
            Method::Post,
            &format!("/managed/blueprints/{}/apply/", segment(&pk)?),
            None,
        )
        .await?;
        for _ in 0..30 {
            if self
                .find_one("/flows/instances/", &[("slug", flow_slug)])
                .await?
                .is_some()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        if let Some(other) = reapply
            && let Some(pk) = self.find_pk("/managed/blueprints/", &[("name", other)]).await?
        {
            self.request(
                Method::Post,
                &format!("/managed/blueprints/{}/apply/", segment(&pk)?),
                None,
            )
            .await?;
        }
        Ok(())
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
            let outpost = self.embedded_outpost().await?;
            let opk = pk(&outpost).ok_or("Authentik: embedded outpost has no pk")?;
            let providers = outpost_providers(&outpost);
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

fn outpost_providers(outpost: &Value) -> Vec<i64> {
    outpost
        .get("providers")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default()
}

fn results(page: Option<Value>) -> Vec<Value> {
    page.and_then(|p| p.get("results").and_then(Value::as_array).cloned())
        .unwrap_or_default()
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
