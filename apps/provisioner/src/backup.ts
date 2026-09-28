// Tenant backups: raw ZFS sends from each node to the backup box.
//
//   node (castor-snap)  ──ssh──►  control plane  ──ssh──►  backup box (castor-backup)
//   zfs send -w (ciphertext)       pipes the stream          zfs receive -u, no key
//
// The node and the box never trust each other; the control plane holds the
// only key for each side. Streams are raw: the box stores tenant data still
// encrypted with the tenant's key, which never leaves the control plane.
// See docs/backups.md.
import { execFileSync, spawn } from "node:child_process";
import { gzipSync } from "node:zlib";
import { prisma, type Node, type Tenant } from "@pc/db";
import { audit } from "@pc/core";
import { storageQuotaMb } from "@pc/pricing";
import { sealDump } from "./db-dump.ts";
import { DEFAULT_RETENTION, genNumber, parseBackupList, snapshotName, toPrune, type BackupSnapshot } from "./retention.ts";

const SSH_TARGET = /^[A-Za-z0-9._-]+@[A-Za-z0-9.-]+$/;
const SSH_WORD = /^[A-Za-z0-9._/=-]+$/;

function sshArgs(target: string, args: string[]) {
  if (!SSH_TARGET.test(target)) throw new Error(`invalid ssh target: ${target}`);
  for (const a of args) if (!SSH_WORD.test(a)) throw new Error(`argument not allowed over ssh: ${a}`);
  return ["-o", "BatchMode=yes", "-T", target, ...args];
}

function ssh(target: string, args: string[], input?: Buffer) {
  try {
    return execFileSync("ssh", sshArgs(target, args), {
      input, stdio: ["pipe", "pipe", "pipe"], encoding: "utf8", maxBuffer: 32 * 1024 * 1024,
    });
  } catch (err) {
    const e = err as { stderr?: string; message?: string };
    throw new Error(`${target} ${args.slice(0, 3).join(" ")} failed: ${(e.stderr || e.message || "").trim().slice(-1000)}`);
  }
}

/**
 * Pipes one ssh command's stdout into another's stdin, without blocking the
 * worker (a first full send can take hours). Resolves with the bytes moved.
 */
function pipeSsh(from: [string, string[]], to: [string, string[]]): Promise<number> {
  return new Promise((resolve, reject) => {
    const src = spawn("ssh", sshArgs(...from), { stdio: ["ignore", "pipe", "pipe"] });
    const dst = spawn("ssh", sshArgs(...to), { stdio: ["pipe", "pipe", "pipe"] });
    let bytes = 0;
    const errs: string[] = [];
    const tail = (label: string) => (d: Buffer) => errs.push(`${label}: ${d.toString().trim()}`.slice(-500));
    src.stderr.on("data", tail(from[1][0]!));
    dst.stderr.on("data", tail(to[1][0]!));
    src.stdout.on("data", (d: Buffer) => (bytes += d.length));
    src.stdout.pipe(dst.stdin);
    dst.stdin.on("error", () => {}); // EPIPE when the receiver dies; its exit code says why
    let pending = 2;
    let failed = false;
    const done = (code: number | null, which: string) => {
      if (code !== 0 && !failed) {
        failed = true;
        // A half-sent stream is rejected by zfs receive; stop the other side too.
        src.kill();
        dst.kill();
        setImmediate(() => reject(new Error(`${which} exited with ${code}: ${errs.join(" | ").slice(-1500)}`)));
      }
      if (--pending === 0 && !failed) resolve(bytes);
    };
    src.on("close", (c) => done(c, "send"));
    dst.on("close", (c) => done(c, "receive"));
    src.on("error", reject);
    dst.on("error", reject);
  });
}

export type BackupSettings = {
  /** user@host of the backup box (its key runs castor-backup as a forced command). */
  target: string;
  intervalHours: number;
  /** X25519 public key for control-plane database dumps (db-dump.ts). */
  dbPublicKey?: string;
  /** The control-plane Postgres container, for pg_dumpall. */
  pgContainer: string;
  pgUser: string;
};

