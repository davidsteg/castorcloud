"use server";

import { redirect } from "next/navigation";
import { prisma } from "@pc/db";
import { APP_LABEL, BASE_DOMAIN, PUBLIC_SCHEME } from "@/lib/config";
import { currentUser } from "@/lib/session";

/** Signs out of the portal on every host and of Authentik (see api/logout). */
export async function signOutEverywhere() {
  const user = await currentUser();
  const tenants = user
    ? await prisma.tenant.findMany({ where: { members: { some: { userId: user.id } } }, select: { slug: true } })
    : [];
  const hops = [APP_LABEL, ...tenants.map((t) => t.slug)];
  redirect(`${PUBLIC_SCHEME}://${hops[0]}.${BASE_DOMAIN}/api/logout?hops=${hops.slice(1).join(",")}`);
}
