// Ops CLI for backups (docs/backups.md). Needs BACKUP_TARGET in .env.
//   npm run backup -w @pc/provisioner -- status              every tenant's last backup, and problems
//   npm run backup -w @pc/provisioner -- list <slug>         the backups of one tenant on the box
//   npm run backup -w @pc/provisioner -- run [<slug>]        back up now (all, or one tenant)
//   npm run backup -w @pc/provisioner -- verify              read every newest backup back
//   npm run backup -w @pc/provisioner -- restore <slug> [<gen>@<snap>] [--node <name>]
//       queues a restore (default: the newest backup). The current data is set
//       aside on the node, not deleted. --node restores onto another node
//       (the tenant's own node is lost); the tenant then runs there.
//   npm run backup -w @pc/provisioner -- discard <slug> <tag>   removes data set aside by a restore
//   npm run backup -w @pc/provisioner -- db-keygen           key pair for database dumps
//   npm run backup -w @pc/provisioner -- db-dump             dump the control-plane database now
//   npm run backup -w @pc/provisioner -- db-list
//   npm run backup -w @pc/provisioner -- db-get <name> <out-file>
//   npm run backup -w @pc/provisioner -- db-open <file> <secret-key-file> <out.sql.gz>
import { readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";
import { config } from "dotenv";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../..");
config({ path: path.join(root, ".env"), quiet: true });
const { dumpKeyPair, openDump } = await import("./db-dump.ts");

const { positionals, values } = parseArgs({ allowPositionals: true, options: { node: { type: "string" } } });
const [cmd, a1, a2, a3] = positionals;

// Commands that need neither the database nor the backup box.
if (cmd === "db-keygen") {
  const kp = dumpKeyPair();
  console.log(`Public key, for .env on the control plane:\n  BACKUP_DB_PUBLIC_KEY=${kp.publicKey}\n`);
  console.log(`Secret key. Keep it OFFLINE (password manager, printed copy in a safe),`);
  console.log(`never on the control plane or the backup box. Without it no dump can be read:\n  ${kp.secretKey}`);
  process.exit(0);
}
if (cmd === "db-open") {
  if (!a1 || !a2 || !a3) throw new Error("usage: db-open <file> <secret-key-file> <out.sql.gz>");
  writeFileSync(a3, openDump(readFileSync(a1), readFileSync(a2, "utf8")), { mode: 0o600 });
  console.log(`decrypted to ${a3} (gzip'd SQL: gunzip -c ${a3} | psql -U <user> -d postgres)`);
  process.exit(0);
}

const { prisma } = await import("@pc/db");
const { audit, enqueueTenantJob } = await import("@pc/core");
const { Backups, backupSettings, hasZfs } = await import("./backup.ts");
const settings = backupSettings();
if (!settings) throw new Error("BACKUP_TARGET is not set in .env");
const backups = new Backups(settings);

const tenantBySlug = (slug: string | undefined) => {
  if (!slug) throw new Error(`usage: ${cmd} <slug>`);
  return prisma.tenant.findUniqueOrThrow({ where: { slug }, include: { node: true, backup: true } });
};

switch (cmd) {
  case "status":
  case undefined: {
    const pool = backups.pool();
    console.log(`backup box ${settings.target}: ${pool.health}, ${pool.usedMb} of ${pool.sizeMb} MB used\n`);
    const rows = await prisma.tenant.findMany({
      where: { status: { not: "DELETED" } }, include: { backup: true, node: true }, orderBy: { slug: "asc" },
    });
    for (const t of rows) {
      const b = t.backup;
      const zfs = t.node && hasZfs(t.node);
      console.log([
        t.slug.padEnd(24), t.status.padEnd(10),
        !zfs ? "no ZFS node" : b?.lastSuccessAt ? `last ${b.lastSuccessAt.toISOString()} g${b.generation}` : "never backed up",
        b?.lastVerifiedAt ? `verified ${b.lastVerifiedAt.toISOString().slice(0, 10)}` : "",
      ].join(" "));
    }
    const problems = await backups.problems();
    console.log(problems.length ? "\nNeeds attention:" : "\nNo problems.");
    for (const p of problems) console.log(`  ${p.slug}: ${p.issues.join("; ")}`);
    const dumps = settings.dbPublicKey ? backups.dbList() : [];
    console.log(settings.dbPublicKey
      ? `\ndatabase dumps: ${dumps.length}, newest ${dumps.at(-1)?.name ?? "none"}`
      : "\nBACKUP_DB_PUBLIC_KEY is not set: the control-plane database is NOT backed up");
    break;
  }
  case "list": {
    const t = await tenantBySlug(a1);
    for (const s of backups.list().get(t.slug) ?? []) {
      console.log(`${s.gen}@${s.snap}  ${s.created.toISOString()}  ${Math.round(s.usedBytes / 1048576)} MB`);
    }
    break;
  }
  case "run": {
    const listing = backups.list();
    if (a1) {
      const t = await tenantBySlug(a1);
      if (!t.node || !hasZfs(t.node)) throw new Error(`${t.slug} isn't on a ZFS node`);
      await backups.backupTenant(t, t.node, listing.get(t.slug) ?? []);
    } else {
      await backups.runAll();
    }
    break;
  }
  case "verify":
    await backups.verifyAll();
    break;
  case "restore": {
    const t = await tenantBySlug(a1);
    const snaps = backups.list().get(t.slug) ?? [];
    const pick = a2 ? snaps.find((s) => `${s.gen}@${s.snap}` === a2) : snaps.at(-1);
    if (!pick) throw new Error(a2 ? `no backup ${a2} of ${t.slug} (see: list ${t.slug})` : `no backups of ${t.slug}`);
    const busy = await prisma.job.count({ where: { tenantId: t.id, status: { in: ["QUEUED", "RUNNING"] } } });
    if (busy) throw new Error(`${t.slug} has a job queued or running; try again when it's done`);
    if (values.node) {
      const node = await prisma.node.findUniqueOrThrow({ where: { name: values.node } });
      if (!hasZfs(node)) throw new Error(`node ${node.name} has no ZFS pool`);
      await prisma.tenant.update({ where: { id: t.id }, data: { nodeId: node.id } });
      console.log(`${t.slug} now runs on ${node.name}${t.node ? ` (was ${t.node.name}; clean that node up by hand if it comes back)` : ""}`);
    }
    await audit("ops-cli", "backup.restore_requested", t.id, { from: `${pick.gen}@${pick.snap}`, node: values.node ?? t.node?.name });
    const job = await enqueueTenantJob(t.id, "RESTORE", { gen: pick.gen, snap: pick.snap });
    console.log(`queued RESTORE of ${t.slug} from ${pick.gen}@${pick.snap} (${pick.created.toISOString()}), job ${job.id}`);
    console.log(`the job log names the tag the current data is set aside under; remove it with: discard ${t.slug} <tag>`);
    break;
  }
  case "discard": {
    const t = await tenantBySlug(a1);
    if (!a2) throw new Error("usage: discard <slug> <tag>");
    if (!t.node) throw new Error(`${t.slug} has no node`);
    backups.discard(t, t.node, a2);
    await audit("ops-cli", "backup.set_aside_discarded", t.id, { tag: a2 });
    console.log(`removed ${t.slug}.${a2}`);
    break;
  }
  case "db-dump":
    await backups.dumpDatabase();
    break;
  case "db-list":
    for (const d of backups.dbList()) console.log(`${d.name}  ${Math.round(d.bytes / 1024)} KB`);
    break;
  case "db-get": {
    if (!a1 || !a2) throw new Error("usage: db-get <name> <out-file>");
    writeFileSync(a2, backups.dbGet(a1), { mode: 0o600 });
    console.log(`saved ${a2} (still encrypted; open it with db-open on a trusted machine)`);
    break;
  }
  default:
    throw new Error(`unknown command: ${cmd}`);
}
await prisma.$disconnect();
process.exit(0);
