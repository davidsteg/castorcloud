// The Telegram bot against the real schema, with Telegram and the assistant
// faked: pairing, a conversation, disconnecting. Runs when TEST_DATABASE_URL is set.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use castord::telegram::{Bot, Message, pairing_code_hash};
use common::*;
use zeroize::Zeroizing;

fn msg(user: i64, chat_type: &str, text: &str) -> Message {
    serde_json::from_value(serde_json::json!({
        "chat": { "id": user, "type": chat_type },
        "from": { "id": user, "username": "ana", "language_code": "en" },
        "text": text,
    }))
    .unwrap()
}

/// Every text the bot sent.
fn sent(log: &std::sync::Mutex<Vec<String>>) -> Vec<String> {
    log.lock()
        .unwrap()
        .drain(..)
        .filter(|r| r.contains("/sendMessage"))
        .map(|r| {
            let body = &r[r.find('{').unwrap()..];
            let v: serde_json::Value = serde_json::from_str(body).unwrap();
            v["text"].as_str().unwrap().to_owned()
        })
        .collect()
}

#[tokio::test]
async fn pairs_talks_and_disconnects() {
    let Some(db) = db().await else { return };
    let w = world(&db, "MAC").await;
    let (user, member) = (format!("u{}", hex8()), format!("m{}", hex8()));
    let tg_user: i64 = 4_000_000 + i64::from(u16::from_str_radix(&hex8()[..4], 16).unwrap());
    sqlx::query(r#"INSERT INTO "User" (id, "authentikSub", email) VALUES ($1, $1, $1 || '@x.test')"#)
        .bind(&user)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query(r#"INSERT INTO "Member" (id, "tenantId", "userId", "createdAt") VALUES ($1, $2, $3, now())"#)
        .bind(&member)
        .bind(&w.tenant)
        .bind(&user)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query(r#"INSERT INTO "ServiceInstance" (id, "tenantId", kind, enabled) VALUES ($1, $2, 'ASSISTANT', true)"#)
        .bind(format!("{member}a"))
        .bind(&w.tenant)
        .execute(&db)
        .await
        .unwrap();
    let key = format!("ASSISTANT_KEY_{member}");
    sqlx::query(
        r#"INSERT INTO "Secret" (id, "tenantId", key, ciphertext, "updatedAt") VALUES ($1, $2, $3, $4, now())"#,
    )
    .bind(format!("{member}s"))
    .bind(&w.tenant)
    .bind(&key)
    .bind(keys().seal_secret(&w.tenant, &key, "hk").unwrap())
    .execute(&db)
    .await
    .unwrap();
    sqlx::query(
        r#"INSERT INTO "TelegramLink" (id, "memberId", "pairingCodeHash", "pairingExpiresAt", "updatedAt")
           VALUES ($1, $1, $2, now() AT TIME ZONE 'UTC' + interval '10 minutes', now())"#,
    )
    .bind(&member)
    .bind(pairing_code_hash("K7M3Q9TX"))
    .execute(&db)
    .await
    .unwrap();

    let (tg, tg_log) = http("unused", |_, _| (200, r#"{"ok":true,"result":{}}"#.into())).await;
    let (hermes, hermes_log) = http("hk", |_, path| {
        if path.ends_with("/api/sessions") {
            (200, r#"{"id":"s1"}"#.into())
        } else {
            (
                200,
                "event: assistant.delta\ndata: {\"delta\":\"Hel\"}\n\nevent: assistant.completed\ndata: {\"content\":\" Hello from Hermes \"}\n\n".into(),
            )
        }
    })
    .await;
    let bot = Bot {
        db: db.clone(),
        http: castord::http::client(None).unwrap(),
        keys: keys(),
        api: Zeroizing::new(format!("{tg}/botTOKEN")),
        scheme: "http".into(),
        base_domain: "castor.test".into(),
        llm_gateway: "http://127.0.0.1:9".into(),
        time_zone: "Europe/Zurich".into(),
        assistant_origin: Some(hermes),
    };

    // Unknown: told how to pair; groups are ignored.
    bot.handle(&msg(tg_user, "private", "hello")).await.unwrap();
    bot.handle(&msg(tg_user, "group", "K7M3Q9TX")).await.unwrap();
    assert_eq!(
        sent(&tg_log),
        [castord::telegram::text::text(castord::telegram::text::Lang::En).welcome]
    );
    // A wrong code, then the right one (in the tenant's language from then on).
    bot.handle(&msg(tg_user, "private", "AAAA-BBBB")).await.unwrap();
    bot.handle(&msg(tg_user, "private", "/start k7m3-q9tx")).await.unwrap();
    let got = sent(&tg_log);
    assert!(got[0].starts_with("This code is unknown"));
    assert!(got[1].starts_with("Verbunden."));
    let linked = rows(
        &db,
        r#"SELECT "telegramUserId"::text || ' ' || "telegramUsername" || ' ' || ("pairingCodeHash" IS NULL)::text FROM "TelegramLink" WHERE id = $1"#,
        &member,
    )
    .await;
    assert_eq!(linked, [format!("{tg_user} ana true")]);

    // A conversation: a session is created once and kept.
    bot.handle(&msg(tg_user, "private", "Hi")).await.unwrap();
    assert_eq!(sent(&tg_log), ["Hello from Hermes"]);
    let calls: Vec<String> = hermes_log
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.split(' ').take(3).collect::<Vec<_>>().join(" "))
        .collect();
    assert_eq!(
        calls,
        [
            format!("POST /p/{member}/api/sessions auth=true"),
            format!("POST /p/{member}/api/sessions/s1/chat/stream auth=true"),
        ]
    );
    assert_eq!(
        rows(&db, r#"SELECT "sessionId" FROM "TelegramLink" WHERE id = $1"#, &member).await,
        ["s1"]
    );
    bot.handle(&msg(tg_user, "private", "/new")).await.unwrap();
    assert_eq!(sent(&tg_log), ["Neue Unterhaltung gestartet."]);

    bot.handle(&msg(tg_user, "private", "/disconnect")).await.unwrap();
    assert_eq!(sent(&tg_log), ["Getrennt. Ihr Assistent antwortet hier nicht mehr."]);
    assert!(
        rows(&db, r#"SELECT id FROM "TelegramLink" WHERE id = $1"#, &member)
            .await
            .is_empty()
    );
    let audit = rows(
        &db,
        r#"SELECT action FROM "AuditLog" WHERE "tenantId" = $1 AND action LIKE 'assistant.telegram%' ORDER BY "createdAt""#,
        &w.tenant,
    )
    .await;
    assert_eq!(audit, ["assistant.telegram_linked", "assistant.telegram_disconnect"]);

    cleanup(&db, &w).await;
    sqlx::query(r#"DELETE FROM "User" WHERE id = $1"#)
        .bind(&user)
        .execute(&db)
        .await
        .unwrap();
}
