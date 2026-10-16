// Tenant lifecycle jobs. The portal enqueues (after Stripe webhooks); a
// worker executes. The Job row is both the user-visible record and the queue
// entry: there is no second copy in Redis.
//
// How a worker takes a job (the Rust worker follows the same contract):
//   claim     QUEUED, runAfter passed, a kind it runs, no older QUEUED job of
//             the same tenant, no RUNNING one (a unique index enforces that
//             even between workers). Oldest first. Sets RUNNING, attempts+1,
//             and a lease (lockedBy, lockedUntil).
//   renew     every RENEW_MS while it works.
//   succeed   SUCCEEDED, finishedAt, lease cleared.
//   fail      QUEUED again with exponential backoff while attempts remain,
//             else FAILED with finishedAt.
//   recover   a RUNNING job whose lease expired (its worker died) counts as
//             a failed attempt.
import { hostname } from "node:os";
import { Prisma, prisma, type JobKind } from "@pc/db";

export type TenantJob = { id: string; tenantId: string; kind: JobKind; attempts: number; maxAttempts: number };

/**
 * A job's lease, renewed every RENEW_MS while it runs. Long, because the
 * TypeScript worker's synchronous SSH and Docker calls block its timers
 * (an image pull can take minutes): a lease must never lapse under a
 * worker that is still busy.
 */
export const LEASE_MS = 30 * 60_000;
export const RENEW_MS = 60_000;
/** First retry after this, doubling per attempt (15 s, 30 s, 1 min, 2 min). */
const BACKOFF_MS = 15_000;

// Columns are UTC timestamps without a zone (Prisma's DateTime); convert
// explicitly so the session's time zone never matters.
const NOW = Prisma.sql`(now() AT TIME ZONE 'UTC')`;
const utc = (d: Date) => Prisma.sql`(${d.toISOString()}::timestamptz AT TIME ZONE 'UTC')`;

/**
 * Records and enqueues a lifecycle job. Jobs for one tenant run one at a
 * time, in order.
 */
export async function enqueueTenantJob(tenantId: string, kind: JobKind, params?: object) {
  return prisma.job.create({ data: { tenantId, kind, params: params as never } });
}

export function workerId() {
  return `${hostname()}:${process.pid}`;
}

/** Takes the next job this worker may run, or undefined. */
export async function claimTenantJob(worker: string, kinds: readonly JobKind[]): Promise<TenantJob | undefined> {
  const kindList = Prisma.join(kinds.map((k) => Prisma.sql`${k}::"JobKind"`));
  try {
    const rows = await prisma.$queryRaw<TenantJob[]>`
      UPDATE "Job" SET status = 'RUNNING', attempts = attempts + 1, error = NULL,
        "lockedBy" = ${worker}, "lockedUntil" = ${NOW} + ${`${LEASE_MS} milliseconds`}::interval
      WHERE id = (
        SELECT j.id FROM "Job" j
        WHERE j.status = 'QUEUED' AND j."runAfter" <= ${NOW} AND j.kind IN (${kindList})
          AND NOT EXISTS (SELECT 1 FROM "Job" r WHERE r."tenantId" = j."tenantId" AND r.status = 'RUNNING')
          AND NOT EXISTS (SELECT 1 FROM "Job" o WHERE o."tenantId" = j."tenantId" AND o.status = 'QUEUED'
                            AND (o."createdAt", o.id) < (j."createdAt", j.id))
        ORDER BY j."createdAt", j.id
        LIMIT 1
        FOR UPDATE SKIP LOCKED
      )
      RETURNING id, "tenantId", kind, attempts, "maxAttempts"`;
    return rows[0];
  } catch (err) {
    // Another worker started a job of the same tenant at the same moment.
    if (isUniqueViolation(err)) return undefined;
    throw err;
  }
}

function isUniqueViolation(err: unknown) {
  const e = err as { code?: string; meta?: { code?: string } };
  return e.code === "P2010" && e.meta?.code === "23505";
}

