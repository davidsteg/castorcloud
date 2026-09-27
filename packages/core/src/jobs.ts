// Tenant lifecycle jobs. The portal enqueues (after Stripe webhooks); the
// provisioner worker executes. The Job row is the user-visible record;
// the BullMQ job only carries its id.
import { Queue } from "bullmq";
import { prisma, type JobKind } from "@pc/db";

export const TENANT_QUEUE = "tenant-lifecycle";

export type TenantJobData = { jobId: string; tenantId: string; kind: JobKind };

export function redisConnection() {
  const url = new URL(process.env.REDIS_URL ?? "redis://localhost:6379");
  return { host: url.hostname, port: Number(url.port || 6379), password: url.password || undefined };
}

let queue: Queue<TenantJobData> | undefined;

function tenantQueue() {
  queue ??= new Queue<TenantJobData>(TENANT_QUEUE, { connection: redisConnection() });
  return queue;
}

/**
 * Records and enqueues a lifecycle job. Jobs for one tenant run one at a time
 * (the worker serializes by tenant), in order.
 */
export async function enqueueTenantJob(tenantId: string, kind: JobKind) {
  const job = await prisma.job.create({ data: { tenantId, kind } });
  await tenantQueue().add(kind, { jobId: job.id, tenantId, kind }, {
    jobId: job.id,
    attempts: 5,
    backoff: { type: "exponential", delay: 15_000 },
    removeOnComplete: 1000,
    removeOnFail: 5000,
  });
  return job;
}

export async function audit(actor: string, action: string, tenantId?: string, details?: object) {
  await prisma.auditLog.create({ data: { actor, action, tenantId, details: details as never } });
}
