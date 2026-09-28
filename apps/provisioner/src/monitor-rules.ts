// The monitor's rules: turn measured state into findings, and findings into
// alert changes and notifications. Pure functions, so they can be tested
// without a database or network (monitor.ts gathers the inputs).
//
// Alert text (`summary`) may be sent to Telegram, which is outside
// Switzerland: it names nodes, tenant slugs, hosts and counts, never e-mail
// addresses, usernames, IPs or error output. Those go in `details`, which
// stays in the control-plane database.

export type Severity = "WARNING" | "CRITICAL";

export type Finding = {
  /** Stable identity; its first segment is the check's scope ("node", "service", ...). */
  key: string;
  severity: Severity;
  summary: string;
  details?: Record<string, unknown>;
  /** A one-off event: notified once, never re-fires, never "resolves". */
  once?: boolean;
  /** Consecutive rounds the condition must hold before it alerts (default 1). */
  after?: number;
};

export type Thresholds = {
  /** A node not probed successfully for this long is unreachable. */
  nodeStaleMin: number;
  /** Free pool space below these percentages. */
  spaceWarnPct: number;
  spaceCritPct: number;
  certWarnDays: number;
  certCritDays: number;
  jobStuckMin: number;
  queueWaitMin: number;
  /** Failed sign-ins within `loginWindowMin` */
  loginWindowMin: number;
  loginFailTotal: number;
  loginFailPerSource: number;
  loginFailPerAccount: number;
};

export const DEFAULT_THRESHOLDS: Thresholds = {
  nodeStaleMin: 15,
  spaceWarnPct: 15,
  spaceCritPct: 10,
  certWarnDays: 14,
  certCritDays: 3,
  jobStuckMin: 180,
  queueWaitMin: 60,
  loginWindowMin: 15,
  loginFailTotal: 30,
  loginFailPerSource: 10,
  loginFailPerAccount: 10,
};

const MIN = 60_000;
const DAY = 24 * 60 * MIN;

// ------------------------------------------------------------------ nodes

export type NodeState = {
  name: string;
  kind: string;
  enabled: boolean;
  tenants: number;
  lastSeenAt: Date | null;
  lastError: string | null;
  poolSizeMb: number | null;
  poolUsedMb: number | null;
  poolHealth: string | null;
};

/** Remote nodes the watcher probes. The local development node is never probed. */
export function nodeFindings(nodes: NodeState[], now: Date, t: Thresholds): Finding[] {
  const out: Finding[] = [];
  for (const n of nodes) {
    if (n.kind === "LOCAL") continue;
    // A drained node without tenants may be switched off on purpose.
    if (!n.enabled && n.tenants === 0) continue;
    const staleMs = n.lastSeenAt ? now.getTime() - n.lastSeenAt.getTime() : Infinity;
    if (staleMs > t.nodeStaleMin * MIN) {
      out.push({
        key: `node:${n.name}:reachable`,
        severity: "CRITICAL",
        summary: n.lastSeenAt
          ? `Node ${n.name} unreachable for ${Math.round(staleMs / MIN)} min (${n.tenants} tenants)`
          : `Node ${n.name} has never been reached`,
        details: { lastSeenAt: n.lastSeenAt, lastError: n.lastError },
      });
      continue; // pool facts are stale too
    }
    if (n.poolHealth && n.poolHealth !== "ONLINE") {
      out.push({
        key: `node:${n.name}:pool`,
        severity: "CRITICAL",
        summary: `Node ${n.name}: ZFS pool is ${n.poolHealth}`,
      });
    }
    if (n.poolSizeMb && n.poolUsedMb !== null) {
      const freePct = (100 * (n.poolSizeMb - n.poolUsedMb)) / n.poolSizeMb;
      if (freePct < t.spaceWarnPct) {
        out.push({
          key: `node:${n.name}:space`,
          severity: freePct < t.spaceCritPct ? "CRITICAL" : "WARNING",
          summary: `Node ${n.name}: ${freePct.toFixed(1)}% pool space free (${Math.round((n.poolSizeMb - n.poolUsedMb) / 1024)} GB)`,
          details: { poolSizeMb: n.poolSizeMb, poolUsedMb: n.poolUsedMb },
        });
      }
    }
  }
  return out;
}

// --------------------------------------------------------- tenant services

export type ProbeResult = { slug: string; service: string; ok: boolean; status?: number; error?: string };