export function backupSettings(env = process.env): BackupSettings | undefined {
  if (!env.BACKUP_TARGET) return undefined;
  return {
    target: env.BACKUP_TARGET,
    intervalHours: Number(env.BACKUP_INTERVAL_HOURS ?? "6"),
    dbPublicKey: env.BACKUP_DB_PUBLIC_KEY || undefined,
    pgContainer: env.BACKUP_PG_CONTAINER ?? "pc-control-plane-postgres-1",
    pgUser: env.POSTGRES_USER ?? "pc",
  };
}

/** Nodes whose tenant data is on ZFS (the local development node uses LUKS files). */
export const hasZfs = (node: Pick<Node, "kind" | "zfsPool">) => node.kind !== "LOCAL" && !!node.zfsPool;
const nodeTarget = (node: Pick<Node, "sshUser" | "sshHost">) => `${node.sshUser}@${node.sshHost}`;
const stamp = (d = new Date()) => d.toISOString().slice(0, 16).replace(/[-:]/g, "").toLowerCase();

type Log = (line: string) => Promise<void> | void;

export class Backups {
  constructor(readonly s: BackupSettings) {}

  private box(args: string[], input?: Buffer) {
    return ssh(this.s.target, ["castor-backup", ...args], input);
  }

  /** Every backup on the box, per tenant slug, oldest first. */
  list() {
    return parseBackupList(this.box(["list"]));
  }

  pool() {
    const [word, size, alloc, health] = this.box(["pool"]).trim().split(/\s+/);
    if (word !== "pool") throw new Error("unexpected output from the backup box");
    return { sizeMb: Number(size), usedMb: Number(alloc), health: health ?? "UNKNOWN" };
  }

  /** One backup of every active tenant on a ZFS node, then retention. */
  async runAll(log: Log = console.log) {
    const pool = this.pool();
    if (pool.health !== "ONLINE") {
      await audit("system", "backup.box_unhealthy", undefined, { health: pool.health });
      throw new Error(`backup box pool is ${pool.health}; not sending`);
    }
    const listing = this.list();
    const tenants = await prisma.tenant.findMany({
      where: { status: "ACTIVE", node: { isNot: null } },
      include: { node: true, backup: true },
      orderBy: { slug: "asc" },
    });
    let ok = 0;
    for (const t of tenants) {
      if (!t.node || !hasZfs(t.node)) continue;
      // Not while a lifecycle job (restore, delete, update) is changing the dataset.
      const busy = await prisma.job.count({ where: { tenantId: t.id, status: { in: ["QUEUED", "RUNNING"] } } });
      if (busy) {
        await log(`${t.slug}: busy, next run`);
        continue;
      }
      if (await this.backupTenant(t, t.node, listing.get(t.slug) ?? [], log)) ok++;
    }
    await log(`backed up ${ok} tenant(s)`);
    await this.prune(log);
  }

  /** Snapshot, send (incremental when the chain is intact), bookmark. Records the outcome. */
  async backupTenant(
    t: Tenant & { backup: { generation: number; lastSnapshot: string | null; failures: number } | null },
    node: Node,
    onBox: BackupSnapshot[],
    log: Log = console.log,
  ): Promise<boolean> {
    const at = new Date();
    const snap = snapshotName(at);
    const n = nodeTarget(node);
    const state = t.backup ?? { generation: 0, lastSnapshot: null, failures: 0 };
    await prisma.tenantBackup.upsert({ where: { tenantId: t.id }, create: { tenantId: t.id, lastAttemptAt: at }, update: { lastAttemptAt: at } });
    try {
      ssh(n, ["castor-snap", "snapshot", t.slug, snap]);
      const bookmarks = ssh(n, ["castor-snap", "bookmarks", t.slug]).split("\n").map((l) => l.trim()).filter(Boolean);
      const current = `g${state.generation}`;
      const base = state.lastSnapshot;
      const incremental = state.generation > 0 && !!base && bookmarks.includes(base)
        && onBox.some((s) => s.gen === current && s.snap === base);
      const gen = incremental
        ? state.generation
        : Math.max(state.generation, ...onBox.map((s) => genNumber(s.gen))) + 1;
      const bytes = await pipeSsh(
        [n, ["castor-snap", "send", t.slug, snap, ...(incremental ? [base!] : [])]],
        [this.s.target, ["castor-backup", "recv", t.slug, `g${gen}`]],
      );
      ssh(n, ["castor-snap", "sent", t.slug, snap]);
      await prisma.tenantBackup.update({
        where: { tenantId: t.id },
        data: { generation: gen, lastSnapshot: snap, lastBytes: BigInt(bytes), lastSuccessAt: new Date(), lastError: null, failures: 0 },
      });
      if (state.failures > 0) await audit("system", "backup.recovered", t.id, { snap });
      await log(`${t.slug}: ${incremental ? "incremental" : "full"} g${gen}@${snap}, ${Math.round(bytes / 1048576)} MB`);
      return true;
    } catch (err) {
      const message = (err as Error).message.slice(0, 2000);
      try {
        ssh(n, ["castor-snap", "drop", t.slug, snap]); // don't let failed snapshots eat the quota
      } catch {
        /* the node may be down; `sent` clears leftovers next time */
      }
      await prisma.tenantBackup.update({ where: { tenantId: t.id }, data: { lastError: message, failures: { increment: 1 } } });
      if (state.failures === 0) await audit("system", "backup.failed", t.id, { error: message.slice(0, 500) });
      await log(`${t.slug}: backup failed: ${message}`);
      return false;
    }
  }

