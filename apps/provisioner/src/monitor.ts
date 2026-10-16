// The monitor: checks the platform every few minutes and alerts the operator
// on Telegram and/or by e-mail (docs/monitoring.md). Its own process, so it
// keeps reporting when the provisioner worker is down.
//
//   npm run monitor -w @pc/provisioner              every MONITOR_INTERVAL_S
//   npm run monitor -w @pc/provisioner -- once      one round, then exit
//   npm run monitor -w @pc/provisioner -- status    open alerts
//   npm run monitor -w @pc/provisioner -- test      sends a test notification
import tls from "node:tls";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { config } from "dotenv";
import nodemailer from "nodemailer";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../..");
config({ path: path.join(root, ".env"), quiet: true });

const { prisma, Prisma, JobKind } = await import("@pc/db");
const { liveWorkers } = await import("@pc/core");
const { SERVICES, STACK_SERVICES, names } = await import("./tenant.ts");
const R = await import("./monitor-rules.ts");
type Finding = import("./monitor-rules.ts").Finding;
type StoredAlert = import("./monitor-rules.ts").StoredAlert;
type Notice = import("./monitor-rules.ts").Notice;
type Json = import("@pc/db").Prisma.InputJsonValue;

const env = process.env;
const platform = {
  baseDomain: env.BASE_DOMAIN ?? "brand.localhost",
  scheme: (env.PUBLIC_SCHEME ?? "https") as "http" | "https",
  brandName: env.BRAND_NAME ?? "Castor Cloud",
  timeZone: env.TZ ?? "Europe/Zurich",
  authHostIp: env.AUTH_HOST_IP ?? "host-gateway",
};
// Authentik's public URL, unless the control plane reaches it more directly.
const authUrl = env.MONITOR_AUTHENTIK_URL || `${platform.scheme}://auth.${platform.baseDomain}`;
const INTERVAL_MS = Number(env.MONITOR_INTERVAL_S ?? "300") * 1000;
const RENOTIFY_MS = Number(env.MONITOR_RENOTIFY_HOURS ?? "12") * 3600_000;
const T = R.DEFAULT_THRESHOLDS;
const HOUR = 3600_000;

// ---------------------------------------------------------------- helpers

function withTimeout<T>(p: Promise<T>, ms: number, what: string): Promise<T> {
  let timer: NodeJS.Timeout;
  return Promise.race([
    p.finally(() => clearTimeout(timer)),
    new Promise<never>((_, reject) => {
      timer = setTimeout(() => reject(new Error(`${what} timed out after ${ms / 1000}s`)), ms);
    }),
  ]);
}

/** Runs `fn` over `items`, `limit` at a time. */
async function pool<T, U>(items: T[], limit: number, fn: (item: T) => Promise<U>): Promise<U[]> {
  const out: U[] = new Array(items.length);
  let next = 0;
  await Promise.all(
    Array.from({ length: Math.min(limit, items.length) }, async () => {
      while (next < items.length) {
        const i = next++;
        out[i] = await fn(items[i]);
      }
    }),
  );
  return out;
}

// ----------------------------------------------------------------- checks

type Check = { scope: string; run: (now: Date) => Promise<Finding[]> };

const nodesCheck: Check = {
  scope: "node",
  async run(now) {
    const rows = await prisma.node.findMany({
      include: { _count: { select: { tenants: { where: { status: { notIn: ["DELETED"] } } } } } },
    });
    return R.nodeFindings(rows.map((n) => ({ ...n, tenants: n._count.tenants })), now, T);
  },
};

/** Every booked service of every active tenant, through the same route customers use. */
const servicesCheck: Check = {
  scope: "service",
  async run() {
    const tenants = await prisma.tenant.findMany({
      where: { status: "ACTIVE" },
      select: { id: true, slug: true, services: { where: { enabled: true }, select: { kind: true } } },
    });
    const targets = tenants.flatMap((t) => {
      const n = names(t.slug, platform);
      return t.services.map((s) => {
        const key = s.kind.toLowerCase();
        const url = s.kind === "ASSISTANT"
          ? `${n.assistantOrigin}/health`
          : n.serviceOrigin(key as (typeof STACK_SERVICES)[number]) + SERVICES[key as (typeof STACK_SERVICES)[number]].healthPath;
        return { tenantId: t.id, slug: t.slug, kind: s.kind, service: key, url };
      });
    });
    const probes = await pool(targets, 8, async (t) => {
      let result: { ok: boolean; status?: number; error?: string };
      try {
        const res = await fetch(t.url, { redirect: "manual", signal: AbortSignal.timeout(15_000) });
        await res.body?.cancel();
        // As the provisioner's readiness check: redirects to sign-in are fine,
        // 404 means no route and 5xx a broken or missing backend.
        result = { ok: res.status < 400, status: res.status };
      } catch (err) {
        result = { ok: false, error: (err as Error).message };
      }
      await prisma.serviceInstance.update({
        where: { tenantId_kind: { tenantId: t.tenantId, kind: t.kind } },
        data: { healthy: result.ok, lastHealthAt: new Date() },
      });
      return { slug: t.slug, service: t.service, ...result };
    });
    return R.serviceFindings(probes);
  },
};