/** A service must fail two rounds in a row: one slow answer is not an outage. */
export function serviceFindings(probes: ProbeResult[]): Finding[] {
  return probes
    .filter((p) => !p.ok)
    .map((p) => ({
      key: `service:${p.slug}:${p.service}`,
      severity: "CRITICAL" as const,
      summary: `${p.service} of ${p.slug} is down (${p.status ? `HTTP ${p.status}` : "no answer"})`,
      details: { status: p.status, error: p.error },
      after: 2,
    }));
}

// ------------------------------------------------------------------- jobs

export type JobState = {
  id: string;
  kind: string;
  slug: string;
  status: "QUEUED" | "RUNNING" | "FAILED";
  createdAt: Date;
  error: string | null;
};

export function jobFindings(jobs: JobState[], now: Date, t: Thresholds): Finding[] {
  const out: Finding[] = [];
  const waiting: JobState[] = [];
  for (const j of jobs) {
    const ageMin = (now.getTime() - j.createdAt.getTime()) / MIN;
    if (j.status === "FAILED") {
      out.push({
        key: `job:${j.id}:failed`,
        severity: "CRITICAL",
        summary: `${j.kind} job for ${j.slug} failed after all retries`,
        details: { jobId: j.id, error: j.error },
        once: true,
      });
    } else if (j.status === "RUNNING" && ageMin > t.jobStuckMin) {
      out.push({
        key: `job:${j.id}:stuck`,
        severity: "WARNING",
        summary: `${j.kind} job for ${j.slug} has been running for ${Math.round(ageMin / 60)} h`,
        details: { jobId: j.id },
      });
    } else if (j.status === "QUEUED" && ageMin > t.queueWaitMin) {
      waiting.push(j);
    }
  }
  if (waiting.length) {
    out.push({
      key: "job:backlog",
      severity: "WARNING",
      summary: `${waiting.length} provisioning job(s) waiting for over ${t.queueWaitMin} min`,
      details: { jobs: waiting.map((j) => j.id) },
    });
  }
  return out;
}

// ---------------------------------------------------------------- backups

export type BackupState = {
  slug: string;
  tenantCreatedAt: Date;
  lastSuccessAt: Date | null;
  failures: number;
  lastError: string | null;
  verifyError: string | null;
};

/**
 * Backups run every `intervalHours`; one missed run is tolerated. A tenant
 * without any backup gets a day for its first (full) send.
 */
export function backupFindings(rows: BackupState[], intervalHours: number, now: Date): Finding[] {
  const out: Finding[] = [];
  const overdueMs = 2 * intervalHours * 3600_000 + 30 * MIN;
  for (const b of rows) {
    const since = b.lastSuccessAt ?? b.tenantCreatedAt;
    const grace = b.lastSuccessAt ? overdueMs : Math.max(overdueMs, DAY);
    const ageMs = now.getTime() - since.getTime();
    if (ageMs > grace) {
      out.push({
        key: `backup:${b.slug}:overdue`,
        severity: "CRITICAL",
        summary: b.lastSuccessAt
          ? `No backup of ${b.slug} for ${Math.round(ageMs / 3600_000)} h (${b.failures} failed runs)`
          : `${b.slug} has never been backed up`,
        details: { lastSuccessAt: b.lastSuccessAt, lastError: b.lastError },
      });
    } else if (b.failures >= 2) {
      out.push({
        key: `backup:${b.slug}:failing`,
        severity: "WARNING",
        summary: `Backup of ${b.slug} failed ${b.failures} times in a row`,
        details: { lastError: b.lastError },
      });
    }
    if (b.verifyError) {
      out.push({
        key: `backup:${b.slug}:verify`,
        severity: "CRITICAL",
        summary: `Backup of ${b.slug} failed verification`,
        details: { verifyError: b.verifyError },
      });
    }
  }
  return out;
}

// ------------------------------------------------------------ certificates

export type CertResult = { host: string; validTo?: Date; trusted: boolean; error?: string };

export function certFindings(results: CertResult[], now: Date, t: Thresholds): Finding[] {
  const out: Finding[] = [];
  for (const c of results) {
    const key = `cert:${c.host}`;
    if (!c.validTo) {
      out.push({ key, severity: "CRITICAL", summary: `TLS on ${c.host} could not be checked`, details: { error: c.error }, after: 2 });
      continue;
    }
    const days = (c.validTo.getTime() - now.getTime()) / DAY;
    if (days < 0) {
      out.push({ key, severity: "CRITICAL", summary: `Certificate for ${c.host} expired`, details: { validTo: c.validTo } });
    } else if (!c.trusted) {
      out.push({ key, severity: "CRITICAL", summary: `Certificate for ${c.host} is not trusted`, details: { error: c.error, validTo: c.validTo } });
    } else if (days < t.certWarnDays) {
      out.push({
        key,
        severity: days < t.certCritDays ? "CRITICAL" : "WARNING",
        summary: `Certificate for ${c.host} expires in ${Math.floor(days)} day${Math.floor(days) === 1 ? "" : "s"}`,
        details: { validTo: c.validTo },
      });
    }
  }
  return out;
}

