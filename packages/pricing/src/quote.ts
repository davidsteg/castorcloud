// Turns a configuration into a price and a transparent breakdown of where
// every franc goes. Used by the pricing page, signup and Stripe checkout, so
// what the customer sees is exactly what they are charged.
//
// Rules:
//  - The open-source donation is 10 % of the net *base price*: base fee,
//    services and members. Storage and the AI credit carry no donation.
//  - More storage only adds to "servers & storage" and "operations"
//    (plus the VAT and payment fees every franc carries).
//  - The AI credit is passed through to the AI provider: never discounted.
import {
  AI_CREDIT_TIERS_CHF, AI_CREDIT_UNIT_RP, BASE, MAX_EXTRA_MEMBERS, MAX_STORAGE_UNITS, MEMBER,
  OSS_SHARE, SERVICES, STACK_CPU, STACK_MEMORY, STACK_STORAGE, STORAGE, STRIPE_FEES, VAT_RATE, YEARLY_MONTHS_CHARGED,
  type CatalogItem, type OssProject, type ServiceKey,
} from "./catalog.ts";
import { NO_STACK_UNITS, type StackUnits } from "./stacks.ts";

/** Most a tenant can book for its custom stacks, summed over all of them. */
export const MAX_STACK_UNITS: StackUnits = { memoryUnits: 256, cpuUnits: 64, storageUnits: 200 };

export type Interval = "month" | "year";

export type Config = {
  services: ServiceKey[];
  storageUnits: number; // x 100 GB
  extraMembers: number;
  interval: Interval;
  /** Monthly AI credit in CHF (one of AI_CREDIT_TIERS_CHF), 0 without assistant. */
  aiCreditChf: number;
  /** Resources reserved by the tenant's custom stacks (docs/stacks.md). */
  stacks: StackUnits;
};

export type LineItem = { key: string; quantity: number; unitRp: number; totalRp: number };

export type Quote = {
  config: Config;
  lines: LineItem[];
  /** Monthly list price incl. VAT (before the yearly discount). */
  monthlyRp: number;
  /** What is charged per billing interval incl. VAT. */
  chargeRp: number;
  /** Effective price per month incl. VAT (after the yearly discount). */
  effectiveMonthlyRp: number;
  /** Where the effective monthly price goes. Parts sum to effectiveMonthlyRp. */
  breakdown: {
    vatRp: number;
    paymentFeesRp: number;
    openSourceRp: number;
    infrastructureRp: number;
    aiCreditRp: number;
    operationsRp: number;
  };
  /** Monthly donation per open-source project. Sums to breakdown.openSourceRp. */
  donations: Partial<Record<OssProject, number>>;
};

export const DEFAULT_AI_CREDIT_CHF = 10;

export function normalize(input: Partial<Config>): Config {
  const services = [...new Set(input.services ?? [])].filter((s): s is ServiceKey => s in SERVICES);
  const clamp = (v: unknown, min: number, max: number) =>
    Math.min(max, Math.max(min, Math.round(Number.isFinite(Number(v)) ? Number(v) : min)));
  const credit = Number(input.aiCreditChf);
  const aiCreditChf = !services.includes("assistant")
    ? 0
    : (AI_CREDIT_TIERS_CHF as readonly number[]).includes(credit) ? credit : DEFAULT_AI_CREDIT_CHF;
  return {
    services,
    storageUnits: clamp(input.storageUnits, 1, MAX_STORAGE_UNITS),
    extraMembers: clamp(input.extraMembers, 0, MAX_EXTRA_MEMBERS),
    interval: input.interval === "year" ? "year" : "month",
    aiCreditChf,
    stacks: {
      memoryUnits: clamp(input.stacks?.memoryUnits ?? 0, 0, MAX_STACK_UNITS.memoryUnits),
      cpuUnits: clamp(input.stacks?.cpuUnits ?? 0, 0, MAX_STACK_UNITS.cpuUnits),
      storageUnits: clamp(input.stacks?.storageUnits ?? NO_STACK_UNITS.storageUnits, 0, MAX_STACK_UNITS.storageUnits),
    },
  };
}

