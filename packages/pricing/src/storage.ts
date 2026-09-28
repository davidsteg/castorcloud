// Storage quotas. The booked storage units are a hard limit: each tenant's
// encrypted volume is exactly that size, so a full volume is a full quota.
// Everyone (provisioner, portal, pricing page) derives sizes from here.
import { MAX_STORAGE_UNITS, STORAGE_UNIT_GB } from "./catalog.ts";

export const MB_PER_GB = 1024;

/** Share of the quota at which the portal warns, and at which it says "almost full". */
export const STORAGE_WARN_AT = 0.8;
export const STORAGE_FULL_AT = 0.95;

/**
 * Free space a smaller booking must leave after a downgrade, so the stack
 * keeps working right after the volume shrinks.
 */
export const DOWNGRADE_HEADROOM = 0.1;

export type StorageLevel = "ok" | "warn" | "full";

/** Size of the encrypted volume for a booking, in MB. */
export function storageQuotaMb(storageUnits: number): number {
  return storageUnits * STORAGE_UNIT_GB * MB_PER_GB;
}

export function storageLevel(usedMb: number, quotaMb: number): StorageLevel {
  if (quotaMb <= 0) return "full";
  const share = usedMb / quotaMb;
  return share >= STORAGE_FULL_AT ? "full" : share >= STORAGE_WARN_AT ? "warn" : "ok";
}

/** Smallest booking that holds `usedMb` with the downgrade headroom to spare. */
export function minStorageUnits(usedMb: number): number {
  const units = Math.ceil((usedMb * (1 + DOWNGRADE_HEADROOM)) / storageQuotaMb(1));
  return Math.min(MAX_STORAGE_UNITS, Math.max(1, units));
}

/** Whether a booking of `storageUnits` can hold `usedMb` (with headroom). */
export function fitsStorage(usedMb: number, storageUnits: number): boolean {
  return usedMb * (1 + DOWNGRADE_HEADROOM) <= storageQuotaMb(storageUnits);
}

/** "12.3 GB" / "850 MB" / "1.2 TB" */
export function formatStorage(mb: number): string {
  if (mb < MB_PER_GB) return `${Math.round(mb)} MB`;
  const gb = mb / MB_PER_GB;
  if (gb < 1000) return `${gb < 10 ? gb.toFixed(1) : Math.round(gb)} GB`;
  return `${(gb / 1000).toFixed(1)} TB`;
}
