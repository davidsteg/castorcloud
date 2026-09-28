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
export const roundCoord = (value: number | null | undefined) =>
  typeof value === "number" && Number.isFinite(value) ? Math.round(value * 1e4) / 1e4 : undefined;

/** Accepts "2026-05-01" or a full ISO timestamp; returns ISO or undefined. */
export function isoDate(value: string | undefined, endOfDay = false): string | undefined {
  if (!value) return undefined;
  const dateOnly = /^\d{4}-\d{2}-\d{2}$/.test(value);
  const d = new Date(dateOnly ? `${value}T${endOfDay ? "23:59:59.999" : "00:00:00.000"}Z` : value);
  if (Number.isNaN(d.getTime())) throw new ToolError(`"${value}" is not a date (use YYYY-MM-DD)`);
  return d.toISOString();
}

/** Drops undefined/null/empty values so answers stay small. */
export function compact<T extends Record<string, unknown>>(obj: T): Partial<T> {
  return Object.fromEntries(
    Object.entries(obj).filter(([, v]) => v !== undefined && v !== null && v !== "" && !(Array.isArray(v) && v.length === 0)),
  ) as Partial<T>;
}

/** An error the model should see (and can tell the user) instead of a crash. */
export class ToolError extends Error {}