const jobsCheck: Check = {
  scope: "job",
  async run(now) {
    const jobs = await prisma.job.findMany({
      where: {
        OR: [
          { status: { in: ["QUEUED", "RUNNING"] } },
          { status: "FAILED", createdAt: { gte: new Date(now.getTime() - 7 * 24 * HOUR) } },
        ],
      },
      select: { id: true, kind: true, status: true, createdAt: true, error: true, tenant: { select: { slug: true } } },
    });
    return R.jobFindings(
      jobs.map((j) => ({ ...j, status: j.status as "QUEUED" | "RUNNING" | "FAILED", slug: j.tenant.slug })),
      now,
      T,
    );
  },
};

/**
 * Backup status as recorded by the provisioner's backup runs (docs/backups.md).
 * Read with SQL so the monitor also runs on a database without backups yet.
 */
const backupsCheck: Check = {
  scope: "backup",
  async run(now) {
    const remote = await prisma.tenant.count({ where: { status: "ACTIVE", node: { kind: { not: "LOCAL" } } } });
    if (!remote) return [];
    if (!env.BACKUP_TARGET) {
      return [{ key: "backup:off", severity: "WARNING", summary: `Tenant backups are off (${remote} active tenants)` }];
    }
    const [{ present }] = await prisma.$queryRaw<{ present: boolean }[]>`SELECT to_regclass('"TenantBackup"') IS NOT NULL AS present`;
    if (!present) return [];
    const rows = await prisma.$queryRaw<import("./monitor-rules.ts").BackupState[]>`
      SELECT t.slug, t."createdAt" AS "tenantCreatedAt", b."lastSuccessAt",
             COALESCE(b.failures, 0)::int AS failures, b."lastError", b."verifyError"
      FROM "Tenant" t
      JOIN "Node" n ON n.id = t."nodeId"
      LEFT JOIN "TenantBackup" b ON b."tenantId" = t.id
      WHERE t.status = 'ACTIVE' AND n.kind <> 'LOCAL' AND n."zfsPool" IS NOT NULL`;
    return R.backupFindings(rows, Number(env.BACKUP_INTERVAL_HOURS ?? "6"), now);
  },
};

/** The certificate Traefik serves: connects like a browser would, reads the expiry. */
function certificateOf(host: string): Promise<import("./monitor-rules.ts").CertResult> {
  return new Promise((resolve) => {
    const socket = tls.connect({
      // MONITOR_TLS_ADDRESS: where to connect when the public address isn't
      // reachable from the LAN (no hairpin NAT); the name is still checked.
      host: env.MONITOR_TLS_ADDRESS || host,
      port: 443,
      servername: host,
      rejectUnauthorized: false, // read the certificate even when it's bad
      timeout: 10_000,
    }, () => {
      const cert = socket.getPeerCertificate();
      resolve({
        host,
        validTo: cert.valid_to ? new Date(cert.valid_to) : undefined,
        trusted: socket.authorized,
        error: socket.authorizationError ? String(socket.authorizationError) : undefined,
      });
      socket.end();
    });
    socket.on("error", (err) => resolve({ host, trusted: false, error: err.message }));
    socket.on("timeout", () => {
      socket.destroy();
      resolve({ host, trusted: false, error: "timeout" });
    });
  });
}

const certsCheck: Check = {
  scope: "cert",
  async run(now) {
    if (platform.scheme !== "https") return [];
    const hosts = (env.MONITOR_TLS_HOSTS || `${platform.baseDomain},auth.${platform.baseDomain}`)
      .split(",").map((h) => h.trim()).filter(Boolean);
    return R.certFindings(await Promise.all(hosts.map(certificateOf)), now, T);
  },
};

