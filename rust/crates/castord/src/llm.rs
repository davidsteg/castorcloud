//! The node's LLM gateway (LiteLLM) admin API: one virtual key per tenant, carrying the
//! tenant's monthly budget. The key value is ours (kept in "Secret"), so
//! every call is idempotent.

use zeroize::Zeroizing;

pub struct Gateway {
    base: String,
    master_key: Zeroizing<String>,
    http: reqwest::Client,
}

impl Gateway {
    pub fn new(base: &str, master_key: Zeroizing<String>, http: reqwest::Client) -> Self {
        Self {
            base: base.trim_end_matches('/').to_owned(),
            master_key,
            http,
        }
    }

    async fn request(&self, post: bool, path: &str, body: Option<serde_json::Value>) -> Result<(u16, String), String> {
        let url = format!("{}{path}", self.base);
        let req = if post {
            self.http.post(&url)
        } else {
            self.http.get(&url)
        };
        let mut req = req.bearer_auth(self.master_key.as_str());
        if let Some(b) = body {
            req = req.json(&b);
        }
        let what = format!(
            "LLM gateway {} {}",
            if post { "POST" } else { "GET" },
            path.split('?').next().unwrap_or(path)
        );
        let res = req.send().await.map_err(|e| format!("{what}: {}", e.without_url()))?;
        let status = res.status().as_u16();
        let text = res.text().await.map_err(|e| format!("{what}: {}", e.without_url()))?;
        if !(200..300).contains(&status) && status != 404 {
            let head: String = text.chars().take(500).collect();
            return Err(format!("{what} -> {status}: {head}"));
        }
        Ok((status, text))
    }

    /// The key's info, None when the gateway doesn't know it (404, or 400/401 on some versions).
    async fn info(&self, key: &str) -> Result<Option<serde_json::Value>, String> {
        match self
            .request(false, &format!("/key/info?key={}", encode(key)), None)
            .await
        {
            Ok((404, _)) => Ok(None),
            Ok((_, text)) => {
                let v: serde_json::Value =
                    serde_json::from_str(&text).map_err(|e| format!("LLM gateway /key/info: {e}"))?;
                Ok(v.get("info").filter(|i| !i.is_null()).cloned())
            }
            Err(e) if e.contains("-> 400") || e.contains("-> 401") => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn exists(&self, key: &str) -> Result<bool, String> {
        Ok(self.info(key).await?.is_some())
    }

    /// Creates or updates the tenant key: budget per month, only the assistant
    /// model, unblocked. An existing key keeps its budget_duration, so the
    /// monthly reset doesn't move.
    pub async fn ensure_tenant_key(
        &self,
        key: &str,
        slug: &str,
        tenant_id: &str,
        max_budget_usd: f64,
    ) -> Result<(), String> {
        // Aliases must be unique; tenant ids never repeat, slugs may after a deletion.
        let mut body = serde_json::json!({
            "key": key,
            "key_alias": format!("tenant-{slug}-{tenant_id}"),
            "max_budget": max_budget_usd,
            "budget_duration": "1mo",
            "models": ["assistant"],
            "metadata": { "tenant_id": tenant_id },
        });
        let Some(existing) = self.info(key).await? else {
            self.request(true, "/key/generate", Some(body)).await?;
            return Ok(());
        };
        if let Some(o) = body.as_object_mut() {
            o.shift_remove("budget_duration");
        }
        self.request(true, "/key/update", Some(body)).await?;
        if existing.get("blocked").and_then(serde_json::Value::as_bool) == Some(true) {
            self.request(true, "/key/unblock", Some(serde_json::json!({ "key": key })))
                .await?;
        }
        Ok(())
    }

    /// Suspended tenant: no more requests, spend kept.
    pub async fn block(&self, key: &str) -> Result<(), String> {
        if self.exists(key).await? {
            self.request(true, "/key/block", Some(serde_json::json!({ "key": key })))
                .await?;
        }
        Ok(())
    }

    /// Tenant deleted: the key goes, with its spend.
    pub async fn delete(&self, key: &str) -> Result<(), String> {
        if self.exists(key).await? {
            self.request(true, "/key/delete", Some(serde_json::json!({ "keys": [key] })))
                .await?;
        }
        Ok(())
    }
}

/// encodeURIComponent.
pub(crate) fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_like_js() {
        assert_eq!(encode("sk-a_b.c"), "sk-a_b.c");
        assert_eq!(encode("a b/+&é"), "a%20b%2F%2B%26%C3%A9");
    }
}
