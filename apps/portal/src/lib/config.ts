export const BASE_DOMAIN = process.env.BASE_DOMAIN ?? "brand.localhost";
export const PUBLIC_SCHEME = process.env.PUBLIC_SCHEME ?? "http";
export const BRAND_NAME = process.env.BRAND_NAME ?? "Castor Cloud";

/**
 * Where assistant requests are processed. Decides which privacy statement the
 * site may make: "swiss" only when a Swiss provider (e.g. Infomaniak) is used.
 */
export const AI_PROVIDER: "openrouter" | "swiss" = process.env.AI_PROVIDER === "swiss" ? "swiss" : "openrouter";

/** Host label reserved for signup, marketing and the admin console. */
export const APP_LABEL = "app";

export const SERVICES = {
  photos: { label: "Photos", icon: "▦" },
  docs: { label: "Documents", icon: "▤" },
  places: { label: "Timeline", icon: "◉" },
  flights: { label: "Flights", icon: "✈" },
} as const;

export type ServiceKey = keyof typeof SERVICES;

/** Service hosts: <service>-<slug>. Slugs may not start with these prefixes. */
export const SERVICE_HOST_RE = /^(photos|docs|places|flights|assistant)-/;

export function isServiceKey(value: string): value is ServiceKey {
  return value in SERVICES;
}

/**
 * Origin of a tenant's embedded service, e.g. https://photos-muster.brand.cloud.
 * Flat names (one label) so a single *.domain certificate covers every tenant.
 */
export function serviceOrigin(slug: string, service: ServiceKey): string {
  return `${PUBLIC_SCHEME}://${service}-${slug}.${BASE_DOMAIN}`;
}

/** Tenant slug from a request host, or null for the app host / foreign hosts. */
export function tenantSlugFromHost(host: string | null): string | null {
  if (!host) return null;
  const hostname = host.split(":")[0];
  const suffix = `.${BASE_DOMAIN}`;
  if (!hostname.endsWith(suffix)) return null;
  const label = hostname.slice(0, -suffix.length);
  if (label === APP_LABEL || label.includes(".") || SERVICE_HOST_RE.test(label)) return null;
  return label;
}
