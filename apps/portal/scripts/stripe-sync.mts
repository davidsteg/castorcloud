// Creates/updates Stripe products and prices from packages/pricing/catalog.ts.
//   npm run stripe:sync -w @pc/portal
// Idempotent. A changed amount creates a new price and moves the lookup key
// to it; existing subscriptions keep their old price until they are updated.
import path from "node:path";
import { config } from "dotenv";
import Stripe from "stripe";
import { AI_CREDIT_UNIT_RP, BASE, CURRENCY, MEMBER, SERVICES, STORAGE, YEARLY_MONTHS_CHARGED, type CatalogItem } from "@pc/pricing";

config({ path: path.resolve(import.meta.dirname, "../../../.env"), quiet: true });
const key = process.env.STRIPE_SECRET_KEY;
if (!key) throw new Error("STRIPE_SECRET_KEY is not set in .env");
if (!key.startsWith("sk_test_") && process.argv[2] !== "--live") {
  throw new Error("Refusing to sync a live key without --live");
}
const stripe = new Stripe(key);

const NAMES: Record<string, string> = {
  base: "Base (account, single sign-on, encrypted backups)",
  storage: "Storage (per 100 GB)",
  member: "Additional household member",
  photos: "Photos",
  docs: "Documents",
  places: "Timeline",
  flights: "Flights",
  assistant: "Assistant",
  credit: "AI credit (CHF 1 per month)",
};

type SyncItem = Pick<CatalogItem, "key" | "priceRp"> & { yearlyRp?: number };

// The AI credit is passed through to the AI provider: no yearly discount.
const CREDIT: SyncItem = { key: "credit", priceRp: AI_CREDIT_UNIT_RP, yearlyRp: AI_CREDIT_UNIT_RP * 12 };

async function ensureProduct(item: SyncItem) {
  const id = `pc_${item.key}`;
  try {
    return await stripe.products.update(id, { name: NAMES[item.key], active: true });
  } catch {
    return stripe.products.create({ id, name: NAMES[item.key], metadata: { key: item.key } });
  }
}

for (const item of [BASE, STORAGE, MEMBER, ...Object.values(SERVICES), CREDIT] as SyncItem[]) {
  const product = await ensureProduct(item);
  for (const interval of ["month", "year"] as const) {
    const lookup = `pc_${item.key}_${interval}`;
    const amount = interval === "year" ? (item.yearlyRp ?? item.priceRp * YEARLY_MONTHS_CHARGED) : item.priceRp;
    const existing = (await stripe.prices.list({ lookup_keys: [lookup], limit: 1 })).data[0];
    if (existing && existing.unit_amount === amount && existing.active) {
      console.log(`ok       ${lookup} CHF ${(amount / 100).toFixed(2)}`);
      continue;
    }
    await stripe.prices.create({
      product: product.id,
      currency: CURRENCY,
      unit_amount: amount,
      recurring: { interval },
      tax_behavior: "inclusive", // consumer prices include Swiss VAT
      lookup_key: lookup,
      transfer_lookup_key: true,
      metadata: { key: item.key },
    });
    console.log(`${existing ? "updated" : "created"}  ${lookup} CHF ${(amount / 100).toFixed(2)}`);
  }
}
