"use server";

import { redirect } from "next/navigation";
import { prisma, type ServiceKind } from "@pc/db";
import { audit, enqueueTenantJob } from "@pc/core";
import { quote } from "@pc/pricing";
import { auth } from "@/auth";
import { APP_LABEL, BASE_DOMAIN, PUBLIC_SCHEME } from "@/lib/config";
import { configFromQuery } from "@/lib/plan";
import { isValidSlug } from "@/lib/slug";
import { STRIPE_LOCALES, lineItemsFor, stripe } from "@/lib/stripe";
import type { Locale } from "@/i18n";

export type SignupState = { error?: "invalid" | "taken" | "accept" | "payment" } | undefined;

const appUrl = (path: string) => `${PUBLIC_SCHEME}://${APP_LABEL}.${BASE_DOMAIN}${path}`;

export async function isSlugAvailable(slug: string) {
  return isValidSlug(slug) && !(await prisma.tenant.findUnique({ where: { slug }, select: { id: true } }));
}

export async function createCloud(_prev: SignupState, form: FormData): Promise<SignupState> {
  const session = await auth();
  const sub = session?.user?.id;
  if (!sub) return { error: "invalid" };
  const user = await prisma.user.findUniqueOrThrow({ where: { authentikSub: sub } });

  const locale = String(form.get("locale")) as Locale;
  const name = String(form.get("name") ?? "").trim().slice(0, 60);
  const slug = String(form.get("slug") ?? "").trim().toLowerCase();
  if (!name || !isValidSlug(slug)) return { error: "invalid" };
  if (form.get("accept") !== "on") return { error: "accept" };

  const q = quote(configFromQuery(Object.fromEntries(new URLSearchParams(String(form.get("config"))))));
  if (!q.config.services.length) return { error: "invalid" };

  let tenantId: string;
  try {
    const tenant = await prisma.tenant.create({
      data: {
        slug,
        displayName: name,
        locale,
        storageUnits: q.config.storageUnits,
        extraMembers: q.config.extraMembers,
        aiCreditChf: q.config.aiCreditChf,
        billingInterval: q.config.interval === "year" ? "YEAR" : "MONTH",
        members: { create: { userId: user.id, role: "OWNER" } },
        services: {
          create: q.config.services.map((s) => ({ kind: s.toUpperCase() as ServiceKind })),
        },
      },
    });
    tenantId = tenant.id;
  } catch {
    return { error: "taken" }; // unique slug
  }
  await audit(user.id, "tenant.create", tenantId, { slug, config: q.config });

  const s = stripe();
  if (!s) {
    if (process.env.DEV_SKIP_PAYMENT !== "true") return { error: "payment" };
    // Development without Stripe keys: provision right away.
    await prisma.tenant.update({ where: { id: tenantId }, data: { status: "PROVISIONING" } });
    await enqueueTenantJob(tenantId, "PROVISION");
    redirect(`/${locale}/welcome/${tenantId}`);
  }

  const customer = await s.customers.create({
    email: user.email,
    name: user.name ?? name,
    preferred_locales: [locale],
    metadata: { tenantId, userId: user.id },
  });
  await prisma.tenant.update({ where: { id: tenantId }, data: { stripeCustomerId: customer.id } });

  const checkout = await s.checkout.sessions.create({
    mode: "subscription",
    customer: customer.id,
    line_items: await lineItemsFor(s, q.lines, q.config.interval),
    locale: STRIPE_LOCALES[locale],
    allow_promotion_codes: true,
    billing_address_collection: "required",
    client_reference_id: tenantId,
    metadata: { tenantId },
    subscription_data: { metadata: { tenantId } },
    success_url: appUrl(`/${locale}/welcome/${tenantId}`),
    cancel_url: appUrl(`/${locale}/signup?${form.get("config")}`),
  });
  redirect(checkout.url!);
}