  /**
   * Applies the retention policy, and forgets tenants that were deleted (their
   * key is gone, so what's left is unreadable). The box refuses anything
   * younger than its own floor; those refusals are expected and skipped.
   */
  async prune(log: Log = console.log) {
    const listing = this.list();
    const tenants = await prisma.tenant.findMany({ where: { slug: { in: [...listing.keys()] } }, select: { slug: true, status: true } });
    const status = new Map(tenants.map((t) => [t.slug, t.status]));
    const now = new Date();
    let pruned = 0;
    for (const [slug, snaps] of listing) {
      const st = status.get(slug);
      if (st === undefined || st === "DELETED") {
        try {
          this.box(["forget", slug]);
          await log(`${slug}: deleted tenant, backups forgotten`);
        } catch (err) {
          if (!/refused/.test((err as Error).message)) await log(`${slug}: forget failed: ${(err as Error).message}`);
        }
        continue;
      }
      for (const s of toPrune(snaps, now, DEFAULT_RETENTION)) {
        try {
          this.box(["prune", slug, s.gen, s.snap]);
          pruned++;
        } catch (err) {
          if (!/refused/.test((err as Error).message)) await log(`${slug}: prune ${s.gen}@${s.snap} failed: ${(err as Error).message}`);
        }
      }
    }
    if (pruned) await log(`pruned ${pruned} old backup(s)`);
  }

  /** Reads each tenant's newest backup back on the box; ZFS checks every block. */
  async verifyAll(log: Log = console.log) {
    const listing = this.list();
    const tenants = await prisma.tenant.findMany({ where: { slug: { in: [...listing.keys()] }, status: { not: "DELETED" } } });
    for (const t of tenants) {
      const newest = listing.get(t.slug)!.at(-1)!;
      try {
        this.box(["verify", t.slug, newest.gen, newest.snap]);
        await prisma.tenantBackup.upsert({
          where: { tenantId: t.id },
          create: { tenantId: t.id, lastVerifiedAt: new Date() },
          update: { lastVerifiedAt: new Date(), verifyError: null },
        });
      } catch (err) {
        const message = (err as Error).message.slice(0, 2000);
        await prisma.tenantBackup.upsert({ where: { tenantId: t.id }, create: { tenantId: t.id, verifyError: message }, update: { verifyError: message } });
        await audit("system", "backup.verify_failed", t.id, { snap: `${newest.gen}@${newest.snap}`, error: message.slice(0, 500) });
        await log(`${t.slug}: verify failed: ${message}`);
      }
    }
  }

  /**
   * Tenants whose backups need attention: failing, overdue (no success for
   * two intervals), or failing verification. For `npm run backup -- status`
   * and monitoring.
   */
  async problems(now = new Date()) {
    const due = new Date(now.getTime() - 2 * this.s.intervalHours * 3600_000 - 30 * 60_000);
    const tenants = await prisma.tenant.findMany({
      where: { status: "ACTIVE", node: { isNot: null } },
      include: { node: true, backup: true },
      orderBy: { slug: "asc" },
    });
    return tenants
      .filter((t) => t.node && hasZfs(t.node) && t.createdAt < due)
      .flatMap((t) => {
        const b = t.backup;
        const out: string[] = [];
        if (!b?.lastSuccessAt || b.lastSuccessAt < due) out.push(`no backup since ${b?.lastSuccessAt?.toISOString() ?? "ever"}`);
        if (b?.lastError) out.push(`last run failed (${b.failures}x): ${b.lastError.slice(0, 200)}`);
        if (b?.verifyError) out.push(`verify failed: ${b.verifyError.slice(0, 200)}`);
        return out.length ? [{ slug: t.slug, issues: out }] : [];
      });
  }