/** Extends the lease; false when the job is no longer this worker's. */
export async function renewLease(jobId: string, worker: string) {
  const n = await prisma.$executeRaw`
    UPDATE "Job" SET "lockedUntil" = ${NOW} + ${`${LEASE_MS} milliseconds`}::interval
    WHERE id = ${jobId} AND status = 'RUNNING' AND "lockedBy" = ${worker}`;
  return n === 1;
}

export async function succeedTenantJob(jobId: string, worker: string) {
  await prisma.$executeRaw`
    UPDATE "Job" SET status = 'SUCCEEDED', "finishedAt" = ${NOW}, "lockedBy" = NULL, "lockedUntil" = NULL
    WHERE id = ${jobId} AND "lockedBy" = ${worker}`;
}

/** Returns whether this was the last attempt (the job is now FAILED). */
export async function failTenantJob(jobId: string, worker: string, error: string): Promise<boolean> {
  const rows = await prisma.$queryRaw<{ status: string }[]>`
    UPDATE "Job" SET
      status = CASE WHEN attempts >= "maxAttempts" THEN 'FAILED'::"JobStatus" ELSE 'QUEUED'::"JobStatus" END,
      "finishedAt" = CASE WHEN attempts >= "maxAttempts" THEN ${NOW} END,
      "runAfter" = ${NOW} + (${BACKOFF_MS}::float8 * power(2, greatest(attempts - 1, 0))) * interval '1 millisecond',
      error = ${error.slice(0, 4000)}, "lockedBy" = NULL, "lockedUntil" = NULL
    WHERE id = ${jobId} AND "lockedBy" = ${worker}
    RETURNING status::text`;
  return rows[0]?.status === "FAILED";
}

/** Jobs whose worker died: that attempt failed. `failed`: it was the last one. */
export async function recoverExpiredJobs(): Promise<(TenantJob & { failed: boolean })[]> {
  const rows = await prisma.$queryRaw<(TenantJob & { status: string })[]>`
    UPDATE "Job" SET
      status = CASE WHEN attempts >= "maxAttempts" THEN 'FAILED'::"JobStatus" ELSE 'QUEUED'::"JobStatus" END,
      "finishedAt" = CASE WHEN attempts >= "maxAttempts" THEN ${NOW} END,
      "runAfter" = ${NOW},
      error = 'the worker running it stopped (lease expired)', "lockedBy" = NULL, "lockedUntil" = NULL
    WHERE status = 'RUNNING' AND ("lockedUntil" IS NULL OR "lockedUntil" < ${NOW})
    RETURNING id, "tenantId", kind, attempts, "maxAttempts", status::text`;
  return rows.map(({ status, ...job }) => ({ ...job, failed: status === "FAILED" }));
}

/** The worker is alive and runs these kinds. */
export async function workerHeartbeat(worker: string, kinds: readonly JobKind[]) {
  await prisma.workerHeartbeat.upsert({
    where: { id: worker },
    create: { id: worker, kinds: [...kinds] },
    update: { kinds: [...kinds], seenAt: new Date() },
  });
}

/**
 * Workers alive and the job kinds they run: seen within `withinMs`, or
 * holding a job whose lease still runs (a busy worker can't always write its
 * heartbeat in time). The TypeScript and Rust workers each run some kinds.
 */
export async function liveWorkers(withinMs: number): Promise<{ id: string; kinds: JobKind[] }[]> {
  return prisma.$queryRaw<{ id: string; kinds: JobKind[] }[]>`
    WITH busy AS (
      SELECT "lockedBy" AS id, array_agg(DISTINCT kind::text) AS kinds FROM "Job"
      WHERE status = 'RUNNING' AND "lockedUntil" >= ${NOW} GROUP BY "lockedBy")
    SELECT COALESCE(h.id, b.id) AS id, COALESCE(h.kinds::text[], b.kinds) AS kinds
    FROM (SELECT * FROM "WorkerHeartbeat" WHERE "seenAt" >= ${utc(new Date(Date.now() - withinMs))}) h
    FULL JOIN busy b ON b.id = h.id`;
}

export async function audit(actor: string, action: string, tenantId?: string, details?: object) {
  await prisma.auditLog.create({ data: { actor, action, tenantId, details: details as never } });
}
