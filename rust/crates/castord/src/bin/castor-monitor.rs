//! castor-monitor: the platform monitor (docs/monitoring.md). Run one: each
//! sends its own digest.
//!
//!   castor-monitor            every MONITOR_INTERVAL_S
//!   castor-monitor once       one round, then exit
//!   castor-monitor status     open alerts
//!   castor-monitor test       sends a test notification
//!
//!   DATABASE_URL, BASE_DOMAIN, PUBLIC_SCHEME, BRAND_NAME
//!   MONITOR_TELEGRAM_CHAT_ID + MONITOR_TELEGRAM_BOT_TOKEN (or TELEGRAM_BOT_TOKEN), TELEGRAM_API_URL
//!   MONITOR_SMTP_URL + MONITOR_EMAIL_TO, MONITOR_EMAIL_FROM
//!   MONITOR_AUTHENTIK_TOKEN, MONITOR_AUTHENTIK_URL
//!   MONITOR_INTERVAL_S (300), MONITOR_RENOTIFY_HOURS (12), MONITOR_HEARTBEAT_URL
//!   MONITOR_TLS_HOSTS, MONITOR_TLS_ADDRESS
//!   BACKUP_TARGET, BACKUP_INTERVAL_HOURS (6)
//!   CASTORD_EXTRA_CA_CERTS    a CA bundle to trust as well (the dev CA)
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use castord::monitor::deliver::{Channels, Mail, Telegram};
use castord::monitor::rules::{Kind, Notice, Severity};
use castord::monitor::{self, Deliver, Settings};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use zeroize::Zeroizing;

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn number(name: &str, default: f64) -> Result<f64, String> {
    match var(name) {
        None => Ok(default),
        Some(v) => v
            .parse::<f64>()
            .ok()
            .filter(|n| n.is_finite() && *n > 0.0)
            .ok_or_else(|| format!("{name} must be a positive number")),
    }
}

struct Setup {
    settings: Arc<Settings>,
    channels: Channels,
    http: reqwest::Client,
    interval: Duration,
    renotify_ms: i64,
    heartbeat_url: Option<String>,
}

fn setup() -> Result<Setup, String> {
    let scheme = var("PUBLIC_SCHEME").unwrap_or_else(|| "https".into());
    if scheme != "https" && scheme != "http" {
        return Err(format!("PUBLIC_SCHEME must be http or https, not {scheme}"));
    }
    let base_domain = var("BASE_DOMAIN").unwrap_or_else(|| "brand.localhost".into());
    let brand = var("BRAND_NAME").unwrap_or_else(|| "Castor Cloud".into());
    let extra_ca = match var("CASTORD_EXTRA_CA_CERTS") {
        Some(p) => Some(std::fs::read(&p).map_err(|e| format!("CASTORD_EXTRA_CA_CERTS {p}: {e}"))?),
        None => None,
    };
    let http = castord::http::client(extra_ca.as_deref())?;
    let auth_url = var("MONITOR_AUTHENTIK_URL").unwrap_or_else(|| format!("{scheme}://auth.{base_domain}"));
    let tls_hosts = var("MONITOR_TLS_HOSTS")
        .unwrap_or_else(|| format!("{base_domain},auth.{base_domain}"))
        .split(',')
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(str::to_owned)
        .collect();
    let telegram = match (
        var("MONITOR_TELEGRAM_CHAT_ID"),
        var("MONITOR_TELEGRAM_BOT_TOKEN").or_else(|| var("TELEGRAM_BOT_TOKEN")),
    ) {
        (Some(chat_id), Some(token)) => Some(Telegram {
            api: var("TELEGRAM_API_URL").unwrap_or_else(|| "https://api.telegram.org".into()),
            token: Zeroizing::new(token),
            chat_id,
            http: http.clone(),
        }),
        _ => None,
    };
    let mail = match (var("MONITOR_SMTP_URL"), var("MONITOR_EMAIL_TO")) {
        (Some(url), Some(to)) => {
            let from = var("MONITOR_EMAIL_FROM").unwrap_or_else(|| format!("{brand} monitor <monitor@{base_domain}>"));
            Some(Mail::new(&url, &to, &from)?)
        }
        _ => None,
    };
    let platform = castord::render::Platform {
        base_domain,
        scheme,
        brand_name: brand.clone(),
        time_zone: var("TZ").unwrap_or_else(|| "Europe/Zurich".into()),
        auth_host_ip: var("AUTH_HOST_IP").unwrap_or_else(|| "host-gateway".into()),
    };
    #[allow(clippy::cast_possible_truncation)]
    let renotify_ms = (number("MONITOR_RENOTIFY_HOURS", 12.0)? * 3_600_000.0) as i64;
    Ok(Setup {
        settings: Arc::new(Settings {
            platform,
            auth_url,
            authentik_token: var("MONITOR_AUTHENTIK_TOKEN").map(Zeroizing::new),
            backups_on: var("BACKUP_TARGET").is_some(),
            backup_interval_hours: number("BACKUP_INTERVAL_HOURS", 6.0)?,
            tls_hosts,
            tls_address: var("MONITOR_TLS_ADDRESS"),
            roots: monitor::tls::roots(extra_ca.as_deref())?,
        }),
        channels: Channels { brand, telegram, mail },
        http,
        interval: Duration::from_secs_f64(number("MONITOR_INTERVAL_S", 300.0)?),
        renotify_ms,
        heartbeat_url: var("MONITOR_HEARTBEAT_URL"),
    })
}

