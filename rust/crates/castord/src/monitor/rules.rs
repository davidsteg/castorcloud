//! The monitor's rules (monitor-rules.ts): turn measured state into findings,
//! and findings into alert changes and notifications. Pure functions, so they
//! are tested without a database or network (the monitor gathers the inputs).
//!
//! Alert text (`summary`) may be sent to Telegram, which is outside
//! Switzerland: it names nodes, tenant slugs, hosts and counts, never e-mail
//! addresses, usernames, IPs or error output. Those go in `details`, which
//! stays in the control-plane database.
//!
//! Times are milliseconds since the epoch.

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Warning,
    Critical,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Warning => "WARNING",
            Self::Critical => "CRITICAL",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "WARNING" => Some(Self::Warning),
            "CRITICAL" => Some(Self::Critical),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    /// Stable identity; its first segment is the check's scope ("node", "service", ...).
    pub key: String,
    pub severity: Severity,
    pub summary: String,
    pub details: Option<Value>,
    /// A one-off event: notified once, never re-fires, never "resolves".
    pub once: bool,
    /// Consecutive rounds the condition must hold before it alerts.
    pub after: u32,
}

impl Finding {
    pub fn new(key: impl Into<String>, severity: Severity, summary: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            severity,
            summary: summary.into(),
            details: None,
            once: false,
            after: 1,
        }
    }

    pub fn details(mut self, d: Value) -> Self {
        self.details = Some(d);
        self
    }

    pub fn once(mut self) -> Self {
        self.once = true;
        self
    }

    pub fn after(mut self, n: u32) -> Self {
        self.after = n;
        self
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Thresholds {
    /// A node not probed successfully for this long is unreachable.
    pub node_stale_min: f64,
    /// Free pool space below these percentages.
    pub space_warn_pct: f64,
    pub space_crit_pct: f64,
    pub cert_warn_days: f64,
    pub cert_crit_days: f64,
    pub job_stuck_min: f64,
    pub queue_wait_min: f64,
    /// Failed sign-ins within `login_window_min`.
    pub login_window_min: i64,
    pub login_fail_total: usize,
    pub login_fail_per_source: usize,
    pub login_fail_per_account: usize,
}

pub const DEFAULT_THRESHOLDS: Thresholds = Thresholds {
    node_stale_min: 15.0,
    space_warn_pct: 15.0,
    space_crit_pct: 10.0,
    cert_warn_days: 14.0,
    cert_crit_days: 3.0,
    job_stuck_min: 180.0,
    queue_wait_min: 60.0,
    login_window_min: 15,
    login_fail_total: 30,
    login_fail_per_source: 10,
    login_fail_per_account: 10,
};

const MIN: f64 = 60_000.0;
const DAY: f64 = 24.0 * 60.0 * MIN;

/// Math.round.
#[allow(clippy::cast_possible_truncation)]
fn round(x: f64) -> i64 {
    (x + 0.5).floor() as i64
}

#[allow(clippy::cast_precision_loss)]
fn f(ms: i64) -> f64 {
    ms as f64
}

/// A time as JSON.stringify writes a Date: 2026-09-28T12:00:00.000Z.
pub fn iso(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (y, m, d) = crate::retention::civil(secs.div_euclid(86_400));
    let s = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        s / 3600,
        s % 3600 / 60,
        s % 60,
        ms.rem_euclid(1000)
    )
}

fn iso_opt(ms: Option<i64>) -> Value {
    ms.map_or(Value::Null, |t| Value::String(iso(t)))
}

// ------------------------------------------------------------------ nodes

#[derive(Debug, Clone)]
pub struct NodeState {
    pub name: String,
    pub kind: String,
    pub enabled: bool,
    pub tenants: i64,
    pub last_seen_at: Option<i64>,
    pub last_error: Option<String>,
    pub pool_size_mb: Option<i64>,
    pub pool_used_mb: Option<i64>,
    pub pool_health: Option<String>,
}

