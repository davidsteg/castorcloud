// The node registry: turns Node rows into drivers, places new tenants, and
// health-checks remote (TrueNAS and Mac) nodes.
import { prisma, type Node } from "@pc/db";
import { audit, enqueueTenantJob } from "@pc/core";
import { LocalNode, SshNode, type NodeExec } from "./node.ts";
import { pickNode } from "./placement.ts";

export class Nodes {
  private readonly drivers = new Map<string, NodeExec>();

  constructor(private readonly root: string) {}

  driver(row: Node): NodeExec {
    let d = this.drivers.get(row.id);
    if (!d) {
      if (row.kind === "LOCAL") {
        d = new LocalNode(this.root);
      } else {
        if (!row.zfsPool) throw new Error(`node ${row.name} has no ZFS pool`);
        d = new SshNode(this.root, {
          name: row.name,
          kind: row.kind,
          target: `${row.sshUser}@${row.sshHost}`,
          pool: row.zfsPool,
          authHostIp: row.authHostIp ?? undefined,
          edgeHost: row.edgeHost ?? undefined,
        });
      }
      // Once per worker process: brings templates, brand assets and the edge
      // proxy on the node up to date with this deployment.
      d.prepare();
      this.drivers.set(row.id, d);
    }
    return d;
  }

  /** Driver for a tenant's node (Lifecycle's NodeResolver). */
  async forTenant(nodeId: string | null): Promise<NodeExec> {
    if (!nodeId) throw new Error("tenant has no node yet");
    return this.driver(await prisma.node.findUniqueOrThrow({ where: { id: nodeId } }));
  }

  /** Assigns a node to the tenant unless it has one. */
  async place(tenantId: string): Promise<{ id: string; name: string }> {
    const tenant = await prisma.tenant.findUniqueOrThrow({ where: { id: tenantId }, include: { node: true } });
    if (tenant.node) return tenant.node;
    const nodes = await prisma.node.findMany({
      include: { tenants: { where: { status: { not: "DELETED" } }, select: { storageUnits: true } } },
    });
    const res = pickNode(nodes, tenant.storageUnits);
    if (!res.node) throw new Error(`no node can take ${tenant.slug}: ${res.reasons.join("; ") || "no nodes registered"}`);
    await prisma.tenant.update({ where: { id: tenant.id }, data: { nodeId: res.node.id } });
    await audit("system", "tenant.placed", tenant.id, { node: res.node.name });
    return res.node;
  }

  /** Measures a remote node and records the result (lastSeenAt only on success). */
  async probe(row: Node) {
    if (row.kind === "LOCAL") throw new Error("the local node isn't probed");
    try {
      const facts = new SshNode(this.root, {
        name: row.name, kind: row.kind, target: `${row.sshUser}@${row.sshHost}`, pool: row.zfsPool ?? "castor",
      }).probe();
      return await prisma.node.update({
        where: { id: row.id },
        data: {
          poolSizeMb: facts.poolSizeMb, poolUsedMb: facts.poolUsedMb, poolHealth: facts.poolHealth,
          memoryMb: facts.memoryMb, lastSeenAt: new Date(), lastError: null,
        },
      });
    } catch (err) {
      await prisma.node.update({ where: { id: row.id }, data: { lastError: (err as Error).message.slice(0, 2000) } });
      throw err;
    }
  }

  /**
   * Every few minutes: probe remote nodes, and reopen tenants whose storage is
   * locked although they are active (the node restarted: ZFS keys are never
   * stored on the node, so datasets come back locked and the stack can't
   * start until the provisioner unlocks it).
   */
  async watch() {
    const remote = await prisma.node.findMany({ where: { kind: { in: ["MAC", "TRUENAS"] } } });
    for (const row of remote) {
      try {
        await this.probe(row);
      } catch (err) {
        console.error(`[node ${row.name}] ${(err as Error).message}`);
        continue;
      }
      const tenants = await prisma.tenant.findMany({
        where: { nodeId: row.id, status: "ACTIVE" },
        include: { volumeKey: true },
      });
      for (const t of tenants) {
        if (!t.volumeKey) continue;
        const busy = await prisma.job.count({ where: { tenantId: t.id, status: { in: ["QUEUED", "RUNNING"] } } });
        if (busy) continue;
        const state = this.driver(row).storage(t.id, t.slug, t.volumeKey.wrappedKey).status().state;
        if (state === "closed") {
          console.log(`[node ${row.name}] ${t.slug}: storage locked, reopening`);
          await enqueueTenantJob(t.id, "UPDATE");
        }
      }
    }
  }
}