// --------------------------------------------------------------- security

/** The parts of an Authentik event the rules read. */
export type AuthEvent = {
  pk: string;
  action: string;
  created: Date;
  clientIp: string | null;
  /** The account signed in to, or the one a failed sign-in named */
  username: string | null;
  userPk: number | null;
};

function countBy<T>(items: T[], key: (i: T) => string | null) {
  const m = new Map<string, number>();
  for (const i of items) {
    const k = key(i);
    if (k) m.set(k, (m.get(k) ?? 0) + 1);
  }
  return [...m.entries()].sort((a, b) => b[1] - a[1]);
}

/**
 * Sign-in abuse and admin activity from Authentik's event log. Bursts are
 * conditions over a sliding window (they resolve when it quiets down);
 * impersonation and admin sign-ins are one-off events.
 */
export function authFindings(events: AuthEvent[], superusers: Set<number>, now: Date, t: Thresholds): Finding[] {
  const out: Finding[] = [];
  const windowStart = now.getTime() - t.loginWindowMin * MIN;
  const recent = events.filter((e) => e.created.getTime() >= windowStart);

  const failed = recent.filter((e) => e.action === "login_failed");
  const bySource = countBy(failed, (e) => e.clientIp);
  const byAccount = countBy(failed, (e) => e.username?.toLowerCase() ?? null);
  const top = { sources: bySource.slice(0, 10), accounts: byAccount.slice(0, 10) };
  if (failed.length >= t.loginFailTotal) {
    out.push({
      key: "auth:login-failed",
      severity: "WARNING",
      summary: `${failed.length} failed sign-ins in ${t.loginWindowMin} min from ${bySource.length} address(es)`,
      details: top,
    });
  }
  const hotSources = bySource.filter(([, n]) => n >= t.loginFailPerSource);
  if (hotSources.length) {
    out.push({
      key: "auth:login-failed-source",
      severity: "WARNING",
      summary: `${hotSources.length} address(es) with ${t.loginFailPerSource}+ failed sign-ins in ${t.loginWindowMin} min (max ${hotSources[0][1]})`,
      details: top,
    });
  }
  const hotAccounts = byAccount.filter(([, n]) => n >= t.loginFailPerAccount);
  if (hotAccounts.length) {
    out.push({
      key: "auth:login-failed-account",
      severity: "WARNING",
      summary: `${hotAccounts.length} account(s) with ${t.loginFailPerAccount}+ failed sign-ins in ${t.loginWindowMin} min (max ${hotAccounts[0][1]})`,
      details: top,
    });
  }
  const suspicious = recent.filter((e) => e.action === "suspicious_request");
  if (suspicious.length) {
    out.push({
      key: "auth:suspicious",
      severity: "WARNING",
      summary: `Authentik flagged ${suspicious.length} suspicious request(s) in ${t.loginWindowMin} min`,
      details: { sources: countBy(suspicious, (e) => e.clientIp).slice(0, 10) },
    });
  }

  // One-off events: every one in the fetched range (deduplicated by key).
  for (const e of events) {
    if (e.action === "impersonation_started") {
      out.push({
        key: `auth:event:${e.pk}`,
        severity: "CRITICAL",
        summary: "An Authentik admin started impersonating a user",
        details: { event: e.pk, by: e.username, clientIp: e.clientIp, at: e.created },
        once: true,
      });
    } else if (e.action === "login" && e.userPk !== null && superusers.has(e.userPk)) {
      out.push({
        key: `auth:event:${e.pk}`,
        severity: "WARNING",
        summary: "Admin sign-in to Authentik",
        details: { event: e.pk, username: e.username, clientIp: e.clientIp, at: e.created },
        once: true,
      });
    }
  }
  return out;
}

/** Audit log actions the operator hears about the moment they happen. */
const AUDITED: Record<string, { severity: Severity; summary: (slug: string | null) => string }> = {
  "staff.break_glass": { severity: "CRITICAL", summary: (s) => `Staff break-glass access${s ? ` to ${s}` : ""}` },
  "backup.box_unhealthy": { severity: "CRITICAL", summary: () => "The backup box's pool is not healthy" },
  "backup.restored": { severity: "WARNING", summary: (s) => `A backup was restored${s ? ` for ${s}` : ""}` },
};

