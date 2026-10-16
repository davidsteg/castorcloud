"use server";

import { headers } from "next/headers";
import { revalidatePath } from "next/cache";
import { prisma } from "@pc/db";
import { auth } from "@/auth";
import { tenantSlugFromHost } from "@/lib/config";
import { addStack, removeStack, type AddResult } from "@/lib/stacks";

/** The tenant of this host, if the signed-in user owns it. The form never says which tenant. */
async function ownedHere() {
  const slug = tenantSlugFromHost((await headers()).get("host"));
  const sub = (await auth())?.user?.id;
  if (!slug || !sub) return null;
  const tenant = await prisma.tenant.findUnique({ where: { slug }, include: { members: { include: { user: true } } } });
  if (!tenant || tenant.status !== "ACTIVE") return null;
  const member = tenant.members.find((m) => m.user.authentikSub === sub);
  return member?.role === "OWNER" ? { tenant, userId: member.userId } : null;
}

export type FormState = AddResult | null;

export async function installStack(_prev: FormState, form: FormData): Promise<FormState> {
  const owner = await ownedHere();
  if (!owner) return { ok: false, error: "unavailable" };
  const catalogKey = String(form.get("catalogKey") ?? "") || undefined;
  const result = await addStack(owner.tenant, owner.userId, {
    name: String(form.get("name") ?? ""),
    catalogKey,
    definition: catalogKey ? undefined : String(form.get("definition") ?? ""),
  });
  revalidatePath("/stacks");
  return result;
}

export async function uninstallStack(form: FormData) {
  const owner = await ownedHere();
  if (!owner || form.get("confirm") !== "on") return;
  await removeStack(owner.tenant, owner.userId, String(form.get("stackId") ?? ""));
  revalidatePath("/stacks");
}
