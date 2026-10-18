import "server-only";
import { prisma, type User } from "@pc/db";
import { enqueueTenantJob } from "@pc/core";
import { quote, type Config } from "@pc/pricing";
import type { Locale } from "@/i18n";
import { appUrl } from "@/lib/config";
import { STRIPE_LOCALES, lineItemsFor, stripe } from "@/lib/stripe";

/**
 * Where to send the owner to pay for a tenant waiting for payment (PENDING):
 * a Stripe Checkout session for the booked configuration. Development
 * without Stripe keys (DEV_SKIP_PAYMENT) provisions right away. null when
 * payment isn't possible.
 */
export async function checkoutUrl(tenantId: string, user: User, locale: Locale, cancelPath: string): Promise<string | null> {
  const tenant = await prisma.tenant.findFirst({
    where: { id: tenantId, status: "PENDING", members: { some: { userId: user.id, role: "OWNER" } } },
    include: { services: { where: { enabled: true } } },
  });
  // Already paid; the subscription webhook will move it on.
  if (!tenant || tenant.stripeSubscriptionId) return null;

  const s = stripe();
  if (!s) {
    if (process.env.DEV_SKIP_PAYMENT !== "true") return null;
    const { count } = await prisma.tenant.updateMany({ where: { id: tenant.id, status: "PENDING" }, data: { status: "PROVISIONING" } });
    if (count === 1) await enqueueTenantJob(tenant.id, "PROVISION");
    return `/${locale}/welcome/${tenant.id}`;
  }

  const q = quote({
    services: tenant.services.map((svc) => svc.kind.toLowerCase()) as Config["services"],
    storageUnits: tenant.storageUnits,
    extraMembers: tenant.extraMembers,
    aiCreditChf: tenant.aiCreditChf,
    interval: tenant.billingInterval === "YEAR" ? "year" : "month",
  });

  let customerId = tenant.stripeCustomerId;
  if (!customerId) {
    const customer = await s.customers.create({
      email: user.email,
      name: user.name ?? tenant.displayName,
      preferred_locales: [locale],
      metadata: { tenantId: tenant.id, userId: user.id },
    });
    customerId = customer.id;
    await prisma.tenant.update({ where: { id: tenant.id }, data: { stripeCustomerId: customerId } });
  }

  // One open checkout per tenant, so it can't be paid for twice.
  const open = await s.checkout.sessions.list({ customer: customerId, status: "open", limit: 100 });
  for (const session of open.data) {
    if (session.metadata?.tenantId === tenant.id) await s.checkout.sessions.expire(session.id);
  }

  const checkout = await s.checkout.sessions.create({
    mode: "subscription",
    customer: customerId,
    line_items: await lineItemsFor(s, q.lines, q.config.interval),
    locale: STRIPE_LOCALES[locale],
    allow_promotion_codes: true,
    billing_address_collection: "required",
    client_reference_id: tenant.id,
    metadata: { tenantId: tenant.id },
    subscription_data: { metadata: { tenantId: tenant.id } },
    success_url: appUrl(`/${locale}/welcome/${tenant.id}`),
    cancel_url: appUrl(cancelPath),
  });
  return checkout.url;
}
