import "server-only";
import { prisma } from "@pc/db";
import { auth } from "@/auth";

/** The signed-in user's DB row, or null. */
export async function currentUser() {
  const session = await auth();
  const sub = session?.user?.id;
  if (!sub) return null;
  return prisma.user.findUnique({ where: { authentikSub: sub } });
}

/** The tenant, if the signed-in user is a member of it. */
export async function memberTenant(tenantId: string) {
  const user = await currentUser();
  if (!user) return null;
  return prisma.tenant.findFirst({ where: { id: tenantId, members: { some: { userId: user.id } } } });
}

/** The tenant, if the signed-in user is its owner. */
export async function ownedTenant(tenantId: string) {
  const user = await currentUser();
  if (!user) return null;
  return prisma.tenant.findFirst({ where: { id: tenantId, members: { some: { userId: user.id, role: "OWNER" } } } });
}

/** The signed-in user if they are staff (admin console), else null. */
export async function staffUser() {
  const user = await currentUser();
  return user?.isStaff ? user : null;
}
