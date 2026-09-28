// Which backups to keep: pure functions, so the policy is unit-tested apart
// from ZFS. The backup box enforces its own floor on top (castor-backup:
// nothing younger than MIN_DAYS, never a tenant's newest backup).

export type BackupSnapshot = { gen: string; snap: string; created: Date };

export type RetentionPolicy = {
  /** Keep every backup this recent. */
  allHours: number;
  /** Then the newest backup of each of the last N days / ISO weeks / months that have one. */
  daily: number;
  weekly: number;
  monthly: number;
};

export const DEFAULT_RETENTION: RetentionPolicy = { allHours: 48, daily: 14, weekly: 8, monthly: 12 };

const day = (d: Date) => d.toISOString().slice(0, 10);
const month = (d: Date) => d.toISOString().slice(0, 7);
function isoWeek(d: Date) {
  const t = new Date(Date.UTC(d.getUTCFullYear(), d.getUTCMonth(), d.getUTCDate()));
  t.setUTCDate(t.getUTCDate() + 4 - (t.getUTCDay() || 7)); // Thursday of this week
  const yearStart = Date.UTC(t.getUTCFullYear(), 0, 1);
  return `${t.getUTCFullYear()}-W${Math.ceil(((t.getTime() - yearStart) / 86400000 + 1) / 7)}`;
}

/** The snapshots to delete, oldest first. The newest is always kept. */
export function toPrune(snaps: BackupSnapshot[], now: Date, policy = DEFAULT_RETENTION): BackupSnapshot[] {
  const newestFirst = [...snaps].sort((a, b) => b.created.getTime() - a.created.getTime());
  const keep = new Set<BackupSnapshot>(newestFirst.slice(0, 1));
  for (const s of newestFirst) {
    if (now.getTime() - s.created.getTime() <= policy.allHours * 3600_000) keep.add(s);
  }
  const buckets: [number, (d: Date) => string][] = [[policy.daily, day], [policy.weekly, isoWeek], [policy.monthly, month]];
  for (const [n, bucket] of buckets) {
    const seen = new Set<string>();
    for (const s of newestFirst) {
      if (seen.size >= n) break;
      const b = bucket(s.created);
      if (seen.has(b)) continue;
      seen.add(b);
      keep.add(s); // newest of its bucket
    }
  }
  return newestFirst.filter((s) => !keep.has(s)).reverse();
}

/** Backup snapshot / bookmark name for a point in time: cb-20260928T0600Z. */
export function snapshotName(at: Date) {
  return `cb-${at.toISOString().slice(0, 16).replace(/[-:]/g, "")}Z`;
}

/** Parses `castor-backup list` lines: "<slug> <gen> <snap> <created-epoch> <used-bytes>". */
export function parseBackupList(text: string): Map<string, (BackupSnapshot & { usedBytes: number })[]> {
  const out = new Map<string, (BackupSnapshot & { usedBytes: number })[]>();
  for (const line of text.split("\n")) {
    const [slug, gen, snap, created, used] = line.trim().split(/\s+/);
    if (!slug || !/^g\d+$/.test(gen ?? "") || !snap?.startsWith("cb-") || !/^\d+$/.test(created ?? "")) continue;
    const list = out.get(slug) ?? [];
    list.push({ gen: gen!, snap, created: new Date(Number(created) * 1000), usedBytes: Number(used) || 0 });
    out.set(slug, list);
  }
  return out;
}

/** Generation number of "g3" (0 when there is none). */
export const genNumber = (gen: string | undefined) => (gen ? Number(gen.slice(1)) : 0);
