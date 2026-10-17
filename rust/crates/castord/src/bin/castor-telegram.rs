//! castor-telegram: the Castor Telegram bot in Rust (castord::telegram).
//! Same settings and pairing as `npm run telegram`; run one of the two
//! (Telegram hands each update to one poller).
//!
//!   DATABASE_URL, TELEGRAM_BOT_TOKEN, TELEGRAM_API_URL
//!   SECRETS_ENCRYPTION_KEY (and the other keys of castord)
//!   BASE_DOMAIN, PUBLIC_SCHEME, LLM_GATEWAY_URL (http://localhost:4000), TZ
//!   CASTORD_EXTRA_CA_CERTS    a CA bundle to trust as well (the dev CA)
use std::collections::HashMap;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use castord::telegram::{Bot, Message, Update};
use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use tokio::sync::mpsc;
use zeroize::Zeroizing;

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

async fn bot() -> Result<Bot, String> {
    let token = var("TELEGRAM_BOT_TOKEN").ok_or("TELEGRAM_BOT_TOKEN is not set")?;
    let url = var("DATABASE_URL").ok_or("DATABASE_URL is not set")?;
    let extra_ca = match var("CASTORD_EXTRA_CA_CERTS") {
        Some(p) => Some(std::fs::read(&p).map_err(|e| format!("CASTORD_EXTRA_CA_CERTS {p}: {e}"))?),
        None => None,
    };
    let scheme = var("PUBLIC_SCHEME").unwrap_or_else(|| "https".into());
    if scheme != "https" && scheme != "http" {
        return Err(format!("PUBLIC_SCHEME must be http or https, not {scheme}"));
    }
    let db = PgPoolOptions::new()
        .max_connections(8)
        .connect_with(castord::db::connect_options(&url)?)
        .await
        .map_err(|e| format!("database: {e}"))?;
    Ok(Bot {
        db,
        http: castord::http::client(extra_ca.as_deref())?,
        keys: castor_crypto::Keys::from_env().map_err(|e| e.to_string())?,
        api: Zeroizing::new(format!(
            "{}/bot{token}",
            var("TELEGRAM_API_URL").unwrap_or_else(|| "https://api.telegram.org".into())
        )),
        scheme,
        base_domain: var("BASE_DOMAIN").unwrap_or_else(|| "brand.localhost".into()),
        llm_gateway: var("LLM_GATEWAY_URL").unwrap_or_else(|| "http://localhost:4000".into()),
        time_zone: var("TZ").unwrap_or_else(|| "Europe/Zurich".into()),
        assistant_origin: None,
    })
}

/// One user's messages, in order; ends after a minute without any.
async fn user_queue(bot: Arc<Bot>, mut rx: mpsc::UnboundedReceiver<Message>) {
    loop {
        let msg = match tokio::time::timeout(Duration::from_secs(60), rx.recv()).await {
            Ok(Some(m)) => m,
            Ok(None) => return,
            Err(_) => {
                // Late arrivals are still answered; new ones get a new queue.
                rx.close();
                while let Ok(m) = rx.try_recv() {
                    handle(&bot, &m).await;
                }
                return;
            }
        };
        handle(&bot, &msg).await;
    }
}

async fn handle(bot: &Bot, msg: &Message) {
    if let Err(e) = bot.handle(msg).await {
        eprintln!("telegram: message failed: {e}");
    }
}

async fn run() -> Result<(), String> {
    let bot = Arc::new(bot().await?);
    let me = bot.call("getMe", &json!({}), Duration::from_secs(30)).await?;
    eprintln!(
        "telegram: @{} polling",
        me.get("username").and_then(|u| u.as_str()).unwrap_or("?")
    );
    let stopping = Arc::new(AtomicBool::new(false));
    let on_signal = stopping.clone();
    tokio::spawn(async move {
        let Ok(mut term) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) else {
            return;
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
        on_signal.store(true, Ordering::SeqCst);
    });

    // Messages of one user are answered in order; different users in parallel.
    let mut queues: HashMap<i64, mpsc::UnboundedSender<Message>> = HashMap::new();
    let mut workers = tokio::task::JoinSet::new();
    let mut offset: i64 = 0;
    while !stopping.load(Ordering::SeqCst) {
        let polled = bot
            .call(
                "getUpdates",
                &json!({ "offset": offset, "timeout": 50, "allowed_updates": ["message"] }),
                Duration::from_secs(60),
            )
            .await
            .and_then(|v| serde_json::from_value::<Vec<Update>>(v).map_err(|e| e.to_string()));
        let updates = match polled {
            Ok(u) => u,
            Err(e) => {
                eprintln!("telegram: polling failed: {e}");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };
        for u in updates {
            offset = u.update_id + 1;
            let Some(msg) = u.message else { continue };
            let key = msg.from.as_ref().map_or(msg.chat.id, |f| f.id);
            let msg = match queues.get(&key) {
                Some(q) => match q.send(msg) {
                    Ok(()) => continue,
                    Err(mpsc::error::SendError(m)) => m,
                },
                None => msg,
            };
            let (tx, rx) = mpsc::unbounded_channel();
            let _ = tx.send(msg);
            queues.insert(key, tx);
            workers.spawn(user_queue(bot.clone(), rx));
        }
        queues.retain(|_, q| !q.is_closed());
        while workers.try_join_next().is_some() {}
    }
    drop(queues);
    while workers.join_next().await.is_some() {}
    bot.db.close().await;
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("castor-telegram: {e}");
            ExitCode::FAILURE
        }
    }
}
