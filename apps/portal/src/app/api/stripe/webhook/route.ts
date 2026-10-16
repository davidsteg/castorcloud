// Stripe is the source of truth for what a customer has booked. Every
// subscription change arrives here and becomes a tenant lifecycle job.
import type Stripe from "stripe";
import { NextResponse, type NextRequest } from "next/server";
import { prisma, type ServiceKind } from "@pc/db";
import { audit, enqueueTenantJob } from "@pc/core";
import { SERVICE_KEYS } from "@pc/pricing";
import { itemKeyFromLookup, stripe } from "@/lib/stripe";

export const runtime = "nodejs";

/** Grace period before a cancelled/unpaid tenant's data is deleted. */
const DELETE_AFTER_DAYS = 30;

export async function POST(req: NextRequest) {
  const s = stripe();
  const secret = process.env.STRIPE_WEBHOOK_SECRET;
  if (!s || !secret) return NextResponse.json({ error: "stripe not configured" }, { status: 503 });

  let event: Stripe.Event;
  try {
    event = s.webhooks.constructEvent(await req.text(), req.headers.get("stripe-signature") ?? "", secret);
  } catch {
    return NextResponse.json({ error: "invalid signature" }, { status: 400 });
  }

  // Exactly once: Stripe retries, and events may arrive more than once.
  try {
    await prisma.stripeEvent.create({ data: { id: event.id, type: event.type } });
  } catch {
    return NextResponse.json({ received: true, duplicate: true });
  }

  try {
    await handle(event);
  } catch (err) {
    // Let Stripe retry later.
    await prisma.stripeEvent.delete({ where: { id: event.id } });
    console.error("stripe webhook failed", event.type, err);
    return NextResponse.json({ error: "handler failed" }, { status: 500 });
  }
  return NextResponse.json({ received: true });
}

async function handle(event: Stripe.Event) {
  switch (event.type) {
    case "checkout.session.completed": {
      const session = event.data.object;
      const tenantId = session.metadata?.tenantId;
      if (!tenantId || session.mode !== "subscription") return;
      const tenant = await prisma.tenant.update({
        where: { id: tenantId },
        data: {
          stripeCustomerId: String(session.customer),
          stripeSubscriptionId: String(session.subscription),
          subscriptionStatus: "active",
        },
      });
      await audit("stripe", "billing.checkout_completed", tenantId, { session: session.id });
      if (tenant.status === "PENDING") {
        await prisma.tenant.update({ where: { id: tenantId }, data: { status: "PROVISIONING" } });
        await enqueueTenantJob(tenantId, "PROVISION");
      }
      return;
    }

    case "customer.subscription.updated": {
      const sub = event.data.object;
      const tenant = await tenantFor(sub);
      if (!tenant) return;

      const booked = bookedConfig(sub);
      const periodEnd = sub.items.data[0]?.current_period_end;
      await prisma.tenant.update({
        where: { id: tenant.id },
        data: {
          subscriptionStatus: sub.status,
          currentPeriodEnd: periodEnd ? new Date(periodEnd * 1000) : null,
          storageUnits: booked.storageUnits,
          extraMembers: booked.extraMembers,
          aiCreditChf: booked.aiCreditChf,
          stackMemoryUnits: booked.stacks.memoryUnits,
          stackCpuUnits: booked.stacks.cpuUnits,
          stackStorageUnits: booked.stacks.storageUnits,
        },
      });
      const changed = await syncServices(tenant.id, booked.services);
      const stacksChanged = booked.stacks.memoryUnits !== tenant.stackMemoryUnits
        || booked.stacks.cpuUnits !== tenant.stackCpuUnits
        || booked.stacks.storageUnits !== tenant.stackStorageUnits;

      if (sub.status === "active" && tenant.status === "SUSPENDED") {
        await prisma.tenant.update({ where: { id: tenant.id }, data: { deleteAfter: null } });
        await enqueueTenantJob(tenant.id, "RESUME");
      } else if (
        tenant.status === "ACTIVE" &&
        (changed || booked.storageUnits !== tenant.storageUnits || booked.aiCreditChf !== tenant.aiCreditChf)
      ) {
        await enqueueTenantJob(tenant.id, "UPDATE");
      } else if (tenant.status === "ACTIVE" && stacksChanged) {
        // Custom stacks only (an UPDATE is followed by a STACKS job anyway).
        await enqueueTenantJob(tenant.id, "STACKS");
      }
      return;
    }

    case "customer.subscription.deleted": {
      const tenant = await tenantFor(event.data.object);
      if (!tenant) return;
      const deleteAfter = new Date(Date.now() + DELETE_AFTER_DAYS * 24 * 3600 * 1000);
      await prisma.tenant.update({ where: { id: tenant.id }, data: { subscriptionStatus: "canceled", deleteAfter } });
      await audit("stripe", "billing.subscription_deleted", tenant.id, { deleteAfter });
      await enqueueTenantJob(tenant.id, "SUSPEND");
      return;
    }
  }
}

async function tenantFor(sub: Stripe.Subscription) {
  return prisma.tenant.findFirst({
    where: { OR: [{ stripeSubscriptionId: sub.id }, { id: sub.metadata?.tenantId ?? "-" }] },
  });
}

function bookedConfig(sub: Stripe.Subscription) {
  const services: string[] = [];
  let storageUnits = 1;
  let extraMembers = 0;
  let aiCreditChf = 0;
  const stacks = { memoryUnits: 0, cpuUnits: 0, storageUnits: 0 };
  for (const item of sub.items.data) {
    const key = itemKeyFromLookup(item.price.lookup_key);
    if (key === "credit") aiCreditChf = item.quantity ?? 0;
    else if (key === "stackram") stacks.memoryUnits = item.quantity ?? 0;
    else if (key === "stackcpu") stacks.cpuUnits = item.quantity ?? 0;
    else if (key === "stackdisk") stacks.storageUnits = item.quantity ?? 0;
    else if (key === "storage") storageUnits = item.quantity ?? 1;
    else if (key === "member") extraMembers = item.quantity ?? 0;
    else if (key && (SERVICE_KEYS as string[]).includes(key)) services.push(key);
  }
  return { services, storageUnits, extraMembers, aiCreditChf, stacks };
}

/** Enables booked services, disables the rest. Returns whether anything changed. */
async function syncServices(tenantId: string, booked: string[]) {
  let changed = false;
  for (const key of SERVICE_KEYS) {
    const kind = key.toUpperCase() as ServiceKind;
    const enabled = booked.includes(key);
    const current = await prisma.serviceInstance.findUnique({ where: { tenantId_kind: { tenantId, kind } } });
    if (current?.enabled === enabled || (!current && !enabled)) continue;
    await prisma.serviceInstance.upsert({
      where: { tenantId_kind: { tenantId, kind } },
      update: { enabled },
      create: { tenantId, kind, enabled },
    });
    changed = true;
  }
  return changed;
}
