//! The node's LLM gateway (LiteLLM) admin API, as in
//! `apps/provisioner/src/llm.ts`. Only what suspend needs so far: block.

use std::time::Duration;

use zeroize::Zeroizing;

pub struct Gateway {
    base: String,
    master_key: Zeroizing<String>,
    http: reqwest::Client,
}

impl Gateway {
    pub fn new(base: &str, master_key: Zeroizing<String>) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            base: base.trim_end_matches('/').to_owned(),
            master_key,
            http,
        })
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

    /// Whether the gateway knows the key (unknown: 404, or 400/401 on some versions).
    async fn exists(&self, key: &str) -> Result<bool, String> {
        match self
            .request(false, &format!("/key/info?key={}", encode(key)), None)
            .await
        {
            Ok((404, _)) => Ok(false),
            Ok((_, text)) => {
                let v: serde_json::Value =
                    serde_json::from_str(&text).map_err(|e| format!("LLM gateway /key/info: {e}"))?;
                Ok(v.get("info").is_some_and(|i| !i.is_null()))
            }
            Err(e) if e.contains("-> 400") || e.contains("-> 401") => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Suspended tenant: no more requests, spend kept.
    pub async fn block(&self, key: &str) -> Result<(), String> {
        if self.exists(key).await? {
            self.request(true, "/key/block", Some(serde_json::json!({ "key": key })))
                .await?;
        }
        Ok(())
    }
}

/// encodeURIComponent.
fn encode(s: &str) -> String {
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