/// Remote nodes the watcher probes. The local development node is never probed.
pub fn node_findings(nodes: &[NodeState], now: i64, t: &Thresholds) -> Vec<Finding> {
    let mut out = Vec::new();
    for n in nodes {
        if n.kind == "LOCAL" {
            continue;
        }
        // A drained node without tenants may be switched off on purpose.
        if !n.enabled && n.tenants == 0 {
            continue;
        }
        let stale = n.last_seen_at.map_or(f64::INFINITY, |s| f(now - s));
        if stale > t.node_stale_min * MIN {
            let summary = if n.last_seen_at.is_some() {
                format!(
                    "Node {} unreachable for {} min ({} tenants)",
                    n.name,
                    round(stale / MIN),
                    n.tenants
                )
            } else {
                format!("Node {} has never been reached", n.name)
            };
            out.push(
                Finding::new(format!("node:{}:reachable", n.name), Severity::Critical, summary)
                    .details(json!({ "lastSeenAt": iso_opt(n.last_seen_at), "lastError": n.last_error })),
            );
            continue; // pool facts are stale too
        }
        if let Some(h) = n.pool_health.as_deref().filter(|h| !h.is_empty() && *h != "ONLINE") {
            out.push(Finding::new(
                format!("node:{}:pool", n.name),
                Severity::Critical,
                format!("Node {}: ZFS pool is {h}", n.name),
            ));
        }
        if let (Some(size), Some(used)) = (n.pool_size_mb.filter(|s| *s != 0), n.pool_used_mb) {
            let free_pct = 100.0 * f(size - used) / f(size);
            if free_pct < t.space_warn_pct {
                out.push(
                    Finding::new(
                        format!("node:{}:space", n.name),
                        if free_pct < t.space_crit_pct {
                            Severity::Critical
                        } else {
                            Severity::Warning
                        },
                        format!(
                            "Node {}: {free_pct:.1}% pool space free ({} GB)",
                            n.name,
                            round(f(size - used) / 1024.0)
                        ),
                    )
                    .details(json!({ "poolSizeMb": size, "poolUsedMb": used })),
                );
            }
        }
    }
    out
}

// --------------------------------------------------------- tenant services

#[derive(Debug, Clone)]
pub struct ProbeResult {
    pub slug: String,
    pub service: String,
    pub ok: bool,
    pub status: Option<u16>,
    pub error: Option<String>,
}

/// A service must fail two rounds in a row: one slow answer is not an outage.
pub fn service_findings(probes: &[ProbeResult]) -> Vec<Finding> {
    probes
        .iter()
        .filter(|p| !p.ok)
        .map(|p| {
            let why = p.status.map_or_else(|| "no answer".to_owned(), |s| format!("HTTP {s}"));
            Finding::new(
                format!("service:{}:{}", p.slug, p.service),
                Severity::Critical,
                format!("{} of {} is down ({why})", p.service, p.slug),
            )
            .details(json!({ "status": p.status, "error": p.error }))
            .after(2)
        })
        .collect()
}

// ------------------------------------------------------------------- jobs

#[derive(Debug, Clone)]
pub struct JobState {
    pub id: String,
    pub kind: String,
    pub slug: String,
    /// QUEUED, RUNNING or FAILED.
    pub status: String,
    pub created_at: i64,
    pub error: Option<String>,
}

pub fn job_findings(jobs: &[JobState], now: i64, t: &Thresholds) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut waiting: Vec<&str> = Vec::new();
    for j in jobs {
        let age_min = f(now - j.created_at) / MIN;
        match j.status.as_str() {
            "FAILED" => out.push(
                Finding::new(
                    format!("job:{}:failed", j.id),
                    Severity::Critical,
                    format!("{} job for {} failed after all retries", j.kind, j.slug),
                )
                .details(json!({ "jobId": j.id, "error": j.error }))
                .once(),
            ),
            "RUNNING" if age_min > t.job_stuck_min => out.push(
                Finding::new(
                    format!("job:{}:stuck", j.id),
                    Severity::Warning,
                    format!(
                        "{} job for {} has been running for {} h",
                        j.kind,
                        j.slug,
                        round(age_min / 60.0)
                    ),
                )
                .details(json!({ "jobId": j.id })),
            ),
            "QUEUED" if age_min > t.queue_wait_min => waiting.push(&j.id),
            _ => {}
        }
    }
    if !waiting.is_empty() {
        out.push(
            Finding::new(
                "job:backlog",
                Severity::Warning,
                format!(
                    "{} provisioning job(s) waiting for over {} min",
                    waiting.len(),
                    t.queue_wait_min
                ),
            )
            .details(json!({ "jobs": waiting })),
        );
    }
    out
}

// ---------------------------------------------------------------- backups

#[derive(Debug, Clone)]
pub struct BackupState {
    pub slug: String,
    pub tenant_created_at: i64,
    pub last_success_at: Option<i64>,
    pub failures: i64,
    pub last_error: Option<String>,
    pub verify_error: Option<String>,
}

