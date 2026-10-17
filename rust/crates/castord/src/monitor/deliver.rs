//! Alert channels: Telegram gets every round's digest; e-mail
//! gets those with anything critical in them (or everything, without
//! Telegram). Plain text everywhere, so nothing in an alert is interpreted.

use std::time::Duration;

use lettre::message::Mailbox;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use serde_json::json;
use zeroize::Zeroizing;

use super::Deliver;
use super::rules::{self, Notice, Severity};

pub struct Telegram {
    pub api: String,
    pub token: Zeroizing<String>,
    pub chat_id: String,
    pub http: reqwest::Client,
}

impl Telegram {
    pub async fn send(&self, text: &str) -> Result<(), String> {
        let url = format!("{}/bot{}/sendMessage", self.api, self.token.as_str());
        let chars: Vec<char> = text.chars().collect();
        for part in chars.chunks(4000) {
            let part: String = part.iter().collect();
            let res = self
                .http
                .post(&url)
                // Plain text: no parse mode.
                .json(
                    &json!({ "chat_id": self.chat_id, "text": part, "link_preview_options": { "is_disabled": true } }),
                )
                .timeout(Duration::from_secs(15))
                .send()
                .await
                // The URL holds the token: never in an error.
                .map_err(|e| format!("Telegram: {}", e.without_url()))?;
            let data: serde_json::Value = res.json().await.map_err(|e| format!("Telegram: {}", e.without_url()))?;
            if data.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
                let why = data.get("description").and_then(|d| d.as_str()).unwrap_or("not ok");
                return Err(format!("Telegram: {why}"));
            }
        }
        Ok(())
    }
}

pub struct Mail {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
    to: Mailbox,
}

impl Mail {
    /// Only over TLS: smtps:// (implicit) or smtp:// with STARTTLS required.
    pub fn new(url: &str, to: &str, from: &str) -> Result<Self, String> {
        let u = reqwest::Url::parse(url).map_err(|_| "MONITOR_SMTP_URL is not a URL".to_owned())?;
        let host = u.host_str().ok_or("MONITOR_SMTP_URL has no host")?;
        let builder = match u.scheme() {
            "smtps" => AsyncSmtpTransport::<Tokio1Executor>::relay(host),
            "smtp" => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host),
            _ => return Err("MONITOR_SMTP_URL must be smtps:// or smtp://".into()),
        }
        .map_err(|e| format!("MONITOR_SMTP_URL: {e}"))?;
        let mut builder = builder
            .port(u.port().unwrap_or(if u.scheme() == "smtps" { 465 } else { 587 }))
            .timeout(Some(Duration::from_secs(30)));
        if !u.username().is_empty() {
            let decode = |s: &str| percent_decode(s).ok_or_else(|| "MONITOR_SMTP_URL: bad percent-encoding".to_owned());
            builder = builder.credentials(Credentials::new(
                decode(u.username())?,
                decode(u.password().unwrap_or(""))?,
            ));
        }
        Ok(Self {
            transport: builder.build(),
            from: from.parse().map_err(|e| format!("MONITOR_EMAIL_FROM: {e}"))?,
            to: to.parse().map_err(|e| format!("MONITOR_EMAIL_TO: {e}"))?,
        })
    }

    pub async fn send(&self, subject: &str, text: &str) -> Result<(), String> {
        let msg = Message::builder()
            .from(self.from.clone())
            .to(self.to.clone())
            .subject(subject)
            .body(text.to_owned())
            .map_err(|e| e.to_string())?;
        self.transport.send(msg).await.map(|_| ()).map_err(|e| e.to_string())
    }
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

pub struct Channels {
    pub brand: String,
    pub telegram: Option<Telegram>,
    pub mail: Option<Mail>,
}

#[async_trait::async_trait]
impl Deliver for Channels {
    async fn deliver(&self, notices: &[Notice]) -> bool {
        let text = rules::digest(&self.brand, notices);
        println!("{text}");
        if self.telegram.is_none() && self.mail.is_none() {
            return true; // console only (development)
        }
        let critical = notices.iter().any(|n| n.severity == Severity::Critical);
        let mut delivered = false;
        if let Some(t) = &self.telegram {
            match t.send(&text).await {
                Ok(()) => delivered = true,
                Err(e) => eprintln!("[monitor] {e}"),
            }
        }
        if let Some(m) = &self.mail
            && (critical || self.telegram.is_none())
        {
            match m.send(text.split('\n').next().unwrap_or(""), &text).await {
                Ok(()) => delivered = true,
                Err(e) => eprintln!("[monitor] e-mail: {e}"),
            }
        }
        delivered
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smtp_urls() {
        assert!(Mail::new("smtps://u%40x:p%2Fw@mail.example.ch", "ops@example.ch", "m@example.ch").is_ok());
        assert!(Mail::new("smtp://mail.example.ch:2525", "ops@example.ch", "m@example.ch").is_ok());
        assert!(Mail::new("http://mail.example.ch", "ops@example.ch", "m@example.ch").is_err());
        assert!(Mail::new("smtps://mail.example.ch", "not an address", "m@example.ch").is_err());
        assert_eq!(percent_decode("u%40x").as_deref(), Some("u@x"));
        assert_eq!(percent_decode("bad%4"), None);
    }
}
