import "server-only";
import { prisma, type Tenant } from "@pc/db";
import { audit, enqueueTenantJob, sealSecret } from "@pc/core";
import {
  MAX_STACK_UNITS, NO_STACK_UNITS, STACK_CPU, STACK_MEMORY, STACK_STORAGE, addUnits, fitsUnits, stackUnits,
  type StackUnits,
} from "@pc/pricing";
import { RESERVED_STACK_NAMES, STACK_NAME_RE, SpecError, catalogApp, specUnits, stackHost, stackResources, storedDefinition } from "@pc/stacks";
import { PUBLIC_SCHEME } from "@/lib/config";
import { itemKeyFromLookup, lineItemsFor, stripe } from "@/lib/stripe";

/**
 * Customers' own apps (docs/stacks.md) are served under their own domain, so
 * code we don't control never shares a cookie domain with the portal. Unset:
 * the feature is off.
 */
export const STACKS_DOMAIN = process.env.STACKS_DOMAIN ?? "";
export const stacksEnabled = () => STACKS_DOMAIN !== "";
export const MAX_STACKS = 10;

export const stackOrigin = (slug: string, name: string) => `${PUBLIC_SCHEME}://${stackHost(slug, name, STACKS_DOMAIN)}`;

/** Must match the provisioner's sealing (rust/crates/castord/src/stacks/run.rs definition_key). */
const definitionKey = (name: string) => `stack-definition:${name}`;

export type AddResult = { ok: true } | { ok: false; error: "name" | "invalid" | "taken" | "limit" | "billing" | "too-big" | "unavailable"; problems?: string[] };

const unitsOf = (t: Pick<Tenant, "stackMemoryUnits" | "stackCpuUnits" | "stackStorageUnits">): StackUnits => ({
  memoryUnits: t.stackMemoryUnits, cpuUnits: t.stackCpuUnits, storageUnits: t.stackStorageUnits,
});

/** What the tenant's stacks reserve together (removed ones excluded). */
async function neededUnits(tenantId: string): Promise<StackUnits> {
  const stacks = await prisma.stack.findMany({ where: { tenantId, status: { not: "REMOVING" } } });
  return stacks.reduce((sum, s) => addUnits(sum, stackUnits(s)), NO_STACK_UNITS);
}

/** Monthly list price of some stack units, in Rappen. */
export function unitsPriceRp(u: StackUnits) {
  return u.memoryUnits * STACK_MEMORY.priceRp + u.cpuUnits * STACK_CPU.priceRp + u.storageUnits * STACK_STORAGE.priceRp;
}

/**
 * Sets the subscription's stack items to what the stacks reserve. The webhook
 * then stores the booking and starts the STACKS job. Without Stripe
 * (DEV_SKIP_PAYMENT) the booking is stored directly.
 */
async function syncBooking(tenant: Tenant, units: StackUnits) {
  const s = stripe();
  if (!s) {
    if (process.env.DEV_SKIP_PAYMENT !== "true") throw new Error("payments are not configured");
    await prisma.tenant.update({
      where: { id: tenant.id },
      data: { stackMemoryUnits: units.memoryUnits, stackCpuUnits: units.cpuUnits, stackStorageUnits: units.storageUnits },
    });
    await enqueueTenantJob(tenant.id, "STACKS");
    return;
  }
  if (!tenant.stripeSubscriptionId) throw new Error("no subscription");
  const interval = tenant.billingInterval === "YEAR" ? "year" : "month";
  const sub = await s.subscriptions.retrieve(tenant.stripeSubscriptionId);
  const wanted: [string, number][] = [[STACK_MEMORY.key, units.memoryUnits], [STACK_CPU.key, units.cpuUnits], [STACK_STORAGE.key, units.storageUnits]];
  const items: { id?: string; price?: string; quantity?: number; deleted?: boolean }[] = [];
  for (const [key, quantity] of wanted) {
    const existing = sub.items.data.find((i) => itemKeyFromLookup(i.price.lookup_key) === key);
    if (existing && quantity > 0) items.push({ id: existing.id, quantity });
    else if (existing) items.push({ id: existing.id, deleted: true });
    else if (quantity > 0) {
      const [line] = await lineItemsFor(s, [{ key, quantity, unitRp: 0, totalRp: 0 }], interval);
      items.push(line);
    }
  }
  if (items.length) await s.subscriptions.update(sub.id, { items, proration_behavior: "create_prorations" });
}

