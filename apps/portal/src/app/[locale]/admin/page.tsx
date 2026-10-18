import type { Metadata } from "next";
import { notFound, redirect } from "next/navigation";
import { prisma } from "@pc/db";
import { formatChf, STORAGE_UNIT_GB } from "@pc/pricing";
import { getDictionary, type Locale } from "@/i18n";
import { adminStats } from "@/lib/admin-stats";
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

  const [applications, awaitingPayment, newsletter, stats] = await Promise.all([
    prisma.tenant.findMany({
      where: { status: "APPLIED" },
      include: { members: { where: { role: "OWNER" }, include: { user: { select: { email: true, name: true } } } }, services: true },
      orderBy: { appliedAt: "asc" },
    }),
    prisma.tenant.count({ where: { status: "PENDING", reviewedAt: { not: null } } }),
    prisma.newsletterSubscriber.groupBy({ by: ["status"], _count: true }),
    adminStats(),
  ]);
  const count = (status: string) => newsletter.find((n) => n.status === status)?._count ?? 0;
  const tenants = (status: keyof typeof stats.tenants) => stats.tenants[status] ?? 0;

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

        <section className="panel" style={{ margin: "24px 0" }}>
          <h3 style={{ fontSize: 18 }}>Statistics</h3>
          <p className="small-print" style={{ marginBottom: 16 }}>
            Totals from the control-plane database only; no tenant is named and no tenant content is read.
          </p>
          <div className="admin-stats">
            <Stat label="Active clouds" value={tenants("ACTIVE")}
              note={`${tenants("APPLIED")} applied · ${tenants("PENDING")} awaiting payment · ${tenants("PROVISIONING")} provisioning`} />
            <Stat label="Suspended / failed" value={`${tenants("SUSPENDED")} / ${tenants("FAILED")}`}
              note={`${tenants("DELETING") + tenants("DELETED")} deleted or being deleted`} />
            <Stat label="MRR (estimate)" value={formatChf(stats.revenue.mrrRp)}
              note={`list price incl. VAT · ${stats.revenue.yearlyPlans} yearly ${stats.revenue.yearlyPlans === 1 ? "plan" : "plans"}`} />
            <Stat label="Users" value={stats.users.total}
              note={`${stats.users.new30d} new in 30 days · ${stats.users.liveMembers} cloud members · ${stats.users.staff} staff`} />
            <Stat label="Applications (30 days)" value={stats.applications30d} />
            <Stat label="Storage booked" value={gb(stats.storage.bookedGb * 1024)}
              note={`${gb(stats.storage.usedMb)} used of ${gb(stats.storage.volumeMb)} provisioned`} />
            <Stat label="Backups" value={`${stats.backups.live - stats.backups.stale} / ${stats.backups.live} current`}
              note={`${stats.backups.stale} older than 2 days · ${stats.backups.failing} failing`} />
            <Stat label="Jobs" value={`${stats.jobs.failed7d} failed`}
              note={`last 7 days · ${stats.jobs.pending} queued or running`} />
            <Stat label="Open alerts" value={(stats.alerts.CRITICAL ?? 0) + (stats.alerts.WARNING ?? 0)}
              note={`${stats.alerts.CRITICAL ?? 0} critical · ${stats.alerts.WARNING ?? 0} warning`} />
          </div>

          <h4 style={{ margin: "24px 0 8px" }}>Services booked</h4>
          <p>
            {Object.keys(stats.services).length === 0
              ? "None yet."
              : Object.entries(stats.services).map(([kind, n]) => `${kind.toLowerCase()} ${n}`).join(" · ")}
          </p>

          <h4 style={{ margin: "24px 0 8px" }}>Nodes</h4>
          {stats.nodes.length === 0 ? (
            <p className="notice">No nodes registered.</p>
          ) : (
            <table className="admin-table">
              <thead>
                <tr><th>Node</th><th>Role</th><th>Clouds</th><th>Pool</th><th>Last seen</th></tr>
              </thead>
              <tbody>
                {stats.nodes.map((n) => (
                  <tr key={n.name}>
                    <td>{n.name}{n.enabled ? null : <span className="small-print"> (draining)</span>}</td>
                    <td className="small-print">{n.role.toLowerCase()}</td>
                    <td>{n.tenants} / {n.capacity}</td>
                    <td className="small-print">
                      {n.poolSizeMb ? `${gb(n.poolUsedMb ?? 0)} of ${gb(n.poolSizeMb)} (${Math.round(((n.poolUsedMb ?? 0) / n.poolSizeMb) * 100)} %)` : "–"}
                      {n.poolHealth ? ` · ${n.poolHealth}` : null}
                    </td>
                    <td className="small-print">{n.lastSeenAt?.toISOString().slice(0, 16).replace("T", " ") ?? "never"}</td>
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

function Stat({ label, value, note }: { label: string; value: string | number; note?: string }) {
  return (
    <div className="admin-stat">
      <span className="small-print">{label}</span>
      <strong>{value}</strong>
      {note ? <span className="small-print">{note}</span> : null}
    </div>
  );
}

/** Megabytes as GB or TB, for display. */
function gb(mb: number): string {
  const g = mb / 1024;
  return g >= 1000 ? `${(g / 1024).toFixed(1)} TB` : `${g < 10 ? g.toFixed(1) : Math.round(g)} GB`;
}
