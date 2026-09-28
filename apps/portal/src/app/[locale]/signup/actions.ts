"use server";

import { redirect } from "next/navigation";
import { prisma, type ServiceKind } from "@pc/db";
import { audit, normalizeEmail } from "@pc/core";
import { quote } from "@pc/pricing";
import { auth } from "@/auth";
import { isLocale } from "@/i18n";
import { applicationReceived } from "@/lib/applications";
import { checkoutUrl } from "@/lib/checkout";
import { BETA } from "@/lib/config";
import { subscribe } from "@/lib/newsletter";
import { configFromQuery } from "@/lib/plan";
import { isValidSlug } from "@/lib/slug";

export type SignupState = { error?: "invalid" | "taken" | "accept" | "payment" | "tooMany" } | undefined;

const NOTE_MAX = 500;

export async function isSlugAvailable(slug: string) {
  return isValidSlug(slug) && !(await prisma.tenant.findUnique({ where: { slug }, select: { id: true } }));
}

export async function createCloud(_prev: SignupState, form: FormData): Promise<SignupState> {
  const session = await auth();
  const sub = session?.user?.id;
  if (!sub) return { error: "invalid" };
  const user = await prisma.user.findUniqueOrThrow({ where: { authentikSub: sub } });

  const rawLocale = String(form.get("locale"));
  const locale = isLocale(rawLocale) ? rawLocale : "de";
  const name = String(form.get("name") ?? "").trim().slice(0, 60);
  const slug = String(form.get("slug") ?? "").trim().toLowerCase();
  if (!name || !isValidSlug(slug)) return { error: "invalid" };
  if (form.get("accept") !== "on" || (BETA && form.get("beta") !== "on")) return { error: "accept" };
  const note = BETA ? String(form.get("note") ?? "").trim().slice(0, NOTE_MAX) || null : null;

  const q = quote(configFromQuery(Object.fromEntries(new URLSearchParams(String(form.get("config"))))));
  if (!q.config.services.length) return { error: "invalid" };

  // One open application per person during the beta.
  if (BETA && (await prisma.tenant.count({ where: { status: "APPLIED", members: { some: { userId: user.id } } } })) > 0) {
    return { error: "tooMany" };
  }

  let tenantId: string;
  try {
    const tenant = await prisma.tenant.create({
      data: {
        slug,
        displayName: name,
        locale,
        status: BETA ? "APPLIED" : "PENDING",
        applicationNote: note,
        appliedAt: BETA ? new Date() : null,
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
  await audit(user.id, BETA ? "tenant.apply" : "tenant.create", tenantId, { slug, config: q.config });

  // Opt-in only; the address still has to be confirmed (double opt-in).
  const email = normalizeEmail(user.email);
  if (form.get("newsletter") === "on" && email) await subscribe(email, locale, "signup");

  if (BETA) {
    await applicationReceived(user, { displayName: name, slug, locale });
    redirect(`/${locale}/account`);
  }

  const url = await checkoutUrl(tenantId, user, locale, `/${locale}/signup?${form.get("config")}`);
  if (!url) return { error: "payment" };
  redirect(url);
}
