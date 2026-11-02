// The staff console's read-only view of AuditLog (apps/portal admin/audit):
// the filters it accepts from the URL, checked here, and how a row's details
// are shown. Nothing here writes.

/** Rows per page. */
export const AUDIT_PAGE = 100;

export type AuditFilter = {
  /** An action ("staff.grant"), or every action under a prefix ("staff."). */
  action?: { value: string; prefix: boolean };
  /** A tenant's slug. */
  slug?: string;
  /** An actor: a user id, "ops-cli", "ops-cli:<name>", "system", "stripe". */
  actor?: string;
  /** Only rows older than this (the next page). */
  before?: Date;
};

const ACTION_RE = /^[a-z][a-z0-9_]*(\.[a-z0-9_]+)*\.?$/;
const SLUG_RE = /^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/;
const ACTOR_RE = /^[A-Za-z0-9._@:-]{1,80}$/;

function one(v: string | string[] | undefined): string | undefined {
  const s = Array.isArray(v) ? v[0] : v;
  const t = s?.trim();
  return t ? t : undefined;
}

/** The URL's search parameters as a filter; anything malformed is dropped. */
export function auditFilter(params: Record<string, string | string[] | undefined>): AuditFilter {
  const f: AuditFilter = {};
  const action = one(params.action)?.toLowerCase();
  if (action && action.length <= 64 && ACTION_RE.test(action)) {
    f.action = action.endsWith(".") ? { value: action, prefix: true } : { value: action, prefix: false };
  }
  const slug = one(params.slug)?.toLowerCase();
  if (slug && SLUG_RE.test(slug)) f.slug = slug;
  const actor = one(params.actor);
  if (actor && ACTOR_RE.test(actor)) f.actor = actor;
  const before = one(params.before);
  if (before && /^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d{1,3})?Z$/.test(before)) {
    const d = new Date(before);
    if (!Number.isNaN(d.getTime())) f.before = d;
  }
  return f;
}

/** The filter back as a query string, with `before` for the next page. */
export function auditQuery(f: AuditFilter, before?: Date): string {
  const q = new URLSearchParams();
  if (f.action) q.set("action", f.action.value);
  if (f.slug) q.set("slug", f.slug);
  if (f.actor) q.set("actor", f.actor);
  if (before) q.set("before", before.toISOString());
  const s = q.toString();
  return s ? `?${s}` : "";
}

/** A row's details on one line, cut at `max` characters. */
export function auditDetails(details: unknown, max = 400): string {
  if (details === null || details === undefined) return "";
  const s = JSON.stringify(details) ?? "";
  return s.length > max ? `${s.slice(0, max - 1)}…` : s;
}
