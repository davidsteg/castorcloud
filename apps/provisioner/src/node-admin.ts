// Ops CLI for data-plane nodes (Mac minis). The Mac must be bootstrapped first
// (infra/node/macos/README.md).
//   npm run node -w @pc/provisioner -- add <name> <user@host> --auth-host-ip <ip> [--pool castor] [--capacity 20]
//     --auth-host-ip: the control plane's address as seen from the Mac (tenant
//     containers reach auth.<domain> there for OIDC, server to server)
//   npm run node -w @pc/provisioner -- list
//   npm run node -w @pc/provisioner -- drain <name>     no new tenants; existing ones stay
//   npm run node -w @pc/provisioner -- enable <name>
import path from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";
import { config } from "dotenv";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../..");
config({ path: path.join(root, ".env"), quiet: true });
const { prisma } = await import("@pc/db");
const { audit } = await import("@pc/core");
const { formatStorage } = await import("@pc/pricing");
const { Nodes } = await import("./nodes.ts");
const { SshNode } = await import("./node.ts");

const { positionals, values } = parseArgs({
  allowPositionals: true,
  options: {
    pool: { type: "string", default: "castor" },
    capacity: { type: "string", default: "20" },
    "auth-host-ip": { type: "string" },
  },
});
const [cmd, name, target] = positionals;
const nodes = new Nodes(root);

switch (cmd) {
  case "add": {
    if (!name || !target?.includes("@") || !values["auth-host-ip"]) {
      throw new Error("usage: add <name> <user@host> --auth-host-ip <ip> [--pool castor] [--capacity 20]");
    }
    const [sshUser, sshHost] = target.split("@");
    const ssh = new SshNode(root, { name, target, pool: values.pool!, authHostIp: values["auth-host-ip"] });
    console.log(`probing ${target}…`);
    const facts = ssh.probe();
    console.log(`  macOS ${facts.macosVersion}, ${facts.cpus} CPUs, ${formatStorage(facts.memoryMb)} RAM, Docker ${facts.dockerVersion}`);
    console.log(`  pool ${values.pool}: ${facts.poolHealth}, ${formatStorage(facts.poolUsedMb)} of ${formatStorage(facts.poolSizeMb)} used`);
    if (facts.poolHealth !== "ONLINE") throw new Error(`pool is ${facts.poolHealth}, not registering`);
    const data = {
      kind: "MAC" as const, sshHost, sshUser, zfsPool: values.pool!, authHostIp: values["auth-host-ip"] ?? null,
      capacity: Number(values.capacity), enabled: true,
      poolSizeMb: facts.poolSizeMb, poolUsedMb: facts.poolUsedMb, poolHealth: facts.poolHealth,
      memoryMb: facts.memoryMb, lastSeenAt: new Date(), lastError: null,
    };
    const row = await prisma.node.upsert({ where: { name }, update: data, create: { name, ...data } });
    console.log("syncing templates and starting the edge proxy…");
    nodes.driver(row);
    await audit("ops-cli", "node.registered", undefined, { name, target, pool: values.pool });
    console.log(`node ${name} is ready for tenants`);
    break;
  }
  case "drain":
  case "enable": {
    if (!name) throw new Error(`usage: ${cmd} <name>`);
    await prisma.node.update({ where: { name }, data: { enabled: cmd === "enable" } });
    await audit("ops-cli", `node.${cmd === "enable" ? "enabled" : "drained"}`, undefined, { name });
    console.log(`${name}: ${cmd === "enable" ? "takes new tenants" : "draining (no new tenants)"}`);
    break;
  }
  case "list":
  case undefined: {
    const rows = await prisma.node.findMany({
      orderBy: { name: "asc" },
      include: { _count: { select: { tenants: { where: { status: { not: "DELETED" } } } } } },
    });
    for (const n of rows) {
      const pool = n.poolSizeMb ? `${n.poolHealth} ${formatStorage(n.poolUsedMb ?? 0)}/${formatStorage(n.poolSizeMb)}` : "-";
      const seen = n.lastSeenAt ? n.lastSeenAt.toISOString() : "never";
      console.log(
        [n.name.padEnd(12), n.kind.padEnd(5), (n.enabled ? "active" : "draining").padEnd(8),
          `${n._count.tenants}/${n.capacity} tenants`.padEnd(14), pool.padEnd(24), `seen ${seen}`,
          n.lastError ? `  error: ${n.lastError.slice(0, 80)}` : ""].join(" "),
      );
    }
    break;
  }
  default:
    throw new Error(`unknown command: ${cmd}`);
}
await prisma.$disconnect();
process.exit(0);
