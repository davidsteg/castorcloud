// The job queue against a real Postgres with the migrations applied. Runs
// when TEST_DATABASE_URL is set (CI); skipped otherwise. Uses its own
// tenants and deletes them afterwards.
import assert from "node:assert/strict";
import { after, before, test } from "node:test";

const url = process.env.TEST_DATABASE_URL;
const skip = url ? false : "TEST_DATABASE_URL is not set";
if (url) process.env.DATABASE_URL = url;

const { prisma } = await import("@pc/db");
const q = await import("./jobs.ts");
const { claimScheduled } = await import("./schedule.ts");

const ALL = ["PROVISION", "UPDATE", "SUSPEND", "RESUME", "DELETE", "REVOKE_ACCESS", "RESTORE"] as const;
const tag = `qtest${process.pid}`;
let a = "";
let b = "";

before(async () => {
  if (skip) return;
  // A session in another time zone must not move any deadline.
  await prisma.$executeRawUnsafe(`ALTER DATABASE "${new URL(url!).pathname.slice(1)}" SET timezone TO 'Pacific/Kiritimati'`);
  await prisma.$disconnect();
  const mk = (n: string) => prisma.tenant.create({ data: { slug: `${tag}${n}`, displayName: n } });
  a = (await mk("a")).id;
  b = (await mk("b")).id;
});

after(async () => {
  if (skip) return;
  await prisma.tenant.deleteMany({ where: { slug: { startsWith: tag } } });
  await prisma.schedule.deleteMany({ where: { name: { startsWith: tag } } });
  await prisma.workerHeartbeat.deleteMany({ where: { id: { startsWith: tag } } });
  await prisma.$executeRawUnsafe(`ALTER DATABASE "${new URL(url!).pathname.slice(1)}" RESET timezone`);
  await prisma.$disconnect();
});

test("jobs of one tenant run one at a time, in order; other tenants don't wait", { skip }, async () => {
  const a1 = await q.enqueueTenantJob(a, "PROVISION");
  const a2 = await q.enqueueTenantJob(a, "UPDATE");
  const b1 = await q.enqueueTenantJob(b, "PROVISION");

  const first = await q.claimTenantJob(`${tag}-w1`, ALL);
  assert.equal(first?.id, a1.id);
  assert.equal(first?.attempts, 1);
  // a2 waits for a1; b1 is free.
  const second = await q.claimTenantJob(`${tag}-w2`, ALL);
  assert.equal(second?.id, b1.id);
  assert.equal(await q.claimTenantJob(`${tag}-w3`, ALL), undefined);

  await q.succeedTenantJob(a1.id, `${tag}-w1`);
  await q.succeedTenantJob(b1.id, `${tag}-w2`);
  const third = await q.claimTenantJob(`${tag}-w1`, ALL);
  assert.equal(third?.id, a2.id);
  await q.succeedTenantJob(a2.id, `${tag}-w1`);
  const rows = await prisma.job.findMany({ where: { tenantId: { in: [a, b] } } });
  assert.ok(rows.every((r) => r.status === "SUCCEEDED" && r.finishedAt && !r.lockedBy));
});

test("a worker only takes the kinds it runs", { skip }, async () => {
  const j = await q.enqueueTenantJob(a, "RESTORE", { gen: "g", snap: "s" });
  assert.equal(await q.claimTenantJob(`${tag}-w1`, ["PROVISION"]), undefined);
  assert.equal((await q.claimTenantJob(`${tag}-w1`, ["RESTORE"]))?.id, j.id);
  await q.succeedTenantJob(j.id, `${tag}-w1`);
});

test("failures back off and end FAILED after the last attempt", { skip }, async () => {
  const j = await prisma.job.create({ data: { tenantId: a, kind: "UPDATE", maxAttempts: 2 } });
  const w = `${tag}-w1`;
  assert.equal((await q.claimTenantJob(w, ALL))?.id, j.id);
  assert.equal(await q.failTenantJob(j.id, w, "boom"), false);
  let row = await prisma.job.findUniqueOrThrow({ where: { id: j.id } });
  assert.equal(row.status, "QUEUED");
  assert.equal(row.error, "boom");
  const wait = row.runAfter.getTime() - Date.now();
  assert.ok(wait > 10_000 && wait <= 15_000, `first retry in ~15 s, got ${wait} ms`);
  // Not before its time.
  assert.equal(await q.claimTenantJob(w, ALL), undefined);

  await prisma.job.update({ where: { id: j.id }, data: { runAfter: new Date(Date.now() - 1000) } });
  assert.equal((await q.claimTenantJob(w, ALL))?.attempts, 2);
  assert.equal(await q.failTenantJob(j.id, w, "boom again"), true);
  row = await prisma.job.findUniqueOrThrow({ where: { id: j.id } });
  assert.equal(row.status, "FAILED");
  assert.ok(row.finishedAt);
});

