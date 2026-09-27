// Tenant addresses: <slug>.<BASE_DOMAIN>
export const SLUG_RE = /^[a-z0-9](?:[a-z0-9-]{1,28}[a-z0-9])$/;

/** Hosts we use ourselves, or that could be used for phishing. */
const RESERVED = new Set([
  "app", "auth", "www", "api", "admin", "mail", "smtp", "imap", "status", "help", "support",
  "docs", "photos", "places", "flights", "assistant", "billing", "login", "account", "security",
  "static", "cdn", "assets", "dev", "staging", "test", "demo", "root", "system", "stripe",
]);

/** <service>-<slug> is a service host, so a slug must not look like one. */
const SERVICE_PREFIX = /^(photos|docs|places|flights|assistant)-/;

export function isValidSlug(slug: string) {
  return SLUG_RE.test(slug) && !slug.includes("--") && !RESERVED.has(slug) && !SERVICE_PREFIX.test(slug);
}

/** "Familie Müller" -> "familie-mueller" */
export function suggestSlug(name: string) {
  return name
    .toLowerCase()
    .replace(/ä/g, "ae").replace(/ö/g, "oe").replace(/ü/g, "ue")
    .normalize("NFKD").replace(/\p{M}/gu, "") // strip combining accents
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 30);
}