/// Backups run every `interval_hours`; one missed run is tolerated. A tenant
/// without any backup gets a day for its first (full) send.
pub fn backup_findings(rows: &[BackupState], interval_hours: f64, now: i64) -> Vec<Finding> {
    let mut out = Vec::new();
    let overdue = 2.0 * interval_hours * 3_600_000.0 + 30.0 * MIN;
    for b in rows {
        let since = b.last_success_at.unwrap_or(b.tenant_created_at);
        let grace = if b.last_success_at.is_some() {
            overdue
        } else {
            overdue.max(DAY)
        };
        let age = f(now - since);
        if age > grace {
            let summary = if b.last_success_at.is_some() {
                format!(
                    "No backup of {} for {} h ({} failed runs)",
                    b.slug,
                    round(age / 3_600_000.0),
                    b.failures
                )
            } else {
                format!("{} has never been backed up", b.slug)
            };
            out.push(
                Finding::new(format!("backup:{}:overdue", b.slug), Severity::Critical, summary)
                    .details(json!({ "lastSuccessAt": iso_opt(b.last_success_at), "lastError": b.last_error })),
            );
        } else if b.failures >= 2 {
            out.push(
                Finding::new(
                    format!("backup:{}:failing", b.slug),
                    Severity::Warning,
                    format!("Backup of {} failed {} times in a row", b.slug, b.failures),
                )
                .details(json!({ "lastError": b.last_error })),
            );
        }
        if let Some(e) = &b.verify_error {
            out.push(
                Finding::new(
                    format!("backup:{}:verify", b.slug),
                    Severity::Critical,
                    format!("Backup of {} failed verification", b.slug),
                )
                .details(json!({ "verifyError": e })),
            );
        }
    }
    out
}

// ------------------------------------------------------------ certificates

#[derive(Debug, Clone)]
pub struct CertResult {
    pub host: String,
    pub valid_to: Option<i64>,
    pub trusted: bool,
    pub error: Option<String>,
}

pub fn cert_findings(results: &[CertResult], now: i64, t: &Thresholds) -> Vec<Finding> {
    let mut out = Vec::new();
    for c in results {
        let key = format!("cert:{}", c.host);
        let Some(valid_to) = c.valid_to else {
            out.push(
                Finding::new(
                    key,
                    Severity::Critical,
                    format!("TLS on {} could not be checked", c.host),
                )
                .details(json!({ "error": c.error }))
                .after(2),
            );
            continue;
        };
        let days = f(valid_to - now) / DAY;
        if days < 0.0 {
            out.push(
                Finding::new(key, Severity::Critical, format!("Certificate for {} expired", c.host))
                    .details(json!({ "validTo": iso(valid_to) })),
            );
        } else if !c.trusted {
            out.push(
                Finding::new(
                    key,
                    Severity::Critical,
                    format!("Certificate for {} is not trusted", c.host),
                )
                .details(json!({ "error": c.error, "validTo": iso(valid_to) })),
            );
        } else if days < t.cert_warn_days {
            #[allow(clippy::cast_possible_truncation)]
            let whole = days.floor() as i64;
            out.push(
                Finding::new(
                    key,
                    if days < t.cert_crit_days {
                        Severity::Critical
                    } else {
                        Severity::Warning
                    },
                    format!(
                        "Certificate for {} expires in {whole} day{}",
                        c.host,
                        if whole == 1 { "" } else { "s" }
                    ),
                )
                .details(json!({ "validTo": iso(valid_to) })),
            );
        }
    }
    out
}

// --------------------------------------------------------------- security

/// The parts of an Authentik event the rules read.
#[derive(Debug, Clone)]
pub struct AuthEvent {
    pub pk: String,
    pub action: String,
    pub created: i64,
    pub client_ip: Option<String>,
    /// The account signed in to, or the one a failed sign-in named.
    pub username: Option<String>,
    pub user_pk: Option<i64>,
}

/// Counts per key, most first (ties in first-seen order, as a stable sort).
fn count_by<'a, T>(items: &[&'a T], key: impl Fn(&'a T) -> Option<String>) -> Vec<(String, usize)> {
    let mut order: Vec<(String, usize)> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for i in items {
        if let Some(k) = key(i).filter(|k| !k.is_empty()) {
            match index.get(&k) {
                Some(&at) => {
                    if let Some(e) = order.get_mut(at) {
                        e.1 += 1;
                    }
                }
                None => {
                    index.insert(k.clone(), order.len());
                    order.push((k, 1));
                }
            }
        }
    }
    order.sort_by_key(|e| std::cmp::Reverse(e.1));
    order
}

fn pairs(v: &[(String, usize)]) -> Value {
    Value::Array(v.iter().take(10).map(|(k, n)| json!([k, n])).collect())
}

