// Recurring worker tasks (maintenance, backups), kept in Postgres so each
// one runs once per slot however many workers are up: a worker runs a task
// only after moving its nextRunAt forward in one UPDATE, and only one
// worker's UPDATE matches.
import { Prisma, prisma } from "@pc/db";

export type Every =
  | { everyMs: number }
  /** Local time of day in `tz`; on `weekday` only (0 = Sunday) when set. */
  | { at: string; tz: string; weekday?: number };

/** When a task runs next, strictly after `after`. */
export function nextRun(when: Every, after: Date): Date {
  if ("everyMs" in when) return new Date(after.getTime() + when.everyMs);
  const m = /^(\d{2}):(\d{2})$/.exec(when.at);
  if (!m) throw new Error(`bad time of day: ${when.at}`);
  const [hh, mm] = [Number(m[1]), Number(m[2])];
  const day = localDate(after, when.tz);
  // Today or one of the next 7 days (a weekly task); 8 covers DST shifts.
  for (let i = 0; i <= 8; i++) {
    const d = new Date(Date.UTC(day.y, day.m - 1, day.d + i));
    if (when.weekday !== undefined && d.getUTCDay() !== when.weekday) continue;
    const t = zonedToUtc(d.getUTCFullYear(), d.getUTCMonth() + 1, d.getUTCDate(), hh, mm, when.tz);
    if (t > after) return t;
  }
  throw new Error("no next run found");
}

function parts(date: Date, tz: string) {
  const f = new Intl.DateTimeFormat("en-US", {
    timeZone: tz, hourCycle: "h23", year: "numeric", month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit", second: "2-digit",
  });
  const p = Object.fromEntries(f.formatToParts(date).map((x) => [x.type, x.value]));
  return { y: Number(p.year), m: Number(p.month), d: Number(p.day), h: Number(p.hour), min: Number(p.minute), s: Number(p.second) };
}

function localDate(date: Date, tz: string) {
  const { y, m, d } = parts(date, tz);
  return { y, m, d };
}

/** The instant when the wall clock in `tz` shows this local time. */
function zonedToUtc(y: number, m: number, d: number, h: number, min: number, tz: string): Date {
  const want = Date.UTC(y, m - 1, d, h, min);
  let t = want;
  // Two corrections settle the offset, also across a DST change.
  for (let i = 0; i < 2; i++) {
    const p = parts(new Date(t), tz);
    t += want - Date.UTC(p.y, p.m - 1, p.d, p.h, p.min, p.s);
  }
  return new Date(t);
}

// Columns are UTC timestamps without a zone; convert explicitly so the
// session's time zone never matters.
const NOW = Prisma.sql`(now() AT TIME ZONE 'UTC')`;
const utc = (d: Date) => Prisma.sql`(${d.toISOString()}::timestamptz AT TIME ZONE 'UTC')`;

/**
 * True when `name` is due and this worker won its slot (it then runs it).
 * A new task runs first at its next slot, or right away for `{ everyMs }`.
 */
export async function claimScheduled(name: string, when: Every, now = new Date()): Promise<boolean> {
  const first = "everyMs" in when ? now : nextRun(when, now);
  await prisma.$executeRaw`
    INSERT INTO "Schedule" (name, "nextRunAt") VALUES (${name}, ${utc(first)}) ON CONFLICT (name) DO NOTHING`;
  const n = await prisma.$executeRaw`
    UPDATE "Schedule" SET "nextRunAt" = ${utc(nextRun(when, now))}, "lastRunAt" = ${NOW}
    WHERE name = ${name} AND "nextRunAt" <= ${NOW}`;
  return n === 1;
}
