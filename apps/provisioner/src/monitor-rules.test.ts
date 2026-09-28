import assert from "node:assert/strict";
import { test } from "node:test";
import {
  DEFAULT_THRESHOLDS as T,
  authFindings,
  auditFindings,
  backupFindings,
  certFindings,
  digest,
  jobFindings,
  nodeFindings,
  reconcile,
  serviceFindings,
  type AuthEvent,
  type Finding,
  type NodeState,
  type StoredAlert,
} from "./monitor-rules.ts";

const now = new Date("2026-09-28T12:00:00Z");
const ago = (min: number) => new Date(now.getTime() - min * 60_000);
const GB = 1024;

function node(over: Partial<NodeState> = {}): NodeState {
  return {
    name: "mini-1", kind: "MAC", enabled: true, tenants: 3, lastSeenAt: ago(2), lastError: null,
    poolSizeMb: 1000 * GB, poolUsedMb: 500 * GB, poolHealth: "ONLINE", ...over,
  };
}

test("a healthy node raises nothing; the local node is never checked", () => {
  assert.deepEqual(nodeFindings([node(), node({ name: "local", kind: "LOCAL", lastSeenAt: null })], now, T), []);
});

test("an unreachable node is critical and its stale pool facts are ignored", () => {
  const f = nodeFindings([node({ lastSeenAt: ago(30), poolHealth: "DEGRADED", lastError: "ssh: timeout" })], now, T);
  assert.equal(f.length, 1);
  assert.equal(f[0].key, "node:mini-1:reachable");
  assert.equal(f[0].severity, "CRITICAL");
  assert.doesNotMatch(f[0].summary, /ssh/); // error output stays in details
});

test("a drained node without tenants may be off", () => {
  assert.deepEqual(nodeFindings([node({ enabled: false, tenants: 0, lastSeenAt: null })], now, T), []);
});

test("pool health and free space", () => {
  const f = nodeFindings([node({ poolHealth: "DEGRADED", poolUsedMb: 880 * GB })], now, T);
  assert.deepEqual(f.map((x) => [x.key, x.severity]), [["node:mini-1:pool", "CRITICAL"], ["node:mini-1:space", "WARNING"]]);
  const crit = nodeFindings([node({ poolUsedMb: 950 * GB })], now, T);
  assert.equal(crit[0].severity, "CRITICAL");
});

test("services need two failed rounds before they alert", () => {
  const f = serviceFindings([
    { slug: "acme", service: "photos", ok: true, status: 200 },
    { slug: "acme", service: "docs", ok: false, status: 502 },
  ]);
  assert.equal(f.length, 1);
  assert.equal(f[0].key, "service:acme:docs");
  assert.equal(f[0].after, 2);
  assert.match(f[0].summary, /HTTP 502/);
});

test("jobs: failed once, stuck, and a backlog", () => {
  const f = jobFindings([
    { id: "j1", kind: "PROVISION", slug: "acme", status: "FAILED", createdAt: ago(10), error: "boom" },
    { id: "j2", kind: "UPDATE", slug: "acme", status: "RUNNING", createdAt: ago(200), error: null },
    { id: "j3", kind: "UPDATE", slug: "beta", status: "RUNNING", createdAt: ago(20), error: null },
    { id: "j4", kind: "UPDATE", slug: "gamma", status: "QUEUED", createdAt: ago(90), error: null },
  ], now, T);
  assert.deepEqual(f.map((x) => x.key), ["job:j1:failed", "job:j2:stuck", "job:backlog"]);
  assert.equal(f[0].once, true);
  assert.doesNotMatch(f[0].summary, /boom/);
});

