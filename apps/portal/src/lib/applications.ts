import "server-only";
import { prisma, type User } from "@pc/db";
import { audit } from "@pc/core";
import { fill, getDictionary, isLocale, type Locale } from "@/i18n";
import { BASE_DOMAIN, appUrl } from "@/lib/config";
import { trySend } from "@/lib/mail";

// Beta applications (docs/beta.md): a new cloud starts as APPLIED; staff
// approve it (it becomes PENDING, waiting for payment) or reject it (it is
// deleted). Nothing is charged or provisioned before approval.

type Kind = "applied" | "approved" | "rejected";

async function mailApplicant(kind: Kind, to: string, tenant: { displayName: string; slug: string; locale: string }) {
  const locale: Locale = isLocale(tenant.locale) ? tenant.locale : "de";
  const t = getDictionary(locale).mail;
  const vars = { name: tenant.displayName, address: `${tenant.slug}.${BASE_DOMAIN}`, link: appUrl(`/${locale}/account`) };
  const [subject, body] = {
    applied: [t.appliedSubject, t.appliedBody],
    approved: [t.approvedSubject, t.approvedBody],
    rejected: [t.rejectedSubject, t.rejectedBody],
  }[kind];
  return trySend({ to, subject, text: `${t.greeting}\n\n${fill(body, vars)}\n\n${t.signature}\n` });
}

/** After a new application: confirmation to the applicant, a heads-up to staff. */
export async function applicationReceived(user: User, tenant: { displayName: string; slug: string; locale: string }) {
  await mailApplicant("applied", user.email, tenant);
  const staff = process.env.APPLICATIONS_NOTIFY_TO;
  if (staff) {
    // No personal data beyond the address: the review happens in the portal.
    await trySend({ to: staff, subject: `New beta application: ${tenant.slug}`, text: `Review it at ${appUrl("/en/admin")}\n` });
  }
}

function owner(tenantId: string) {
  return prisma.member.findFirst({ where: { tenantId, role: "OWNER" }, include: { user: true } });
}

/** Returns whether the application was still open. */
export async function approveApplication(staff: User, tenantId: string): Promise<boolean> {
  const { count } = await prisma.tenant.updateMany({
    where: { id: tenantId, status: "APPLIED" },
    data: { status: "PENDING", reviewedAt: new Date(), reviewedBy: staff.id, applicationNote: null },
  });
  if (count !== 1) return false;
  const tenant = await prisma.tenant.findUniqueOrThrow({ where: { id: tenantId } });
  await audit(staff.id, "application.approve", tenantId, { slug: tenant.slug });
  const o = await owner(tenantId);
  if (o) await mailApplicant("approved", o.user.email, tenant);
  return true;
}

/** Deletes the application (and frees its address). */
export async function rejectApplication(staff: User, tenantId: string): Promise<boolean> {
  const tenant = await prisma.tenant.findFirst({ where: { id: tenantId, status: "APPLIED" } });
  if (!tenant) return false;
  const o = await owner(tenantId);
  const { count } = await prisma.tenant.deleteMany({ where: { id: tenantId, status: "APPLIED" } });
  if (count !== 1) return false;
  await audit(staff.id, "application.reject", tenantId, { slug: tenant.slug });
  if (o) await mailApplicant("rejected", o.user.email, tenant);
  return true;
}

/** The applicant takes an application back before review. */
export async function withdrawApplication(user: User, tenantId: string): Promise<boolean> {
  const { count } = await prisma.tenant.deleteMany({
    where: { id: tenantId, status: "APPLIED", members: { some: { userId: user.id, role: "OWNER" } } },
  });
  if (count === 1) await audit(user.id, "application.withdraw", tenantId);
  return count === 1;
}