// Authentik, with the monitor's own read-only token (40-monitor.yaml).
type AkEvent = {
  pk: string;
  action: string;
  created: string;
  client_ip: string | null;
  user?: { pk?: number; username?: string };
  context?: { username?: unknown };
};

async function ak<T>(p: string): Promise<T> {
  const res = await fetch(`${authUrl}/api/v3${p}`, {
    headers: { Authorization: `Bearer ${env.MONITOR_AUTHENTIK_TOKEN}`, Accept: "application/json" },
    signal: AbortSignal.timeout(15_000),
  });
  if (!res.ok) throw new Error(`Authentik GET ${p.split("?")[0]} -> ${res.status}`);
  return (await res.json()) as T;
}

const authCheck: Check = {
  scope: "auth",
  async run(now) {
    if (!env.MONITOR_AUTHENTIK_TOKEN) {
      return [{ key: "auth:off", severity: "WARNING", summary: "Sign-in security checks are off (no monitor token for Authentik)" }];
    }
    const since = now.getTime() - 24 * HOUR;
    // Authentik's action filter also matches longer actions ("login" finds
    // "login_failed"), so results are filtered to the exact action here.
    const events = async (action: string, query = "") => {
      const page = await ak<{ results: AkEvent[] }>(`/events/events/?ordering=-created&page_size=500&action=${action}${query}`);
      return page.results.filter((e) => e.action === action);
    };
    const admins = (await ak<{ results: { pk: number; username: string }[] }>("/core/users/?is_superuser=true&page_size=100")).results;
    const raw = [
      ...(await events("login_failed")),
      ...(await events("suspicious_request")),
      ...(await events("impersonation_started")),
      // Admin sign-ins, per admin: customers' sign-ins would crowd them out.
      ...(await Promise.all(admins.map((a) => events("login", `&username=${encodeURIComponent(a.username)}`)))).flat(),
    ];
    const parsed = raw
      .map((e) => ({
        pk: e.pk,
        action: e.action,
        created: new Date(e.created),
        clientIp: e.client_ip ?? null,
        // A failed sign-in has no user; the name that was tried is in the context.
        username: typeof e.context?.username === "string" ? e.context.username : (e.user?.username ?? null),
        userPk: typeof e.user?.pk === "number" ? e.user.pk : null,
      }))
      .filter((e) => e.created.getTime() >= since);
    return R.authFindings(parsed, new Set(admins.map((a) => a.pk)), now, T);
  },
};

const auditCheck: Check = {
  scope: "audit",
  async run(now) {
    const rows = await prisma.auditLog.findMany({
      where: { action: { in: R.AUDITED_ACTIONS }, createdAt: { gte: new Date(now.getTime() - 24 * HOUR) } },
      orderBy: { createdAt: "asc" },
    });
    const slugs = new Map(
      (await prisma.tenant.findMany({
        where: { id: { in: rows.flatMap((r) => (r.tenantId ? [r.tenantId] : [])) } },
        select: { id: true, slug: true },
      })).map((t) => [t.id, t.slug]),
    );
    return R.auditFindings(rows.map((r) => ({ ...r, slug: r.tenantId ? (slugs.get(r.tenantId) ?? null) : null })));
  },
};

// The platform itself: the provisioner worker, Authentik.
const platformCheck: Check = {
  scope: "platform",
  async run() {
    const out: Finding[] = [];
    try {
      // Workers write a heartbeat every few seconds while they take jobs.
      const workers = await withTimeout(liveWorkers(2 * 60_000), 10_000, "database");
      const covered = new Set(workers.flatMap((w) => w.kinds));
      const missing = Object.values(JobKind).filter((k) => !covered.has(k));
      if (workers.length === 0) {
        out.push({ key: "platform:queue", severity: "CRITICAL", summary: "The provisioner worker is not running", after: 2 });
      } else if (missing.length) {
        // The TypeScript and Rust workers split the kinds: one of them is down.
        out.push({ key: "platform:queue", severity: "CRITICAL", summary: `No worker takes ${missing.join(", ")} jobs`, details: { workers: workers.map((w) => w.id) }, after: 2 });
      }
    } catch (err) {
      // Same key: without the database the worker can't be seen either, and
      // its alert must not read as resolved.
      out.push({ key: "platform:queue", severity: "CRITICAL", summary: "The job queue's database is not reachable", details: { error: (err as Error).message }, after: 2 });
    }
    try {
      const res = await fetch(`${authUrl}/-/health/live/`, { signal: AbortSignal.timeout(10_000) });
      await res.body?.cancel();
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
    } catch (err) {
      out.push({ key: "platform:authentik", severity: "CRITICAL", summary: "Authentik (sign-in) is not responding", details: { error: (err as Error).message }, after: 2 });
    }
    return out;
  },
};