test("a dead worker's job is recovered; its late result is ignored", { skip }, async () => {
  const j = await q.enqueueTenantJob(b, "UPDATE");
  const dead = `${tag}-dead`;
  assert.equal((await q.claimTenantJob(dead, ALL))?.id, j.id);
  assert.equal(await q.renewLease(j.id, dead), true);
  assert.deepEqual(await q.recoverExpiredJobs().then((r) => r.filter((x) => x.id === j.id)), []);
  assert.ok((await q.liveWorkers(60_000)).includes(dead), "a worker holding a lease counts as alive");

  await prisma.job.update({ where: { id: j.id }, data: { lockedUntil: new Date(Date.now() - 1000) } });
  const recovered = (await q.recoverExpiredJobs()).find((x) => x.id === j.id);
  assert.equal(recovered?.failed, false);
  assert.equal(await q.renewLease(j.id, dead), false);

  const alive = `${tag}-alive`;
  assert.equal((await q.claimTenantJob(alive, ALL))?.id, j.id);
  await q.succeedTenantJob(j.id, dead); // too late: not its job any more
  assert.equal((await prisma.job.findUniqueOrThrow({ where: { id: j.id } })).status, "RUNNING");
  await q.succeedTenantJob(j.id, alive);
  assert.equal((await prisma.job.findUniqueOrThrow({ where: { id: j.id } })).status, "SUCCEEDED");
});

test("the database refuses two running jobs for one tenant", { skip }, async () => {
  const j1 = await q.enqueueTenantJob(a, "UPDATE");
  const j2 = await q.enqueueTenantJob(a, "UPDATE");
  await prisma.job.update({ where: { id: j1.id }, data: { status: "RUNNING" } });
  await assert.rejects(prisma.job.update({ where: { id: j2.id }, data: { status: "RUNNING" } }));
  await prisma.job.deleteMany({ where: { id: { in: [j1.id, j2.id] } } });
});

test("racing workers never run one tenant's jobs at once", { skip }, async () => {
  const jobs = await Promise.all([1, 2, 3, 4].map(() => q.enqueueTenantJob(b, "UPDATE")));
  const claims = await Promise.all([1, 2, 3, 4, 5, 6].map((n) => q.claimTenantJob(`${tag}-r${n}`, ALL)));
  const got = claims.filter(Boolean);
  assert.equal(got.length, 1);
  assert.equal(got[0]!.id, jobs.sort((x, y) => +x.createdAt - +y.createdAt || x.id.localeCompare(y.id))[0]!.id);
  await prisma.job.deleteMany({ where: { id: { in: jobs.map((j) => j.id) } } });
});

test("a schedule slot is won by exactly one worker", { skip }, async () => {
  const name = `${tag}-hourly`;
  const wins = await Promise.all([1, 2, 3, 4, 5].map(() => claimScheduled(name, { everyMs: 3_600_000 })));
  assert.equal(wins.filter(Boolean).length, 1);
  assert.equal(await claimScheduled(name, { everyMs: 3_600_000 }), false);
  const row = await prisma.schedule.findUniqueOrThrow({ where: { name } });
  const inMs = row.nextRunAt.getTime() - Date.now();
  assert.ok(inMs > 3_590_000 && inMs <= 3_600_000, `next run in an hour, got ${inMs} ms`);

  // A daily task doesn't run at once on its first sight.
  assert.equal(await claimScheduled(`${tag}-daily`, { at: "02:30", tz: "Europe/Zurich" }), false);
});

test("heartbeats", { skip }, async () => {
  await q.workerHeartbeat(`${tag}-hb`, ALL);
  assert.ok((await q.liveWorkers(60_000)).includes(`${tag}-hb`));
  await prisma.workerHeartbeat.update({ where: { id: `${tag}-hb` }, data: { seenAt: new Date(Date.now() - 600_000) } });
  assert.ok(!(await q.liveWorkers(60_000)).includes(`${tag}-hb`));
});