async fn connect() -> Result<PgPool, String> {
    let url = var("DATABASE_URL").ok_or("DATABASE_URL is not set")?;
    PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(10))
        .connect_with(castord::db::connect_options(&url)?)
        .await
        .map_err(|e| format!("database: {e}"))
}

async fn round(s: &Setup, db: &PgPool, checks: &[Arc<dyn monitor::Check>]) -> Result<(), String> {
    let now = monitor::now_ms();
    let r = monitor::round(db, checks, &s.channels, s.renotify_ms, now).await?;
    eprintln!(
        "[monitor] {}: {} finding(s), {} open alert(s), {} notice(s){}",
        monitor::rules::iso(now),
        r.findings,
        r.open,
        r.notices.len(),
        if r.skipped.is_empty() {
            String::new()
        } else {
            format!(", skipped: {}", r.skipped.join(", "))
        }
    );
    // Dead-man's switch: an outside service that alerts when these pings stop
    // (the monitor itself, or the whole control plane, is down). Only a URL
    // is sent, no data.
    if let Some(url) = &s.heartbeat_url
        && let Err(e) = s.http.get(url).timeout(Duration::from_secs(10)).send().await
    {
        eprintln!("[monitor] heartbeat: {}", e.without_url());
    }
    Ok(())
}

async fn status(db: &PgPool) -> Result<(), String> {
    let open: Vec<(String, String, String, Option<serde_json::Value>)> = sqlx::query_as(
        r#"SELECT severity::text, to_char("firstSeenAt", 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'), summary, details
           FROM "Alert" WHERE "resolvedAt" IS NULL ORDER BY severity DESC, "firstSeenAt""#,
    )
    .fetch_all(db)
    .await
    .map_err(|e| e.to_string())?;
    if open.is_empty() {
        println!("no open alerts");
    }
    for (severity, since, summary, details) in open {
        println!("{severity:<8} since {since}  {summary}");
        if let Some(d) = details {
            println!("         {d}");
        }
    }
    Ok(())
}

async fn run() -> Result<ExitCode, String> {
    let command = std::env::args().nth(1).unwrap_or_else(|| "run".into());
    let s = setup()?;
    match command.as_str() {
        "status" => {
            status(&connect().await?).await?;
            return Ok(ExitCode::SUCCESS);
        }
        "test" => {
            let ok = s
                .channels
                .deliver(&[Notice {
                    kind: Kind::New,
                    severity: Severity::Critical,
                    summary: "Test alert: the monitor can reach you".into(),
                }])
                .await;
            println!("{}", if ok { "sent" } else { "NOT sent (see errors above)" });
            return Ok(if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE });
        }
        "once" | "run" => {}
        _ => {
            eprintln!("usage: castor-monitor [run|once|status|test]");
            return Ok(ExitCode::from(2));
        }
    }
    let db = connect().await?;
    let checks = monitor::all_checks(&db, &s.http, &s.settings);
    if command == "once" {
        round(&s, &db, &checks).await?;
        return Ok(ExitCode::SUCCESS);
    }
    if s.channels.telegram.is_none() && s.channels.mail.is_none() {
        eprintln!(
            "[monitor] no alert channel configured (MONITOR_TELEGRAM_CHAT_ID or MONITOR_SMTP_URL + MONITOR_EMAIL_TO): alerts only go to this log"
        );
    }
    // The database holds the alert state. If it is down, say so directly (at
    // most every MONITOR_RENOTIFY_HOURS), since nothing else can.
    let renotify = Duration::from_millis(u64::try_from(s.renotify_ms).unwrap_or(u64::MAX));
    let mut db_down_notified: Option<Instant> = None;
    loop {
        let started = Instant::now();
        match round(&s, &db, &checks).await {
            Ok(()) => db_down_notified = None,
            Err(e) => {
                eprintln!("[monitor] round failed: {e}");
                if db_down_notified.is_none_or(|t| t.elapsed() >= renotify) {
                    let ok = s
                        .channels
                        .deliver(&[Notice {
                            kind: Kind::New,
                            severity: Severity::Critical,
                            summary: "The monitor cannot complete its checks (control-plane database?)".into(),
                        }])
                        .await;
                    if ok {
                        db_down_notified = Some(Instant::now());
                    }
                }
            }
        }
        let wait = s
            .interval
            .saturating_sub(started.elapsed())
            .max(Duration::from_secs(10));
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            _ = tokio::signal::ctrl_c() => return Ok(ExitCode::SUCCESS),
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("castor-monitor: {e}");
            ExitCode::FAILURE
        }
    }
}
