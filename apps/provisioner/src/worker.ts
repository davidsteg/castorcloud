// Provisioning worker: executes tenant lifecycle jobs enqueued by the portal
// (after Stripe webhooks), and runs periodic maintenance.
//   npm run worker -w @pc/provisioner
import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { config } from "dotenv";
import { Queue, Worker, type Job as BullJob } from "bullmq";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../..");
config({ path: path.join(root, ".env"), quiet: true });

const { prisma } = await import("@pc/db");
const { TENANT_QUEUE, audit, enqueueTenantJob, redisConnection } = await import("@pc/core");
const { Authentik } = await import("./authentik.ts");
const { Lifecycle } = await import("./lifecycle.ts");
const { LlmGateway } = await import("./llm.ts");
const { Nodes } = await import("./nodes.ts");
const { Backups, backupSettings } = await import("./backup.ts");
const { Stacks } = await import("./stacks.ts");
const { parseEnvFile } = await import("./tenant.ts");
type TenantJobData = import("@pc/core").TenantJobData;

const versionsFile = path.join(root, "packages/stack-templates/versions.env");
const nodes = new Nodes(root);
const platform = {
  baseDomain: process.env.BASE_DOMAIN ?? "brand.localhost",
  scheme: (process.env.PUBLIC_SCHEME ?? "https") as "http" | "https",
  brandName: process.env.BRAND_NAME ?? "Castor Cloud",
  timeZone: process.env.TZ ?? "Europe/Zurich",
  authHostIp: process.env.AUTH_HOST_IP ?? "host-gateway",
};
const ak = new Authentik(`${platform.scheme}://auth.${platform.baseDomain}`, process.env.AUTHENTIK_BOOTSTRAP_TOKEN ?? "");
// Assistant: the node's LLM gateway. Without a master key tenants can't book it.
const assistant = process.env.LITELLM_MASTER_KEY
  ? {
      gateway: new LlmGateway(process.env.LLM_GATEWAY_URL ?? "http://localhost:4000", process.env.LITELLM_MASTER_KEY),
      usdPerChf: Number(process.env.AI_USD_PER_CHF ?? "1.1"),
    }
  : undefined;
// Customers' own stacks (docs/stacks.md), served under their own domain: code we
// don't control must never share cookies with the portal (*.BASE_DOMAIN).
const stacksDomain = process.env.STACKS_DOMAIN ?? "";
if (stacksDomain && stacksDomain === platform.baseDomain && platform.scheme === "https") {
  throw new Error("STACKS_DOMAIN must not be BASE_DOMAIN in production (custom stacks would share the portal's cookie domain)");
}
const stacks = stacksDomain
  ? new Stacks((tenantId) => nodes.placeStacks(tenantId), (nodeId) => nodes.forStacks(nodeId), ak, {
      domain: stacksDomain,
      scheme: platform.scheme,
      certResolver: process.env.STACKS_CERT_RESOLVER || undefined,
    })
  : undefined;
if (!stacks) console.warn("STACKS_DOMAIN is not set: custom stacks are OFF");
const lifecycle = new Lifecycle((id) => nodes.forTenant(id), ak, platform, parseEnvFile(readFileSync(versionsFile, "utf8")), versionsFile, assistant, stacks);

/** After the tenant's own stack: its custom stacks, in their own job (a stacks node outage never fails the tenant's). */
async function followWithStacks(tenantId: string) {
  if (!stacks) return;
  const t = await prisma.tenant.findUniqueOrThrow({ where: { id: tenantId }, select: { stacksNodeId: true, _count: { select: { stacks: true } } } });
  if (t.stacksNodeId || t._count.stacks > 0) await enqueueTenantJob(tenantId, "STACKS");
}
const connection = redisConnection();
const backupConfig = backupSettings();
const backups = backupConfig ? new Backups(backupConfig) : undefined;
if (!backups) console.warn("BACKUP_TARGET is not set: tenant backups are OFF");

// Self-registration flow: verified e-mail when SMTP is configured.
await ak.ensureBlueprintInstance(
  "personal-cloud-enrollment",
  process.env.AUTHENTIK_EMAIL__HOST ? "example/flows-enrollment-email-verification.yaml" : "example/flows-enrollment-2-stage.yaml",
  "default-enrollment-flow",
  "personal-cloud-signup",
);
// The Docker host the worker runs on takes tenants only when LOCAL_NODE isn't
// "off" (development). Production places tenants on Mac nodes (npm run node).
const localEnabled = process.env.LOCAL_NODE !== "off";
const localNode = await prisma.node.upsert({
  where: { name: "local" },
  update: { enabled: localEnabled },
  create: { name: "local", kind: "LOCAL", sshHost: "local", capacity: 50, enabled: localEnabled },
});
if (localEnabled) {
  console.log("preparing local node…");
  nodes.driver(localNode);
}

