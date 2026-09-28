// Ops CLI for data-plane nodes: TrueNAS SCALE or Mac minis. Set the node up
// first (infra/node/truenas/README.md, infra/node/macos/README.md).
//   npm run node -w @pc/provisioner -- add <name> <user@host> --kind truenas|mac --pool <pool> --edge <host[:port]> --auth-host-ip <ip> [--capacity 20]
//     --edge: where the node's edge proxy listens on the LAN; the control
//     plane's Traefik forwards the node's tenant hosts there. TrueNAS: the
//     NAS's address with :8480. Mac: the Colima VM's address (port 80).
//     setup.sh / bootstrap.sh print the exact command.
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
    kind: { type: "string", default: "truenas" },
    pool: { type: "string" },
    capacity: { type: "string", default: "20" },
    "auth-host-ip": { type: "string" },
    edge: { type: "string" },
  },
});
const [cmd, name, target] = positionals;
const nodes = new Nodes(root);

switch (cmd) {
  case "add": {
    const kind = values.kind?.toUpperCase() as "TRUENAS" | "MAC";
    if (!name || !target?.includes("@") || !values.pool || !values["auth-host-ip"] || !values.edge || !["TRUENAS", "MAC"].includes(kind)) {
      throw new Error("usage: add <name> <user@host> --kind truenas|mac --pool <pool> --edge <host[:port]> --auth-host-ip <ip> [--capacity 20]");
    }
    const [sshUser, sshHost] = target.split("@");
    const ssh = new SshNode(root, { name, kind, target, pool: values.pool, authHostIp: values["auth-host-ip"], edgeHost: values.edge });
    console.log(`probing ${target}…`);
    const facts = ssh.probe();
    console.log(`  ${facts.osVersion}, ${facts.cpus} CPUs, ${formatStorage(facts.memoryMb)} RAM, Docker ${facts.dockerVersion}`);
    console.log(`  pool ${values.pool}: ${facts.poolHealth}, ${formatStorage(facts.poolUsedMb)} of ${formatStorage(facts.poolSizeMb)} used`);
    if (facts.poolHealth !== "ONLINE") throw new Error(`pool is ${facts.poolHealth}, not registering`);
    const data = {
      kind, sshHost, sshUser, zfsPool: values.pool, authHostIp: values["auth-host-ip"] ?? null, edgeHost: values.edge!,
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