const CHECKS = [platformCheck, nodesCheck, servicesCheck, jobsCheck, backupsCheck, certsCheck, authCheck, auditCheck];

// --------------------------------------------------------------- channels

const telegram = env.MONITOR_TELEGRAM_CHAT_ID && (env.MONITOR_TELEGRAM_BOT_TOKEN || env.TELEGRAM_BOT_TOKEN)
  ? { chatId: env.MONITOR_TELEGRAM_CHAT_ID, token: env.MONITOR_TELEGRAM_BOT_TOKEN || env.TELEGRAM_BOT_TOKEN! }
  : undefined;
// Only over TLS: smtps:// (implicit) or smtp:// with STARTTLS required.
function smtpTransport(url: string) {
  const u = new URL(url);
  if (u.protocol !== "smtps:" && u.protocol !== "smtp:") throw new Error("MONITOR_SMTP_URL must be smtps:// or smtp://");
  const secure = u.protocol === "smtps:";
  return nodemailer.createTransport({
    host: u.hostname,
    port: Number(u.port || (secure ? 465 : 587)),
    secure,
    requireTLS: true,
    tls: { minVersion: "TLSv1.2" },
    auth: u.username ? { user: decodeURIComponent(u.username), pass: decodeURIComponent(u.password) } : undefined,
  });
}
const mailer = env.MONITOR_SMTP_URL && env.MONITOR_EMAIL_TO ? smtpTransport(env.MONITOR_SMTP_URL) : undefined;

async function sendTelegram(text: string) {
  const api = `${env.TELEGRAM_API_URL ?? "https://api.telegram.org"}/bot${telegram!.token}`;
  for (let i = 0; i < text.length; i += 4000) {
    const res = await fetch(`${api}/sendMessage`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      // Plain text: no parse mode, so nothing in an alert is interpreted.
      body: JSON.stringify({ chat_id: telegram!.chatId, text: text.slice(i, i + 4000), link_preview_options: { is_disabled: true } }),
      signal: AbortSignal.timeout(15_000),
    });
    const data = (await res.json()) as { ok: boolean; description?: string };
    if (!data.ok) throw new Error(`Telegram: ${data.description}`);
  }
}

async function sendMail(subject: string, text: string) {
  await mailer!.sendMail({
    from: env.MONITOR_EMAIL_FROM || `${platform.brandName} monitor <monitor@${platform.baseDomain}>`,
    to: env.MONITOR_EMAIL_TO,
    subject,
    text,
  });
}

/**
 * Telegram gets every round's digest; e-mail gets those with anything
 * critical in them (or everything, without Telegram). Returns whether the
 * digest reached the operator; if not, the next round sends it again.
 */
async function deliver(notices: Notice[]): Promise<boolean> {
  const text = R.digest(platform.brandName, notices);
  console.log(text);
  if (!telegram && !mailer) return true; // console only (development)
  const critical = notices.some((n) => n.severity === "CRITICAL");
  let delivered = false;
  if (telegram) {
    try {
      await sendTelegram(text);
      delivered = true;
    } catch (err) {
      console.error(`[monitor] Telegram: ${(err as Error).message}`);
    }
  }
  if (mailer && (critical || !telegram)) {
    try {
      await sendMail(text.split("\n", 1)[0], text);
      delivered = true;
    } catch (err) {
      console.error(`[monitor] e-mail: ${(err as Error).message}`);
    }
  }
  return delivered;
}

// ------------------------------------------------------------------ round

