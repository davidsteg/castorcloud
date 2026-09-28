// Per-tenant encrypted volumes via the pc-cryptd helper (infra/node/cryptd).
// The volume key is unwrapped here, in memory, and handed over on stdin.
// The volume's size is the tenant's storage quota (see @pc/pricing storage.ts).
import { unwrapVolumeKey } from "@pc/core";
import { storageQuotaMb } from "@pc/pricing";
import { CRYPTD_IMAGE, type NodeExec } from "./node.ts";
import { parseStatus, parseUsage, type StorageStatus, type TenantStorage } from "./storage.ts";

/** @deprecated use StorageStatus */
export type VolumeStatus = StorageStatus;

export class TenantVolume implements TenantStorage {
  /** ext4 can't shrink while mounted. */
  readonly offlineShrink = true;
  readonly dataDir: string;

  constructor(
    private readonly node: NodeExec,
    private readonly tenantId: string,
    private readonly slug: string,
    private readonly wrappedKey: string,
  ) {
    this.dataDir = `/srv/pc/tenants/${slug}/data`;
  }

  private run(args: string[], withKey: boolean) {
    const input = withKey ? unwrapVolumeKey(this.tenantId, this.wrappedKey) : undefined;
    try {
      return this.node
        .docker(["run", "--rm", "-i", "--privileged", "-v", "/srv/pc:/srv/pc:rshared", CRYPTD_IMAGE, ...args], { input })
        .trim();
    } finally {
      input?.fill(0); // don't leave the key in memory longer than needed
    }
  }

  /** Creates the volume if needed, opens it and creates the data directories. */
  ensureOpen(storageUnits: number, dirs: string[]) {
    return this.run(["create", this.slug, String(storageQuotaMb(storageUnits)), ...dirs.map((d) => `data/${d}`)], true);
  }

  grow(storageUnits: number) {
    return this.run(["grow", this.slug, String(storageQuotaMb(storageUnits))], true);
  }

  /** Offline: every container using the volume must be stopped first. */
  shrink(storageUnits: number) {
    return this.run(["shrink", this.slug, String(storageQuotaMb(storageUnits))], true);
  }

  close() {
    return this.run(["close", this.slug], false);
  }

  /** Wipes the LUKS header and deletes the file. Irreversible. */
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