/// Sign-in abuse and admin activity from Authentik's event log. Bursts are
/// conditions over a sliding window (they resolve when it quiets down);
/// impersonation and admin sign-ins are one-off events.
pub fn auth_findings(events: &[AuthEvent], superusers: &HashSet<i64>, now: i64, t: &Thresholds) -> Vec<Finding> {
    let mut out = Vec::new();
    let window = t.login_window_min;
    let start = now - window * 60_000;
    let recent: Vec<&AuthEvent> = events.iter().filter(|e| e.created >= start).collect();

    let failed: Vec<&AuthEvent> = recent.iter().copied().filter(|e| e.action == "login_failed").collect();
    let by_source = count_by(&failed, |e| e.client_ip.clone());
    let by_account = count_by(&failed, |e| e.username.as_ref().map(|u| u.to_lowercase()));
    let top = json!({ "sources": pairs(&by_source), "accounts": pairs(&by_account) });
    if failed.len() >= t.login_fail_total {
        out.push(
            Finding::new(
                "auth:login-failed",
                Severity::Warning,
                format!(
                    "{} failed sign-ins in {window} min from {} address(es)",
                    failed.len(),
                    by_source.len()
                ),
            )
            .details(top.clone()),
        );
    }
    let hot: Vec<&(String, usize)> = by_source
        .iter()
        .filter(|(_, n)| *n >= t.login_fail_per_source)
        .collect();
    if let Some(max) = hot.first() {
        out.push(
            Finding::new(
                "auth:login-failed-source",
                Severity::Warning,
                format!(
                    "{} address(es) with {}+ failed sign-ins in {window} min (max {})",
                    hot.len(),
                    t.login_fail_per_source,
                    max.1
                ),
            )
            .details(top.clone()),
        );
    }
    let hot: Vec<&(String, usize)> = by_account
        .iter()
        .filter(|(_, n)| *n >= t.login_fail_per_account)
        .collect();
    if let Some(max) = hot.first() {
        out.push(
            Finding::new(
                "auth:login-failed-account",
                Severity::Warning,
                format!(
                    "{} account(s) with {}+ failed sign-ins in {window} min (max {})",
                    hot.len(),
                    t.login_fail_per_account,
                    max.1
                ),
            )
            .details(top),
        );
    }
    let suspicious: Vec<&AuthEvent> = recent
        .iter()
        .copied()
        .filter(|e| e.action == "suspicious_request")
        .collect();
    if !suspicious.is_empty() {
        out.push(
            Finding::new(
                "auth:suspicious",
                Severity::Warning,
                format!(
                    "Authentik flagged {} suspicious request(s) in {window} min",
                    suspicious.len()
                ),
            )
            .details(json!({ "sources": pairs(&count_by(&suspicious, |e| e.client_ip.clone())) })),
        );
    }

    // One-off events: every one in the fetched range (deduplicated by key).
    for e in events {
        if e.action == "impersonation_started" {
            out.push(
                Finding::new(
                    format!("auth:event:{}", e.pk),
                    Severity::Critical,
                    "An Authentik admin started impersonating a user",
                )
                .details(json!({ "event": e.pk, "by": e.username, "clientIp": e.client_ip, "at": iso(e.created) }))
                .once(),
            );
        } else if e.action == "login" && e.user_pk.is_some_and(|pk| superusers.contains(&pk)) {
            out.push(
                Finding::new(
                    format!("auth:event:{}", e.pk),
                    Severity::Warning,
                    "Admin sign-in to Authentik",
                )
                .details(
                    json!({ "event": e.pk, "username": e.username, "clientIp": e.client_ip, "at": iso(e.created) }),
                )
                .once(),
            );
        }
    }
    out
}

/// Audit log actions the operator hears about the moment they happen.
pub const AUDITED_ACTIONS: [&str; 3] = ["staff.break_glass", "backup.box_unhealthy", "backup.restored"];

