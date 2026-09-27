// What a tenant stack consists of, and how its config files are rendered.
import { randomBytes } from "node:crypto";
import type { RedirectUri } from "./authentik.ts";

export type ServiceKey = "photos" | "docs" | "places" | "flights";
export const STACK_SERVICES: ServiceKey[] = ["photos", "docs", "places", "flights"];

export type Platform = {
  baseDomain: string;
  scheme: "http" | "https";
  brandName: string;
  timeZone: string;
  /** How containers reach the OIDC issuer host: "host-gateway" in dev. */
  authHostIp: string;
};

export type TenantSecrets = Record<string, string>;

type ServiceDef = {
  label: string;
  /** Compose services started by this service's profile. */
  containers: string[];
  /** Named volumes (see compose.yml) holding this service's data. */
  volumes: string[];
  redirectPaths: string[];
  extraRedirects?: string[];
  /** Path answered with < 400 once the service is up. */
  healthPath: string;
};

export const SERVICES: Record<ServiceKey, ServiceDef> = {
  photos: {
    label: "Photos",
    containers: ["immich-server", "immich-db"],
    volumes: ["immich-db", "photos"],
    redirectPaths: ["/auth/login", "/user-settings"],
    extraRedirects: ["app.immich:///oauth-callback"],
    healthPath: "/api/server/ping",
  },
  docs: {
    label: "Documents",
    containers: ["paperless"],
    volumes: ["docs-data", "docs-media", "docs-consume"],
    redirectPaths: ["/accounts/oidc/authentik/login/callback/"],
    healthPath: "/accounts/login/",
  },
  places: {
    label: "Timeline",
    containers: ["dawarich", "dawarich-sidekiq"],
    volumes: ["places-public", "places-watched", "places-storage"],
    redirectPaths: ["/users/auth/openid_connect/callback"],
    healthPath: "/api/v1/health",
  },
  flights: {
    label: "Flights",
    containers: ["airtrail"],
    volumes: ["flights-uploads"],
    redirectPaths: ["/login"],
    healthPath: "/login",
  },
};

/** Volumes every stack has, whatever is booked. */
export const BASE_VOLUMES = ["db", "redis"];
export const ALL_VOLUMES = [...BASE_VOLUMES, ...STACK_SERVICES.flatMap((s) => SERVICES[s].volumes)];

export function names(slug: string, p: Platform) {
  return {
    project: `pc-t-${slug}`,
    membersGroup: `cust-${slug}`,
    ownersGroup: `cust-${slug}-owners`,
    authHost: `auth.${p.baseDomain}`,
    portalOrigin: `${p.scheme}://${slug}.${p.baseDomain}`,
    // Flat hostnames: one *.domain certificate covers every tenant.
    serviceOrigin: (s: ServiceKey) => `${p.scheme}://${s}-${slug}.${p.baseDomain}`,
    appSlug: (s: ServiceKey) => `${slug}-${s}`,
    issuer: (s: ServiceKey) => `${p.scheme}://auth.${p.baseDomain}/application/o/${slug}-${s}/`,
  };
}

export function redirectUris(slug: string, p: Platform, service: ServiceKey): RedirectUri[] {
  const origin = names(slug, p).serviceOrigin(service);
  const def = SERVICES[service];
  return [
    ...def.redirectPaths.map((path) => ({ matching_mode: "strict" as const, url: origin + path })),
    ...(def.extraRedirects ?? []).map((url) => ({ matching_mode: "strict" as const, url })),
  ];
}

// Alphanumeric only: some upstreams (Immich, AirTrail) reject special characters.
const token = (bytes = 24) => randomBytes(bytes).toString("hex");

export const SECRET_KEYS = [
  "IMMICH_DB_PASSWORD", "DB_ROOT_PASSWORD", "PAPERLESS_DB_PASSWORD", "DAWARICH_DB_PASSWORD",
  "AIRTRAIL_DB_PASSWORD", "PAPERLESS_SECRET_KEY", "PAPERLESS_SERVICE_PASSWORD",
  "IMMICH_SERVICE_PASSWORD", "PHOTOS_OIDC_CLIENT_SECRET", "DOCS_OIDC_CLIENT_SECRET",
  "PLACES_OIDC_CLIENT_SECRET", "FLIGHTS_OIDC_CLIENT_SECRET", "DAWARICH_SECRET_KEY_BASE",
] as const;