test("backups: overdue, failing, verification, and a grace day for the first", () => {
  const base = { tenantCreatedAt: ago(60 * 24 * 30), failures: 0, lastError: null, verifyError: null };
  const f = backupFindings([
    { ...base, slug: "ok", lastSuccessAt: ago(60) },
    { ...base, slug: "late", lastSuccessAt: ago(60 * 13), failures: 2 },
    { ...base, slug: "flaky", lastSuccessAt: ago(60 * 5), failures: 2 },
    { ...base, slug: "bad", lastSuccessAt: ago(60), verifyError: "checksum" },
    { ...base, slug: "new", lastSuccessAt: null, tenantCreatedAt: ago(60 * 20) },
    { ...base, slug: "never", lastSuccessAt: null, tenantCreatedAt: ago(60 * 25) },
  ], 6, now);
  assert.deepEqual(f.map((x) => [x.key, x.severity]), [
    ["backup:late:overdue", "CRITICAL"],
    ["backup:flaky:failing", "WARNING"],
    ["backup:bad:verify", "CRITICAL"],
    ["backup:never:overdue", "CRITICAL"],
  ]);
});

test("certificates: expiry windows, untrusted and unreachable", () => {
  const days = (d: number) => new Date(now.getTime() + d * 86_400_000);
  const f = certFindings([
    { host: "a", validTo: days(60), trusted: true },
    { host: "b", validTo: days(10), trusted: true },
    { host: "c", validTo: days(2), trusted: true },
    { host: "d", validTo: days(60), trusted: false, error: "Hostname/IP does not match" },
    { host: "e", validTo: days(-1), trusted: false },
    { host: "f", trusted: false, error: "ECONNREFUSED" },
  ], now, T);
  assert.deepEqual(f.map((x) => [x.key, x.severity]), [
    ["cert:b", "WARNING"], ["cert:c", "CRITICAL"], ["cert:d", "CRITICAL"], ["cert:e", "CRITICAL"], ["cert:f", "CRITICAL"],
  ]);
  assert.match(f.find((x) => x.key === "cert:e")!.summary, /expired/);
});

function ev(pk: number, action: string, over: Partial<AuthEvent> = {}): AuthEvent {
  return { pk: String(pk), action, created: ago(1), clientIp: "203.0.113.9", username: "someone@example.com", userPk: null, ...over };
}

test("failed sign-in bursts, without names or addresses in the summary", () => {
  const failed = Array.from({ length: 12 }, (_, i) => ev(i, "login_failed"));
  const f = authFindings(failed, new Set(), now, T);
  assert.deepEqual(f.map((x) => x.key), ["auth:login-failed-source", "auth:login-failed-account"]);
  for (const x of f) {
    assert.doesNotMatch(x.summary, /203\.0\.113|example\.com/);
    assert.ok(JSON.stringify(x.details).includes("203.0.113.9")); // for the operator, in the DB
  }
  const spread = Array.from({ length: 35 }, (_, i) => ev(i, "login_failed", { clientIp: `198.51.100.${i}`, username: `u${i}` }));
  assert.deepEqual(authFindings(spread, new Set(), now, T).map((x) => x.key), ["auth:login-failed"]);
  const old = Array.from({ length: 35 }, (_, i) => ev(i, "login_failed", { created: ago(30) }));
  assert.deepEqual(authFindings(old, new Set(), now, T), []);
});

test("admin sign-ins and impersonation are one-off events", () => {
  const f = authFindings([
    ev(1, "login", { userPk: 7, username: "akadmin" }),
    ev(2, "login", { userPk: 8, username: "customer" }),
    ev(3, "impersonation_started", { userPk: 7 }),
  ], new Set([7]), now, T);
  assert.deepEqual(f.map((x) => [x.key, x.severity, x.once]), [
    ["auth:event:1", "WARNING", true],
    ["auth:event:3", "CRITICAL", true],
  ]);
});

test("audit: break-glass access alerts, routine actions don't", () => {
  const f = auditFindings([
    { id: "a1", action: "staff.break_glass", actor: "u1", slug: "acme", createdAt: ago(1), details: null },
    { id: "a2", action: "volume.unlock", actor: "system", slug: "acme", createdAt: ago(1), details: null },
  ]);
  assert.deepEqual(f.map((x) => [x.key, x.severity]), [["audit:a1", "CRITICAL"]]);
});

// --------------------------------------------------------------- reconcile

