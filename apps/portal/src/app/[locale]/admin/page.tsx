import type { Metadata } from "next";
import { notFound, redirect } from "next/navigation";
import { prisma } from "@pc/db";
import { STORAGE_UNIT_GB } from "@pc/pricing";
import { getDictionary, type Locale } from "@/i18n";
import { approveApplication, rejectApplication } from "@/lib/applications";
import { BASE_DOMAIN } from "@/lib/config";
import { staffUser } from "@/lib/session";
import { SiteHeader } from "@/components/site/SiteHeader";

// Staff console (User.isStaff, set with `castor-admin staff add`).
// Everyone else gets a 404. Every action re-checks staff and is audited.
export const metadata: Metadata = { robots: { index: false } };

export default async function AdminPage({ params }: { params: Promise<{ locale: Locale }> }) {
  const { locale } = await params;
  const staff = await staffUser();
  if (!staff) notFound();
  const t = getDictionary(locale);

  const [applications, awaitingPayment, newsletter] = await Promise.all([
    prisma.tenant.findMany({
      where: { status: "APPLIED" },
      include: { members: { where: { role: "OWNER" }, include: { user: { select: { email: true, name: true } } } }, services: true },
      orderBy: { appliedAt: "asc" },
    }),
    prisma.tenant.count({ where: { status: "PENDING", reviewedAt: { not: null } } }),
    prisma.newsletterSubscriber.groupBy({ by: ["status"], _count: true }),
  ]);
  const count = (status: string) => newsletter.find((n) => n.status === status)?._count ?? 0;

  async function review(form: FormData) {
    "use server";
    const me = await staffUser();
    if (!me) notFound();
    const tenantId = String(form.get("tenantId"));
    if (form.get("decision") === "approve") await approveApplication(me, tenantId);
    else if (form.get("decision") === "reject") await rejectApplication(me, tenantId);
    redirect(`/${locale}/admin`);
  }

  return (
    <>
      <SiteHeader locale={locale} t={t} path="/admin" />
      <main className="container" style={{ padding: "48px 24px" }}>
        <h1>Staff</h1>

        <section className="panel" style={{ margin: "24px 0" }}>
          <h3 style={{ fontSize: 18 }}>Beta applications ({applications.length})</h3>
          <p className="small-print" style={{ marginBottom: 16 }}>
            Approving e-mails the applicant a link to pay; nothing is provisioned before payment. Rejecting deletes the
            application and frees the address. {awaitingPayment} approved {awaitingPayment === 1 ? "cloud waits" : "clouds wait"} for payment.
          </p>
          {applications.length === 0 ? (
            <p className="notice">No open applications.</p>
          ) : (
            <table className="admin-table">
              <thead>
                <tr><th>Applied</th><th>Applicant</th><th>Cloud</th><th>Plan</th><th>Note</th><th /></tr>
              </thead>
              <tbody>
                {applications.map((a) => (
                  <tr key={a.id}>
                    <td>{a.appliedAt?.toISOString().slice(0, 10)}</td>
                    <td>{a.members[0]?.user.name}<br /><span className="small-print">{a.members[0]?.user.email}</span></td>
                    <td>{a.displayName}<br /><span className="small-print">{a.slug}.{BASE_DOMAIN} · {a.locale}</span></td>
                    <td className="small-print">
                      {a.services.map((s) => s.kind.toLowerCase()).join(", ")}<br />
                      {a.storageUnits * STORAGE_UNIT_GB} GB · +{a.extraMembers} members · {a.billingInterval.toLowerCase()}
                    </td>
                    <td style={{ maxWidth: 280, whiteSpace: "pre-wrap", overflowWrap: "anywhere" }}>{a.applicationNote}</td>
                    <td>
                      <form action={review} style={{ display: "flex", gap: 8 }}>
                        <input type="hidden" name="tenantId" value={a.id} />
                        <button className="button small" type="submit" name="decision" value="approve">Approve</button>
                        <button className="button secondary small" type="submit" name="decision" value="reject">Reject</button>
                      </form>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </section>

        <section className="panel">
          <h3 style={{ fontSize: 18 }}>Newsletter</h3>
          <p style={{ marginBottom: 16 }}>
            {count("CONFIRMED")} confirmed · {count("PENDING")} waiting for confirmation · {count("UNSUBSCRIBED")} unsubscribed
          </p>
          <a className="button secondary small" href="/api/admin/newsletter">Export confirmed (CSV)</a>
          <p className="small-print" style={{ marginTop: 12 }}>
            Each row has its personal unsubscribe link: put it in every mail you send. The export is audited.
          </p>
        </section>
      </main>
    </>
  );
}