/** Adds a catalog app or the owner's own definition. The caller has checked the user owns the tenant. */
export async function addStack(tenant: Tenant, actorId: string, input: { name: string; catalogKey?: string; definition?: string }): Promise<AddResult> {
  if (!stacksEnabled()) return { ok: false, error: "unavailable" };
  const name = input.name.trim().toLowerCase();
  if (!STACK_NAME_RE.test(name) || RESERVED_STACK_NAMES.has(name)) return { ok: false, error: "name" };

  const app = input.catalogKey ? catalogApp(input.catalogKey) : undefined;
  if (input.catalogKey && !app) return { ok: false, error: "name" };
  const source = app ? app.source : (input.definition ?? "");

  let spec, definition;
  try {
    ({ spec, definition } = storedDefinition(source));
  } catch (err) {
    if (err instanceof SpecError) return { ok: false, error: "invalid", problems: err.problems };
    throw err;
  }

  const count = await prisma.stack.count({ where: { tenantId: tenant.id, status: { not: "REMOVING" } } });
  if (count >= MAX_STACKS) return { ok: false, error: "limit" };
  if (await prisma.stack.findUnique({ where: { tenantId_name: { tenantId: tenant.id, name } } })) return { ok: false, error: "taken" };

  const total = addUnits(await neededUnits(tenant.id), specUnits(spec));
  if (!fitsUnits(total, MAX_STACK_UNITS)) return { ok: false, error: "too-big" };

  const r = stackResources(spec);
  const stack = await prisma.stack.create({
    data: {
      tenantId: tenant.id,
      name,
      source: app ? "CATALOG" : "CUSTOM",
      catalogKey: app?.key,
      // May hold passwords: sealed like every other tenant secret. Stored as
      // the parsed document in JSON, which castord reads (storedDefinition).
      definition: sealSecret(tenant.id, definitionKey(name), definition),
      memoryMb: r.memoryMb,
      cpus: r.cpus,
      storageGb: r.storageGb,
    },
  });
  try {
    await syncBooking(tenant, total);
  } catch (err) {
    console.error("stack booking failed", err);
    await prisma.stack.delete({ where: { id: stack.id } });
    return { ok: false, error: "billing" };
  }
  await audit(actorId, "stack.added", tenant.id, { stack: name, source: stack.source, catalogKey: app?.key, units: specUnits(spec) });
  // The booking already covers it (e.g. after a failed deploy): no webhook change, so start the job here.
  if (fitsUnits(total, unitsOf(tenant))) await enqueueTenantJob(tenant.id, "STACKS");
  return { ok: true };
}

/** Removes a stack and, with it, its data. The caller has checked the user owns the tenant. */
export async function removeStack(tenant: Tenant, actorId: string, stackId: string) {
  const stack = await prisma.stack.findFirst({ where: { id: stackId, tenantId: tenant.id } });
  if (!stack || stack.status === "REMOVING") return;
  await prisma.stack.update({ where: { id: stack.id }, data: { status: "REMOVING" } });
  await audit(actorId, "stack.remove_requested", tenant.id, { stack: stack.name });
  await enqueueTenantJob(tenant.id, "STACKS");
  try {
    await syncBooking(tenant, await neededUnits(tenant.id));
  } catch (err) {
    // The stack goes anyway; the booking is corrected by the next change.
    console.error("stack booking update failed", err);
  }
}