  /**
   * Puts a backup back as the tenant's dataset. The stack must be stopped and
   * the dataset closed (Lifecycle.stop). The current dataset is set aside,
   * not destroyed; if receiving fails it's put back. Returns the tag it was
   * set aside under.
   */
  async restore(tenant: Tenant, node: Node, gen: string, snap: string, log: Log = console.log) {
    if (!hasZfs(node)) throw new Error(`node ${node.name} has no ZFS datasets`);
    const n = nodeTarget(node);
    const tag = `before-restore-${stamp()}`;
    const aside = ssh(n, ["castor-snap", "set-aside", tenant.slug, tag]).trim();
    const hadData = aside !== "missing";
    await log(hadData ? `current data set aside as ${tenant.slug}.${tag}` : "no data on the node, restoring into an empty slot");
    const quotaMb = tenant.storageVolumeMb ?? storageQuotaMb(tenant.storageUnits);
    try {
      const bytes = await pipeSsh(
        [this.s.target, ["castor-backup", "send", tenant.slug, gen, snap]],
        [n, ["castor-snap", "receive", tenant.slug, String(quotaMb)]],
      );
      await log(`received ${gen}@${snap}, ${Math.round(bytes / 1048576)} MB`);
    } catch (err) {
      if (hadData) ssh(n, ["castor-snap", "put-back", tenant.slug, tag]);
      throw err;
    }
    // The node has no bookmark for the restored data: the next backup starts a new chain.
    await prisma.tenantBackup.upsert({
      where: { tenantId: tenant.id },
      create: { tenantId: tenant.id },
      update: { lastSnapshot: null },
    });
    await audit("system", "backup.restored", tenant.id, { node: node.name, from: `${gen}@${snap}`, setAside: hadData ? tag : null });
    return hadData ? tag : null;
  }

  /** Removes data set aside by a restore, once the restored stack is confirmed good. */
  discard(tenant: Tenant, node: Node, tag: string) {
    ssh(nodeTarget(node), ["castor-snap", "discard", tenant.slug, tag]);
  }

  /**
   * pg_dumpall of the control plane (portal and Authentik, with the wrapped
   * tenant keys), gzipped and sealed to the offline key, stored on the box.
   */
  async dumpDatabase(log: Log = console.log) {
    if (!this.s.dbPublicKey) throw new Error("BACKUP_DB_PUBLIC_KEY is not set (npm run backup -- db-keygen)");
    const sql = await new Promise<Buffer>((resolve, reject) => {
      const p = spawn("docker", ["exec", this.s.pgContainer, "pg_dumpall", "-U", this.s.pgUser, "--clean", "--if-exists"], { stdio: ["ignore", "pipe", "pipe"] });
      const out: Buffer[] = [];
      let err = "";
      p.stdout.on("data", (d: Buffer) => out.push(d));
      p.stderr.on("data", (d: Buffer) => (err += d.toString()));
      p.on("error", reject);
      p.on("close", (c) => (c === 0 ? resolve(Buffer.concat(out)) : reject(new Error(`pg_dumpall failed: ${err.trim().slice(-1000)}`))));
    });
    const plain = gzipSync(sql);
    sql.fill(0);
    const sealed = sealDump(plain, this.s.dbPublicKey);
    plain.fill(0);
    const name = `cp-${stamp()}.sql.gz.enc`;
    this.box(["db-put", name], sealed);
    await log(`database dump ${name}, ${Math.round(sealed.length / 1024)} KB`);
    return name;
  }

  dbList() {
    return this.box(["db-list"]).split("\n").filter(Boolean).map((l) => {
      const [name, bytes] = l.split(" ");
      return { name: name!, bytes: Number(bytes) };
    });
  }

  dbGet(name: string): Buffer {
    return execFileSync("ssh", sshArgs(this.s.target, ["castor-backup", "db-get", name]), { maxBuffer: 1024 * 1024 * 1024 });
  }
}
