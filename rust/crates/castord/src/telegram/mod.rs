//! The Castor Telegram bot: one bot for all tenants. Members
//! opt in from the portal (Assistant → Telegram), which shows a one-time
//! pairing code; sending that code to the bot links their Telegram account to
//! their own assistant. After that, every text message is forwarded to the
//! member's Hermes profile and the answer is sent back.
//!
//! Telegram is not end-to-end encrypted, so the portal warns before pairing.
//! Only private chats are served; group chats are ignored. Answers are sent
//! as plain text (no parse mode), so nothing in them is interpreted.

pub mod sse;
pub mod text;

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use zeroize::Zeroizing;

use sse::Outcome;
use text::{Lang, Text, lang};

pub const MAX_INPUT: usize = 4000;
/// Telegram allows 4096 characters per message.
const MAX_MESSAGE: usize = 4000;

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub id: i64,
    pub username: Option<String>,
    pub first_name: Option<String>,
    pub language_code: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Chat {
    pub id: i64,
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub chat: Chat,
    pub from: Option<User>,
    pub text: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Update {
    pub update_id: i64,
    pub message: Option<Message>,
}

/// Stored instead of the code; tolerant of case, spaces and dashes.
pub fn pairing_code_hash(code: &str) -> String {
    let clean: String = code
        .to_uppercase()
        .chars()
        .filter(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        .collect();
    hex::encode(Sha256::digest(clean.as_bytes()))
}

/// What a pairing code may look like when typed: "K7M3Q9TX", "K7M3-Q9TX".
fn looks_like_code(s: &str) -> bool {
    (8..=11).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b' ' || b == b'-')
}

/// "/start@CastorBot ABC 123" -> ("/start", "ABC123").
fn command(text: &str) -> Option<(String, String)> {
    if !text.starts_with('/') {
        return None;
    }
    let mut words = text.split_whitespace();
    let head = words.next().unwrap_or("");
    let name = head.split('@').next().unwrap_or("").to_lowercase();
    Some((name, words.collect()))
}

pub struct Bot {
    pub db: PgPool,
    pub http: reqwest::Client,
    pub keys: castor_crypto::Keys,
    /// <TELEGRAM_API_URL>/bot<token>
    pub api: Zeroizing<String>,
    pub scheme: String,
    pub base_domain: String,
    pub llm_gateway: String,
    /// For the credit reset date.
    pub time_zone: String,
    /// Tests: where the assistants are, instead of their public origins.
    pub assistant_origin: Option<String>,
}

#[derive(sqlx::FromRow)]
struct Link {
    id: String,
    session_id: Option<String>,
    member_id: String,
    user_id: String,
    tenant_id: String,
    slug: String,
    active: bool,
    booked: bool,
    user_locale: Option<String>,
    tenant_locale: String,
}

impl Link {
    fn lang(&self) -> Lang {
        lang(&[self.user_locale.as_deref(), Some(&self.tenant_locale)])
    }
}

const LINK: &str = r#"SELECT l.id, l."sessionId" AS session_id, m.id AS member_id, m."userId" AS user_id,
       m."tenantId" AS tenant_id, t.slug, t.status = 'ACTIVE' AS active,
       EXISTS (SELECT 1 FROM "ServiceInstance" s WHERE s."tenantId" = t.id AND s.kind = 'ASSISTANT' AND s.enabled) AS booked,
       u.locale AS user_locale, t.locale AS tenant_locale
FROM "TelegramLink" l JOIN "Member" m ON m.id = l."memberId"
JOIN "User" u ON u.id = m."userId" JOIN "Tenant" t ON t.id = m."tenantId""#;

fn db_err(e: sqlx::Error) -> String {
    e.to_string()
}

impl Bot {
    pub async fn call(&self, method: &str, body: &Value, timeout: Duration) -> Result<Value, String> {
        let res = self
            .http
            .post(format!("{}/{method}", self.api.as_str()))
            .json(body)
            .timeout(timeout)
            .send()
            .await
            // The URL holds the token: never in an error.
            .map_err(|e| format!("Telegram {method}: {}", e.without_url()))?;
        let data: Value = res
            .json()
            .await
            .map_err(|e| format!("Telegram {method}: {}", e.without_url()))?;
        if data.get("ok").and_then(Value::as_bool) != Some(true) {
            let why = data.get("description").and_then(Value::as_str).unwrap_or("not ok");
            return Err(format!("Telegram {method}: {why}"));
        }
        Ok(data.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Plain text, split to fit.
    pub async fn send(&self, chat_id: i64, text: &str) -> Result<(), String> {
        let chars: Vec<char> = text.chars().collect();
        for part in chars.chunks(MAX_MESSAGE) {
            let part: String = part.iter().collect();
            self.call(
                "sendMessage",
                &json!({ "chat_id": chat_id, "text": part, "link_preview_options": { "is_disabled": true } }),
                Duration::from_secs(30),
            )
            .await?;
        }
        Ok(())
    }

    async fn link_for(&self, telegram_user: i64) -> Result<Option<Link>, String> {
        sqlx::query_as(&format!(r#"{LINK} WHERE l."telegramUserId" = $1"#))
            .bind(telegram_user)
            .fetch_optional(&self.db)
            .await
            .map_err(db_err)
    }

    async fn pair(&self, chat_id: i64, from: &User, code: &str) -> Result<(), String> {
        let t = text::text(lang(&[from.language_code.as_deref()]));
        let pending: Option<(String, bool, String, String, Option<String>, String)> = sqlx::query_as(
            r#"SELECT l.id, COALESCE(l."pairingExpiresAt" >= now() AT TIME ZONE 'UTC', false),
                      m."userId", m."tenantId", u.locale, t.locale
               FROM "TelegramLink" l JOIN "Member" m ON m.id = l."memberId"
               JOIN "User" u ON u.id = m."userId" JOIN "Tenant" t ON t.id = m."tenantId"
               WHERE l."pairingCodeHash" = $1"#,
        )
        .bind(pairing_code_hash(code))
        .fetch_optional(&self.db)
        .await
        .map_err(db_err)?;
        let Some((id, true, user_id, tenant_id, user_locale, tenant_locale)) = pending else {
            return self.send(chat_id, t.bad_code).await;
        };
        // One Telegram account talks to one assistant: the newest pairing wins.
        let mut tx = self.db.begin().await.map_err(db_err)?;
        sqlx::query(r#"DELETE FROM "TelegramLink" WHERE "telegramUserId" = $1 AND id <> $2"#)
            .bind(from.id)
            .bind(&id)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        sqlx::query(
            r#"UPDATE "TelegramLink" SET "telegramUserId" = $2, "telegramUsername" = $3,
                      "linkedAt" = now() AT TIME ZONE 'UTC', "pairingCodeHash" = NULL, "pairingExpiresAt" = NULL,
                      "sessionId" = NULL, "updatedAt" = now() AT TIME ZONE 'UTC'
               WHERE id = $1"#,
        )
        .bind(&id)
        .bind(from.id)
        .bind(from.username.as_ref().or(from.first_name.as_ref()))
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        crate::db::audit(&self.db, &user_id, "assistant.telegram_linked", Some(&tenant_id), None).await?;
        let l = lang(&[user_locale.as_deref(), Some(&tenant_locale)]);
        self.send(chat_id, text::text(l).paired).await
    }

    async fn secret(&self, tenant_id: &str, key: &str) -> Result<Option<Zeroizing<String>>, String> {
        let sealed: Option<String> =
            sqlx::query_scalar(r#"SELECT ciphertext FROM "Secret" WHERE "tenantId" = $1 AND key = $2"#)
                .bind(tenant_id)
                .bind(key)
                .fetch_optional(&self.db)
                .await
                .map_err(db_err)?;
        sealed
            .map(|s| {
                self.keys
                    .open_secret(tenant_id, key, &s)
                    .map_err(|e| format!("{key}: {e}"))
            })
            .transpose()
    }

    /// When the tenant's monthly AI credit resets, formatted for the member:
    /// the gateway's reset time, else the first of next month (in TZ).
    async fn reset_date(&self, tenant_id: &str, l: Lang) -> Result<String, String> {
        let mut reset: Option<String> = None;
        if let Some(key) = self.secret(tenant_id, "LLM_VIRTUAL_KEY").await? {
            let info = self
                .http
                .get(format!("{}/key/info", self.llm_gateway))
                .bearer_auth(key.as_str())
                .timeout(Duration::from_secs(5))
                .send()
                .await;
            if let Ok(r) = info
                && r.status().is_success()
                && let Ok(v) = r.json::<Value>().await
            {
                reset = v
                    .pointer("/info/budget_reset_at")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
        }
        let (y, m, d): (i32, i32, i32) = sqlx::query_as(
            r#"SELECT extract(year FROM t)::int, extract(month FROM t)::int, extract(day FROM t)::int FROM (
                 SELECT COALESCE($1::timestamptz AT TIME ZONE $2,
                                 date_trunc('month', now() AT TIME ZONE $2) + interval '1 month') AS t) x"#,
        )
        .bind(reset)
        .bind(&self.time_zone)
        .fetch_one(&self.db)
        .await
        .map_err(db_err)?;
        Ok(text::long_date(
            l,
            y,
            u32::try_from(m).unwrap_or(1),
            u32::try_from(d).unwrap_or(1),
        ))
    }

    fn assistant_api(&self, slug: &str, member_id: &str) -> String {
        let origin = self
            .assistant_origin
            .clone()
            .unwrap_or_else(|| format!("{}://assistant-{slug}.{}", self.scheme, self.base_domain));
        format!("{origin}/p/{member_id}/api")
    }

    /// Forwards one message to the member's assistant; returns the text to send back.
    async fn ask(&self, link: &Link, input: &str, t: &Text, l: Lang) -> Result<String, String> {
        if !link.active || !link.booked {
            return Ok(t.unavailable.into());
        }
        let Some(api_key) = self
            .secret(&link.tenant_id, &format!("ASSISTANT_KEY_{}", link.member_id))
            .await?
        else {
            return Ok(t.unavailable.into());
        };
        let base = self.assistant_api(&link.slug, &link.member_id);
        let hermes = |path: String, body: Value| {
            self.http
                .post(format!("{base}{path}"))
                .bearer_auth(api_key.as_str())
                .json(&body)
                .timeout(Duration::from_secs(10 * 60))
                .send()
        };
        let mut session = link.session_id.clone();
        for _ in 0..2 {
            let id = match &session {
                Some(id) => id.clone(),
                None => {
                    let res = hermes("/sessions".into(), json!({})).await.map_err(|e| e.to_string())?;
                    if !res.status().is_success() {
                        return Ok(t.unavailable.into());
                    }
                    let created: Value = res.json().await.map_err(|e| e.to_string())?;
                    let Some(id) = created
                        .get("id")
                        .or_else(|| created.pointer("/session/id"))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                    else {
                        return Ok(t.unavailable.into());
                    };
                    sqlx::query(
                        r#"UPDATE "TelegramLink" SET "sessionId" = $2, "updatedAt" = now() AT TIME ZONE 'UTC' WHERE id = $1"#,
                    )
                    .bind(&link.id)
                    .bind(&id)
                    .execute(&self.db)
                    .await
                    .map_err(db_err)?;
                    id
                }
            };
            // The id is the assistant's; it only ever goes back to it, as one path segment.
            if !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
                return Ok(t.failed.into());
            }
            let mut res = hermes(format!("/sessions/{id}/chat/stream"), json!({ "input": input }))
                .await
                .map_err(|e| e.to_string())?;
            if res.status() == reqwest::StatusCode::NOT_FOUND {
                session = None; // conversation deleted in the portal: start a new one
                continue;
            }
            if !res.status().is_success() {
                return Ok(t.unavailable.into());
            }
            let mut parser = sse::Parser::default();
            while let Some(chunk) = res.chunk().await.map_err(|e| e.to_string())? {
                match parser.feed(&chunk) {
                    Some(Outcome::Done(text)) => {
                        let text = text.trim();
                        return Ok(if text.is_empty() { t.failed.into() } else { text.into() });
                    }
                    Some(Outcome::Credit) => {
                        return Ok(t.credit.replace("{date}", &self.reset_date(&link.tenant_id, l).await?));
                    }
                    Some(Outcome::Failed) => return Ok(t.failed.into()),
                    None => {}
                }
            }
            return Ok(t.failed.into()); // stream ended without an answer
        }
        Ok(t.failed.into())
    }

    /// One incoming message.
    pub async fn handle(&self, msg: &Message) -> Result<(), String> {
        let Some(from) = msg.from.as_ref().filter(|_| msg.chat.kind == "private") else {
            return Ok(());
        };
        let chat = msg.chat.id;
        let text = msg.text.as_deref().unwrap_or("").trim();
        let cmd = command(text);
        let is = |names: &[&str]| cmd.as_ref().is_some_and(|(c, _)| names.contains(&c.as_str()));
        let arg = cmd.as_ref().map_or("", |(_, a)| a.as_str());

        let Some(link) = self.link_for(from.id).await? else {
            let t = text::text(lang(&[from.language_code.as_deref()]));
            let code = if is(&["/start"]) { arg } else { text };
            if looks_like_code(code) {
                return self.pair(chat, from, code).await;
            }
            return self.send(chat, t.welcome).await;
        };

        let l = link.lang();
        let t = text::text(l);
        if is(&["/start"]) && !arg.is_empty() {
            return self.pair(chat, from, arg).await; // re-pairing, possibly to another member
        }
        if is(&["/new", "/start"]) {
            sqlx::query(
                r#"UPDATE "TelegramLink" SET "sessionId" = NULL, "updatedAt" = now() AT TIME ZONE 'UTC' WHERE id = $1"#,
            )
            .bind(&link.id)
            .execute(&self.db)
            .await
            .map_err(db_err)?;
            return self.send(chat, t.new_chat).await;
        }
        if is(&["/disconnect", "/stop"]) {
            sqlx::query(r#"DELETE FROM "TelegramLink" WHERE id = $1"#)
                .bind(&link.id)
                .execute(&self.db)
                .await
                .map_err(db_err)?;
            crate::db::audit(
                &self.db,
                &link.user_id,
                "assistant.telegram_disconnect",
                Some(&link.tenant_id),
                None,
            )
            .await?;
            return self.send(chat, t.disconnected).await;
        }
        if text.is_empty() {
            return self.send(chat, t.text_only).await;
        }
        if text.chars().count() > MAX_INPUT {
            return self.send(chat, t.failed).await;
        }

        // "typing…" lasts 5 seconds; renewed until the answer is there.
        let ask = self.ask(&link, text, t, l);
        tokio::pin!(ask);
        let answer = loop {
            let typing = async {
                let action = json!({ "chat_id": chat, "action": "typing" });
                let _ = self.call("sendChatAction", &action, Duration::from_secs(10)).await;
                tokio::time::sleep(Duration::from_millis(4500)).await;
            };
            tokio::select! {
                a = &mut ask => break a,
                () = typing => {}
            }
        };
        let answer = answer.unwrap_or_else(|e| {
            eprintln!("telegram: assistant: {e}");
            t.unavailable.into()
        });
        self.send(chat, &answer).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_commands() {
        // As the portal hashes pairing codes.
        assert_eq!(pairing_code_hash("k7m3-q9tx"), pairing_code_hash("K7M3Q9TX"));
        assert_eq!(
            pairing_code_hash("K7M3Q9TX"),
            "e7c607e25b2698c286442d803f24431cbb72d743f35850499c5a7b9927c7e47a"
        );
        assert!(looks_like_code("K7M3 Q9TX"));
        assert!(!looks_like_code("hello there, assistant"));
        assert!(!looks_like_code("K7M3Q9T"));
        assert_eq!(
            command("/Start@CastorBot ab cd"),
            Some(("/start".into(), "abcd".into()))
        );
        assert_eq!(command("hi"), None);
    }
}
