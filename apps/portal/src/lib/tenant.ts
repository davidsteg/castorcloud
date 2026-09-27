import { cache } from "react";
import { notFound, redirect } from "next/navigation";
import { prisma } from "@pc/db";
import { auth } from "@/auth";
import { DEFAULT_LOCALE, getDictionary, isLocale } from "@/i18n";

/**
 * Loads the tenant for the current request and checks the user is a member.
 * `t` is the dictionary in the tenant's language (chosen at signup).
 */
export const requireTenantMember = cache(async (slug: string) => {
  const session = await auth();
  const sub = session?.user?.id;
  if (!session || !sub) redirect("/api/auth/signin?callbackUrl=/");

  const tenant = await prisma.tenant.findUnique({
    where: { slug },
    include: { members: { include: { user: true } }, services: true },
  });
  if (!tenant || tenant.status === "DELETED") notFound();

  const member = tenant.members.find((m) => m.user.authentikSub === sub);
  if (!member) notFound();

  const enabledServices = tenant.services.filter((s) => s.enabled).map((s) => s.kind.toLowerCase());
  const locale = isLocale(tenant.locale) ? tenant.locale : DEFAULT_LOCALE;
  return { tenant, member, session, enabledServices, locale, t: getDictionary(locale) };
});