fn audited(action: &str, slug: Option<&str>) -> Option<(Severity, String)> {
    let to = |prefix: &str| slug.map_or_else(String::new, |s| format!(" {prefix} {s}"));
    match action {
        "staff.break_glass" => Some((Severity::Critical, format!("Staff break-glass access{}", to("to")))),
        "backup.box_unhealthy" => Some((Severity::Critical, "The backup box's pool is not healthy".into())),
        "backup.restored" => Some((Severity::Warning, format!("A backup was restored{}", to("for")))),
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub struct AuditRow {
    pub id: String,
    pub action: String,
    pub actor: String,
    pub slug: Option<String>,
    pub created_at: i64,
    pub details: Option<Value>,
}

pub fn audit_findings(rows: &[AuditRow]) -> Vec<Finding> {
    rows.iter()
        .filter_map(|r| {
            let (severity, summary) = audited(&r.action, r.slug.as_deref())?;
            Some(
                Finding::new(format!("audit:{}", r.id), severity, summary)
                    .details(json!({
                        "action": r.action, "actor": r.actor, "at": iso(r.created_at), "details": r.details,
                    }))
                    .once(),
            )
        })
        .collect()
}

// -------------------------------------------------------------- reconcile

#[derive(Debug, Clone)]
pub struct StoredAlert {
    pub key: String,
    pub severity: Severity,
    pub summary: String,
    pub once: bool,
    pub hits: i32,
    pub notified_at: Option<i64>,
    pub resolved_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    New,
    Escalated,
    Reminder,
    Resolved,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    Upsert {
        finding: Finding,
        hits: i32,
        reopen: bool,
        notify: Option<Kind>,
    },
    Resolve {
        key: String,
        notify: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Notice {
    pub kind: Kind,
    pub severity: Severity,
    pub summary: String,
}

pub fn scope_of(key: &str) -> &str {
    key.split(':').next().unwrap_or(key)
}

/// Compares this round's findings with the stored alerts, in `stored`'s
/// order. `stored` holds every unresolved alert plus any stored row whose key
/// is among the findings. `skipped` lists scopes whose check could not run
/// this round: their open alerts stay open rather than being reported resolved.
pub fn reconcile(
    findings: &[Finding],
    stored: &[StoredAlert],
    now: i64,
    renotify_ms: i64,
    skipped: &HashSet<String>,
) -> (Vec<Change>, Vec<Notice>) {
    let by_key: HashMap<&str, &StoredAlert> = stored.iter().map(|s| (s.key.as_str(), s)).collect();
    let mut changes = Vec::new();
    let mut notices = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();

    for fd in findings {
        if !seen.insert(fd.key.as_str()) {
            continue;
        }
        let s = by_key.get(fd.key.as_str()).copied();
        if fd.once && s.is_some_and(|s| s.notified_at.is_some()) {
            continue; // already reported
        }
        let open = s.filter(|s| s.resolved_at.is_none());
        let hits = open.map_or(1, |s| s.hits.saturating_add(1));
        let mut notify = None;
        if hits >= i32::try_from(fd.after).unwrap_or(i32::MAX) {
            notify = match open.and_then(|s| s.notified_at.map(|n| (s, n))) {
                None => Some(Kind::New),
                Some((s, _)) if s.severity == Severity::Warning && fd.severity == Severity::Critical => {
                    Some(Kind::Escalated)
                }
                Some((_, n)) if now - n >= renotify_ms => Some(Kind::Reminder),
                Some(_) => None,
            };
        }
        if let Some(kind) = notify {
            notices.push(Notice {
                kind,
                severity: fd.severity,
                summary: fd.summary.clone(),
            });
        }
        changes.push(Change::Upsert {
            finding: fd.clone(),
            hits,
            reopen: open.is_none(),
            notify,
        });
    }

    for s in stored {
        if s.resolved_at.is_some() || seen.contains(s.key.as_str()) || s.once {
            continue;
        }
        if skipped.contains(scope_of(&s.key)) {
            continue;
        }
        // Only announce the end of something that was announced.
        changes.push(Change::Resolve {
            key: s.key.clone(),
            notify: s.notified_at.is_some(),
        });
        if s.notified_at.is_some() {
            notices.push(Notice {
                kind: Kind::Resolved,
                severity: s.severity,
                summary: s.summary.clone(),
            });
        }
    }
    (changes, notices)
}

// ----------------------------------------------------------------- digest

const MAX_LINES: usize = 25;

/// One message per round; plain text (no markup, so nothing in it is interpreted).
pub fn digest(brand: &str, notices: &[Notice]) -> String {
    let order = |k: Kind| match k {
        Kind::New | Kind::Escalated => 0,
        Kind::Reminder => 1,
        Kind::Resolved => 2,
    };
    let mut sorted: Vec<&Notice> = notices.iter().collect();
    sorted.sort_by_key(|n| (order(n.kind), n.severity != Severity::Critical));
    let mut lines: Vec<String> = sorted
        .iter()
        .take(MAX_LINES)
        .map(|n| {
            let icon = match n.severity {
                Severity::Critical => "🔴",
                Severity::Warning => "🟠",
            };
            match n.kind {
                Kind::Resolved => format!("✅ Resolved: {}", n.summary),
                Kind::Reminder => format!("{icon} {} (still)", n.summary),
                Kind::Escalated => format!("{icon} {} (now critical)", n.summary),
                Kind::New => format!("{icon} {}", n.summary),
            }
        })
        .collect();
    if sorted.len() > MAX_LINES {
        lines.push(format!("… and {} more", sorted.len() - MAX_LINES));
    }
    let firing = notices.iter().filter(|n| n.kind != Kind::Resolved).count();
    let head = if firing > 0 {
        format!("{brand} monitor: {firing} alert(s)")
    } else {
        format!("{brand} monitor: {} resolved", notices.len())
    };
    let mut out = vec![head, String::new()];
    out.extend(lines);
    out.join("\n")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// 2026-09-28T12:00:00Z
    const NOW: i64 = 1_790_596_800_000;
    const T: Thresholds = DEFAULT_THRESHOLDS;
    const GB: i64 = 1024;

    fn ago(min: i64) -> i64 {
        NOW - min * 60_000
    }

    fn keys(f: &[Finding]) -> Vec<(&str, Severity)> {
        f.iter().map(|x| (x.key.as_str(), x.severity)).collect()
    }

    fn node() -> NodeState {
        NodeState {
            name: "mini-1".into(),
            kind: "MAC".into(),
            enabled: true,
            tenants: 3,
            last_seen_at: Some(ago(2)),
            last_error: None,
            pool_size_mb: Some(1000 * GB),
            pool_used_mb: Some(500 * GB),
            pool_health: Some("ONLINE".into()),
        }
    }

    #[test]
    fn times_as_json() {
        assert_eq!(iso(NOW + 7), "2026-09-28T12:00:00.007Z");
    }

    #[test]
    fn healthy_and_local_nodes_raise_nothing() {
        let local = NodeState {
            name: "local".into(),
            kind: "LOCAL".into(),
            last_seen_at: None,
            ..node()
        };
        assert!(node_findings(&[node(), local], NOW, &T).is_empty());
    }

    #[test]
    fn unreachable_node() {
        let n = NodeState {
            last_seen_at: Some(ago(30)),
            pool_health: Some("DEGRADED".into()),
            last_error: Some("ssh: timeout".into()),
            ..node()
        };
        let f = node_findings(&[n], NOW, &T);
        assert_eq!(keys(&f), [("node:mini-1:reachable", Severity::Critical)]);
        assert_eq!(f[0].summary, "Node mini-1 unreachable for 30 min (3 tenants)");
        assert!(!f[0].summary.contains("ssh"));
    }

    #[test]
    fn drained_node_may_be_off() {
        let n = NodeState {
            enabled: false,
            tenants: 0,
            last_seen_at: None,
            ..node()
        };
        assert!(node_findings(&[n], NOW, &T).is_empty());
    }

    #[test]
    fn pool_health_and_space() {
        let n = NodeState {
            pool_health: Some("DEGRADED".into()),
            pool_used_mb: Some(880 * GB),
            ..node()
        };
        let f = node_findings(&[n], NOW, &T);
        assert_eq!(
            keys(&f),
            [
                ("node:mini-1:pool", Severity::Critical),
                ("node:mini-1:space", Severity::Warning)
            ]
        );
        assert_eq!(f[1].summary, "Node mini-1: 12.0% pool space free (120 GB)");
        let n = NodeState {
            pool_used_mb: Some(950 * GB),
            ..node()
        };
        assert_eq!(node_findings(&[n], NOW, &T)[0].severity, Severity::Critical);
    }

    #[test]
    fn services_need_two_rounds() {
        let p = |service: &str, ok, status| ProbeResult {
            slug: "acme".into(),
            service: service.into(),
            ok,
            status: Some(status),
            error: None,
        };
        let f = service_findings(&[p("photos", true, 200), p("docs", false, 502)]);
        assert_eq!(keys(&f), [("service:acme:docs", Severity::Critical)]);
        assert_eq!(f[0].after, 2);
        assert_eq!(f[0].summary, "docs of acme is down (HTTP 502)");
    }

    #[test]
    fn jobs() {
        let j = |id: &str, status: &str, min, error: Option<&str>| JobState {
            id: id.into(),
            kind: "UPDATE".into(),
            slug: "acme".into(),
            status: status.into(),
            created_at: ago(min),
            error: error.map(Into::into),
        };
        let f = job_findings(
            &[
                j("j1", "FAILED", 10, Some("boom")),
                j("j2", "RUNNING", 200, None),
                j("j3", "RUNNING", 20, None),
                j("j4", "QUEUED", 90, None),
            ],
            NOW,
            &T,
        );
        let k: Vec<&str> = f.iter().map(|x| x.key.as_str()).collect();
        assert_eq!(k, ["job:j1:failed", "job:j2:stuck", "job:backlog"]);
        assert!(f[0].once);
        assert!(!f[0].summary.contains("boom"));
        assert_eq!(f[1].summary, "UPDATE job for acme has been running for 3 h");
        assert_eq!(f[2].summary, "1 provisioning job(s) waiting for over 60 min");
    }

    #[test]
    fn backups() {
        let b = |slug: &str, last: Option<i64>, created: i64, failures, verify: Option<&str>| BackupState {
            slug: slug.into(),
            tenant_created_at: created,
            last_success_at: last,
            failures,
            last_error: None,
            verify_error: verify.map(Into::into),
        };
        let month = ago(60 * 24 * 30);
        let f = backup_findings(
            &[
                b("ok", Some(ago(60)), month, 0, None),
                b("late", Some(ago(60 * 13)), month, 2, None),
                b("flaky", Some(ago(60 * 5)), month, 2, None),
                b("bad", Some(ago(60)), month, 0, Some("checksum")),
                b("new", None, ago(60 * 20), 0, None),
                b("never", None, ago(60 * 25), 0, None),
            ],
            6.0,
            NOW,
        );
        assert_eq!(
            keys(&f),
            [
                ("backup:late:overdue", Severity::Critical),
                ("backup:flaky:failing", Severity::Warning),
                ("backup:bad:verify", Severity::Critical),
                ("backup:never:overdue", Severity::Critical),
            ]
        );
        assert_eq!(f[0].summary, "No backup of late for 13 h (2 failed runs)");
    }

    #[test]
    fn certificates() {
        let days = |d: i64| NOW + d * 86_400_000;
        let c = |host: &str, valid_to: Option<i64>, trusted| CertResult {
            host: host.into(),
            valid_to,
            trusted,
            error: None,
        };
        let f = cert_findings(
            &[
                c("a", Some(days(60)), true),
                c("b", Some(days(10)), true),
                c("c", Some(days(2)), true),
                c("d", Some(days(60)), false),
                c("e", Some(days(-1)), false),
                c("f", None, false),
            ],
            NOW,
            &T,
        );
        assert_eq!(
            keys(&f),
            [
                ("cert:b", Severity::Warning),
                ("cert:c", Severity::Critical),
                ("cert:d", Severity::Critical),
                ("cert:e", Severity::Critical),
                ("cert:f", Severity::Critical),
            ]
        );
        assert_eq!(f[0].summary, "Certificate for b expires in 10 days");
        assert_eq!(f[3].summary, "Certificate for e expired");
    }

    fn ev(pk: i64, action: &str) -> AuthEvent {
        AuthEvent {
            pk: pk.to_string(),
            action: action.into(),
            created: ago(1),
            client_ip: Some("203.0.113.9".into()),
            username: Some("someone@example.com".into()),
            user_pk: None,
        }
    }

    #[test]
    fn failed_sign_in_bursts() {
        let failed: Vec<AuthEvent> = (0..12).map(|i| ev(i, "login_failed")).collect();
        let f = auth_findings(&failed, &HashSet::new(), NOW, &T);
        let k: Vec<&str> = f.iter().map(|x| x.key.as_str()).collect();
        assert_eq!(k, ["auth:login-failed-source", "auth:login-failed-account"]);
        for x in &f {
            assert!(!x.summary.contains("203.0.113") && !x.summary.contains("example.com"));
            assert!(x.details.as_ref().unwrap().to_string().contains("203.0.113.9"));
        }
        assert_eq!(
            f[0].summary,
            "1 address(es) with 10+ failed sign-ins in 15 min (max 12)"
        );
        let spread: Vec<AuthEvent> = (0..35)
            .map(|i| AuthEvent {
                client_ip: Some(format!("198.51.100.{i}")),
                username: Some(format!("u{i}")),
                ..ev(i, "login_failed")
            })
            .collect();
        let f = auth_findings(&spread, &HashSet::new(), NOW, &T);
        assert_eq!(keys(&f), [("auth:login-failed", Severity::Warning)]);
        assert_eq!(f[0].summary, "35 failed sign-ins in 15 min from 35 address(es)");
        let old: Vec<AuthEvent> = (0..35)
            .map(|i| AuthEvent {
                created: ago(30),
                ..ev(i, "login_failed")
            })
            .collect();
        assert!(auth_findings(&old, &HashSet::new(), NOW, &T).is_empty());
    }

    #[test]
    fn admin_sign_ins_and_impersonation() {
        let f = auth_findings(
            &[
                AuthEvent {
                    user_pk: Some(7),
                    ..ev(1, "login")
                },
                AuthEvent {
                    user_pk: Some(8),
                    ..ev(2, "login")
                },
                AuthEvent {
                    user_pk: Some(7),
                    ..ev(3, "impersonation_started")
                },
            ],
            &HashSet::from([7]),
            NOW,
            &T,
        );
        assert_eq!(
            keys(&f),
            [
                ("auth:event:1", Severity::Warning),
                ("auth:event:3", Severity::Critical)
            ]
        );
        assert!(f.iter().all(|x| x.once));
    }

    #[test]
    fn audit() {
        let r = |id: &str, action: &str| AuditRow {
            id: id.into(),
            action: action.into(),
            actor: "u1".into(),
            slug: Some("acme".into()),
            created_at: ago(1),
            details: None,
        };
        let f = audit_findings(&[r("a1", "staff.break_glass"), r("a2", "volume.unlock")]);
        assert_eq!(keys(&f), [("audit:a1", Severity::Critical)]);
        assert_eq!(f[0].summary, "Staff break-glass access to acme");
    }

    // ------------------------------------------------------------ reconcile

    const RENOTIFY: i64 = 12 * 3_600_000;

    fn finding(key: &str) -> Finding {
        Finding::new(key, Severity::Warning, key)
    }

    fn stored(key: &str) -> StoredAlert {
        StoredAlert {
            key: key.into(),
            severity: Severity::Warning,
            summary: key.into(),
            once: false,
            hits: 1,
            notified_at: Some(ago(10)),
            resolved_at: None,
        }
    }

    fn kinds(findings: &[Finding], stored: &[StoredAlert]) -> Vec<Kind> {
        reconcile(findings, stored, NOW, RENOTIFY, &HashSet::new())
            .1
            .iter()
            .map(|n| n.kind)
            .collect()
    }

    #[test]
    fn new_then_reminder() {
        let f = [finding("node:a:space")];
        assert_eq!(kinds(&f, &[]), [Kind::New]);
        assert!(kinds(&f, &[stored("node:a:space")]).is_empty());
        let old = StoredAlert {
            notified_at: Some(ago(13 * 60)),
            ..stored("node:a:space")
        };
        assert_eq!(kinds(&f, &[old]), [Kind::Reminder]);
    }

    #[test]
    fn escalation() {
        let f = Finding::new("node:a:space", Severity::Critical, "x");
        assert_eq!(kinds(&[f], &[stored("node:a:space")]), [Kind::Escalated]);
    }

    #[test]
    fn debounce() {
        let f = finding("service:acme:photos").after(2);
        let (changes, notices) = reconcile(std::slice::from_ref(&f), &[], NOW, RENOTIFY, &HashSet::new());
        assert!(notices.is_empty());
        assert!(matches!(changes[0], Change::Upsert { hits: 1, .. }));
        let s = StoredAlert {
            notified_at: None,
            ..stored(&f.key)
        };
        assert_eq!(kinds(&[f], &[s]), [Kind::New]);
    }

    #[test]
    fn resolves_announced_only_if_announced() {
        let quiet = StoredAlert {
            notified_at: None,
            ..stored("service:x:docs")
        };
        let (changes, notices) = reconcile(&[], &[stored("node:a:space"), quiet], NOW, RENOTIFY, &HashSet::new());
        assert_eq!(
            changes,
            [
                Change::Resolve {
                    key: "node:a:space".into(),
                    notify: true
                },
                Change::Resolve {
                    key: "service:x:docs".into(),
                    notify: false
                },
            ]
        );
        assert_eq!(notices.iter().map(|n| n.kind).collect::<Vec<_>>(), [Kind::Resolved]);
    }

    #[test]
    fn skipped_checks_stay_open() {
        let (changes, _) = reconcile(
            &[],
            &[stored("auth:login-failed")],
            NOW,
            RENOTIFY,
            &HashSet::from(["auth".to_owned()]),
        );
        assert!(changes.is_empty());
    }

    #[test]
    fn one_off_events() {
        let f = finding("audit:a1").once();
        let sent = StoredAlert {
            once: true,
            resolved_at: Some(ago(5)),
            ..stored(&f.key)
        };
        assert!(kinds(std::slice::from_ref(&f), std::slice::from_ref(&sent)).is_empty());
        let unsent = StoredAlert {
            notified_at: None,
            ..sent.clone()
        };
        assert_eq!(kinds(std::slice::from_ref(&f), &[unsent]), [Kind::New]);
        let open = StoredAlert {
            once: true,
            ..stored(&f.key)
        };
        assert!(reconcile(&[], &[open], NOW, RENOTIFY, &HashSet::new()).0.is_empty());
    }

    #[test]
    fn comes_back_new() {
        let s = StoredAlert {
            resolved_at: Some(ago(60)),
            ..stored("node:a:space")
        };
        let (changes, notices) = reconcile(&[finding("node:a:space")], &[s], NOW, RENOTIFY, &HashSet::new());
        assert_eq!(notices[0].kind, Kind::New);
        assert!(matches!(changes[0], Change::Upsert { reopen: true, .. }));
    }

    #[test]
    fn digest_order_and_cap() {
        let n = |kind, severity, summary: &str| Notice {
            kind,
            severity,
            summary: summary.into(),
        };
        let text = digest(
            "Castor Cloud",
            &[
                n(Kind::Resolved, Severity::Warning, "gone"),
                n(Kind::New, Severity::Warning, "warn"),
                n(Kind::New, Severity::Critical, "crit"),
            ],
        );
        assert_eq!(
            text.split('\n').collect::<Vec<_>>(),
            [
                "Castor Cloud monitor: 2 alert(s)",
                "",
                "🔴 crit",
                "🟠 warn",
                "✅ Resolved: gone"
            ]
        );
        let many: Vec<Notice> = (0..40)
            .map(|i| n(Kind::New, Severity::Warning, &format!("n{i}")))
            .collect();
        assert!(digest("C", &many).ends_with("… and 15 more"));
    }
}
