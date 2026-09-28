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
const { Nodes } = await import("./nodes.ts");
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
const lifecycle = new Lifecycle((id) => nodes.forTenant(id), ak, platform, parseEnvFile(readFileSync(versionsFile, "utf8")), versionsFile);
const connection = redisConnection();

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
      break;
    case "SUSPEND":
      await lifecycle.suspend(tenantId);
      await log("suspended");
      break;
    case "DELETE":
      await lifecycle.delete(tenantId);
      await log("deleted");
      break;
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

new Worker(
  "maintenance",
  async (job) => {
    if (job.name === "watch-nodes") {
      await nodes.watch();
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
