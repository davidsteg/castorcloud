// Ops CLI for data-plane nodes: TrueNAS SCALE or Mac minis. Set the node up
// first (infra/node/truenas/README.md, infra/node/macos/README.md).
//   npm run node -w @pc/provisioner -- add <name> <user@host> --kind truenas|mac --pool <pool> --edge <host[:port]> --auth-host-ip <ip> [--capacity 20]
//     --edge: where the node's edge proxy listens on the LAN; the control
//     plane's Traefik forwards the node's tenant hosts there. TrueNAS: the
//     NAS's address with :8480. Mac: the Colima VM's address (port 80).
//     setup.sh / bootstrap.sh print the exact command.
//     --auth-host-ip: the control plane's address as seen from the Mac (tenant
//     containers reach auth.<domain> there for OIDC, server to server)
//   npm run node -w @pc/provisioner -- add <name> <user@host> --kind mac --role stacks --pool <pool> --edge <host[:port]> --control-plane-ip <ip> [--uid-base 100000] [--no-gvisor]
//     A STACKS node runs customers' own stacks and nothing else (docs/stacks.md).
//     Its Docker must run with userns-remap (checked), and should have gVisor
//     (runsc; --no-gvisor registers it without, and says so in the audit log).
//     --control-plane-ip: the only address its edge proxy accepts requests from.
//     --uid-base: the host uid that root in a container maps to (/etc/subuid of dockremap).
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
const { stacksFacts } = await import("./stacks-node.ts");

const { positionals, values } = parseArgs({
  allowPositionals: true,
  options: {
    kind: { type: "string", default: "truenas" },
    pool: { type: "string" },
    capacity: { type: "string", default: "20" },
    "auth-host-ip": { type: "string" },
    edge: { type: "string" },
    role: { type: "string", default: "tenants" },
    "control-plane-ip": { type: "string", multiple: true },
    "uid-base": { type: "string", default: "100000" },
    "no-gvisor": { type: "boolean", default: false },
  },
});
const [cmd, name, target] = positionals;
const nodes = new Nodes(root);

switch (cmd) {
  case "add": {
    if (values.role === "stacks") {
      await addStacksNode();
      break;
    }
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
    const existing = await prisma.node.findUnique({ where: { name }, include: { _count: { select: { stackTenants: true } } } });
    if (existing?.role === "STACKS" && existing._count.stackTenants > 0) throw new Error(`${name} runs custom stacks; it can't take tenants`);
    const data = {
      kind, role: "TENANTS" as const, sshHost, sshUser, zfsPool: values.pool, authHostIp: values["auth-host-ip"] ?? null, edgeHost: values.edge!,
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
          (n.role === "STACKS" ? `stacks (${n.stackRuntime ?? "?"})` : `${n._count.tenants}/${n.capacity} tenants`).padEnd(14), pool.padEnd(24), `seen ${seen}`,
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

async function addStacksNode() {
  const kind = values.kind?.toUpperCase() as "TRUENAS" | "MAC";
  const ips = values["control-plane-ip"] ?? [];
  const uidBase = Number(values["uid-base"]);
  if (!name || !target?.includes("@") || !values.pool || !values.edge || !ips.length || !["TRUENAS", "MAC"].includes(kind)) {
    throw new Error("usage: add <name> <user@host> --kind mac|truenas --role stacks --pool <pool> --edge <host[:port]> --control-plane-ip <ip> [--uid-base 100000] [--no-gvisor]");
  }
  for (const ip of ips) if (!/^[0-9a-fA-F.:]+(\/\d{1,3})?$/.test(ip)) throw new Error(`invalid --control-plane-ip ${ip}`);
  if (!Number.isInteger(uidBase) || uidBase < 65536) throw new Error("--uid-base must be at least 65536");
  const [sshUser, sshHost] = target.split("@");
  const ssh = new SshNode(root, { name, kind, target, pool: values.pool, edgeHost: values.edge, role: "STACKS", controlPlaneIps: ips });
  console.log(`probing ${target}…`);
  const facts = ssh.probe();
  const docker = stacksFacts(ssh);
  console.log(`  ${facts.osVersion}, Docker ${facts.dockerVersion}: ${docker.cpus} CPUs, ${formatStorage(docker.memoryMb)} RAM, ${docker.arch}`);
  console.log(`  pool ${values.pool}: ${facts.poolHealth}, ${formatStorage(facts.poolUsedMb)} of ${formatStorage(facts.poolSizeMb)} used`);
  console.log(`  userns-remap: ${docker.userns ? "yes" : "NO"}, gVisor: ${docker.runsc ? "yes" : "no"}`);
  if (facts.poolHealth !== "ONLINE") throw new Error(`pool is ${facts.poolHealth}, not registering`);
  if (!docker.userns) throw new Error("Docker must run with userns-remap on a stacks node (root in a container must not be root on the node)");
  if (!docker.runsc && !values["no-gvisor"]) throw new Error("gVisor (runsc) is missing; install it, or pass --no-gvisor to accept plain runc");
  const already = await prisma.node.findUnique({ where: { name }, include: { _count: { select: { tenants: true } } } });
  if (already && (already.role !== "STACKS" || already._count.tenants > 0)) throw new Error(`${name} already holds tenants; a stacks node must be a separate machine`);
  const data = {
    kind, role: "STACKS" as const, sshHost, sshUser, zfsPool: values.pool, edgeHost: values.edge!,
    capacity: 0, enabled: true, controlPlaneIps: ips.join(","), uidBase, stackRuntime: docker.runsc ? "runsc" : "runc",
    poolSizeMb: facts.poolSizeMb, poolUsedMb: facts.poolUsedMb, poolHealth: facts.poolHealth,
    memoryMb: docker.memoryMb, cpus: docker.cpus, lastSeenAt: new Date(), lastError: null,
  };
  const row = await prisma.node.upsert({ where: { name }, update: data, create: { name, ...data } });
  console.log("building the egress proxy and starting the edge proxy…");
  nodes.driver(row);
  await audit("ops-cli", "node.registered", undefined, { name, target, pool: values.pool, role: "STACKS", runtime: data.stackRuntime, controlPlaneIps: ips });
  console.log(`node ${name} is ready for custom stacks`);
}