export type AuditRow = { id: string; action: string; actor: string; slug: string | null; createdAt: Date; details: unknown };

export function auditFindings(rows: AuditRow[]): Finding[] {
  return rows.flatMap((r) => {
    const rule = AUDITED[r.action];
    if (!rule) return [];
    return [{
      key: `audit:${r.id}`,
      severity: rule.severity,
      summary: rule.summary(r.slug),
      details: { action: r.action, actor: r.actor, at: r.createdAt, details: r.details },
      once: true,
    }];
  });
}

export const AUDITED_ACTIONS = Object.keys(AUDITED);

// -------------------------------------------------------------- reconcile

export type StoredAlert = {
  key: string;
  severity: Severity;
  summary: string;
  once: boolean;
  hits: number;
  notifiedAt: Date | null;
  resolvedAt: Date | null;
};

export type Change =
  | { type: "upsert"; finding: Finding; hits: number; reopen: boolean; notify: "new" | "escalated" | "reminder" | null }
  | { type: "resolve"; key: string; notify: boolean };

export type Notice = { kind: "new" | "escalated" | "reminder" | "resolved"; severity: Severity; summary: string };

/**
 * Compares this round's findings with the stored alerts.
 * `stored` holds every unresolved alert plus any stored row whose key is
 * among the findings. `skipped` lists scopes whose check could not run this
 * round: their open alerts stay open rather than being reported resolved.
 */
export function reconcile(
  findings: Finding[],
  stored: Map<string, StoredAlert>,
  now: Date,
  opts: { renotifyMs: number; skipped: Set<string> },
): { changes: Change[]; notices: Notice[] } {
  const changes: Change[] = [];
  const notices: Notice[] = [];
  const seen = new Set<string>();

  for (const f of findings) {
    if (seen.has(f.key)) continue;
    seen.add(f.key);
    const s = stored.get(f.key);
    if (f.once && s?.notifiedAt) continue; // already reported
    const open = !!s && !s.resolvedAt;
    const hits = open ? s.hits + 1 : 1;
    let notify: "new" | "escalated" | "reminder" | null = null;
    if (hits >= (f.after ?? 1)) {
      if (!open || !s.notifiedAt) notify = "new";
      else if (s.severity === "WARNING" && f.severity === "CRITICAL") notify = "escalated";
      else if (now.getTime() - s.notifiedAt.getTime() >= opts.renotifyMs) notify = "reminder";
    }
    changes.push({ type: "upsert", finding: f, hits, reopen: !open, notify });
    if (notify) notices.push({ kind: notify, severity: f.severity, summary: f.summary });
  }

  for (const s of stored.values()) {
    if (s.resolvedAt || seen.has(s.key) || s.once) continue;
    if (opts.skipped.has(scopeOf(s.key))) continue;
    // Only announce the end of something that was announced.
    changes.push({ type: "resolve", key: s.key, notify: !!s.notifiedAt });
    if (s.notifiedAt) notices.push({ kind: "resolved", severity: s.severity, summary: s.summary });
  }
  return { changes, notices };
}

export const scopeOf = (key: string) => key.split(":", 1)[0];

// ----------------------------------------------------------------- digest

const ICON = { CRITICAL: "🔴", WARNING: "🟠" } as const;
const MAX_LINES = 25;

/** One message per round; plain text (no markup, so nothing in it is interpreted). */
export function digest(brand: string, notices: Notice[]): string {
  const order = { new: 0, escalated: 0, reminder: 1, resolved: 2 } as const;
  const sorted = [...notices].sort(
    (a, b) => order[a.kind] - order[b.kind] || (a.severity === b.severity ? 0 : a.severity === "CRITICAL" ? -1 : 1),
  );
  const lines = sorted.slice(0, MAX_LINES).map((n) => {
    if (n.kind === "resolved") return `✅ Resolved: ${n.summary}`;
    const tag = n.kind === "reminder" ? " (still)" : n.kind === "escalated" ? " (now critical)" : "";
    return `${ICON[n.severity]} ${n.summary}${tag}`;
  });
  if (sorted.length > MAX_LINES) lines.push(`… and ${sorted.length - MAX_LINES} more`);
  const firing = notices.filter((n) => n.kind !== "resolved").length;
  const head = firing ? `${brand} monitor: ${firing} alert(s)` : `${brand} monitor: ${notices.length} resolved`;
  return [head, "", ...lines].join("\n");
}
