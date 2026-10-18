import "server-only";
import { prisma, type TenantStatus } from "@pc/db";
import { quote, STORAGE_UNIT_GB, type Config } from "@pc/pricing";

// Figures for the staff console. Aggregates only: nothing here names a
// tenant or a person, and nothing reads tenant content (that never lives in
// the control-plane database anyway). Callers must check staffUser() first.

const DAY = 24 * 60 * 60 * 1000;
/** Tenants that hold data on a node: they count towards storage and backups. */
const LIVE: TenantStatus[] = ["PROVISIONING", "ACTIVE", "SUSPENDED"];
/** A backup older than this counts as stale (backups run daily, docs/backups.md). */
const BACKUP_STALE_MS = 2 * DAY;

export async function adminStats(now = new Date()) {
  const since30 = new Date(now.getTime() - 30 * DAY);
  const since7 = new Date(now.getTime() - 7 * DAY);
  const staleBefore = new Date(now.getTime() - BACKUP_STALE_MS);

  const [
    byStatus, users, newUsers, staff, members, appliedRecent, activated,
    services, storage, nodes, failedJobs, queuedJobs, alerts, liveCount, backupsFailing, backupsStale,
  ] = await Promise.all([
    prisma.tenant.groupBy({ by: ["status"], _count: true }),
    prisma.user.count(),
    prisma.user.count({ where: { createdAt: { gte: since30 } } }),
    prisma.user.count({ where: { isStaff: true } }),
    prisma.member.count({ where: { tenant: { status: { in: LIVE } } } }),
    prisma.tenant.count({ where: { appliedAt: { gte: since30 } } }),
    // Only the fields the price depends on.
    prisma.tenant.findMany({
      where: { status: "ACTIVE" },
      select: {
        storageUnits: true, extraMembers: true, aiCreditChf: true, billingInterval: true,
        stackMemoryUnits: true, stackCpuUnits: true, stackStorageUnits: true,
        services: { select: { kind: true } },
      },
    }),
    prisma.serviceInstance.groupBy({ by: ["kind"], where: { tenant: { status: { in: LIVE } } }, _count: true }),
    prisma.tenant.aggregate({
      where: { status: { in: LIVE } },
      _sum: { storageUnits: true, storageVolumeMb: true, storageUsedMb: true },
    }),
    prisma.node.findMany({
      select: {
        name: true, role: true, enabled: true, capacity: true, poolSizeMb: true, poolUsedMb: true, poolHealth: true, lastSeenAt: true,
        _count: { select: { tenants: { where: { status: { in: LIVE } } }, stackTenants: { where: { status: { in: LIVE } } } } },
      },
      orderBy: { name: "asc" },
    }),
    prisma.job.count({ where: { status: "FAILED", finishedAt: { gte: since7 } } }),
    prisma.job.count({ where: { status: { in: ["QUEUED", "RUNNING"] } } }),
    prisma.alert.groupBy({ by: ["severity"], where: { resolvedAt: null }, _count: true }),
    prisma.tenant.count({ where: { status: { in: LIVE } } }),
    prisma.tenantBackup.count({ where: { failures: { gt: 0 }, tenant: { status: { in: LIVE } } } }),
    prisma.tenant.count({
      where: {
        status: { in: LIVE },
        OR: [{ backup: null }, { backup: { lastSuccessAt: null } }, { backup: { lastSuccessAt: { lt: staleBefore } } }],
      },
    }),
  ]);

  // Monthly recurring revenue at list price incl. VAT, yearly plans spread
  // over 12 months (quote().effectiveMonthlyRp). An estimate: Stripe is the
  // source of truth for what was actually charged.
  const mrrRp = activated.reduce((sum, t) => sum + quote({
    services: t.services.map((s) => s.kind.toLowerCase()) as Config["services"],
    storageUnits: t.storageUnits,
    extraMembers: t.extraMembers,
    aiCreditChf: t.aiCreditChf,
    interval: t.billingInterval === "YEAR" ? "year" : "month",
    stacks: { memoryUnits: t.stackMemoryUnits, cpuUnits: t.stackCpuUnits, storageUnits: t.stackStorageUnits },
  }).effectiveMonthlyRp, 0);

  const tenants = Object.fromEntries(byStatus.map((s) => [s.status, s._count])) as Partial<Record<TenantStatus, number>>;
  return {
    tenants,
    users: { total: users, new30d: newUsers, staff, liveMembers: members },
    applications30d: appliedRecent,
    revenue: { mrrRp, yearlyPlans: activated.filter((t) => t.billingInterval === "YEAR").length },
    services: Object.fromEntries(services.map((s) => [s.kind, s._count])) as Record<string, number>,
    storage: {
      bookedGb: (storage._sum.storageUnits ?? 0) * STORAGE_UNIT_GB,
      volumeMb: storage._sum.storageVolumeMb ?? 0,
      usedMb: storage._sum.storageUsedMb ?? 0,
    },
    nodes: nodes.map(({ _count, ...n }) => ({ ...n, tenants: n.role === "STACKS" ? _count.stackTenants : _count.tenants })),
    jobs: { failed7d: failedJobs, pending: queuedJobs },
    alerts: Object.fromEntries(alerts.map((a) => [a.severity, a._count])) as Record<string, number>,
    backups: { live: liveCount, failing: backupsFailing, stale: backupsStale },
  };
}
