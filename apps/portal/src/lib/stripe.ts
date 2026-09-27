import "server-only";
import Stripe from "stripe";
import type { Interval, LineItem } from "@pc/pricing";

let client: Stripe | null | undefined;

/** null when Stripe is not configured (local development without keys). */
export function stripe(): Stripe | null {
  if (client === undefined) {
    const key = process.env.STRIPE_SECRET_KEY;
    client = key ? new Stripe(key, { appInfo: { name: "personal-cloud" } }) : null;
  }
  return client;
}

/** Prices are addressed by lookup key, e.g. "pc_photos_month" (see scripts/stripe-sync.ts). */
export const lookupKey = (itemKey: string, interval: Interval) => `pc_${itemKey}_${interval}`;

/** Reverse of lookupKey: "pc_storage_year" -> "storage". */
export function itemKeyFromLookup(lookup: string | null | undefined): string | null {
  const m = lookup?.match(/^pc_([a-z]+)_(month|year)$/);
  return m ? m[1] : null;
}

export async function lineItemsFor(s: Stripe, lines: LineItem[], interval: Interval) {
  const keys = lines.map((l) => lookupKey(l.key, interval));
  const prices = await s.prices.list({ lookup_keys: keys, active: true, limit: 100 });
  const byKey = new Map(prices.data.map((p) => [p.lookup_key, p.id]));
  return lines.map((l) => {
    const price = byKey.get(lookupKey(l.key, interval));
    if (!price) throw new Error(`Stripe price ${lookupKey(l.key, interval)} missing – run npm run stripe:sync`);
    return { price, quantity: l.quantity };
  });
}

export const STRIPE_LOCALES = { de: "de", fr: "fr", it: "it", en: "en" } as const;