async function run(job: BullJob<TenantJobData>) {
  const { jobId, tenantId, kind } = job.data;
  const log = async (line: string) => {
    console.log(`[${kind} ${tenantId}] ${line}`);
    await prisma.$executeRaw`UPDATE "Job" SET log = log || ${`${new Date().toISOString()} ${line}\n`} WHERE id = ${jobId}`;
  };
  await prisma.job.update({ where: { id: jobId }, data: { status: "RUNNING", error: null } });

  const report = async (step: string, message: string) => {
    await prisma.job.update({ where: { id: jobId }, data: { step } });
    await log(`${step}: ${message}`);
  };

  switch (kind) {
    case "PROVISION":
      await log(`placed on node ${(await nodes.place(tenantId)).name}`);
      await lifecycle.apply(tenantId, report);
      break;
    case "UPDATE":
    case "RESUME":
      await nodes.place(tenantId); // no-op once placed
      await lifecycle.apply(tenantId, report);
      await followWithStacks(tenantId);
      break;
    case "STACKS":
      if (!stacks) throw new Error("STACKS_DOMAIN is not set");
      await stacks.apply(tenantId, report);
      break;
    case "REVOKE_ACCESS":
      await lifecycle.revokeAccess(tenantId, report);
      break;
    case "SUSPEND":
      await lifecycle.suspend(tenantId);
      await log("suspended");
      break;
    case "DELETE":
      await lifecycle.delete(tenantId);
      await log("deleted");
      break;
    case "RESTORE": {
      if (!backups) throw new Error("BACKUP_TARGET is not set");
      const row = await prisma.job.findUniqueOrThrow({ where: { id: jobId } });
      const params = row.params as { gen: string; snap: string; received?: boolean } | null;
      if (!params?.gen || !params.snap) throw new Error("RESTORE needs params { gen, snap }");
      // A retry after the data is back only restarts the stack.
      if (!params.received) {
        await lifecycle.stop(tenantId);
        const tenant = await prisma.tenant.findUniqueOrThrow({ where: { id: tenantId }, include: { node: true } });
        if (!tenant.node) throw new Error("tenant has no node");
        await backups.restore(tenant, tenant.node, params.gen, params.snap, log);
        await prisma.job.update({ where: { id: jobId }, data: { params: { ...params, received: true } } });
      }
      await lifecycle.apply(tenantId, report);
      break;
    }
  }
  await prisma.job.update({ where: { id: jobId }, data: { status: "SUCCEEDED", finishedAt: new Date() } });
}

// One job at a time: jobs of a tenant must not overlap, and stacks are heavy
// to start. Scale out with more workers per node group later.
const worker = new Worker<TenantJobData>(TENANT_QUEUE, run, { connection, concurrency: 1 });

worker.on("failed", async (job, err) => {
  if (!job) return;
  const final = job.attemptsMade >= (job.opts.attempts ?? 1);
  console.error(`[${job.data.kind} ${job.data.tenantId}] attempt ${job.attemptsMade} failed: ${err.message}`);
  await prisma.job.update({
    where: { id: job.data.jobId },
    data: { status: final ? "FAILED" : "QUEUED", error: err.message.slice(0, 4000) },
  });
  if (final && job.data.kind === "PROVISION") {
    await prisma.tenant.update({ where: { id: job.data.tenantId }, data: { status: "FAILED" } });
    await audit("system", "tenant.provision_failed", job.data.tenantId, { error: err.message.slice(0, 500) });
  }
});

// ------------------------------------------------------------ maintenance
// Hourly: delete tenants whose grace period after cancellation has ended,
// and measure every active tenant's storage usage for the portal.
const maintenance = new Queue("maintenance", { connection });
await maintenance.upsertJobScheduler("sweep-deletions", { every: 60 * 60 * 1000 }, { name: "sweep-deletions" });
await maintenance.upsertJobScheduler("measure-storage", { every: 60 * 60 * 1000 }, { name: "measure-storage" });
await maintenance.upsertJobScheduler("watch-nodes", { every: 5 * 60 * 1000 }, { name: "watch-nodes" });

// Backups: their own queue, so a long first send doesn't hold up the node
// watcher. Every BACKUP_INTERVAL_HOURS: send, then prune. Nightly: the
// control-plane database. Weekly: read every tenant's newest backup back.
if (backups) {
  const backupQueue = new Queue("backups", { connection });
  await backupQueue.upsertJobScheduler("backup-tenants", { every: backups.s.intervalHours * 3600_000 }, { name: "backup-tenants" });
  await backupQueue.upsertJobScheduler("backup-database", { pattern: "30 2 * * *", tz: platform.timeZone }, { name: "backup-database" });
  await backupQueue.upsertJobScheduler("verify-backups", { pattern: "0 4 * * 0", tz: platform.timeZone }, { name: "verify-backups" });
  new Worker(
    "backups",
    async (job) => {
      const log = (line: string) => console.log(`[${job.name}] ${line}`);
      if (job.name === "backup-tenants") {
        await backups.runAll(log);
        for (const p of await backups.problems()) console.error(`[backup ${p.slug}] ${p.issues.join("; ")}`);
      } else if (job.name === "backup-database") {
        if (backups.s.dbPublicKey) await backups.dumpDatabase(log);
        else console.error("[backup-database] BACKUP_DB_PUBLIC_KEY is not set: the control-plane database is NOT backed up");
      } else if (job.name === "verify-backups") {
        await backups.verifyAll(log);
      }
    },
    // Hours-long sends: renew the lock well within its duration.
    { connection, concurrency: 1, lockDuration: 5 * 60_000 },
  );
}

new Worker(
  "maintenance",
  async (job) => {
    if (job.name === "watch-nodes") {
      await nodes.watch(stacks ? (id) => stacks.needsUnlock(id) : undefined);
      return;
    }
    if (job.name === "measure-storage") {
      const active = await prisma.tenant.findMany({ where: { status: "ACTIVE" }, select: { id: true } });
      for (const t of active) {
        try {
          await lifecycle.measure(t.id);
        } catch (err) {
          console.error(`[measure ${t.id}] ${(err as Error).message}`);
        }
      }
      return;
    }
    const due = await prisma.tenant.findMany({
      where: { status: "SUSPENDED", deleteAfter: { lt: new Date() } },
      select: { id: true },
    });
    for (const t of due) await enqueueTenantJob(t.id, "DELETE");
    if (due.length) console.log(`scheduled ${due.length} deletion(s)`);
  },
  { connection },
);

console.log(`worker ready (queue ${TENANT_QUEUE})`);
