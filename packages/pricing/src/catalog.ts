// The price list. All amounts are in Rappen (1 CHF = 100 Rp), monthly,
// consumer prices including Swiss VAT. Change prices here; Stripe prices are
// synced from this file (apps/portal: npm run stripe:sync).

export type ServiceKey = "photos" | "docs" | "places" | "flights" | "assistant";

export const CURRENCY = "chf";

/** Swiss standard VAT rate (MWST/TVA/IVA) since 2024. */
export const VAT_RATE = 0.081;

/** Stripe Switzerland: 2.9% + CHF 0.30 per card charge, plus 0.7% Stripe Billing. */
export const STRIPE_FEES = { percent: 0.029 + 0.007, fixedRp: 30 };

/**
 * Share donated to the open-source projects we run: 10 % of the net base
 * price (everything except storage and the AI credit, excl. VAT).
 */
export const OSS_SHARE = 0.1;

/** Yearly billing: pay 10 months, get 12. */
export const YEARLY_MONTHS_CHARGED = 10;

export const STORAGE_UNIT_GB = 100;
export const MAX_STORAGE_UNITS = 50; // 5 TB
export const MAX_EXTRA_MEMBERS = 5; // 6 people per household

export type CatalogItem = {
  /** Stable key, also the Stripe price lookup key prefix. */
  key: string;
  priceRp: number;
  /** Our estimated monthly infrastructure cost, excl. VAT (servers, disks, backups, APIs). */
  costRp: number;
  /**
   * Open-source project that receives this item's donation share.
   * "platform": shared equally by the projects of the booked services.
   * null: no donation (storage only affects servers/storage and operations).
   */
  project: OssProject | "platform" | null;
};

export type OssProject = "immich" | "paperless" | "dawarich" | "airtrail" | "hermes";

export const OSS_PROJECTS: Record<OssProject, { name: string; url: string }> = {
  immich: { name: "Immich", url: "https://immich.app" },
  paperless: { name: "Paperless-ngx", url: "https://docs.paperless-ngx.com" },
  dawarich: { name: "Dawarich", url: "https://dawarich.app" },
  airtrail: { name: "AirTrail", url: "https://airtrail.johan.ohly.dk" },
  hermes: { name: "Hermes Agent (Nous Research)", url: "https://nousresearch.com" },
};

/** Always included: account, single sign-on, Swiss hosting, encrypted backups, support. */
export const BASE: CatalogItem = { key: "base", priceRp: 300, costRp: 60, project: "platform" };

/** Per 100 GB of encrypted storage (incl. off-site encrypted backup). */
export const STORAGE: CatalogItem = { key: "storage", priceRp: 200, costRp: 70, project: null };

/** Per additional household member. */
export const MEMBER: CatalogItem = { key: "member", priceRp: 150, costRp: 10, project: "platform" };

export const SERVICES: Record<ServiceKey, CatalogItem> = {
  photos: { key: "photos", priceRp: 300, costRp: 90, project: "immich" },
  docs: { key: "docs", priceRp: 200, costRp: 50, project: "paperless" },
  places: { key: "places", priceRp: 150, costRp: 30, project: "dawarich" },
  flights: { key: "flights", priceRp: 100, costRp: 15, project: "airtrail" },
  // Agent hosting and memory; the AI usage itself is paid by the monthly credit.
  assistant: { key: "assistant", priceRp: 300, costRp: 40, project: "hermes" },
};

export const SERVICE_KEYS = Object.keys(SERVICES) as ServiceKey[];

/**
 * Monthly AI credit (CHF) when the assistant is booked. Passed through to the
 * AI provider and enforced as a hard monthly limit. Never discounted, no
 * donation share. Stripe: quantity x CHF 1.
 */
export const AI_CREDIT_TIERS_CHF = [5, 10, 20, 50] as const;
export const AI_CREDIT_UNIT_RP = 100;
