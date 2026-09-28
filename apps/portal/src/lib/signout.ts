"use server";

import { redirect } from "next/navigation";
import { prisma, type RevocationScope } from "@pc/db";
import { audit, enqueueTenantJob } from "@pc/core";
import { APP_LABEL, BASE_DOMAIN, PUBLIC_SCHEME } from "@/lib/config";
import { currentUser } from "@/lib/session";

/**
 * Log out: this browser leaves the portal on every host and Authentik (see
 * api/logout), and the apps end the person's browser sessions. Phone apps stay
 * connected. Embedded apps sign in again silently on another computer that is
 * still signed in to the portal.
 */
export async function signOutBrowser() {
  const user = await currentUser();
  const slugs = user ? await revokeInApps(user, "WEB") : [];
  redirect(logoutUrl(slugs));
}

/**
 * Log out on all devices (a lost phone, a shared computer): also every other
 * browser's portal session, every Authentik session, the phone apps' sessions
 * and API keys, and Telegram links to the assistant.
 */
export async function signOutAllDevices() {
  const user = await currentUser();
  let slugs: string[] = [];
  if (user) {
    await prisma.user.update({ where: { id: user.id }, data: { sessionsRevokedAt: new Date() } });
    await prisma.telegramLink.deleteMany({ where: { member: { userId: user.id } } });
    slugs = await revokeInApps(user, "DEVICES");
    await audit(user.id, "user.signed_out_everywhere");
  }
  redirect(logoutUrl(slugs));
}

/** Queues the revocation in each of the user's clouds; returns their slugs. */
async function revokeInApps(user: { id: string; email: string; authentikSub: string }, scope: RevocationScope) {
  const tenants = await prisma.tenant.findMany({
    where: { members: { some: { userId: user.id } }, status: { not: "DELETED" } },
    select: { id: true, slug: true },
  });
  for (const t of tenants) {
    await prisma.accessRevocation.create({ data: { tenantId: t.id, email: user.email, sub: user.authentikSub, scope } });
    await enqueueTenantJob(t.id, "REVOKE_ACCESS");
  }
  return tenants.map((t) => t.slug);
}

/** First hop of the logout chain through app.<domain> and each tenant host. */
function logoutUrl(slugs: string[]) {
  const hops = [APP_LABEL, ...slugs];
  return `${PUBLIC_SCHEME}://${hops[0]}.${BASE_DOMAIN}/api/logout?hops=${hops.slice(1).join(",")}`;
}
