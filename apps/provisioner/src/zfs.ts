// Per-tenant encrypted ZFS datasets on TrueNAS and macOS nodes, via the
// castor-zfs helper (infra/node/bin/castor-zfs) over SSH. The dataset key is derived from the
// tenant's volume key here, in memory, and handed over on stdin.
import { hkdfSync } from "node:crypto";
import { unwrapVolumeKey } from "@pc/core";
import { storageQuotaMb } from "@pc/pricing";
import { parseStatus, parseUsage, type StorageStatus, type TenantStorage } from "./storage.ts";

/** Runs a command on the node's host (not in Docker). */
export interface HostExec {
  host(args: string[], opts?: { input?: Buffer }): string;
}

/**
 * ZFS wants a 32-byte key; volume keys are 64 random bytes. HKDF keeps one key
 * per tenant in the database (deleting it still crypto-shreds the dataset).
 * Hex, with a newline, is what `keyformat=hex` reads from stdin.
 */
export function datasetKey(volumeKey: Buffer): Buffer {
  const key = Buffer.from(hkdfSync("sha256", volumeKey, Buffer.alloc(0), "castor-zfs-dataset-v1", 32));
  const hex = Buffer.from(key.toString("hex") + "\n");
  key.fill(0);
  return hex;
}

export class ZfsDataset implements TenantStorage {
  /** ZFS quotas change online, in both directions. */
  readonly offlineShrink = false;
  readonly dataDir: string;
  private readonly node: HostExec;
  private readonly tenantId: string;
  private readonly slug: string;
  private readonly wrappedKey: string;

  /** mountRoot: where castor-zfs mounts tenant datasets (see nodeLayout). */
  constructor(node: HostExec, tenantId: string, slug: string, wrappedKey: string, mountRoot: string) {
    this.node = node;
    this.tenantId = tenantId;
    this.slug = slug;
    this.wrappedKey = wrappedKey;
    // Docker sees the same path: natively on TrueNAS, and on macOS because
    // Colima mounts the pool into its VM at the same place.
    this.dataDir = `${mountRoot}/${slug}/data`;
  }

  private run(args: string[], withKey: boolean) {
    let input: Buffer | undefined;
    if (withKey) {
      const volumeKey = unwrapVolumeKey(this.tenantId, this.wrappedKey);
      input = datasetKey(volumeKey);
      volumeKey.fill(0);
    }
    try {
      return this.node.host(["castor-zfs", ...args], { input }).trim();
    } finally {
      input?.fill(0);
    }
  }

  ensureOpen(storageUnits: number, dirs: string[]) {
    return this.run(["create", this.slug, String(storageQuotaMb(storageUnits)), ...dirs], true);
  }

  grow(storageUnits: number) {
    return this.run(["quota", this.slug, String(storageQuotaMb(storageUnits))], false);
  }

  shrink(storageUnits: number) {
    return this.run(["quota", this.slug, String(storageQuotaMb(storageUnits))], false);
  }

  close() {
    return this.run(["close", this.slug], false);
  }

  /** Destroys the dataset and its snapshots. Irreversible. */
  destroy() {
    return this.run(["destroy", this.slug], false);
  }

  status(): StorageStatus {
    return parseStatus(this.run(["status", this.slug], false));
  }

  usage(): Record<string, number> {
    return parseUsage(this.run(["usage", this.slug], false));
  }
}
