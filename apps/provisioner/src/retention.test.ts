import assert from "node:assert/strict";
import { test } from "node:test";
import { genNumber, parseBackupList, snapshotName, toPrune, type BackupSnapshot } from "./retention.ts";

const now = new Date("2026-09-28T12:00:00Z");
/** One backup every 6 hours for `days` days before now. */
function every6h(days: number): BackupSnapshot[] {
  const out: BackupSnapshot[] = [];
  for (let h = 0; h < days * 24; h += 6) {
    const created = new Date(now.getTime() - h * 3600_000);
    out.push({ gen: "g1", snap: snapshotName(created), created });
  }
  return out;
}

test("keeps everything from the last 48 hours", () => {
  const snaps = every6h(2);
  assert.deepEqual(toPrune(snaps, now), []);
});

test("thins older backups to daily, weekly and monthly", () => {
  const snaps = every6h(400);
  const pruned = new Set(toPrune(snaps, now));
  const kept = snaps.filter((s) => !pruned.has(s));
  // 9 within 48h (0..48h step 6), plus one per day for the 14 days, weeks and months beyond.
  assert.ok(kept.length >= 9 + 12 && kept.length <= 9 + 14 + 8 + 12, `kept ${kept.length}`);
  assert.ok(kept.includes(snaps[0]!), "newest kept");
  const oldest = kept.at(-1)!;
  assert.ok(now.getTime() - oldest.created.getTime() > 300 * 86400_000, "a monthly from about a year ago survives");
  // Pruned list is oldest first.
  const p = toPrune(snaps, now);
  for (let i = 1; i < p.length; i++) assert.ok(p[i]!.created >= p[i - 1]!.created);
});

test("always keeps the newest, however old", () => {
  const created = new Date("2024-01-01T00:00:00Z");
  const snaps = [{ gen: "g1", snap: snapshotName(created), created }];
  assert.deepEqual(toPrune(snaps, now), []);
});

test("keeps the newest per day across generations", () => {
  const a = { gen: "g1", snap: "cb-20260920T0000Z", created: new Date("2026-09-20T00:00:00Z") };
  const b = { gen: "g2", snap: "cb-20260920T1800Z", created: new Date("2026-09-20T18:00:00Z") };
  const pruned = toPrune([a, b], now, { allHours: 0, daily: 5, weekly: 0, monthly: 0 });
  assert.deepEqual(pruned, [a]);
});

test("snapshot names are sortable UTC minutes", () => {
  assert.equal(snapshotName(new Date("2026-09-28T06:05:59Z")), "cb-20260928T0605Z");
});

test("parses the backup box's list and skips junk", () => {
  const m = parseBackupList("alpha g1 cb-20260928T0600Z 1790000000 4096\nalpha g2 cb-20260928T1200Z 1790021600 0\nbad line\nbeta gx cb-1 1 1\n");
  assert.equal(m.get("alpha")?.length, 2);
  assert.equal(m.get("alpha")?.[1]?.gen, "g2");
  assert.equal(m.has("beta"), false);
  assert.equal(genNumber("g12"), 12);
  assert.equal(genNumber(undefined), 0);
});