async function round(now = new Date()) {
  const findings: Finding[] = [];
  const skipped = new Set<string>();
  await Promise.all(CHECKS.map(async (c) => {
    try {
      findings.push(...(await withTimeout(c.run(now), 120_000, `${c.scope} check`)));
    } catch (err) {
      // Its open alerts stay open; the failure itself is reported.
      skipped.add(c.scope);
      console.error(`[monitor] ${c.scope} check: ${(err as Error).message}`);
      findings.push({
        key: `monitor:${c.scope}`,
        severity: "WARNING",
        summary: `The ${c.scope} check could not run`,
        details: { error: (err as Error).message.slice(0, 2000) },
        after: 2,
      });
    }
  }));

  const rows = await prisma.alert.findMany({
    where: { OR: [{ resolvedAt: null }, { key: { in: findings.map((f) => f.key) } }] },
  });
  const stored = new Map<string, StoredAlert>(rows.map((r) => [r.key, r]));
  const { changes, notices } = R.reconcile(findings, stored, now, { renotifyMs: RENOTIFY_MS, skipped });
  const delivered = notices.length ? await deliver(notices) : false;

  for (const c of changes) {
    if (c.type === "resolve") {
      if (c.notify && !delivered) continue; // announce it next round
      await prisma.alert.update({ where: { key: c.key }, data: { resolvedAt: now, hits: 0 } });
      continue;
    }
    const f = c.finding;
    const sent = c.notify && delivered ? { notifiedAt: now } : {};
    const data = {
      severity: f.severity,
      summary: f.summary,
      details: (f.details as Json | undefined) ?? Prisma.DbNull,
      once: !!f.once,
      hits: c.hits,
      lastSeenAt: now,
      // One-off events are over as soon as they're recorded.
      resolvedAt: f.once ? now : null,
      ...sent,
    };
    await prisma.alert.upsert({
      where: { key: f.key },
      create: { key: f.key, firstSeenAt: now, ...data },
      update: c.reopen ? { firstSeenAt: now, notifiedAt: null, ...data } : data,
    });
  }
  // Keep a quarter of history.
  await prisma.alert.deleteMany({ where: { resolvedAt: { lt: new Date(now.getTime() - 90 * 24 * HOUR) } } });

  const open = await prisma.alert.count({ where: { resolvedAt: null } });
  console.log(`[monitor] ${now.toISOString()}: ${findings.length} finding(s), ${open} open alert(s), ${notices.length} notice(s)${skipped.size ? `, skipped: ${[...skipped].join(", ")}` : ""}`);

  // Dead-man's switch: an outside service that alerts when these pings stop
  // (the monitor itself, or the whole control plane, is down). Only a URL
  // is sent, no data.
  if (env.MONITOR_HEARTBEAT_URL) {
    await fetch(env.MONITOR_HEARTBEAT_URL, { signal: AbortSignal.timeout(10_000) })
      .then((r) => r.body?.cancel())
      .catch((err) => console.error(`[monitor] heartbeat: ${(err as Error).message}`));
  }
}

// ------------------------------------------------------------------- main

const command = process.argv[2] ?? "run";

if (command === "status") {
  const open = await prisma.alert.findMany({ where: { resolvedAt: null }, orderBy: [{ severity: "desc" }, { firstSeenAt: "asc" }] });
  if (!open.length) console.log("no open alerts");
  for (const a of open) {
    console.log(`${a.severity.padEnd(8)} since ${a.firstSeenAt.toISOString()}  ${a.summary}`);
    if (a.details) console.log(`         ${JSON.stringify(a.details)}`);
  }
  process.exit(0);
}

if (command === "test") {
  const ok = await deliver([{ kind: "new", severity: "CRITICAL", summary: "Test alert: the monitor can reach you" }]);
  console.log(ok ? "sent" : "NOT sent (see errors above)");
  process.exit(ok ? 0 : 1);
}

if (command === "once") {
  await round();
  process.exit(0);
}

if (command !== "run") {
  console.error("usage: monitor [run|once|status|test]");
  process.exit(2);
}

if (!telegram && !mailer) {
  console.warn("[monitor] no alert channel configured (MONITOR_TELEGRAM_CHAT_ID or MONITOR_SMTP_URL + MONITOR_EMAIL_TO): alerts only go to this log");
}

// The database holds the alert state. If it is down, say so directly (at
// most every MONITOR_RENOTIFY_HOURS), since nothing else can.
let dbDownNotifiedAt = 0;
for (;;) {
  const started = Date.now();
  try {
    await round();
    dbDownNotifiedAt = 0;
  } catch (err) {
    console.error(`[monitor] round failed: ${(err as Error).message}`);
    if (Date.now() - dbDownNotifiedAt >= RENOTIFY_MS) {
      const ok = await deliver([{ kind: "new", severity: "CRITICAL", summary: "The monitor cannot complete its checks (control-plane database?)" }]);
      if (ok) dbDownNotifiedAt = Date.now();
    }
  }
  await new Promise((r) => setTimeout(r, Math.max(10_000, INTERVAL_MS - (Date.now() - started))));
}
