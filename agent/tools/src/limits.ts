// Minimal disclosure: tools answer with a few short, structured facts and a
// link back into the portal, never whole files or long lists. Everything a
// tool returns is sent to the LLM provider.

export const MAX_ITEMS = 25;
export const EXCERPT_CHARS = 280;
export const DOCUMENT_CHARS = 6000;

export function clampLimit(limit: number | undefined, fallback = 10) {
  return Math.max(1, Math.min(MAX_ITEMS, Math.trunc(limit ?? fallback)));
}

/** Plain text, whitespace collapsed, cut at a word boundary. */
export function excerpt(text: string | null | undefined, max = EXCERPT_CHARS): string | undefined {
  if (!text) return undefined;
  const plain = text.replace(/<[^>]*>/g, "").replace(/\s+/g, " ").trim();
  if (plain.length <= max) return plain || undefined;
  const cut = plain.slice(0, max);
  const space = cut.lastIndexOf(" ");
  return `${cut.slice(0, space > max * 0.6 ? space : max)}…`;
}

/** ~11 m precision: enough to name a place, not to pinpoint a doorstep. */
export function roundCoord(value: number | string | null | undefined) {
  if (value === null || value === undefined || value === "") return undefined;
  const n = Number(value);
  return Number.isFinite(n) ? Math.round(n * 1e4) / 1e4 : undefined;
}

/** The tenant's time zone: dates the user types and times we answer are local. */
export const TIME_ZONE = process.env.TZ || "Europe/Zurich";

/** Minutes the zone is ahead of UTC at `date`. */
function offsetMinutes(date: Date, timeZone: string) {
  const parts = Object.fromEntries(
    new Intl.DateTimeFormat("en-US", {
      timeZone, hourCycle: "h23", year: "numeric", month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit", second: "2-digit",
    }).formatToParts(date).map((p) => [p.type, p.value]),
  );
  const asUtc = Date.UTC(+parts.year, +parts.month - 1, +parts.day, +parts.hour, +parts.minute, +parts.second);
  return Math.round((asUtc - date.getTime()) / 60_000);
}

/**
 * Accepts "2026-05-01" (a local day: start, or end with `endOfDay`) or a full
 * ISO timestamp; returns an ISO instant or undefined.
 */
export function isoDate(value: string | undefined, endOfDay = false, timeZone = TIME_ZONE): string | undefined {
  if (!value) return undefined;
  if (/^\d{4}-\d{2}-\d{2}$/.test(value)) {
    const wall = new Date(`${value}T${endOfDay ? "23:59:59.999" : "00:00:00.000"}Z`);
    if (Number.isNaN(wall.getTime())) throw new ToolError(`"${value}" is not a date (use YYYY-MM-DD)`);
    return new Date(wall.getTime() - offsetMinutes(wall, timeZone) * 60_000).toISOString();
  }
  const d = new Date(value);
  if (Number.isNaN(d.getTime())) throw new ToolError(`"${value}" is not a date (use YYYY-MM-DD)`);
  return d.toISOString();
}

/** "2026-05-01T10:00" in the tenant's zone, for answers. */
export function localTime(date: Date | string | number, timeZone = TIME_ZONE): string {
  const d = typeof date === "number" ? new Date(date * 1000) : new Date(date);
  return new Intl.DateTimeFormat("sv-SE", {
    timeZone, year: "numeric", month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit",
  }).format(d).replace(" ", "T");
}

/** The local calendar day of an instant, "2026-05-01". */
export const localDay = (iso: string, timeZone = TIME_ZONE) => localTime(iso, timeZone).slice(0, 10);

/** Drops undefined/null/empty values so answers stay small. */
export function compact<T extends Record<string, unknown>>(obj: T): Partial<T> {
  return Object.fromEntries(
    Object.entries(obj).filter(([, v]) => v !== undefined && v !== null && v !== "" && !(Array.isArray(v) && v.length === 0)),
  ) as Partial<T>;
}

/** An error the model should see (and can tell the user) instead of a crash. */
export class ToolError extends Error {}
