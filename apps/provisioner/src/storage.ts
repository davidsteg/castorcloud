// A tenant's encrypted storage on its node. Two implementations:
//   TenantVolume (volume.ts) - LUKS file per tenant, Linux nodes and local dev
//   ZfsDataset   (zfs.ts)    - encrypted ZFS dataset per tenant, macOS nodes
// The size is the tenant's hard storage quota (see @pc/pricing storage.ts).

export type StorageStatus =
  | { state: "missing" | "closed" }
  /** usedMb/sizeMb: what the tenant sees; volumeMb: the enforced limit. */
  | { state: "open"; usedMb: number; sizeMb: number; volumeMb: number };

export interface TenantStorage {
  /** Where the data directories live, as seen by the node's Docker engine. */
  readonly dataDir: string;
  /** True when shrinking needs every container using the storage stopped. */
  readonly offlineShrink: boolean;
  /** Creates the storage if needed, unlocks and mounts it, creates the data directories. */
  ensureOpen(storageUnits: number, dirs: string[]): string;
  grow(storageUnits: number): string;
  /** The caller checks the data fits first (fitsStorage). */
  shrink(storageUnits: number): string;
  /** Unmounts and locks (suspend). */
  close(): string;
  /** Irreversible. The caller then deletes the key (crypto-shredding). */
  destroy(): string;
  status(): StorageStatus;
  /** Used MB per data directory (compose volume name), empty when closed. */
  usage(): Record<string, number>;
}

/** Parses "<dir> <mb>" lines printed by the node helpers' usage command. */
export function parseUsage(text: string): Record<string, number> {
  const out: Record<string, number> = {};
  for (const line of text.split("\n")) {
    const [dir, mb] = line.trim().split(/\s+/);
    if (dir && mb && /^\d+$/.test(mb)) out[dir] = Number(mb);
  }
  return out;
}

/** Parses "open <used> <size> <volume>" / "closed" / "missing". */
export function parseStatus(text: string): StorageStatus {
  const out = text.trim().split(/\s+/);
  if (out[0] === "open") {
    return { state: "open", usedMb: Number(out[1]), sizeMb: Number(out[2]), volumeMb: Number(out[3]) };
  }
  return { state: out[0] === "closed" ? "closed" : "missing" };
}