/** Fills in any missing secret; existing values are kept so re-runs are safe. */
export function ensureSecrets(existing: TenantSecrets): { secrets: TenantSecrets; created: string[] } {
  const secrets = { ...existing };
  const created: string[] = [];
  for (const key of SECRET_KEYS) {
    if (secrets[key]) continue;
    secrets[key] = key === "DAWARICH_SECRET_KEY_BASE" ? token(64) : token();
    created.push(key);
  }
  return { secrets, created };
}

export function immichConfig(slug: string, p: Platform, secrets: TenantSecrets) {
  const n = names(slug, p);
  return {
    oauth: {
      enabled: true,
      issuerUrl: n.issuer("photos"),
      clientId: n.appSlug("photos"),
      clientSecret: secrets.PHOTOS_OIDC_CLIENT_SECRET,
      scope: "openid email profile",
      autoLaunch: true,
      autoRegister: true,
      buttonText: "Continue",
      tokenEndpointAuthMethod: "client_secret_post",
      signingAlgorithm: "RS256",
      mobileOverrideEnabled: false,
    },
    passwordLogin: { enabled: false },
    machineLearning: { urls: ["http://pc-node-immich-ml:3003"] },
    server: { externalDomain: n.serviceOrigin("photos") },
    newVersionCheck: { enabled: false },
  };
}

export function paperlessProviders(slug: string, p: Platform, secrets: TenantSecrets) {
  const n = names(slug, p);
  return {
    openid_connect: {
      SCOPE: ["openid", "profile", "email"],
      OAUTH_PKCE_ENABLED: true,
      APPS: [
        {
          provider_id: "authentik",
          name: p.brandName,
          client_id: n.appSlug("docs"),
          secret: secrets.DOCS_OIDC_CLIENT_SECRET,
          settings: { server_url: `${n.issuer("docs")}.well-known/openid-configuration` },
        },
      ],
    },
  };
}

/** The env file consumed by packages/stack-templates/tenant/compose.yml. */
export function tenantEnv(opts: {
  slug: string;
  p: Platform;
  secrets: TenantSecrets;
  versions: Record<string, string>;
  configDir: string;
  brandDir: string;
  caDir: string;
}): string {
  const { slug, p, secrets } = opts;
  const n = names(slug, p);
  const vars: Record<string, string> = {
    ...opts.versions,
    SLUG: slug,
    BASE_DOMAIN: p.baseDomain,
    SCHEME: p.scheme,
    BRAND_NAME: p.brandName,
    TZ: p.timeZone,
    AUTH_HOST: n.authHost,
    AUTH_HOST_IP: p.authHostIp,
    PORTAL_ORIGIN: n.portalOrigin,
    OWNERS_GROUP: n.ownersGroup,
    TENANT_DIR: opts.configDir,
    BRAND_DIR: opts.brandDir,
    CA_DIR: opts.caDir,
    OAUTH_ALLOW_INSECURE_HTTP: String(p.scheme === "http"),
    PLACES_OIDC_CLIENT_ID: n.appSlug("places"),
    FLIGHTS_OIDC_CLIENT_ID: n.appSlug("flights"),
    PAPERLESS_SOCIALACCOUNT_PROVIDERS: JSON.stringify(paperlessProviders(slug, p, secrets)),
    ...secrets,
  };
  // Single quotes: compose takes the value literally (JSON, spaces).
  return Object.entries(vars).map(([k, v]) => `${k}='${v}'`).join("\n") + "\n";
}

/** Binds every named volume into the tenant's encrypted mount. */
export function volumesOverride(mountDir: string): string {
  const lines = ["# Generated by the provisioner: volumes live on the tenant's LUKS volume.", "volumes:"];
  for (const v of ALL_VOLUMES) {
    lines.push(
      `  ${v}:`,
      "    driver: local",
      "    driver_opts:",
      "      type: none",
      "      o: bind",
      `      device: ${mountDir}/data/${v}`,
    );
  }
  return lines.join("\n") + "\n";
}

export function parseEnvFile(text: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const line of text.split(/\r?\n/)) {
    const m = line.match(/^\s*([A-Z0-9_]+)\s*=\s*(.*)\s*$/);
    if (m) out[m[1]] = m[2].replace(/^'(.*)'$/, "$1");
  }
  return out;
}