const opts = { renotifyMs: 12 * 3600_000, skipped: new Set<string>() };
const finding = (key: string, over: Partial<Finding> = {}): Finding => ({ key, severity: "WARNING", summary: key, ...over });
const stored = (key: string, over: Partial<StoredAlert> = {}): StoredAlert => ({
  key, severity: "WARNING", summary: key, once: false, hits: 1, notifiedAt: ago(10), resolvedAt: null, ...over,
});
const map = (...rows: StoredAlert[]) => new Map(rows.map((r) => [r.key, r]));

test("a new condition notifies once, then reminds after the renotify interval", () => {
  let r = reconcile([finding("node:a:space")], map(), now, opts);
  assert.deepEqual(r.notices.map((n) => n.kind), ["new"]);
  r = reconcile([finding("node:a:space")], map(stored("node:a:space")), now, opts);
  assert.deepEqual(r.notices, []);
  r = reconcile([finding("node:a:space")], map(stored("node:a:space", { notifiedAt: ago(13 * 60) })), now, opts);
  assert.deepEqual(r.notices.map((n) => n.kind), ["reminder"]);
});

test("escalation from warning to critical notifies at once", () => {
  const r = reconcile([finding("node:a:space", { severity: "CRITICAL" })], map(stored("node:a:space")), now, opts);
  assert.deepEqual(r.notices.map((n) => n.kind), ["escalated"]);
});

test("debounced conditions wait for enough hits", () => {
  const f = finding("service:acme:photos", { after: 2 });
  let r = reconcile([f], map(), now, opts);
  assert.deepEqual(r.notices, []);
  assert.equal((r.changes[0] as { hits: number }).hits, 1);
  r = reconcile([f], map(stored(f.key, { hits: 1, notifiedAt: null })), now, opts);
  assert.deepEqual(r.notices.map((n) => n.kind), ["new"]);
});

test("a cleared condition resolves, announced only if it was announced", () => {
  const r = reconcile([], map(stored("node:a:space"), stored("service:x:docs", { notifiedAt: null })), now, opts);
  assert.deepEqual(r.changes, [
    { type: "resolve", key: "node:a:space", notify: true },
    { type: "resolve", key: "service:x:docs", notify: false },
  ]);
  assert.deepEqual(r.notices.map((n) => n.kind), ["resolved"]);
});

test("alerts of a check that could not run stay open", () => {
  const r = reconcile([], map(stored("auth:login-failed")), now, { ...opts, skipped: new Set(["auth"]) });
  assert.deepEqual(r.changes, []);
});

test("one-off events are sent once; an undelivered one is retried", () => {
  const f = finding("audit:a1", { once: true });
  assert.deepEqual(reconcile([f], map(stored(f.key, { once: true, resolvedAt: ago(5) })), now, opts).notices, []);
  const retry = reconcile([f], map(stored(f.key, { once: true, resolvedAt: ago(5), notifiedAt: null })), now, opts);
  assert.deepEqual(retry.notices.map((n) => n.kind), ["new"]);
  // They never "resolve".
  assert.deepEqual(reconcile([], map(stored(f.key, { once: true })), now, opts).changes, []);
});

test("a condition that comes back after resolving is new again", () => {
  const r = reconcile([finding("node:a:space")], map(stored("node:a:space", { resolvedAt: ago(60) })), now, opts);
  assert.deepEqual(r.notices.map((n) => n.kind), ["new"]);
  assert.equal((r.changes[0] as { reopen: boolean }).reopen, true);
});

test("the digest puts critical news first and caps its length", () => {
  const text = digest("Castor Cloud", [
    { kind: "resolved", severity: "WARNING", summary: "gone" },
    { kind: "new", severity: "WARNING", summary: "warn" },
    { kind: "new", severity: "CRITICAL", summary: "crit" },
  ]);
  assert.deepEqual(text.split("\n"), ["Castor Cloud monitor: 2 alert(s)", "", "🔴 crit", "🟠 warn", "✅ Resolved: gone"]);
  const many = digest("C", Array.from({ length: 40 }, (_, i) => ({ kind: "new" as const, severity: "WARNING" as const, summary: `n${i}` })));
  assert.match(many, /… and 15 more$/);
});
