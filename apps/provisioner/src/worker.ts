// Provisioning worker: executes tenant lifecycle jobs enqueued by the portal
// (after Stripe webhooks), and runs periodic maintenance.
//   npm run worker -w @pc/provisioner
import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { config } from "dotenv";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../..");
config({ path: path.join(root, ".env"), quiet: true });

const { prisma } = await import("@pc/db");
const { RENEW_MS, audit, claimScheduled, claimTenantJob, enqueueTenantJob, failTenantJob, recoverExpiredJobs, renewLease, succeedTenantJob, workerHeartbeat, workerId } = await import("@pc/core");
const { Authentik } = await import("./authentik.ts");
const { Lifecycle } = await import("./lifecycle.ts");
const { LlmGateway } = await import("./llm.ts");
const { Nodes } = await import("./nodes.ts");
const { Backups, backupSettings } = await import("./backup.ts");
const { parseEnvFile } = await import("./tenant.ts");
type TenantJob = import("@pc/core").TenantJob;
type JobKind = import("@pc/db").JobKind;

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
const lifecycle = new Lifecycle((id) => nodes.forTenant(id), ak, platform, parseEnvFile(readFileSync(versionsFile, "utf8")), versionsFile, assistant);
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

async function run(job: TenantJob) {
  const { id: jobId, tenantId, kind } = job;
  const log = async (line: string) => {
    console.log(`[${kind} ${tenantId}] ${line}`);
    await prisma.$executeRaw`UPDATE "Job" SET log = log || ${`${new Date().toISOString()} ${line}\n`} WHERE id = ${jobId}`;
  };

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
}

// ------------------------------------------------------------ the queue
// Jobs come from the Job table (packages/core/src/jobs.ts). One at a time:
// stacks are heavy to start. The kinds this worker runs; once the Rust
// worker takes one over, it leaves this list.
const KINDS: readonly JobKind[] = ["PROVISION", "UPDATE", "SUSPEND", "RESUME", "DELETE", "REVOKE_ACCESS", "RESTORE"];
const me = workerId();
let stopping = false;
for (const sig of ["SIGINT", "SIGTERM"] as const) {
  process.on(sig, () => {
    if (stopping) process.exit(130); // a second signal: stop now (the job's lease expires and it is retried)
    console.log(`${sig}: finishing the current job, then stopping`);
    stopping = true;
  });
}

async function onFailed(job: TenantJob, message: string, final: boolean) {
  console.error(`[${job.kind} ${job.tenantId}] attempt ${job.attempts} failed: ${message}`);
  if (final && job.kind === "PROVISION") {
    await prisma.tenant.update({ where: { id: job.tenantId }, data: { status: "FAILED" } });
    await audit("system", "tenant.provision_failed", job.tenantId, { error: message.slice(0, 500) });
  }
}

async function work(job: TenantJob) {
  const renew = setInterval(() => {
    renewLease(job.id, me).then((mine) => {
      if (!mine) console.error(`[${job.kind} ${job.tenantId}] lost the lease on job ${job.id}`);
    }, (err) => console.error(`[${job.kind} ${job.tenantId}] renewing the lease: ${(err as Error).message}`));
  }, RENEW_MS);
  try {
    await run(job);
    await succeedTenantJob(job.id, me);
  } catch (err) {
    const message = (err as Error).message ?? String(err);
    await onFailed(job, message, await failTenantJob(job.id, me, message));
  } finally {
    clearInterval(renew);
  }
}

async function queueLoop() {
  while (!stopping) {
    try {
      await workerHeartbeat(me, KINDS);
      for (const job of await recoverExpiredJobs()) await onFailed(job, "the worker running it stopped", job.failed);
      const job = await claimTenantJob(me, KINDS);
      if (job) {
        await work(job);
        continue;
      }
    } catch (err) {
      console.error(`[queue] ${(err as Error).message}`);
    }
    await sleep(2_000);
  }
}

// ------------------------------------------------------------ recurring tasks
// Each runs once per slot across all workers (packages/core/src/schedule.ts).
type Task = { name: string; when: import("@pc/core").Every; run: () => Promise<void> };

// Hourly: delete tenants whose grace period after cancellation has ended,
// and measure every active tenant's storage usage for the portal.
const maintenance: Task[] = [
  { name: "watch-nodes", when: { everyMs: 5 * 60_000 }, run: () => nodes.watch() },
  {
    name: "measure-storage",
    when: { everyMs: 60 * 60_000 },
    async run() {
      const active = await prisma.tenant.findMany({ where: { status: "ACTIVE" }, select: { id: true } });
      for (const t of active) {
        try {
          await lifecycle.measure(t.id);
        } catch (err) {
          console.error(`[measure ${t.id}] ${(err as Error).message}`);
        }
      }
    },
  },
  {
    name: "sweep-deletions",
    when: { everyMs: 60 * 60_000 },
    async run() {
      const due = await prisma.tenant.findMany({
        where: { status: "SUSPENDED", deleteAfter: { lt: new Date() } },
        select: { id: true },
      });
      for (const t of due) await enqueueTenantJob(t.id, "DELETE");
      if (due.length) console.log(`scheduled ${due.length} deletion(s)`);
    },
  },
];

// Backups: their own loop, so a long first send doesn't hold up the node
// watcher. Every BACKUP_INTERVAL_HOURS: send, then prune. Nightly: the
// control-plane database. Weekly: read every tenant's newest backup back.
const backupTasks: Task[] = backups
  ? [
      {
        name: "backup-tenants",
        when: { everyMs: backups.s.intervalHours * 3600_000 },
        async run() {
          await backups.runAll((line) => console.log(`[backup-tenants] ${line}`));
          for (const p of await backups.problems()) console.error(`[backup ${p.slug}] ${p.issues.join("; ")}`);
        },
      },
      {
        name: "backup-database",
        when: { at: "02:30", tz: platform.timeZone },
        async run() {
          if (backups.s.dbPublicKey) await backups.dumpDatabase((line) => console.log(`[backup-database] ${line}`));
          else console.error("[backup-database] BACKUP_DB_PUBLIC_KEY is not set: the control-plane database is NOT backed up");
        },
      },
      {
        name: "verify-backups",
        when: { at: "04:00", tz: platform.timeZone, weekday: 0 },
        run: () => backups.verifyAll((line) => console.log(`[verify-backups] ${line}`)),
      },
    ]
  : [];

async function taskLoop(tasks: Task[]) {
  while (!stopping) {
    for (const t of tasks) {
      if (stopping) break;
      try {
        if (await claimScheduled(t.name, t.when)) await t.run();
      } catch (err) {
        console.error(`[${t.name}] ${(err as Error).message}`);
      }
    }
    await sleep(30_000);
  }
}

console.log(`worker ${me} ready (jobs: ${KINDS.join(", ")})`);
await Promise.all([queueLoop(), taskLoop(maintenance), taskLoop(backupTasks)]);
await prisma.$disconnect();
