// Per-tenant encrypted volumes via the pc-cryptd helper (infra/node/cryptd).
// The volume key is unwrapped here, in memory, and handed over on stdin.
import { unwrapVolumeKey } from "@pc/core";
import { CRYPTD_IMAGE, type NodeExec } from "./node.ts";

const MB_PER_UNIT = 100 * 1024; // one storage unit = 100 GB

export type VolumeStatus = { state: "missing" | "closed" } | { state: "open"; usedMb: number; sizeMb: number };

export class TenantVolume {
  constructor(
    private readonly node: NodeExec,
    private readonly tenantId: string,
    private readonly slug: string,
    private readonly wrappedKey: string,
  ) {}

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
    return this.run(["create", this.slug, String(storageUnits * MB_PER_UNIT), ...dirs], true);
  }

  grow(storageUnits: number) {
    return this.run(["grow", this.slug, String(storageUnits * MB_PER_UNIT)], true);
  }

  close() {
    return this.run(["close", this.slug], false);
  }

  /** Wipes the LUKS header and deletes the file. Irreversible. */
  destroy() {
    return this.run(["destroy", this.slug], false);
  }

  status(): VolumeStatus {
    const out = this.run(["status", this.slug], false).split(/\s+/);
    if (out[0] === "open") return { state: "open", usedMb: Number(out[1]), sizeMb: Number(out[2]) };
    return { state: out[0] === "closed" ? "closed" : "missing" };
  }
}