const net = (grossRp: number) => grossRp / (1 + VAT_RATE);

export function quote(input: Partial<Config>): Quote {
  const config = normalize(input);
  const items: Array<[CatalogItem, number]> = [
    [BASE, 1],
    [STORAGE, config.storageUnits],
    ...config.services.map((s) => [SERVICES[s], 1] as [CatalogItem, number]),
  ];
  if (config.extraMembers > 0) items.push([MEMBER, config.extraMembers]);
  // Stack resources: pure infrastructure, so no donation share (project null).
  if (config.stacks.memoryUnits > 0) items.push([STACK_MEMORY, config.stacks.memoryUnits]);
  if (config.stacks.cpuUnits > 0) items.push([STACK_CPU, config.stacks.cpuUnits]);
  if (config.stacks.storageUnits > 0) items.push([STACK_STORAGE, config.stacks.storageUnits]);

  const lines: LineItem[] = items.map(([item, quantity]) => ({
    key: item.key,
    quantity,
    unitRp: item.priceRp,
    totalRp: item.priceRp * quantity,
  }));
  const creditRp = config.aiCreditChf * AI_CREDIT_UNIT_RP;
  if (creditRp > 0) {
    lines.push({ key: "credit", quantity: config.aiCreditChf, unitRp: AI_CREDIT_UNIT_RP, totalRp: creditRp });
  }

  const subscriptionRp = items.reduce((sum, [item, q]) => sum + item.priceRp * q, 0);
  const monthlyRp = subscriptionRp + creditRp;

  const yearly = config.interval === "year";
  const monthsCovered = yearly ? 12 : 1;
  const factor = yearly ? YEARLY_MONTHS_CHARGED / 12 : 1; // yearly discount (not on the credit)
  const chargeRp = yearly ? subscriptionRp * YEARLY_MONTHS_CHARGED + creditRp * 12 : monthlyRp;
  const effectiveMonthlyRp = Math.round(subscriptionRp * factor + creditRp);

  // Per month, following the money of one effective monthly payment.
  const vatRp = Math.round(effectiveMonthlyRp - net(effectiveMonthlyRp));
  const netRp = effectiveMonthlyRp - vatRp;
  const paymentFeesRp = Math.round(effectiveMonthlyRp * STRIPE_FEES.percent + STRIPE_FEES.fixedRp / monthsCovered);

  // Donation: 10 % of the net base price. Services fund their own project;
  // base fee and members are shared by the booked services' projects.
  const platformProjects = [...new Set<OssProject>(config.services.map((s) => SERVICES[s].project as OssProject))];
  const donations: Partial<Record<OssProject, number>> = {};
  for (const [item, quantity] of items) {
    if (item.project === null) continue;
    const share = net(item.priceRp * quantity * factor) * OSS_SHARE;
    const recipients = item.project === "platform" ? platformProjects : [item.project];
    if (recipients.length === 0) continue; // no service booked: nothing to fund
    for (const p of recipients) donations[p] = (donations[p] ?? 0) + share / recipients.length;
  }
  for (const k of Object.keys(donations) as OssProject[]) donations[k] = Math.round(donations[k]!);
  const openSourceRp = Object.values(donations).reduce((a, b) => a + (b ?? 0), 0);

  const infrastructureRp = items.reduce((sum, [item, quantity]) => sum + item.costRp * quantity, 0);
  const aiCreditRp = Math.round(net(creditRp));
  const operationsRp = netRp - paymentFeesRp - openSourceRp - infrastructureRp - aiCreditRp;

  return {
    config,
    lines,
    monthlyRp,
    chargeRp,
    effectiveMonthlyRp,
    breakdown: { vatRp, paymentFeesRp, openSourceRp, infrastructureRp, aiCreditRp, operationsRp },
    donations,
  };
}

/** "CHF 12.50" / "12.50" */
export function formatChf(rp: number, withCurrency = true): string {
  const s = (rp / 100).toFixed(2);
  return withCurrency ? `CHF ${s}` : s;
}
